//! Data loaded in the background and handed to the UI as plain values.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use sg_agents::{AgentState, Session, SessionIndex};
use sg_git::{Commit, Repo, Worktree, WorktreeStatus};

#[derive(Clone, Debug)]
pub struct WorktreeInfo {
    pub wt: Worktree,
    pub status: Result<WorktreeStatus, String>,
    pub sessions: Vec<Session>,
}

impl WorktreeInfo {
    pub fn path(&self) -> &Path {
        &self.wt.path
    }

    /// The most attention-worthy state among this worktree's sessions.
    pub fn agent_state(&self) -> Option<AgentState> {
        self.sessions
            .iter()
            .map(|s| s.state)
            .min()
            .filter(|s| s.is_active())
    }

    pub fn changed_files(&self) -> usize {
        self.status.as_ref().map(|s| s.files.len()).unwrap_or(0)
    }
}

pub type SharedSessionIndex = Arc<Mutex<SessionIndex>>;

/// The view already names the operation; Git's stderr explains what failed.
pub fn git_error_message(error: anyhow::Error) -> String {
    if let Some(git) = error.downcast_ref::<sg_git::GitError>()
        && !git.stderr.trim().is_empty()
    {
        return git.stderr.trim().to_string();
    }
    error.to_string()
}

/// Every worktree with its status and Claude sessions. Status runs in
/// parallel: with dozens of worktrees this is the hot path of each refresh.
pub fn load_worktrees(repo: &Repo, index: &SharedSessionIndex) -> Result<Vec<WorktreeInfo>> {
    let worktrees = sg_git::list_worktrees(&repo.root)?;
    let statuses: Vec<Result<WorktreeStatus, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = worktrees
            .iter()
            .map(|wt| {
                scope.spawn(move || {
                    if wt.is_bare {
                        Err("bare".to_string())
                    } else if let Some(reason) = &wt.prunable {
                        Err(format!("prunable: {reason}"))
                    } else {
                        sg_git::status(&wt.path).map_err(git_error_message)
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err("status panicked".into())))
            .collect()
    });

    let paths: Vec<PathBuf> = worktrees.iter().map(|w| w.path.clone()).collect();
    let sessions = index
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .sessions_by_worktree(&paths);
    Ok(worktrees
        .into_iter()
        .zip(statuses)
        .zip(sessions)
        .map(|((wt, status), sessions)| WorktreeInfo {
            wt,
            status,
            sessions,
        })
        .collect())
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
    pub base: Option<String>,
    pub suggested_base: Option<String>,
    pub merge_base: Option<String>,
    pub files: LoadState<Vec<sg_git::FileChange>>,
    pub status: LoadState<WorktreeStatus>,
    pub patch: LoadState<String>,
    pub commits: LoadState<Vec<Commit>>,
}

impl Details {
    pub fn update(&mut self, next: Self) {
        if self.path != next.path || self.base != next.base {
            *self = next;
            return;
        }
        self.status.update(next.status);
        self.files.update(next.files);
        self.merge_base = next.merge_base;
        self.suggested_base = next.suggested_base;
        self.patch.update(next.patch);
        self.commits.update(next.commits);
    }
}

pub fn load_details(
    path: &Path,
    status: Result<WorktreeStatus, String>,
    base: Option<String>,
) -> Details {
    let suggested_base = sg_git::suggested_base(path);
    let mut files = status
        .as_ref()
        .map(|status| status.files.clone())
        .map_err(Clone::clone);
    let mut merge_base = None;
    let patch = match &status {
        Ok(st) if base.is_some() => {
            match sg_git::branch_changes(path, st, base.as_deref().unwrap_or_default())
                .map_err(git_error_message)
            {
                Ok(changes) => {
                    files = Ok(changes.files);
                    merge_base = Some(changes.merge_base);
                    Ok(changes.patch)
                }
                Err(error) => {
                    files = Err(error.clone());
                    Err(error)
                }
            }
        }
        Ok(st) if !st.is_clean() => sg_git::worktree_patch(path, st).map_err(git_error_message),
        Ok(_) => Ok(String::new()),
        Err(error) => Err(error.clone()),
    };
    let commits = sg_git::log(path, "HEAD", 400).map_err(git_error_message);
    Details {
        path: path.to_path_buf(),
        base,
        suggested_base,
        merge_base,
        files: LoadState::from_result(files),
        status: LoadState::from_result(status),
        patch: LoadState::from_result(patch),
        commits: LoadState::from_result(commits),
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
        let details = load_details(&temp.0, status, None);
        assert!(details.status.error().is_some());
        assert!(details.patch.error().is_some());
        assert!(details.commits.error().is_some());
        assert!(details.status.value().is_none());
        assert!(details.patch.value().is_none());
        assert!(details.commits.value().is_none());
    }

    #[test]
    fn unborn_repository_has_successful_empty_diff_and_history() {
        let temp = TempDir::new();
        temp.git(&["init", "-q", "-b", "main"]);
        assert!(sg_git::log(&temp.0, "missing-branch", 10).is_err());
        let details = load_details(
            &temp.0,
            sg_git::status(&temp.0).map_err(|e| e.to_string()),
            None,
        );
        assert!(details.status.value().unwrap().is_clean());
        assert_eq!(details.patch.value().map(String::as_str), Some(""));
        assert!(matches!(details.commits, LoadState::Ready(commits) if commits.is_empty()));
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
        );
        assert!(details.status.error().is_none());
        assert!(details.patch.error().is_some());
        assert_eq!(details.commits.value().unwrap().len(), 1);

        temp.git(&["config", "--unset", "diff.broken.textconv"]);
        details.update(load_details(
            &temp.0,
            sg_git::status(&temp.0).map_err(|e| e.to_string()),
            None,
        ));
        assert!(details.patch.error().is_none());
        assert!(details.patch.value().unwrap().contains("+after"));
    }
}
