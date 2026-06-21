use crate::autostart;
use crate::config::{self, AppConfig};
use crate::hid::client;
use crate::icon;
use crate::model::{BatteryState, PollResult};
use crate::notify::Notifier;
use crate::APP_ID;
use anyhow::{Context, Result};
use hidapi::HidApi;
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
    Poll(PollResult),
}

enum WorkerCommand {
    Refresh,
    SetInterval(u64),
    Exit,
}

/// Preset choices for the menu submenus. The numeric value is encoded into each
/// item's id (e.g. "poll:60") so the event handler can parse it back.
const POLL_PRESETS: &[(&str, u64)] = &[
    ("15 seconds", 15),
    ("30 seconds", 30),
    ("1 minute", 60),
    ("2 minutes", 120),
    ("5 minutes", 300),
    ("15 minutes", 900),
];
const THRESHOLD_PRESETS: &[u8] = &[5, 10, 15, 20, 25, 30];
const COOLDOWN_PRESETS: &[(&str, u64)] = &[
    ("30 minutes", 30),
    ("1 hour", 60),
    ("2 hours", 120),
    ("4 hours", 240),
    ("8 hours", 480),
];

struct MenuHandles {
    root: Menu,
    status_item: MenuItem,
    select_submenu: Submenu,
    refresh_item: MenuItem,
    view_mode_item: CheckMenuItem,
    notify_item: CheckMenuItem,
    autostart_item: CheckMenuItem,
    open_config_item: MenuItem,
    exit_item: MenuItem,
    device_items: Vec<CheckMenuItem>,
    poll_items: Vec<CheckMenuItem>,
    threshold_items: Vec<CheckMenuItem>,
    cooldown_items: Vec<CheckMenuItem>,
}

impl MenuHandles {
    fn build(cfg: &AppConfig, initial_autostart: bool, initial_text_mode: bool) -> Result<Self> {
        let root = Menu::new();
        let status_item = MenuItem::new("No Logitech devices found", false, None);
        let select_submenu = Submenu::new("Select Device", true);
        let refresh_item = MenuItem::with_id("refresh", "Refresh now", true, None);
        let view_mode_item = CheckMenuItem::with_id(
            "viewmode",
            "Show percentage as text",
            true,
            initial_text_mode,
            None,
        );

        let (poll_submenu, poll_items) = build_preset_submenu(
            "Poll interval",
            "poll",
            POLL_PRESETS
                .iter()
                .map(|&(label, value)| (label.to_string(), value)),
            cfg.poll_interval_seconds,
        )?;
        let notify_item = CheckMenuItem::with_id(
            "notify",
            "Enable low-battery notifications",
            true,
            cfg.notifications_enabled,
            None,
        );
        let (threshold_submenu, threshold_items) = build_preset_submenu(
            "Low battery alert at",
            "threshold",
            THRESHOLD_PRESETS
                .iter()
                .map(|&n| (format!("{n}%"), u64::from(n))),
            u64::from(cfg.low_battery_threshold),
        )?;
        let (cooldown_submenu, cooldown_items) = build_preset_submenu(
            "Reminder interval",
            "cooldown",
            COOLDOWN_PRESETS
                .iter()
                .map(|&(label, value)| (label.to_string(), value)),
            cfg.low_battery_cooldown_minutes,
        )?;

        let autostart_item =
            CheckMenuItem::with_id("autostart", "Start at login", true, initial_autostart, None);
        let open_config_item = MenuItem::with_id("openconfig", "Open config file…", true, None);
        let exit_item = MenuItem::with_id("exit", "Exit", true, None);

        root.append_items(&[
            &status_item,
            &select_submenu,
            &refresh_item,
            &PredefinedMenuItem::separator(),
            &view_mode_item,
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
            status_item,
            select_submenu,
            refresh_item,
            view_mode_item,
            notify_item,
            autostart_item,
            open_config_item,
            exit_item,
            device_items: Vec::new(),
            poll_items,
            threshold_items,
            cooldown_items,
        })
    }

    fn rebuild_device_menu(&mut self, devices: &[BatteryState], selected_id: &str) -> Result<()> {
        for item in self.select_submenu.items() {
            remove_item(&self.select_submenu, &item)?;
        }
        self.device_items.clear();

        if devices.is_empty() {
            let empty = MenuItem::new("No devices", false, None);
            self.select_submenu.append(&empty)?;
            return Ok(());
        }

        for device in devices {
            let checked = device.device_key == selected_id;
            let label = format!(
                "{} — {}{}",
                device.display_name,
                device.battery_percent,
                if device.is_charging {
                    "% (charging)"
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

    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCommand>();

    spawn_poll_worker(proxy.clone(), cmd_rx, cfg.poll_interval_seconds.max(5));

    let mut text_mode = cfg.text_mode();
    let mut menu = MenuHandles::build(&cfg, autostart_enabled, text_mode)?;
    menu.touch_ids();

    let initial_icon = icon::neutral_icon()?;
    let mut tray = build_tray_icon(&menu.root, initial_icon)?;

    let mut notifier = Notifier::new(
        cfg.notifications_enabled,
        cfg.low_battery_threshold,
        cfg.low_battery_cooldown_minutes,
    );
    let mut devices: Vec<BatteryState> = Vec::new();
    let mut selected_id = cfg.selected_device_id.clone();
    // Number of consecutive polls that returned nothing. We keep showing the
    // last known reading through brief gaps (the mouse sleeping, a single
    // enumeration timeout) and only blank the tray once the device has been
    // missing for several poll cycles.
    let mut missed_polls: u32 = 0;
    const MAX_MISSED_POLLS: u32 = 3;

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
                    } else if id == "openconfig" {
                        open_config_file();
                    } else if let Some(value) = id.strip_prefix("poll:") {
                        if let Ok(secs) = value.parse::<u64>() {
                            cfg.poll_interval_seconds = secs;
                            let _ = cmd_tx.send(WorkerCommand::SetInterval(secs));
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
                        ) {
                            tracing::warn!("failed updating tray: {err}");
                        }
                    }
                }
                UserEvent::Poll(mut poll_result) => {
                    poll_result.sort_devices();
                    let PollResult { devices: new_devices, errors } = poll_result;

                    for err in errors {
                        tracing::warn!("poll error: {err}");
                    }

                    // Keep the last known reading through brief gaps: a poll that
                    // comes back empty while we still have devices is treated as a
                    // transient miss until it persists for MAX_MISSED_POLLS cycles.
                    if new_devices.is_empty() && !devices.is_empty() {
                        missed_polls += 1;
                        if missed_polls < MAX_MISSED_POLLS {
                            tracing::debug!(
                                "transient empty poll ({missed_polls}/{MAX_MISSED_POLLS}); keeping last reading"
                            );
                            return;
                        }
                    }
                    missed_polls = 0;
                    devices = new_devices;

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
                    ) {
                        tracing::warn!("failed refreshing tray: {err}");
                    }

                    for device in &devices {
                        notifier.maybe_notify_low_battery(device);
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
                "% (charging)"
            } else {
                "%"
            }
        );
        tray.set_tooltip(Some(tooltip.clone()))?;
        status_item.set_text(&tooltip);
    } else {
        tray.set_icon(Some(icon::neutral_icon()?))?;
        tray.set_tooltip(Some("No Logitech devices found"))?;
        status_item.set_text("No Logitech devices found");
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

/// Shortest wait before re-polling after a poll that found no devices. The
/// first reading at startup, or recovery after the mouse sleeps, shouldn't have
/// to wait the full poll interval.
const EMPTY_RETRY_START_SECS: u64 = 2;

fn spawn_poll_worker(
    proxy: EventLoopProxy<UserEvent>,
    cmd_rx: mpsc::Receiver<WorkerCommand>,
    poll_interval_seconds: u64,
) {
    thread::spawn(move || {
        // Live-adjustable via WorkerCommand::SetInterval; always kept at the
        // enforced 5s floor to avoid hammering the USB bus.
        let mut poll_interval_seconds = poll_interval_seconds.max(5);

        // When a poll finds nothing we retry quickly, doubling the delay each
        // time (2s, 4s, 8s, …) up to the normal interval, so a sleeping or
        // not-yet-ready device is picked up fast without hammering the USB bus
        // forever when no device is present.
        let mut empty_backoff = EMPTY_RETRY_START_SECS;

        // Create the HidApi handle and feature cache once and reuse them across
        // polls; `refresh_devices` picks up newly attached/removed receivers
        // without a full re-enumeration. The handle is recreated lazily if it
        // ever fails to initialize.
        let mut api: Option<HidApi> = None;
        let mut cache = client::FeatureCache::new();

        loop {
            if api.is_none() {
                match HidApi::new() {
                    Ok(a) => api = Some(a),
                    Err(err) => tracing::warn!("failed initializing hidapi: {err}"),
                }
            }

            let poll_result = match api.as_mut() {
                Some(a) => {
                    let _ = a.refresh_devices();
                    client::poll_devices(a, &mut cache)
                }
                None => PollResult {
                    devices: Vec::new(),
                    errors: vec!["hidapi unavailable".to_string()],
                },
            };

            let found = !poll_result.devices.is_empty();
            let _ = proxy.send_event(UserEvent::Poll(poll_result));

            let wait = if found {
                empty_backoff = EMPTY_RETRY_START_SECS;
                poll_interval_seconds
            } else {
                let w = empty_backoff.min(poll_interval_seconds);
                empty_backoff = (empty_backoff * 2).min(poll_interval_seconds);
                w
            };

            match cmd_rx.recv_timeout(Duration::from_secs(wait)) {
                Ok(WorkerCommand::Refresh) => continue,
                Ok(WorkerCommand::SetInterval(secs)) => {
                    poll_interval_seconds = secs.max(5);
                    empty_backoff = EMPTY_RETRY_START_SECS;
                    continue;
                }
                Ok(WorkerCommand::Exit) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
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
