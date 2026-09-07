use crate::autostart;
use crate::config::{self, AppConfig};
use crate::hid::client::{self, DeviceEvent, WorkerCommand};
use crate::hid::scanner::scan_receivers;
use crate::i18n::{Language, Text};
use crate::icon;
use crate::model::BatteryState;
use crate::notify::Notifier;
use crate::APP_ID;
use anyhow::{Context, Result};
use hidapi::HidApi;
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[derive(Debug, Clone)]
enum UserEvent {
    Menu(String),
    /// An incremental device update or departure from a receiver worker.
    Device(DeviceEvent),
}

/// Preset choices for the menu submenus. The numeric value is encoded into each
/// item's id (e.g. "poll:60") so the event handler can parse it back.
const POLL_PRESETS: &[(Text, u64)] = &[
    (Text::Seconds15, 15),
    (Text::Seconds30, 30),
    (Text::Minute1, 60),
    (Text::Minutes2, 120),
    (Text::Minutes3, 180),
    (Text::Minutes5, 300),
    (Text::Minutes15, 900),
];
const THRESHOLD_PRESETS: &[u8] = &[5, 10, 15, 20, 25, 30];
const COOLDOWN_PRESETS: &[(Text, u64)] = &[
    (Text::Minutes30, 30),
    (Text::Hour1, 60),
    (Text::Hours2, 120),
    (Text::Hours4, 240),
    (Text::Hours8, 480),
];
/// Language submenu choices in menu order. Each item's id is `"language:{id}"`,
/// where `id` matches the serialized `language` config value.
const LANGUAGE_CHOICES: &[(&str, Language)] = &[
    ("auto", Language::Auto),
    ("en", Language::English),
    ("zh-CN", Language::SimplifiedChinese),
];

struct MenuHandles {
    root: Menu,
    language: Language,
    status_item: MenuItem,
    select_submenu: Submenu,
    refresh_item: MenuItem,
    view_mode_item: CheckMenuItem,
    notify_item: CheckMenuItem,
    autostart_item: CheckMenuItem,
    open_config_item: MenuItem,
    exit_item: MenuItem,
    device_items: Vec<CheckMenuItem>,
    language_items: Vec<CheckMenuItem>,
    poll_items: Vec<CheckMenuItem>,
    threshold_items: Vec<CheckMenuItem>,
    cooldown_items: Vec<CheckMenuItem>,
}

impl MenuHandles {
    fn build(cfg: &AppConfig, initial_autostart: bool, initial_text_mode: bool) -> Result<Self> {
        let language = cfg.language.resolve();
        let root = Menu::new();
        let status_item = MenuItem::new(language.text(Text::NoDevicesFound), false, None);
        let select_submenu = Submenu::new(language.text(Text::SelectDevice), true);
        let refresh_item = MenuItem::with_id("refresh", language.text(Text::Refresh), true, None);
        let view_mode_item = CheckMenuItem::with_id(
            "viewmode",
            language.text(Text::TextMode),
            true,
            initial_text_mode,
            None,
        );

        let (poll_submenu, poll_items) = build_preset_submenu(
            language.text(Text::PollInterval),
            "poll",
            POLL_PRESETS
                .iter()
                .map(|&(label, value)| (language.text(label).to_string(), value)),
            cfg.poll_interval_seconds,
        )?;
        let notify_item = CheckMenuItem::with_id(
            "notify",
            language.text(Text::Notifications),
            true,
            cfg.notifications_enabled,
            None,
        );
        let (threshold_submenu, threshold_items) = build_preset_submenu(
            language.text(Text::Threshold),
            "threshold",
            THRESHOLD_PRESETS
                .iter()
                .map(|&n| (format!("{n}%"), u64::from(n))),
            u64::from(cfg.low_battery_threshold),
        )?;
        let (cooldown_submenu, cooldown_items) = build_preset_submenu(
            language.text(Text::Cooldown),
            "cooldown",
            COOLDOWN_PRESETS
                .iter()
                .map(|&(label, value)| (language.text(label).to_string(), value)),
            cfg.low_battery_cooldown_minutes,
        )?;

        let autostart_item = CheckMenuItem::with_id(
            "autostart",
            language.text(Text::Autostart),
            true,
            initial_autostart,
            None,
        );
        let open_config_item =
            MenuItem::with_id("openconfig", language.text(Text::OpenConfig), true, None);
        let exit_item = MenuItem::with_id("exit", language.text(Text::Exit), true, None);

        let languages = Submenu::new(language.text(Text::Language), true);
        let mut language_items = Vec::new();
        for &(id, choice) in LANGUAGE_CHOICES {
            let label = match choice {
                Language::Auto => language.text(Text::Automatic),
                Language::English => "English",
                Language::SimplifiedChinese => "简体中文",
            };
            let item = CheckMenuItem::with_id(
                format!("language:{id}"),
                label,
                true,
                cfg.language == choice,
                None,
            );
            languages.append(&item)?;
            language_items.push(item);
        }

        root.append_items(&[
            &status_item,
            &select_submenu,
            &refresh_item,
            &PredefinedMenuItem::separator(),
            &view_mode_item,
            &languages,
            &poll_submenu,
            &notify_item,
            &threshold_submenu,
            &cooldown_submenu,
            &autostart_item,
            &PredefinedMenuItem::separator(),
            &open_config_item,
            &PredefinedMenuItem::separator(),
            &exit_item,
        ])?;

        Ok(Self {
            root,
            language,
            status_item,
            select_submenu,
            refresh_item,
            view_mode_item,
            notify_item,
            autostart_item,
            open_config_item,
            exit_item,
            device_items: Vec::new(),
            language_items,
            poll_items,
            threshold_items,
            cooldown_items,
        })
    }

    fn rebuild_device_menu(&mut self, devices: &[BatteryState], selected_id: &str) -> Result<()> {
        let language = self.language;
        for item in self.select_submenu.items() {
            remove_item(&self.select_submenu, &item)?;
        }
        self.device_items.clear();

        if devices.is_empty() {
            let empty = MenuItem::new(language.text(Text::NoDevices), false, None);
            self.select_submenu.append(&empty)?;
            return Ok(());
        }

        for device in devices {
            let checked = device.device_key == selected_id;
            let label = format!(
                "{}: {}{}",
                device.display_name,
                device.battery_percent,
                if device.is_charging {
                    language.text(Text::Charging)
                } else {
                    "%"
                }
            );
            let item = CheckMenuItem::with_id(
                format!("device:{}", device.device_key),
                label,
                true,
                checked,
                None,
            );
            self.select_submenu.append(&item)?;
            self.device_items.push(item);
        }

        Ok(())
    }

    fn set_selected(&self, selected_id: &str) {
        for item in &self.device_items {
            let is_selected = item.id().0.strip_prefix("device:") == Some(selected_id);
            item.set_checked(is_selected);
        }
    }

    fn touch_ids(&self) {
        let _ = self.refresh_item.id();
        let _ = self.exit_item.id();
        let _ = self.open_config_item.id();
    }
}

/// Build a submenu of radio-style preset choices. Each item's id is
/// `"{prefix}:{value}"`; the item whose value equals `current` starts checked.
fn build_preset_submenu(
    title: &str,
    prefix: &str,
    presets: impl Iterator<Item = (String, u64)>,
    current: u64,
) -> Result<(Submenu, Vec<CheckMenuItem>)> {
    let submenu = Submenu::new(title, true);
    let mut items = Vec::new();
    for (label, value) in presets {
        let item = CheckMenuItem::with_id(
            format!("{prefix}:{value}"),
            label,
            true,
            value == current,
            None,
        );
        submenu.append(&item)?;
        items.push(item);
    }
    Ok((submenu, items))
}

/// Re-sync a preset submenu's checkmarks so exactly the item matching `value`
/// is checked. muda auto-toggles the clicked item, so without this, clicking the
/// already-active preset would leave it unchecked.
fn set_preset(items: &[CheckMenuItem], prefix: &str, value: u64) {
    let target = format!("{prefix}:{value}");
    for item in items {
        item.set_checked(item.id().0 == target);
    }
}

/// Re-sync the Language submenu's checkmarks to `language`. Needed when a
/// switch fails, because muda has already toggled the clicked item by then.
fn set_language_choice(items: &[CheckMenuItem], language: Language) {
    for (item, &(_, choice)) in items.iter().zip(LANGUAGE_CHOICES) {
        item.set_checked(choice == language);
    }
}

fn language_for_id(id: &str) -> Option<Language> {
    LANGUAGE_CHOICES
        .iter()
        .find(|(choice_id, _)| *choice_id == id)
        .map(|&(_, language)| language)
}

pub fn run_tray_app(mut cfg: AppConfig) -> Result<()> {
    let exe_path = std::env::current_exe().context("failed resolving executable path")?;
    if let Err(err) = autostart::set_enabled(&exe_path, cfg.autostart) {
        tracing::warn!("failed to apply autostart setting: {err}");
    }

    let autostart_enabled = autostart::is_enabled().unwrap_or(cfg.autostart);

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    MenuEvent::set_event_handler(Some({
        let proxy = proxy.clone();
        move |event: MenuEvent| {
            let _ = proxy.send_event(UserEvent::Menu(event.id.0.clone()));
        }
    }));

    // Commands are sent to the supervisor, which fans them out to the per-
    // receiver workers (and spawns workers for receivers as they appear).
    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCommand>();
    spawn_supervisor(proxy.clone(), cmd_rx, cfg.poll_interval_seconds);

    let mut text_mode = cfg.text_mode();
    let mut menu = MenuHandles::build(&cfg, autostart_enabled, text_mode)?;
    menu.touch_ids();

    let initial_icon = icon::neutral_icon()?;
    let mut tray = build_tray_icon(&menu.root, initial_icon)?;

    let mut notifier = Notifier::new(
        menu.language,
        cfg.notifications_enabled,
        cfg.low_battery_threshold,
        cfg.low_battery_cooldown_minutes,
    );
    // Localize the tooltip and the device submenu placeholder before the first
    // device event arrives.
    if let Err(err) = refresh_tray_visuals(
        &mut tray,
        &[],
        "",
        &menu.status_item,
        text_mode,
        menu.language,
    ) {
        tracing::warn!("failed initializing tray: {err}");
    }
    if let Err(err) = menu.rebuild_device_menu(&[], "") {
        tracing::warn!("failed initializing device menu: {err}");
    }
    // Source of truth for what's currently connected, keyed by device_key. The
    // workers push explicit arrival/update/departure events, so there's no need
    // to infer absence from empty polls any more. `devices` is the sorted view
    // of `device_map` that the menu/tray rendering consumes.
    let mut device_map: BTreeMap<String, BatteryState> = BTreeMap::new();
    let mut devices: Vec<BatteryState> = Vec::new();
    let mut selected_id = cfg.selected_device_id.clone();

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;

        if let Event::UserEvent(user_event) = event {
            match user_event {
                UserEvent::Menu(id) => {
                    if id == "refresh" {
                        let _ = cmd_tx.send(WorkerCommand::Refresh);
                    } else if id == "exit" {
                        let _ = cmd_tx.send(WorkerCommand::Exit);
                        *control_flow = ControlFlow::Exit;
                    } else if id == "autostart" {
                        // muda auto-toggles the check state before firing this
                        // event, so is_checked() already holds the new value.
                        let enabled = menu.autostart_item.is_checked();
                        if let Err(err) = autostart::set_enabled(&exe_path, enabled) {
                            tracing::warn!("failed to set autostart: {err}");
                        }
                        cfg.autostart = enabled;
                        if let Err(err) = config::save_config(&cfg) {
                            tracing::warn!("failed saving config: {err}");
                        }
                    } else if id == "viewmode" {
                        // muda already toggled the check mark; read it directly.
                        text_mode = menu.view_mode_item.is_checked();
                        cfg.view_mode = if text_mode { "text" } else { "icon" }.to_string();
                        if let Err(err) = config::save_config(&cfg) {
                            tracing::warn!("failed saving config: {err}");
                        }
                        if let Err(err) = refresh_tray_visuals(
                            &mut tray,
                            &devices,
                            &selected_id,
                            &menu.status_item,
                            text_mode,
                            menu.language,
                        ) {
                            tracing::warn!("failed updating tray: {err}");
                        }
                    } else if id == "notify" {
                        // muda already toggled the check mark; read it directly.
                        cfg.notifications_enabled = menu.notify_item.is_checked();
                        notifier.set_enabled(cfg.notifications_enabled);
                        if let Err(err) = config::save_config(&cfg) {
                            tracing::warn!("failed saving config: {err}");
                        }
                    } else if let Some(choice) = id.strip_prefix("language:") {
                        let mut next_config = cfg.clone();
                        next_config.language = match language_for_id(choice) {
                            Some(language) => language,
                            None => return,
                        };
                        let rebuilt = MenuHandles::build(
                            &next_config,
                            menu.autostart_item.is_checked(),
                            text_mode,
                        )
                        .and_then(|mut replacement| {
                            replacement.rebuild_device_menu(&devices, &selected_id)?;
                            Ok(replacement)
                        });
                        match rebuilt {
                            Ok(replacement) => {
                                cfg = next_config;
                                tray.set_menu(Some(Box::new(replacement.root.clone())));
                                menu = replacement;
                                notifier.set_language(menu.language);
                                if let Err(err) = refresh_tray_visuals(
                                    &mut tray,
                                    &devices,
                                    &selected_id,
                                    &menu.status_item,
                                    text_mode,
                                    menu.language,
                                ) {
                                    tracing::warn!("failed updating translated tray: {err}");
                                }
                                if let Err(err) = config::save_config(&cfg) {
                                    tracing::warn!("failed saving config: {err}");
                                }
                            }
                            Err(err) => {
                                tracing::warn!("failed changing language: {err}");
                                set_language_choice(&menu.language_items, cfg.language);
                            }
                        }
                    } else if id == "openconfig" {
                        open_config_file();
                    } else if let Some(value) = id.strip_prefix("poll:") {
                        if let Ok(secs) = value.parse::<u64>() {
                            // Now controls the safety re-read interval: arrivals
                            // and battery changes are pushed, so this only bounds
                            // how often we re-read as a fallback.
                            cfg.poll_interval_seconds = secs;
                            let _ = cmd_tx.send(WorkerCommand::SetSafetyInterval(secs));
                            if let Err(err) = config::save_config(&cfg) {
                                tracing::warn!("failed saving config: {err}");
                            }
                            set_preset(&menu.poll_items, "poll", secs);
                        }
                    } else if let Some(value) = id.strip_prefix("threshold:") {
                        if let Ok(threshold) = value.parse::<u8>() {
                            cfg.low_battery_threshold = threshold;
                            notifier.set_threshold(threshold);
                            if let Err(err) = config::save_config(&cfg) {
                                tracing::warn!("failed saving config: {err}");
                            }
                            set_preset(&menu.threshold_items, "threshold", u64::from(threshold));
                        }
                    } else if let Some(value) = id.strip_prefix("cooldown:") {
                        if let Ok(minutes) = value.parse::<u64>() {
                            cfg.low_battery_cooldown_minutes = minutes;
                            notifier.set_cooldown(minutes);
                            if let Err(err) = config::save_config(&cfg) {
                                tracing::warn!("failed saving config: {err}");
                            }
                            set_preset(&menu.cooldown_items, "cooldown", minutes);
                        }
                    } else if let Some(device_id) = id.strip_prefix("device:") {
                        selected_id = device_id.to_string();
                        cfg.selected_device_id = selected_id.clone();
                        menu.set_selected(&selected_id);
                        if let Err(err) = config::save_config(&cfg) {
                            tracing::warn!("failed saving config: {err}");
                        }
                        if let Err(err) = refresh_tray_visuals(
                            &mut tray,
                            &devices,
                            &selected_id,
                            &menu.status_item,
                            text_mode,
                            menu.language,
                        ) {
                            tracing::warn!("failed updating tray: {err}");
                        }
                    }
                }
                UserEvent::Device(event) => {
                    match event {
                        DeviceEvent::Update(state) => {
                            // Notify on the fresh reading before it's moved into the map.
                            notifier.maybe_notify_low_battery(&state);
                            device_map.insert(state.device_key.clone(), state);
                        }
                        DeviceEvent::Gone(key) => {
                            device_map.remove(&key);
                        }
                    }

                    devices = sorted_devices(&device_map);

                    if ensure_selected_device(&mut selected_id, &devices) {
                        cfg.selected_device_id = selected_id.clone();
                        if let Err(err) = config::save_config(&cfg) {
                            tracing::warn!("failed saving config: {err}");
                        }
                    }

                    if let Err(err) = menu.rebuild_device_menu(&devices, &selected_id) {
                        tracing::warn!("failed rebuilding menu: {err}");
                    }
                    if let Err(err) = refresh_tray_visuals(
                        &mut tray,
                        &devices,
                        &selected_id,
                        &menu.status_item,
                        text_mode,
                        menu.language,
                    ) {
                        tracing::warn!("failed refreshing tray: {err}");
                    }
                }
            }
        }
    });
}

fn build_tray_icon(menu: &Menu, icon: Icon) -> Result<TrayIcon> {
    TrayIconBuilder::new()
        .with_menu(Box::new(menu.clone()))
        .with_tooltip(APP_ID)
        .with_icon(icon)
        .build()
        .context("failed creating tray icon")
}

fn refresh_tray_visuals(
    tray: &mut TrayIcon,
    devices: &[BatteryState],
    selected_id: &str,
    status_item: &MenuItem,
    text_mode: bool,
    language: Language,
) -> Result<()> {
    let selected = devices.iter().find(|d| d.device_key == selected_id);

    if let Some(device) = selected {
        let icon = if text_mode {
            icon::text_icon(device.battery_percent, device.is_charging)?
        } else {
            icon::battery_icon(device.battery_percent, device.is_charging)?
        };
        tray.set_icon(Some(icon))?;

        let tooltip = format!(
            "{}: {}{}",
            device.display_name,
            device.battery_percent,
            if device.is_charging {
                language.text(Text::Charging)
            } else {
                "%"
            }
        );
        tray.set_tooltip(Some(tooltip.clone()))?;
        status_item.set_text(&tooltip);
    } else {
        tray.set_icon(Some(icon::neutral_icon()?))?;
        tray.set_tooltip(Some(language.text(Text::NoDevicesFound)))?;
        status_item.set_text(language.text(Text::NoDevicesFound));
    }

    Ok(())
}

fn ensure_selected_device(selected_id: &mut String, devices: &[BatteryState]) -> bool {
    if devices.is_empty() {
        if !selected_id.is_empty() {
            selected_id.clear();
            return true;
        }
        return false;
    }

    if devices.iter().any(|d| d.device_key == *selected_id) {
        return false;
    }

    *selected_id = devices[0].device_key.clone();
    true
}

/// Sorted view of the current device map, matching the ordering the old
/// `PollResult::sort_devices` produced (by name, then pid, then key).
fn sorted_devices(map: &BTreeMap<String, BatteryState>) -> Vec<BatteryState> {
    let mut devices: Vec<BatteryState> = map.values().cloned().collect();
    devices.sort_by(|a, b| {
        a.display_name
            .cmp(&b.display_name)
            .then(a.pid.cmp(&b.pid))
            .then(a.device_key.cmp(&b.device_key))
    });
    devices
}

/// How often the supervisor re-scans for newly attached receivers. Workers for
/// existing receivers keep running independently; this only governs detecting a
/// receiver that was just plugged in (rare), so it can be relaxed.
const RESCAN_SECS: u64 = 20;

/// Supervise the per-receiver workers: spawn one for each receiver as it appears,
/// prune workers whose receiver vanished, and fan out UI commands to all of them.
fn spawn_supervisor(
    proxy: EventLoopProxy<UserEvent>,
    cmd_rx: mpsc::Receiver<WorkerCommand>,
    safety_secs: u64,
) {
    thread::spawn(move || {
        // pid -> (command sender, join handle) for each live worker.
        let mut workers: HashMap<u16, (mpsc::Sender<WorkerCommand>, thread::JoinHandle<()>)> =
            HashMap::new();
        let mut safety_secs = safety_secs;

        loop {
            // Drop workers whose thread has exited (receiver unplugged), so a
            // re-plugged receiver gets a fresh worker below.
            workers.retain(|_, (_, handle)| !handle.is_finished());

            // Spawn workers for any receiver we're not already tracking.
            match HidApi::new() {
                Ok(api) => {
                    for receiver in scan_receivers(&api) {
                        if workers.contains_key(&receiver.pid) {
                            continue;
                        }
                        let (tx, rx) = mpsc::channel::<WorkerCommand>();
                        let proxy = proxy.clone();
                        let handle = client::spawn_receiver_worker(
                            receiver.clone(),
                            safety_secs,
                            rx,
                            move |event| {
                                let _ = proxy.send_event(UserEvent::Device(event));
                            },
                        );
                        workers.insert(receiver.pid, (tx, handle));
                    }
                }
                Err(err) => tracing::warn!("failed initializing hidapi for scan: {err}"),
            }

            match cmd_rx.recv_timeout(Duration::from_secs(RESCAN_SECS)) {
                Ok(WorkerCommand::Exit) => {
                    for (tx, _) in workers.values() {
                        let _ = tx.send(WorkerCommand::Exit);
                    }
                    return;
                }
                Ok(cmd) => {
                    if let WorkerCommand::SetSafetyInterval(secs) = cmd {
                        safety_secs = secs;
                    }
                    for (tx, _) in workers.values() {
                        let _ = tx.send(cmd.clone());
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

/// Open the config file in the user's default editor. The file always exists by
/// the time the tray runs (created by `load_or_create_config`).
fn open_config_file() {
    let path = config::config_path();
    #[cfg(target_os = "windows")]
    {
        // explorer hands the file to its associated editor and, unlike `cmd
        // /C start`, does so without flashing a console window.
        if let Err(err) = std::process::Command::new("explorer").arg(&path).spawn() {
            tracing::warn!("failed opening config file {}: {err}", path.display());
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        tracing::warn!(
            "opening the config file is only supported on Windows: {}",
            path.display()
        );
    }
}

fn remove_item(submenu: &Submenu, item: &tray_icon::menu::MenuItemKind) -> Result<()> {
    match item {
        tray_icon::menu::MenuItemKind::MenuItem(it) => submenu.remove(it)?,
        tray_icon::menu::MenuItemKind::Submenu(it) => submenu.remove(it)?,
        tray_icon::menu::MenuItemKind::Predefined(it) => submenu.remove(it)?,
        tray_icon::menu::MenuItemKind::Check(it) => submenu.remove(it)?,
        tray_icon::menu::MenuItemKind::Icon(it) => submenu.remove(it)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ensure_selected_device;
    use crate::model::BatteryState;

    fn mk(id: &str) -> BatteryState {
        BatteryState {
            device_key: id.to_string(),
            display_name: "Mouse".to_string(),
            pid: 0xC52B,
            device_index: 1,
            battery_percent: 80,
            is_charging: false,
        }
    }

    #[test]
    fn falls_back_to_first_device() {
        let devices = vec![mk("a"), mk("b")];
        let mut selected = "missing".to_string();
        assert!(ensure_selected_device(&mut selected, &devices));
        assert_eq!(selected, "a");
    }

    #[test]
    fn no_change_when_selected_present() {
        let devices = vec![mk("a"), mk("b")];
        let mut selected = "b".to_string();
        assert!(!ensure_selected_device(&mut selected, &devices));
        assert_eq!(selected, "b");
    }
}
