//! Freedesktop sound theme events: `canberra-gtk-play` plays them from the
//! user's theme (and honors the desktop's event-sound switch); without it,
//! `pw-play` or `paplay` play the theme's file from the XDG data dirs.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Event sounds of the freedesktop theme that most desktops ship.
pub const SOUNDS: &[&str] = &[
    "bell",
    "complete",
    "dialog-information",
    "dialog-warning",
    "message",
    "message-new-instant",
    "phone-incoming-call",
    "service-login",
    "window-attention",
];
pub const NEEDS_YOU: &str = "message-new-instant";
pub const FINISHED: &str = "complete";

pub fn play(name: &str) {
    let name = name.to_owned();
    // The players run until the sound ends: wait for them off the UI thread.
    let spawned = std::thread::Builder::new()
        .name("sound".into())
        .spawn(move || {
            if succeeds(Command::new("canberra-gtk-play").args(["-i", &name, "-d", "SuperGIT"])) {
                return;
            }
            let Some(file) = theme_file(&name) else {
                return;
            };
            for player in ["pw-play", "paplay"] {
                if succeeds(Command::new(player).arg(&file)) {
                    return;
                }
            }
        });
    if let Err(e) = spawned {
        eprintln!("SuperGIT: cannot play a sound: {e}");
    }
}

fn succeeds(command: &mut Command) -> bool {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The theme's file for `name`: the user's data dir first, then the
/// system's (`XDG_DATA_DIRS`).
fn theme_file(name: &str) -> Option<PathBuf> {
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    crate::settings::xdg_dir("XDG_DATA_HOME", ".local/share")
        .into_iter()
        .chain(system.split(':').map(PathBuf::from))
        .flat_map(|dir| {
            ["oga", "ogg", "wav"].map(|extension| {
                dir.join("sounds/freedesktop/stereo")
                    .join(format!("{name}.{extension}"))
            })
        })
        .find(|file| file.is_file())
}
