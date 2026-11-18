#![expect(clippy::unwrap_used, reason = "tests fail on unexpected Git errors")]

use sg_git::{
    IndexEdit, commit, edit_file, edit_hunk, fetch, git, index_hunks, pull, squash, status,
};
use std::{fs, path::Path};

fn repo() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();
    git(path, ["init", "-q", "-b", "main"]).unwrap();
    for (key, value) in [
        ("user.name", "Tester"),
        ("user.email", "test@example.com"),
        ("commit.gpgSign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        git(path, ["config", key, value]).unwrap();
    }
    temp
}

fn record(path: &Path, message: &str, amend: bool) {
    let state = status(path).unwrap();
    commit(
        path,
        state.branch.as_deref(),
        state.head_oid.as_deref(),
        message,
        amend,
    )
    .unwrap();
}

#[test]
fn commits_only_staged_hunks_and_amends_with_original_message() {
    let temp = repo();
    let path = temp.path();
    let file = "source[1].txt";
    let original: String = (1..=24).map(|n| format!("line {n}\n")).collect();
    let updated = original
        .replace("line 2\n", "first edit\n")
        .replace("line 21\n", "second edit\n");
    fs::write(path.join(file), &original).unwrap();
    let change = status(path).unwrap().files.remove(0);
    edit_file(path, &change, IndexEdit::Stage).unwrap();
    edit_file(path, &change, IndexEdit::Unstage).unwrap();
    assert_eq!(status(path).unwrap().staged_count(), 0);
    edit_file(path, &change, IndexEdit::Stage).unwrap();
    record(path, "initial", false);

    fs::write(path.join(file), &updated).unwrap();
    let hunks = index_hunks(path, file, IndexEdit::Stage).unwrap();
    assert_eq!(hunks.len(), 2);
    fs::write(
        path.join(file),
        updated.replace("second edit", "new content"),
    )
    .unwrap();
    assert!(edit_hunk(path, file, &hunks[1], IndexEdit::Stage).is_err());
    fs::write(path.join(file), &updated).unwrap();
    edit_hunk(path, file, &hunks[1], IndexEdit::Stage).unwrap();
    let staged = index_hunks(path, file, IndexEdit::Unstage).unwrap();
    assert_eq!(staged.len(), 1);
    edit_hunk(path, file, &staged[0], IndexEdit::Unstage).unwrap();
    assert_eq!(status(path).unwrap().staged_count(), 0);
    edit_hunk(path, file, &hunks[0], IndexEdit::Stage).unwrap();
    record(path, "one hunk", false);
    let committed = git(path, ["show", "HEAD:source[1].txt"]).unwrap();
    assert!(committed.contains("first edit"));
    assert!(!committed.contains("second edit"));
    assert_eq!(fs::read_to_string(path.join(file)).unwrap(), updated);
    let change = status(path).unwrap().files.remove(0);
    edit_file(path, &change, IndexEdit::Stage).unwrap();
    record(path, "", true);
    assert_eq!(
        git(path, ["log", "-1", "--format=%s"]).unwrap().trim(),
        "one hunk"
    );
    assert_eq!(git(path, ["show", "HEAD:source[1].txt"]).unwrap(), updated);
}

#[test]
fn squash_keeps_index_worktree_tree_and_first_author() {
    let temp = repo();
    let path = temp.path();
    for (author, message) in [("First author", "one"), ("Second author", "two")] {
        git(path, ["config", "user.name", author]).unwrap();
        fs::write(path.join("tracked"), message).unwrap();
        edit_file(
            path,
            &status(path).unwrap().files.remove(0),
            IndexEdit::Stage,
        )
        .unwrap();
        record(path, message, false);
    }
    fs::write(path.join("tracked"), "staged").unwrap();
    edit_file(
        path,
        &status(path).unwrap().files.remove(0),
        IndexEdit::Stage,
    )
    .unwrap();
    fs::write(path.join("tracked"), "unstaged").unwrap();
    let state = status(path).unwrap();
    let head = state.head_oid.as_deref().unwrap();
    assert!(squash(path, "main", "deadbeef", 2, "combined").is_err());
    squash(path, "main", head, 2, "combined").unwrap();
    assert_eq!(
        git(path, ["rev-list", "--count", "HEAD"]).unwrap().trim(),
        "1"
    );
    assert_eq!(
        git(path, ["log", "-1", "--format=%an"]).unwrap().trim(),
        "First author"
    );
    assert_eq!(git(path, ["show", "HEAD:tracked"]).unwrap(), "two");
    assert_eq!(git(path, ["show", ":tracked"]).unwrap(), "staged");
    assert_eq!(
        fs::read_to_string(path.join("tracked")).unwrap(),
        "unstaged"
    );
}

#[test]
fn fetch_refreshes_tracking_and_pull_only_fast_forwards() {
    let origin = repo();
    let worktree = repo();
    let remote = origin.path().to_str().unwrap();
    git(origin.path(), ["commit", "--allow-empty", "-m", "base"]).unwrap();
    git(worktree.path(), ["remote", "add", "origin", remote]).unwrap();
    fetch(worktree.path()).unwrap();
    git(
        worktree.path(),
        ["switch", "-C", "main", "--track", "origin/main"],
    )
    .unwrap();
    git(origin.path(), ["commit", "--allow-empty", "-m", "forward"]).unwrap();
    fetch(worktree.path()).unwrap();
    assert_eq!(status(worktree.path()).unwrap().behind, 1);
    pull(worktree.path(), "main").unwrap();
    assert_eq!(status(worktree.path()).unwrap().behind, 0);
    git(worktree.path(), ["commit", "--allow-empty", "-m", "local"]).unwrap();
    git(origin.path(), ["commit", "--allow-empty", "-m", "diverge"]).unwrap();
    let head = status(worktree.path()).unwrap().head_oid;
    assert!(pull(worktree.path(), "main").is_err());
    assert_eq!(status(worktree.path()).unwrap().head_oid, head);
}
