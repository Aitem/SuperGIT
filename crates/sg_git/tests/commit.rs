#![expect(clippy::unwrap_used, reason = "test setup may panic")]

use std::path::Path;

use sg_git::{commit_staged, git, preview_commit};

fn repository() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    git(root, ["init", "-q", "-b", "main"]).unwrap();
    for (key, value) in [
        ("user.name", "Tester"),
        ("user.email", "fixture@example.test"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        git(root, ["config", key, value]).unwrap();
    }
    temp
}

fn stage(root: &Path, name: &str, contents: &str) {
    std::fs::write(root.join(name), contents).unwrap();
    git(root, ["add", "--", name]).unwrap();
}

#[test]
fn first_commit_records_staged_content_and_keeps_working_edits() {
    let temp = repository();
    let root = temp.path();
    stage(root, "selected.txt", "staged\n");
    std::fs::write(root.join("selected.txt"), "uncommitted edit\n").unwrap();
    std::fs::write(root.join("untracked.txt"), "unrelated\n").unwrap();
    let preview = preview_commit(root).unwrap();
    assert_eq!(preview.branch(), Some("main"));
    assert_eq!(preview.head(), None);
    assert_eq!(preview.files, 1);
    let patch = preview.patch.as_deref().unwrap();
    assert!(patch.contains("+staged"));
    assert!(!patch.contains("uncommitted edit") && !patch.contains("untracked.txt"));
    commit_staged(root, &preview, "Initial change\n\nBody with café").unwrap();
    assert_eq!(
        git(root, ["show", "HEAD:selected.txt"]).unwrap(),
        "staged\n"
    );
    assert!(git(root, ["show", "HEAD:untracked.txt"]).is_err());
    assert!(
        git(root, ["log", "-1", "--format=%B"])
            .unwrap()
            .contains("Body with café")
    );
    assert!(
        git(root, ["diff", "--", "selected.txt"])
            .unwrap()
            .contains("+uncommitted edit")
    );
}

#[test]
fn moved_index_or_branch_requires_a_fresh_preview() {
    let temp = repository();
    let root = temp.path();
    git(root, ["commit", "--allow-empty", "-qm", "base"]).unwrap();
    stage(root, "selected.txt", "first\n");
    let preview = preview_commit(root).unwrap();
    stage(root, "selected.txt", "later\n");
    assert!(
        commit_staged(root, &preview, "Wrong contents")
            .unwrap_err()
            .to_string()
            .contains("Reload the preview")
    );
    let preview = preview_commit(root).unwrap();
    git(root, ["switch", "-qc", "other"]).unwrap();
    assert!(commit_staged(root, &preview, "Wrong branch").is_err());
    assert_eq!(
        git(root, ["log", "-1", "--format=%s"]).unwrap().trim(),
        "base"
    );
    assert_eq!(git(root, ["show", ":selected.txt"]).unwrap(), "later\n");
}

#[cfg(unix)]
#[test]
fn reports_a_rejecting_hook_without_dropping_the_staged_file() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = repository();
    let root = temp.path();
    let hooks = root.join(".git/fixture-hooks");
    std::fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'fixture pre-commit rejected' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(root, ["config", "core.hooksPath", ".git/fixture-hooks"]).unwrap();
    stage(root, "selected.txt", "staged\n");
    let preview = preview_commit(root).unwrap();
    assert!(
        commit_staged(root, &preview, "Attempt")
            .unwrap_err()
            .to_string()
            .contains("fixture pre-commit rejected")
    );
    assert!(git(root, ["rev-parse", "--verify", "HEAD"]).is_err());
    assert_eq!(git(root, ["show", ":selected.txt"]).unwrap(), "staged\n");
}
