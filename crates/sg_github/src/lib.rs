//! GitHub access through the `gh` CLI: the user's existing auth, no tokens to
//! manage. Every call runs in a repository directory so `gh` resolves the
//! remote itself.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Result, bail};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

impl PrState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Draft => "draft",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Checks {
    pub passed: u32,
    pub failed: u32,
    pub pending: u32,
}

impl Checks {
    pub fn total(&self) -> u32 {
        self.passed + self.failed + self.pending
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub head_branch: String,
    pub base_branch: String,
    pub state: PrState,
    pub url: String,
    pub author: String,
    /// `APPROVED`, `CHANGES_REQUESTED`, `REVIEW_REQUIRED` or empty.
    pub review_decision: String,
    pub checks: Checks,
    pub additions: u64,
    pub deletions: u64,
    pub updated_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPr {
    number: u64,
    title: String,
    head_ref_name: String,
    base_ref_name: String,
    state: String,
    is_draft: bool,
    url: String,
    author: Option<RawAuthor>,
    review_decision: Option<String>,
    status_check_rollup: Option<Vec<RawCheck>>,
    additions: Option<u64>,
    deletions: Option<u64>,
    updated_at: String,
}

#[derive(Deserialize)]
struct RawAuthor {
    login: String,
}

#[derive(Deserialize)]
struct RawCheck {
    // CheckRun
    status: Option<String>,
    conclusion: Option<String>,
    // StatusContext
    state: Option<String>,
}

const FIELDS: &str = "number,title,headRefName,baseRefName,state,isDraft,url,author,reviewDecision,statusCheckRollup,additions,deletions,updatedAt";

/// Whether `gh` is installed and on PATH.
pub fn gh_available() -> bool {
    Command::new("gh")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Open pull requests plus the most recently merged ones. Two narrow queries
/// run in parallel: `--state all` with check rollups is several times slower.
pub fn list_prs(repo: &Path, open_limit: usize, merged_limit: usize) -> Result<Vec<PullRequest>> {
    let (open, merged) = std::thread::scope(|s| {
        let open = s.spawn(|| gh_pr_list(repo, "open", open_limit));
        let merged = s.spawn(|| gh_pr_list(repo, "merged", merged_limit));
        (join(open), join(merged))
    });
    let mut prs = open?;
    // Merged PRs are best-effort: a failure there should not hide open ones.
    if let Ok(merged) = merged {
        prs.extend(
            merged
                .into_iter()
                .filter(|m| !prs.iter().any(|p| p.number == m.number))
                .collect::<Vec<_>>(),
        );
    }
    Ok(prs)
}

fn join(
    h: std::thread::ScopedJoinHandle<'_, Result<Vec<PullRequest>>>,
) -> Result<Vec<PullRequest>> {
    h.join().unwrap_or_else(|_| bail!("gh thread panicked"))
}

fn gh_pr_list(repo: &Path, state: &str, limit: usize) -> Result<Vec<PullRequest>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let out = Command::new("gh")
        .args(["pr", "list", "--state", state, "--limit"])
        .arg(limit.to_string())
        .args(["--json", FIELDS])
        .current_dir(repo)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        bail!(
            "gh pr list: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_prs(&out.stdout)
}

fn parse_prs(json: &[u8]) -> Result<Vec<PullRequest>> {
    let raw: Vec<RawPr> = serde_json::from_slice(json)?;
    let mut prs: Vec<PullRequest> = raw.into_iter().map(PullRequest::from).collect();
    // Open/draft PRs first; `gh` already orders by recency within that.
    prs.sort_by_key(|pr| !matches!(pr.state, PrState::Open | PrState::Draft));
    Ok(prs)
}

impl From<RawPr> for PullRequest {
    fn from(raw: RawPr) -> Self {
        let state = match raw.state.as_str() {
            "MERGED" => PrState::Merged,
            "CLOSED" => PrState::Closed,
            _ if raw.is_draft => PrState::Draft,
            _ => PrState::Open,
        };
        let mut checks = Checks::default();
        for c in raw.status_check_rollup.unwrap_or_default() {
            let verdict = c
                .conclusion
                .filter(|s| !s.is_empty())
                .or(c.state)
                .unwrap_or_default();
            let running = c.status.as_deref().is_some_and(|s| s != "COMPLETED");
            match verdict.as_str() {
                _ if running => checks.pending += 1,
                "SUCCESS" | "NEUTRAL" | "SKIPPED" => checks.passed += 1,
                "PENDING" | "EXPECTED" | "QUEUED" | "IN_PROGRESS" | "" => checks.pending += 1,
                _ => checks.failed += 1,
            }
        }
        Self {
            number: raw.number,
            title: raw.title,
            head_branch: raw.head_ref_name,
            base_branch: raw.base_ref_name,
            state,
            url: raw.url,
            author: raw.author.map(|a| a.login).unwrap_or_default(),
            review_decision: raw.review_decision.unwrap_or_default(),
            checks,
            additions: raw.additions.unwrap_or(0),
            deletions: raw.deletions.unwrap_or(0),
            updated_at: raw.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gh_json() {
        let json = br#"[
          {"number":7,"title":"Merged one","headRefName":"a","baseRefName":"main","state":"MERGED","isDraft":false,
           "url":"u","author":{"login":"x"},"reviewDecision":"APPROVED","statusCheckRollup":[],"additions":1,"deletions":2,"updatedAt":"t"},
          {"number":8,"title":"Open one","headRefName":"b","baseRefName":"main","state":"OPEN","isDraft":true,
           "url":"u","author":{"login":"y"},"reviewDecision":"","statusCheckRollup":[
             {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"},
             {"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE"},
             {"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":""},
             {"__typename":"StatusContext","state":"PENDING"}],
           "additions":3,"deletions":4,"updatedAt":"t"}]"#;
        let prs = parse_prs(json).unwrap();
        assert_eq!(prs[0].number, 8, "open PRs sort first");
        assert_eq!(prs[0].state, PrState::Draft);
        assert_eq!(
            prs[0].checks,
            Checks {
                passed: 1,
                failed: 1,
                pending: 2
            }
        );
        assert_eq!(prs[1].state, PrState::Merged);
    }
}
