//! Task review compares the working tree with a pinned merge-base commit.

use std::path::Path;

use anyhow::{Result, bail};

use crate::runner::git_bytes_in_index;
use crate::status::PATCH_OPTIONS;
use crate::submodule::{Patch, SubmoduleChange, inline_submodules, with_submodule_files};
use crate::{FileChange, FileState, WorktreeStatus, git};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchChanges {
    pub merge_base: String,
    /// The superproject's files, each changed submodule's after its entry.
    pub files: Vec<FileChange>,
    pub patch: String,
    pub submodules: Vec<SubmoduleChange>,
}

/// Only branch refs are accepted; revision expressions/options are not bases.
pub fn resolve_branch(worktree: &Path, base: &str) -> Result<String> {
    if base.is_empty() || base.starts_with('-') || base.contains(['\n', '\0']) {
        bail!("Choose an existing local or remote base branch");
    }
    let candidates = if base.starts_with("refs/heads/") || base.starts_with("refs/remotes/") {
        vec![base.to_string()]
    } else {
        vec![
            format!("refs/heads/{base}"),
            format!("refs/remotes/{base}"),
            format!("refs/remotes/origin/{base}"),
        ]
    };
    for candidate in candidates {
        if let Ok(oid) = git(worktree, ["show-ref", "--verify", "--hash", &candidate]) {
            return Ok(oid.trim().to_string());
        }
    }
    bail!("Base branch '{base}' is unavailable locally. Fetch it or choose another branch.")
}

/// Where a pull request's changes start, from local commits: the merge base
/// of `head` with the base commit GitHub reports (`baseRefOid` — exact, for
/// merged PRs too) or, when that commit is not here, with `origin/<base>` or
/// the local `<base>` (right for open PRs). Nothing is fetched.
pub fn pr_merge_base(repo: &Path, base_oid: &str, base: &str, head: &str) -> Result<String> {
    let commit = |oid: &str| {
        oid.len() >= 7
            && oid.chars().all(|c| c.is_ascii_hexdigit())
            && git(repo, ["cat-file", "-e", &format!("{oid}^{{commit}}")]).is_ok()
    };
    if !commit(head) {
        bail!(
            "Its head {} is not in the local repository; fetch the branch first",
            &head[..8.min(head.len())]
        );
    }
    let base_oid = if commit(base_oid) {
        base_oid.to_string()
    } else {
        resolve_branch(repo, &format!("origin/{base}")).or_else(|_| resolve_branch(repo, base))?
    };
    Ok(git(repo, ["merge-base", &base_oid, head])?
        .trim()
        .to_string())
}

/// The patch from `from` to `head`, as `git diff` shows it, with changed
/// submodules inlined from their checkouts in `repo`.
pub fn range_patch(repo: &Path, from: &str, head: &str) -> Result<Patch> {
    let mut args = vec!["diff"];
    args.extend(PATCH_OPTIONS);
    args.extend([from, head, "--"]);
    Ok(inline_submodules(repo, &git(repo, args)?, None))
}

/// An explicit default suggestion, never a hard-coded assumption about main.
pub fn suggested_base(worktree: &Path) -> Option<String> {
    git(
        worktree,
        [
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .ok()
    .map(|name| name.trim().to_string())
}

pub fn branch_changes(
    worktree: &Path,
    status: &WorktreeStatus,
    base: &str,
) -> Result<BranchChanges> {
    if status.is_unborn() {
        bail!("Branch comparison needs a first commit. Use Uncommitted for an unborn branch.");
    }
    let base_oid = resolve_branch(worktree, base)?;
    let merge_base = git(worktree, ["merge-base", "HEAD", &base_oid])?
        .trim()
        .to_string();
    // Intent-to-add entries in a private copy let Git compare untracked files
    // correctly, including a path deleted in HEAD and recreated locally. The
    // user's index, staging and worktree are never modified.
    let untracked: Vec<_> = status
        .files
        .iter()
        .filter(|file| file.state() == FileState::Untracked)
        .collect();
    let temporary = if untracked.is_empty() {
        None
    } else {
        Some(tempfile::tempdir()?)
    };
    let index = temporary.as_ref().map(|dir| dir.path().join("index"));
    if let Some(index) = &index {
        let source = git(
            worktree,
            ["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )?;
        if Path::new(source.trim()).is_file() {
            std::fs::copy(source.trim(), index)?;
        } else {
            git_bytes_in_index(worktree, ["read-tree", "HEAD"], Some(index))?;
        }
        for chunk in untracked.chunks(100) {
            let mut args = vec!["add", "--intent-to-add", "--"];
            args.extend(chunk.iter().map(|file| file.path.as_str()));
            git_bytes_in_index(worktree, args, Some(index))?;
        }
    }
    // The patch and the file list read the same (private) index: both at once.
    let (patch, raw) = crate::parallel::join(
        || -> Result<Patch> {
            let mut args = vec!["diff"];
            args.extend(PATCH_OPTIONS);
            args.extend(["--no-textconv", &merge_base, "--"]);
            let patch = git_bytes_in_index(worktree, args, index.as_deref())?;
            Ok(inline_submodules(
                worktree,
                &String::from_utf8_lossy(&patch),
                Some(status),
            ))
        },
        || {
            git_bytes_in_index(
                worktree,
                [
                    "diff",
                    "--name-status",
                    "-z",
                    "-M",
                    "--submodule=short",
                    &merge_base,
                    "--",
                ],
                index.as_deref(),
            )
        },
    );
    let (patch, raw) = (patch?, raw?);
    let mut files = parse_name_status(&String::from_utf8_lossy(&raw));
    // A conflicted index may emit multiple records for a path.
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.dedup_by(|a, b| a.path == b.path);
    for file in &mut files {
        if status
            .files
            .iter()
            .any(|current| current.path == file.path && current.state() == FileState::Conflicted)
        {
            file.unstaged = Some(FileState::Conflicted);
        }
    }
    Ok(BranchChanges {
        merge_base,
        files: with_submodule_files(files, &patch.submodules),
        patch: patch.text,
        submodules: patch.submodules,
    })
}

/// `git diff --name-status -z` records as changed files.
pub(crate) fn parse_name_status(text: &str) -> Vec<FileChange> {
    let mut fields = text.split('\0');
    let mut files = Vec::new();
    while let Some(code) = fields.next().filter(|code| !code.is_empty()) {
        let Some(first) = fields.next() else {
            break;
        };
        let kind = code.as_bytes()[0];
        let (path, orig_path) = if matches!(kind, b'R' | b'C') {
            let Some(next) = fields.next() else {
                break;
            };
            (next.to_string(), Some(first.to_string()))
        } else {
            (first.to_string(), None)
        };
        files.push(FileChange {
            path,
            orig_path,
            unstaged: Some(FileState::from_code(kind).unwrap_or(FileState::Conflicted)),
            ..FileChange::default()
        });
    }
    files
}
