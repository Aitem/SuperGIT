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
                        sg_git::status(&wt.path).map_err(|e| e.to_string())
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

/// What the main panel shows for the selected worktree.
#[derive(Clone, Debug, Default)]
pub struct Details {
    pub path: PathBuf,
    pub patch: String,
    pub commits: Vec<Commit>,
}

pub fn load_details(path: &Path, status: Option<&WorktreeStatus>) -> Details {
    let patch = match status {
        Some(st) if !st.is_clean() => sg_git::worktree_patch(path, st)
            .unwrap_or_else(|e| format!("# failed to compute diff: {e}\n")),
        _ => String::new(),
    };
    let commits = sg_git::log(path, "HEAD", 400).unwrap_or_default();
    Details {
        path: path.to_path_buf(),
        patch,
        commits,
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
