//! Rebasing a worktree's branch onto a local base branch, all or nothing:
//! a rebase that stops (conflicts, hooks, untracked files in the way) is
//! aborted, so the branch is either fully rebased or left untouched.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail, ensure};

use crate::{FileState, GitError, git, git_bytes, status};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// The base was already part of the branch: nothing changed.
    UpToDate,
    /// The branch's own `commits` now sit on top of the base (0: fast-forward).
    Rebased { commits: u32 },
}

/// Commits on the local branch `base` that the worktree's HEAD lacks.
pub fn behind_base(worktree: &Path, base: &str) -> Result<u32> {
    let count = git(
        worktree,
        ["rev-list", "--count", &format!("HEAD..refs/heads/{base}")],
    )?;
    Ok(count.trim().parse()?)
}

/// Rebase the branch checked out in `worktree` onto `refs/heads/<base>`.
///
/// Refuses a detached HEAD, the base itself, tracked changes (untracked files
/// are fine unless the rebase would overwrite them), an unfinished Git
/// operation, and a HEAD that moved since the caller saw `expected_head`.
pub fn rebase_onto(worktree: &Path, base: &str, expected_head: &str) -> Result<RebaseOutcome> {
    git(worktree, ["check-ref-format", "--branch", base])
        .with_context(|| format!("'{base}' is not a valid base branch"))?;
    let target = git(
        worktree,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{base}^{{commit}}"),
        ],
    )
    .map(|oid| oid.trim().to_string())
    .map_err(|_| anyhow!("Base '{base}' has no local commit; choose an existing branch"))?;
    let branch = git(worktree, ["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|name| name.trim().to_string())
        .map_err(|_| anyhow!("HEAD is detached; check out a branch before rebasing"))?;
    ensure!(branch != base, "'{branch}' is the base branch itself");
    let state = status(worktree)?;
    ensure!(
        state.head_oid.as_deref() == Some(expected_head),
        "The branch moved since the last refresh; try again"
    );
    ensure!(
        state
            .files
            .iter()
            .all(|file| file.unstaged == Some(FileState::Untracked)),
        "The worktree has uncommitted changes; commit or discard them before rebasing"
    );
    if let Some(marker) = unfinished_operation(worktree)? {
        bail!("The worktree has an unfinished Git operation ({marker})");
    }
    if is_ancestor(worktree, &target, "HEAD") {
        return Ok(RebaseOutcome::UpToDate);
    }
    let commits = git(
        worktree,
        ["rev-list", "--count", &format!("{target}..HEAD")],
    )?
    .trim()
    .parse()
    .unwrap_or(0);
    // The oid, not the name: the base may move while the rebase runs. No
    // autostash (changes were refused above), no moving of other branches.
    let rebase = git(
        worktree,
        [
            "rebase",
            "--no-autostash",
            "--no-autosquash",
            "--no-update-refs",
            &target,
        ],
    );
    if let Err(error) = rebase {
        return Err(abort(worktree, base, error));
    }
    ensure!(
        is_ancestor(worktree, &target, "HEAD"),
        "The rebase finished but '{branch}' does not contain {base}; inspect it manually"
    );
    Ok(RebaseOutcome::Rebased { commits })
}

fn is_ancestor(worktree: &Path, ancestor: &str, descendant: &str) -> bool {
    git(
        worktree,
        ["merge-base", "--is-ancestor", ancestor, descendant],
    )
    .is_ok()
}

fn git_dir(worktree: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(
        git(worktree, ["rev-parse", "--absolute-git-dir"])?.trim(),
    ))
}

/// The marker of an operation Git has not finished in this worktree, if any.
pub(crate) fn unfinished_operation(worktree: &Path) -> Result<Option<&'static str>> {
    let directory = git_dir(worktree)?;
    Ok([
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "index.lock",
    ]
    .into_iter()
    .find(|marker| directory.join(marker).exists()))
}

/// Undo a rebase that stopped, and explain why it stopped.
fn abort(worktree: &Path, base: &str, error: anyhow::Error) -> anyhow::Error {
    let detail = match error.downcast_ref::<GitError>() {
        Some(git) if !git.stderr.trim().is_empty() => git.stderr.trim().to_string(),
        _ => format!("{error:#}"),
    };
    let stopped = git_dir(worktree).is_ok_and(|directory| {
        directory.join("rebase-merge").exists() || directory.join("rebase-apply").exists()
    });
    if !stopped {
        // Refused before it started (e.g. a pre-rebase hook): nothing to undo.
        return anyhow!("Rebase onto {base} did not start: {detail}");
    }
    let conflicts: Vec<String> =
        git_bytes(worktree, ["diff", "--name-only", "--diff-filter=U", "-z"])
            .map(|out| {
                String::from_utf8_lossy(&out)
                    .split('\0')
                    .filter(|path| !path.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
    if let Err(abort_error) = git(worktree, ["rebase", "--abort"]) {
        return anyhow!(
            "Rebase onto {base} stopped and could not be aborted ({abort_error:#}). \
             Finish it or run `git rebase --abort` in a terminal"
        );
    }
    if conflicts.is_empty() {
        return anyhow!(
            "Rebase onto {base} stopped and was aborted; the branch is unchanged. {detail}"
        );
    }
    let mut files = conflicts
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if conflicts.len() > 3 {
        files.push_str(&format!(" and {} more", conflicts.len() - 3));
    }
    anyhow!("Conflicts with {base} in {files}. The rebase was aborted; the branch is unchanged")
}
