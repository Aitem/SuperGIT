#![expect(clippy::unwrap_used, reason = "test setup may panic")]

use std::path::Path;

use sg_git::{ChangeScope, FileState, Staging, change_staging, git, scoped_worktree_patch, status};

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
    std::fs::write(worktree.join("new.txt"), "untracked contents\n").unwrap();
    let mixed_status = status(&worktree).unwrap();
    let mixed = mixed_status
        .files
        .iter()
        .find(|file| file.path == "pick.txt")
        .unwrap();
    let staged = scoped_worktree_patch(&worktree, &mixed_status, ChangeScope::Staged).unwrap();
    assert!(staged.text.contains("+staged version"));
    assert!(!staged.text.contains("working version") && !staged.text.contains("new.txt"));
    let unstaged = scoped_worktree_patch(&worktree, &mixed_status, ChangeScope::Unstaged).unwrap();
    assert!(
        unstaged.text.contains("-staged version") && unstaged.text.contains("+working version")
    );
    assert!(unstaged.text.contains("+untracked contents"));
    let staged_pair = sg_git::difftastic::read_pairs(
        &worktree,
        &sg_git::difftastic::Source::Revision("HEAD".into()),
        &sg_git::difftastic::Source::Index,
        &[sg_git::difftastic::FileRequest {
            path: "pick.txt".into(),
            old_path: None,
            added: false,
            deleted: false,
            submodule: false,
        }],
        &[],
    )
    .unwrap();
    assert!(
        matches!(&staged_pair[0].new, sg_git::difftastic::Content::Bytes(bytes) if bytes == b"staged version\n")
    );
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

#[test]
fn stages_and_unstages_one_hunk_without_touching_other_hunks_or_files() {
    let temp = repository();
    let root = temp.path();
    let base: String = (1..=60).map(|line| format!("Line {line}\n")).collect();
    let file = root.join("two [hunks].txt");
    std::fs::write(&file, &base).unwrap();
    git(root, ["add", "."]).unwrap();
    git(root, ["commit", "-qm", "baseline"]).unwrap();

    let working = base
        .replace("Line 5\n", "First edit\n")
        .replace("Line 55\n", "Second edit\n");
    std::fs::write(&file, &working).unwrap();
    let snapshot = status(root).unwrap();
    let change = &snapshot.files[0];
    let patch = scoped_worktree_patch(root, &snapshot, ChangeScope::Unstaged).unwrap();
    sg_git::change_staging_hunk(
        root,
        &snapshot,
        change,
        Staging::Stage,
        &patch.text,
        (false, 55),
    )
    .unwrap();

    let index = git(root, ["show", ":two [hunks].txt"]).unwrap();
    assert!(index.contains("Line 5\n") && !index.contains("First edit"));
    assert!(index.contains("Second edit"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), working);

    let staged = status(root).unwrap();
    let patch = scoped_worktree_patch(root, &staged, ChangeScope::Staged).unwrap();
    sg_git::change_staging_hunk(
        root,
        &staged,
        &staged.files[0],
        Staging::Unstage,
        &patch.text,
        (true, 55),
    )
    .unwrap();
    assert_eq!(git(root, ["show", ":two [hunks].txt"]).unwrap(), base);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), working);
}

#[test]
fn hunk_action_rejects_a_stale_diff_even_when_status_is_unchanged() {
    let temp = repository();
    let root = temp.path();
    std::fs::write(root.join("chosen.txt"), "preceding\nbase\nfollowing\n").unwrap();
    git(root, ["add", "."]).unwrap();
    git(root, ["commit", "-qm", "baseline"]).unwrap();
    std::fs::write(root.join("chosen.txt"), "preceding\nreviewed\nfollowing\n").unwrap();

    let snapshot = status(root).unwrap();
    let patch = scoped_worktree_patch(root, &snapshot, ChangeScope::Unstaged).unwrap();
    std::fs::write(root.join("chosen.txt"), "preceding\nnewer\nfollowing\n").unwrap();
    assert_eq!(status(root).unwrap(), snapshot);
    assert!(
        sg_git::change_staging_hunk(
            root,
            &snapshot,
            &snapshot.files[0],
            Staging::Stage,
            &patch.text,
            (false, 2)
        )
        .is_err()
    );
    assert!(
        git(root, ["diff", "--cached", "--name-only"])
            .unwrap()
            .is_empty()
    );
    assert!(
        std::fs::read_to_string(root.join("chosen.txt"))
            .unwrap()
            .contains("newer")
    );
}
