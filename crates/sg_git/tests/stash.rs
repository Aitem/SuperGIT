#![expect(
    clippy::unwrap_used,
    reason = "test helpers fail the test by panicking"
)]

use std::fs;
use std::path::Path;

use sg_git::{apply_stash, git, list_stashes, save_stash, status};

fn run(path: &Path, args: &[&str]) -> String {
    git(path, args).unwrap().trim().to_owned()
}

fn repo() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let path = root.path();
    run(path, &["init", "-q", "-b", "main"]);
    run(path, &["config", "user.name", "Test"]);
    run(path, &["config", "user.email", "test@example.test"]);
    run(path, &["config", "commit.gpgsign", "false"]);
    fs::write(path.join("file.txt"), "base\n").unwrap();
    run(path, &["add", "file.txt"]);
    run(path, &["commit", "-qm", "base"]);
    root
}

#[test]
fn restores_the_named_stash_by_oid_after_new_entries_are_added() {
    let root = repo();
    let path = root.path();
    fs::write(path.join("file.txt"), "staged\n").unwrap();
    run(path, &["add", "file.txt"]);
    fs::write(path.join("file.txt"), "unstaged\n").unwrap();
    fs::write(path.join("new file.txt"), "untracked\n").unwrap();
    let preview = status(path).unwrap();
    save_stash(path, &preview, true, "Незавершённая работа").unwrap();
    assert!(status(path).unwrap().is_clean());
    let [first] = list_stashes(path).unwrap().try_into().unwrap();
    let first: sg_git::Stash = first;
    assert!(first.subject.contains("Незавершённая работа"));
    assert_eq!(first.reference, "stash@{0}");

    fs::write(path.join("file.txt"), "newer stash\n").unwrap();
    save_stash(path, &status(path).unwrap(), false, "newer").unwrap();
    let entries = list_stashes(path).unwrap();
    assert_eq!(entries[1].oid, first.oid);
    assert_eq!(entries[1].reference, "stash@{1}");
    apply_stash(path, &status(path).unwrap(), &first.oid).unwrap();
    assert_eq!(run(path, &["show", ":file.txt"]), "staged");
    assert_eq!(
        fs::read_to_string(path.join("file.txt")).unwrap(),
        "unstaged\n"
    );
    assert_eq!(
        fs::read_to_string(path.join("new file.txt")).unwrap(),
        "untracked\n"
    );
    assert_eq!(list_stashes(path).unwrap(), entries);
}

#[test]
fn stale_or_dirty_previews_do_not_replace_worktree_changes() {
    let root = repo();
    let path = root.path();
    fs::write(path.join("file.txt"), "work in progress\n").unwrap();
    let preview = status(path).unwrap();
    fs::write(path.join("notes.txt"), "local notes\n").unwrap();
    assert!(save_stash(path, &preview, false, "stale").is_err());
    assert!(list_stashes(path).unwrap().is_empty());
    save_stash(path, &status(path).unwrap(), false, "tracked").unwrap();
    let entries = list_stashes(path).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(apply_stash(path, &status(path).unwrap(), &entries[0].oid).is_err());
    assert_eq!(
        fs::read_to_string(path.join("notes.txt")).unwrap(),
        "local notes\n"
    );
    assert_eq!(fs::read_to_string(path.join("file.txt")).unwrap(), "base\n");
    assert_eq!(list_stashes(path).unwrap(), entries);
}
