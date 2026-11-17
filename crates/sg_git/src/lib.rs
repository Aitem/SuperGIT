//! Git core for SuperGIT.
//!
//! Reads go through the `git` CLI with porcelain formats: it honours the
//! user's config and hooks, understands every repository layout git does, and
//! `GIT_OPTIONAL_LOCKS=0` keeps our frequent status polling from fighting the
//! agents that run git in the same worktrees. Hot paths can move to `gix`
//! later without changing this API.

mod base;
mod cleanup;
mod comparison;
pub mod difftastic;
mod log;
#[cfg(feature = "git2-probe")]
mod native_refs;
#[cfg(feature = "git2-probe")]
mod native_status;
#[cfg(feature = "git2-probe")]
pub use native_status::read_fixture as probe_native_status;
mod rebase;
mod runner;
mod status;
mod submodule;
mod worktree;

pub use base::{BaseInfo, Upstream, load_base, update_base};
pub use cleanup::{
    BranchDeletion, CleanupOutcome, CleanupPlan, CleanupPolicy, WorktreeIdentity,
    cleanup_base_branch, cleanup_worktree, delete_cleanup_branch, fetch_cleanup_base,
    worktree_identity,
};
pub use comparison::{
    BranchChanges, branch_changes, pr_merge_base, range_patch, resolve_branch, suggested_base,
};
pub use log::{
    Commit, CommitDetails, Fork, ForkParent, RefKind, RefLabel, base_branch, commit_details,
    commit_patch, fork, history, history_refs, log,
};
pub use rebase::{RebaseOutcome, RebasePlan, plan_rebase, rebase};
pub use runner::{GitError, git, git_bytes};
pub use status::{FileChange, FileState, WorktreeStatus, status, worktree_patch};
pub use submodule::{
    Inline, MAX_COMMITS, Patch, SubmoduleChange, SubmoduleCommit, SubmoduleFlags,
    inline_submodules, submodule_of, with_submodule_files,
};
pub use worktree::{Worktree, add_worktree, list_worktrees, remove_worktree};

#[cfg(feature = "git2-probe")]
fn use_git2_probe() -> bool {
    std::env::var_os("SUPERGIT_GIT_BACKEND").as_deref() == Some(std::ffi::OsStr::new("git2"))
        && [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_NAMESPACE",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_REPLACE_REF_BASE",
            "GIT_NO_REPLACE_OBJECTS",
        ]
        .iter()
        .all(|key| std::env::var_os(key).is_none())
}

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

pub use status::polled_status;

/// A repository as SuperGIT sees it: the main worktree plus the shared git dir.
#[derive(Clone, Debug)]
pub struct Repo {
    /// Top level of the main worktree (where `.git` lives).
    pub root: PathBuf,
    /// The shared `.git` directory common to all worktrees.
    pub common_dir: PathBuf,
}

impl Repo {
    /// Find the repository containing `path`. Works from any linked worktree:
    /// the result always points at the main worktree.
    pub fn discover(path: &Path) -> Result<Self> {
        let out = git(
            path,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .with_context(|| format!("{} is not inside a git repository", path.display()))?;
        let common_dir = PathBuf::from(out.trim());
        let root = if common_dir.file_name().is_some_and(|n| n == ".git") {
            common_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| common_dir.clone())
        } else {
            // Bare repository: no main worktree, use the git dir itself.
            common_dir.clone()
        };
        Ok(Self { root, common_dir })
    }

    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string())
    }
}

/// Human-friendly relative time ("5m", "3h", "2d") for a unix timestamp.
pub fn relative_time(unix_secs: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(unix_secs);
    let delta = (now - unix_secs).max(0);
    match delta {
        0..60 => "now".into(),
        60..3_600 => format!("{}m", delta / 60),
        3_600..86_400 => format!("{}h", delta / 3_600),
        86_400..2_592_000 => format!("{}d", delta / 86_400),
        2_592_000..31_536_000 => format!("{}mo", delta / 2_592_000),
        _ => format!("{}y", delta / 31_536_000),
    }
}
