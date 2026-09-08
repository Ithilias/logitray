use crate::APP_ID;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Sentinel `selected_device_id` meaning "follow whichever connected device has
/// the lowest battery" instead of a fixed device. Real keys are `"PID:index"`
/// (see `device_key`), so a colon-free sentinel cannot collide with one.
pub const AUTO_SUBJECT_ID: &str = "lowest";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AppConfig {
    #[serde(default)]
    pub language: crate::i18n::Language,
    #[serde(default = "default_poll_interval_seconds")]
    pub poll_interval_seconds: u64,
    #[serde(default = "default_low_battery_threshold")]
    pub low_battery_threshold: u8,
    #[serde(default = "default_low_battery_cooldown_minutes")]
    pub low_battery_cooldown_minutes: u64,
    #[serde(default = "default_selected_device_id")]
    pub selected_device_id: String,
    #[serde(default = "default_autostart")]
    pub autostart: bool,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Tray display style: "icon" (battery glyph) or "text" (percentage number).
    #[serde(default = "default_view_mode")]
    pub view_mode: String,
    /// Whether low-battery toast notifications are shown at all.
    #[serde(default = "default_notifications_enabled")]
    pub notifications_enabled: bool,
    /// Whether to look for a newer release on GitHub at startup and once a day.
    #[serde(default = "default_check_for_updates")]
    pub check_for_updates: bool,
    /// Release the update toast was last shown for, so restarts do not repeat it.
    #[serde(default)]
    pub last_notified_update: String,
}

/// Battery and charging changes are pushed via HID++ notifications, so this is
/// only a backstop re-read for missed events and resume. A few minutes is
/// plenty and keeps idle USB traffic low.
fn default_poll_interval_seconds() -> u64 {
    180
}

fn default_low_battery_threshold() -> u8 {
    15
}

fn default_low_battery_cooldown_minutes() -> u64 {
    120
}

/// Fresh installs follow the lowest battery: with one device it is the same
/// behaviour as pinning it, and with several the icon answers "is anything
/// about to die?". Configs that already name a device keep that device.
fn default_selected_device_id() -> String {
    AUTO_SUBJECT_ID.to_string()
}

fn default_autostart() -> bool {
    false
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_view_mode() -> String {
    "icon".to_string()
}

fn default_notifications_enabled() -> bool {
    true
}

fn default_check_for_updates() -> bool {
    true
}

impl AppConfig {
    /// True when the tray should render the percentage as text instead of the
    /// battery icon.
    pub fn text_mode(&self) -> bool {
        self.view_mode.eq_ignore_ascii_case("text")
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            language: crate::i18n::Language::default(),
            poll_interval_seconds: default_poll_interval_seconds(),
            low_battery_threshold: default_low_battery_threshold(),
            low_battery_cooldown_minutes: default_low_battery_cooldown_minutes(),
            selected_device_id: default_selected_device_id(),
            autostart: default_autostart(),
            log_level: default_log_level(),
            view_mode: default_view_mode(),
            notifications_enabled: default_notifications_enabled(),
            check_for_updates: default_check_for_updates(),
            last_notified_update: String::new(),
        }
    }
}

pub fn app_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join(APP_ID);
        }
    }

    if let Some(mut dir) = dirs::config_dir() {
        dir.push(APP_ID);
        return dir;
    }

    PathBuf::from(".").join(APP_ID)
}

pub fn config_path() -> PathBuf {
    app_data_dir().join("config.toml")
}

pub fn log_path() -> PathBuf {
    app_data_dir().join(format!("{APP_ID}.log"))
}

fn write_atomic(path: &Path, raw: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("missing parent directory for {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed creating {}", parent.display()))?;
    let file_name = path
        .file_name()
        .with_context(|| format!("missing file name for {}", path.display()))?
        .to_string_lossy();

    let tmp_path = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    {
        let mut tmp = fs::File::create(&tmp_path)
            .with_context(|| format!("failed creating {}", tmp_path.display()))?;
        tmp.write_all(raw)
            .with_context(|| format!("failed writing {}", tmp_path.display()))?;
        tmp.sync_all()
            .with_context(|| format!("failed syncing {}", tmp_path.display()))?;
    }

    #[cfg(target_os = "windows")]
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("failed replacing {}", path.display()))?;
    }

    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err).with_context(|| {
            format!(
                "failed renaming {} to {}",
                tmp_path.display(),
                path.display()
            )
        });
    }

    Ok(())
}

pub fn load_or_create_config() -> Result<AppConfig> {
    let path = config_path();
    if !path.exists() {
        let cfg = AppConfig::default();
        save_config(&cfg)?;
        return Ok(cfg);
    }

    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed reading {}", path.display()))?;
    let parsed: AppConfig =
        toml::from_str(&raw).with_context(|| format!("failed parsing {}", path.display()))?;
    Ok(parsed)
}

/// Where an unparsable `config.toml` is moved so falling back to defaults does
/// not cost the user the file the moment the app next saves.
pub fn invalid_config_path() -> PathBuf {
    app_data_dir().join("config.toml.invalid")
}

/// Load the config for the tray, which has no console to report to and must not
/// exit over a broken file.
///
/// A config that cannot be read or parsed is moved aside and replaced with
/// defaults, so the app still starts and the user's original is still there to
/// look at. The returned error describes what happened; the caller logs it once
/// logging is up, because at this point it is not.
pub fn load_config_or_default() -> (AppConfig, Option<anyhow::Error>) {
    match load_or_create_config() {
        Ok(cfg) => (cfg, None),
        Err(err) => {
            let cfg = AppConfig::default();
            let note = preserve_invalid_config(err);
            // Best effort: if this fails too, the app still runs on defaults.
            let _ = save_config(&cfg);
            (cfg, Some(note))
        }
    }
}

/// Move a config we could not parse out of the way, and describe the outcome.
fn preserve_invalid_config(err: anyhow::Error) -> anyhow::Error {
    let path = config_path();
    if !path.exists() {
        return anyhow!("{err:#}; starting from defaults");
    }
    let aside = invalid_config_path();
    match fs::rename(&path, &aside) {
        Ok(()) => anyhow!(
            "{err:#}; moved it to {} and starting from defaults",
            aside.display()
        ),
        Err(rename_err) => {
            anyhow!("{err:#}; could not move it aside ({rename_err}), starting from defaults")
        }
    }
}

pub fn save_config(cfg: &AppConfig) -> Result<()> {
    let path = config_path();
    let raw = toml::to_string_pretty(cfg).context("failed serializing config")?;
    write_atomic(&path, raw.as_bytes())?;
    Ok(())
}

/// What we learned about a device the first time we enumerated it — which HID++
/// 2.0 battery feature to use and its table index, plus the display name. This
/// is the slow, multi-round-trip part of talking to a freshly-woken device, so
/// we persist it keyed by wireless product id (WPID) and reuse it across boots:
/// on the next cold start we can read the battery directly instead of running
/// (and possibly failing) enumeration while the device is still half-asleep.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeviceProfile {
    pub battery_feature_id: u16,
    pub battery_feature_index: u8,
    pub name: String,
}

/// Persisted map of WPID (lower-case hex, e.g. "4099") -> [`DeviceProfile`].
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DeviceProfiles {
    #[serde(default)]
    pub devices: HashMap<String, DeviceProfile>,
}

impl DeviceProfiles {
    /// Format a WPID as the map key.
    pub fn key(wpid: u16) -> String {
        format!("{wpid:04x}")
    }

    pub fn get(&self, wpid: u16) -> Option<&DeviceProfile> {
        self.devices.get(&Self::key(wpid))
    }

    /// Insert/replace a profile, returning true if it changed (so the caller can
    /// avoid a redundant disk write).
    pub fn upsert(&mut self, wpid: u16, profile: DeviceProfile) -> bool {
        match self.devices.get(&Self::key(wpid)) {
            Some(existing)
                if existing.battery_feature_id == profile.battery_feature_id
                    && existing.battery_feature_index == profile.battery_feature_index
                    && existing.name == profile.name =>
            {
                false
            }
            _ => {
                self.devices.insert(Self::key(wpid), profile);
                true
            }
        }
    }
}

pub fn device_profiles_path() -> PathBuf {
    app_data_dir().join("devices.toml")
}

/// Load the persisted device profiles, returning an empty set if the file is
/// missing or unreadable — this is a best-effort cache, never a hard dependency.
pub fn load_device_profiles() -> DeviceProfiles {
    let path = device_profiles_path();
    let Ok(raw) = fs::read_to_string(path) else {
        return DeviceProfiles::default();
    };
    toml::from_str(&raw).unwrap_or_default()
}

pub fn save_device_profiles(profiles: &DeviceProfiles) -> Result<()> {
    let raw = toml::to_string_pretty(profiles).context("failed serializing device profiles")?;
    write_atomic(&device_profiles_path(), raw.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AppConfig, DeviceProfile, DeviceProfiles, AUTO_SUBJECT_ID};

    /// Serializes the default config without `key`, appends `line` if given,
    /// then loads, saves and reloads it, returning both parsed configs.
    fn reload_with(key: &str, line: Option<String>) -> (AppConfig, AppConfig) {
        let raw = toml::to_string(&AppConfig::default()).unwrap();
        let mut text = raw
            .lines()
            .filter(|l| !l.starts_with(&format!("{key} =")))
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(line) = line {
            text.push('\n');
            text.push_str(&line);
            text.push('\n');
        }
        let cfg: AppConfig = toml::from_str(&text).unwrap();
        let saved = toml::to_string(&cfg).unwrap();
        let restored: AppConfig = toml::from_str(&saved).unwrap();
        (cfg, restored)
    }

    #[test]
    fn language_config_compatibility() {
        use crate::i18n::Language;
        let actual: Vec<_> = [None, Some("auto"), Some("en"), Some("zh-CN"), Some("fr")]
            .into_iter()
            .map(|value| {
                let line = value.map(|value| format!("language = \"{value}\""));
                let (cfg, restored) = reload_with("language", line);
                (cfg.language, restored.language)
            })
            .collect();
        assert_eq!(
            actual,
            [
                (Language::Auto, Language::Auto),
                (Language::Auto, Language::Auto),
                (Language::English, Language::English),
                (Language::SimplifiedChinese, Language::SimplifiedChinese),
                (Language::English, Language::English),
            ]
        );
    }

    #[test]
    fn check_for_updates_defaults_on_and_roundtrips() {
        let actual: Vec<_> = [None, Some(true), Some(false)]
            .into_iter()
            .map(|value| {
                let line = value.map(|value| format!("check_for_updates = {value}"));
                let (cfg, restored) = reload_with("check_for_updates", line);
                (cfg.check_for_updates, restored.check_for_updates)
            })
            .collect();
        assert_eq!(actual, [(true, true), (true, true), (false, false)]);
    }

    #[test]
    fn last_notified_update_defaults_empty_and_roundtrips() {
        let actual: Vec<_> = [None, Some("v0.4.0")]
            .into_iter()
            .map(|value| {
                let line = value.map(|value| format!("last_notified_update = \"{value}\""));
                let (cfg, restored) = reload_with("last_notified_update", line);
                (cfg.last_notified_update, restored.last_notified_update)
            })
            .collect();
        assert_eq!(
            actual,
            [
                (String::new(), String::new()),
                ("v0.4.0".to_string(), "v0.4.0".to_string()),
            ]
        );
    }

    #[test]
    fn device_profiles_roundtrip_and_upsert() {
        let mut profiles = DeviceProfiles::default();
        assert!(profiles.upsert(
            0x4099,
            DeviceProfile {
                battery_feature_id: 0x1001,
                battery_feature_index: 6,
                name: "G Pro".to_string(),
            },
        ));
        // Re-inserting the identical profile reports "no change".
        assert!(!profiles.upsert(
            0x4099,
            DeviceProfile {
                battery_feature_id: 0x1001,
                battery_feature_index: 6,
                name: "G Pro".to_string(),
            },
        ));

        let raw = toml::to_string_pretty(&profiles).expect("serialize profiles");
        let parsed: DeviceProfiles = toml::from_str(&raw).expect("parse profiles");
        let p = parsed.get(0x4099).expect("profile present after roundtrip");
        assert_eq!(p.battery_feature_id, 0x1001);
        assert_eq!(p.battery_feature_index, 6);
        assert_eq!(p.name, "G Pro");
        assert!(parsed.get(0x1234).is_none());
    }

    #[test]
    fn config_toml_roundtrip() {
        let cfg = AppConfig::default();
        let raw = toml::to_string_pretty(&cfg).expect("serialize config");
        let parsed: AppConfig = toml::from_str(&raw).expect("parse config");
        assert_eq!(parsed.poll_interval_seconds, cfg.poll_interval_seconds);
        assert_eq!(parsed.low_battery_threshold, cfg.low_battery_threshold);
        assert_eq!(parsed.autostart, cfg.autostart);
        assert_eq!(parsed.view_mode, cfg.view_mode);
        assert_eq!(parsed.notifications_enabled, cfg.notifications_enabled);
    }

    /// A missing key must cost that key's value, not the whole file. These six
    /// had no serde default, so deleting a single line by hand made the config
    /// unparsable and the tray exited without a word.
    #[test]
    fn a_missing_key_falls_back_to_its_own_default() {
        let d = AppConfig::default();
        let missing = |key: &str| reload_with(key, None).0;
        assert_eq!(
            missing("poll_interval_seconds").poll_interval_seconds,
            d.poll_interval_seconds
        );
        assert_eq!(
            missing("low_battery_threshold").low_battery_threshold,
            d.low_battery_threshold
        );
        assert_eq!(
            missing("low_battery_cooldown_minutes").low_battery_cooldown_minutes,
            d.low_battery_cooldown_minutes
        );
        assert_eq!(
            missing("selected_device_id").selected_device_id,
            d.selected_device_id
        );
        assert_eq!(missing("autostart").autostart, d.autostart);
        assert_eq!(missing("log_level").log_level, d.log_level);
    }

    /// The limit case of the above: even an empty file is a usable config, so
    /// there is no longer any content that stops the tray from starting.
    #[test]
    fn an_empty_config_parses_as_all_defaults() {
        let cfg: AppConfig = toml::from_str("").expect("empty config parses");
        let d = AppConfig::default();
        assert_eq!(cfg.selected_device_id, d.selected_device_id);
        assert_eq!(cfg.poll_interval_seconds, d.poll_interval_seconds);
        assert_eq!(cfg.low_battery_threshold, d.low_battery_threshold);
        assert_eq!(cfg.log_level, d.log_level);
        assert_eq!(cfg.autostart, d.autostart);
    }

    #[test]
    fn fresh_configs_follow_the_lowest_battery() {
        assert_eq!(AppConfig::default().selected_device_id, AUTO_SUBJECT_ID);
    }

    /// The sentinel, a real device key and the empty string left by older
    /// versions all have to survive a load/save/reload cycle unchanged.
    #[test]
    fn selected_device_id_roundtrips() {
        let actual: Vec<_> = [AUTO_SUBJECT_ID, "C547:1", ""]
            .into_iter()
            .map(|value| {
                let line = format!("selected_device_id = \"{value}\"");
                let (cfg, restored) = reload_with("selected_device_id", Some(line));
                (cfg.selected_device_id, restored.selected_device_id)
            })
            .collect();
        assert_eq!(
            actual,
            [
                ("lowest".to_string(), "lowest".to_string()),
                ("C547:1".to_string(), "C547:1".to_string()),
                (String::new(), String::new()),
            ]
        );
    }

    #[test]
    fn default_autostart_is_opt_in() {
        assert!(!AppConfig::default().autostart);
    }
}
