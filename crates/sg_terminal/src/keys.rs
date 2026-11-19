//! Key presses as the app sees them (GPUI's key names, kept as plain strings
//! here) mapped to Ghostty's logical keys for the encoder.

use libghostty_vt::key::Key;

/// One key press.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyInput {
    /// The key's name: `a`, `1`, `-`, `enter`, `left`, `f5`, …
    pub key: String,
    /// The text the press produces with the current layout and modifiers
    /// (`A`, `ф`, `ü`), if any.
    pub text: Option<String>,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub cmd: bool,
    pub repeat: bool,
    /// macOS: Option composes characters (Option+u then e → é) rather than
    /// acting as Alt, so a press with text sends that text.
    pub option_composes: bool,
}

/// Ghostty's logical key for a key name, and the unshifted character it
/// stands for (the Kitty protocol reports it).
pub fn logical_key(name: &str) -> (Key, Option<char>) {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        let c = c.to_ascii_lowercase();
        let key = match c {
            'a'..='z' => LETTERS[usize::from(c as u8 - b'a')],
            '0'..='9' => DIGITS[usize::from(c as u8 - b'0')],
            '`' => Key::Backquote,
            '\\' => Key::Backslash,
            '[' => Key::BracketLeft,
            ']' => Key::BracketRight,
            ',' => Key::Comma,
            '=' => Key::Equal,
            '-' => Key::Minus,
            '.' => Key::Period,
            '\'' => Key::Quote,
            ';' => Key::Semicolon,
            '/' => Key::Slash,
            ' ' => Key::Space,
            _ => Key::Unidentified,
        };
        return (key, Some(c));
    }
    let key = match name {
        "enter" => Key::Enter,
        "tab" => Key::Tab,
        "space" => return (Key::Space, Some(' ')),
        "backspace" => Key::Backspace,
        "delete" => Key::Delete,
        "escape" => Key::Escape,
        "up" => Key::ArrowUp,
        "down" => Key::ArrowDown,
        "left" => Key::ArrowLeft,
        "right" => Key::ArrowRight,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "insert" => Key::Insert,
        _ => function_key(name).unwrap_or(Key::Unidentified),
    };
    (key, None)
}

const LETTERS: [Key; 26] = [
    Key::A,
    Key::B,
    Key::C,
    Key::D,
    Key::E,
    Key::F,
    Key::G,
    Key::H,
    Key::I,
    Key::J,
    Key::K,
    Key::L,
    Key::M,
    Key::N,
    Key::O,
    Key::P,
    Key::Q,
    Key::R,
    Key::S,
    Key::T,
    Key::U,
    Key::V,
    Key::W,
    Key::X,
    Key::Y,
    Key::Z,
];

const DIGITS: [Key; 10] = [
    Key::Digit0,
    Key::Digit1,
    Key::Digit2,
    Key::Digit3,
    Key::Digit4,
    Key::Digit5,
    Key::Digit6,
    Key::Digit7,
    Key::Digit8,
    Key::Digit9,
];

const FUNCTION_KEYS: [Key; 12] = [
    Key::F1,
    Key::F2,
    Key::F3,
    Key::F4,
    Key::F5,
    Key::F6,
    Key::F7,
    Key::F8,
    Key::F9,
    Key::F10,
    Key::F11,
    Key::F12,
];

fn function_key(name: &str) -> Option<Key> {
    let n: usize = name.strip_prefix('f')?.parse().ok()?;
    FUNCTION_KEYS.get(n.checked_sub(1)?).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_map_to_logical_keys() {
        assert_eq!(logical_key("a"), (Key::A, Some('a')));
        assert_eq!(logical_key("Z"), (Key::Z, Some('z')));
        assert_eq!(logical_key("7"), (Key::Digit7, Some('7')));
        assert_eq!(logical_key("enter"), (Key::Enter, None));
        assert_eq!(logical_key("f12"), (Key::F12, None));
        assert_eq!(logical_key("f13").0, Key::Unidentified);
        assert_eq!(logical_key("ф"), (Key::Unidentified, Some('ф')));
    }
}
