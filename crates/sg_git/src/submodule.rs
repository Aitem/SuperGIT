//! Submodules in diffs.
//!
//! Git shows a changed submodule as a single line pair — `Subproject commit
//! <old>` → `<new>` — which says nothing about what changed. This module
//! finds those entries in a patch and inlines each submodule's own diff right
//! after it, its paths prefixed with the submodule's path (as `git diff
//! --submodule=diff` does), and lists the commits between the two recorded
//! commits. Untracked files in a submodule's checkout, which `git diff`
//! ignores but `git status` reports, become new-file patches like the
//! superproject's own untracked files.
//!
//! Patches given here must come from `git diff --submodule=short` (the
//! default, made explicit so a user's `diff.submodule` cannot change it).

use std::borrow::Cow;
use std::path::Path;

use crate::status::append_new_file_patch;
use crate::{FileChange, FileState, WorktreeStatus, git, git_bytes};

/// Commits listed per submodule; the counts cover all of them.
pub const MAX_COMMITS: usize = 100;
/// A submodule diff touching more files than this is summarized instead.
const MAX_INLINE_FILES: usize = 300;
/// Untracked files of one submodule shown as new-file patches.
const MAX_UNTRACKED_FILES: usize = 100;

/// A submodule's state in `git status --porcelain=v2` (`S<c><m><u>`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubmoduleFlags {
    /// The checkout's HEAD differs from the recorded commit.
    pub commit_changed: bool,
    /// The checkout has changes to tracked files.
    pub modified: bool,
    /// The checkout has untracked files.
    pub untracked: bool,
}

impl SubmoduleFlags {
    /// Parses the porcelain v2 `<sub>` field; `None` for a regular file (`N...`).
    pub(crate) fn parse(field: &str) -> Option<Self> {
        let flags = field.strip_prefix('S')?.as_bytes();
        let flag = |ix: usize, set: u8| flags.get(ix) == Some(&set);
        Some(Self {
            commit_changed: flag(0, b'C'),
            modified: flag(1, b'M'),
            untracked: flag(2, b'U'),
        })
    }
}

/// A commit an update of a submodule brings in, or drops.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmoduleCommit {
    pub sha: String,
    pub subject: String,
    /// Reachable only from the old commit: the update drops it (a rewind).
    pub removed: bool,
}

/// Whether a submodule's own diff follows its entry in the patch, and why not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inline {
    Shown,
    /// Added or removed as a whole: its contents are not repeated.
    AddedOrRemoved,
    /// Not checked out in this worktree (`git submodule update --init`).
    NotCheckedOut,
    /// The checkout lacks one of the two commits (fetch in the submodule).
    CommitsMissing,
    /// More files changed than are worth inlining.
    TooLarge(usize),
    Failed(String),
}

/// A submodule whose recorded commit or checkout differs in a diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmoduleChange {
    pub path: String,
    /// The recorded commit before; `None` for a new submodule.
    pub old: Option<String>,
    /// The commit after — for a working-tree diff, the checkout's HEAD;
    /// `None` for a removed submodule.
    pub new: Option<String>,
    /// The checkout has uncommitted changes to tracked files.
    pub modified: bool,
    /// The checkout has untracked files.
    pub untracked: bool,
    /// Commits between `old` and `new`, newest first, at most [`MAX_COMMITS`].
    pub commits: Vec<SubmoduleCommit>,
    /// All commits only `new` has.
    pub added_commits: usize,
    /// All commits only `old` has.
    pub removed_commits: usize,
    pub inline: Inline,
    /// The files its inlined diff touches, paths prefixed with `path`.
    pub files: Vec<FileChange>,
}

impl SubmoduleChange {
    /// Whether `path` is inside this submodule.
    pub fn contains(&self, path: &str) -> bool {
        path.strip_prefix(self.path.as_str())
            .is_some_and(|rest| rest.starts_with('/'))
    }
}

/// A diff as text, and the submodules it changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Patch {
    pub text: String,
    pub submodules: Vec<SubmoduleChange>,
}

/// The innermost submodule of `submodules` that contains `path`.
pub fn submodule_of<'a>(
    submodules: &'a [SubmoduleChange],
    path: &str,
) -> Option<&'a SubmoduleChange> {
    submodules
        .iter()
        .filter(|submodule| submodule.contains(path))
        .max_by_key(|submodule| submodule.path.len())
}

/// `files` with the files inside each submodule after the submodule's own
/// entry (at the end when it has none).
pub fn with_submodule_files(
    mut files: Vec<FileChange>,
    submodules: &[SubmoduleChange],
) -> Vec<FileChange> {
    for submodule in submodules.iter().filter(|s| !s.files.is_empty()) {
        let at = files
            .iter()
            .position(|file| file.path == submodule.path)
            .map_or(files.len(), |ix| ix + 1);
        let inner = submodule
            .files
            .iter()
            .filter(|inner| !files.iter().any(|file| file.path == inner.path))
            .cloned()
            .collect::<Vec<_>>();
        files.splice(at..at, inner);
    }
    files
}

/// Inlines each changed submodule's diff into `patch` (see the module docs).
/// `status` is given when the new side is the working tree: it supplies the
/// checkouts' untracked files, and submodules `git diff` leaves out because
/// only untracked files changed. Submodules are read from their checkouts
/// under `worktree`; one that cannot be read keeps its plain entry.
pub fn inline_submodules(worktree: &Path, patch: &str, status: Option<&WorktreeStatus>) -> Patch {
    // Most patches touch no submodule: skip splitting them.
    let dirty_checkout = status.is_some_and(|status| {
        status
            .files
            .iter()
            .any(|f| f.submodule.is_some_and(|s| s.modified || s.untracked))
    });
    if !dirty_checkout && !patch.contains("Subproject commit ") {
        return Patch {
            text: patch.to_string(),
            submodules: Vec::new(),
        };
    }
    let mut chunks: Vec<Chunk> = split_chunks(patch).into_iter().map(Chunk::new).collect();
    if let Some(status) = status {
        for file in &status.files {
            let Some(flags) = file.submodule else {
                continue;
            };
            let listed = chunks
                .iter()
                .any(|chunk| chunk.gitlink.as_ref().is_some_and(|g| g.path == file.path));
            if listed || !(flags.untracked || flags.modified) {
                continue;
            }
            // Git leaves a checkout that only gained untracked files out of
            // its diff; show it like `--ignore-submodules=none` would.
            let checkout = worktree.join(&file.path);
            let Some(head) = checkout_head(&checkout) else {
                continue;
            };
            let chunk = Chunk::new(Cow::Owned(gitlink_chunk(&file.path, &head)));
            let at = chunks
                .iter()
                .position(|c| c.path.as_deref().is_some_and(|p| p > file.path.as_str()))
                .unwrap_or(chunks.len());
            chunks.insert(at, chunk);
        }
    }
    let mut text = String::with_capacity(patch.len());
    let mut submodules = Vec::new();
    for chunk in chunks {
        text.push_str(&chunk.text);
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        let Some(gitlink) = chunk.gitlink else {
            continue;
        };
        let flags = status
            .and_then(|status| status.files.iter().find(|f| f.path == gitlink.path))
            .and_then(|file| file.submodule)
            .unwrap_or_default();
        let (change, inner) = inspect(worktree, gitlink, status.is_some(), flags);
        text.push_str(&inner);
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        submodules.push(change);
    }
    Patch { text, submodules }
}

/// One file's part of a patch (`diff --git` up to the next one), or the text
/// before the first file.
struct Chunk<'a> {
    text: Cow<'a, str>,
    /// The file's path, for keeping the patch in path order.
    path: Option<String>,
    gitlink: Option<Gitlink>,
}

impl<'a> Chunk<'a> {
    fn new(text: Cow<'a, str>) -> Self {
        let path = chunk_path(&text);
        let gitlink = parse_gitlink(&text);
        Self {
            text,
            path,
            gitlink,
        }
    }
}

/// A submodule's entry in a patch.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Gitlink {
    path: String,
    old: Option<String>,
    new: Option<String>,
    /// `-dirty`: the checkout has uncommitted changes.
    dirty: bool,
}

fn split_chunks(patch: &str) -> Vec<Cow<'_, str>> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut offset = 0;
    for line in patch.split_inclusive('\n') {
        if line.starts_with("diff --git ") && offset > start {
            chunks.push(Cow::Borrowed(&patch[start..offset]));
            start = offset;
        }
        offset += line.len();
    }
    if offset > start {
        chunks.push(Cow::Borrowed(&patch[start..offset]));
    }
    chunks
}

/// The new path of a chunk (the old one for a deletion).
fn chunk_path(chunk: &str) -> Option<String> {
    let mut old = None;
    for line in chunk.lines() {
        if line.starts_with("@@ ") {
            break;
        }
        if let Some(path) = line.strip_prefix("--- ") {
            old = side_path(path, "a/");
        } else if let Some(path) = line.strip_prefix("+++ ") {
            return side_path(path, "b/").or(old);
        } else if let Some(path) = line.strip_prefix("rename to ") {
            return Some(unquote(path));
        }
    }
    if old.is_some() {
        return old;
    }
    // Without `---`/`+++` lines (a mode change, a binary file): the header
    // names the same path twice, `diff --git a/<path> b/<path>`.
    let paths = chunk.lines().next()?.strip_prefix("diff --git ")?;
    let half = paths.len().checked_sub(1)? / 2;
    let (old, new) = (paths.get(..half)?, paths.get(half + 1..)?);
    (old.strip_prefix("a/") == new.strip_prefix("b/"))
        .then(|| new.strip_prefix("b/").map(str::to_string))
        .flatten()
}

/// A path on a `---`/`+++` line, without its `a/`/`b/` prefix; `None` for
/// `/dev/null`.
fn side_path(value: &str, prefix: &str) -> Option<String> {
    let value = value.split('\t').next().unwrap_or(value);
    if value == "/dev/null" {
        return None;
    }
    let path = unquote(value);
    Some(
        path.strip_prefix(prefix)
            .map(str::to_string)
            .unwrap_or(path),
    )
}

/// Git's C-style quoting of unusual paths (`"a/\320\271 b"`), undone.
fn unquote(value: &str) -> String {
    let Some(inner) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return value.to_string();
    };
    let bytes = inner.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut ix = 0;
    while ix < bytes.len() {
        let byte = bytes[ix];
        ix += 1;
        if byte != b'\\' || ix == bytes.len() {
            out.push(byte);
            continue;
        }
        let escaped = bytes[ix];
        ix += 1;
        out.push(match escaped {
            b'0'..=b'7' => {
                let mut value = u32::from(escaped - b'0');
                for _ in 0..2 {
                    match bytes.get(ix) {
                        Some(digit @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(digit - b'0');
                            ix += 1;
                        }
                        _ => break,
                    }
                }
                u8::try_from(value).unwrap_or(b'?')
            }
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            other => other,
        });
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The submodule entry in `chunk`, if it is one: mode `160000` (or the same
/// commit on both sides, `-dirty` after it) and nothing but `Subproject
/// commit` lines in its hunk.
fn parse_gitlink(chunk: &str) -> Option<Gitlink> {
    let mut gitlink_mode = false;
    let mut in_hunk = false;
    let (mut old_path, mut new_path) = (None, None);
    let (mut old, mut new, mut dirty) = (None, None, false);
    for line in chunk.lines() {
        if !in_hunk {
            if line.starts_with("@@ ") {
                in_hunk = true;
            } else if let Some(path) = line.strip_prefix("--- ") {
                old_path = side_path(path, "a/");
            } else if let Some(path) = line.strip_prefix("+++ ") {
                new_path = side_path(path, "b/");
            } else if matches!(
                line,
                "new file mode 160000"
                    | "deleted file mode 160000"
                    | "old mode 160000"
                    | "new mode 160000"
            ) || (line.starts_with("index ") && line.ends_with(" 160000"))
            {
                gitlink_mode = true;
            }
            continue;
        }
        if let Some(oid) = line.strip_prefix("-Subproject commit ") {
            old = Some(oid.to_string());
        } else if let Some(oid) = line.strip_prefix("+Subproject commit ") {
            let clean = oid.strip_suffix("-dirty");
            dirty = clean.is_some();
            new = Some(clean.unwrap_or(oid).to_string());
        } else if !line.starts_with("@@ ") {
            return None;
        }
    }
    let is_oid = |oid: &Option<String>| {
        oid.as_ref()
            .is_none_or(|oid| oid.len() >= 7 && oid.chars().all(|c| c.is_ascii_hexdigit()))
    };
    if old.is_none() && new.is_none() || !is_oid(&old) || !is_oid(&new) {
        return None;
    }
    // A checkout with only uncommitted changes has no mode line.
    if !gitlink_mode && !(dirty && old.is_some() && old == new) {
        return None;
    }
    Some(Gitlink {
        path: new_path.or(old_path)?,
        old,
        new,
        dirty,
    })
}

/// The entry Git would show for a checkout at `head` with local changes.
fn gitlink_chunk(path: &str, head: &str) -> String {
    format!(
        "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n\
         -Subproject commit {head}\n+Subproject commit {head}-dirty\n"
    )
}

/// The commit checked out in a submodule's own repository.
fn checkout_head(checkout: &Path) -> Option<String> {
    if !is_checkout(checkout) {
        return None;
    }
    let head = git(checkout, ["rev-parse", "--verify", "--quiet", "HEAD"]).ok()?;
    Some(head.trim().to_string()).filter(|head| !head.is_empty())
}

/// An initialized submodule has its own `.git`; an empty directory would
/// make Git find the superproject instead.
fn is_checkout(checkout: &Path) -> bool {
    checkout.join(".git").exists()
}

/// The submodule's commits and its own diff, prefixed with its path.
fn inspect(
    worktree: &Path,
    gitlink: Gitlink,
    working_tree: bool,
    flags: SubmoduleFlags,
) -> (SubmoduleChange, String) {
    let mut change = SubmoduleChange {
        path: gitlink.path,
        old: gitlink.old,
        new: gitlink.new,
        modified: gitlink.dirty || flags.modified,
        untracked: flags.untracked,
        commits: Vec::new(),
        added_commits: 0,
        removed_commits: 0,
        inline: Inline::Shown,
        files: Vec::new(),
    };
    let (Some(old), Some(new)) = (change.old.clone(), change.new.clone()) else {
        change.inline = Inline::AddedOrRemoved;
        return (change, String::new());
    };
    let checkout = worktree.join(&change.path);
    if !is_checkout(&checkout) {
        change.inline = Inline::NotCheckedOut;
        return (change, String::new());
    }
    if old != new {
        match commits(&checkout, &old, &new) {
            Some((commits, added, removed)) => {
                change.commits = commits;
                change.added_commits = added;
                change.removed_commits = removed;
            }
            None => {
                change.inline = Inline::CommitsMissing;
                return (change, String::new());
            }
        }
    }
    let range: Vec<&str> = if working_tree {
        vec![&old]
    } else {
        vec![&old, &new]
    };
    let inlined = inner_files(&checkout, &range, working_tree).and_then(|(files, untracked)| {
        let total = files.len() + untracked.len();
        if total > MAX_INLINE_FILES {
            return Ok(Err(total));
        }
        inner_patch(worktree, &change.path, &range, files, &untracked).map(Ok)
    });
    match inlined {
        Ok(Ok((files, patch))) => {
            change.files = files;
            (change, patch)
        }
        Ok(Err(total)) => {
            change.inline = Inline::TooLarge(total);
            (change, String::new())
        }
        Err(error) => {
            change.inline = Inline::Failed(format!("{error:#}"));
            (change, String::new())
        }
    }
}

/// Commits only `new` has (added) and only `old` has (removed), newest first.
fn commits(checkout: &Path, old: &str, new: &str) -> Option<(Vec<SubmoduleCommit>, usize, usize)> {
    let range = format!("{old}...{new}");
    let out = git(
        checkout,
        [
            "log",
            "--left-right",
            "--no-show-signature",
            "--format=%m%x1f%H%x1f%s",
            &range,
            "--",
        ],
    )
    .ok()?;
    let (mut added, mut removed) = (0, 0);
    let mut commits = Vec::new();
    for line in out.lines() {
        let mut fields = line.splitn(3, '\x1f');
        let (Some(side), Some(sha), Some(subject)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let is_removed = side == "<";
        if is_removed {
            removed += 1;
        } else {
            added += 1;
        }
        if commits.len() < MAX_COMMITS {
            commits.push(SubmoduleCommit {
                sha: sha.to_string(),
                subject: subject.to_string(),
                removed: is_removed,
            });
        }
    }
    Some((commits, added, removed))
}

/// What a submodule's diff over `range` touches: changed tracked files
/// (paths inside the submodule) and, against its working tree, untracked ones.
fn inner_files(
    checkout: &Path,
    range: &[&str],
    working_tree: bool,
) -> anyhow::Result<(Vec<FileChange>, Vec<String>)> {
    let mut args = vec!["diff", "--name-status", "-z", "-M", "--submodule=short"];
    args.extend(range);
    args.push("--");
    let names = git_bytes(checkout, &args)?;
    let files = crate::comparison::parse_name_status(&String::from_utf8_lossy(&names));
    let mut untracked = Vec::new();
    if working_tree {
        let out = git_bytes(
            checkout,
            ["ls-files", "-z", "--others", "--exclude-standard"],
        )?;
        untracked = String::from_utf8_lossy(&out)
            .split('\0')
            .filter(|path| !path.is_empty())
            .take(MAX_UNTRACKED_FILES)
            .map(str::to_string)
            .collect();
    }
    Ok((files, untracked))
}

/// A submodule's diff over `range` with its paths under `path`, followed by
/// its untracked files as new-file patches; and the files it shows.
fn inner_patch(
    worktree: &Path,
    path: &str,
    range: &[&str],
    mut files: Vec<FileChange>,
    untracked: &[String],
) -> anyhow::Result<(Vec<FileChange>, String)> {
    let (src, dst) = (
        format!("--src-prefix=a/{path}/"),
        format!("--dst-prefix=b/{path}/"),
    );
    let mut args = vec![
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--submodule=short",
        "-M",
        &src,
        &dst,
    ];
    args.extend(range);
    args.push("--");
    let mut patch = git(&worktree.join(path), &args)?;
    for file in &mut files {
        file.path = format!("{path}/{}", file.path);
        if let Some(orig) = &mut file.orig_path {
            *orig = format!("{path}/{orig}");
        }
    }
    for rel in untracked {
        let full = format!("{path}/{rel}");
        append_new_file_patch(&mut patch, worktree, &full);
        files.push(FileChange {
            path: full,
            unstaged: Some(FileState::Untracked),
            ..FileChange::default()
        });
    }
    Ok((files, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "ec97fb1a7529586bca9bb734c22f88c0a235537a";
    const NEW: &str = "d45142b1a80fee4801eb65482ed3e72930b66e79";

    #[test]
    fn parses_status_flags() {
        assert_eq!(SubmoduleFlags::parse("N..."), None);
        assert_eq!(
            SubmoduleFlags::parse("SC.U"),
            Some(SubmoduleFlags {
                commit_changed: true,
                modified: false,
                untracked: true,
            })
        );
    }

    #[test]
    fn finds_submodule_entries() {
        let updated = format!(
            "diff --git a/vendor/lib b/vendor/lib\nindex ec97fb1..d45142b 160000\n--- a/vendor/lib\n+++ b/vendor/lib\n\
             @@ -1 +1 @@\n-Subproject commit {OLD}\n+Subproject commit {NEW}-dirty\n"
        );
        assert_eq!(
            parse_gitlink(&updated),
            Some(Gitlink {
                path: "vendor/lib".into(),
                old: Some(OLD.into()),
                new: Some(NEW.into()),
                dirty: true,
            })
        );
        let added = format!(
            "diff --git a/lib b/lib\nnew file mode 160000\nindex 0000000..d45142b\n--- /dev/null\n+++ b/lib\n\
             @@ -0,0 +1 @@\n+Subproject commit {NEW}\n"
        );
        let added = parse_gitlink(&added).unwrap();
        assert_eq!((added.path.as_str(), added.old), ("lib", None));
        // Uncommitted changes only: no mode line, the same commit twice.
        let dirty = gitlink_chunk("сабмодуль", OLD);
        assert_eq!(parse_gitlink(&dirty).unwrap().path, "сабмодуль");
        // A text file that happens to mention a submodule is not one.
        let text = format!(
            "diff --git a/notes.txt b/notes.txt\nindex 1..2 100644\n--- a/notes.txt\n+++ b/notes.txt\n\
             @@ -1 +1 @@\n-Subproject commit {OLD}\n+Subproject commit {NEW}\n"
        );
        assert_eq!(parse_gitlink(&text), None);
    }

    #[test]
    fn splits_and_names_chunks() {
        let patch = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-x\n+y\n\
                     diff --git a/bin.png b/bin.png\nindex 1..2 100644\nBinary files a/bin.png and b/bin.png differ\n\
                     diff --git \"a/\\320\\271 x\" \"b/\\320\\271 x\"\n--- \"a/\\320\\271 x\"\n+++ \"b/\\320\\271 x\"\n";
        let chunks = split_chunks(patch);
        assert_eq!(chunks.len(), 3);
        let paths: Vec<_> = chunks.iter().map(|chunk| chunk_path(chunk)).collect();
        assert_eq!(
            paths,
            [
                Some("a.txt".to_string()),
                Some("bin.png".to_string()),
                Some("й x".to_string())
            ]
        );
    }

    #[test]
    fn puts_submodule_files_after_their_entry() {
        let file = |path: &str| FileChange {
            path: path.into(),
            unstaged: Some(FileState::Modified),
            ..FileChange::default()
        };
        let submodule = SubmoduleChange {
            path: "lib".into(),
            old: None,
            new: None,
            modified: false,
            untracked: false,
            commits: Vec::new(),
            added_commits: 0,
            removed_commits: 0,
            inline: Inline::Shown,
            files: vec![file("lib/a.rs"), file("lib/b.rs")],
        };
        let files = with_submodule_files(
            vec![file("a.rs"), file("lib"), file("z.rs")],
            std::slice::from_ref(&submodule),
        );
        let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.rs", "lib", "lib/a.rs", "lib/b.rs", "z.rs"]);
        assert!(submodule.contains("lib/a.rs") && !submodule.contains("library.rs"));
    }
}
