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
