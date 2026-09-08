use crate::i18n::Language;
use crate::model::BatteryState;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Floor for the repeat-alert cooldown.
///
/// Zero would not mean "do not repeat", it would mean no cooldown at all, since
/// nothing is ever less than Duration::ZERO. The config file is hand-editable
/// and unvalidated, so a user setting `low_battery_cooldown_minutes = 0` would
/// get a toast on every pushed battery event and every safety re-read for as
/// long as the device stayed below the threshold.
const MIN_COOLDOWN: Duration = Duration::from_secs(60);

fn cooldown_from_minutes(minutes: u64) -> Duration {
    Duration::from_secs(minutes.saturating_mul(60)).max(MIN_COOLDOWN)
}

pub struct Notifier {
    language: Language,
    enabled: bool,
    threshold: u8,
    cooldown: Duration,
    last_sent: HashMap<String, Instant>,
}

impl Notifier {
    pub fn new(language: Language, enabled: bool, threshold: u8, cooldown_minutes: u64) -> Self {
        Self {
            language,
            enabled,
            threshold,
            cooldown: cooldown_from_minutes(cooldown_minutes),
            last_sent: HashMap::new(),
        }
    }

    pub fn set_language(&mut self, language: Language) {
        self.language = language;
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn set_threshold(&mut self, threshold: u8) {
        self.threshold = threshold;
    }

    pub fn set_cooldown(&mut self, cooldown_minutes: u64) {
        self.cooldown = cooldown_from_minutes(cooldown_minutes);
    }

    fn should_notify(&self, state: &BatteryState, now: Instant) -> bool {
        if !self.enabled || state.is_charging || state.battery_percent > self.threshold {
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
        // Charging, or back above the threshold, ends this device's low-battery
        // episode. Drop its cooldown stamp so the next dip is treated as a new
        // episode rather than being silenced by the alert for the previous one.
        if state.is_charging || state.battery_percent > self.threshold {
            self.last_sent.remove(&state.device_key);
            return false;
        }

        let now = Instant::now();
        if !self.should_notify(state, now) {
            return false;
        }

        if send_toast_low_battery(state, self.language).is_ok() {
            self.last_sent.insert(state.device_key.clone(), now);
            return true;
        }

        false
    }
}

#[cfg(target_os = "windows")]
fn send_toast_low_battery(state: &BatteryState, language: Language) -> anyhow::Result<()> {
    use tauri_winrt_notification::Toast;
    let app_id = toast_app_id();

    Toast::new(app_id)
        .title(&low_battery_title(state))
        .text1(language.text(crate::i18n::Text::LowBattery))
        .show()?;

    Ok(())
}

#[cfg(target_os = "windows")]
pub fn send_toast_update_available(title: &str, language: Language) -> anyhow::Result<()> {
    use crate::i18n::Text;
    use tauri_winrt_notification::Toast;

    Toast::new(toast_app_id())
        .title(title)
        .text1(language.text(Text::UpdateToastBody))
        .on_activated(|_| {
            crate::shell::open(crate::update::LATEST_RELEASE_URL);
            Ok(())
        })
        .show()?;

    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn send_toast_update_available(_title: &str, _language: Language) -> anyhow::Result<()> {
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
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

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
fn send_toast_low_battery(_state: &BatteryState, _language: Language) -> anyhow::Result<()> {
    Ok(())
}

fn low_battery_title(state: &BatteryState) -> String {
    format!("{}: {}%", state.display_name, state.battery_percent)
}

#[cfg(test)]
mod tests {
    use super::{low_battery_title, Notifier};
    use crate::i18n::Language;
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
        let mut notifier = Notifier::new(Language::English, true, 15, 120);
        assert!(!notifier.maybe_notify_low_battery(&make_state(10, true)));
    }

    #[test]
    fn above_threshold_ignored() {
        let mut notifier = Notifier::new(Language::English, true, 15, 120);
        assert!(!notifier.maybe_notify_low_battery(&make_state(50, false)));
    }

    #[test]
    fn disabled_suppresses_notifications() {
        let notifier = Notifier::new(Language::English, false, 15, 120);
        assert!(!notifier.should_notify(&make_state(10, false), Instant::now()));
    }

    /// The cooldown exists to stop nagging inside one low-battery episode, not
    /// to span a recharge. A device that charges back up and drains again has
    /// started a new episode and must be allowed to warn.
    #[test]
    fn recharging_re_arms_the_warning() {
        let mut notifier = Notifier::new(Language::English, true, 15, 120);
        let now = Instant::now();
        let low = make_state(12, false);

        // Stand in for an alert that has just fired.
        notifier.last_sent.insert(low.device_key.clone(), now);
        assert!(!notifier.should_notify(&low, now));

        // Plugged in, then back above the threshold: the episode is over.
        assert!(!notifier.maybe_notify_low_battery(&make_state(30, true)));
        assert!(!notifier.maybe_notify_low_battery(&make_state(80, false)));

        // The next dip is a new episode, well inside the 120 minute cooldown.
        assert!(notifier.should_notify(&low, now + Duration::from_secs(60)));
    }

    /// A cooldown of zero has to mean "the shortest cooldown we allow", not "no
    /// cooldown", or the alert turns into a toast on every battery event.
    #[test]
    fn a_zero_cooldown_still_suppresses_repeats() {
        let mut notifier = Notifier::new(Language::English, true, 15, 0);
        let state = make_state(10, false);
        let now = Instant::now();

        assert!(notifier.should_notify(&state, now));
        notifier.last_sent.insert(state.device_key.clone(), now);

        // This was true before the floor, so every pushed HID++ battery event
        // and every safety re-read produced another toast.
        assert!(!notifier.should_notify(&state, now + Duration::from_secs(30)));
        assert!(notifier.should_notify(&state, now + Duration::from_secs(61)));
    }

    /// The menu presets start at 30 minutes, so the floor must not disturb them.
    #[test]
    fn a_normal_cooldown_is_left_alone() {
        let notifier = Notifier::new(Language::English, true, 15, 30);
        assert_eq!(notifier.cooldown, Duration::from_secs(30 * 60));
    }

    #[test]
    fn cooldown_suppresses_repeat() {
        let mut notifier = Notifier::new(Language::English, true, 15, 120);
        let state = make_state(10, false);
        let now = Instant::now();
        assert!(notifier.should_notify(&state, now));
        notifier.last_sent.insert(state.device_key.clone(), now);
        assert!(!notifier.should_notify(&state, now + Duration::from_secs(30)));
        assert!(notifier.should_notify(&state, now + notifier.cooldown + Duration::from_secs(1)));
    }

    #[test]
    fn title_format() {
        assert_eq!(
            low_battery_title(&make_state(12, false)),
            "MX Master 3: 12%"
        );
    }
}
