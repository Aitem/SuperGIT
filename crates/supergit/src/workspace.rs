//! Restorable UI metadata. Transcripts remain owned by their agent provider.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct WindowRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl WindowRect {
    pub fn clamped(self, display: Self) -> Self {
        let width = finite(self.width, 1440.).clamp(1100_f32.min(display.width), display.width);
        let height = finite(self.height, 900.).clamp(600_f32.min(display.height), display.height);
        Self {
            x: finite(self.x, display.x).clamp(display.x, display.x + display.width - width),
            y: finite(self.y, display.y).clamp(display.y, display.y + display.height - height),
            width,
            height,
        }
    }
}

fn finite(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewPosition {
    pub path: String,
    pub line: usize,
    pub original: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeView {
    pub branch_changes: bool,
    pub base: String,
    pub tab: String,
    pub commit: Option<String>,
    pub changes_position: Option<ReviewPosition>,
    pub commit_position: Option<ReviewPosition>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedConversation {
    pub local_id: String,
    pub provider: String,
    pub session_id: Option<String>,
    pub transcript: Option<PathBuf>,
    pub cwd: PathBuf,
    pub title: String,
    pub draft: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RepositoryView {
    pub recent_targets: Vec<String>,
    pub selected: Option<PathBuf>,
    pub worktrees: BTreeMap<PathBuf, WorktreeView>,
    pub conversations: BTreeMap<PathBuf, Vec<SavedConversation>>,
    pub active_conversations: BTreeMap<PathBuf, usize>,
    pub sidebar_width: f32,
    pub chat_width: f32,
    pub changes_width: f32,
    pub history_width: f32,
    pub chat_open: bool,
    /// The Changes tab lists files as a folder tree instead of flat paths.
    pub changes_tree: bool,
}

impl Default for RepositoryView {
    fn default() -> Self {
        Self {
            recent_targets: Vec::new(),
            selected: None,
            worktrees: BTreeMap::new(),
            conversations: BTreeMap::new(),
            active_conversations: BTreeMap::new(),
            sidebar_width: 320.,
            chat_width: 480.,
            changes_width: 300.,
            history_width: 300.,
            chat_open: true,
            changes_tree: false,
        }
    }
}

impl RepositoryView {
    pub fn sanitize(&mut self) {
        self.sidebar_width = finite(self.sidebar_width, 320.).clamp(220., 520.);
        self.chat_width = finite(self.chat_width, 480.).clamp(320., 960.);
        self.changes_width = finite(self.changes_width, 300.).clamp(140., 600.);
        self.history_width = finite(self.history_width, 300.).clamp(160., 800.);
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    pub window: Option<WindowRect>,
    pub repositories: BTreeMap<PathBuf, RepositoryView>,
}

fn file() -> Option<PathBuf> {
    Some(crate::settings::support_dir()?.join("workspace.json"))
}

pub fn load() -> Store {
    file()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Store>(&bytes).ok())
        .map(|mut store| {
            for view in store.repositories.values_mut() {
                view.sanitize();
            }
            store
        })
        .unwrap_or_default()
}

/// Merge only the current repository, so other windows keep their own state.
pub fn save_repository(root: &Path, view: &RepositoryView, window: Option<WindowRect>) {
    let Some(path) = file() else { return };
    let mut store = load();
    store.repositories.insert(root.to_path_buf(), view.clone());
    if window.is_some() {
        store.window = window;
    }
    save_store(&path, &store);
}

pub fn save_window(window: WindowRect) {
    let Some(path) = file() else { return };
    let mut store = load();
    store.window = Some(window);
    save_store(&path, &store);
}

fn save_store(path: &Path, store: &Store) {
    let result = serde_json::to_vec_pretty(store)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| crate::settings::atomic_write(path, &bytes));
    if let Err(error) = result {
        eprintln!("cannot save workspace: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_and_partial_state_load_with_defaults() {
        let mut state: RepositoryView =
            serde_json::from_str(r#"{"chat_width":9999,"future":true}"#).unwrap();
        state.sanitize();
        assert_eq!(state.sidebar_width, 320.);
        assert_eq!(state.chat_width, 960.);
        assert!(state.chat_open);
        assert!(
            serde_json::from_str::<Store>("{}")
                .unwrap()
                .repositories
                .is_empty()
        );
    }

    #[test]
    fn offscreen_and_oversized_windows_fit_the_display() {
        let screen = WindowRect {
            x: 0.,
            y: 0.,
            width: 1280.,
            height: 800.,
        };
        let restored = WindowRect {
            x: 4000.,
            y: -1000.,
            width: 2000.,
            height: 1200.,
        }
        .clamped(screen);
        assert_eq!(restored, screen);
    }

    #[test]
    fn conversations_keep_provider_session_and_draft_separate() {
        let saved = SavedConversation {
            provider: "claude".into(),
            session_id: Some("session-1".into()),
            draft: "unsent question".into(),
            cwd: PathBuf::from("/work/repo"),
            ..Default::default()
        };
        let restored: SavedConversation =
            serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
        assert_eq!(restored.provider, "claude");
        assert_eq!(restored.draft, "unsent question");
        assert_eq!(restored.session_id.as_deref(), Some("session-1"));
    }
}
