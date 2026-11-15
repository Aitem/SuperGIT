use sg_git::{RebaseOutcome, behind_base, rebase_onto};
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

    fn head(&self) -> String {
        git(&self.feature, &["rev-parse", "HEAD"])
    }
}

#[test]
fn replays_branch_commits_on_top_of_the_base() {
    let f = Fixture::new();
    commit(&f.feature, "feature.txt", "feature\n", "feature work");
    let base = commit(&f.root, "trunk.txt", "trunk\n", "trunk work");
    assert_eq!(behind_base(&f.feature, "trunk").unwrap(), 1);

    let outcome = rebase_onto(&f.feature, "trunk", &f.head()).unwrap();

    assert_eq!(outcome, RebaseOutcome::Rebased { commits: 1 });
    assert_eq!(git(&f.feature, &["rev-parse", "HEAD~1"]), base);
    assert_eq!(behind_base(&f.feature, "trunk").unwrap(), 0);
    assert!(f.feature.join("trunk.txt").exists());
    assert_eq!(
        git(&f.feature, &["symbolic-ref", "--short", "HEAD"]),
        "feature"
    );
}

#[test]
fn up_to_date_branch_is_left_alone() {
    let f = Fixture::new();
    commit(&f.feature, "feature.txt", "feature\n", "feature work");
    let head = f.head();
    assert_eq!(
        rebase_onto(&f.feature, "trunk", &head).unwrap(),
        RebaseOutcome::UpToDate
    );
    assert_eq!(f.head(), head);
}

#[test]
fn branch_without_own_commits_fast_forwards() {
    let f = Fixture::new();
    let base = commit(&f.root, "trunk.txt", "trunk\n", "trunk work");
    assert_eq!(
        rebase_onto(&f.feature, "trunk", &f.head()).unwrap(),
        RebaseOutcome::Rebased { commits: 0 }
    );
    assert_eq!(f.head(), base);
}

#[test]
fn conflicts_abort_and_leave_the_branch_untouched() {
    let f = Fixture::new();
    commit(&f.feature, "shared.txt", "feature\n", "feature edit");
    commit(&f.root, "shared.txt", "trunk\n", "trunk edit");
    let head = f.head();

    let error = rebase_onto(&f.feature, "trunk", &head)
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
    std::fs::write(f.feature.join("feature.txt"), "edited\n").unwrap();
    let head = f.head();
    let error = rebase_onto(&f.feature, "trunk", &head).unwrap_err();
    assert!(error.to_string().contains("uncommitted changes"), "{error}");
    assert_eq!(f.head(), head);

    git(&f.feature, &["checkout", "--", "feature.txt"]);
    std::fs::write(f.feature.join("notes.txt"), "scratch\n").unwrap();
    assert!(matches!(
        rebase_onto(&f.feature, "trunk", &head).unwrap(),
        RebaseOutcome::Rebased { commits: 1 }
    ));
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

    assert!(rebase_onto(&f.feature, "trunk", &head).is_err());

    assert_eq!(f.head(), head);
    assert_eq!(
        std::fs::read_to_string(f.feature.join("incoming.txt")).unwrap(),
        "mine\n"
    );
    let git_dir = PathBuf::from(git(&f.feature, &["rev-parse", "--absolute-git-dir"]));
    assert!(!git_dir.join("rebase-merge").exists());
}

#[test]
fn refuses_a_moved_head_the_base_itself_and_detached_head() {
    let f = Fixture::new();
    let stale = f.head();
    commit(&f.feature, "feature.txt", "feature\n", "feature work");
    commit(&f.root, "trunk.txt", "trunk\n", "trunk work");
    let error = rebase_onto(&f.feature, "trunk", &stale).unwrap_err();
    assert!(error.to_string().contains("moved"), "{error}");

    let root_head = git(&f.root, &["rev-parse", "HEAD"]);
    assert!(rebase_onto(&f.root, "trunk", &root_head).is_err());
    assert!(rebase_onto(&f.feature, "missing", &f.head()).is_err());

    git(&f.feature, &["checkout", "-q", "--detach"]);
    let error = rebase_onto(&f.feature, "trunk", &f.head()).unwrap_err();
    assert!(error.to_string().contains("detached"), "{error}");
}
