//! User settings, stored as JSON in `~/Library/Application Support/SuperGIT/`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// `~/Library/Application Support/SuperGIT`.
pub fn support_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SUPERGIT_STATE_DIR") {
        return Some(PathBuf::from(dir));
    }
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
    pub notify_approvals: bool,
    pub notify_completions: bool,
    /// The agent new conversations start with (`claude`, `codex`): the one
    /// picked last.
    pub agent: String,
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
    if let Ok(json) = serde_json::to_string_pretty(settings)
        && let Err(error) = atomic_write(&file, json.as_bytes())
    {
        eprintln!("cannot save settings: {error}");
    }
}

/// Replace a complete JSON file atomically; drafts are private to the user.
pub fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing settings directory"))?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".supergit-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        out.write_all(bytes)?;
        out.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    Ok(result?)
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
        assert!(!s.notify_approvals && !s.notify_completions);
        assert_eq!(Appearance::System.next().next().next(), Appearance::System);
    }

    #[test]
    fn atomic_replacement_keeps_private_permissions_and_valid_json() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir =
            std::env::temp_dir().join(format!("supergit_settings_atomic_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        atomic_write(&path, br#"{"draft":"first"}"#).unwrap();
        atomic_write(&path, br#"{"draft":"second"}"#).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["draft"], "second");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
