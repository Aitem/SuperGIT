#![expect(clippy::unwrap_used, reason = "test setup may panic")]

use std::path::Path;

use sg_git::{FileState, Staging, change_staging, git, status};

fn repository() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    git(root, ["init", "-q", "-b", "main"]).unwrap();
    for (key, value) in [
        ("user.name", "Test"),
        ("user.email", "fixture@example.test"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        git(root, ["config", key, value]).unwrap();
    }
    temp
}

fn file_action(worktree: &Path, path: &str, action: Staging) -> anyhow::Result<()> {
    let snapshot = status(worktree)?;
    let file = snapshot
        .files
        .iter()
        .find(|file| file.path == path)
        .unwrap();
    change_staging(worktree, &snapshot, file, action)
}

#[test]
fn stages_literal_names_and_unstages_on_an_unborn_branch() {
    let temp = repository();
    let root = temp.path();
    for path in ["[a].txt", "a.txt", "--help"] {
        std::fs::write(root.join(path), "content\n").unwrap();
    }
    file_action(root, "[a].txt", Staging::Stage).unwrap();
    assert_eq!(
        git(root, ["diff", "--cached", "--name-only"])
            .unwrap()
            .trim(),
        "[a].txt"
    );
    file_action(root, "--help", Staging::Stage).unwrap();
    file_action(root, "[a].txt", Staging::Unstage).unwrap();
    assert_eq!(
        git(root, ["diff", "--cached", "--name-only"])
            .unwrap()
            .trim(),
        "--help"
    );
    assert!(root.join("[a].txt").exists());
    assert!(status(root).unwrap().is_unborn());
}

#[test]
fn changes_only_the_linked_worktree_index_and_keeps_working_edits() {
    let temp = repository();
    let root = temp.path();
    std::fs::write(root.join("pick.txt"), "base\n").unwrap();
    git(root, ["add", "pick.txt"]).unwrap();
    git(root, ["commit", "-qm", "base"]).unwrap();
    let linked = tempfile::tempdir().unwrap();
    let worktree = linked.path().join("worktree");
    sg_git::add_worktree(root, &worktree, Some("feature"), None).unwrap();
    std::fs::write(worktree.join("pick.txt"), "staged version\n").unwrap();
    file_action(&worktree, "pick.txt", Staging::Stage).unwrap();
    assert!(status(root).unwrap().is_clean());
    std::fs::write(worktree.join("pick.txt"), "working version\n").unwrap();
    let mixed = &status(&worktree).unwrap().files[0];
    assert_eq!(mixed.staged, Some(FileState::Modified));
    assert_eq!(mixed.unstaged, Some(FileState::Modified));
    file_action(&worktree, "pick.txt", Staging::Unstage).unwrap();
    assert_eq!(git(&worktree, ["show", ":pick.txt"]).unwrap(), "base\n");
    assert_eq!(
        std::fs::read_to_string(worktree.join("pick.txt")).unwrap(),
        "working version\n"
    );
    assert!(status(root).unwrap().is_clean());
}

#[test]
fn unstages_both_sides_of_a_rename_without_touching_working_files() {
    let temp = repository();
    let root = temp.path();
    std::fs::write(root.join("old.txt"), "content\n").unwrap();
    git(root, ["add", "old.txt"]).unwrap();
    git(root, ["commit", "-qm", "base"]).unwrap();
    git(root, ["mv", "old.txt", "new.txt"]).unwrap();
    std::fs::write(root.join("new.txt"), "working edit\n").unwrap();
    assert_eq!(
        status(root).unwrap().files[0].staged,
        Some(FileState::Renamed)
    );
    file_action(root, "new.txt", Staging::Unstage).unwrap();
    assert!(
        git(root, ["diff", "--cached", "--name-only"])
            .unwrap()
            .is_empty()
    );
    assert!(!root.join("old.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("new.txt")).unwrap(),
        "working edit\n"
    );
}

#[test]
fn rejects_stale_selection_and_directory_overlap() {
    let temp = repository();
    let root = temp.path();
    std::fs::write(root.join("pick"), "base\n").unwrap();
    git(root, ["add", "pick"]).unwrap();
    git(root, ["commit", "-qm", "base"]).unwrap();
    std::fs::write(root.join("pick"), "changed\n").unwrap();
    let old = status(root).unwrap();
    git(root, ["switch", "-qc", "other"]).unwrap();
    assert!(change_staging(root, &old, &old.files[0], Staging::Stage).is_err());
    std::fs::remove_file(root.join("pick")).unwrap();
    std::fs::create_dir(root.join("pick")).unwrap();
    std::fs::write(root.join("pick/other"), "unrelated\n").unwrap();
    assert!(file_action(root, "pick", Staging::Stage).is_err());
    assert!(
        git(root, ["diff", "--cached", "--name-only"])
            .unwrap()
            .is_empty()
    );
}
