use std::path::Path;

use anyhow::Result;

use crate::status::PATCH_OPTIONS;
use crate::submodule::{Patch, inline_submodules};
use crate::{GitError, git, git_bytes};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    /// Author time, unix seconds.
    pub time: i64,
    pub subject: String,
    /// Branches, remote branches and tags pointing at this commit.
    pub refs: Vec<RefLabel>,
    /// The coding agent its `Co-authored-by` trailer names, if any.
    pub agent: Option<CommitAgent>,
}

/// A coding agent credited as co-author of a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitAgent {
    Claude,
    Codex,
}

impl CommitAgent {
    /// From the `Co-authored-by` trailer values: `Claude Opus …`, or Codex
    /// by name or by OpenAI.
    fn from_trailers(trailers: &str) -> Option<Self> {
        let trailers = trailers.to_ascii_lowercase();
        if trailers.contains("claude") {
            Some(Self::Claude)
        } else if trailers.contains("codex") || trailers.contains("openai") {
            Some(Self::Codex)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RefKind {
    /// A detached `HEAD`.
    Head,
    Branch,
    /// A remote-tracking branch such as `origin/main`.
    Remote,
    Tag,
}

/// One decoration of a commit, classified from its full ref name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefLabel {
    pub kind: RefKind,
    /// Short name: `main`, `origin/main`, `v1.0`, `HEAD`.
    pub name: String,
    /// The worktree's checked-out branch, or its detached `HEAD`.
    pub current: bool,
}

impl RefLabel {
    /// `origin/main` → `main`; other kinds keep their name.
    pub fn branch_name(&self) -> &str {
        match self.kind {
            RefKind::Remote => self.name.split_once('/').map_or(&self.name, |(_, b)| b),
            _ => &self.name,
        }
    }
}

/// `%D` with `--decorate=full`: `HEAD -> refs/heads/main, tag: refs/tags/v1,
/// refs/remotes/origin/main`. Symbolic remote heads, stashes and notes are
/// left out.
fn parse_refs(field: &str) -> Vec<RefLabel> {
    field
        .split(", ")
        .filter_map(|decoration| {
            let label = |kind, name: &str, current| RefLabel {
                kind,
                name: name.to_string(),
                current,
            };
            if decoration == "HEAD" {
                return Some(label(RefKind::Head, "HEAD", true));
            }
            if let Some(branch) = decoration.strip_prefix("HEAD -> refs/heads/") {
                return Some(label(RefKind::Branch, branch, true));
            }
            if let Some(tag) = decoration.strip_prefix("tag: refs/tags/") {
                return Some(label(RefKind::Tag, tag, false));
            }
            if let Some(branch) = decoration.strip_prefix("refs/heads/") {
                return Some(label(RefKind::Branch, branch, false));
            }
            decoration
                .strip_prefix("refs/remotes/")
                .filter(|remote| !remote.ends_with("/HEAD"))
                .map(|remote| label(RefKind::Remote, remote, false))
        })
        .collect()
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

/// What the History tab shows: HEAD plus every local branch, remote branch
/// and tag, newest first, so other branches and the base are visible too.
pub fn history(path: &Path, limit: usize) -> Result<Vec<Commit>> {
    let mut revs = vec!["--branches", "--remotes", "--tags"];
    if git(path, ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]).is_ok() {
        revs.insert(0, "HEAD");
    }
    log(path, &revs, limit)
}

/// The refs that supply history, decorations and replacement history. Include
/// symbolic targets and the checked out branch even when their OIDs agree.
pub fn history_refs(path: &Path) -> Result<Vec<u8>> {
    #[cfg(feature = "git2-probe")]
    if crate::use_git2_probe()
        && let Ok(refs) = crate::native_refs::read(path)
    {
        return Ok(refs);
    }
    history_refs_cli(path)
}

pub(crate) fn history_refs_cli(path: &Path) -> Result<Vec<u8>> {
    git_bytes(
        path,
        [
            "for-each-ref",
            "--sort=refname",
            "--format=%(HEAD)%00%(refname)%00%(objectname)%00%(symref)",
            "refs/heads/",
            "refs/remotes/",
            "refs/tags/",
            "refs/replace/",
        ],
    )
}

/// Commits reachable from `revs` (e.g. `["HEAD"]`, a branch, `["--all"]`).
pub fn log(path: &Path, revs: &[&str], limit: usize) -> Result<Vec<Commit>> {
    let format = format!(
        "--format=%H{FIELD}%P{FIELD}%an{FIELD}%ae{FIELD}%at{FIELD}%D{FIELD}%s{FIELD}%(trailers:key=Co-authored-by,valueonly,separator=%x2C){RECORD}"
    );
    let limit = format!("-n{limit}");
    let mut args = vec!["log", &format, &limit, "--date-order", "--decorate=full"];
    args.extend_from_slice(revs);
    args.push("--");
    let out = match git(path, &args) {
        Ok(out) => out,
        Err(e) => {
            // Unborn branch (no commits yet): an empty history, not an error.
            if revs == ["HEAD"]
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
                refs: parse_refs(f[5]),
                subject: f[6].into(),
                agent: CommitAgent::from_trailers(f[7]),
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
    let commit = log(path, &[sha], 1)?
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
    /// Commits on the base that HEAD does not have.
    pub behind: usize,
    /// The branch HEAD most likely started from, and where it left it.
    pub parent: Option<ForkParent>,
}

/// Git does not record where a branch came from; this is the branch that
/// shares the most history with HEAD (fewest commits of HEAD's own since),
/// preferring the base, then local branches, on ties.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkParent {
    pub name: String,
    /// The fork point: the merge base of HEAD and `name`.
    pub merge_base: String,
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
    let counts = git(
        path,
        [
            "rev-list",
            "--left-right",
            "--count",
            &format!("{base}...HEAD"),
            "--",
        ],
    )?;
    let mut counts = counts.split_whitespace().map(str::parse::<usize>);
    let (Some(behind), Some(commits)) = (counts.next(), counts.next()) else {
        anyhow::bail!("unexpected rev-list output");
    };
    let commits = commits?;
    let parent = if commits == 0 {
        // No work of its own: it sits on the base line.
        git(path, ["rev-parse", "--verify", "HEAD^{commit}"])
            .ok()
            .map(|head| ForkParent {
                name: base.clone(),
                merge_base: head.trim().to_string(),
            })
    } else {
        nearest_branch(path, head.as_deref().map(str::trim), &base)
    };
    Ok(Some(Fork {
        base,
        commits,
        behind: behind?,
        parent,
    }))
}

/// See [`ForkParent`]. Older Git lacks the batched ahead/behind atom.
fn nearest_branch(path: &Path, current: Option<&str>, base: &str) -> Option<ForkParent> {
    let list_refs = |format| {
        git(
            path,
            ["for-each-ref", format, "refs/heads/", "refs/remotes/"],
        )
    };
    let batched =
        list_refs("--format=%(refname)%00%(refname:short)%00%(symref)%00%(ahead-behind:HEAD)");
    let out = batched
        .or_else(|_| list_refs("--format=%(refname)%00%(refname:short)%00%(symref)"))
        .ok()?;
    let base_name = base.split_once('/').map_or(base, |(_, branch)| branch);
    let upstream = git(
        path,
        [
            "rev-parse",
            "--symbolic-full-name",
            "--verify",
            "--quiet",
            "@{upstream}",
        ],
    )
    .ok();
    let upstream = upstream.as_deref().map(str::trim);
    let mut candidates: Vec<_> = out
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\0');
            let (full, short, symref) = (fields.next()?, fields.next()?, fields.next()?);
            let remote = full.starts_with("refs/remotes/");
            let branch = if remote {
                short.split_once('/').map_or(short, |(_, branch)| branch)
            } else {
                short
            };
            // Its own branch and upstream copies are not where it came from.
            if !symref.is_empty() || Some(branch) == current || Some(full) == upstream {
                return None;
            }
            // Commits of HEAD's own since the fork; 0: the ref contains HEAD.
            let own: usize = match fields.next() {
                Some(counts) => counts.split_whitespace().nth(1)?.parse().ok()?,
                None => git(
                    path,
                    ["rev-list", "--count", "HEAD", &format!("^{full}"), "--"],
                )
                .ok()?
                .trim()
                .parse()
                .ok()?,
            };
            let is_base = short == base || (!remote && short == base_name);
            (own > 0).then_some(((own, !is_base, remote, short), full))
        })
        .collect();
    candidates.sort();
    // Commits made on this branch: its reflog minus the creation entry.
    let reflog = |reference: &str| -> Vec<String> {
        git(path, ["log", "-g", "--format=%H", reference, "--"])
            .map(|out| out.lines().map(str::to_string).collect())
            .unwrap_or_default()
    };
    let made_here: std::collections::HashSet<String> = current
        .map(|branch| {
            let mut entries = reflog(&format!("refs/heads/{branch}"));
            entries.pop();
            entries.into_iter().collect()
        })
        .unwrap_or_default();
    candidates.into_iter().find_map(|((_, _, _, name), full)| {
        // A branch created on a commit made here started from this branch
        // (a child), even though it shares more history than the real parent.
        if reflog(full)
            .last()
            .is_some_and(|created| made_here.contains(created))
        {
            return None;
        }
        let merge_base = git(path, ["merge-base", "HEAD", full]).ok()?;
        Some(ForkParent {
            name: name.to_string(),
            merge_base: merge_base.trim().to_string(),
        })
    })
}

/// The patch introduced by a commit (against its first parent).
/// The commit's changes against its first parent, with changed submodules
/// inlined from their checkouts in `path`.
pub fn commit_patch(path: &Path, sha: &str) -> Result<Patch> {
    let mut args = vec!["show", "--format=", "--patch", "--first-parent"];
    args.extend(PATCH_OPTIONS);
    args.extend([sha, "--"]);
    Ok(inline_submodules(path, &git(path, args)?, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_log_records() {
        let out = format!(
            "aaa{FIELD}p1 p2{FIELD}Ann{FIELD}a@x{FIELD}1700000000{FIELD}HEAD -> refs/heads/main, tag: refs/tags/v1.0, refs/remotes/origin/main, refs/remotes/origin/HEAD, refs/heads/feat/x, refs/stash{FIELD}Merge it{FIELD}Claude Opus <noreply@anthropic.com>{RECORD}\n\
             bbb{FIELD}{FIELD}Bob{FIELD}b@x{FIELD}1690000000{FIELD}{FIELD}init{FIELD}{RECORD}\n"
        );
        let commits = parse_log(&out);
        assert_eq!(commits.len(), 2);
        assert!(commits[0].is_merge());
        assert_eq!(commits[0].agent, Some(CommitAgent::Claude));
        assert_eq!(commits[1].agent, None);
        assert_eq!(
            CommitAgent::from_trailers("Codex <noreply@openai.com>"),
            Some(CommitAgent::Codex)
        );
        let label = |kind, name: &str, current| RefLabel {
            kind,
            name: name.into(),
            current,
        };
        assert_eq!(
            commits[0].refs,
            vec![
                label(RefKind::Branch, "main", true),
                label(RefKind::Tag, "v1.0", false),
                label(RefKind::Remote, "origin/main", false),
                label(RefKind::Branch, "feat/x", false),
            ]
        );
        assert_eq!(commits[0].refs[2].branch_name(), "main");
        assert_eq!(parse_refs("HEAD"), vec![label(RefKind::Head, "HEAD", true)]);
        assert!(commits[1].parents.is_empty());
    }
}
