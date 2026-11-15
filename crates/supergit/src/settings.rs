//! User settings, stored as JSON in `~/Library/Application Support/SuperGIT/`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// `~/Library/Application Support/SuperGIT`.
pub fn support_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/SuperGIT"))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    /// Follow macOS (light/dark), including live changes.
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::System => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::System,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "system" | "auto" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
}

fn file() -> Option<PathBuf> {
    Some(support_dir()?.join("settings.json"))
}

/// Settings from disk; defaults for anything missing or unreadable.
pub fn load() -> Settings {
    file()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(settings: &Settings) {
    let Some(file) = file() else {
        return;
    };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(file, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_system_and_tolerates_unknown_fields() {
        assert_eq!(Settings::default().appearance, Appearance::System);
        let s: Settings = serde_json::from_str(r#"{"appearance":"dark","future":1}"#).unwrap();
        assert_eq!(s.appearance, Appearance::Dark);
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.appearance, Appearance::System);
        assert_eq!(Appearance::System.next().next().next(), Appearance::System);
    }
}
