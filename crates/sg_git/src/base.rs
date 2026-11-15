//! Explicit worktree bases and deliberate, fast-forward-only updates.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};

use crate::{Repo, Worktree, git, list_worktrees, status};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upstream {
    pub remote: String,
    pub source: String,
    pub tracking: String,
    url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct BaseInfo {
    pub root: PathBuf,
    pub common_dir: PathBuf,
    pub branches: Vec<String>,
    pub branch: Option<String>,
    pub tip: Option<String>,
    pub upstream: Option<Upstream>,
    pub ahead_behind: Option<(u64, u64)>,
    pub checkouts: Vec<Worktree>,
}

impl BaseInfo {
    pub fn creation_ref(&self) -> Result<String> {
        let branch = self
            .branch
            .as_deref()
            .context("Choose a local base branch")?;
        ensure!(
            self.tip.is_some(),
            "Base '{branch}' has no local commit; choose an existing branch"
        );
        Ok(format!("refs/heads/{branch}"))
    }
}

/// Local metadata only. Opening the app or creating a worktree never fetches.
pub fn load_base(root: &Path, selected: Option<&str>) -> Result<BaseInfo> {
    let repo = Repo::discover(root)?;
    let branches: Vec<String> = git(
        root,
        ["for-each-ref", "--format=%(refname:strip=2)", "refs/heads/"],
    )?
    .lines()
    .map(str::to_string)
    .collect();
    let worktrees = list_worktrees(root)?;
    let remote_heads = git(
        root,
        [
            "for-each-ref",
            "--format=%(refname)%00%(symref)",
            "refs/remotes/",
        ],
    )?;
    let defaults: Vec<_> = remote_heads
        .lines()
        .filter_map(|line| {
            let (reference, target) = line.split_once('\0')?;
            let remote = reference
                .strip_prefix("refs/remotes/")?
                .strip_suffix("/HEAD")?;
            let branch = target.strip_prefix(&format!("refs/remotes/{remote}/"))?;
            Some((remote, branch))
        })
        .collect();
    let default = defaults
        .iter()
        .find(|(remote, _)| *remote == "origin")
        .map(|(_, branch)| (*branch).to_string())
        .or_else(|| (defaults.len() == 1).then(|| defaults[0].1.to_string()));
    let branch = selected
        .map(str::to_string)
        .or(default)
        .or_else(|| {
            ["main", "master"]
                .into_iter()
                .find(|name| branches.iter().any(|branch| branch == name))
                .map(str::to_string)
        })
        .or_else(|| {
            worktrees
                .iter()
                .find(|wt| wt.is_main)
                .and_then(|wt| wt.branch.clone())
        })
        .or_else(|| (branches.len() == 1).then(|| branches[0].clone()));
    let mut info = BaseInfo {
        root: repo.root,
        common_dir: repo.common_dir,
        branches,
        branch,
        tip: None,
        upstream: None,
        ahead_behind: None,
        checkouts: Vec::new(),
    };
    let Some(branch) = &info.branch else {
        return Ok(info);
    };
    git(root, ["check-ref-format", "--branch", branch])?;
    let reference = format!("refs/heads/{branch}");
    info.tip = git(root, ["show-ref", "--verify", "--hash", &reference])
        .ok()
        .map(|oid| oid.trim().to_string());
    info.checkouts = worktrees
        .into_iter()
        .filter(|wt| wt.branch.as_ref() == Some(branch))
        .collect();
    if info.tip.is_none() {
        return Ok(info);
    }
    let fields = git(
        root,
        [
            "for-each-ref",
            "--format=%(upstream:remotename)%00%(upstream:remoteref)%00%(upstream)",
            &reference,
        ],
    )?;
    let parts: Vec<_> = fields.trim_end_matches('\n').split('\0').collect();
    if let [remote, source, tracking] = parts.as_slice()
        && !remote.is_empty()
        && !source.is_empty()
        && !tracking.is_empty()
    {
        let url = if *remote == "." {
            None
        } else {
            Some(git(root, ["remote", "get-url", remote])?.trim().to_string())
        };
        info.upstream = Some(Upstream {
            remote: remote.to_string(),
            source: source.to_string(),
            tracking: tracking.to_string(),
            url,
        });
        info.ahead_behind = git(
            root,
            [
                "rev-list",
                "--left-right",
                "--count",
                &format!("{reference}...{tracking}"),
            ],
        )
        .ok()
        .and_then(|counts| {
            let mut numbers = counts.split_whitespace();
            Some((numbers.next()?.parse().ok()?, numbers.next()?.parse().ok()?))
        });
    }
    Ok(info)
}

fn validate_checkout(worktree: &Worktree) -> Result<()> {
    ensure!(
        worktree.locked.is_none(),
        "Base worktree is locked: {}",
        worktree.path.display()
    );
    ensure!(
        !worktree.is_bare && worktree.prunable.is_none(),
        "Base worktree is unavailable"
    );
    ensure!(
        status(&worktree.path)?.is_clean(),
        "Base worktree has local changes; commit or handle them before updating"
    );
    let directory = PathBuf::from(git(&worktree.path, ["rev-parse", "--absolute-git-dir"])?.trim());
    for marker in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "index.lock",
    ] {
        ensure!(
            !directory.join(marker).exists(),
            "Base worktree has an unfinished Git operation ({marker})"
        );
    }
    Ok(())
}

fn revalidate(expected: &BaseInfo) -> Result<()> {
    let current = load_base(&expected.root, expected.branch.as_deref())?;
    ensure!(
        current.common_dir == expected.common_dir
            && current.tip == expected.tip
            && current.upstream == expected.upstream
            && current.checkouts == expected.checkouts,
        "The base branch, upstream or checkout changed; refresh and try again"
    );
    ensure!(
        current.checkouts.len() <= 1,
        "The base is checked out more than once; update it manually"
    );
    for worktree in &current.checkouts {
        validate_checkout(worktree)?;
    }
    Ok(())
}

/// Fetch the configured upstream, then advance only this base. No stash or branch switch.
pub fn update_base(expected: &BaseInfo) -> Result<()> {
    let reference = expected.creation_ref()?;
    let tip = expected.tip.as_deref().context("The base has no commit")?;
    let upstream = expected
        .upstream
        .as_ref()
        .context("This base has no upstream; configure its tracking branch in Git first")?;
    ensure!(
        upstream.source.starts_with("refs/heads/"),
        "The upstream is not a branch"
    );
    revalidate(expected)?;
    if upstream.remote != "." {
        ensure!(
            upstream.tracking.starts_with("refs/remotes/"),
            "The upstream tracking ref is not a remote branch"
        );
        let refspec = format!("{}:{}", upstream.source, upstream.tracking);
        crate::runner::git_timeout(
            &expected.root,
            &["fetch", "--no-tags", "--", &upstream.remote, &refspec],
            Duration::from_secs(30),
        )
        .context(
            "Could not fetch the configured upstream; check connectivity and Git authentication",
        )?;
    }
    // A fetch can take time: recheck local changes and branch/config identity afterward.
    revalidate(expected)?;
    let target = git(
        &expected.root,
        [
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", upstream.tracking),
        ],
    )?
    .trim()
    .to_string();
    if tip == target {
        return Ok(());
    }
    ensure!(
        git(
            &expected.root,
            ["merge-base", "--is-ancestor", tip, &target]
        )
        .is_ok(),
        "Base cannot fast-forward: it has local or diverged commits. Review it manually; no merge, rebase or reset was performed"
    );
    if let Some(worktree) = expected.checkouts.first() {
        // Git checks index/working-tree safety and updates HEAD with its own ref lock.
        git(
            &worktree.path,
            [
                "-c",
                "merge.autoStash=false",
                "merge",
                "--ff-only",
                "--no-edit",
                "--no-autostash",
                &target,
            ],
        )?;
    } else {
        // Local fetch checks fast-forward safety and refuses a ref checked out in
        // any worktree. It also uses Git's expected-old ref update under lock.
        crate::runner::git_timeout(
            &expected.root,
            &[
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                ".",
                &format!("{target}:{reference}"),
            ],
            Duration::from_secs(30),
        )?;
    }
    ensure!(
        git(
            &expected.root,
            ["show-ref", "--verify", "--hash", &reference]
        )?
        .trim()
            == target,
        "The base changed concurrently; refresh and inspect its current commit"
    );
    Ok(())
}
