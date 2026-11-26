use std::path::Path;

use anyhow::Result;

use crate::submodule::{Patch, SubmoduleFlags, inline_submodules};
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
    pub(crate) fn from_code(c: u8) -> Option<Self> {
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub orig_path: Option<String>,
    /// Change recorded in the index (staged).
    pub staged: Option<FileState>,
    /// Change in the working tree not yet staged.
    pub unstaged: Option<FileState>,
    /// A submodule: what changed in its checkout (`git status` only).
    pub submodule: Option<SubmoduleFlags>,
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

/// Worktree sidebar polling. An explicitly opted-in probe may answer when
/// its supported Git layout and config match. Operations still read CLI status.
pub fn polled_status(worktree: &Path) -> Result<WorktreeStatus> {
    #[cfg(feature = "git2-probe")]
    if crate::use_git2_probe()
        && let Ok(status) = crate::native_status::read(worktree)
    {
        return Ok(status);
    }
    status(worktree)
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
                    st.files.push(change(parts[1], parts[2], parts[8], None));
                }
            }
            // 2 XY sub mH mI mW hH hI Xscore path \0 origPath
            b'2' => {
                let parts: Vec<&str> = entry.splitn(10, ' ').collect();
                let orig = fields.next().map(str::to_string);
                if parts.len() == 10 {
                    st.files.push(change(parts[1], parts[2], parts[9], orig));
                }
            }
            // u XY sub m1 m2 m3 mW h1 h2 h3 path
            b'u' => {
                let parts: Vec<&str> = entry.splitn(11, ' ').collect();
                if parts.len() == 11 {
                    st.files.push(FileChange {
                        path: parts[10].into(),
                        unstaged: Some(FileState::Conflicted),
                        submodule: SubmoduleFlags::parse(parts[2]),
                        ..FileChange::default()
                    });
                }
            }
            b'?' => st.files.push(FileChange {
                path: entry[2..].into(),
                unstaged: Some(FileState::Untracked),
                ..FileChange::default()
            }),
            _ => {}
        }
    }
    st
}

fn change(xy: &str, sub: &str, path: &str, orig_path: Option<String>) -> FileChange {
    let xy = xy.as_bytes();
    FileChange {
        path: path.into(),
        orig_path,
        staged: xy.first().copied().and_then(FileState::from_code),
        unstaged: xy.get(1).copied().and_then(FileState::from_code),
        submodule: SubmoduleFlags::parse(sub),
    }
}

/// `git diff` options for every patch SuperGIT shows: plain output whatever
/// the user's config, with submodules as `Subproject commit` entries for
/// [`inline_submodules`] to expand.
pub(crate) const PATCH_OPTIONS: [&str; 6] = [
    "--no-color",
    "--no-ext-diff",
    "--submodule=short",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "-M",
];

const MAX_UNTRACKED_FILES: usize = 100;
const MAX_UNTRACKED_BYTES: u64 = 256 * 1024;

/// Which side of the index to show for local changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeScope {
    #[default]
    All,
    Staged,
    Unstaged,
}

impl ChangeScope {
    pub fn project(self, file: &FileChange) -> Option<FileChange> {
        let mut file = file.clone();
        match self {
            Self::All => {}
            Self::Staged => {
                file.staged?;
                file.unstaged = None;
            }
            Self::Unstaged => {
                file.unstaged?;
                file.staged = None;
            }
        }
        Some(file)
    }
}

/// Working tree versus HEAD, including untracked files.
pub fn worktree_patch(worktree: &Path, status: &WorktreeStatus) -> Result<Patch> {
    scoped_worktree_patch(worktree, status, ChangeScope::All)
}

pub fn scoped_worktree_patch(
    worktree: &Path,
    status: &WorktreeStatus,
    scope: ChangeScope,
) -> Result<Patch> {
    let diff = |args: &[&str]| {
        let mut all = vec!["diff"];
        all.extend(PATCH_OPTIONS);
        all.extend(args);
        all.push("--");
        git(worktree, all)
    };
    let mut patch = match scope {
        ChangeScope::Staged => return Ok(inline_submodules(worktree, &diff(&["--cached"])?, None)),
        ChangeScope::Unstaged => diff(&[])?,
        ChangeScope::All if status.is_unborn() => {
            let mut staged = diff(&["--cached"])?;
            staged.push_str(&diff(&[])?);
            staged
        }
        ChangeScope::All => diff(&["HEAD"])?,
    };
    for file in status
        .files
        .iter()
        .filter(|f| f.unstaged == Some(FileState::Untracked))
        .take(MAX_UNTRACKED_FILES)
    {
        append_new_file_patch(&mut patch, worktree, &file.path);
    }
    let mut unstaged;
    let status = if scope == ChangeScope::Unstaged {
        unstaged = status.clone();
        unstaged.files.retain(|file| file.unstaged.is_some());
        &unstaged
    } else {
        status
    };
    Ok(inline_submodules(worktree, &patch, Some(status)))
}

pub(crate) fn append_new_file_patch(patch: &mut String, worktree: &Path, rel: &str) {
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
    let lines = text.split_inclusive('\n');
    patch.push_str(&format!(
        "--- /dev/null\n+++ b/{rel}\n@@ -0,0 +1,{} @@\n",
        lines.clone().count()
    ));
    for line in lines {
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
