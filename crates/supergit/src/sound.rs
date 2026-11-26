//! Sounds for agent events: an agent needs the user (approval, question)
//! or finished (done, failed). macOS plays its alert sounds (`NSSound`,
//! `/System/Library/Sounds`); Linux the freedesktop sound theme
//! (`canberra-gtk-play`, else `pw-play` / `paplay` with the theme's file).
//! Playing never blocks the main thread, and a burst of events (several
//! agents ending together) makes one sound, not a chord.

use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod freedesktop;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use freedesktop as platform;
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "freebsd")))]
mod platform {
    pub const SOUNDS: &[&str] = &[];
    pub const NEEDS_YOU: &str = "";
    pub const FINISHED: &str = "";
    pub fn play(_name: &str) {}
}

/// The sounds offered in Settings, by the name the platform plays them by.
pub const SOUNDS: &[&str] = platform::SOUNDS;

/// Sounds closer together than this make one: the first.
const GAP: Duration = Duration::from_secs(1);

static LAST: Mutex<Option<Instant>> = Mutex::new(None);

/// The sound an event plays until the user picks another: `needs_you` for
/// approvals and questions, else for finished and failed turns.
pub fn default(needs_you: bool) -> &'static str {
    if needs_you {
        platform::NEEDS_YOU
    } else {
        platform::FINISHED
    }
}

/// "message-new-instant" → "Message new instant"; macOS names stay.
pub fn label(name: &str) -> String {
    let mut chars = name.chars();
    chars
        .next()
        .map_or_else(String::new, |first| {
            first.to_uppercase().chain(chars).collect::<String>()
        })
        .replace('-', " ")
}

/// Play an event's sound, unless one played within the last second.
/// Screenshot runs stay silent: they start agents in unfocused windows.
pub fn play(name: &str) {
    if name.is_empty() || std::env::var_os("SUPERGIT_SCREENSHOT").is_some() {
        return;
    }
    let now = Instant::now();
    let Ok(mut last) = LAST.lock() else {
        return;
    };
    if !due(*last, now) {
        return;
    }
    *last = Some(now);
    drop(last);
    platform::play(name);
}

/// Play a sound picked in Settings at once, so the user hears it.
pub fn preview(name: &str) {
    if !name.is_empty() {
        platform::play(name);
    }
}

fn due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= GAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_read_as_words() {
        assert_eq!(label("message-new-instant"), "Message new instant");
        assert_eq!(label("Glass"), "Glass");
        assert_eq!(label(""), "");
    }

    #[test]
    fn a_burst_makes_one_sound() {
        let start = Instant::now();
        assert!(due(None, start));
        assert!(!due(Some(start), start + Duration::from_millis(300)));
        assert!(due(Some(start), start + GAP));
    }

    #[test]
    fn defaults_are_offered() {
        for needs_you in [true, false] {
            let name = default(needs_you);
            assert!(name.is_empty() || SOUNDS.contains(&name), "{name}");
        }
    }
}
