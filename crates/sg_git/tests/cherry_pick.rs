#![expect(clippy::unwrap_used, reason = "real Git fixture")]

use std::fs;
use std::path::Path;

use sg_git::{
    CherryPickAction as Action, CherryPickState, git, inspect_cherry_pick, run_cherry_pick,
};

fn run(path: &Path, args: &[&str]) -> String {
    git(path, args).unwrap().trim().to_owned()
}

fn setup(conflict: bool) -> (tempfile::TempDir, String, String) {
    let repo = tempfile::tempdir().unwrap();
    let path = repo.path();
    run(path, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.name", "Test"),
        ("user.email", "test@example.test"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        run(path, &["config", key, value]);
    }
    fs::write(path.join("file.txt"), "base\n").unwrap();
    run(path, &["add", "file.txt"]);
    run(path, &["commit", "-qm", "base"]);
    run(path, &["switch", "-qc", "source"]);
    fs::write(path.join("file.txt"), "source\n").unwrap();
    run(path, &["commit", "-qam", "Pick this fix"]);
    let source = run(path, &["rev-parse", "HEAD"]);
    run(path, &["switch", "-q", "main"]);
    if conflict {
        fs::write(path.join("file.txt"), "destination\n").unwrap();
        run(path, &["commit", "-qam", "Conflicting local change"]);
    } else {
        run(
            path,
            &["commit", "--allow-empty", "-qm", "Destination commit"],
        );
    }
    let target = run(path, &["rev-parse", "HEAD"]);
    (repo, source, target)
}

#[test]
fn applies_the_previewed_commit_and_rejects_moved_or_dirty_destinations() {
    let (repo, source, target) = setup(false);
    let path = repo.path();
    fs::write(path.join("untracked.txt"), "keep it").unwrap();
    let plan = inspect_cherry_pick(path, Some(&source)).unwrap().unwrap();
    fs::write(path.join("file.txt"), "dirty\n").unwrap();
    assert!(run_cherry_pick(path, &plan, Action::Start).is_err());
    assert_eq!(run(path, &["rev-parse", "HEAD"]), target);
    run(path, &["restore", "file.txt"]);
    run(
        path,
        &["commit", "--allow-empty", "-qm", "Destination moved"],
    );
    assert!(run_cherry_pick(path, &plan, Action::Start).is_err());
    let plan = inspect_cherry_pick(path, Some(&source)).unwrap().unwrap();
    assert_eq!(plan.state, CherryPickState::Preview);
    run_cherry_pick(path, &plan, Action::Start).unwrap();
    assert_eq!(run(path, &["log", "-1", "--format=%s"]), "Pick this fix");
    assert_eq!(
        fs::read_to_string(path.join("file.txt")).unwrap(),
        "source\n"
    );
    assert_eq!(
        fs::read_to_string(path.join("untracked.txt")).unwrap(),
        "keep it"
    );
    assert!(inspect_cherry_pick(path, None).unwrap().is_none());
}

#[test]
fn reloads_a_conflicted_pick_and_continues_after_staging_resolution() {
    let (repo, source, target) = setup(true);
    let path = repo.path();
    let plan = inspect_cherry_pick(path, Some(&source)).unwrap().unwrap();
    assert!(run_cherry_pick(path, &plan, Action::Start).is_err());
    assert_eq!(run(path, &["rev-parse", "HEAD"]), target);
    let paused = inspect_cherry_pick(path, None).unwrap().unwrap();
    assert_eq!(paused.source.sha, source);
    assert_eq!(paused.state, CherryPickState::Paused { conflicts: 1 });
    assert!(run_cherry_pick(path, &paused, Action::Continue).is_err());
    fs::write(path.join("file.txt"), "combined resolution\n").unwrap();
    run(path, &["add", "file.txt"]);
    run_cherry_pick(path, &paused, Action::Continue).unwrap();
    assert!(inspect_cherry_pick(path, None).unwrap().is_none());
    assert_eq!(run(path, &["show", "HEAD:file.txt"]), "combined resolution");
    assert_eq!(run(path, &["rev-parse", "HEAD^"]), target);
}

#[test]
fn abort_restores_destination_and_leaves_unrelated_untracked_file() {
    let (repo, source, target) = setup(true);
    let path = repo.path();
    let plan = inspect_cherry_pick(path, Some(&source)).unwrap().unwrap();
    assert!(run_cherry_pick(path, &plan, Action::Start).is_err());
    let paused = inspect_cherry_pick(path, None).unwrap().unwrap();
    fs::write(path.join("unrelated.txt"), "keep me").unwrap();
    run_cherry_pick(path, &paused, Action::Abort).unwrap();
    assert_eq!(run(path, &["rev-parse", "HEAD"]), target);
    assert_eq!(
        fs::read_to_string(path.join("file.txt")).unwrap(),
        "destination\n"
    );
    assert_eq!(
        fs::read_to_string(path.join("unrelated.txt")).unwrap(),
        "keep me"
    );
    assert!(inspect_cherry_pick(path, None).unwrap().is_none());
}
