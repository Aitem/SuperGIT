#![expect(
    clippy::unwrap_used,
    reason = "test helpers fail the test by panicking"
)]

//! Submodules in diffs: their own changes inlined after their entries.

use std::path::{Path, PathBuf};
use std::process::Command;

use sg_git::difftastic::{self, Content, FileRequest, Source};
use sg_git::{FileState, Inline, SubmoduleChange};

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=Tester",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.file.allow=always",
        ])
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

/// `app` with the submodule `vendor/lib` (one file, `a.txt`).
struct Fixture {
    _temp: tempfile::TempDir,
    lib: PathBuf,
    app: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let lib = temp.path().join("lib");
        let app = temp.path().join("app");
        for dir in [&lib, &app] {
            std::fs::create_dir(dir).unwrap();
            git(dir, &["init", "-q", "-b", "main"]);
        }
        std::fs::write(lib.join("a.txt"), "one\ntwo\n").unwrap();
        git(&lib, &["add", "."]);
        git(&lib, &["commit", "-qm", "lib: first"]);
        std::fs::write(app.join("readme"), "hi\n").unwrap();
        git(&app, &["add", "."]);
        git(&app, &["commit", "-qm", "init"]);
        git(
            &app,
            &[
                "submodule",
                "add",
                "-q",
                lib.to_str().unwrap(),
                "vendor/lib",
            ],
        );
        git(&app, &["commit", "-qm", "add lib"]);
        Self {
            _temp: temp,
            lib,
            app,
        }
    }

    fn checkout(&self) -> PathBuf {
        self.app.join("vendor/lib")
    }

    /// Two commits in the submodule's checkout, not yet recorded in `app`.
    fn advance(&self) {
        let sub = self.checkout();
        std::fs::write(sub.join("a.txt"), "one\nTWO\n").unwrap();
        git(&sub, &["commit", "-qam", "lib: shout"]);
        std::fs::write(sub.join("b.txt"), "new\n").unwrap();
        git(&sub, &["add", "b.txt"]);
        git(&sub, &["commit", "-qm", "lib: add b"]);
    }

    fn worktree_patch(&self) -> sg_git::Patch {
        let status = sg_git::status(&self.app).unwrap();
        sg_git::worktree_patch(&self.app, &status).unwrap()
    }
}

fn only(submodules: &[SubmoduleChange]) -> &SubmoduleChange {
    assert_eq!(submodules.len(), 1, "{submodules:#?}");
    &submodules[0]
}

#[test]
fn status_reports_what_changed_in_a_checkout() {
    let f = Fixture::new();
    f.advance();
    std::fs::write(f.checkout().join("scratch.txt"), "x\n").unwrap();
    let status = sg_git::status(&f.app).unwrap();
    let flags = status.files[0].submodule.unwrap();
    assert!(flags.commit_changed && flags.untracked && !flags.modified);
    assert!(sg_git::status(&f.lib).unwrap().files.is_empty());
}

#[test]
fn working_tree_changes_inline_the_submodule_diff() {
    let f = Fixture::new();
    f.advance();
    std::fs::write(f.checkout().join("a.txt"), "one\nTWO\nthree\n").unwrap();
    std::fs::write(f.checkout().join("scratch.txt"), "draft\n").unwrap();
    let patch = f.worktree_patch();
    let text = &patch.text;
    // The submodule's own entry comes first, then its files under its path.
    let entry = text.find("diff --git a/vendor/lib b/vendor/lib\n").unwrap();
    let inner = text.find("diff --git a/vendor/lib/a.txt").unwrap();
    assert!(entry < inner, "{text}");
    assert!(text.contains("+TWO\n+three\n"), "{text}");
    assert!(text.contains("+++ b/vendor/lib/b.txt\n"), "{text}");
    assert!(
        text.contains("+++ b/vendor/lib/scratch.txt\n@@ -0,0 +1,1 @@\n+draft\n"),
        "{text}"
    );

    let lib = only(&patch.submodules);
    assert_eq!(lib.path, "vendor/lib");
    assert_eq!(lib.inline, Inline::Shown);
    assert!(lib.modified && lib.untracked);
    assert_eq!((lib.added_commits, lib.removed_commits), (2, 0));
    let subjects: Vec<_> = lib.commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["lib: add b", "lib: shout"]);
    let files: Vec<_> = lib
        .files
        .iter()
        .map(|file| (file.path.as_str(), file.state()))
        .collect();
    assert_eq!(
        files,
        [
            ("vendor/lib/a.txt", FileState::Modified),
            ("vendor/lib/b.txt", FileState::Added),
            ("vendor/lib/scratch.txt", FileState::Untracked),
        ]
    );

    // The Changes list: the submodule, then what changed inside it.
    let status = sg_git::status(&f.app).unwrap();
    let list = sg_git::with_submodule_files(status.files, &patch.submodules);
    let paths: Vec<_> = list.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "vendor/lib",
            "vendor/lib/a.txt",
            "vendor/lib/b.txt",
            "vendor/lib/scratch.txt"
        ]
    );
}

#[test]
fn a_checkout_with_only_new_files_still_shows() {
    let f = Fixture::new();
    std::fs::write(f.checkout().join("scratch.txt"), "draft\n").unwrap();
    let patch = f.worktree_patch();
    // `git diff` alone says nothing about untracked files in a submodule.
    assert!(patch.text.contains("+Subproject commit "));
    assert!(patch.text.contains("+++ b/vendor/lib/scratch.txt\n"));
    let lib = only(&patch.submodules);
    assert!(lib.untracked && lib.commits.is_empty());
    assert_eq!(lib.old, lib.new);
}

#[test]
fn commits_and_ranges_inline_between_recorded_commits() {
    let f = Fixture::new();
    f.advance();
    std::fs::write(f.checkout().join("a.txt"), "uncommitted\n").unwrap();
    git(&f.app, &["add", "vendor/lib"]);
    git(&f.app, &["commit", "-qm", "bump lib"]);
    let bump = git(&f.app, &["rev-parse", "HEAD"]);

    let patch = sg_git::commit_patch(&f.app, &bump).unwrap();
    // A commit shows recorded commits only, not the checkout's edits.
    assert!(patch.text.contains("+TWO\n"), "{}", patch.text);
    assert!(!patch.text.contains("uncommitted"), "{}", patch.text);
    let lib = only(&patch.submodules);
    assert!(!lib.modified);
    assert_eq!(lib.added_commits, 2);

    let range = sg_git::range_patch(&f.app, "HEAD~1", "HEAD").unwrap();
    assert_eq!(range.text, patch.text);

    // Going back lists the dropped commits.
    let back = sg_git::range_patch(&f.app, "HEAD", "HEAD~1").unwrap();
    let lib = only(&back.submodules);
    assert_eq!((lib.added_commits, lib.removed_commits), (0, 2));
    assert!(lib.commits.iter().all(|commit| commit.removed));
    assert!(back.text.contains("-TWO\n"), "{}", back.text);

    // Adding the submodule does not repeat its whole contents.
    let added = sg_git::commit_patch(&f.app, "HEAD~1").unwrap();
    let lib = only(&added.submodules);
    assert_eq!(lib.inline, Inline::AddedOrRemoved);
    assert!(lib.old.is_none() && lib.new.is_some());
    assert!(!added.text.contains("vendor/lib/a.txt"));
}

#[test]
fn branch_changes_include_submodule_files() {
    let f = Fixture::new();
    git(&f.app, &["branch", "base"]);
    f.advance();
    git(&f.app, &["commit", "-qam", "bump lib"]);
    std::fs::write(f.checkout().join("c.txt"), "later\n").unwrap();
    let status = sg_git::status(&f.app).unwrap();
    let changes = sg_git::branch_changes(&f.app, &status, "base").unwrap();
    let paths: Vec<_> = changes.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "vendor/lib",
            "vendor/lib/a.txt",
            "vendor/lib/b.txt",
            "vendor/lib/c.txt"
        ]
    );
    assert!(changes.patch.contains("+++ b/vendor/lib/c.txt\n"));
    assert_eq!(only(&changes.submodules).added_commits, 2);
}

#[test]
fn missing_checkouts_and_commits_keep_the_plain_entry() {
    let f = Fixture::new();
    f.advance();
    git(&f.app, &["commit", "-qam", "bump lib"]);
    // A clone without `submodule update --init` has an empty directory.
    let clone = f.app.with_file_name("clone");
    git(
        f.app.parent().unwrap(),
        &[
            "clone",
            "-q",
            f.app.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let patch = sg_git::commit_patch(&clone, "HEAD").unwrap();
    assert_eq!(only(&patch.submodules).inline, Inline::NotCheckedOut);
    assert!(patch.text.contains("+Subproject commit "));
    assert!(!patch.text.contains("vendor/lib/"));

    // The checkout never saw the recorded commit.
    git(
        &f.app,
        &[
            "update-index",
            "--cacheinfo",
            "160000,1111111111111111111111111111111111111111,vendor/lib",
        ],
    );
    git(&f.app, &["commit", "-qm", "point lib at a missing commit"]);
    let patch = sg_git::commit_patch(&f.app, "HEAD").unwrap();
    assert_eq!(only(&patch.submodules).inline, Inline::CommitsMissing);
}

#[test]
fn difftastic_reads_submodule_files_from_their_checkout() {
    let f = Fixture::new();
    f.advance();
    let patch = f.worktree_patch();
    let request = |path: &str, added: bool, submodule: bool| FileRequest {
        path: path.into(),
        old_path: None,
        added,
        deleted: false,
        submodule,
    };
    let files = [
        request("vendor/lib", false, true),
        request("vendor/lib/a.txt", false, false),
        request("vendor/lib/b.txt", true, false),
    ];
    let pairs = difftastic::read_pairs(
        &f.app,
        &Source::Revision("HEAD".into()),
        &Source::WorkTree,
        &files,
        &patch.submodules,
    )
    .unwrap();
    assert_eq!(pairs[1].old, Content::Bytes(b"one\ntwo\n".to_vec()));
    assert_eq!(pairs[1].new, Content::Bytes(b"one\nTWO\n".to_vec()));
    assert_eq!(pairs[2].old, Content::Absent);
    assert_eq!(pairs[2].new, Content::Bytes(b"new\n".to_vec()));
    let entry = difftastic::compare(&pairs[0]);
    assert_eq!(entry.outcome, difftastic::Outcome::Submodule);

    // Between commits both sides come from the submodule's commits.
    git(&f.app, &["commit", "-qam", "bump lib"]);
    let patch = sg_git::commit_patch(&f.app, "HEAD").unwrap();
    let pairs = difftastic::read_pairs(
        &f.app,
        &Source::Revision("HEAD^".into()),
        &Source::Revision("HEAD".into()),
        &files[1..2],
        &patch.submodules,
    )
    .unwrap();
    assert_eq!(pairs[0].new, Content::Bytes(b"one\nTWO\n".to_vec()));
}
