//! Explicit index edits and local history operations for a single worktree.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};

use crate::rebase::unfinished_operation;
use crate::runner::{git_bytes_with_env_and_input, git_bytes_with_input, git_timeout};
use crate::{FileChange, git, git_bytes, status};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexEdit {
    Stage,
    Unstage,
}

pub fn edit_file(worktree: &Path, file: &FileChange, edit: IndexEdit) -> Result<()> {
    let mut args = match edit {
        IndexEdit::Stage => vec!["--literal-pathspecs", "add", "-A", "--"],
        IndexEdit::Unstage => vec!["--literal-pathspecs", "reset", "-q", "HEAD", "--"],
    };
    args.push(&file.path);
    if let Some(original) = &file.orig_path
        && match edit {
            IndexEdit::Stage => file.unstaged == Some(crate::FileState::Renamed),
            IndexEdit::Unstage => file.staged == Some(crate::FileState::Renamed),
        }
    {
        args.push(original);
    }
    git(worktree, args).map(drop)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexHunk {
    pub text: String,
    patch: String,
}

/// Read the exact index/worktree comparison used for a subsequent edit.
pub fn index_hunks(worktree: &Path, path: &str, edit: IndexEdit) -> Result<Vec<IndexHunk>> {
    let mut args = vec![
        "--literal-pathspecs",
        "-c",
        "core.quotePath=true",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--submodule=short",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--unified=3",
        "--no-indent-heuristic",
        "--no-relative",
        "--output-indicator-new=+",
        "--output-indicator-old=-",
        "--output-indicator-context= ",
    ];
    if edit == IndexEdit::Unstage {
        args.push("--cached");
    }
    args.extend(["--", path]);
    let raw = git_bytes(worktree, args)?;
    let patch =
        String::from_utf8(raw).context("Use file staging for a diff with non-UTF-8 content")?;
    let mut lines = patch.split_inclusive('\n').peekable();
    let mut header = String::new();
    while let Some(line) = lines.next_if(|line| !line.starts_with("@@ ")) {
        if line.contains("mode 160000") || line.trim_end().ends_with(" 160000") {
            return Ok(Vec::new());
        }
        // Keep creation/deletion records, but leave a separate chmod change
        // alone when applying individual text hunks.
        if !["old mode ", "new mode ", "index "]
            .iter()
            .any(|prefix| line.starts_with(prefix))
        {
            header.push_str(line);
        }
    }
    let mut hunks = Vec::new();
    while let Some(start) = lines.next() {
        let mut text = start.to_string();
        while let Some(line) = lines.next_if(|line| !line.starts_with("@@ ")) {
            text.push_str(line);
        }
        let patch = format!("{header}{text}");
        hunks.push(IndexHunk { text, patch });
    }
    Ok(hunks)
}

pub fn edit_hunk(worktree: &Path, path: &str, hunk: &IndexHunk, edit: IndexEdit) -> Result<()> {
    ensure!(
        index_hunks(worktree, path, edit)?.contains(hunk),
        "This hunk changed since it was displayed. Review the refreshed diff and try again"
    );
    let mut args = vec!["apply", "--cached", "--whitespace=nowarn"];
    if edit == IndexEdit::Unstage {
        args.push("--reverse");
    }
    git_bytes_with_input(worktree, &args, hunk.patch.as_bytes().to_vec()).map(drop)
}

/// Commit the index only. Git runs the usual commit hooks and signing.
pub fn commit(
    worktree: &Path,
    branch: Option<&str>,
    head: Option<&str>,
    message: &str,
    amend: bool,
) -> Result<()> {
    let state = status(worktree)?;
    ensure!(
        state.branch.as_deref() == branch && state.head_oid.as_deref() == head,
        "HEAD moved. Review the current worktree and try again"
    );
    if amend {
        ensure!(head.is_some(), "There is no commit to amend");
        ensure_no_operation(worktree)?;
    } else {
        ensure!(!message.trim().is_empty(), "Enter a commit message");
        // Git also permits a resolved merge commit with the same tree.
        let merge = git(worktree, ["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]).is_ok();
        ensure!(
            state.staged_count() > 0 || merge,
            "Stage changes before committing"
        );
    }
    let mut args = vec!["commit"];
    if amend {
        args.push("--amend");
    }
    if amend && message.trim().is_empty() {
        args.push("--no-edit");
        return git(worktree, args).map(drop);
    }
    args.push("--file=-");
    git_bytes_with_input(worktree, &args, message.as_bytes().to_vec()).map(drop)
}

fn ensure_no_operation(worktree: &Path) -> Result<()> {
    if let Some(marker) = unfinished_operation(worktree)? {
        anyhow::bail!("The worktree has an unfinished Git operation ({marker})");
    }
    Ok(())
}

/// Collapse a contiguous first-parent range ending at HEAD. The new commit
/// has exactly HEAD's tree, and the branch moves with a compare-and-swap.
pub fn squash(
    worktree: &Path,
    branch: &str,
    head: &str,
    count: usize,
    message: &str,
) -> Result<()> {
    ensure!(count >= 2, "Choose at least two commits");
    ensure!(!message.trim().is_empty(), "Enter a commit message");
    ensure_no_operation(worktree)?;
    let reference = format!("refs/heads/{branch}");
    ensure!(
        git(worktree, ["symbolic-ref", "--quiet", "HEAD"])?.trim() == reference
            && git(worktree, ["rev-parse", "HEAD"])?.trim() == head,
        "HEAD moved. Review the commits and try again"
    );
    let limit = format!("--max-count={count}");
    let history = git(
        worktree,
        ["rev-list", "--first-parent", "--parents", &limit, "HEAD"],
    )?;
    let commits: Vec<_> = history.lines().collect();
    ensure!(
        commits.len() == count,
        "There are fewer than {count} commits"
    );
    ensure!(
        commits
            .iter()
            .all(|line| line.split_whitespace().count() <= 2),
        "The selected range contains a merge commit"
    );
    let mut oldest = commits[count - 1].split_whitespace();
    let first = oldest.next().context("Missing squash starting commit")?;
    let parent = oldest.next();
    let author = git(
        worktree,
        ["show", "-s", "--format=%an%x00%ae%x00%aI", first, "--"],
    )?;
    let mut fields = author.trim_end_matches('\n').split('\0');
    let identity = [
        (
            "GIT_AUTHOR_NAME",
            fields.next().context("Missing author name")?,
        ),
        (
            "GIT_AUTHOR_EMAIL",
            fields.next().context("Missing author email")?,
        ),
        (
            "GIT_AUTHOR_DATE",
            fields.next().context("Missing author date")?,
        ),
    ];
    let tree = format!("{head}^{{tree}}");
    let mut args = vec!["commit-tree", &tree];
    if let Some(parent) = parent {
        args.extend(["-p", parent]);
    }
    if git(worktree, ["config", "--bool", "--get", "commit.gpgSign"])
        .is_ok_and(|s| s.trim() == "true")
    {
        args.push("-S");
    }
    args.extend(["-F", "-"]);
    let oid =
        git_bytes_with_env_and_input(worktree, &args, &identity, message.as_bytes().to_vec())?;
    let oid = std::str::from_utf8(&oid)?.trim();
    ensure_no_operation(worktree)?;
    ensure!(
        git(worktree, ["symbolic-ref", "--quiet", "HEAD"])?.trim() == reference,
        "The worktree switched branches. Review the commits and try again"
    );
    git(
        worktree,
        [
            "update-ref",
            "-m",
            "supergit: squash",
            &reference,
            oid,
            head,
        ],
    )
    .map(drop)
}

const NETWORK_TIMEOUT: Duration = Duration::from_secs(90);

pub fn fetch(worktree: &Path) -> Result<()> {
    git_timeout(worktree, &["fetch", "--all"], NETWORK_TIMEOUT)
}

pub fn pull(worktree: &Path, branch: &str) -> Result<()> {
    let state = status(worktree)?;
    ensure!(
        state.branch.as_deref() == Some(branch),
        "The worktree switched branches"
    );
    ensure!(
        state.upstream.is_some(),
        "Configure an upstream before pulling"
    );
    ensure_no_operation(worktree)?;
    git_timeout(
        worktree,
        &["pull", "--ff-only", "--no-rebase", "--no-autostash"],
        NETWORK_TIMEOUT,
    )
}
