#![expect(
    clippy::unwrap_used,
    reason = "test helpers fail the test by panicking"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use sg_git::{CleanupOutcome, CleanupPlan, CleanupPolicy, Repo};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    task: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        let this = Self {
            task: temp.path().join("task"),
            root,
            _temp: temp,
        };
        this.git(&this.root, &["init", "-q", "-b", "main"]);
        this.git(&this.root, &["config", "user.name", "Tester"]);
        this.git(&this.root, &["config", "user.email", "test@example.com"]);
        this.git(&this.root, &["config", "commit.gpgsign", "false"]);
        this.git(&this.root, &["config", "core.hooksPath", "/dev/null"]);
        std::fs::write(this.root.join("file.txt"), "base\n").unwrap();
        this.git(&this.root, &["add", "."]);
        this.git(&this.root, &["commit", "-qm", "base"]);
        sg_git::add_worktree(&this.root, &this.task, Some("task"), None).unwrap();
        this
    }
    fn git(&self, path: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn plan(&self) -> CleanupPlan {
        let repo = Repo::discover(&self.root).unwrap();
        let worktree = sg_git::list_worktrees(&repo.root)
            .unwrap()
            .into_iter()
            .find(|worktree| worktree.path == self.task.canonicalize().unwrap())
            .unwrap();
        let identity = sg_git::worktree_identity(&repo.common_dir, &worktree).unwrap();
        CleanupPlan::snapshot(
            &repo,
            &worktree,
            &sg_git::status(&worktree.path).unwrap(),
            identity,
        )
        .unwrap()
    }
    fn branch_exists(&self) -> bool {
        sg_git::git(&self.root, ["show-ref", "--verify", "refs/heads/task"]).is_ok()
    }
    fn commit_task(&self) {
        std::fs::write(self.task.join("file.txt"), "task work\n").unwrap();
        self.git(&self.task, &["commit", "-qam", "task work"]);
    }
}

#[test]
fn clean_worktree_and_local_branch_are_removed_together() {
    let fixture = Fixture::new();
    assert!(matches!(
        sg_git::cleanup_worktree(&fixture.plan(), &CleanupPolicy::default()).unwrap(),
        CleanupOutcome::Complete
    ));
    assert!(!fixture.task.exists());
    assert!(!fixture.branch_exists());
    assert_eq!(sg_git::list_worktrees(&fixture.root).unwrap().len(), 1);
}

#[test]
fn dirty_locked_and_protected_targets_preserve_both_resources() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    std::fs::write(fixture.task.join("unsent.txt"), "keep me").unwrap();
    assert!(sg_git::cleanup_worktree(&plan, &CleanupPolicy::default()).is_err());
    std::fs::remove_file(fixture.task.join("unsent.txt")).unwrap();
    fixture.git(
        &fixture.root,
        &["worktree", "lock", fixture.task.to_str().unwrap()],
    );
    assert!(sg_git::cleanup_worktree(&plan, &CleanupPolicy::default()).is_err());
    fixture.git(
        &fixture.root,
        &["worktree", "unlock", fixture.task.to_str().unwrap()],
    );
    let policy = CleanupPolicy {
        protected_branches: vec!["task".into()],
        ..Default::default()
    };
    assert!(sg_git::cleanup_worktree(&plan, &policy).is_err());
    assert!(fixture.task.exists() && fixture.branch_exists());
}

#[test]
fn changed_commits_and_recreated_branch_names_invalidate_the_preview() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    fixture.commit_task();
    assert!(sg_git::cleanup_worktree(&plan, &CleanupPolicy::default()).is_err());
    let plan = fixture.plan();
    fixture.git(&fixture.root, &["update-ref", "-d", "refs/heads/task"]);
    fixture.git(&fixture.root, &["branch", "task", &plan.tip]);
    assert!(sg_git::cleanup_worktree(&plan, &CleanupPolicy::default()).is_err());
    assert!(fixture.task.exists() && fixture.branch_exists());
}

#[test]
fn partial_cleanup_retries_only_the_preserved_branch_after_the_base_updates() {
    let fixture = Fixture::new();
    fixture.git(&fixture.root, &["branch", "integration"]);
    fixture.commit_task();
    fixture.git(&fixture.root, &["merge", "--ff-only", "task"]);
    let policy = CleanupPolicy {
        base_branch: Some("integration".into()),
        ..Default::default()
    };
    let result = sg_git::cleanup_worktree(&fixture.plan(), &policy).unwrap();
    let CleanupOutcome::BranchPreserved { pending, error } = result else {
        panic!("expected branch preservation");
    };
    assert!(error.contains("not reachable"));
    assert!(!fixture.task.exists());
    assert!(fixture.branch_exists());
    fixture.git(&fixture.root, &["branch", "-f", "integration", "main"]);
    sg_git::delete_cleanup_branch(&pending, &policy).unwrap();
    sg_git::delete_cleanup_branch(&pending, &policy).unwrap();
    assert!(!fixture.task.exists() && !fixture.branch_exists());
}

#[test]
fn squash_merge_preserves_unreachable_branch_commits_and_auto_cleanup_skips_it() {
    let fixture = Fixture::new();
    fixture.commit_task();
    fixture.git(&fixture.root, &["merge", "--squash", "task"]);
    fixture.git(&fixture.root, &["commit", "-qm", "squashed task"]);
    let automatic = CleanupPolicy {
        automatic: true,
        ..Default::default()
    };
    assert!(sg_git::cleanup_worktree(&fixture.plan(), &automatic).is_err());
    assert!(fixture.task.exists() && fixture.branch_exists());
    let result = sg_git::cleanup_worktree(&fixture.plan(), &CleanupPolicy::default()).unwrap();
    let CleanupOutcome::BranchPreserved { pending, error } = result else {
        panic!("squashed commits must be preserved");
    };
    assert!(error.contains("Squash or rebase"));
    assert!(!fixture.task.exists() && fixture.branch_exists());
    assert!(sg_git::delete_cleanup_branch(&pending, &CleanupPolicy::default()).is_err());
}

#[test]
fn a_branch_checked_out_elsewhere_is_preserved() {
    let fixture = Fixture::new();
    let other = fixture._temp.path().join("other");
    fixture.git(
        &fixture.root,
        &[
            "worktree",
            "add",
            "--force",
            other.to_str().unwrap(),
            "task",
        ],
    );
    let result = sg_git::cleanup_worktree(&fixture.plan(), &CleanupPolicy::default()).unwrap();
    assert!(matches!(result, CleanupOutcome::BranchPreserved { .. }));
    assert!(fixture.branch_exists() && other.exists() && !fixture.task.exists());
}

#[test]
fn retry_never_deletes_a_newer_tip_or_a_reused_branch() {
    let fixture = Fixture::new();
    fixture.commit_task();
    let result = sg_git::cleanup_worktree(&fixture.plan(), &CleanupPolicy::default()).unwrap();
    let CleanupOutcome::BranchPreserved { pending, .. } = result else {
        panic!("expected partial result");
    };
    fixture.git(&fixture.root, &["branch", "-D", "task"]);
    fixture.git(&fixture.root, &["branch", "task", "main"]);
    assert!(sg_git::delete_cleanup_branch(&pending, &CleanupPolicy::default()).is_err());
    assert!(fixture.branch_exists());
}

#[test]
fn ref_transaction_hooks_can_refuse_deletion_without_losing_the_branch() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new();
    let hook_dir = fixture._temp.path().join("hooks");
    std::fs::create_dir(&hook_dir).unwrap();
    let hook = hook_dir.join("reference-transaction");
    std::fs::write(&hook,"#!/bin/sh\nif [ \"$1\" = prepared ]; then\nwhile read old new ref; do\nif [ \"$ref\" = refs/heads/task ] && [ \"$new\" = 0000000000000000000000000000000000000000 ]; then exit 1; fi\ndone\nfi\nexit 0\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    fixture.git(
        &fixture.root,
        &["config", "core.hooksPath", hook_dir.to_str().unwrap()],
    );
    let result = sg_git::cleanup_worktree(&fixture.plan(), &CleanupPolicy::default()).unwrap();
    let CleanupOutcome::BranchPreserved { pending, .. } = result else {
        panic!("hook must preserve branch");
    };
    assert!(!fixture.task.exists() && fixture.branch_exists());
    fixture.git(&fixture.root, &["config", "core.hooksPath", "/dev/null"]);
    sg_git::delete_cleanup_branch(&pending, &CleanupPolicy::default()).unwrap();
    assert!(!fixture.branch_exists());
}
