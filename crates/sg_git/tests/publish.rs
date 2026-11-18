#![expect(clippy::unwrap_used, reason = "test fixture setup should fail fast")]

use std::path::Path;

use sg_git::{branch_publication, git, publication_commits, publish_branch};
use tempfile::TempDir;

fn fixture() -> (TempDir, TempDir) {
    let local = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();
    git(remote.path(), ["init", "-qb", "main", "--bare"]).unwrap();
    git(local.path(), ["init", "-qb", "main"]).unwrap();
    git(local.path(), ["config", "user.name", "Fixture"]).unwrap();
    git(
        local.path(),
        ["config", "user.email", "fixture@example.invalid"],
    )
    .unwrap();
    git(local.path(), ["config", "commit.gpgsign", "false"]).unwrap();
    git(local.path(), ["commit", "--allow-empty", "-qm", "base"]).unwrap();
    git(
        local.path(),
        ["remote", "add", "origin", remote.path().to_str().unwrap()],
    )
    .unwrap();
    git(local.path(), ["push", "-qu", "origin", "main"]).unwrap();
    git(local.path(), ["switch", "-qc", "feature/demo"]).unwrap();
    git(
        local.path(),
        ["commit", "--allow-empty", "-qm", "publish me"],
    )
    .unwrap();
    (local, remote)
}

fn object(path: &Path, rev: &str) -> String {
    git(path, ["rev-parse", "--verify", rev])
        .unwrap()
        .trim_end()
        .to_string()
}

#[test]
fn previews_and_pushes_only_the_confirmed_commit() {
    let (local, remote) = fixture();
    let path = local.path();
    let base = object(remote.path(), "refs/heads/main");
    std::fs::write(path.join("local-only"), "dirty worktree").unwrap();
    git(path, ["config", "push.followTags", "true"]).unwrap();
    git(
        path,
        [
            "-c",
            "tag.gpgsign=false",
            "tag",
            "-a",
            "v-fixture",
            "-m",
            "not part of the branch publish",
        ],
    )
    .unwrap();
    let preview = branch_publication(path).unwrap();
    let (count, subjects) = publication_commits(path, &preview, &base).unwrap().unwrap();
    assert_eq!(count, 1);
    assert!(subjects[0].ends_with("publish me"));
    publish_branch(path, &preview).unwrap();
    publish_branch(path, &preview).unwrap();
    assert_eq!(
        object(remote.path(), "refs/heads/feature/demo"),
        preview.head
    );
    assert_eq!(object(remote.path(), "refs/heads/main"), base);
    assert!(
        git(
            remote.path(),
            ["rev-parse", "--verify", "refs/tags/v-fixture"]
        )
        .is_err()
    );
    assert!(path.join("local-only").is_file());
    assert_eq!(
        publication_commits(path, &preview, "1111111111111111111111111111111111111111").unwrap(),
        None
    );
}

#[test]
fn rejects_moved_tips_and_destinations_before_pushing() {
    let (local, remote) = fixture();
    let path = local.path();
    let first = branch_publication(path).unwrap();
    git(path, ["commit", "--allow-empty", "-qm", "new work"]).unwrap();
    assert!(publish_branch(path, &first).is_err());
    assert!(
        git(
            remote.path(),
            ["rev-parse", "--verify", "refs/heads/feature/demo"]
        )
        .is_err()
    );
    let second = branch_publication(path).unwrap();
    git(
        path,
        ["config", "remote.origin.pushurl", "/tmp/another-repo"],
    )
    .unwrap();
    assert!(publish_branch(path, &second).is_err());
}

#[test]
fn respects_the_selected_push_remote_and_the_tracking_branch() {
    let (local, remote) = fixture();
    let path = local.path();
    git(path, ["config", "branch.feature/demo.remote", "origin"]).unwrap();
    git(
        path,
        [
            "config",
            "branch.feature/demo.merge",
            "refs/heads/remote-demo",
        ],
    )
    .unwrap();
    let tracking = branch_publication(path).unwrap();
    assert_eq!(tracking.remote_branch, "remote-demo");
    git(
        path,
        ["remote", "add", "publish", remote.path().to_str().unwrap()],
    )
    .unwrap();
    git(
        path,
        ["config", "branch.feature/demo.pushRemote", "publish"],
    )
    .unwrap();
    let explicit = branch_publication(path).unwrap();
    assert_eq!(explicit.remote, "publish");
    assert_eq!(explicit.remote_branch, "feature/demo");
}
