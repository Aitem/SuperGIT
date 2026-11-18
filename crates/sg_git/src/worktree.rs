use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};

use crate::{git, git_bytes, runner::git_timeout};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// Commit checked out, if any.
    pub head: Option<String>,
    /// Short branch name (`feature/x`), `None` when detached.
    pub branch: Option<String>,
    pub is_main: bool,
    pub is_bare: bool,
    pub is_detached: bool,
    pub locked: Option<String>,
    pub prunable: Option<String>,
}

impl Worktree {
    /// Name to show in the UI: the branch, or a short sha when detached.
    pub fn display_name(&self) -> String {
        if let Some(branch) = &self.branch {
            return branch.clone();
        }
        match &self.head {
            Some(sha) => format!("({})", &sha[..sha.len().min(8)]),
            None => "(unknown)".into(),
        }
    }

    pub fn dir_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// All worktrees of the repository, main worktree first.
pub fn list_worktrees(repo_root: &Path) -> Result<Vec<Worktree>> {
    let out = git_bytes(repo_root, ["worktree", "list", "--porcelain", "-z"])?;
    Ok(parse_porcelain(&String::from_utf8_lossy(&out)))
}

fn parse_porcelain(out: &str) -> Vec<Worktree> {
    let mut list = Vec::new();
    let mut current: Option<Worktree> = None;
    // With -z every attribute ends with NUL and an empty attribute ends a record.
    for field in out.split('\0') {
        if field.is_empty() {
            if let Some(wt) = current.take() {
                list.push(wt);
            }
            continue;
        }
        let (key, value) = field.split_once(' ').unwrap_or((field, ""));
        match key {
            "worktree" => {
                if let Some(wt) = current.take() {
                    list.push(wt);
                }
                current = Some(Worktree {
                    path: PathBuf::from(value),
                    ..Default::default()
                });
            }
            _ => {
                let Some(wt) = current.as_mut() else { continue };
                match key {
                    "HEAD" => wt.head = Some(value.to_string()),
                    "branch" => {
                        wt.branch = Some(
                            value
                                .strip_prefix("refs/heads/")
                                .unwrap_or(value)
                                .to_string(),
                        )
                    }
                    "detached" => wt.is_detached = true,
                    "bare" => wt.is_bare = true,
                    "locked" => wt.locked = Some(value.to_string()),
                    "prunable" => wt.prunable = Some(value.to_string()),
                    _ => {}
                }
            }
        }
    }
    if let Some(wt) = current.take() {
        list.push(wt);
    }
    if let Some(first) = list.first_mut() {
        first.is_main = true;
    }
    list
}

/// Create a worktree at `path`. With `new_branch`, creates that branch from
/// `base` (or HEAD); otherwise checks out the existing branch `base`.
pub fn add_worktree(
    repo_root: &Path,
    path: &Path,
    new_branch: Option<&str>,
    base: Option<&str>,
) -> Result<()> {
    let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
    if let Some(branch) = new_branch {
        args.push("-b".into());
        args.push(branch.into());
    }
    args.push(path.to_string_lossy().into_owned());
    if let Some(base) = base {
        args.push(base.into());
    }
    git_bytes(repo_root, &args)?;
    Ok(())
}

/// Get the reviewed PR commit from a GitHub pull ref when it is missing locally.
/// The explicit refspec, empty refmap and no FETCH_HEAD keep local refs intact.
pub fn require_pull_commit(
    root: &Path,
    remote: Option<&str>,
    number: u64,
    head: &str,
) -> Result<()> {
    ensure!(
        number > 0
            && matches!(head.len(), 40 | 64)
            && head.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "The pull request did not provide a valid commit ID"
    );
    let exists = || git(root, ["cat-file", "-t", head]).is_ok_and(|kind| kind.trim() == "commit");
    if exists() {
        return Ok(());
    }
    let remote = remote.context("Add a fetch remote for the pull request’s GitHub repository")?;
    let reference = format!("refs/pull/{number}/head");
    git_timeout(
        root,
        &[
            "fetch",
            "--no-write-fetch-head",
            "--no-tags",
            "--no-recurse-submodules",
            "--refmap=",
            "--",
            remote,
            &reference,
        ],
        Duration::from_secs(120),
    )?;
    ensure!(
        exists(),
        "The pull request head changed. Refresh its details and review the new commit."
    );
    Ok(())
}

/// Remove a linked worktree. Without `force`, git refuses when it is dirty.
pub fn remove_worktree(repo_root: &Path, path: &Path, force: bool) -> Result<()> {
    let path = path.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path);
    git_bytes(repo_root, args)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_z() {
        let out = "worktree /repo\0HEAD abc123\0branch refs/heads/main\0\0\
                   worktree /repo-wt/feat\0HEAD def456\0branch refs/heads/feature/x\0locked\0\0\
                   worktree /repo-wt/det\0HEAD 0123456789\0detached\0\0";
        let wts = parse_porcelain(out);
        assert_eq!(wts.len(), 3);
        assert!(wts[0].is_main);
        assert_eq!(wts[0].branch.as_deref(), Some("main"));
        assert_eq!(wts[1].branch.as_deref(), Some("feature/x"));
        assert_eq!(wts[1].locked.as_deref(), Some(""));
        assert!(wts[2].is_detached);
        assert_eq!(wts[2].display_name(), "(01234567)");
    }
}
