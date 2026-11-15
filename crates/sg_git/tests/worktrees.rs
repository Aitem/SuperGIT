//! End-to-end checks against a real temporary repository: the operations the
//! UI buttons run (create/list/status/log/remove worktrees).

use std::path::{Path, PathBuf};
use std::process::Command;

use sg_git::{FileState, Repo};

struct TempRepo {
    dir: PathBuf,
}

impl TempRepo {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("sg_git_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        let this = Self { dir };
        this.git(&["init", "-q", "-b", "main"]);
        this.git(&["config", "user.email", "t@example.com"]);
        this.git(&["config", "user.name", "Tester"]);
        // Temporary test commits must not depend on the user's signing key or hooks.
        this.git(&["config", "commit.gpgsign", "false"]);
        this.git(&["config", "core.hooksPath", "/dev/null"]);
        std::fs::write(this.root().join("a.txt"), "one\n").unwrap();
        this.git(&["add", "."]);
        this.git(&["commit", "-qm", "init"]);
        this
    }

    fn root(&self) -> PathBuf {
        self.dir.join("repo")
    }

    fn git(&self, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(self.root())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap()
}

#[test]
fn worktree_lifecycle() {
    let t = TempRepo::new("lifecycle");
    let repo = Repo::discover(&t.root()).unwrap();
    assert_eq!(canonical(&repo.root), canonical(&t.root()));

    // Create a worktree on a new branch, as "Create worktree" does.
    let wt_path = t.dir.join("wt").join("feature-x");
    std::fs::create_dir_all(wt_path.parent().unwrap()).unwrap();
    sg_git::add_worktree(&repo.root, &wt_path, Some("feature/x"), None).unwrap();

    let wts = sg_git::list_worktrees(&repo.root).unwrap();
    assert_eq!(wts.len(), 2);
    assert!(wts[0].is_main);
    let wt = wts
        .iter()
        .find(|w| w.branch.as_deref() == Some("feature/x"))
        .unwrap();
    assert_eq!(canonical(&wt.path), canonical(&wt_path));

    // Discovering from inside the linked worktree still finds the main one.
    let from_wt = Repo::discover(&wt_path).unwrap();
    assert_eq!(canonical(&from_wt.root), canonical(&repo.root));

    // Dirty it: one modified, one untracked file.
    std::fs::write(wt_path.join("a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(wt_path.join("new.txt"), "fresh\n").unwrap();
    let st = sg_git::status(&wt_path).unwrap();
    assert_eq!(st.branch.as_deref(), Some("feature/x"));
    assert_eq!(st.files.len(), 2);
    assert_eq!(st.untracked_count(), 1);

    let patch = sg_git::worktree_patch(&wt_path, &st).unwrap();
    assert!(patch.contains("+two"), "{patch}");
    assert!(patch.contains("+++ b/new.txt"), "{patch}");
    assert!(patch.contains("+fresh"), "{patch}");

    // Commit there and read history + the commit patch.
    let run = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&wt_path)
                .status()
                .unwrap()
                .success()
        )
    };
    run(&["add", "-A"]);
    run(&[
        "commit",
        "-qm",
        "work\n\nCo-Authored-By: Claude <noreply@anthropic.com>",
    ]);
    let log = sg_git::log(&wt_path, "HEAD", 10).unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].subject, "work");
    assert!(log[0].by_agent);
    assert!(!log[1].by_agent);
    let commit_patch = sg_git::commit_patch(&wt_path, &log[0].sha).unwrap();
    assert!(commit_patch.contains("new.txt"));

    // Removing a dirty worktree needs force; a clean one does not.
    std::fs::write(wt_path.join("dirty.txt"), "x").unwrap();
    assert!(sg_git::remove_worktree(&repo.root, &wt_path, false).is_err());
    sg_git::remove_worktree(&repo.root, &wt_path, true).unwrap();
    assert_eq!(sg_git::list_worktrees(&repo.root).unwrap().len(), 1);
    assert!(!wt_path.exists());
}

#[test]
fn existing_branch_checkout_and_unborn_repo() {
    let t = TempRepo::new("existing");
    t.git(&["branch", "topic"]);
    let wt_path = t.dir.join("topic");
    sg_git::add_worktree(&t.root(), &wt_path, None, Some("topic")).unwrap();
    let st = sg_git::status(&wt_path).unwrap();
    assert_eq!(st.branch.as_deref(), Some("topic"));
    assert!(st.is_clean());

    // A repository with no commits yet.
    let empty = t.dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&empty)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(empty.join("f.txt"), "hi\n").unwrap();
    let st = sg_git::status(&empty).unwrap();
    assert!(st.is_unborn());
    assert_eq!(sg_git::log(&empty, "HEAD", 10).unwrap().len(), 0);
    let patch = sg_git::worktree_patch(&empty, &st).unwrap();
    assert!(patch.contains("+hi"));
    assert_eq!(FileState::Untracked, st.files[0].state());
}

#[test]
fn branch_comparison_keeps_committed_and_new_work_against_merge_base() {
    let t = TempRepo::new("comparison");
    std::fs::write(t.root().join("a.txt"), "one\n".repeat(20)).unwrap();
    t.git(&["add", "."]);
    t.git(&["commit", "-qm", "base content"]);
    t.git(&["branch", "release/trunk"]);
    t.git(&["checkout", "-qb", "agent/task"]);
    t.git(&["mv", "a.txt", "renamed.txt"]);
    std::fs::write(t.root().join("committed.txt"), "committed work\n").unwrap();
    t.git(&["add", "."]);
    t.git(&["commit", "-qm", "agent checkpoint"]);
    std::fs::write(
        t.root().join("renamed.txt"),
        format!("{}next edit\n", "one\n".repeat(20)),
    )
    .unwrap();
    std::fs::write(t.root().join("fresh.txt"), "new untracked work\n").unwrap();
    std::fs::write(t.root().join("binary.bin"), [0, 1, 2]).unwrap();
    let status = sg_git::status(&t.root()).unwrap();
    let changes = sg_git::branch_changes(&t.root(), &status, "release/trunk").unwrap();
    assert!(
        changes.patch.contains("+committed work"),
        "{}",
        changes.patch
    );
    assert!(changes.patch.contains("+next edit"));
    assert!(changes.patch.contains("+new untracked work"));
    assert!(changes.patch.contains("Binary files"));
    assert_eq!(changes.files.len(), 4);
    assert!(changes.files.iter().any(|file| file.path == "renamed.txt"));
    assert!(
        !sg_git::worktree_patch(&t.root(), &status)
            .unwrap()
            .contains("committed work")
    );
    t.git(&["checkout", "--detach", "-q"]);
    assert!(sg_git::branch_changes(&t.root(), &status, "release/trunk").is_ok());
    assert!(sg_git::branch_changes(&t.root(), &status, "missing").is_err());
    assert!(sg_git::resolve_branch(&t.root(), "HEAD~1").is_err());
    assert!(sg_git::resolve_branch(&t.root(), "--help").is_err());
}

#[test]
fn unborn_branch_comparison_is_an_error() {
    let t = TempRepo::new("comparison_unborn");
    let empty = t.dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "trunk"])
            .current_dir(&empty)
            .status()
            .unwrap()
            .success()
    );
    let status = sg_git::status(&empty).unwrap();
    assert!(
        sg_git::branch_changes(&empty, &status, "trunk")
            .unwrap_err()
            .to_string()
            .contains("first commit")
    );
}

#[test]
fn recreated_untracked_path_is_compared_to_base_without_changing_staging() {
    let t = TempRepo::new("comparison_recreated");
    t.git(&["branch", "trunk"]);
    t.git(&["rm", "-q", "a.txt"]);
    t.git(&["commit", "-qm", "remove file"]);
    std::fs::write(t.root().join("a.txt"), "one\nrestored locally\n").unwrap();
    let index = t.root().join(".git/index");
    let before = std::fs::read(&index).unwrap();
    let status = sg_git::status(&t.root()).unwrap();
    let changes = sg_git::branch_changes(&t.root(), &status, "trunk").unwrap();
    assert_eq!(changes.files.len(), 1);
    assert_eq!(changes.files[0].state(), FileState::Modified);
    assert!(changes.patch.contains("+restored locally"));
    assert!(!changes.patch.contains("new file mode"));
    assert_eq!(std::fs::read(index).unwrap(), before);
    assert_eq!(sg_git::status(&t.root()).unwrap(), status);
}

#[test]
fn branch_comparison_keeps_conflict_markers_and_conflicted_badge() {
    let t = TempRepo::new("comparison_conflict");
    t.git(&["checkout", "-qb", "topic"]);
    std::fs::write(t.root().join("a.txt"), "topic\n").unwrap();
    t.git(&["commit", "-qam", "topic edit"]);
    t.git(&["checkout", "-q", "main"]);
    std::fs::write(t.root().join("a.txt"), "main\n").unwrap();
    t.git(&["commit", "-qam", "main edit"]);
    let merge = Command::new("git")
        .args(["merge", "topic"])
        .current_dir(t.root())
        .output()
        .unwrap();
    assert!(!merge.status.success());
    let status = sg_git::status(&t.root()).unwrap();
    let changes = sg_git::branch_changes(&t.root(), &status, "topic").unwrap();
    assert!(changes.patch.contains("<<<<<<<"));
    assert!(
        changes
            .files
            .iter()
            .any(|file| file.path == "a.txt" && file.state() == FileState::Conflicted)
    );
}

#[cfg(unix)]
#[test]
fn untracked_symlinks_show_targets_without_reading_them() {
    use std::os::unix::fs::symlink;

    let t = TempRepo::new("symlink");
    std::fs::write(t.dir.join("outside.txt"), "external file contents\n").unwrap();
    symlink("../outside.txt", t.root().join("link")).unwrap();
    symlink("../missing.txt", t.root().join("broken-link")).unwrap();
    symlink("..", t.root().join("directory-link")).unwrap();

    let st = sg_git::status(&t.root()).unwrap();
    let patch = sg_git::worktree_patch(&t.root(), &st).unwrap();
    assert!(!patch.contains("external file contents"), "{patch}");
    for (name, target) in [
        ("link", "../outside.txt"),
        ("broken-link", "../missing.txt"),
        ("directory-link", ".."),
    ] {
        assert!(
            patch.contains(&format!(
                "diff --git a/{name} b/{name}\nnew file mode 120000\n"
            )),
            "{patch}"
        );
        assert!(
            patch.contains(&format!("+{target}\n\\ No newline at end of file\n")),
            "{patch}"
        );
    }
}

#[test]
fn fork_counts_commits_since_branching_from_the_base() {
    let t = TempRepo::new("fork");
    // The main checkout is the base itself: nothing to count.
    assert_eq!(sg_git::base_branch(&t.root()).as_deref(), Some("main"));
    assert_eq!(sg_git::fork(&t.root()).unwrap(), None);

    let wt = t.dir.join("feature");
    sg_git::add_worktree(&t.root(), &wt, Some("feature"), None).unwrap();
    let commit = |dir: &Path, msg: &str| {
        let ok = Command::new("git")
            .args(["commit", "-q", "--allow-empty", "-m", msg])
            .current_dir(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok);
    };
    assert_eq!(sg_git::fork(&wt).unwrap().unwrap().commits, 0);
    commit(&wt, "one");
    commit(&wt, "two");
    // New work on main does not count against the branch.
    commit(&t.root(), "main moves on");
    assert_eq!(
        sg_git::fork(&wt).unwrap(),
        Some(sg_git::Fork {
            base: "main".into(),
            commits: 2
        })
    );

    // A remote's default branch wins over the local one.
    t.git(&["update-ref", "refs/remotes/origin/main", "HEAD~1"]);
    t.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    ]);
    assert_eq!(sg_git::base_branch(&wt).as_deref(), Some("origin/main"));
    assert_eq!(sg_git::fork(&wt).unwrap().unwrap().commits, 2);
    // Local `main` is ahead of `origin/main` by one commit: unpushed, but
    // still the base checkout, so no count.
    assert_eq!(sg_git::fork(&t.root()).unwrap(), None);
}

#[test]
fn commit_details_carry_the_full_message() {
    let t = TempRepo::new("details");
    std::fs::write(t.root().join("b.txt"), "two\n").unwrap();
    t.git(&["add", "."]);
    t.git(&[
        "commit",
        "-q",
        "-m",
        "Add b",
        "-m",
        "Why: the second file.\nSecond line.",
        "-m",
        "Co-Authored-By: Claude <noreply@anthropic.com>",
    ]);
    let head = sg_git::log(&t.root(), "HEAD", 1).unwrap().remove(0);
    let details = sg_git::commit_details(&t.root(), &head.sha).unwrap();
    assert_eq!(details.commit, head);
    assert_eq!(details.commit.subject, "Add b");
    assert!(details.commit.by_agent);
    assert_eq!(
        details.body,
        "Why: the second file.\nSecond line.\n\nCo-Authored-By: Claude <noreply@anthropic.com>"
    );
    assert_eq!(details.committer, "Tester");
    // `YYYY-MM-DD HH:MM`
    assert_eq!(details.date.len(), 16, "{}", details.date);
    assert!(sg_git::commit_details(&t.root(), "0000000000000000000000000000000000000000").is_err());
}
