//! Update check against the GitHub releases page. Notify-only: the app never
//! downloads or replaces itself, it only points the user at the release page.

use anyhow::{anyhow, Context, Result};
use std::fmt;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Redirects to the newest release, so the tag can be read from the Location
/// header without touching the rate-limited GitHub API or parsing JSON.
pub const LATEST_RELEASE_URL: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/releases/latest");
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub fn current() -> Self {
        Self::parse_core(env!("CARGO_PKG_VERSION")).expect("Cargo guarantees a semver version")
    }

    /// Parses `1.2.3` or `v1.2.3`. Pre-release suffixes and extra components
    /// yield `None`, so unusual tags are ignored rather than compared.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let mut parts = text.split('.').map(|part| part.parse::<u64>().ok());
        let version = Self {
            major: parts.next()??,
            minor: parts.next()??,
            patch: parts.next()??,
        };
        parts.next().is_none().then_some(version)
    }

    /// Like `parse`, but drops a pre-release or build suffix first, so a
    /// `0.4.0-rc.1` build compares as 0.4.0 instead of aborting the check.
    fn parse_core(text: &str) -> Option<Self> {
        Self::parse(text.split(['-', '+']).next().unwrap_or(text))
    }

    /// Extracts the tag from a release page URL such as
    /// `https://github.com/Ithilias/logitray/releases/tag/v0.3.0`.
    pub fn from_release_url(url: &str) -> Option<Self> {
        let path = url.split(['?', '#']).next().unwrap_or(url);
        path.trim_end_matches('/')
            .rsplit('/')
            .next()
            .and_then(Self::parse)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// `Some(latest)` when `latest` is newer than `current`.
pub fn newer_version(current: Version, latest: Version) -> Option<Version> {
    (latest > current).then_some(latest)
}

pub fn fetch_latest_version() -> Result<Version> {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

    let config = ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(REQUEST_TIMEOUT))
        .user_agent(concat!("logitray/", env!("CARGO_PKG_VERSION")))
        .tls_config(
            TlsConfig::builder()
                .provider(TlsProvider::NativeTls)
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let response = agent
        .head(LATEST_RELEASE_URL)
        .call()
        .context("update check request failed")?;
    let status = response.status();
    if !status.is_redirection() {
        return Err(anyhow!(
            "unexpected status {status} from {LATEST_RELEASE_URL}"
        ));
    }
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .context("redirect without Location header")?;
    Version::from_release_url(location)
        .ok_or_else(|| anyhow!("unrecognized release URL: {location}"))
}

pub enum CheckerCommand {
    SetEnabled(bool),
}

#[derive(Clone, Copy, Debug)]
pub struct Schedule {
    /// Delay before the first check, so an autostarted instance does not race
    /// the network coming up and a first-time user can opt out beforehand.
    pub first_delay: Duration,
    pub interval: Duration,
    /// Used instead of `interval` after a failed check.
    pub retry: Duration,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            first_delay: Duration::from_secs(60),
            interval: Duration::from_secs(24 * 60 * 60),
            retry: Duration::from_secs(10 * 60),
        }
    }
}

/// Runs the periodic check on a background thread. Enabling via
/// `SetEnabled(true)` checks right away. `on_update` fires only when `fetch`
/// reports something newer than `current`.
pub fn spawn_checker(
    enabled: bool,
    current: Version,
    schedule: Schedule,
    fetch: impl Fn() -> Result<Version> + Send + 'static,
    on_update: impl Fn(Version) + Send + 'static,
) -> mpsc::Sender<CheckerCommand> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || run_checker(enabled, &rx, current, schedule, fetch, on_update));
    tx
}

fn run_checker(
    mut enabled: bool,
    rx: &mpsc::Receiver<CheckerCommand>,
    current: Version,
    schedule: Schedule,
    fetch: impl Fn() -> Result<Version>,
    on_update: impl Fn(Version),
) {
    let mut next_check = Instant::now() + schedule.first_delay;
    loop {
        let command = if enabled {
            match rx.recv_timeout(next_check.saturating_duration_since(Instant::now())) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(command) => Some(command),
                Err(_) => return,
            }
        };

        if let Some(CheckerCommand::SetEnabled(value)) = command {
            if value && !enabled {
                next_check = Instant::now();
            }
            enabled = value;
            continue;
        }

        if enabled && Instant::now() >= next_check {
            let outcome = fetch();
            // A disable that arrived during the request wins over its result.
            while let Ok(CheckerCommand::SetEnabled(value)) = rx.try_recv() {
                enabled = value;
            }
            if !enabled {
                continue;
            }
            next_check = Instant::now()
                + match outcome {
                    Ok(latest) => {
                        match newer_version(current, latest) {
                            Some(newer) => on_update(newer),
                            None => tracing::debug!(
                                "update check: {latest} is not newer than {current}"
                            ),
                        }
                        schedule.interval
                    }
                    Err(err) => {
                        tracing::warn!("update check failed: {err:#}");
                        schedule.retry
                    }
                };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::RecvTimeoutError;
    use std::sync::Arc;

    const fn v(major: u64, minor: u64, patch: u64) -> Version {
        Version {
            major,
            minor,
            patch,
        }
    }

    const CURRENT: Version = v(0, 3, 0);
    const NEWER: Version = v(0, 4, 0);

    /// Checks immediately and then effectively never again.
    fn once() -> Schedule {
        Schedule {
            first_delay: Duration::ZERO,
            interval: Duration::from_secs(3600),
            retry: Duration::from_secs(3600),
        }
    }

    fn assert_silent(rx: &mpsc::Receiver<Version>, for_ms: u64) {
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(for_ms)),
            Err(RecvTimeoutError::Timeout)
        );
    }

    fn wait_until(deadline_ms: u64, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_millis(deadline_ms);
        while !condition() {
            assert!(Instant::now() < deadline, "condition not met in time");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn parses_tags_and_release_urls() {
        let actual = [
            Version::parse("0.3.0"),
            Version::parse("v1.2.3"),
            Version::parse("v1.2"),
            Version::parse("v1.2.3.4"),
            Version::parse("v1.2.3-rc1"),
            Version::parse(""),
            Version::parse_core("0.4.0-rc.1"),
            Version::parse_core("0.4.0+build.7"),
            Version::parse_core("0.4"),
            Version::from_release_url("https://github.com/Ithilias/logitray/releases/tag/v0.3.0"),
            Version::from_release_url("https://github.com/Ithilias/logitray/releases/tag/v0.3.0/"),
            Version::from_release_url(
                "https://github.com/Ithilias/logitray/releases/tag/0.3.0?x=1#top",
            ),
            Version::from_release_url("https://github.com/Ithilias/logitray/releases"),
        ];
        assert_eq!(
            actual,
            [
                Some(v(0, 3, 0)),
                Some(v(1, 2, 3)),
                None,
                None,
                None,
                None,
                Some(v(0, 4, 0)),
                Some(v(0, 4, 0)),
                None,
                Some(v(0, 3, 0)),
                Some(v(0, 3, 0)),
                Some(v(0, 3, 0)),
                None,
            ]
        );
        assert_eq!(v(0, 3, 0).to_string(), "v0.3.0");
        assert_eq!(
            Version::current(),
            Version::parse_core(env!("CARGO_PKG_VERSION")).unwrap()
        );
    }

    #[test]
    fn newer_version_compares_numerically() {
        let actual = [
            newer_version(CURRENT, v(0, 3, 0)),
            newer_version(CURRENT, v(0, 2, 9)),
            newer_version(CURRENT, v(0, 3, 1)),
            newer_version(CURRENT, v(0, 10, 0)),
            newer_version(CURRENT, v(1, 0, 0)),
        ];
        assert_eq!(
            actual,
            [
                None,
                None,
                Some(v(0, 3, 1)),
                Some(v(0, 10, 0)),
                Some(v(1, 0, 0))
            ]
        );
    }

    #[test]
    #[ignore = "needs network access; run with --ignored"]
    fn live_fetch_returns_a_version() {
        fetch_latest_version().unwrap();
    }

    #[test]
    fn checker_reports_once_and_only_while_enabled() {
        let (found_tx, found_rx) = mpsc::channel();
        let commands = spawn_checker(
            false,
            CURRENT,
            once(),
            || Ok(NEWER),
            move |version| found_tx.send(version).unwrap(),
        );
        assert_silent(&found_rx, 200);

        commands.send(CheckerCommand::SetEnabled(true)).unwrap();
        assert_eq!(found_rx.recv_timeout(Duration::from_secs(5)), Ok(NEWER));
        // The next check is an hour away, so nothing else arrives.
        assert_silent(&found_rx, 200);

        // Re-enabling checks again right away instead of waiting out the interval.
        commands.send(CheckerCommand::SetEnabled(false)).unwrap();
        commands.send(CheckerCommand::SetEnabled(true)).unwrap();
        assert_eq!(found_rx.recv_timeout(Duration::from_secs(5)), Ok(NEWER));
    }

    #[test]
    fn checker_ignores_current_version_and_errors() {
        let (found_tx, found_rx) = mpsc::channel();
        let fetches = Arc::new(AtomicUsize::new(0));
        let _same = spawn_checker(
            true,
            CURRENT,
            once(),
            {
                let fetches = Arc::clone(&fetches);
                move || {
                    fetches.fetch_add(1, Ordering::SeqCst);
                    Ok(CURRENT)
                }
            },
            {
                let found_tx = found_tx.clone();
                move |version| found_tx.send(version).unwrap()
            },
        );
        let _failing = spawn_checker(
            true,
            CURRENT,
            once(),
            {
                let fetches = Arc::clone(&fetches);
                move || {
                    fetches.fetch_add(1, Ordering::SeqCst);
                    Err(anyhow!("offline"))
                }
            },
            move |version| found_tx.send(version).unwrap(),
        );
        // Both checks must actually have run for the silence below to mean anything.
        wait_until(5000, || fetches.load(Ordering::SeqCst) >= 2);
        assert_silent(&found_rx, 300);
    }

    #[test]
    fn checker_waits_out_the_first_delay() {
        let (found_tx, found_rx) = mpsc::channel();
        let schedule = Schedule {
            first_delay: Duration::from_millis(300),
            ..once()
        };
        let _checker = spawn_checker(
            true,
            CURRENT,
            schedule,
            || Ok(NEWER),
            move |version| found_tx.send(version).unwrap(),
        );
        assert_silent(&found_rx, 100);
        assert_eq!(found_rx.recv_timeout(Duration::from_secs(5)), Ok(NEWER));
    }

    #[test]
    fn checker_retries_soon_after_a_failure() {
        let (found_tx, found_rx) = mpsc::channel();
        let schedule = Schedule {
            first_delay: Duration::ZERO,
            interval: Duration::from_secs(10),
            retry: Duration::from_millis(30),
        };
        let fetches = Arc::new(AtomicUsize::new(0));
        let _checker = spawn_checker(
            true,
            CURRENT,
            schedule,
            {
                let fetches = Arc::clone(&fetches);
                move || match fetches.fetch_add(1, Ordering::SeqCst) {
                    0 => Err(anyhow!("network not up yet")),
                    _ => Ok(NEWER),
                }
            },
            move |version| found_tx.send(version).unwrap(),
        );
        // Well before the 10 s interval: the failure was retried.
        assert_eq!(found_rx.recv_timeout(Duration::from_secs(2)), Ok(NEWER));
        assert!(fetches.load(Ordering::SeqCst) >= 2);
    }

    #[test]
    fn checker_repeats_after_the_interval_and_stops_when_disabled() {
        let (found_tx, found_rx) = mpsc::channel();
        let schedule = Schedule {
            first_delay: Duration::ZERO,
            interval: Duration::from_millis(50),
            retry: Duration::from_secs(10),
        };
        let commands = spawn_checker(
            true,
            CURRENT,
            schedule,
            || Ok(NEWER),
            move |version| found_tx.send(version).unwrap(),
        );
        assert_eq!(found_rx.recv_timeout(Duration::from_secs(2)), Ok(NEWER));
        assert_eq!(found_rx.recv_timeout(Duration::from_secs(2)), Ok(NEWER));

        commands.send(CheckerCommand::SetEnabled(false)).unwrap();
        // Discard anything that was already in flight when we disabled.
        thread::sleep(Duration::from_millis(150));
        while found_rx.try_recv().is_ok() {}
        assert_silent(&found_rx, 300);
    }
}
