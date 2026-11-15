use std::path::Path;

use anyhow::Result;

use crate::{git, git_bytes};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileState {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflicted,
}

impl FileState {
    fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            b'A' => Self::Added,
            b'M' => Self::Modified,
            b'D' => Self::Deleted,
            b'R' => Self::Renamed,
            b'C' => Self::Copied,
            b'T' => Self::TypeChanged,
            _ => return None,
        })
    }

    /// One-letter badge, like `git status -s`.
    pub fn letter(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Modified => "M",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Copied => "C",
            Self::TypeChanged => "T",
            Self::Untracked => "?",
            Self::Conflicted => "!",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub orig_path: Option<String>,
    /// Change recorded in the index (staged).
    pub staged: Option<FileState>,
    /// Change in the working tree not yet staged.
    pub unstaged: Option<FileState>,
}

impl FileChange {
    /// The most relevant state for a single badge.
    pub fn state(&self) -> FileState {
        self.unstaged.or(self.staged).unwrap_or(FileState::Modified)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeStatus {
    pub head_oid: Option<String>,
    /// `None` when detached.
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<FileChange>,
}

impl WorktreeStatus {
    pub fn is_clean(&self) -> bool {
        self.files.is_empty()
    }
    pub fn staged_count(&self) -> usize {
        self.files.iter().filter(|f| f.staged.is_some()).count()
    }
    pub fn untracked_count(&self) -> usize {
        self.files
            .iter()
            .filter(|f| f.unstaged == Some(FileState::Untracked))
            .count()
    }
    pub fn conflicted_count(&self) -> usize {
        self.files
            .iter()
            .filter(|f| f.unstaged == Some(FileState::Conflicted))
            .count()
    }
    pub fn is_unborn(&self) -> bool {
        self.head_oid.is_none()
    }
}

pub fn status(worktree: &Path) -> Result<WorktreeStatus> {
    let out = git_bytes(
        worktree,
        [
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
        ],
    )?;
    Ok(parse_status(&String::from_utf8_lossy(&out)))
}

fn parse_status(out: &str) -> WorktreeStatus {
    let mut st = WorktreeStatus::default();
    let mut fields = out.split('\0').peekable();
    while let Some(entry) = fields.next() {
        if entry.is_empty() {
            continue;
        }
        if let Some(header) = entry.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" if value != "(initial)" => st.head_oid = Some(value.into()),
                "branch.head" if value != "(detached)" => st.branch = Some(value.into()),
                "branch.upstream" => st.upstream = Some(value.into()),
                "branch.ab" => {
                    for part in value.split(' ') {
                        if let Some(n) = part.strip_prefix('+') {
                            st.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            st.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let kind = entry.as_bytes()[0];
        match kind {
            // 1 XY sub mH mI mW hH hI path
            b'1' => {
                let parts: Vec<&str> = entry.splitn(9, ' ').collect();
                if parts.len() == 9 {
                    st.files.push(change(parts[1], parts[8], None));
                }
            }
            // 2 XY sub mH mI mW hH hI Xscore path \0 origPath
            b'2' => {
                let parts: Vec<&str> = entry.splitn(10, ' ').collect();
                let orig = fields.next().map(str::to_string);
                if parts.len() == 10 {
                    st.files.push(change(parts[1], parts[9], orig));
                }
            }
            // u XY sub m1 m2 m3 mW h1 h2 h3 path
            b'u' => {
                let parts: Vec<&str> = entry.splitn(11, ' ').collect();
                if parts.len() == 11 {
                    st.files.push(FileChange {
                        path: parts[10].into(),
                        orig_path: None,
                        staged: None,
                        unstaged: Some(FileState::Conflicted),
                    });
                }
            }
            b'?' => st.files.push(FileChange {
                path: entry[2..].into(),
                orig_path: None,
                staged: None,
                unstaged: Some(FileState::Untracked),
            }),
            _ => {}
        }
    }
    st
}

fn change(xy: &str, path: &str, orig_path: Option<String>) -> FileChange {
    let xy = xy.as_bytes();
    FileChange {
        path: path.into(),
        orig_path,
        staged: xy.first().copied().and_then(FileState::from_code),
        unstaged: xy.get(1).copied().and_then(FileState::from_code),
    }
}

const MAX_UNTRACKED_FILES: usize = 100;
const MAX_UNTRACKED_BYTES: u64 = 256 * 1024;

/// Unified patch of everything that differs from HEAD in the worktree:
/// staged and unstaged changes, plus untracked files as new-file patches.
pub fn worktree_patch(worktree: &Path, status: &WorktreeStatus) -> Result<String> {
    let mut patch = if status.is_unborn() {
        let mut p = git(
            worktree,
            ["diff", "--cached", "--no-color", "--no-ext-diff"],
        )?;
        p.push_str(&git(worktree, ["diff", "--no-color", "--no-ext-diff"])?);
        p
    } else {
        git(
            worktree,
            ["diff", "HEAD", "--no-color", "--no-ext-diff", "-M", "--"],
        )?
    };

    for file in status
        .files
        .iter()
        .filter(|f| f.unstaged == Some(FileState::Untracked))
        .take(MAX_UNTRACKED_FILES)
    {
        append_new_file_patch(&mut patch, worktree, &file.path);
    }
    Ok(patch)
}

fn append_new_file_patch(patch: &mut String, worktree: &Path, rel: &str) {
    let full = worktree.join(rel);
    let Ok(meta) = std::fs::symlink_metadata(&full) else {
        return;
    };
    let is_symlink = meta.file_type().is_symlink();
    if !meta.is_file() && !is_symlink {
        return;
    }
    if !patch.is_empty() && !patch.ends_with('\n') {
        patch.push('\n');
    }
    let mode = if is_symlink { "120000" } else { "100644" };
    patch.push_str(&format!(
        "diff --git a/{rel} b/{rel}\nnew file mode {mode}\n"
    ));
    if meta.len() > MAX_UNTRACKED_BYTES {
        patch.push_str(&format!("Binary files /dev/null and b/{rel} differ\n"));
        return;
    }
    // Git stores the link's target path, including for dangling/directory links.
    // Never read the target's contents (which may be outside this worktree).
    let bytes = if is_symlink {
        std::fs::read_link(&full).map(|p| p.as_os_str().as_encoded_bytes().to_vec())
    } else {
        std::fs::read(&full)
    };
    let Ok(bytes) = bytes else {
        return;
    };
    if bytes.contains(&0) {
        patch.push_str(&format!("Binary files /dev/null and b/{rel} differ\n"));
        return;
    }
    if bytes.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    patch.push_str(&format!(
        "--- /dev/null\n+++ b/{rel}\n@@ -0,0 +1,{} @@\n",
        lines.len()
    ));
    for line in &lines {
        patch.push('+');
        patch.push_str(line);
    }
    if !text.ends_with('\n') {
        patch.push_str("\n\\ No newline at end of file\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_v2() {
        let out = "# branch.oid 1234abcd\0# branch.head feat\0# branch.upstream origin/feat\0# branch.ab +2 -1\0\
                   1 .M N... 100644 100644 100644 aaa bbb src/main.rs\0\
                   1 A. N... 000000 100644 100644 000 ccc new file.rs\0\
                   2 R. N... 100644 100644 100644 aaa bbb R100 to.rs\0from.rs\0\
                   u UU N... 100644 100644 100644 100644 a b c conflict.rs\0\
                   ? untracked.txt\0";
        let st = parse_status(out);
        assert_eq!(st.branch.as_deref(), Some("feat"));
        assert_eq!((st.ahead, st.behind), (2, 1));
        assert_eq!(st.files.len(), 5);
        assert_eq!(st.files[0].unstaged, Some(FileState::Modified));
        assert_eq!(st.files[1].path, "new file.rs");
        assert_eq!(st.files[1].staged, Some(FileState::Added));
        assert_eq!(st.files[2].orig_path.as_deref(), Some("from.rs"));
        assert_eq!(st.conflicted_count(), 1);
        assert_eq!(st.untracked_count(), 1);
    }

    #[test]
    fn detached_and_initial() {
        let st = parse_status("# branch.oid (initial)\0# branch.head (detached)\0");
        assert!(st.is_unborn());
        assert!(st.branch.is_none());
    }
}
