//! macOS alert sounds through `NSSound`: no process, and a sound that is
//! still playing is not started over. Works without the app bundle too.

use objc2_app_kit::NSSound;
use objc2_foundation::NSString;

/// `/System/Library/Sounds`, the list System Settings ▸ Sound offers.
pub const SOUNDS: &[&str] = &[
    "Basso",
    "Blow",
    "Bottle",
    "Frog",
    "Funk",
    "Glass",
    "Hero",
    "Morse",
    "Ping",
    "Pop",
    "Purr",
    "Sosumi",
    "Submarine",
    "Tink",
];
pub const NEEDS_YOU: &str = "Ping";
pub const FINISHED: &str = "Glass";

pub fn play(name: &str) {
    // `soundNamed` keeps the sounds it loads, so ours may go out of scope
    // while it plays. An unknown name has no sound: nothing plays.
    if let Some(sound) = NSSound::soundNamed(&NSString::from_str(name)) {
        sound.play();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_offered_sound_exists() {
        for name in SOUNDS {
            assert!(
                NSSound::soundNamed(&NSString::from_str(name)).is_some(),
                "{name} is not a system sound"
            );
        }
        assert!(SOUNDS.contains(&NEEDS_YOU) && SOUNDS.contains(&FINISHED));
    }
}
