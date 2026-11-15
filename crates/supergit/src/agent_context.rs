//! Bounded, inspectable snapshots attached to an explicitly chosen conversation.

use gpui_kit::component::diff::DiffFile;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const MAX_ATTACHMENTS: usize = 5;
pub const MAX_BYTES: usize = 16 * 1024;
pub const CONTEXT_MARKER: &str = "\n\nAttached context snapshots:\n";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Attachment {
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
            "Attached diff snapshot\nWorktree: {}\nFile: {}\nComparison: {}\nSelection: {}{}{}\n\n{}\nEnd of attached snapshot\n",
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

pub fn hunk_patch(patch: &str, original: bool, line: usize) -> Option<String> {
    let first = patch.find("\n@@ ")? + 1;
    let headers = &patch[..first];
    let mut starts = vec![first];
    starts.extend(
        patch[first..]
            .match_indices("\n@@ ")
            .map(|(offset, _)| first + offset + 1),
    );
    starts.push(patch.len());
    for range in starts.windows(2) {
        let hunk = &patch[range[0]..range[1]];
        let header = hunk.lines().next()?;
        let field = header
            .split_whitespace()
            .nth(if original { 1 } else { 2 })?;
        let (start, count) = field
            .get(1..)?
            .split_once(',')
            .unwrap_or((&field[1..], "1"));
        let start: usize = start.parse().ok()?;
        let count: usize = count.parse().ok()?;
        if count > 0 && line >= start && line < start.saturating_add(count) {
            return Some(format!("{headers}{hunk}"));
        }
    }
    None
}

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
