use crate::config;
use crate::hid::client;
use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::EnvFilter;

const LOG_FILES_TO_KEEP: usize = 3;
const MAX_LOG_FILE_BYTES: u64 = 1_048_576;

pub fn run_once() -> Result<()> {
    let cfg = config::load_or_create_config()?;
    init_logging(&cfg);

    let result = client::poll_once();

    if result.devices.is_empty() {
        println!("No supported Logitech devices found.");
    } else {
        for dev in &result.devices {
            println!(
                "{}: {}{}",
                dev.display_name,
                dev.battery_percent,
                if dev.is_charging { "% (charging)" } else { "%" }
            );
        }
    }

    if !result.errors.is_empty() {
        eprintln!("Errors:");
        for err in result.errors {
            eprintln!("  {err}");
        }
    }

    Ok(())
}

#[cfg(target_os = "windows")]
pub fn run_tray() -> Result<()> {
    // The tray is a GUI-subsystem process with no console, so an error returned
    // from here is completely invisible: no icon, no message, and nothing in the
    // log because logging is not up yet. A config we cannot read therefore
    // degrades to defaults and is reported through the log, rather than exiting
    // and leaving the user with an app that silently never appears.
    let (cfg, problem) = config::load_config_or_default();
    init_logging(&cfg);
    if let Some(err) = problem {
        tracing::warn!("config: {err:#}");
    }
    crate::tray::run_tray_app(cfg)
}

#[cfg(not(target_os = "windows"))]
pub fn run_tray() -> Result<()> {
    anyhow::bail!("tray mode is only supported on Windows")
}

fn init_logging(cfg: &config::AppConfig) {
    let filter = EnvFilter::try_new(log_directives(&cfg.log_level))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let log_path = config::log_path();

    if let Some(parent) = log_path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            eprintln!("failed creating log dir {}: {err}", parent.display());
        }
    }

    let can_open_log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .is_ok();

    if can_open_log {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_thread_ids(true)
            .with_ansi(false)
            .with_writer(LogFileWriter {
                path: log_path.clone(),
            })
            .try_init();
        tracing::info!("logging to {}", log_path.display());
    } else {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_thread_ids(true)
            .try_init();
        tracing::warn!(
            "failed to open log file {}, using stderr",
            log_path.display()
        );
    }
}

/// The levels `config.toml` documents for `log_level`.
const LOG_LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];

/// Turns a configured `log_level` into `EnvFilter` directives that apply it to
/// logitray only. Dependencies are capped at `info`, because `debug`/`trace`
/// otherwise fills the log with the update check's HTTP internals (request,
/// response headers and connection-pool churn on every check). Anything that
/// isn't one of the documented levels is passed through untouched, so a
/// hand-written directive string such as `logitray=debug,ureq=off` still works.
fn log_directives(log_level: &str) -> String {
    let level = log_level.trim().to_ascii_lowercase();
    if !LOG_LEVELS.contains(&level.as_str()) {
        return log_level.trim().to_string();
    }
    let dependencies = match level.as_str() {
        "error" => "error",
        "warn" => "warn",
        _ => "info",
    };
    format!("{dependencies},logitray={level}")
}

#[derive(Clone, Debug)]
struct LogFileWriter {
    path: PathBuf,
}

impl<'a> MakeWriter<'a> for LogFileWriter {
    type Writer = Box<dyn Write + Send + 'a>;

    fn make_writer(&'a self) -> Self::Writer {
        let _ = maybe_rotate_logs(&self.path, MAX_LOG_FILE_BYTES, LOG_FILES_TO_KEEP);
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => Box::new(file),
            Err(_) => Box::new(io::sink()),
        }
    }
}

fn maybe_rotate_logs(base: &Path, max_bytes: u64, keep: usize) -> Result<()> {
    let size = match fs::metadata(base) {
        Ok(meta) => meta.len(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).context("failed reading log metadata"),
    };

    if size < max_bytes {
        return Ok(());
    }

    if keep <= 1 {
        return Ok(());
    }

    let archive_max = keep - 1;
    let oldest = rotated_path(base, archive_max)?;
    if oldest.exists() {
        fs::remove_file(&oldest)?;
    }
    for idx in (1..archive_max).rev() {
        let src = rotated_path(base, idx)?;
        if src.exists() {
            fs::rename(&src, rotated_path(base, idx + 1)?)?;
        }
    }
    if base.exists() {
        fs::rename(base, rotated_path(base, 1)?)?;
    }

    Ok(())
}

fn rotated_path(base: &Path, index: usize) -> Result<PathBuf> {
    let parent = base.parent().context("missing parent")?;
    let name = base
        .file_name()
        .context("missing file name")?
        .to_string_lossy();
    Ok(parent.join(format!("{name}.{index}")))
}

#[cfg(test)]
mod tests {
    use super::log_directives;
    use tracing_subscriber::EnvFilter;

    #[test]
    fn documented_levels_apply_to_logitray_only() {
        let actual: Vec<_> = [
            "error", "warn", "info", "debug", "trace", "DEBUG", " debug ",
        ]
        .into_iter()
        .map(log_directives)
        .collect();
        assert_eq!(
            actual,
            [
                "error,logitray=error",
                "warn,logitray=warn",
                "info,logitray=info",
                // debug/trace stay out of the dependencies, which is the point.
                "info,logitray=debug",
                "info,logitray=trace",
                "info,logitray=debug",
                "info,logitray=debug",
            ]
        );
    }

    #[test]
    fn directive_strings_pass_through() {
        let actual: Vec<_> = ["logitray=debug,ureq=off", "logitray::hid=trace", "nonsense"]
            .into_iter()
            .map(log_directives)
            .collect();
        assert_eq!(
            actual,
            ["logitray=debug,ureq=off", "logitray::hid=trace", "nonsense"]
        );
    }

    #[test]
    fn every_produced_directive_parses() {
        for level in ["error", "warn", "info", "debug", "trace"] {
            let directives = log_directives(level);
            assert!(
                EnvFilter::try_new(&directives).is_ok(),
                "EnvFilter rejected {directives}"
            );
        }
    }
}
