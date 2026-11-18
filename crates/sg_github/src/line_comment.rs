use std::path::Path;

use anyhow::{Result, ensure};
use serde_json::{Value, json};

use crate::{ReviewSide, gh_with_input, pr_details, pull_request_repository};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewLine {
    pub repository: String,
    pub number: u64,
    pub head: String,
    pub base: String,
    pub path: String,
    pub line: usize,
    pub side: ReviewSide,
}

/// Post a single review comment at the pinned GitHub diff line.
pub fn post_review_line(repo: &Path, target: &ReviewLine, body: &str) -> Result<()> {
    ensure!(!body.trim().is_empty(), "Write a comment first");
    ensure!(
        target.line > 0 && !target.path.is_empty(),
        "Select a line in the pull request diff"
    );
    let current = pr_details(repo, target.number)?;
    ensure!(
        current.pr.is_open()
            && pull_request_repository(&current.pr).as_ref() == Some(&target.repository)
            && current.pr.head_oid == target.head
            && current.base_oid == target.base,
        "The pull request changed. Refresh the diff and select the line again."
    );
    let endpoint = format!(
        "repos/{}/pulls/{}/comments",
        target.repository, target.number
    );
    let input = serde_json::to_vec(&payload(target, body))?;
    gh_with_input(
        repo,
        [
            "api",
            "--hostname",
            "github.com",
            "--method",
            "POST",
            &endpoint,
            "--input",
            "-",
        ],
        Some(&input),
    )
    .map(drop)
}

fn payload(target: &ReviewLine, body: &str) -> Value {
    let side = match target.side {
        ReviewSide::Original => "LEFT",
        ReviewSide::Modified => "RIGHT",
    };
    json!({ "body": body, "commit_id": target.head, "path": target.path, "line": target.line, "side": side })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posting_preserves_exact_side_path_head_and_multiline_text() {
        let mut target = ReviewLine {
            repository: "owner/repo".into(),
            number: 7,
            head: "source-oid".into(),
            base: "base-oid".into(),
            path: "src/сброс cache.rs".into(),
            line: 13,
            side: ReviewSide::Original,
        };
        let text = "Why is the fallback removed?\n\nCould we keep it for retries?";
        assert_eq!(
            payload(&target, text),
            json!({"body":text,"commit_id":"source-oid","path":"src/сброс cache.rs","line":13,"side":"LEFT"})
        );
        target.side = ReviewSide::Modified;
        assert_eq!(payload(&target, text)["side"], "RIGHT");
    }
}
