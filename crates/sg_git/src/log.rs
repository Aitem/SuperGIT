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
