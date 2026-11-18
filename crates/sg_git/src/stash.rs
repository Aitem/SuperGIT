use std::path::Path;

use anyhow::{Result, ensure};

use crate::{FileState, WorktreeStatus, git, rebase::unfinished_operation, status};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stash {
    pub oid: String,
    pub reference: String,
    pub subject: String,
    pub time: i64,
}

/// The stash reflog is shared by every worktree of this repository.
pub fn list_stashes(path: &Path) -> Result<Vec<Stash>> {
    let raw = git(path, ["stash", "list", "--format=%H%x00%gd%x00%gs%x00%ct"])?;
    raw.lines()
        .map(|line| {
            let fields: Vec<_> = line.splitn(4, '\0').collect();
            let [oid, reference, subject, time] = fields.as_slice() else {
                anyhow::bail!("Git returned an invalid stash entry");
            };
            Ok(Stash {
                oid: (*oid).into(),
                reference: (*reference).into(),
                subject: (*subject).into(),
                time: time.parse()?,
            })
        })
        .collect()
}

pub fn save_stash(
    path: &Path,
    expected: &WorktreeStatus,
    include_untracked: bool,
    message: &str,
) -> Result<()> {
    let current = check_worktree(path, expected)?;
    ensure!(
        current.conflicted_count() == 0,
        "Resolve the worktree conflicts before saving a stash"
    );
    ensure!(
        can_save_stash(&current, include_untracked),
        "There are no changes to stash"
    );
    let mut args = vec!["stash", "push"];
    if include_untracked {
        args.push("--include-untracked");
    }
    if !message.trim().is_empty() {
        args.extend(["--message", message.trim()]);
    }
    git(path, args).map(drop)
}

pub fn can_save_stash(status: &WorktreeStatus, include_untracked: bool) -> bool {
    !status.is_unborn()
        && status.conflicted_count() == 0
        && status.files.iter().any(|file| {
            file.staged.is_some()
                || file.unstaged == Some(FileState::Untracked) && include_untracked
                || file
                    .unstaged
                    .is_some_and(|state| state != FileState::Untracked)
                    && file
                        .submodule
                        .is_none_or(|submodule| submodule.commit_changed)
        })
}

/// Reapply exactly the selected stash, with its staged state, and keep its entry.
pub fn apply_stash(path: &Path, expected: &WorktreeStatus, oid: &str) -> Result<()> {
    let current = check_worktree(path, expected)?;
    ensure!(
        current.is_clean(),
        "Save or commit this worktree’s changes before applying a stash"
    );
    ensure!(
        list_stashes(path)?.iter().any(|stash| stash.oid == oid),
        "The selected stash is no longer available"
    );
    git(path, ["stash", "apply", "--index", "--", oid]).map(drop)
}

fn check_worktree(path: &Path, expected: &WorktreeStatus) -> Result<WorktreeStatus> {
    ensure!(
        unfinished_operation(path)?.is_none(),
        "Finish the current Git operation before working with stashes"
    );
    let current = status(path)?;
    ensure!(
        current.head_oid == expected.head_oid
            && current.branch == expected.branch
            && current.files == expected.files,
        "The worktree changed. Refresh the stash dialog and review it again."
    );
    ensure!(
        !current.is_unborn(),
        "Create the first commit before working with stashes"
    );
    Ok(current)
}
