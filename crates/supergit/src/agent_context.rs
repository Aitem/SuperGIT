//! Bounded, inspectable snapshots attached to an explicitly chosen conversation.

use gpui_kit::component::diff::DiffFile;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const MAX_ATTACHMENTS: usize = 5;
pub const MAX_BYTES: usize = 16 * 1024;
pub const CONTEXT_MARKER: &str = "\n\nAttached context snapshots:\n";

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    #[default]
    Diff,
    ReviewThread,
    CheckDiagnostics,
}

impl AttachmentKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Diff => "diff snapshot",
            Self::ReviewThread => "GitHub review thread",
            Self::CheckDiagnostics => "GitHub check diagnostics",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Attachment {
    pub kind: AttachmentKind,
    pub worktree: PathBuf,
    pub path: String,
    pub comparison: String,
    pub selection: String,
    pub body: String,
    pub truncated: bool,
    pub unavailable: bool,
}

impl Attachment {
    pub fn validate_destination(
        &self,
        worktree: &std::path::Path,
        cwd: &std::path::Path,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.worktree == worktree && cwd.starts_with(worktree),
            "This conversation runs in {}. Use Ask agent in new conversation for this worktree.",
            cwd.display()
        );
        Ok(())
    }
    pub fn bounded(mut self) -> Self {
        if self.body.len() > MAX_BYTES {
            let mut end = MAX_BYTES;
            while !self.body.is_char_boundary(end) {
                end -= 1;
            }
            self.body.truncate(end);
            self.truncated = true;
        }
        self
    }

    pub fn prompt(&self) -> String {
        format!(
            "Attached {}\nWorktree: {}\nFile: {}\nComparison: {}\nSelection: {}{}{}\n\n{}\nEnd of attached snapshot\n",
            self.kind.label(),
            self.worktree.display(),
            self.path,
            self.comparison,
            self.selection,
            if self.truncated { " · truncated" } else { "" },
            if self.unavailable {
                " · content unavailable (metadata only)"
            } else {
                ""
            },
            self.body
        )
    }
}

/// Keep the historical hunk and the discussion together, pinned to the PR head.
pub fn review_attachment(
    worktree: PathBuf,
    pr: &sg_github::PullRequest,
    thread: &sg_github::ReviewThread,
) -> Attachment {
    use std::fmt::Write as _;

    let selection = match (thread.outdated, thread.line) {
        (true, Some(line)) => format!("Outdated review thread · original line {line}"),
        (false, Some(line)) => format!("Review thread · line {line}"),
        (_, None) => "Review thread".into(),
    };
    let mut body = format!("PR #{} · {}\n{}\n", pr.number, pr.title, pr.url);
    if !pr.head_oid.is_empty() {
        let _ = writeln!(body, "PR head {}", pr.head_oid);
    }
    if thread.resolved {
        let _ = writeln!(body, "Review thread is resolved");
    }
    if thread.outdated {
        let _ = writeln!(body, "This hunk is from an older version of the PR");
    }
    for comment in thread.comments.iter().take(1) {
        let _ = writeln!(
            body,
            "\n@{} · {}\n{}",
            comment.author, comment.url, comment.body
        );
    }
    if !thread.diff_hunk.is_empty() {
        let _ = writeln!(body, "\nHunk from the first comment\n{}", thread.diff_hunk);
    }
    for comment in thread.comments.iter().skip(1) {
        let _ = writeln!(
            body,
            "\n@{} · {}\n{}",
            comment.author, comment.url, comment.body
        );
    }
    Attachment {
        kind: AttachmentKind::ReviewThread,
        worktree,
        path: thread.path.clone(),
        comparison: format!(
            "PR #{} · {} → {}",
            pr.number, pr.head_branch, pr.base_branch
        ),
        selection,
        body,
        ..Default::default()
    }
    .bounded()
}

/// Match parsed paths rather than splitting a quoted Git filename by spaces.
pub fn file_patch<'a>(patch: &'a str, path: &str) -> Option<&'a str> {
    let mut starts = vec![0];
    starts.extend(
        patch
            .match_indices("\ndiff --git ")
            .map(|(offset, _)| offset + 1),
    );
    starts.push(patch.len());
    starts.windows(2).find_map(|range| {
        let block = &patch[range[0]..range[1]];
        let files = DiffFile::parse(block).ok()?;
        files
            .iter()
            .any(|file| file.path().as_ref() == path)
            .then_some(block)
    })
}

pub use sg_git::hunk_patch;

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "diff --git a/deleted.txt b/deleted.txt\ndeleted file mode 100644\n--- a/deleted.txt\n+++ /dev/null\n@@ -10,2 +9,0 @@\n-old one\n-old two\ndiff --git a/other.txt b/other.txt\n--- a/other.txt\n+++ b/other.txt\n@@ -1 +1 @@\n-before\n+after\n@@ -20 +20 @@\n-old\n+new\n";

    #[test]
    fn deleted_file_original_hunk_and_modified_hunk_keep_their_own_content() {
        let deleted = file_patch(PATCH, "deleted.txt").unwrap();
        assert!(hunk_patch(deleted, true, 11).unwrap().contains("-old two"));
        assert!(hunk_patch(deleted, false, 11).is_none());
        assert!(hunk_patch(deleted, false, 9).is_none());
        let other = file_patch(PATCH, "other.txt").unwrap();
        let last = hunk_patch(other, false, 20).unwrap();
        assert!(last.contains("+new"));
        assert!(!last.contains("+after"));
        assert!(file_patch(PATCH, "stale-selection.txt").is_none());
    }

    #[test]
    fn context_is_bounded_without_splitting_unicode_and_names_its_worktree() {
        let item = Attachment {
            worktree: "/work/task-a".into(),
            path: "file.txt".into(),
            body: "界".repeat(MAX_BYTES),
            ..Default::default()
        }
        .bounded();
        assert!(item.truncated);
        assert!(item.body.len() <= MAX_BYTES);
        assert!(item.prompt().contains("/work/task-a"));
        assert!(item.prompt().contains("truncated"));
    }

    #[test]
    fn a_review_attachment_keeps_the_historical_line_and_discussion() {
        let pr = sg_github::PullRequest {
            created_at: None,
            merged_at: None,
            number: 28,
            title: "Cache workspace reads".into(),
            head_branch: "feat/cache".into(),
            head_oid: "abcdef".into(),
            head_repository: Some("demo/harbor".into()),
            base_branch: "main".into(),
            state: sg_github::PrState::Open,
            url: "https://github.com/demo/harbor/pull/28".into(),
            author: "maya".into(),
            review_decision: String::new(),
            checks: Default::default(),
            additions: 1,
            deletions: 0,
            updated_at: String::new(),
        };
        let thread = sg_github::ReviewThread {
            id: "thread-1".into(),
            path: "src/cache.ts".into(),
            line: Some(22),
            resolved: false,
            outdated: true,
            permissions: Default::default(),
            side: sg_github::ReviewSide::Modified,
            diff_hunk: "@@ -22 +22 @@\n-read()\n+readWithExpiry()".into(),
            comments: vec![sg_github::Comment {
                author: "alex".into(),
                body: "Handle expiry at the boundary.".into(),
                created_at: None,
                url: "https://github.com/demo/harbor/pull/28#discussion_r1".into(),
            }],
        };
        let attachment = review_attachment("/repo/cache".into(), &pr, &thread);
        let prompt = attachment.prompt();
        assert!(prompt.contains("Attached GitHub review thread"));
        assert!(prompt.contains("original line 22"));
        assert!(prompt.contains("PR head abcdef"));
        assert!(prompt.contains("Handle expiry at the boundary"));
        assert!(prompt.contains("+readWithExpiry()"));
        assert!(
            attachment
                .validate_destination(
                    std::path::Path::new("/repo/other"),
                    std::path::Path::new("/repo/other")
                )
                .is_err()
        );
        let old: Attachment =
            serde_json::from_str(r#"{"path":"old.txt","body":"legacy"}"#).unwrap();
        assert!(old.prompt().starts_with("Attached diff snapshot"));
    }

    #[test]
    fn snapshots_cannot_follow_navigation_into_another_worktree_or_session_cwd() {
        let item = Attachment {
            worktree: "/repo/task-a".into(),
            ..Default::default()
        };
        assert!(
            item.validate_destination(
                std::path::Path::new("/repo/task-a"),
                std::path::Path::new("/repo/task-a/src")
            )
            .is_ok()
        );
        assert!(
            item.validate_destination(
                std::path::Path::new("/repo/task-b"),
                std::path::Path::new("/repo/task-b")
            )
            .is_err()
        );
        assert!(
            item.validate_destination(
                std::path::Path::new("/repo/task-a"),
                std::path::Path::new("/repo/task-a-other")
            )
            .is_err()
        );
    }
}
