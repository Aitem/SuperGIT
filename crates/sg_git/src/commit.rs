//! Review and commit the selected worktree's index with the normal Git hooks.

use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};

use crate::rebase::unfinished_operation;
use crate::runner::git_input;
use crate::status::PATCH_OPTIONS;
use crate::{git, git_bytes, status};

const MAX_PREVIEW_BYTES: usize = 256 * 1024;

#[derive(Debug, PartialEq, Eq)]
struct CommitBasis {
    branch: Option<String>,
    head: Option<String>,
    staged: Vec<u8>,
}

pub struct CommitPreview {
    worktree: PathBuf,
    basis: CommitBasis,
    pub files: usize,
    pub stat: String,
    /// None when the staged patch exceeds the UI's preview limit.
    pub patch: Option<String>,
}

impl CommitPreview {
    pub fn branch(&self) -> Option<&str> {
        self.basis.branch.as_deref()
    }

    pub fn head(&self) -> Option<&str> {
        self.basis.head.as_deref()
    }
}

fn read_basis(worktree: &Path) -> Result<CommitBasis> {
    if let Some(operation) = unfinished_operation(worktree)? {
        anyhow::bail!(
            "A Git operation is in progress ({operation}). Finish it before committing here."
        );
    }
    let status = status(worktree)?;
    ensure!(
        status.conflicted_count() == 0,
        "Resolve the worktree's conflicts first"
    );
    let staged = git_bytes(
        worktree,
        [
            "diff",
            "--cached",
            "--raw",
            "--no-abbrev",
            "--no-renames",
            "--ignore-submodules=none",
            "-z",
            "--",
        ],
    )?;
    Ok(CommitBasis {
        branch: status.branch,
        head: status.head_oid,
        staged,
    })
}

pub fn preview_commit(worktree: &Path) -> Result<CommitPreview> {
    let basis = read_basis(worktree)?;
    ensure!(
        !basis.staged.is_empty(),
        "Stage at least one file before committing"
    );
    // In this raw format each path has one NUL after the header and one after the path.
    let files = basis.staged.iter().filter(|&&byte| byte == 0).count() / 2;
    let stat = git(
        worktree,
        [
            "diff",
            "--cached",
            "--no-color",
            "--ignore-submodules=none",
            "--stat",
            "--",
        ],
    )?;
    let mut args = vec![
        "diff",
        "--cached",
        "--no-textconv",
        "--ignore-submodules=none",
    ];
    args.extend(PATCH_OPTIONS);
    args.push("--");
    let bytes = git_bytes(worktree, args)?;
    let patch =
        (bytes.len() <= MAX_PREVIEW_BYTES).then(|| String::from_utf8_lossy(&bytes).into_owned());
    ensure!(
        read_basis(worktree)? == basis,
        "Staged changes or the branch changed. Reload the preview."
    );
    Ok(CommitPreview {
        worktree: worktree.to_path_buf(),
        basis,
        files,
        stat,
        patch,
    })
}

pub fn commit_staged(worktree: &Path, preview: &CommitPreview, message: &str) -> Result<()> {
    ensure!(
        worktree == preview.worktree,
        "Reload the preview in the current worktree"
    );
    ensure!(!message.trim().is_empty(), "Enter a commit message");
    ensure!(
        read_basis(worktree)? == preview.basis,
        "Staged changes or the branch changed. Reload the preview."
    );
    git_input(worktree, &["commit", "--file=-"], message)
}
