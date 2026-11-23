//! User settings, stored as JSON in `support_dir()`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// `~/Library/Application Support/SuperGIT` on macOS,
/// `$XDG_CONFIG_HOME/supergit` (`~/.config/supergit`) elsewhere.
pub fn support_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SUPERGIT_STATE_DIR") {
        return Some(PathBuf::from(dir));
    }
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME")?;
        return Some(PathBuf::from(home).join("Library/Application Support/SuperGIT"));
    }
    Some(xdg_dir("XDG_CONFIG_HOME", ".config")?.join("supergit"))
}

/// An XDG base directory: the variable when it holds an absolute path
/// (as the spec requires), else `~/<fallback>`.
pub fn xdg_dir(var: &str, fallback: &str) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(var).map(PathBuf::from)
        && dir.is_absolute()
    {
        return Some(dir);
    }
    Some(PathBuf::from(std::env::var_os("HOME")?).join(fallback))
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

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "system" | "auto" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }
}

/// How a diff shows the two sides of a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffLayout {
    /// One column: removed lines above added ones.
    #[default]
    Unified,
    /// Old and new side by side.
    Split,
}

/// How the agent panel shows a conversation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentView {
    /// SuperGIT's native chat (stream-json / app-server).
    #[default]
    Chat,
    /// The agent's own TUI (`claude`, `codex`) in an embedded terminal.
    Terminal,
}

impl AgentView {
    pub fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Terminal => "Terminal",
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
    pub diff_layout: DiffLayout,
    /// Show Changes and History diffs through difftastic (`difft`), when it
    /// is installed.
    pub difftastic: bool,
    /// What New / Resume open: the native chat or the agent's TUI.
    pub agent_view: AgentView,
    /// The chat's text box height in pixels, dragged by the composer's top
    /// edge; none: it grows with the text.
    pub composer_height: Option<f32>,
    /// The embedded terminal's font size in points, picked with `cmd-+` /
    /// `cmd--`; none: Ghostty's (`cmd-0`).
    pub terminal_font_size: Option<f32>,
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
        assert_eq!(s.diff_layout, DiffLayout::Unified);
        assert!(!s.difftastic);
        let s: Settings =
            serde_json::from_str(r#"{"diff_layout":"split","difftastic":true}"#).unwrap();
        assert_eq!(s.diff_layout, DiffLayout::Split);
        assert!(s.difftastic);
        assert_eq!(s.agent_view, AgentView::Chat);
        let s: Settings = serde_json::from_str(r#"{"agent_view":"terminal"}"#).unwrap();
        assert_eq!(s.agent_view, AgentView::Terminal);
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
