use sg_git::{BaseInfo, load_base, update_base};
use std::path::{Path, PathBuf};
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
fn configure(path: &Path) {
    for (key, value) in [
        ("user.name", "Tester"),
        ("user.email", "test@example.com"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        git(path, &["config", key, value]);
    }
}
struct Fixture {
    temp: tempfile::TempDir,
    root: PathBuf,
    remote: PathBuf,
    publisher: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let remote = temp.path().join("remote.git");
        let publisher = temp.path().join("publisher");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "trunk"]);
        configure(&root);
        std::fs::write(root.join("file.txt"), "initial\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "initial"]);
        git(
            temp.path(),
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        git(
            &root,
            &["remote", "add", "server", remote.to_str().unwrap()],
        );
        git(&root, &["push", "-qu", "server", "trunk"]);
        git(
            temp.path(),
            &[
                "clone",
                "-q",
                "--branch",
                "trunk",
                remote.to_str().unwrap(),
                publisher.to_str().unwrap(),
            ],
        );
        configure(&publisher);
        Self {
            temp,
            root,
            remote,
            publisher,
        }
    }
    fn info(&self) -> BaseInfo {
        load_base(&self.root, Some("trunk")).unwrap()
    }
    fn publish(&self) -> String {
        std::fs::write(self.publisher.join("file.txt"), "upstream\n").unwrap();
        git(&self.publisher, &["commit", "-qam", "upstream"]);
        git(&self.publisher, &["push", "-q", "origin", "trunk"]);
        git(&self.publisher, &["rev-parse", "HEAD"])
    }
    fn tip(&self) -> String {
        git(&self.root, &["rev-parse", "trunk"])
    }
}

#[test]
fn updates_non_main_base_from_its_configured_remote_and_is_idempotent() {
    let f = Fixture::new();
    let info = load_base(&f.root, None).unwrap();
    assert_eq!(info.branch.as_deref(), Some("trunk"));
    assert_eq!(info.upstream.as_ref().unwrap().remote, "server");
    let target = f.publish();
    update_base(&info).unwrap();
    assert_eq!(f.tip(), target);
    assert_eq!(
        std::fs::read_to_string(f.root.join("file.txt")).unwrap(),
        "upstream\n"
    );
    let current = f.info();
    assert_eq!(current.ahead_behind, Some((0, 0)));
    update_base(&current).unwrap();
}
#[test]
fn an_unchecked_base_updates_without_switching_the_active_task() {
    let f = Fixture::new();
    git(&f.root, &["switch", "-qc", "task"]);
    let task = git(&f.root, &["rev-parse", "HEAD"]);
    let target = f.publish();
    update_base(&f.info()).unwrap();
    assert_eq!(f.tip(), target);
    assert_eq!(git(&f.root, &["branch", "--show-current"]), "task");
    assert_eq!(git(&f.root, &["rev-parse", "HEAD"]), task);
    assert_eq!(
        std::fs::read_to_string(f.root.join("file.txt")).unwrap(),
        "initial\n"
    );
}
#[test]
fn dirty_and_locked_linked_base_worktrees_are_preserved() {
    let f = Fixture::new();
    git(&f.root, &["switch", "-qc", "task"]);
    let base = f.temp.path().join("base");
    sg_git::add_worktree(&f.root, &base, None, Some("trunk")).unwrap();
    let tip = f.tip();
    f.publish();
    std::fs::write(base.join("draft.txt"), "preserve").unwrap();
    assert!(update_base(&f.info()).is_err());
    assert_eq!(f.tip(), tip);
    std::fs::remove_file(base.join("draft.txt")).unwrap();
    git(&f.root, &["worktree", "lock", base.to_str().unwrap()]);
    assert!(update_base(&f.info()).is_err());
    assert_eq!(f.tip(), tip);
    git(&f.root, &["worktree", "unlock", base.to_str().unwrap()]);
    update_base(&f.info()).unwrap();
    assert_eq!(
        std::fs::read_to_string(base.join("file.txt")).unwrap(),
        "upstream\n"
    );
}
#[test]
fn divergence_never_merges_rebases_resets_or_stashes() {
    let f = Fixture::new();
    f.publish();
    std::fs::write(f.root.join("local.txt"), "local").unwrap();
    git(&f.root, &["add", "."]);
    git(&f.root, &["commit", "-qm", "local"]);
    let tip = f.tip();
    let error = update_base(&f.info()).unwrap_err().to_string();
    assert!(error.contains("cannot fast-forward"));
    assert_eq!(f.tip(), tip);
    assert_eq!(git(&f.root, &["stash", "list"]), "");
    assert!(f.root.join("local.txt").exists());
}
#[test]
fn changed_branch_or_upstream_invalidates_the_frozen_request() {
    let f = Fixture::new();
    let info = f.info();
    git(&f.root, &["commit", "--allow-empty", "-qm", "concurrent"]);
    let tip = f.tip();
    assert!(
        update_base(&info)
            .unwrap_err()
            .to_string()
            .contains("changed")
    );
    assert_eq!(f.tip(), tip);
    let info = f.info();
    git(&f.root, &["remote", "set-url", "server", "missing.git"]);
    assert!(update_base(&info).is_err());
    assert_eq!(f.tip(), tip);
}
#[test]
fn offline_fetch_and_missing_upstream_keep_local_work() {
    let f = Fixture::new();
    let tip = f.tip();
    std::fs::rename(&f.remote, f.temp.path().join("offline.git")).unwrap();
    assert!(update_base(&f.info()).is_err());
    assert_eq!(f.tip(), tip);
    git(&f.root, &["branch", "--unset-upstream", "trunk"]);
    assert!(
        update_base(&f.info())
            .unwrap_err()
            .to_string()
            .contains("no upstream")
    );
}
#[test]
fn creation_uses_the_explicit_base_and_never_fetches() {
    let f = Fixture::new();
    let base = f.info();
    git(&f.root, &["switch", "-qc", "task"]);
    git(&f.root, &["commit", "--allow-empty", "-qm", "task"]);
    std::fs::rename(&f.remote, f.temp.path().join("offline.git")).unwrap();
    let target = f.temp.path().join("new-task");
    sg_git::add_worktree(
        &f.root,
        &target,
        Some("new-task"),
        Some(&base.creation_ref().unwrap()),
    )
    .unwrap();
    assert_eq!(git(&target, &["rev-parse", "HEAD"]), base.tip.unwrap());
    assert_eq!(git(&f.root, &["branch", "--show-current"]), "task");
}
#[test]
fn missing_unborn_and_detached_bases_are_explicit() {
    let f = Fixture::new();
    let missing = load_base(&f.root, Some("missing")).unwrap();
    assert!(missing.creation_ref().is_err());
    assert!(update_base(&missing).is_err());
    git(&f.root, &["switch", "--detach", "-q"]);
    assert_eq!(
        load_base(&f.root, None).unwrap().branch.as_deref(),
        Some("trunk")
    );
    let temp = tempfile::tempdir().unwrap();
    git(temp.path(), &["init", "-q", "-b", "unborn"]);
    let unborn = load_base(temp.path(), None).unwrap();
    assert!(unborn.creation_ref().is_err());
}

#[test]
fn remote_default_wins_over_the_primary_task_and_a_local_main_branch() {
    let f = Fixture::new();
    git(
        &f.root,
        &["remote", "add", "origin", f.remote.to_str().unwrap()],
    );
    git(
        &f.root,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/server/trunk",
        ],
    );
    // A conventional origin HEAD points inside origin's namespace.
    git(
        &f.root,
        &["update-ref", "refs/remotes/origin/trunk", &f.tip()],
    );
    git(
        &f.root,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk",
        ],
    );
    git(&f.root, &["branch", "main"]);
    git(&f.root, &["switch", "-qc", "task"]);
    assert_eq!(
        load_base(&f.root, None).unwrap().branch.as_deref(),
        Some("trunk")
    );
}

#[test]
fn checkout_changes_during_fetch_stop_before_the_local_update() {
    use std::os::unix::fs::PermissionsExt as _;
    let f = Fixture::new();
    git(&f.root, &["branch", "task"]);
    let info = f.info();
    let tip = f.tip();
    f.publish();
    let hooks = f.temp.path().join("hooks");
    std::fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("reference-transaction");
    std::fs::write(&hook,"#!/bin/sh\nif [ \"$1\" = committed ]; then\nwhile read old new ref; do\nif [ \"$ref\" = refs/remotes/server/trunk ]; then git switch -q task; fi\ndone\nfi\nexit 0\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &f.root,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    assert!(
        update_base(&info)
            .unwrap_err()
            .to_string()
            .contains("changed")
    );
    assert_eq!(f.tip(), tip);
    assert_eq!(git(&f.root, &["branch", "--show-current"]), "task");
    assert_eq!(
        std::fs::read_to_string(f.root.join("file.txt")).unwrap(),
        "initial\n"
    );
}

#[test]
fn a_renamed_remote_keeps_its_non_main_default() {
    let f = Fixture::new();
    git(
        &f.root,
        &[
            "symbolic-ref",
            "refs/remotes/server/HEAD",
            "refs/remotes/server/trunk",
        ],
    );
    git(&f.root, &["branch", "main"]);
    git(&f.root, &["switch", "-qc", "task"]);
    assert_eq!(
        load_base(&f.root, None).unwrap().branch.as_deref(),
        Some("trunk")
    );
}
