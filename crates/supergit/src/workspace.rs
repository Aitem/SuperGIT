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
    pub kept: bool,
    pub branch_changes: bool,
    pub base: String,
    pub commit: Option<String>,
    pub changes_position: Option<ReviewPosition>,
    pub commit_position: Option<ReviewPosition>,
    /// Pull requests linked to this worktree by hand, beyond those whose
    /// head is its branch.
    pub linked_prs: Vec<u64>,
    /// Pull requests unlinked by hand even though their head is its branch.
    pub unlinked_prs: Vec<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedConversation {
    pub attachments: Vec<crate::agent_context::Attachment>,
    pub attention: crate::attention::Attention,
    pub local_id: String,
    pub provider: String,
    pub session_id: Option<String>,
    pub transcript: Option<PathBuf>,
    pub cwd: PathBuf,
    pub title: String,
    pub draft: String,
    /// Codex's approval + sandbox policy (`sg_codex::Policy`); empty for
    /// Claude, whose mode the CLI reports itself.
    pub policy: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RepositoryView {
    pub auto_cleanup: bool,
    pub protected_branches: Vec<String>,
    pub base_branch: Option<String>,
    pub pending_branch_cleanup: Vec<PendingBranchCleanup>,
    pub completed_collapsed: bool,
    pub recent_targets: Vec<String>,
    pub selected: Option<PathBuf>,
    pub worktrees: BTreeMap<PathBuf, WorktreeView>,
    pub conversations: BTreeMap<PathBuf, Vec<SavedConversation>>,
    pub active_conversations: BTreeMap<PathBuf, usize>,
    pub chat_width: f32,
    pub changes_width: f32,
    pub history_width: f32,
    /// File list beside a commit's diff in the History tab.
    pub commit_files_width: f32,
    /// The pull request list beside a PR's details.
    pub pulls_width: f32,
    pub chat_open: bool,
    /// File lists (Changes, and a commit's files in History) show a folder
    /// tree instead of flat paths.
    pub changes_tree: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingBranchCleanup {
    pub deletion: sg_git::BranchDeletion,
    pub error: String,
}

impl Default for RepositoryView {
    fn default() -> Self {
        Self {
            auto_cleanup: false,
            protected_branches: vec!["main".into(), "master".into()],
            base_branch: None,
            pending_branch_cleanup: Vec::new(),
            completed_collapsed: true,
            recent_targets: Vec::new(),
            selected: None,
            worktrees: BTreeMap::new(),
            conversations: BTreeMap::new(),
            active_conversations: BTreeMap::new(),
            chat_width: 480.,
            changes_width: 300.,
            history_width: 300.,
            commit_files_width: 240.,
            pulls_width: 320.,
            chat_open: true,
            changes_tree: false,
        }
    }
}

impl RepositoryView {
    pub fn sanitize(&mut self) {
        self.chat_width = finite(self.chat_width, 480.).clamp(320., 960.);
        self.changes_width = finite(self.changes_width, 300.).clamp(140., 600.);
        self.history_width = finite(self.history_width, 300.).clamp(160., 800.);
        self.commit_files_width = finite(self.commit_files_width, 240.).clamp(140., 600.);
        self.pulls_width = finite(self.pulls_width, 320.).clamp(200., 640.);
    }
}

/// The sidebar: a tree of the repositories open in the window (projects)
/// and their worktrees.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Projects {
    /// Repository roots, in the order the user added them.
    pub roots: Vec<PathBuf>,
    /// Collapsed, the sidebar is a narrow rail of initials.
    pub collapsed: bool,
    /// The sidebar's width, window-wide (not per project).
    pub sidebar_width: f32,
    /// Projects folded in the tree: their worktrees are hidden.
    pub folded: Vec<PathBuf>,
}

impl Default for Projects {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            collapsed: false,
            sidebar_width: 340.,
            folded: Vec::new(),
        }
    }
}

impl Projects {
    pub fn sanitize(&mut self) {
        self.sidebar_width = finite(self.sidebar_width, 340.).clamp(220., 520.);
        let mut seen = std::collections::HashSet::new();
        self.roots.retain(|root| seen.insert(root.clone()));
        let roots = &self.roots;
        let mut seen = std::collections::HashSet::new();
        self.folded
            .retain(|root| roots.contains(root) && seen.insert(root.clone()));
    }

    pub fn is_folded(&self, root: &Path) -> bool {
        self.folded.iter().any(|known| known == root)
    }

    pub fn set_folded(&mut self, root: &Path, folded: bool) {
        self.folded.retain(|known| known != root);
        if folded {
            self.folded.push(root.to_path_buf());
        }
    }

    /// Append `root` unless it is already listed.
    pub fn add(&mut self, root: &Path) {
        if !self.roots.iter().any(|known| known == root) {
            self.roots.push(root.to_path_buf());
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    pub window: Option<WindowRect>,
    pub projects: Projects,
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
            store.projects.sanitize();
            for view in store.repositories.values_mut() {
                view.sanitize();
                for conversations in view.conversations.values_mut() {
                    for conversation in conversations.iter_mut().filter(|conversation| {
                        conversation.provider.is_empty() || conversation.provider == "claude"
                    }) {
                        conversation.attention = conversation.attention.clone().restored();
                    }
                }
            }
            store
        })
        .unwrap_or_default()
}

/// Merge only the current repository, so other windows keep their own state.
pub fn save_repository(root: &Path, view: &RepositoryView) {
    let Some(path) = file() else { return };
    let mut store = load();
    store.repositories.insert(root.to_path_buf(), view.clone());
    save_store(&path, &store);
}

/// Merge the given repositories (the current one and those with live
/// conversations in the background), the sidebar's projects and the window.
pub fn save(views: &[(&Path, &RepositoryView)], projects: &Projects, window: Option<WindowRect>) {
    let Some(path) = file() else { return };
    let mut store = load();
    for (root, view) in views {
        store
            .repositories
            .insert(root.to_path_buf(), (*view).clone());
    }
    store.projects = projects.clone();
    if window.is_some() {
        store.window = window;
    }
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
        assert_eq!(state.chat_width, 960.);
        assert!(state.chat_open);
        assert!(!state.auto_cleanup);
        assert_eq!(state.protected_branches, ["main", "master"]);
        assert!(state.pending_branch_cleanup.is_empty());
        assert!(
            serde_json::from_str::<Store>("{}")
                .unwrap()
                .repositories
                .is_empty()
        );
    }

    #[test]
    fn projects_load_with_defaults_and_without_duplicates() {
        let store: Store = serde_json::from_str("{}").unwrap();
        assert!(store.projects.roots.is_empty());
        assert!(!store.projects.collapsed);
        assert_eq!(store.projects.sidebar_width, 340.);

        // `width` is the old Projects panel's: the tree ignores it.
        let mut projects: Projects = serde_json::from_str(
            r#"{"roots":["/a","/b","/a"],"width":200,"sidebar_width":9000,"folded":["/b","/gone","/b"]}"#,
        )
        .unwrap();
        projects.sanitize();
        assert_eq!(projects.roots, [PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(projects.sidebar_width, 520.);
        assert_eq!(projects.folded, [PathBuf::from("/b")]);
        assert!(projects.is_folded(Path::new("/b")));
        projects.set_folded(Path::new("/b"), false);
        projects.set_folded(Path::new("/a"), true);
        assert_eq!(projects.folded, [PathBuf::from("/a")]);
        projects.add(Path::new("/b"));
        projects.add(Path::new("/c"));
        assert_eq!(projects.roots.len(), 3);
        assert_eq!(projects.roots[2], PathBuf::from("/c"));
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
