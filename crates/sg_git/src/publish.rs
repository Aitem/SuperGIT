//! Preview and publish exactly one branch tip from a worktree.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};

use crate::git;
use crate::runner::{GitError, git_timeout};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchPublication {
    pub branch: String,
    pub head: String,
    pub remote: String,
    pub remote_branch: String,
    push_url: String,
}

impl BranchPublication {
    pub fn push_url(&self) -> &str {
        &self.push_url
    }
}

fn config(path: &Path, key: &str) -> Result<Option<String>> {
    match git(path, ["config", "--get", key]) {
        Ok(value) => Ok(Some(value.trim_end_matches('\n').to_string())),
        Err(error)
            if error
                .downcast_ref::<GitError>()
                .is_some_and(|e| e.code == Some(1)) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub fn branch_publication(path: &Path) -> Result<BranchPublication> {
    let reference = git(path, ["symbolic-ref", "--quiet", "HEAD"])
        .context("Select a worktree on a branch to publish it")?;
    let reference = reference.trim_end();
    let branch = reference
        .strip_prefix("refs/heads/")
        .context("HEAD does not point to a local branch")?;
    let head = git(path, ["rev-parse", "--verify", "HEAD^{commit}"])
        .context("Commit your changes before publishing this worktree")?;
    let tracked = config(path, &format!("branch.{branch}.remote"))?;
    let remotes = git(path, ["remote"])?;
    let remotes: Vec<_> = remotes.lines().collect();
    let configured = config(path, &format!("branch.{branch}.pushRemote"))?;
    let configured = match configured {
        Some(remote) => Some(remote),
        None => config(path, "remote.pushDefault")?
            .or_else(|| tracked.clone().filter(|remote| remote != ".")),
    };
    let remote = configured
        .as_deref()
        .or_else(|| {
            if remotes.contains(&"origin") {
                Some("origin")
            } else if let [only] = remotes.as_slice() {
                Some(*only)
            } else {
                None
            }
        })
        .context("Configure a push remote for this branch or add an origin remote")?;
    ensure!(
        remotes.contains(&remote),
        "Push remote '{remote}' is unavailable"
    );
    let push_urls = git(path, ["remote", "get-url", "--push", "--all", "--", remote])?;
    let mut push_urls = push_urls.lines();
    let url = push_urls
        .next()
        .filter(|url| !url.is_empty())
        .context("The push remote has no URL")?;
    ensure!(
        push_urls.next().is_none(),
        "Choose a remote with a single push URL"
    );
    let tracking_ref = if tracked.as_deref() == Some(remote) {
        config(path, &format!("branch.{branch}.merge"))?
    } else {
        None
    };
    let remote_branch = match tracking_ref.as_deref() {
        Some(reference) => reference
            .strip_prefix("refs/heads/")
            .context("The upstream is not a branch")?,
        None => branch,
    };
    git(
        path,
        ["check-ref-format", &format!("refs/heads/{remote_branch}")],
    )?;
    Ok(BranchPublication {
        branch: branch.to_string(),
        head: head.trim_end().to_string(),
        remote: remote.to_string(),
        remote_branch: remote_branch.to_string(),
        push_url: url.to_string(),
    })
}

/// Preview commits against GitHub's observed base tip when that commit is local.
pub fn publication_commits(
    path: &Path,
    publication: &BranchPublication,
    base_oid: &str,
) -> Result<Option<(usize, Vec<String>)>> {
    ensure!(
        matches!(base_oid.len(), 40 | 64) && base_oid.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid remote base commit"
    );
    match git(
        path,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{base_oid}^{{commit}}"),
        ],
    ) {
        Ok(_) => {}
        Err(error)
            if error
                .downcast_ref::<GitError>()
                .is_some_and(|e| e.code == Some(1)) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    }
    let range = format!("{base_oid}..{}", publication.head);
    let count = git(path, ["rev-list", "--count", &range, "--"])?
        .trim()
        .parse()?;
    let subjects = git(path, ["log", "-6", "--format=%h %s", &range, "--"])?;
    Ok(Some((
        count,
        subjects.lines().map(str::to_string).collect(),
    )))
}

/// Push the confirmed commit by object ID, even if the branch moves after validation.
/// The explicit refspec and options limit the write to its remote branch.
pub fn publish_branch(path: &Path, expected: &BranchPublication) -> Result<()> {
    ensure!(
        branch_publication(path)? == *expected,
        "The branch tip or push destination changed; reopen the preview"
    );
    let refspec = format!("{}:refs/heads/{}", expected.head, expected.remote_branch);
    git_timeout(
        path,
        &[
            "push",
            "--no-mirror",
            "--no-follow-tags",
            "--recurse-submodules=no",
            "--",
            &expected.remote,
            &refspec,
        ],
        Duration::from_secs(120),
    )
}
