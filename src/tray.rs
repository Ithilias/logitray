use crate::autostart;
use crate::config::{self, AppConfig};
use crate::hid::client::{self, DeviceEvent, WorkerCommand};
use crate::hid::scanner::scan_receivers;
use crate::i18n::{Language, Text};
use crate::icon;
use crate::model::BatteryState;
use crate::notify::{self, Notifier};
use crate::shell;
use crate::update::{self, CheckerCommand, Version};
use crate::APP_ID;
use anyhow::{Context, Result};
use hidapi::HidApi;
use std::collections::{BTreeMap, HashMap, HashSet};
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
    /// The background update check found a release newer than this build.
    UpdateAvailable(Version),
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
    check_updates_item: CheckMenuItem,
    update_available_item: Option<MenuItem>,
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
        let check_updates_item = CheckMenuItem::with_id(
            "checkupdates",
            language.text(Text::CheckForUpdates),
            true,
            cfg.check_for_updates,
            None,
        );

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
            &check_updates_item,
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
            check_updates_item,
            update_available_item: None,
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

        // Auto is a mode rather than a device, so it is offered even when
        // nothing is connected. Its id shares the "device:" prefix, so the
        // click handler and `set_selected` treat it like any other entry.
        let auto = CheckMenuItem::with_id(
            format!("device:{}", config::AUTO_SUBJECT_ID),
            language.text(Text::AutomaticLowest),
            true,
            selected_id == config::AUTO_SUBJECT_ID,
            None,
        );
        self.select_submenu.append(&auto)?;
        self.device_items.push(auto);
        self.select_submenu
            .append(&PredefinedMenuItem::separator())?;

        if devices.is_empty() {
            let empty = MenuItem::new(language.text(Text::NoDevices), false, None);
            self.select_submenu.append(&empty)?;
            return Ok(());
        }

        for device in devices {
            let checked = device.device_key == selected_id;
            let label = battery_label(
                language,
                &device.display_name,
                device.battery_percent,
                device.is_charging,
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

    fn show_update(&mut self, version: Version) -> Result<()> {
        let text = update_label(self.language, version);
        match &self.update_available_item {
            Some(item) => item.set_text(text),
            None => {
                let item = MenuItem::with_id("updateavailable", text, true, None);
                // Directly under the status line.
                self.root.insert(&item, 1)?;
                self.update_available_item = Some(item);
            }
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

fn update_label(language: Language, version: Version) -> String {
    format!("{}{version}", language.text(Text::UpdateAvailable))
}

/// The `G502 X PLUS: 46%` label shared by the tray tooltip, the status line and
/// the device submenu, so the three cannot drift apart. Both unit strings come
/// from the string table, so a future language can space or place the percent
/// sign the way its typography wants.
/// How much of the Windows tooltip we are willing to fill, in UTF-16 units.
///
/// tray-icon copies up to 128 units into a `[u16; 128]` `szTip`, and at exactly
/// 128 it fills every slot and leaves no NUL terminator, so we stay clear of
/// the edge and do our own trimming rather than being chopped mid-name.
const TOOLTIP_BUDGET: usize = 120;

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `+N more`, for the devices that did not fit in the tooltip.
fn more_marker(language: Language, dropped: usize) -> String {
    language
        .text(Text::MoreDevices)
        .replace("{n}", &dropped.to_string())
}

/// One line per device for the tray tooltip, the followed device first so the
/// thing the icon represents is always the line you read first.
///
/// Trimmed to [`TOOLTIP_BUDGET`] by dropping whole trailing lines and adding a
/// `+N more` marker. The subject line is kept even if it alone overflows, since
/// there is nothing more useful to show in its place.
fn tooltip_text(language: Language, subject: &BatteryState, devices: &[BatteryState]) -> String {
    let label = |device: &BatteryState| {
        battery_label(
            language,
            &device.display_name,
            device.battery_percent,
            device.is_charging,
        )
    };

    let mut lines = vec![label(subject)];
    lines.extend(
        devices
            .iter()
            .filter(|device| device.device_key != subject.device_key)
            .map(label),
    );

    // Longest prefix of lines that still fits once its marker is accounted for.
    for keep in (1..=lines.len()).rev() {
        let mut candidate = lines[..keep].join("\n");
        if keep < lines.len() {
            candidate.push('\n');
            candidate.push_str(&more_marker(language, lines.len() - keep));
        }
        if utf16_len(&candidate) <= TOOLTIP_BUDGET {
            return candidate;
        }
    }

    lines.swap_remove(0)
}

fn battery_label(language: Language, name: &str, percent: u8, charging: bool) -> String {
    let unit = language.text(if charging {
        Text::Charging
    } else {
        Text::Percent
    });
    format!("{name}: {percent}{unit}")
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
    let update_tx = update::spawn_checker(
        cfg.check_for_updates,
        Version::current(),
        update::Schedule::default(),
        update::fetch_latest_version,
        {
            let proxy = proxy.clone();
            move |version| {
                let _ = proxy.send_event(UserEvent::UpdateAvailable(version));
            }
        },
    );

    let mut text_mode = cfg.text_mode();
    let mut available_update: Option<Version> = None;
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
        None,
        &menu.status_item,
        text_mode,
        menu.language,
        cfg.low_battery_threshold,
    ) {
        tracing::warn!("failed initializing tray: {err}");
    }
    if let Err(err) = menu.rebuild_device_menu(&[], &cfg.selected_device_id) {
        tracing::warn!("failed initializing device menu: {err}");
    }
    // Source of truth for what's currently connected, keyed by device_key. The
    // workers push explicit arrival/update/departure events, so there's no need
    // to infer absence from empty polls any more. `devices` is the sorted view
    // of `device_map` that the menu/tray rendering consumes.
    let mut device_map: BTreeMap<String, BatteryState> = BTreeMap::new();
    let mut devices: Vec<BatteryState> = Vec::new();
    let mut selected_id = cfg.selected_device_id.clone();
    let mut subject = SubjectTracker::default();

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
                            subject.pick(&devices, &selected_id, cfg.low_battery_threshold),
                            &menu.status_item,
                            text_mode,
                            menu.language,
                            cfg.low_battery_threshold,
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
                            if let Some(version) = available_update {
                                replacement.show_update(version)?;
                            }
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
                                    subject.pick(&devices, &selected_id, cfg.low_battery_threshold),
                                    &menu.status_item,
                                    text_mode,
                                    menu.language,
                                    cfg.low_battery_threshold,
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
                    } else if id == "checkupdates" {
                        // muda already toggled the check mark; read it directly.
                        cfg.check_for_updates = menu.check_updates_item.is_checked();
                        let _ = update_tx.send(CheckerCommand::SetEnabled(cfg.check_for_updates));
                        if let Err(err) = config::save_config(&cfg) {
                            tracing::warn!("failed saving config: {err}");
                        }
                    } else if id == "updateavailable" {
                        shell::open(update::LATEST_RELEASE_URL);
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
                            // The icon's color bands are derived from the
                            // threshold, so the change has to be repainted now
                            // rather than at the next device event, which with
                            // no battery activity is a whole safety re-read away.
                            if let Err(err) = refresh_tray_visuals(
                                &mut tray,
                                &devices,
                                subject.pick(&devices, &selected_id, cfg.low_battery_threshold),
                                &menu.status_item,
                                text_mode,
                                menu.language,
                                cfg.low_battery_threshold,
                            ) {
                                tracing::warn!("failed updating tray: {err}");
                            }
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
                            subject.pick(&devices, &selected_id, cfg.low_battery_threshold),
                            &menu.status_item,
                            text_mode,
                            menu.language,
                            cfg.low_battery_threshold,
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
                        DeviceEvent::ReceiverGone(pid) => {
                            forget_receiver(&mut device_map, pid);
                        }
                    }

                    devices = sorted_devices(&device_map);

                    if adopt_initial_device(&mut selected_id, &devices) {
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
                        subject.pick(&devices, &selected_id, cfg.low_battery_threshold),
                        &menu.status_item,
                        text_mode,
                        menu.language,
                        cfg.low_battery_threshold,
                    ) {
                        tracing::warn!("failed refreshing tray: {err}");
                    }
                }
                UserEvent::UpdateAvailable(version) => {
                    available_update = Some(version);
                    if let Err(err) = menu.show_update(version) {
                        tracing::warn!("failed adding update entry: {err}");
                    }
                    // The checker reports daily and on every start; toast once per release.
                    let tag = version.to_string();
                    if cfg.last_notified_update != tag {
                        let toast = notify::send_toast_update_available(
                            &update_label(menu.language, version),
                            menu.language,
                        );
                        match toast {
                            Ok(()) => {
                                cfg.last_notified_update = tag;
                                if let Err(err) = config::save_config(&cfg) {
                                    tracing::warn!("failed saving config: {err}");
                                }
                            }
                            Err(err) => tracing::warn!("failed showing update toast: {err}"),
                        }
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
    subject: Option<&BatteryState>,
    status_item: &MenuItem,
    text_mode: bool,
    language: Language,
    low_battery_threshold: u8,
) -> Result<()> {
    if let Some(device) = subject {
        let icon = if text_mode {
            icon::text_icon(
                device.battery_percent,
                device.is_charging,
                low_battery_threshold,
            )?
        } else {
            icon::battery_icon(
                device.battery_percent,
                device.is_charging,
                low_battery_threshold,
            )?
        };
        tray.set_icon(Some(icon))?;

        // The tooltip covers every device; the status line stays a single one,
        // because the Select Device submenu already lists them all and
        // newlines do not belong in a Windows menu item.
        let status = battery_label(
            language,
            &device.display_name,
            device.battery_percent,
            device.is_charging,
        );
        tray.set_tooltip(Some(tooltip_text(language, device, devices)))?;
        status_item.set_text(&status);
    } else {
        // Nothing at all, versus the chosen device being away while others are
        // present. The second case is reachable only with a pinned device,
        // because the choice is no longer silently re-pointed at whatever is
        // still connected; Auto always has a subject when anything is present.
        let message = if devices.is_empty() {
            language.text(Text::NoDevicesFound)
        } else {
            language.text(Text::SelectedDeviceOffline)
        };
        tray.set_icon(Some(icon::neutral_icon()?))?;
        tray.set_tooltip(Some(message))?;
        status_item.set_text(message);
    }

    Ok(())
}

/// How much lower a candidate's battery must be before Auto mode hands the icon
/// over. HID++ reports coarse, discrete levels, so near-ties are common and an
/// exact comparison would flap the icon between two devices as they drift.
const AUTO_SWITCH_MARGIN: u8 = 5;

/// The device the icon speaks for: the pinned one, or under
/// [`config::AUTO_SUBJECT_ID`] whichever connected device is closest to dying.
///
/// Charging devices are not candidates. Otherwise a mouse sitting on the cable
/// at 12% would take the icon, paint it blue, and hide a keyboard at 20%. When
/// everything is charging there is nothing to warn about, so the lowest overall
/// is used and the icon still shows something real.
///
/// `current` is the subject from the previous pass and is kept unless a
/// candidate is lower by at least [`AUTO_SWITCH_MARGIN`], so an exact tie leaves
/// the icon alone. A subject that starts charging or disappears leaves the pool
/// and is replaced at once.
///
/// The margin does not apply across `low_battery_threshold`: a candidate that
/// has crossed it while the subject has not takes the icon immediately, so the
/// icon cannot sit on a healthy device while a different one is low enough to
/// be firing toasts.
fn pick_subject<'a>(
    devices: &'a [BatteryState],
    selected_id: &str,
    current: Option<&str>,
    low_battery_threshold: u8,
) -> Option<&'a BatteryState> {
    if selected_id != config::AUTO_SUBJECT_ID {
        return devices
            .iter()
            .find(|device| device.device_key == selected_id);
    }

    // Both are bound before the choice: collecting inside the branch would
    // build a temporary that is dropped at the end of the expression.
    let discharging: Vec<&BatteryState> = devices
        .iter()
        .filter(|device| !device.is_charging)
        .collect();
    let all: Vec<&BatteryState> = devices.iter().collect();
    let pool = if discharging.is_empty() {
        &all
    } else {
        &discharging
    };

    // min_by_key keeps the first minimum and the caller passes `sorted_devices`
    // output, so equal levels resolve to the same device on every pass.
    let lowest = *pool.iter().min_by_key(|device| device.battery_percent)?;
    let Some(held) = current.and_then(|key| pool.iter().find(|d| d.device_key == key).copied())
    else {
        return Some(lowest);
    };

    // Hysteresis exists to stop the icon flapping between two similar readings.
    // It must not outrank the alert: once a candidate is at or below the
    // threshold and the incumbent is not, the icon belongs to the candidate,
    // whatever the gap between them.
    let crossed_alert = lowest.battery_percent <= low_battery_threshold
        && held.battery_percent > low_battery_threshold;
    let clearly_lower =
        held.battery_percent.saturating_sub(lowest.battery_percent) >= AUTO_SWITCH_MARGIN;

    if crossed_alert || clearly_lower {
        Some(lowest)
    } else {
        Some(held)
    }
}

/// The subject the icon last spoke for, which [`pick_subject`] needs to apply
/// its hysteresis. It lives here rather than in the config because under Auto it
/// changes whenever two batteries cross, and persisting every crossover would
/// churn `config.toml` over derived state.
#[derive(Default)]
struct SubjectTracker(Option<String>);

impl SubjectTracker {
    /// Re-pick the subject for the current device list and remember it.
    fn pick<'a>(
        &mut self,
        devices: &'a [BatteryState],
        selected_id: &str,
        low_battery_threshold: u8,
    ) -> Option<&'a BatteryState> {
        let picked = pick_subject(
            devices,
            selected_id,
            self.0.as_deref(),
            low_battery_threshold,
        );
        self.0 = picked.map(|device| device.device_key.clone());
        picked
    }
}

/// Adopt a device on first run, when the user has not chosen one yet.
///
/// The choice is intent, not a cache: once set it is never reassigned or
/// cleared just because the device is absent. A receiver unplugged for a
/// moment, or a mouse switched off overnight, used to re-point the tray at
/// another device (or wipe the setting) and persist that over the user's pick.
///
/// Returns `true` only when `selected_id` was newly adopted and so is worth
/// persisting.
fn adopt_initial_device(selected_id: &mut String, devices: &[BatteryState]) -> bool {
    if !selected_id.is_empty() || devices.is_empty() {
        return false;
    }

    *selected_id = devices[0].device_key.clone();
    true
}

/// Drop every device behind one receiver, for when the receiver itself is no
/// longer attached. Device keys are "PID:index" (see `device_key`), so the
/// receiver's devices are exactly those under the "PID:" prefix.
fn forget_receiver(map: &mut BTreeMap<String, BatteryState>, pid: u16) {
    let prefix = format!("{pid:04X}:");
    map.retain(|key, _| !key.starts_with(&prefix));
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
            // Which receivers are attached right now. None means the scan itself
            // failed, which must not be read as "everything went away".
            let scanned = match HidApi::new() {
                Ok(api) => Some(scan_receivers(&api)),
                Err(err) => {
                    tracing::warn!("failed initializing hidapi for scan: {err}");
                    None
                }
            };
            let present: Option<HashSet<u16>> = scanned
                .as_ref()
                .map(|receivers| receivers.iter().map(|r| r.pid).collect());

            // Retire workers that have died, and workers whose receiver is no
            // longer attached. Both need saying out loud: a worker that dies
            // mid-read never emits Gone for the devices it knew, and one with no
            // short collection has nothing that can fail, so it never dies at
            // all and would hold its slot against a re-plug forever.
            workers.retain(|pid, (tx, handle)| {
                let unplugged = present.as_ref().is_some_and(|p| !p.contains(pid));
                if !handle.is_finished() && !unplugged {
                    return true;
                }
                if unplugged {
                    tracing::info!("receiver {pid:04X} is gone, retiring its worker");
                    let _ = tx.send(WorkerCommand::Exit);
                }
                let _ = proxy.send_event(UserEvent::Device(DeviceEvent::ReceiverGone(*pid)));
                false
            });

            // Spawn workers for any receiver we're not already tracking.
            if let Some(receivers) = scanned {
                for receiver in receivers {
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
    shell::open(config::config_path());
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
    use super::{
        adopt_initial_device, battery_label, forget_receiver, pick_subject, tooltip_text,
        utf16_len, SubjectTracker, TOOLTIP_BUDGET,
    };
    use crate::config::AUTO_SUBJECT_ID;
    use crate::i18n::Language;
    use crate::model::BatteryState;
    use std::collections::BTreeMap;

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
    fn adopts_a_device_only_when_nothing_is_chosen_yet() {
        let devices = vec![mk("a"), mk("b")];
        let mut selected = String::new();
        assert!(adopt_initial_device(&mut selected, &devices));
        assert_eq!(selected, "a");
    }

    #[test]
    fn no_change_when_selected_present() {
        let devices = vec![mk("a"), mk("b")];
        let mut selected = "b".to_string();
        assert!(!adopt_initial_device(&mut selected, &devices));
        assert_eq!(selected, "b");
    }

    /// The bug this replaced: an absent device used to be swapped for
    /// `devices[0]` and the swap persisted, so unplugging a receiver for a
    /// moment re-pointed the tray at something else for good.
    #[test]
    fn absent_selection_survives_other_devices_being_present() {
        let devices = vec![mk("a"), mk("b")];
        let mut selected = "missing".to_string();
        assert!(!adopt_initial_device(&mut selected, &devices));
        assert_eq!(selected, "missing");
    }

    /// The other half: everything going away used to clear the setting.
    #[test]
    fn selection_survives_all_devices_disappearing() {
        let mut selected = "a".to_string();
        assert!(!adopt_initial_device(&mut selected, &[]));
        assert_eq!(selected, "a");
    }

    #[test]
    fn nothing_to_adopt_when_there_are_no_devices() {
        let mut selected = String::new();
        assert!(!adopt_initial_device(&mut selected, &[]));
        assert_eq!(selected, "");
    }

    /// A fresh config already reads "lowest", so first-run adoption must leave
    /// it alone rather than pinning whatever showed up first.
    #[test]
    fn auto_is_not_replaced_by_the_first_device_seen() {
        let mut selected = AUTO_SUBJECT_ID.to_string();
        assert!(!adopt_initial_device(&mut selected, &[mk("a")]));
        assert_eq!(selected, AUTO_SUBJECT_ID);
    }

    fn at(id: &str, percent: u8) -> BatteryState {
        BatteryState {
            battery_percent: percent,
            ..mk(id)
        }
    }

    fn charging_at(id: &str, percent: u8) -> BatteryState {
        BatteryState {
            is_charging: true,
            ..at(id, percent)
        }
    }

    /// 15 is the default `low_battery_threshold`, so these read the way the
    /// shipped configuration behaves.
    const THRESHOLD: u8 = 15;

    fn subject_key(
        devices: &[BatteryState],
        selected: &str,
        current: Option<&str>,
    ) -> Option<String> {
        pick_subject(devices, selected, current, THRESHOLD).map(|device| device.device_key.clone())
    }

    #[test]
    fn a_pinned_device_is_followed_whatever_its_battery() {
        let devices = vec![at("a", 90), at("b", 5)];
        assert_eq!(subject_key(&devices, "a", None).as_deref(), Some("a"));
        // Absent means no subject at all, not a silent fall back to another
        // device: that is what the "Selected device not connected" line is for.
        assert_eq!(subject_key(&devices, "missing", None), None);
    }

    #[test]
    fn auto_follows_the_lowest_battery() {
        let devices = vec![at("a", 90), at("b", 42), at("c", 12)];
        assert_eq!(
            subject_key(&devices, AUTO_SUBJECT_ID, None).as_deref(),
            Some("c")
        );
        assert_eq!(subject_key(&[], AUTO_SUBJECT_ID, None), None);
    }

    /// A mouse resting on its cable must not take the icon, paint it blue and
    /// hide a keyboard that is actually running out.
    #[test]
    fn auto_skips_charging_devices() {
        let devices = vec![charging_at("a", 5), at("b", 20)];
        assert_eq!(
            subject_key(&devices, AUTO_SUBJECT_ID, None).as_deref(),
            Some("b")
        );
    }

    #[test]
    fn auto_falls_back_to_the_lowest_when_everything_is_charging() {
        let devices = vec![charging_at("a", 60), charging_at("b", 30)];
        assert_eq!(
            subject_key(&devices, AUTO_SUBJECT_ID, None).as_deref(),
            Some("b")
        );
    }

    /// The margin is 5 points, so 40 against 36 holds and 40 against 35 gives way.
    #[test]
    fn auto_switches_only_once_a_candidate_is_clearly_lower() {
        let held = vec![at("a", 40), at("b", 36)];
        assert_eq!(
            subject_key(&held, AUTO_SUBJECT_ID, Some("a")).as_deref(),
            Some("a")
        );
        let switched = vec![at("a", 40), at("b", 35)];
        assert_eq!(
            subject_key(&switched, AUTO_SUBJECT_ID, Some("a")).as_deref(),
            Some("b")
        );
    }

    /// 0x1000 reports coarse discrete levels, so exact ties are common.
    #[test]
    fn auto_keeps_the_incumbent_on_a_tie() {
        let devices = vec![at("a", 30), at("b", 30)];
        assert_eq!(
            subject_key(&devices, AUTO_SUBJECT_ID, Some("b")).as_deref(),
            Some("b")
        );
        // With no incumbent, sorted order decides, so the pick is repeatable.
        assert_eq!(
            subject_key(&devices, AUTO_SUBJECT_ID, None).as_deref(),
            Some("a")
        );
    }

    /// Hysteresis protects a subject that is still a candidate. One that starts
    /// charging or vanishes is not, and is handed over at once.
    #[test]
    fn auto_replaces_a_subject_that_charges_or_disappears() {
        let charging = vec![charging_at("a", 10), at("b", 80)];
        assert_eq!(
            subject_key(&charging, AUTO_SUBJECT_ID, Some("a")).as_deref(),
            Some("b")
        );
        let gone = vec![at("b", 80)];
        assert_eq!(
            subject_key(&gone, AUTO_SUBJECT_ID, Some("a")).as_deref(),
            Some("b")
        );
    }

    /// The margin must not outrank the alert. The toast fires for any device at
    /// or below the threshold, so the icon has to be showing that device rather
    /// than a healthier one that happens to be within five points.
    #[test]
    fn auto_hands_over_at_once_when_a_candidate_crosses_the_threshold() {
        // 18 against 14 is inside the margin, but 14 is below the threshold and
        // 18 is not, so the icon follows the device that is actually alerting.
        let crossed = vec![at("a", 18), at("b", 14)];
        assert_eq!(
            subject_key(&crossed, AUTO_SUBJECT_ID, Some("a")).as_deref(),
            Some("b")
        );

        // Both below the threshold: no crossing, so the margin applies again and
        // the incumbent keeps the icon.
        let both_low = vec![at("a", 14), at("b", 12)];
        assert_eq!(
            subject_key(&both_low, AUTO_SUBJECT_ID, Some("a")).as_deref(),
            Some("a")
        );

        // Both above it: unchanged, the margin still holds the icon still.
        let both_fine = vec![at("a", 40), at("b", 37)];
        assert_eq!(
            subject_key(&both_fine, AUTO_SUBJECT_ID, Some("a")).as_deref(),
            Some("a")
        );
    }

    /// A charging device is not a candidate, so it cannot trigger the crossing
    /// either: the icon does not chase something that is already recovering.
    #[test]
    fn a_charging_device_below_the_threshold_does_not_take_the_icon() {
        let devices = vec![at("a", 18), charging_at("b", 5)];
        assert_eq!(
            subject_key(&devices, AUTO_SUBJECT_ID, Some("a")).as_deref(),
            Some("a")
        );
    }

    #[test]
    fn the_tracker_carries_the_subject_across_passes() {
        let mut subject = SubjectTracker::default();
        let key = |picked: Option<&BatteryState>| picked.map(|d| d.device_key.clone());

        let first = vec![at("a", 40), at("b", 80)];
        assert_eq!(
            key(subject.pick(&first, AUTO_SUBJECT_ID, THRESHOLD)).as_deref(),
            Some("a")
        );
        // b drops below a without clearing the margin, so the icon holds still.
        let second = vec![at("a", 40), at("b", 38)];
        assert_eq!(
            key(subject.pick(&second, AUTO_SUBJECT_ID, THRESHOLD)).as_deref(),
            Some("a")
        );
        let third = vec![at("a", 40), at("b", 34)];
        assert_eq!(
            key(subject.pick(&third, AUTO_SUBJECT_ID, THRESHOLD)).as_deref(),
            Some("b")
        );
    }

    /// An unplugged receiver takes its own devices with it and leaves every
    /// other receiver's alone. Before this the devices simply stayed, and under
    /// Auto the icon would happily go on speaking for absent hardware.
    #[test]
    fn forgetting_a_receiver_drops_exactly_its_devices() {
        let mut map: BTreeMap<String, BatteryState> = ["C547:1", "C547:2", "C52B:1", "C5:47"]
            .into_iter()
            .map(|key| (key.to_string(), mk(key)))
            .collect();

        forget_receiver(&mut map, 0xC547);

        let left: Vec<_> = map.keys().cloned().collect();
        // "C5:47" survives: the prefix is "C547:", so a key that merely shares
        // leading characters with the pid is not swept up with it.
        assert_eq!(left, ["C52B:1", "C5:47"]);
    }

    #[test]
    fn forgetting_an_unknown_receiver_changes_nothing() {
        let mut map: BTreeMap<String, BatteryState> =
            [("C547:1".to_string(), mk("C547:1"))].into_iter().collect();
        forget_receiver(&mut map, 0x1234);
        assert_eq!(map.len(), 1);
    }

    fn named(id: &str, name: &str, percent: u8) -> BatteryState {
        BatteryState {
            display_name: name.to_string(),
            battery_percent: percent,
            ..mk(id)
        }
    }

    #[test]
    fn tooltip_lists_the_subject_first_then_the_rest() {
        let devices = vec![
            named("a", "MX Keys", 80),
            named("b", "G502 X PLUS", 42),
            named("c", "MX Master 3S", 15),
        ];
        let tooltip = tooltip_text(Language::English, &devices[1], &devices);
        assert_eq!(tooltip, "G502 X PLUS: 42%\nMX Keys: 80%\nMX Master 3S: 15%");
    }

    #[test]
    fn tooltip_of_a_lone_device_has_no_trailing_newline() {
        let devices = vec![named("a", "G502 X PLUS", 42)];
        let tooltip = tooltip_text(Language::English, &devices[0], &devices);
        assert_eq!(tooltip, "G502 X PLUS: 42%");
    }

    /// szTip is a fixed 128-unit buffer that tray-icon fills without a
    /// terminator, so we must trim whole lines ourselves rather than let a
    /// name be chopped in half.
    #[test]
    fn tooltip_drops_whole_lines_to_stay_inside_the_budget() {
        let devices: Vec<_> = (0..6)
            .map(|i| named(&format!("d{i}"), "Logitech Device With A Long Name", 50 + i))
            .collect();
        let tooltip = tooltip_text(Language::English, &devices[0], &devices);

        assert!(
            utf16_len(&tooltip) <= TOOLTIP_BUDGET,
            "tooltip was {} units: {tooltip}",
            utf16_len(&tooltip)
        );
        // Whole lines only: nothing but the marker may be a partial entry.
        let lines: Vec<_> = tooltip.lines().collect();
        let kept = lines.len() - 1;
        assert_eq!(lines[kept], format!("+{} more", 6 - kept));
        for line in &lines[..kept] {
            assert!(line.ends_with('%'), "line was cut mid-entry: {line}");
        }
    }

    #[test]
    fn battery_label_composes_unit_from_the_string_table() {
        let actual: Vec<_> = [Language::English, Language::SimplifiedChinese]
            .into_iter()
            .flat_map(|language| {
                [false, true].map(|charging| battery_label(language, "G502 X PLUS", 46, charging))
            })
            .collect();
        assert_eq!(
            actual,
            [
                "G502 X PLUS: 46%",
                "G502 X PLUS: 46% (charging)",
                "G502 X PLUS: 46%",
                "G502 X PLUS: 46%（充电中）",
            ]
        );
    }
}
