use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};

use crate::{Commit, FileState, GitError, git, log, rebase, status};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CherryPickState {
    Preview,
    Paused { conflicts: usize },
}

#[derive(Clone, Debug)]
pub struct CherryPickPlan {
    pub source: Commit,
    pub branch: Option<String>,
    pub state: CherryPickState,
    head: String,
    git_dir: PathBuf,
}

impl CherryPickPlan {
    pub fn destination_short_sha(&self) -> &str {
        &self.head[..self.head.len().min(8)]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CherryPickAction {
    Start,
    Continue,
    Abort,
}

/// Preview the requested commit, or inspect a paused single-commit operation.
pub fn inspect_cherry_pick(path: &Path, requested: Option<&str>) -> Result<Option<CherryPickPlan>> {
    let pending = match git(
        path,
        ["rev-parse", "--verify", "--quiet", "CHERRY_PICK_HEAD"],
    ) {
        Ok(head) => Some(head.trim().to_owned()),
        Err(error)
            if error
                .downcast_ref::<GitError>()
                .is_some_and(|error| error.code == Some(1)) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    let Some(source) = pending.as_deref().or(requested) else {
        return Ok(None);
    };
    ensure!(
        matches!(source.len(), 40 | 64) && source.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Choose an existing commit from History"
    );
    let git_dir = rebase::git_dir(path)?;
    ensure!(
        !git_dir.join("sequencer/todo").exists(),
        "Finish the multi-commit Git operation in the terminal"
    );
    let status = status(path)?;
    let head = status
        .head_oid
        .as_deref()
        .context("Create the first commit before cherry-picking")?
        .to_owned();
    let state = if pending.is_some() {
        CherryPickState::Paused {
            conflicts: status.conflicted_count(),
        }
    } else {
        ensure!(
            status.branch.is_some(),
            "Check out a destination branch before cherry-picking"
        );
        if let Some(marker) = rebase::unfinished_operation(path)? {
            anyhow::bail!("Finish the current Git operation ({marker}) before cherry-picking");
        }
        ensure!(
            status
                .files
                .iter()
                .all(|file| file.staged.is_none() && file.unstaged == Some(FileState::Untracked)),
            "Commit or stash tracked changes in the destination worktree first"
        );
        match git(path, ["merge-base", "--is-ancestor", source, &head]) {
            Ok(_) => anyhow::bail!("This commit is already in the destination branch"),
            Err(error)
                if error
                    .downcast_ref::<GitError>()
                    .is_some_and(|error| error.code == Some(1)) => {}
            Err(error) => return Err(error),
        }
        CherryPickState::Preview
    };
    let commit = log(path, &[source], 1)?
        .into_iter()
        .next()
        .context("The selected commit is missing")?;
    ensure!(
        !commit.is_merge() || pending.is_some(),
        "Choose a single-parent commit to cherry-pick"
    );
    Ok(Some(CherryPickPlan {
        source: commit,
        branch: status.branch,
        state,
        head,
        git_dir,
    }))
}

/// Revalidate the same branch, destination tip and paused commit before invoking Git.
pub fn run_cherry_pick(
    path: &Path,
    expected: &CherryPickPlan,
    action: CherryPickAction,
) -> Result<()> {
    let start = action == CherryPickAction::Start;
    let current = inspect_cherry_pick(path, start.then_some(expected.source.sha.as_str()))?
        .context("There is no cherry-pick in progress in this worktree")?;
    ensure!(
        current.git_dir == expected.git_dir
            && current.branch == expected.branch
            && current.head == expected.head
            && current.source.sha == expected.source.sha,
        "The worktree or selected commit changed. Reopen the cherry-pick preview."
    );
    match (action, current.state) {
        (CherryPickAction::Start, CherryPickState::Preview) => {
            git(path, ["cherry-pick", "--", &current.source.sha]).map(drop)
        }
        (CherryPickAction::Continue, CherryPickState::Paused { conflicts: 0 }) => {
            git(path, ["cherry-pick", "--continue"]).map(drop)
        }
        (CherryPickAction::Abort, CherryPickState::Paused { .. }) => {
            git(path, ["cherry-pick", "--abort"]).map(drop)
        }
        (CherryPickAction::Continue, CherryPickState::Paused { .. }) => {
            anyhow::bail!("Resolve and stage the conflicted files before continuing")
        }
        _ => anyhow::bail!("The cherry-pick state changed. Refresh the dialog."),
    }
}
