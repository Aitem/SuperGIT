//! Index-only changes to the explicitly selected file in one worktree.

use std::path::Path;

use anyhow::{Context as _, Result, ensure};

use crate::runner::git_input;
use crate::status::PATCH_OPTIONS;
use crate::{FileChange, FileState, WorktreeStatus, git, hunk_patch, status};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Staging {
    Stage,
    Unstage,
}

impl Staging {
    pub fn hunk_available(self, file: &FileChange) -> bool {
        self.available(file)
            && file.orig_path.is_none()
            && file.submodule.is_none()
            && match self {
                Self::Stage => file.unstaged == Some(FileState::Modified),
                Self::Unstage => file.staged == Some(FileState::Modified),
            }
    }

    pub fn available(self, file: &FileChange) -> bool {
        if file.path.contains('\u{fffd}') {
            return false;
        }
        match self {
            Self::Stage => file.unstaged.is_some_and(|state| {
                state == FileState::Deleted || file.submodule.is_none_or(|sub| sub.commit_changed)
            }),
            Self::Unstage => file.unstaged != Some(FileState::Conflicted) && file.staged.is_some(),
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
    let current = validate_selection(worktree, snapshot, file, action)?;
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

fn validate_selection(
    worktree: &Path,
    snapshot: &WorktreeStatus,
    file: &FileChange,
    action: Staging,
) -> Result<WorktreeStatus> {
    let current = status(worktree)?;
    ensure!(
        current.branch == snapshot.branch && current.head_oid == snapshot.head_oid,
        "The worktree's branch or HEAD changed. Review the changes again."
    );
    ensure!(
        current.files.contains(file) && action.available(file),
        "The file's status changed. Review the changes again."
    );
    Ok(current)
}

/// Apply one reviewed hunk to the index. The text displayed by the UI must
/// exactly match a fresh Git patch before we select and apply its hunk.
pub fn change_staging_hunk(
    worktree: &Path,
    snapshot: &WorktreeStatus,
    file: &FileChange,
    action: Staging,
    shown_file: &str,
    (original, line): (bool, usize),
) -> Result<()> {
    validate_selection(worktree, snapshot, file, action)?;
    ensure!(
        action.hunk_available(file),
        "Individual hunks require a tracked text modification"
    );
    let mut args = vec!["--literal-pathspecs", "diff", "--no-textconv"];
    args.extend(PATCH_OPTIONS);
    if action == Staging::Unstage {
        args.push("--cached");
    }
    args.extend(["--", &file.path]);
    let fresh = git(worktree, args)?;
    ensure!(
        fresh.trim_end_matches('\n') == shown_file.trim_end_matches('\n'),
        "The file's diff changed. Review the hunk again."
    );
    let patch =
        text_hunk(&fresh, original, line).context("Select a hunk in a regular text file")?;
    let mut apply = vec!["apply", "--cached", "--whitespace=nowarn"];
    if action == Staging::Unstage {
        apply.push("--reverse");
    }
    apply.push("-");
    git_input(worktree, &apply, &patch)
}

pub fn text_hunk(file_patch: &str, original: bool, line: usize) -> Option<String> {
    let headers = file_patch.split_once("\n@@ ")?.0;
    let regular = headers.lines().any(|line| {
        line.starts_with("index ") && (line.ends_with(" 100644") || line.ends_with(" 100755"))
    });
    let unsupported = headers.lines().any(|line| {
        line.starts_with("old mode ")
            || line.starts_with("new mode ")
            || line.starts_with("rename from ")
            || line.starts_with("copy from ")
    });
    (regular && !unsupported)
        .then(|| hunk_patch(file_patch, original, line))
        .flatten()
}
