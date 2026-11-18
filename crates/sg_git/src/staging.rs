//! Index-only changes to the explicitly selected file in one worktree.

use std::path::Path;

use anyhow::{Context as _, Result, ensure};

use crate::{FileChange, FileState, WorktreeStatus, git, status};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Staging {
    Stage,
    Unstage,
}

impl Staging {
    pub fn available(self, file: &FileChange) -> bool {
        if file.unstaged == Some(FileState::Conflicted) || file.path.contains('\u{fffd}') {
            return false;
        }
        match self {
            Self::Stage => file.unstaged.is_some_and(|state| {
                state == FileState::Deleted || file.submodule.is_none_or(|sub| sub.commit_changed)
            }),
            Self::Unstage => file.staged.is_some(),
        }
    }
}

/// Stage or unstage a file using the worktree and change the user reviewed.
/// Reset is used only in its path mode, which updates the index.
pub fn change_staging(
    worktree: &Path,
    snapshot: &WorktreeStatus,
    file: &FileChange,
    action: Staging,
) -> Result<()> {
    let current = status(worktree)?;
    ensure!(
        current.branch == snapshot.branch && current.head_oid == snapshot.head_oid,
        "The worktree's branch or HEAD changed. Review the changes again."
    );
    ensure!(
        current.files.contains(file) && action.available(file),
        "The file's status changed. Review the changes again."
    );
    let mut paths = vec![file.path.as_str()];
    let rename = match action {
        Staging::Stage => file.unstaged,
        Staging::Unstage => file.staged,
    } == Some(FileState::Renamed);
    if rename {
        let original = file.orig_path.as_deref().context("Missing rename source")?;
        ensure!(
            !original.contains('\u{fffd}'),
            "The rename source is not valid UTF-8"
        );
        paths.push(original);
    }
    for path in &paths {
        let prefix = format!("{path}/");
        ensure!(
            !current
                .files
                .iter()
                .any(|other| other.path.starts_with(&prefix)
                    || other
                        .orig_path
                        .as_ref()
                        .is_some_and(|original| original.starts_with(&prefix))),
            "'{path}' overlaps a changed directory. Use Git to update its index entries explicitly."
        );
        if action == Staging::Stage && file.submodule.is_none() {
            let directory = worktree
                .join(path)
                .symlink_metadata()
                .is_ok_and(|entry| entry.is_dir());
            ensure!(
                !directory,
                "'{path}' is now a directory. Review the changes again."
            );
        }
    }
    let mut args = match action {
        Staging::Stage => vec!["--literal-pathspecs", "add", "-A", "--"],
        Staging::Unstage => vec!["--literal-pathspecs", "reset", "--quiet", "--"],
    };
    args.extend(paths);
    git(worktree, args)?;
    Ok(())
}
