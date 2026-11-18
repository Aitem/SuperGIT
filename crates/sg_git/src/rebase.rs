//! Rebasing a worktree's branch onto a chosen local branch, in two steps:
//! [`plan_rebase`] describes what would happen so the user can confirm it,
//! [`rebase`] does exactly that or refuses. A rebase that stops (conflicts,
//! hooks, untracked files in the way) is aborted, so the branch is either
//! fully rebased or left untouched.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail, ensure};

use crate::{FileState, GitError, git, git_bytes, status};

/// What rebasing `branch` onto `onto` would do, as observed at planning time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebasePlan {
    pub worktree: PathBuf,
    pub branch: String,
    /// The branch tip the plan was made for.
    pub head: String,
    pub onto: String,
    pub onto_tip: String,
    /// Commits on `onto` that the branch lacks.
    pub behind: u32,
    /// The branch's own commits that would be replayed (merges and patches
    /// already on `onto` are skipped, as `git rebase` does).
    pub replayed: u32,
    /// The branch's upstream (`origin/feature`), if configured.
    pub upstream: Option<String>,
    /// The upstream is configured but no longer exists (deleted on the remote).
    pub upstream_gone: bool,
}

impl RebasePlan {
    pub fn is_up_to_date(&self) -> bool {
        self.behind == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// `onto` was already part of the branch: nothing changed.
    UpToDate,
    /// The branch's own `commits` now sit on top of `onto` (0: it just moved there).
    Rebased { commits: u32 },
}

/// Describe rebasing the branch checked out in `worktree` onto the local
/// branch `onto`. Reads only.
pub fn plan_rebase(worktree: &Path, onto: &str) -> Result<RebasePlan> {
    git(worktree, ["check-ref-format", "--branch", onto])
        .with_context(|| format!("'{onto}' is not a valid branch name"))?;
    let onto_tip = branch_tip(worktree, onto)?;
    let branch = git(worktree, ["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|name| name.trim().to_string())
        .map_err(|_| anyhow!("HEAD is detached; check out a branch before rebasing"))?;
    ensure!(
        branch != onto,
        "Choose a branch other than '{branch}' itself"
    );
    let head = git(worktree, ["rev-parse", "--verify", "HEAD^{commit}"])
        .map(|oid| oid.trim().to_string())
        .map_err(|_| anyhow!("'{branch}' has no commits yet"))?;
    let count = |args: &[&str]| -> Result<u32> { Ok(git(worktree, args)?.trim().parse()?) };
    let behind = count(&["rev-list", "--count", &format!("{head}..{onto_tip}")])?;
    let replayed = count(&[
        "rev-list",
        "--count",
        "--right-only",
        "--cherry-pick",
        "--no-merges",
        &format!("{onto_tip}...{head}"),
    ])?;
    let fields = git(
        worktree,
        [
            "for-each-ref",
            "--format=%(upstream:short)%00%(upstream:track)",
            &format!("refs/heads/{branch}"),
        ],
    )?;
    let (upstream, track) = fields
        .trim_end_matches('\n')
        .split_once('\0')
        .unwrap_or_default();
    Ok(RebasePlan {
        worktree: worktree.to_path_buf(),
        branch,
        head,
        onto: onto.to_string(),
        onto_tip,
        behind,
        replayed,
        upstream: (!upstream.is_empty()).then(|| upstream.to_string()),
        upstream_gone: track == "[gone]",
    })
}

/// Carry out a confirmed [`RebasePlan`]. Refuses when the branch or `onto`
/// moved since planning, the worktree has tracked changes (untracked files
/// are fine unless the rebase would overwrite them) or an unfinished Git
/// operation.
pub fn rebase(plan: &RebasePlan) -> Result<RebaseOutcome> {
    let worktree = plan.worktree.as_path();
    let onto = plan.onto.as_str();
    ensure!(
        branch_tip(worktree, onto)? == plan.onto_tip,
        "'{onto}' moved since the preview; open Rebase again"
    );
    let branch = git(worktree, ["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|name| name.trim().to_string())
        .unwrap_or_default();
    ensure!(
        branch == plan.branch,
        "The worktree switched branches since the preview; open Rebase again"
    );
    let state = status(worktree)?;
    ensure!(
        state.head_oid.as_deref() == Some(plan.head.as_str()),
        "'{branch}' moved since the preview; open Rebase again"
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
    if is_ancestor(worktree, &plan.onto_tip, "HEAD") {
        return Ok(RebaseOutcome::UpToDate);
    }
    // The planned oid, not the name; no autostash (changes were refused
    // above), and no moving of other branches.
    let result = git(
        worktree,
        [
            "rebase",
            "--no-autostash",
            "--no-autosquash",
            "--no-update-refs",
            &plan.onto_tip,
        ],
    );
    if let Err(error) = result {
        return Err(abort(worktree, onto, error));
    }
    ensure!(
        is_ancestor(worktree, &plan.onto_tip, "HEAD"),
        "The rebase finished but '{branch}' does not contain {onto}; inspect it manually"
    );
    Ok(RebaseOutcome::Rebased {
        commits: plan.replayed,
    })
}

fn branch_tip(worktree: &Path, branch: &str) -> Result<String> {
    git(
        worktree,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}^{{commit}}"),
        ],
    )
    .map(|oid| oid.trim().to_string())
    .map_err(|_| anyhow!("Branch '{branch}' has no local commit"))
}

fn is_ancestor(worktree: &Path, ancestor: &str, descendant: &str) -> bool {
    git(
        worktree,
        ["merge-base", "--is-ancestor", ancestor, descendant],
    )
    .is_ok()
}

pub(crate) fn git_dir(worktree: &Path) -> Result<PathBuf> {
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
fn abort(worktree: &Path, onto: &str, error: anyhow::Error) -> anyhow::Error {
    let detail = match error.downcast_ref::<GitError>() {
        Some(git) if !git.stderr.trim().is_empty() => git.stderr.trim().to_string(),
        _ => format!("{error:#}"),
    };
    let stopped = git_dir(worktree).is_ok_and(|directory| {
        directory.join("rebase-merge").exists() || directory.join("rebase-apply").exists()
    });
    if !stopped {
        // Refused before it started (e.g. a pre-rebase hook): nothing to undo.
        return anyhow!("Rebase onto {onto} did not start: {detail}");
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
            "Rebase onto {onto} stopped and could not be aborted ({abort_error:#}). \
             Finish it or run `git rebase --abort` in a terminal"
        );
    }
    if conflicts.is_empty() {
        return anyhow!(
            "Rebase onto {onto} stopped and was aborted; the branch is unchanged. {detail}"
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
    anyhow!("Conflicts with {onto} in {files}. The rebase was aborted; the branch is unchanged")
}
