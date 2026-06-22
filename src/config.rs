use crate::APP_ID;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AppConfig {
    pub poll_interval_seconds: u64,
    pub low_battery_threshold: u8,
    pub low_battery_cooldown_minutes: u64,
    pub selected_device_id: String,
    pub autostart: bool,
    pub log_level: String,
    /// Tray display style: "icon" (battery glyph) or "text" (percentage number).
    #[serde(default = "default_view_mode")]
    pub view_mode: String,
    /// Whether low-battery toast notifications are shown at all.
    #[serde(default = "default_notifications_enabled")]
    pub notifications_enabled: bool,
}

fn default_view_mode() -> String {
    "icon".to_string()
}

fn default_notifications_enabled() -> bool {
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
            // Battery and charging changes are pushed via HID++ notifications, so
            // this is only a backstop re-read for missed events / resume — a few
            // minutes is plenty and keeps idle USB traffic low.
            poll_interval_seconds: 180,
            low_battery_threshold: 15,
            low_battery_cooldown_minutes: 120,
            selected_device_id: String::new(),
            autostart: false,
            log_level: "info".to_string(),
            view_mode: default_view_mode(),
            notifications_enabled: default_notifications_enabled(),
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
    use super::{AppConfig, DeviceProfile, DeviceProfiles};

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

    #[test]
    fn default_autostart_is_opt_in() {
        assert!(!AppConfig::default().autostart);
    }
}
