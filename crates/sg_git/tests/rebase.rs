#![expect(
    clippy::unwrap_used,
    reason = "test helpers fail the test by panicking"
)]

use sg_git::{RebaseOutcome, plan_rebase, rebase};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn commit(path: &Path, file: &str, content: &str, message: &str) -> String {
    std::fs::write(path.join(file), content).unwrap();
    git(path, &["add", file]);
    git(path, &["commit", "-qm", message]);
    git(path, &["rev-parse", "HEAD"])
}

/// A repository on `trunk` with a linked worktree on `feature`.
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    feature: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let feature = temp.path().join("feature");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "trunk"]);
        for (key, value) in [
            ("user.name", "Tester"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
        ] {
            git(&root, &["config", key, value]);
        }
        commit(&root, "shared.txt", "one\n", "initial");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                feature.to_str().unwrap(),
            ],
        );
        Self {
            _temp: temp,
            root,
            feature,
        }
    }

    fn temp_path(&self) -> &Path {
        self._temp.path()
    }

    fn head(&self) -> String {
        git(&self.feature, &["rev-parse", "HEAD"])
    }
}

#[test]
fn plan_describes_and_rebase_replays_onto_the_chosen_branch() {
    let f = Fixture::new();
    commit(&f.feature, "feature.txt", "feature\n", "feature work");
    let base = commit(&f.root, "trunk.txt", "trunk\n", "trunk work");

    let plan = plan_rebase(&f.feature, "trunk").unwrap();
    assert_eq!(plan.branch, "feature");
    assert_eq!(plan.onto_tip, base);
    assert_eq!((plan.behind, plan.replayed), (1, 1));
    assert_eq!(plan.upstream, None);

    assert_eq!(
        rebase(&plan).unwrap(),
        RebaseOutcome::Rebased { commits: 1 }
    );
    assert_eq!(git(&f.feature, &["rev-parse", "HEAD~1"]), base);
    assert!(f.feature.join("trunk.txt").exists());
    assert_eq!(
        git(&f.feature, &["symbolic-ref", "--short", "HEAD"]),
        "feature"
    );
    assert!(plan_rebase(&f.feature, "trunk").unwrap().is_up_to_date());
}

#[test]
fn any_local_branch_can_be_the_target() {
    let f = Fixture::new();
    git(&f.root, &["branch", "release"]);
    let release = f.temp_path().join("release");
    git(
        &f.root,
        &[
            "worktree",
            "add",
            "-q",
            release.to_str().unwrap(),
            "release",
        ],
    );
    let fix = commit(&release, "fix.txt", "fix\n", "release fix");
    commit(&f.feature, "feature.txt", "feature\n", "feature work");

    let plan = plan_rebase(&f.feature, "release").unwrap();
    rebase(&plan).unwrap();

    assert_eq!(git(&f.feature, &["rev-parse", "HEAD~1"]), fix);
}

#[test]
fn up_to_date_branch_is_left_alone() {
    let f = Fixture::new();
    commit(&f.feature, "feature.txt", "feature\n", "feature work");
    let head = f.head();
    let plan = plan_rebase(&f.feature, "trunk").unwrap();
    assert!(plan.is_up_to_date());
    assert_eq!(rebase(&plan).unwrap(), RebaseOutcome::UpToDate);
    assert_eq!(f.head(), head);
}

#[test]
fn merged_branch_has_nothing_to_replay_and_just_moves() {
    let f = Fixture::new();
    commit(&f.feature, "feature.txt", "feature\n", "feature work");
    git(
        &f.root,
        &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"],
    );
    let merge = git(&f.root, &["rev-parse", "HEAD"]);

    let plan = plan_rebase(&f.feature, "trunk").unwrap();
    assert_eq!((plan.behind, plan.replayed), (1, 0));
    assert_eq!(
        rebase(&plan).unwrap(),
        RebaseOutcome::Rebased { commits: 0 }
    );
    assert_eq!(f.head(), merge);
}

#[test]
fn plan_reports_a_pushed_or_gone_upstream() {
    let f = Fixture::new();
    let remote = f.temp_path().join("remote.git");
    git(
        f.temp_path(),
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );
    git(
        &f.root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&f.feature, &["push", "-qu", "origin", "feature"]);
    commit(&f.root, "trunk.txt", "trunk\n", "trunk work");

    let plan = plan_rebase(&f.feature, "trunk").unwrap();
    assert_eq!(plan.upstream.as_deref(), Some("origin/feature"));
    assert!(!plan.upstream_gone);

    git(
        &f.root,
        &["update-ref", "-d", "refs/remotes/origin/feature"],
    );
    assert!(plan_rebase(&f.feature, "trunk").unwrap().upstream_gone);
}

#[test]
fn conflicts_abort_and_leave_the_branch_untouched() {
    let f = Fixture::new();
    commit(&f.feature, "shared.txt", "feature\n", "feature edit");
    commit(&f.root, "shared.txt", "trunk\n", "trunk edit");
    let head = f.head();

    let error = rebase(&plan_rebase(&f.feature, "trunk").unwrap())
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("Conflicts with trunk in shared.txt"),
        "{error}"
    );
    assert_eq!(f.head(), head);
    assert_eq!(git(&f.feature, &["status", "--porcelain"]), "");
    let git_dir = PathBuf::from(git(&f.feature, &["rev-parse", "--absolute-git-dir"]));
    assert!(!git_dir.join("rebase-merge").exists());
    assert!(!git_dir.join("rebase-apply").exists());
}

#[test]
fn refuses_tracked_changes_but_keeps_untracked_files() {
    let f = Fixture::new();
    commit(&f.feature, "feature.txt", "feature\n", "feature work");
    commit(&f.root, "trunk.txt", "trunk\n", "trunk work");
    let plan = plan_rebase(&f.feature, "trunk").unwrap();
    std::fs::write(f.feature.join("feature.txt"), "edited\n").unwrap();
    let error = rebase(&plan).unwrap_err();
    assert!(error.to_string().contains("uncommitted changes"), "{error}");
    assert_eq!(f.head(), plan.head);

    git(&f.feature, &["checkout", "--", "feature.txt"]);
    std::fs::write(f.feature.join("notes.txt"), "scratch\n").unwrap();
    assert_eq!(
        rebase(&plan).unwrap(),
        RebaseOutcome::Rebased { commits: 1 }
    );
    assert_eq!(
        std::fs::read_to_string(f.feature.join("notes.txt")).unwrap(),
        "scratch\n"
    );
}

#[test]
fn untracked_file_in_the_way_aborts_cleanly() {
    let f = Fixture::new();
    commit(&f.feature, "feature.txt", "feature\n", "feature work");
    commit(&f.root, "incoming.txt", "trunk\n", "trunk adds a file");
    std::fs::write(f.feature.join("incoming.txt"), "mine\n").unwrap();
    let head = f.head();

    assert!(rebase(&plan_rebase(&f.feature, "trunk").unwrap()).is_err());

    assert_eq!(f.head(), head);
    assert_eq!(
        std::fs::read_to_string(f.feature.join("incoming.txt")).unwrap(),
        "mine\n"
    );
    let git_dir = PathBuf::from(git(&f.feature, &["rev-parse", "--absolute-git-dir"]));
    assert!(!git_dir.join("rebase-merge").exists());
}

#[test]
fn a_branch_or_target_that_moved_after_the_preview_is_refused() {
    let f = Fixture::new();
    commit(&f.feature, "feature.txt", "feature\n", "feature work");
    commit(&f.root, "trunk.txt", "trunk\n", "trunk work");

    let plan = plan_rebase(&f.feature, "trunk").unwrap();
    commit(&f.root, "later.txt", "later\n", "trunk moves on");
    let error = rebase(&plan).unwrap_err();
    assert!(error.to_string().contains("'trunk' moved"), "{error}");

    let plan = plan_rebase(&f.feature, "trunk").unwrap();
    commit(&f.feature, "more.txt", "more\n", "agent commits");
    let error = rebase(&plan).unwrap_err();
    assert!(error.to_string().contains("'feature' moved"), "{error}");
}

#[test]
fn plan_refuses_the_branch_itself_a_missing_target_and_detached_head() {
    let f = Fixture::new();
    assert!(plan_rebase(&f.feature, "feature").is_err());
    assert!(plan_rebase(&f.feature, "missing").is_err());
    assert!(plan_rebase(&f.feature, "--force").is_err());
    git(&f.feature, &["checkout", "-q", "--detach"]);
    let error = plan_rebase(&f.feature, "trunk").unwrap_err();
    assert!(error.to_string().contains("detached"), "{error}");
}
