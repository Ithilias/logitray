use crate::model::BatteryState;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub struct Notifier {
    threshold: u8,
    cooldown: Duration,
    last_sent: HashMap<String, Instant>,
}

impl Notifier {
    pub fn new(threshold: u8, cooldown_minutes: u64) -> Self {
        Self {
            threshold,
            cooldown: Duration::from_secs(cooldown_minutes.saturating_mul(60)),
            last_sent: HashMap::new(),
        }
    }

    fn should_notify(&self, state: &BatteryState, now: Instant) -> bool {
        if state.is_charging || state.battery_percent > self.threshold {
            return false;
        }

        if let Some(last) = self.last_sent.get(&state.device_key) {
            if now.duration_since(*last) < self.cooldown {
                return false;
            }
        }

        true
    }

    pub fn maybe_notify_low_battery(&mut self, state: &BatteryState) -> bool {
        let now = Instant::now();
        if !self.should_notify(state, now) {
            return false;
        }

        if send_toast_low_battery(state).is_ok() {
            self.last_sent.insert(state.device_key.clone(), now);
            return true;
        }

        false
    }
}

#[cfg(target_os = "windows")]
fn send_toast_low_battery(state: &BatteryState) -> anyhow::Result<()> {
    use tauri_winrt_notification::Toast;
    let app_id = toast_app_id();

    Toast::new(app_id)
        .title(&low_battery_title(state))
        .text1("Battery low — plug in charger soon")
        .show()?;

    Ok(())
}

#[cfg(target_os = "windows")]
fn toast_app_id() -> &'static str {
    use crate::APP_ID;
    use std::sync::OnceLock;
    use tauri_winrt_notification::Toast;

    static AUMID_REGISTERED: OnceLock<bool> = OnceLock::new();

    if *AUMID_REGISTERED.get_or_init(|| match register_toast_aumid() {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!("failed registering toast AUMID, falling back: {err}");
            false
        }
    }) {
        APP_ID
    } else {
        Toast::POWERSHELL_APP_ID
    }
}

#[cfg(target_os = "windows")]
fn register_toast_aumid() -> anyhow::Result<()> {
    use crate::APP_ID;
    use anyhow::Context;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let key_path = format!("SOFTWARE\\Classes\\AppUserModelId\\{APP_ID}");
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(&key_path)
        .with_context(|| format!("failed to create/open {key_path}"))?;
    key.set_value("DisplayName", &APP_ID)
        .context("failed writing DisplayName")?;
    if let Ok(exe) = std::env::current_exe() {
        let _ = key.set_value("IconUri", &exe.display().to_string());
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn send_toast_low_battery(_state: &BatteryState) -> anyhow::Result<()> {
    Ok(())
}

fn low_battery_title(state: &BatteryState) -> String {
    format!("{}: {}%", state.display_name, state.battery_percent)
}

#[cfg(test)]
mod tests {
    use super::{Notifier, low_battery_title};
    use crate::model::BatteryState;
    use std::time::{Duration, Instant};

    fn make_state(percent: u8, charging: bool) -> BatteryState {
        BatteryState {
            device_key: "test".to_string(),
            display_name: "MX Master 3".to_string(),
            pid: 0xC52B,
            device_index: 1,
            battery_percent: percent,
            is_charging: charging,
        }
    }

    #[test]
    fn low_battery_ignored_while_charging() {
        let mut notifier = Notifier::new(15, 120);
        assert!(!notifier.maybe_notify_low_battery(&make_state(10, true)));
    }

    #[test]
    fn above_threshold_ignored() {
        let mut notifier = Notifier::new(15, 120);
        assert!(!notifier.maybe_notify_low_battery(&make_state(50, false)));
    }

    #[test]
    fn cooldown_suppresses_repeat() {
        let mut notifier = Notifier::new(15, 120);
        let state = make_state(10, false);
        let now = Instant::now();
        assert!(notifier.should_notify(&state, now));
        notifier.last_sent.insert(state.device_key.clone(), now);
        assert!(!notifier.should_notify(&state, now + Duration::from_secs(30)));
        assert!(
            notifier.should_notify(&state, now + notifier.cooldown + Duration::from_secs(1))
        );
    }

    #[test]
    fn title_format() {
        assert_eq!(low_battery_title(&make_state(12, false)), "MX Master 3: 12%");
    }
}
