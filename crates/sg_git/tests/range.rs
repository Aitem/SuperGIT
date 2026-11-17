#![expect(
    clippy::unwrap_used,
    reason = "test helpers fail the test by panicking"
)]

use sg_git::{pr_merge_base, range_patch};
use std::path::Path;
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

fn repository() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    git(root, &["init", "-q", "-b", "trunk"]);
    for (key, value) in [
        ("user.name", "Tester"),
        ("user.email", "test@example.com"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        git(root, &["config", key, value]);
    }
    commit(root, "shared.txt", "one\n", "initial");
    temp
}

#[test]
fn a_pull_request_diff_is_its_head_against_the_merge_base() {
    let temp = repository();
    let root = temp.path();
    let fork = git(root, &["rev-parse", "HEAD"]);
    git(root, &["switch", "-q", "-c", "feature"]);
    let head = commit(root, "feature.txt", "новое\n", "feature work");
    git(root, &["switch", "-q", "trunk"]);
    // The base moved on after the branch forked: not part of the PR.
    let moved = commit(root, "shared.txt", "one\ntwo\n", "trunk work");

    let from = pr_merge_base(root, &moved, "trunk", &head).unwrap();
    assert_eq!(from, fork);
    let patch = range_patch(root, &from, &head).unwrap();
    assert!(patch.contains("+++ b/feature.txt"));
    assert!(patch.contains("+новое"));
    assert!(!patch.contains("shared.txt"), "{patch}");

    // Without GitHub's base commit: `origin/<base>`, then the local branch.
    assert_eq!(pr_merge_base(root, "", "trunk", &head).unwrap(), fork);
    // Once merged, the base contains the head: only GitHub's base commit
    // (the base when it merged) still gives the PR's changes.
    git(root, &["merge", "-q", "--no-ff", "-m", "merge", "feature"]);
    git(root, &["update-ref", "refs/remotes/origin/trunk", "HEAD"]);
    assert_eq!(pr_merge_base(root, "", "trunk", &head).unwrap(), head);
    assert_eq!(pr_merge_base(root, &moved, "trunk", &head).unwrap(), fork);
}

#[test]
fn missing_heads_and_bases_are_explained() {
    let temp = repository();
    let root = temp.path();
    let error = pr_merge_base(
        root,
        "",
        "trunk",
        "0123456789abcdef0123456789abcdef01234567",
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("not in the local repository"), "{error}");
    assert!(pr_merge_base(root, "", "trunk", "").is_err());
    let head = git(root, &["rev-parse", "HEAD"]);
    assert!(pr_merge_base(root, "", "nope", &head).is_err());
    // A base commit GitHub knows but this clone lacks falls back to the branch.
    assert_eq!(
        pr_merge_base(
            root,
            "0123456789abcdef0123456789abcdef01234567",
            "trunk",
            &head
        )
        .unwrap(),
        head
    );
}
