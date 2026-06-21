use crate::APP_ID;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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
            poll_interval_seconds: 60,
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

#[cfg(test)]
mod tests {
    use super::AppConfig;

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
