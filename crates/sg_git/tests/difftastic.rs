#![expect(
    clippy::unwrap_used,
    reason = "test helpers fail the test by panicking"
)]

//! Runs the real `difft` when it is installed (CI does not have it).

use std::path::Path;
use std::process::Command;

use sg_git::difftastic::{self, Content, FileRequest, Outcome, Source};

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

fn repo() -> tempfile::TempDir {
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
    std::fs::write(
        root.join("lib.rs"),
        "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);
    temp
}

fn request(path: &str) -> FileRequest {
    FileRequest {
        path: path.into(),
        old_path: None,
        added: false,
        deleted: false,
    }
}

#[test]
fn compares_commits_and_the_working_tree() {
    if difftastic::version().is_none() {
        eprintln!("difft not installed; skipping");
        return;
    }
    let temp = repo();
    let root = temp.path();

    // The root commit's parent does not exist: the file reads as created.
    let pairs = difftastic::read_pairs(
        root,
        &Source::Revision("HEAD^".into()),
        &Source::Revision("HEAD".into()),
        &[request("lib.rs")],
    )
    .unwrap();
    assert_eq!(pairs[0].old, Content::Absent);
    let files = difftastic::compare_many(&pairs.iter().collect::<Vec<_>>());
    assert_eq!(files[0].outcome, Outcome::Created);
    assert_eq!(files[0].language.as_deref(), Some("Rust"));
    assert_eq!(files[0].additions(), 3);

    // Reformatting only: difftastic sees no syntactic change.
    std::fs::write(
        root.join("lib.rs"),
        "fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    std::fs::write(root.join("new.py"), "print('привет')\n").unwrap();
    let pairs = difftastic::read_pairs(
        root,
        &Source::Revision("HEAD".into()),
        &Source::WorkTree,
        &[
            request("lib.rs"),
            FileRequest {
                added: true,
                ..request("new.py")
            },
        ],
    )
    .unwrap();
    let files = difftastic::compare_many(&pairs.iter().collect::<Vec<_>>());
    assert_eq!(files[0].outcome, Outcome::Unchanged);
    assert!(files[0].hunks(3).is_empty());
    assert_eq!(files[1].outcome, Outcome::Created);
    assert_eq!(files[1].new[0].text, "print('привет')");

    // A real change pairs the edited line and marks only the new token.
    std::fs::write(
        root.join("lib.rs"),
        "fn add(a: i32, b: i32) -> i32 {\n    a + b + 1\n}\n",
    )
    .unwrap();
    let pairs = difftastic::read_pairs(
        root,
        &Source::Revision("HEAD".into()),
        &Source::WorkTree,
        &[request("lib.rs")],
    )
    .unwrap();
    let file = &difftastic::compare_many(&pairs.iter().collect::<Vec<_>>())[0];
    assert_eq!(file.outcome, Outcome::Changed);
    assert_eq!(file.hunks(0).len(), 1);
    let line = &file.new[1];
    assert!(line.changed);
    let novel: Vec<&str> = line.novel.iter().map(|r| &line.text[r.clone()]).collect();
    assert_eq!(novel.concat().replace(' ', ""), "+1");
}
