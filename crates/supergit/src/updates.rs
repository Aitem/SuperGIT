//! Update checks: the latest GitHub release against the running version.
//!
//! The app checks at startup and then at most twice a day. The last check
//! is stored in `updates.json` under `settings::support_dir()`, so restarts
//! do not add checks: a start within `INTERVAL` of the last check uses the
//! release that check found.

pub mod install;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sg_github::Release;

/// Where SuperGIT's releases are published (`.github/workflows/release.yml`).
const REPOSITORY: &str = "Aitem/SuperGIT";

/// Automatic checks are at least this far apart, restarts included.
const INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

/// How often the app asks whether a check is due. Due-ness is wall-clock
/// time, because timers stop while the computer sleeps. A failed check is
/// retried on a later tick.
pub const TICK: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Unix seconds of the last successful check.
    pub checked_at: u64,
    /// The latest release that check found.
    pub latest: Option<Release>,
}

impl State {
    /// Whether an automatic check is due at `now` (Unix seconds). A clock
    /// set back past the last check makes one due too.
    pub fn due(&self, now: u64) -> bool {
        now < self.checked_at || now - self.checked_at >= INTERVAL.as_secs()
    }
}

fn file() -> Option<std::path::PathBuf> {
    Some(crate::settings::support_dir()?.join("updates.json"))
}

/// The stored state; empty when missing or unreadable.
pub fn load() -> State {
    file()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Fetch the latest release and remember it. Blocking: `gh`, else `curl`.
pub fn check() -> anyhow::Result<Release> {
    let latest = sg_github::latest_release(REPOSITORY)?;
    let state = State {
        checked_at: now(),
        latest: Some(latest.clone()),
    };
    if let Some(file) = file()
        && let Err(error) =
            crate::settings::atomic_write(&file, &serde_json::to_vec_pretty(&state)?)
    {
        eprintln!("cannot save the update check: {error:#}");
    }
    Ok(latest)
}

/// The version this build reports. Screenshot builds take
/// `SUPERGIT_DEBUG_UPDATE=<version>` instead, to show an available update.
pub fn running_version() -> String {
    #[cfg(feature = "screenshot")]
    if let Some(version) = debug_version() {
        return version;
    }
    env!("CARGO_PKG_VERSION").to_string()
}

/// Screenshot builds: `SUPERGIT_DEBUG_UPDATE=<version>` pretends to run
/// that version and checks right away, ignoring the stored state.
#[cfg(feature = "screenshot")]
pub fn debug_version() -> Option<String> {
    std::env::var("SUPERGIT_DEBUG_UPDATE").ok()
}

/// Whether `release` is newer than `running`. Tags are `vX.Y.Z` or
/// `X.Y.Z` with an optional pre-release; anything else is never newer.
pub fn is_newer(release: &Release, running: &str) -> bool {
    let parse = |v: &str| semver::Version::parse(v.trim().trim_start_matches('v')).ok();
    match (parse(&release.tag), parse(running)) {
        (Some(release), Some(running)) => release > running,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str) -> Release {
        Release {
            tag: tag.into(),
            url: format!("https://github.com/{REPOSITORY}/releases/tag/{tag}"),
            assets: Vec::new(),
        }
    }

    #[test]
    fn compares_release_tags_with_the_running_version() {
        assert!(is_newer(&release("v0.4.0"), "0.3.0"));
        assert!(is_newer(&release("0.3.1"), "0.3.0"));
        assert!(is_newer(&release("v0.10.0"), "0.9.9"));
        assert!(!is_newer(&release("v0.3.0"), "0.3.0"));
        assert!(!is_newer(&release("v0.2.0"), "0.3.0"));
        // A release beats its own release candidates, not the other way.
        assert!(is_newer(&release("v0.4.0"), "0.4.0-rc.1"));
        assert!(!is_newer(&release("v0.4.0-rc.1"), "0.4.0"));
        assert!(!is_newer(&release("nightly"), "0.3.0"));
        assert!(!is_newer(&release("v1.0"), "0.3.0"));
    }

    #[test]
    fn checks_twice_a_day_at_most() {
        let day = 24 * 60 * 60;
        let never = State::default();
        assert!(never.due(now()));
        let checked = State {
            checked_at: 10 * day,
            latest: Some(release("v0.3.0")),
        };
        assert!(!checked.due(10 * day));
        assert!(!checked.due(10 * day + day / 2 - 1));
        assert!(checked.due(10 * day + day / 2));
        // The clock went back before the last check.
        assert!(checked.due(9 * day));
    }

    #[test]
    fn stored_state_round_trips_and_tolerates_other_shapes() {
        let state = State {
            checked_at: 1_763_000_000,
            latest: Some(release("v0.3.0")),
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<State>(&json).unwrap(), state);
        let partial: State = serde_json::from_str(r#"{"checked_at":5,"future":true}"#).unwrap();
        assert_eq!(partial.checked_at, 5);
        assert_eq!(partial.latest, None);
    }
}
