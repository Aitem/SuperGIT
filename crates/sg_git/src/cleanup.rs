//! Combined local cleanup with immutable identity and compare-and-delete guards.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    Repo, Worktree, WorktreeStatus, git, git_bytes, list_worktrees, remove_worktree, status,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
    created: SystemTime,
}

impl FileIdentity {
    fn read(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path)?;
        Ok(Self {
            device: meta.dev(),
            inode: meta.ino(),
            created: meta.created()?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchIdentity {
    file: FileIdentity,
    len: u64,
    modified: SystemTime,
}

impl BranchIdentity {
    fn read(common_dir: &Path, branch: &str) -> Result<Self> {
        ensure!(
            Path::new(branch)
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_))),
            "Invalid local branch name"
        );
        let path = common_dir.join("logs/refs/heads").join(branch);
        let meta = std::fs::metadata(&path)
            .context("Branch history identity is unavailable; cleanup needs a local reflog")?;
        Ok(Self {
            file: FileIdentity::read(&path)?,
            len: meta.len(),
            modified: meta.modified()?,
        })
    }
    pub fn created(&self) -> SystemTime {
        self.file.created
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeIdentity {
    git_dir: PathBuf,
    registration: FileIdentity,
    branch: BranchIdentity,
}

impl WorktreeIdentity {
    pub fn same_lifecycle(&self, other: &Self) -> bool {
        self.git_dir == other.git_dir
            && self.registration == other.registration
            && self.branch.file == other.branch.file
    }
    pub fn predates(&self, time: SystemTime) -> bool {
        self.registration.created <= time && self.branch.created() <= time
    }
}

/// Filesystem metadata avoids another Git subprocess in each status poll.
pub fn worktree_identity(common_dir: &Path, worktree: &Worktree) -> Result<WorktreeIdentity> {
    ensure!(
        !worktree.is_main && !worktree.is_bare,
        "Only a linked worktree has a cleanup identity"
    );
    let branch = worktree
        .branch
        .as_deref()
        .context("Detached worktrees need manual Git management")?;
    let pointer = std::fs::read_to_string(worktree.path.join(".git"))?;
    let pointer = pointer
        .trim()
        .strip_prefix("gitdir: ")
        .context("Invalid worktree registration")?;
    let git_dir = worktree.path.join(pointer).canonicalize()?;
    ensure!(
        git_dir.starts_with(common_dir.join("worktrees").canonicalize()?),
        "Worktree belongs to another repository"
    );
    Ok(WorktreeIdentity {
        registration: FileIdentity::read(&git_dir)?,
        git_dir,
        branch: BranchIdentity::read(common_dir, branch)?,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupPlan {
    pub root: PathBuf,
    pub common_dir: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub tip: String,
    pub identity: WorktreeIdentity,
}

impl CleanupPlan {
    pub fn snapshot(
        repo: &Repo,
        worktree: &Worktree,
        status: &WorktreeStatus,
        identity: WorktreeIdentity,
    ) -> Result<Self> {
        ensure!(
            !worktree.is_main && !worktree.is_bare,
            "The main worktree is protected"
        );
        Ok(Self {
            root: repo.root.clone(),
            common_dir: repo.common_dir.clone(),
            path: worktree.path.clone(),
            branch: worktree
                .branch
                .clone()
                .context("Detached worktrees need manual Git management")?,
            tip: status
                .head_oid
                .clone()
                .context("An unborn branch cannot be cleaned up")?,
            identity,
        })
    }
}

#[derive(Clone, Debug)]
pub struct CleanupPolicy {
    pub protected_branches: Vec<String>,
    pub base_branch: Option<String>,
    pub automatic: bool,
}

impl Default for CleanupPolicy {
    fn default() -> Self {
        Self {
            protected_branches: vec!["main".into(), "master".into()],
            base_branch: None,
            automatic: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchDeletion {
    pub root: PathBuf,
    pub common_dir: PathBuf,
    pub branch: String,
    pub tip: String,
    pub base_ref: String,
    identity: BranchIdentity,
}

#[derive(Debug)]
pub enum CleanupOutcome {
    Complete,
    BranchPreserved {
        pending: BranchDeletion,
        error: String,
    },
}

fn verify_repository(root: &Path, expected_common: &Path) -> Result<Repo> {
    let repo = Repo::discover(root)?;
    ensure!(
        repo.common_dir.canonicalize()? == expected_common.canonicalize()?,
        "The repository changed; cleanup was stopped"
    );
    Ok(repo)
}

fn origin_default(root: &Path) -> Option<String> {
    git(
        root,
        [
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .ok()
    .and_then(|name| name.trim().strip_prefix("origin/").map(str::to_string))
}

pub fn cleanup_base_branch(root: &Path, policy: &CleanupPolicy) -> Result<String> {
    let branch = policy
        .base_branch
        .clone()
        .or_else(|| origin_default(root))
        .or_else(|| {
            list_worktrees(root)
                .ok()?
                .into_iter()
                .find(|worktree| worktree.is_main)?
                .branch
        })
        .context("Choose a default base branch before cleanup")?;
    git(root, ["check-ref-format", "--branch", &branch])?;
    Ok(branch)
}

fn ensure_unprotected(root: &Path, branch: &str, policy: &CleanupPolicy) -> Result<()> {
    let primary = list_worktrees(root)?
        .into_iter()
        .find(|worktree| worktree.is_main)
        .and_then(|worktree| worktree.branch);
    let default = origin_default(root);
    ensure!(
        primary.as_deref() != Some(branch)
            && default.as_deref() != Some(branch)
            && policy.base_branch.as_deref() != Some(branch)
            && !policy
                .protected_branches
                .iter()
                .any(|protected| protected == branch),
        "Local branch '{branch}' is protected"
    );
    git(root, ["check-ref-format", "--branch", branch])?;
    ensure!(
        git(
            root,
            ["symbolic-ref", "--quiet", &format!("refs/heads/{branch}")]
        )
        .is_err(),
        "Symbolic branches need manual Git management"
    );
    Ok(())
}

fn base_ref(root: &Path, policy: &CleanupPolicy) -> Result<String> {
    let base = cleanup_base_branch(root, policy)?;
    let local = format!("refs/heads/{base}");
    let remote = format!("refs/remotes/origin/{base}");
    let candidates = if policy.automatic {
        [remote, local]
    } else {
        [local, remote]
    };
    candidates
        .into_iter()
        .find(|reference| git(root, ["show-ref", "--verify", "--hash", reference]).is_ok())
        .context("The cleanup base is unavailable; fetch or update it first")
}

/// Fetch only the default tracking ref; never pull, prune, force or switch a checkout.
pub fn fetch_cleanup_base(
    root: &Path,
    policy: &CleanupPolicy,
    expected_origin: &str,
) -> Result<()> {
    ensure!(
        git(root, ["remote", "get-url", "origin"])?.trim() == expected_origin,
        "Origin changed; cleanup was stopped"
    );
    let base = cleanup_base_branch(root, policy)?;
    let refspec = format!("refs/heads/{base}:refs/remotes/origin/{base}");
    crate::runner::git_timeout(
        root,
        &["fetch", "--no-tags", "origin", &refspec],
        std::time::Duration::from_secs(30),
    )?;
    ensure!(
        git(root, ["remote", "get-url", "origin"])?.trim() == expected_origin,
        "Origin changed; cleanup was stopped"
    );
    Ok(())
}

fn validate_plan(plan: &CleanupPlan, policy: &CleanupPolicy) -> Result<BranchDeletion> {
    let repo = verify_repository(&plan.root, &plan.common_dir)?;
    let worktree = list_worktrees(&repo.root)?
        .into_iter()
        .find(|worktree| worktree.path == plan.path)
        .context("This worktree is no longer registered")?;
    ensure!(
        !worktree.is_main && !worktree.is_bare,
        "The main worktree is protected"
    );
    ensure!(worktree.locked.is_none(), "This worktree is locked");
    ensure!(worktree.prunable.is_none(), "This worktree is unavailable");
    ensure!(
        worktree.branch.as_deref() == Some(plan.branch.as_str())
            && worktree.head.as_deref() == Some(plan.tip.as_str()),
        "The worktree branch or commit changed; preview cleanup again"
    );
    let status = status(&plan.path)?;
    ensure!(
        status.is_clean(),
        "Local changes were found; worktree and branch were preserved"
    );
    ensure!(
        status.branch.as_deref() == Some(plan.branch.as_str())
            && status.head_oid.as_deref() == Some(plan.tip.as_str()),
        "The worktree changed; preview cleanup again"
    );
    ensure!(
        worktree_identity(&repo.common_dir, &worktree)? == plan.identity,
        "The worktree or branch was recreated or changed; preview cleanup again"
    );
    ensure_unprotected(&repo.root, &plan.branch, policy)?;
    Ok(BranchDeletion {
        root: repo.root,
        common_dir: repo.common_dir,
        branch: plan.branch.clone(),
        tip: plan.tip.clone(),
        base_ref: base_ref(&plan.root, policy)?,
        identity: plan.identity.branch.clone(),
    })
}

fn safe_deletion_base(
    pending: &BranchDeletion,
    policy: &CleanupPolicy,
    exclude: Option<&Path>,
) -> Result<String> {
    verify_repository(&pending.root, &pending.common_dir)?;
    ensure_unprotected(&pending.root, &pending.branch, policy)?;
    ensure!(
        !list_worktrees(&pending.root)?
            .iter()
            .any(
                |worktree| worktree.branch.as_deref() == Some(pending.branch.as_str())
                    && exclude != Some(worktree.path.as_path())
            ),
        "The local branch is still used by another worktree"
    );
    ensure!(
        BranchIdentity::read(&pending.common_dir, &pending.branch)? == pending.identity,
        "The local branch changed or was reused; it was preserved"
    );
    let branch_ref = format!("refs/heads/{}", pending.branch);
    ensure!(
        git(
            &pending.root,
            ["show-ref", "--verify", "--hash", &branch_ref]
        )?
        .trim()
            == pending.tip,
        "The branch tip changed; it was preserved"
    );
    ensure!(
        pending.base_ref.starts_with("refs/heads/")
            || pending.base_ref.starts_with("refs/remotes/"),
        "Invalid cleanup base"
    );
    git(&pending.root, ["check-ref-format", &pending.base_ref])?;
    let base_oid = git(
        &pending.root,
        ["show-ref", "--verify", "--hash", &pending.base_ref],
    )?
    .trim()
    .to_string();
    ensure!(
        git(
            &pending.root,
            ["merge-base", "--is-ancestor", &pending.tip, &base_oid]
        )
        .is_ok(),
        "Branch '{}' was preserved: its commits are not reachable from {}. Squash or rebase merges may need manual review.",
        pending.branch,
        pending.base_ref
    );
    Ok(base_oid)
}

/// Retry only the remaining branch step. No worktree path is removed here.
pub fn delete_cleanup_branch(pending: &BranchDeletion, policy: &CleanupPolicy) -> Result<()> {
    verify_repository(&pending.root, &pending.common_dir)?;
    if let Err(error) = git(
        &pending.root,
        [
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{}", pending.branch),
        ],
    ) {
        if error
            .downcast_ref::<crate::GitError>()
            .is_some_and(|error| error.code == Some(1))
        {
            return Ok(());
        }
        return Err(error);
    }
    let base_oid = safe_deletion_base(pending, policy, None)?;
    let input = format!(
        "start\nverify {} {}\ndelete refs/heads/{} {}\nprepare\ncommit\n",
        pending.base_ref, base_oid, pending.branch, pending.tip
    );
    // The transaction checks both refs under their locks: a concurrent commit
    // or base update must fail instead of deleting a newer branch tip.
    crate::runner::git_input(
        &pending.root,
        &["update-ref", "--no-deref", "--stdin"],
        &input,
    )?;
    // Preserve configuration if another process has already recreated the name.
    if git(
        &pending.root,
        [
            "show-ref",
            "--verify",
            "--hash",
            &format!("refs/heads/{}", pending.branch),
        ],
    )
    .is_err()
    {
        let _ = git(
            &pending.root,
            [
                "config",
                "--local",
                "--remove-section",
                &format!("branch.{}", pending.branch),
            ],
        );
    }
    Ok(())
}

/// Initialized submodule checkouts under `dir`, nested ones included.
fn submodule_checkouts(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let listed = git_bytes(dir, ["ls-files", "-z", "--stage"])?;
    for entry in String::from_utf8_lossy(&listed).split('\0') {
        // `<mode> <object> <stage>\t<path>`; gitlinks have mode 160000.
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let checkout = dir.join(path);
        if meta.starts_with("160000 ")
            && checkout.join(".git").exists()
            && !found.contains(&checkout)
        {
            found.extend(submodule_checkouts(&checkout)?);
            found.push(checkout);
        }
    }
    Ok(found)
}

/// What removing these submodule checkouts would lose. A linked worktree's
/// submodule repositories live in its admin directory
/// (`.git/worktrees/<name>/modules`) and go with it, so anything only they
/// hold — uncommitted work, commits no remote has, stashes — would be gone.
fn submodule_losses(worktree: &Path, checkouts: &[PathBuf]) -> Result<Vec<String>> {
    let mut losses = Vec::new();
    for checkout in checkouts {
        let name = checkout
            .strip_prefix(worktree)
            .unwrap_or(checkout)
            .display()
            .to_string();
        let changes = git(checkout, ["status", "--porcelain", "--untracked-files=all"])?;
        if !changes.trim().is_empty() {
            losses.push(format!("{name} has uncommitted changes"));
        }
        let local = git(
            checkout,
            [
                "rev-list",
                "--count",
                "HEAD",
                "--branches",
                "--not",
                "--remotes",
            ],
        )?;
        match local.trim() {
            "0" => {}
            "1" => losses.push(format!("{name} has 1 commit that no remote has")),
            n => losses.push(format!("{name} has {n} commits that no remote has")),
        }
        if !git(checkout, ["stash", "list"])?.trim().is_empty() {
            losses.push(format!("{name} has stashed changes"));
        }
    }
    Ok(losses)
}

pub fn cleanup_worktree(plan: &CleanupPlan, policy: &CleanupPolicy) -> Result<CleanupOutcome> {
    let pending = validate_plan(plan, policy)?;
    if policy.automatic {
        safe_deletion_base(&pending, policy, Some(&plan.path))?;
    }
    // Git removes a worktree with initialized submodules only with --force,
    // which also skips its own last check for local changes: do both checks
    // here, right before removing.
    let submodules = submodule_checkouts(&plan.path)?;
    if !submodules.is_empty() {
        let losses = submodule_losses(&plan.path, &submodules)?;
        ensure!(
            losses.is_empty(),
            "Submodules hold work that would be lost ({}); worktree and branch were preserved",
            losses.join("; ")
        );
        ensure!(
            status(&plan.path)?.is_clean(),
            "Local changes were found; worktree and branch were preserved"
        );
    }
    remove_worktree(&plan.root, &plan.path, !submodules.is_empty())?;
    match delete_cleanup_branch(&pending, policy) {
        Ok(()) => Ok(CleanupOutcome::Complete),
        Err(error) => Ok(CleanupOutcome::BranchPreserved {
            pending,
            error: error.to_string(),
        }),
    }
}
