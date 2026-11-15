use std::path::Path;

use anyhow::Result;

use crate::{GitError, git};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    /// Author time, unix seconds.
    pub time: i64,
    pub subject: String,
    /// Decorations such as `HEAD -> main`, `origin/main`, `tag: v1`.
    pub refs: Vec<String>,
    /// `Co-Authored-By: Claude …` trailer present — written with an agent.
    pub by_agent: bool,
}

impl Commit {
    pub fn short_sha(&self) -> &str {
        &self.sha[..self.sha.len().min(8)]
    }
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}

const FIELD: char = '\u{1f}';
const RECORD: char = '\u{1e}';

/// Commits reachable from `rev` (e.g. `HEAD`, a branch, or `--all`).
pub fn log(path: &Path, rev: &str, limit: usize) -> Result<Vec<Commit>> {
    let format = format!(
        "--format=%H{FIELD}%P{FIELD}%an{FIELD}%ae{FIELD}%at{FIELD}%D{FIELD}%s{FIELD}%(trailers:key=Co-authored-by,valueonly,separator=%x2C){RECORD}"
    );
    let limit = format!("-n{limit}");
    let out = match git(path, ["log", &format, &limit, "--date-order", rev, "--"]) {
        Ok(out) => out,
        Err(e) => {
            // Unborn branch (no commits yet): an empty history, not an error.
            if rev == "HEAD"
                && let Ok(branch) = git(path, ["symbolic-ref", "--quiet", "HEAD"])
                && let Err(missing) = git(path, ["show-ref", "--verify", "--quiet", branch.trim()])
                && missing
                    .downcast_ref::<GitError>()
                    .is_some_and(|e| e.code == Some(1))
            {
                return Ok(Vec::new());
            }
            return Err(e);
        }
    };
    Ok(parse_log(&out))
}

fn parse_log(out: &str) -> Vec<Commit> {
    out.split(RECORD)
        .filter_map(|rec| {
            let rec = rec.trim_start_matches('\n');
            if rec.is_empty() {
                return None;
            }
            let f: Vec<&str> = rec.split(FIELD).collect();
            if f.len() < 8 {
                return None;
            }
            Some(Commit {
                sha: f[0].into(),
                parents: f[1].split_whitespace().map(Into::into).collect(),
                author: f[2].into(),
                email: f[3].into(),
                time: f[4].trim().parse().unwrap_or(0),
                refs: f[5]
                    .split(", ")
                    .filter(|r| !r.is_empty())
                    .map(Into::into)
                    .collect(),
                subject: f[6].into(),
                by_agent: f[7].to_ascii_lowercase().contains("claude"),
            })
        })
        .collect()
}

/// One commit with what its card shows beyond the list row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitDetails {
    pub commit: Commit,
    /// The message after the subject line, trailers included.
    pub body: String,
    /// Author date in local time, `2026-11-15 17:52`.
    pub date: String,
    pub committer: String,
    pub committer_email: String,
}

/// The commit `sha` with its full message, for the commit card.
pub fn commit_details(path: &Path, sha: &str) -> Result<CommitDetails> {
    let commit = log(path, sha, 1)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("commit {sha} not found"))?;
    let format = format!("--format=%ad{FIELD}%cn{FIELD}%ce{FIELD}%b");
    let out = git(
        path,
        [
            "show",
            "-s",
            "--date=format-local:%Y-%m-%d %H:%M",
            &format,
            sha,
            "--",
        ],
    )?;
    let mut fields = out.splitn(4, FIELD);
    let mut next = || fields.next().unwrap_or_default().to_string();
    let (date, committer, committer_email, body) = (next(), next(), next(), next());
    Ok(CommitDetails {
        commit,
        body: body.trim_end().to_string(),
        date: date.trim().to_string(),
        committer,
        committer_email,
    })
}

/// How far a worktree's branch has gone since it forked from the base branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fork {
    /// What HEAD is compared with: `origin/main`, or a local `main` when
    /// there is no remote.
    pub base: String,
    /// Commits on HEAD that the base does not have.
    pub commits: usize,
}

/// The branch new work forks from: the remote's default (`origin/HEAD`), else
/// the first of `origin/main`, `origin/master`, `main`, `master` that exists.
pub fn base_branch(path: &Path) -> Option<String> {
    if let Ok(out) = git(
        path,
        [
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) && !out.trim().is_empty()
    {
        return Some(out.trim().to_string());
    }
    [
        "refs/remotes/origin/main",
        "refs/remotes/origin/master",
        "refs/heads/main",
        "refs/heads/master",
    ]
    .into_iter()
    .find(|r| git(path, ["show-ref", "--verify", "--quiet", r]).is_ok())
    .map(|r| {
        r.trim_start_matches("refs/remotes/")
            .trim_start_matches("refs/heads/")
            .to_string()
    })
}

/// Commits on HEAD since it forked from [`base_branch`]. `None` when there is
/// no base branch, or HEAD is the base branch itself (the main checkout).
pub fn fork(path: &Path) -> Result<Option<Fork>> {
    let Some(base) = base_branch(path) else {
        return Ok(None);
    };
    let head = git(path, ["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
    let base_name = base.strip_prefix("origin/").unwrap_or(&base);
    if head.as_deref().map(str::trim) == Some(base_name) {
        return Ok(None);
    }
    let count = git(
        path,
        ["rev-list", "--count", &format!("{base}..HEAD"), "--"],
    )?;
    Ok(Some(Fork {
        base,
        commits: count.trim().parse()?,
    }))
}

/// The patch introduced by a commit (against its first parent).
pub fn commit_patch(path: &Path, sha: &str) -> Result<String> {
    git(
        path,
        [
            "show",
            "--format=",
            "--patch",
            "-M",
            "--no-color",
            "--no-ext-diff",
            "--first-parent",
            sha,
            "--",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_log_records() {
        let out = format!(
            "aaa{FIELD}p1 p2{FIELD}Ann{FIELD}a@x{FIELD}1700000000{FIELD}HEAD -> main, origin/main{FIELD}Merge it{FIELD}Claude Opus <noreply@anthropic.com>{RECORD}\n\
             bbb{FIELD}{FIELD}Bob{FIELD}b@x{FIELD}1690000000{FIELD}{FIELD}init{FIELD}{RECORD}\n"
        );
        let commits = parse_log(&out);
        assert_eq!(commits.len(), 2);
        assert!(commits[0].is_merge());
        assert!(commits[0].by_agent);
        assert_eq!(commits[0].refs, vec!["HEAD -> main", "origin/main"]);
        assert!(commits[1].parents.is_empty());
        assert!(!commits[1].by_agent);
    }
}
