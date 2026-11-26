//! Data loaded in the background and handed to the UI as plain values.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use sg_agents::{AgentState, Session, SessionIndex};
use sg_codex::CodexSessionIndex;
use sg_git::{Repo, Worktree, WorktreeStatus};

pub(super) mod history;
pub use history::{History, HistorySource};

/// A project's folder name.
pub fn project_name(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string())
}

/// What a project is: a Git repository, or a plain folder (a workspace
/// holding several repositories, notes, anything) where agents and the
/// terminal work without Git.
#[derive(Clone, Debug)]
pub enum Project {
    Repo(Repo),
    Folder(PathBuf),
}

impl Project {
    /// The project at `path`: the repository holding it, else the folder
    /// itself.
    pub fn open(path: &Path) -> Result<Self> {
        if !path.is_dir() {
            anyhow::bail!("{} is not a folder", path.display());
        }
        Ok(match Repo::find(path)? {
            Some(repo) => Self::Repo(repo),
            None => Self::Folder(path.to_path_buf()),
        })
    }

    pub fn root(&self) -> &Path {
        match self {
            Self::Repo(repo) => &repo.root,
            Self::Folder(root) => root,
        }
    }

    /// Its places to work: a repository's worktrees, or the folder alone.
    pub fn load(&self, index: &SharedSessionIndex) -> Result<Vec<WorktreeInfo>> {
        match self {
            Self::Repo(repo) => load_worktrees(repo, index),
            Self::Folder(root) => load_folder(root, index),
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorktreeInfo {
    pub wt: Worktree,
    pub status: Result<WorktreeStatus, String>,
    pub sessions: Vec<Session>,
    pub cleanup_identity: Option<sg_git::WorktreeIdentity>,
    /// A folder project's folder: no Git, so no branch, changes or history.
    pub folder: bool,
}

impl WorktreeInfo {
    pub fn path(&self) -> &Path {
        &self.wt.path
    }

    /// Its name in the UI: the branch (a short sha when detached), or a
    /// folder's own name.
    pub fn name(&self) -> String {
        if self.folder {
            return project_name(&self.wt.path);
        }
        self.wt.display_name()
    }

    /// The most attention-worthy state among this worktree's sessions.
    pub fn agent_state(&self) -> Option<AgentState> {
        self.sessions
            .iter()
            .map(|s| s.state)
            .min()
            .filter(|s| s.is_active())
    }
}

/// Sessions of every agent, discovered from the transcripts each one writes.
#[derive(Default)]
pub struct AgentSessions {
    claude: SessionIndex,
    codex: CodexSessionIndex,
}

impl AgentSessions {
    /// Sessions for each of `worktrees` (same order), most recent first.
    pub fn sessions_by_worktree(&mut self, worktrees: &[PathBuf]) -> Vec<Vec<Session>> {
        // Separate indexes reading separate files: scan both at once.
        let Self { claude, codex } = self;
        let (claude, codex) = sg_git::parallel::join(
            || claude.sessions_by_worktree(worktrees),
            || codex.sessions_by_worktree(worktrees),
        );
        claude
            .into_iter()
            .zip(codex)
            .map(|(mut list, codex)| {
                list.extend(codex);
                list.sort_by_key(|s| std::cmp::Reverse(s.modified));
                list
            })
            .collect()
    }
}

pub type SharedSessionIndex = Arc<Mutex<AgentSessions>>;

/// The view already names the operation; Git's stderr explains what failed.
pub fn git_error_message(error: anyhow::Error) -> String {
    if let Some(git) = error.downcast_ref::<sg_git::GitError>()
        && !git.stderr.trim().is_empty()
    {
        return git.stderr.trim().to_string();
    }
    error.to_string()
}

/// Every worktree with its status and agent sessions. With dozens of
/// worktrees this is the hot path of each refresh: statuses run several at
/// a time, next to the session scan (it needs only the paths).
pub fn load_worktrees(repo: &Repo, index: &SharedSessionIndex) -> Result<Vec<WorktreeInfo>> {
    let worktrees = sg_git::list_worktrees(&repo.root)?;
    let paths: Vec<PathBuf> = worktrees.iter().map(|w| w.path.clone()).collect();
    let (statuses, sessions) = sg_git::parallel::join(
        || {
            sg_git::parallel::map(&worktrees, |wt| {
                let status = if wt.is_bare {
                    Err("bare".to_string())
                } else if let Some(reason) = &wt.prunable {
                    Err(format!("prunable: {reason}"))
                } else {
                    sg_git::polled_status(&wt.path).map_err(git_error_message)
                };
                let identity = sg_git::worktree_identity(&repo.common_dir, wt).ok();
                (status, identity)
            })
            .into_iter()
            .map(|result| result.unwrap_or_else(|_| (Err("status panicked".into()), None)))
            .collect::<Vec<_>>()
        },
        || {
            index
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .sessions_by_worktree(&paths)
        },
    );
    Ok(worktrees
        .into_iter()
        .zip(statuses)
        .zip(sessions)
        .map(
            |((wt, (status, cleanup_identity)), sessions)| WorktreeInfo {
                wt,
                status,
                sessions,
                cleanup_identity,
                folder: false,
            },
        )
        .collect())
}

/// A folder project's one place to work: the folder itself, in the role of
/// a main checkout without Git (no branch, nothing changed), with the agent
/// sessions that started in it.
pub fn load_folder(root: &Path, index: &SharedSessionIndex) -> Result<Vec<WorktreeInfo>> {
    if !root.is_dir() {
        anyhow::bail!("{} is not a folder anymore", root.display());
    }
    let mut sessions = index
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .sessions_by_worktree(&[root.to_path_buf()])
        .pop()
        .unwrap_or_default();
    sessions.retain(|session| started_in(session, root));
    Ok(vec![WorktreeInfo {
        wt: Worktree {
            path: root.to_path_buf(),
            head: None,
            branch: None,
            is_main: true,
            is_bare: false,
            is_detached: false,
            locked: None,
            prunable: None,
        },
        status: Ok(WorktreeStatus::default()),
        sessions,
        cleanup_identity: None,
        folder: true,
    }])
}

/// A folder holds whole repositories (a workspace): a session that started
/// in one of them, or in any other folder under it, is not the folder's.
/// Its own sessions start in it (and may move on, `cd` into a repository).
fn started_in(session: &Session, root: &Path) -> bool {
    session
        .start_cwd
        .as_ref()
        .or(session.cwd.as_ref())
        .map(PathBuf::as_path)
        == Some(root)
}

/// A successful empty result is different from loading or a failed refresh.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LoadState<T> {
    #[default]
    Loading,
    Ready(T),
    Failed {
        error: String,
        previous: Option<T>,
    },
}

impl<T> LoadState<T> {
    pub fn from_result(result: Result<T, String>) -> Self {
        match result {
            Ok(value) => Self::Ready(value),
            Err(error) => Self::Failed {
                error,
                previous: None,
            },
        }
    }

    /// Includes a last successful value, which must be labeled stale in the UI.
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Ready(value) => Some(value),
            Self::Failed { previous, .. } => previous.as_ref(),
            Self::Loading => None,
        }
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Failed { error, .. } => Some(error),
            _ => None,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading)
    }
}

impl<T: Clone> LoadState<T> {
    pub fn update(&mut self, next: Self) {
        *self = match next {
            Self::Failed { error, .. } => Self::Failed {
                error,
                previous: self.value().cloned(),
            },
            next => next,
        };
    }
}

/// What the main panel shows for the selected worktree. Each part can fail
/// independently, without making the other parts look empty or successful.
#[derive(Clone, Debug, Default)]
pub struct Details {
    pub path: PathBuf,
    pub scope: sg_git::ChangeScope,
    pub base: Option<String>,
    pub suggested_base: Option<String>,
    pub merge_base: Option<String>,
    pub files: LoadState<Vec<sg_git::FileChange>>,
    pub status: LoadState<WorktreeStatus>,
    pub patch: LoadState<String>,
    /// The submodules `patch` changes, their own diffs inlined in it.
    pub submodules: Vec<sg_git::SubmoduleChange>,
}

impl Details {
    pub fn update(&mut self, next: Self) {
        if self.path != next.path || self.base != next.base || self.scope != next.scope {
            *self = next;
            return;
        }
        self.status.update(next.status);
        self.files.update(next.files);
        self.merge_base = next.merge_base;
        self.suggested_base = next.suggested_base;
        // They describe the patch: keep them with the last one that loaded.
        if next.patch.value().is_some() {
            self.submodules = next.submodules;
        }
        self.patch.update(next.patch);
    }
}

pub fn load_details(
    path: &Path,
    status: Result<WorktreeStatus, String>,
    base: Option<String>,
    scope: sg_git::ChangeScope,
) -> Details {
    // The suggestion is a separate lookup: next to the diff, not before it.
    let (suggested_base, changes) = sg_git::parallel::join(
        || sg_git::suggested_base(path),
        || changes(path, &status, base.as_deref(), scope),
    );
    Details {
        path: path.to_path_buf(),
        scope,
        base,
        suggested_base,
        merge_base: changes.merge_base,
        files: LoadState::from_result(changes.files),
        status: LoadState::from_result(status),
        patch: LoadState::from_result(changes.patch),
        submodules: changes.submodules,
    }
}

/// What `load_details` shows of a worktree's changes.
struct Changes {
    files: Result<Vec<sg_git::FileChange>, String>,
    merge_base: Option<String>,
    submodules: Vec<sg_git::SubmoduleChange>,
    patch: Result<String, String>,
}

fn changes(
    path: &Path,
    status: &Result<WorktreeStatus, String>,
    base: Option<&str>,
    scope: sg_git::ChangeScope,
) -> Changes {
    let files = status
        .as_ref()
        .map(|status| {
            status
                .files
                .iter()
                .filter_map(|file| scope.project(file))
                .collect::<Vec<_>>()
        })
        .map_err(Clone::clone);
    match status {
        Ok(st) if base.is_some() => {
            match sg_git::branch_changes(path, st, base.unwrap_or_default())
                .map_err(git_error_message)
            {
                Ok(changes) => Changes {
                    files: Ok(changes.files),
                    merge_base: Some(changes.merge_base),
                    submodules: changes.submodules,
                    patch: Ok(changes.patch),
                },
                Err(error) => Changes {
                    files: Err(error.clone()),
                    merge_base: None,
                    submodules: Vec::new(),
                    patch: Err(error),
                },
            }
        }
        Ok(st) if !st.is_clean() => match sg_git::scoped_worktree_patch(path, st, scope) {
            Ok(patch) => Changes {
                // Each changed submodule's files follow its entry.
                files: Ok(sg_git::with_submodule_files(
                    files.unwrap_or_default(),
                    &patch.submodules,
                )),
                merge_base: None,
                submodules: patch.submodules,
                patch: Ok(patch.text),
            },
            Err(error) => Changes {
                files,
                merge_base: None,
                submodules: Vec::new(),
                patch: Err(git_error_message(error)),
            },
        },
        Ok(_) => Changes {
            files,
            merge_base: None,
            submodules: Vec::new(),
            patch: Ok(String::new()),
        },
        Err(error) => Changes {
            files,
            merge_base: None,
            submodules: Vec::new(),
            patch: Err(error.clone()),
        },
    }
}

/// Where a new worktree for `branch` goes: a sibling of the main worktree
/// named `<repo>-<branch>` (`myrepo` + `feature/x` → `../myrepo-feature-x`),
/// the common convention for hand-made worktrees.
pub fn default_worktree_path(repo: &Repo, branch: &str) -> PathBuf {
    let parent = repo.root.parent().unwrap_or(&repo.root);
    let slug: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    parent.join(format!("{}-{}", repo.name(), slug.trim_matches('-')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "supergit_view_state_{}_{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn git(&self, args: &[&str]) {
            let output = Command::new("git")
                .args([
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .current_dir(&self.0)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_folder_without_git_opens_as_a_folder_project() {
        let dir = TempDir::new();
        let folder = dir.0.join("workspace");
        std::fs::create_dir_all(folder.join("notes")).unwrap();
        let project = Project::open(&folder).unwrap();
        assert!(matches!(&project, Project::Folder(root) if *root == folder));
        assert_eq!(project.root(), folder);
        assert!(Project::open(&folder.join("missing")).is_err());

        // A repository inside it is a project of its own.
        let repo = folder.join("app");
        std::fs::create_dir_all(&repo).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        let project = Project::open(&repo.join(".")).unwrap();
        assert!(matches!(project, Project::Repo(_)));

        // Its one place is named after the folder, not "(unknown)".
        let place = WorktreeInfo {
            wt: Worktree {
                path: folder.clone(),
                head: None,
                branch: None,
                is_main: true,
                is_bare: false,
                is_detached: false,
                locked: None,
                prunable: None,
            },
            status: Ok(WorktreeStatus::default()),
            sessions: Vec::new(),
            cleanup_identity: None,
            folder: true,
        };
        assert_eq!(place.name(), "workspace");
    }

    #[test]
    fn a_folder_keeps_the_sessions_that_started_in_it() {
        let session = |start: &str, now: &str| Session {
            provider: sg_agents::Provider::Claude,
            id: "s".into(),
            transcript: PathBuf::new(),
            title: String::new(),
            last_prompt: None,
            git_branch: None,
            cwd: Some(now.into()),
            start_cwd: Some(start.into()),
            modified: std::time::SystemTime::UNIX_EPOCH,
            state: AgentState::Idle,
            busy: false,
        };
        let root = Path::new("/work");
        assert!(started_in(&session("/work", "/work"), root));
        // Moved into a repository of the workspace: still the folder's.
        assert!(started_in(&session("/work", "/work/app"), root));
        // Started in that repository: its project's, not the folder's.
        assert!(!started_in(&session("/work/app", "/work/app"), root));
    }

    #[test]
    fn loading_and_successful_empty_data_are_distinct() {
        let state = LoadState::<String>::default();
        assert!(state.is_loading());
        assert_eq!(state.value(), None);
        let state = LoadState::from_result(Ok(String::new()));
        assert!(!state.is_loading());
        assert_eq!(state.value().map(String::as_str), Some(""));
        assert_eq!(state.error(), None);
    }

    #[test]
    fn failed_refresh_preserves_last_success_and_recovers() {
        let mut state = LoadState::Ready("previous diff".to_string());
        for error in ["Git failed", "Git is still unavailable"] {
            state.update(LoadState::from_result(Err(error.into())));
            assert_eq!(state.value().map(String::as_str), Some("previous diff"));
            assert_eq!(state.error(), Some(error));
        }
        state.update(LoadState::Ready(String::new()));
        assert_eq!(state.error(), None);
        assert_eq!(state.value().map(String::as_str), Some(""));
    }

    #[test]
    fn another_worktree_cannot_inherit_stale_data() {
        let mut details = Details {
            path: PathBuf::from("/repo/first"),
            patch: LoadState::Ready("first worktree diff".into()),
            ..Default::default()
        };
        details.update(Details {
            path: PathBuf::from("/repo/second"),
            patch: LoadState::from_result(Err("unavailable".into())),
            ..Default::default()
        });
        assert_eq!(details.patch.value(), None);
        assert_eq!(details.patch.error(), Some("unavailable"));
    }

    #[test]
    fn repository_errors_do_not_become_clean_status_or_empty_history() {
        let temp = TempDir::new();
        let status = sg_git::status(&temp.0).map_err(|e| e.to_string());
        assert!(status.is_err());
        let details = load_details(&temp.0, status, None, sg_git::ChangeScope::All);
        assert!(details.status.error().is_some());
        assert!(details.patch.error().is_some());
        assert!(
            History::default()
                .load(&temp.0, None, history::HISTORY_PAGE)
                .commits
                .error()
                .is_some()
        );
        assert!(details.status.value().is_none());
        assert!(details.patch.value().is_none());
        assert!(
            History::default()
                .load(&temp.0, None, history::HISTORY_PAGE)
                .commits
                .value()
                .is_none()
        );
    }

    #[test]
    fn unborn_repository_has_successful_empty_diff_and_history() {
        let temp = TempDir::new();
        temp.git(&["init", "-q", "-b", "main"]);
        assert!(sg_git::log(&temp.0, &["missing-branch"], 10).is_err());
        let details = load_details(
            &temp.0,
            sg_git::status(&temp.0).map_err(|e| e.to_string()),
            None,
            sg_git::ChangeScope::All,
        );
        assert!(details.status.value().unwrap().is_clean());
        assert_eq!(details.patch.value().map(String::as_str), Some(""));
        assert!(
            matches!(History::default().load(&temp.0, None, history::HISTORY_PAGE).commits, LoadState::Ready(commits) if commits.is_empty())
        );
    }

    #[test]
    fn failed_history_refresh_keeps_its_previous_commits() {
        let temp = TempDir::new();
        temp.git(&["init", "-q", "-b", "main"]);
        temp.git(&["config", "user.email", "test@example.com"]);
        temp.git(&["config", "user.name", "Tester"]);
        temp.git(&["commit", "--allow-empty", "-qm", "initial"]);
        let mut history = History::default().load(&temp.0, None, history::HISTORY_PAGE);
        let original = history.commits.value().unwrap().clone();
        history.update(History::default().load(
            &temp.0.join("missing"),
            None,
            history::HISTORY_PAGE,
        ));
        assert!(history.commits.error().is_some());
        assert!(Arc::ptr_eq(history.commits.value().unwrap(), &original));
    }

    #[test]
    fn history_reuses_unchanged_commits_and_invalidates_ref_changes() {
        let temp = TempDir::new();
        temp.git(&["init", "-q", "-b", "main"]);
        temp.git(&["config", "user.email", "test@example.com"]);
        temp.git(&["config", "user.name", "Tester"]);
        temp.git(&["commit", "--allow-empty", "-qm", "initial"]);
        temp.git(&["branch", "feature"]);
        let source = || HistorySource::new(&temp.0.join(".git"), &sg_git::status(&temp.0).unwrap());
        let load = |previous: &History| previous.load(&temp.0, Some(&source()), previous.limit());
        let initial = load(&History::default());
        std::fs::write(temp.0.join("work.txt"), "uncommitted edit").unwrap();
        let cached = load(&initial);
        assert!(Arc::ptr_eq(
            cached.commits.value().unwrap(),
            initial.commits.value().unwrap()
        ));

        temp.git(&["tag", "v1"]);
        let tagged = load(&cached);
        let commit = &tagged.commits.value().unwrap()[0];
        assert!(commit.refs.iter().any(|label| label.name == "v1"));

        temp.git(&["switch", "-q", "feature"]);
        let switched = load(&tagged);
        let commit = &switched.commits.value().unwrap()[0];
        assert!(
            commit
                .refs
                .iter()
                .any(|label| label.name == "feature" && label.current)
        );
        assert!(
            !commit
                .refs
                .iter()
                .any(|label| label.name == "main" && label.current)
        );

        temp.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        let fetched = load(&switched);
        assert!(
            fetched.commits.value().unwrap()[0]
                .refs
                .iter()
                .any(|label| label.name == "origin/main")
        );

        let mut before_manual_refresh = fetched.clone();
        before_manual_refresh.invalidate();
        let refreshed = load(&before_manual_refresh);
        assert!(!Arc::ptr_eq(
            refreshed.commits.value().unwrap(),
            fetched.commits.value().unwrap()
        ));
    }

    #[test]
    fn history_refreshes_after_deepening_a_shallow_clone_with_the_same_refs() {
        let remote = TempDir::new();
        remote.git(&["init", "-q", "-b", "main"]);
        remote.git(&["config", "user.email", "test@example.com"]);
        remote.git(&["config", "user.name", "Tester"]);
        remote.git(&["commit", "--allow-empty", "-qm", "first"]);
        remote.git(&["commit", "--allow-empty", "-qm", "second"]);
        let clone = TempDir::new();
        clone.git(&[
            "clone",
            "-q",
            "--no-local",
            "--depth=1",
            remote.0.to_str().unwrap(),
            ".",
        ]);
        let source = HistorySource::new(&clone.0.join(".git"), &sg_git::status(&clone.0).unwrap());
        let initial = History::default().load(&clone.0, Some(&source), history::HISTORY_PAGE);
        assert_eq!(initial.commits.value().unwrap().len(), 1);
        let before = sg_git::history_refs(&clone.0).unwrap();
        clone.git(&["fetch", "-q", "--deepen=1"]);
        assert_eq!(sg_git::history_refs(&clone.0).unwrap(), before);
        let deepened = initial.load(&clone.0, Some(&source), initial.limit());
        assert_eq!(deepened.commits.value().unwrap().len(), 2);
    }

    #[test]
    fn expanding_history_reads_more_even_with_the_same_refs() {
        let repo = TempDir::new();
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.email", "test@example.com"]);
        repo.git(&["config", "user.name", "Tester"]);
        repo.git(&["commit", "--allow-empty", "-qm", "older"]);
        repo.git(&["commit", "--allow-empty", "-qm", "newer"]);
        let source = HistorySource::new(&repo.0.join(".git"), &sg_git::status(&repo.0).unwrap());
        let mut history = History::default().load(&repo.0, Some(&source), 1);
        assert_eq!(history.commits.value().unwrap().len(), 1);
        assert!(history.has_more);

        let limit = history.next_limit();
        history.update(history.load(&repo.0.join("missing"), None, limit));
        assert_eq!(history.limit(), 1);
        assert!(history.has_more);
        assert_eq!(history.commits.value().unwrap().len(), 1);

        history.update(history.load(&repo.0, Some(&source), 1));
        history.update(history.load(&repo.0, Some(&source), limit));
        let commits = history.commits.value().unwrap();
        assert_eq!(
            commits
                .iter()
                .map(|commit| commit.subject.as_str())
                .collect::<Vec<_>>(),
            vec!["newer", "older"]
        );
        assert_eq!(history.limit(), limit);
        assert!(!history.has_more);
    }

    #[test]
    fn diff_failure_is_independent_from_status_and_history() {
        let temp = TempDir::new();
        temp.git(&["init", "-q", "-b", "main"]);
        temp.git(&["config", "user.email", "test@example.com"]);
        temp.git(&["config", "user.name", "Tester"]);
        std::fs::write(temp.0.join("tracked.txt"), "before\n").unwrap();
        std::fs::write(temp.0.join(".gitattributes"), "tracked.txt diff=broken\n").unwrap();
        temp.git(&["add", "."]);
        temp.git(&["commit", "-qm", "initial"]);
        temp.git(&[
            "config",
            "diff.broken.textconv",
            "supergit-nonexistent-test-textconv",
        ]);
        std::fs::write(temp.0.join("tracked.txt"), "after\n").unwrap();
        let mut details = load_details(
            &temp.0,
            sg_git::status(&temp.0).map_err(|e| e.to_string()),
            None,
            sg_git::ChangeScope::All,
        );
        assert!(details.status.error().is_none());
        assert!(details.patch.error().is_some());
        assert_eq!(
            History::default()
                .load(&temp.0, None, history::HISTORY_PAGE)
                .commits
                .value()
                .unwrap()
                .len(),
            1
        );

        temp.git(&["config", "--unset", "diff.broken.textconv"]);
        details.update(load_details(
            &temp.0,
            sg_git::status(&temp.0).map_err(|e| e.to_string()),
            None,
            sg_git::ChangeScope::All,
        ));
        assert!(details.patch.error().is_none());
        assert!(details.patch.value().unwrap().contains("+after"));
    }
}
