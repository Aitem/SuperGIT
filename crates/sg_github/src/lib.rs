//! GitHub access through the `gh` CLI: the user's existing auth, no tokens to
//! manage. Every call runs in a repository directory so `gh` resolves the
//! remote itself.
#![forbid(unsafe_code)]

mod check_inspection;
mod details;

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Result, bail};
use serde::Deserialize;

pub use check_inspection::{CheckAnnotation, CheckInspection, check_inspection};
pub use details::{
    AutoMerge, Check, CheckStatus, Comment, MergeMethod, MergeStatus, PrCommit, PrDetails, Review,
    ReviewState, ReviewThread, disable_auto_merge, merge_pr, pr_commits, pr_details,
};

mod publish;

pub use publish::{
    DraftPullRequest, PublishBase, PublishRepository, create_draft_pr, open_pr_from_branch,
    publish_repository,
};

/// Run `gh` in `repo` without prompts or colors; stdout on success, its
/// stderr as the error otherwise.
fn gh<I, S>(repo: &Path, args: I) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<S> = args.into_iter().collect();
    let out = Command::new("gh")
        .args(&args)
        .current_dir(repo)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        let command: Vec<_> = args
            .iter()
            .take(2)
            .map(|a| a.as_ref().to_string_lossy())
            .collect();
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stderr = stderr.trim();
        bail!(
            "gh {}: {}",
            command.join(" "),
            if stderr.is_empty() { "failed" } else { stderr }
        );
    }
    Ok(out.stdout)
}

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
    pub created_at: Option<std::time::SystemTime>,
    pub merged_at: Option<std::time::SystemTime>,
    pub number: u64,
    pub title: String,
    pub head_branch: String,
    pub head_oid: String,
    pub head_repository: Option<String>,
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

impl PullRequest {
    /// Open or draft: it can still change and be merged.
    pub fn is_open(&self) -> bool {
        matches!(self.state, PrState::Open | PrState::Draft)
    }

    /// When it last changed on GitHub.
    pub fn updated(&self) -> Option<std::time::SystemTime> {
        parse_timestamp(&self.updated_at)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPr {
    created_at: Option<String>,
    merged_at: Option<String>,
    number: u64,
    title: String,
    head_ref_name: String,
    #[serde(default)]
    head_ref_oid: String,
    head_repository: Option<RawRepository>,
    head_repository_owner: Option<RawAuthor>,
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

/// `gh pr list` leaves `nameWithOwner` empty; `name` plus
/// `headRepositoryOwner` say the same.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRepository {
    #[serde(default)]
    name_with_owner: String,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct RawCheck {
    // CheckRun
    status: Option<String>,
    conclusion: Option<String>,
    // StatusContext
    state: Option<String>,
}

const FIELDS: &str = "number,title,headRefName,headRefOid,headRepository,headRepositoryOwner,baseRefName,state,isDraft,url,author,reviewDecision,statusCheckRollup,additions,deletions,createdAt,mergedAt,updatedAt";

/// A commit author as GitHub knows them: the email on their commits and
/// the account it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Author {
    pub email: String,
    pub login: String,
    pub avatar_url: String,
}

const AUTHORS_QUERY: &str = "query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    defaultBranchRef { target { ... on Commit {
      history(first: 100) { nodes { author { email user { login avatarUrl(size: 64) } } } }
    } } }
  }
}";

/// The accounts behind the default branch's latest commits, one per email.
/// A single GraphQL call; it reads the repository's history and sends no
/// emails anywhere.
pub fn commit_authors(repo: &Path) -> Result<Vec<Author>> {
    let query = format!("query={AUTHORS_QUERY}");
    let out = gh(
        repo,
        [
            "api",
            "graphql",
            "-F",
            "owner={owner}",
            "-F",
            "name={repo}",
            "-f",
            &query,
        ],
    )?;
    parse_authors(&out)
}

fn parse_authors(json: &[u8]) -> Result<Vec<Author>> {
    let value: serde_json::Value = serde_json::from_slice(json)?;
    let nodes = value
        .pointer("/data/repository/defaultBranchRef/target/history/nodes")
        .and_then(|n| n.as_array())
        .cloned()
        .unwrap_or_default();
    let mut authors: Vec<Author> = Vec::new();
    for node in nodes {
        let author = &node["author"];
        let (Some(email), Some(login), Some(url)) = (
            author["email"].as_str(),
            author["user"]["login"].as_str(),
            author["user"]["avatarUrl"].as_str(),
        ) else {
            continue;
        };
        if !authors.iter().any(|a| a.email.eq_ignore_ascii_case(email)) {
            authors.push(Author {
                email: email.to_string(),
                login: login.to_string(),
                avatar_url: url.to_string(),
            });
        }
    }
    Ok(authors)
}

/// GitHub's private commit emails name the account themselves:
/// `123+login@users.noreply.github.com` (or the older `login@…`). Returns
/// the login and an avatar URL without asking GitHub.
pub fn noreply_avatar(email: &str) -> Option<(String, String)> {
    let local = email
        .to_ascii_lowercase()
        .strip_suffix("@users.noreply.github.com")?
        .to_string();
    match local.split_once('+') {
        Some((id, login)) if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) => Some((
            login.to_string(),
            format!("https://avatars.githubusercontent.com/u/{id}?s=64"),
        )),
        _ if !local.is_empty() && !local.contains('+') => Some((
            local.clone(),
            format!("https://github.com/{local}.png?size=64"),
        )),
        _ => None,
    }
}

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
    let limit = limit.to_string();
    let out = gh(
        repo,
        [
            "pr", "list", "--state", state, "--limit", &limit, "--json", FIELDS,
        ],
    )?;
    parse_prs(&out)
}

/// Every pull request (any state) whose head is `branch`: the ones a
/// worktree on that branch produced, beyond the recent ones `list_prs` sees.
pub fn list_branch_prs(repo: &Path, branch: &str, limit: usize) -> Result<Vec<PullRequest>> {
    let limit = limit.to_string();
    let out = gh(
        repo,
        [
            "pr", "list", "--state", "all", "--head", branch, "--limit", &limit, "--json", FIELDS,
        ],
    )?;
    parse_prs(&out)
}

/// GitHub's search over all pull requests (titles, bodies, `author:x`…),
/// most recently updated first unless the query sorts itself.
pub fn search_prs(repo: &Path, query: &str, limit: usize) -> Result<Vec<PullRequest>> {
    let query = if query.contains("sort:") {
        query.to_string()
    } else {
        format!("{query} sort:updated-desc")
    };
    let limit = limit.to_string();
    let out = gh(
        repo,
        [
            "pr", "list", "--state", "all", "--search", &query, "--limit", &limit, "--json", FIELDS,
        ],
    )?;
    parse_prs(&out)
}

/// The pull request's changes as a unified Git patch. GitHub refuses very
/// large ones (over 20 000 lines or 300 files).
pub fn pr_diff(repo: &Path, number: u64) -> Result<String> {
    let number = number.to_string();
    let out = gh(repo, ["pr", "diff", &number, "--color=never"])?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// One pull request by number, in the list's shape.
pub fn view_pr(repo: &Path, number: u64) -> Result<PullRequest> {
    let number = number.to_string();
    let out = gh(repo, ["pr", "view", &number, "--json", FIELDS])?;
    let raw: RawPr = serde_json::from_slice(&out)?;
    Ok(raw.into())
}

/// The head branch lives in the PR's own repository, not a fork: only
/// then can a local branch of the same name be where it came from.
pub fn head_in_same_repository(pr: &PullRequest) -> bool {
    match (&pr.head_repository, pull_request_repository(pr)) {
        (Some(head), Some(repository)) => head.eq_ignore_ascii_case(&repository),
        _ => false,
    }
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
            created_at: raw.created_at.as_deref().and_then(parse_timestamp),
            merged_at: raw.merged_at.as_deref().and_then(parse_timestamp),
            number: raw.number,
            title: raw.title,
            head_branch: raw.head_ref_name,
            head_oid: raw.head_ref_oid,
            head_repository: raw.head_repository.and_then(|repo| {
                if !repo.name_with_owner.is_empty() {
                    Some(repo.name_with_owner)
                } else {
                    let owner = raw.head_repository_owner?.login;
                    (!owner.is_empty() && !repo.name.is_empty())
                        .then(|| format!("{owner}/{}", repo.name))
                }
            }),
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

fn parse_timestamp(value: &str) -> Option<std::time::SystemTime> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(std::time::SystemTime::from)
}

mod issues;
pub use issues::{Issue, IssueReference, view_issue};

/// Recognize canonical GitHub remotes without guessing a repository namespace.
pub fn repository_identity(url: &str) -> Option<String> {
    let path = url
        .trim()
        .strip_prefix("git@github.com:")
        .or_else(|| url.trim().strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.trim().strip_prefix("https://github.com/"))?;
    let path = path
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or(path.trim_end_matches('/'));
    let parts: Vec<_> = path.split('/').collect();
    (parts.len() == 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        }))
    .then(|| path.to_ascii_lowercase())
}

pub fn pull_request_repository(pr: &PullRequest) -> Option<String> {
    let (repository, number) = pr.url.split_once("/pull/")?;
    (number.parse::<u64>().ok()? == pr.number).then_some(())?;
    repository_identity(repository)
}

/// A merged PR is proof only for this repository, branch lifetime and exact tip.
pub fn merged_pr_matches(
    pr: &PullRequest,
    origin: &str,
    branch: &str,
    tip: &str,
    now: std::time::SystemTime,
) -> bool {
    let Some(repository) = repository_identity(origin) else {
        return false;
    };
    pr.state == PrState::Merged
        && pr.head_branch == branch
        && !tip.is_empty()
        && pr.head_oid == tip
        && pull_request_repository(pr).as_deref() == Some(repository.as_str())
        && pr
            .head_repository
            .as_ref()
            .is_some_and(|head| head.eq_ignore_ascii_case(&repository))
        && pr
            .created_at
            .zip(pr.merged_at)
            .is_some_and(|(created, merged)| created <= merged && merged <= now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noreply_emails_name_their_account() {
        assert_eq!(
            noreply_avatar("12345+Octo-Cat@users.noreply.github.com"),
            Some((
                "octo-cat".into(),
                "https://avatars.githubusercontent.com/u/12345?s=64".into()
            ))
        );
        assert_eq!(
            noreply_avatar("octocat@users.noreply.github.com"),
            Some((
                "octocat".into(),
                "https://github.com/octocat.png?size=64".into()
            ))
        );
        assert_eq!(noreply_avatar("someone@example.com"), None);
        assert_eq!(noreply_avatar("x+y@users.noreply.github.com"), None);
    }

    #[test]
    fn authors_come_one_per_email_and_skip_unknown_accounts() {
        let json = br#"{"data":{"repository":{"defaultBranchRef":{"target":{"history":{"nodes":[
            {"author":{"email":"a@x.io","user":{"login":"alice","avatarUrl":"https://av/a"}}},
            {"author":{"email":"A@X.io","user":{"login":"alice","avatarUrl":"https://av/a"}}},
            {"author":{"email":"ghost@x.io","user":null}},
            {"author":{"email":"b@x.io","user":{"login":"bob","avatarUrl":"https://av/b"}}}
        ]}}}}}}"#;
        let authors = parse_authors(json).unwrap();
        assert_eq!(
            authors.iter().map(|a| a.login.as_str()).collect::<Vec<_>>(),
            ["alice", "bob"]
        );
        assert!(
            parse_authors(br#"{"data":{"repository":null}}"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn automatic_cleanup_requires_exact_repository_tip_and_merge_time() {
        let mut pr = parse_prs(br#"[{"number":7,"title":"Task","headRefName":"task","headRefOid":"abc","headRepository":{"nameWithOwner":"Owner/Repo"},"baseRefName":"main","state":"MERGED","isDraft":false,"url":"https://github.com/owner/repo/pull/7","createdAt":"2025-01-01T00:00:00Z","mergedAt":"2025-01-02T00:00:00Z","updatedAt":"2025-01-02T00:00:00Z"}]"#).unwrap().remove(0);
        let now = parse_timestamp("2025-01-03T00:00:00Z").unwrap();
        assert!(merged_pr_matches(
            &pr,
            "git@github.com:owner/repo.git",
            "task",
            "abc",
            now
        ));
        assert!(!merged_pr_matches(
            &pr,
            "git@github.com:other/repo.git",
            "task",
            "abc",
            now
        ));
        assert!(!merged_pr_matches(
            &pr,
            "git@github.com:owner/repo.git",
            "task",
            "new",
            now
        ));
        assert!(!merged_pr_matches(
            &pr,
            "git@github.com:owner/repo.git",
            "reused",
            "abc",
            now
        ));
        assert!(!merged_pr_matches(
            &pr,
            "git@github.com:owner/repo.git",
            "task",
            "abc",
            pr.created_at.unwrap()
        ));
        pr.head_repository = Some("fork/repo".into());
        assert!(!merged_pr_matches(
            &pr,
            "git@github.com:owner/repo.git",
            "task",
            "abc",
            now
        ));
        pr.head_repository = Some("owner/repo".into());
        pr.merged_at = None;
        assert!(!merged_pr_matches(
            &pr,
            "git@github.com:owner/repo.git",
            "task",
            "abc",
            now
        ));
    }

    #[test]
    fn head_repository_comes_from_owner_and_name() {
        // What `gh pr list` really returns: `nameWithOwner` is empty.
        let prs = parse_prs(br#"[
          {"number":36,"title":"T","headRefName":"x","headRepository":{"id":"R_1","name":"SuperGIT","nameWithOwner":""},
           "headRepositoryOwner":{"id":"U_1","login":"Aitem"},"baseRefName":"main","state":"MERGED","isDraft":false,
           "url":"https://github.com/Aitem/SuperGIT/pull/36","updatedAt":"t"},
          {"number":37,"title":"T","headRefName":"x","headRepository":null,"headRepositoryOwner":null,
           "baseRefName":"main","state":"CLOSED","isDraft":false,"url":"https://github.com/Aitem/SuperGIT/pull/37","updatedAt":"t"}
        ]"#).unwrap();
        assert_eq!(prs[0].head_repository.as_deref(), Some("Aitem/SuperGIT"));
        assert!(head_in_same_repository(&prs[0]));
        assert_eq!(prs[1].head_repository, None, "a deleted fork");
    }

    #[test]
    fn fork_heads_are_not_local_branches() {
        let mut pr = parse_prs(br#"[{"number":7,"title":"T","headRefName":"main","headRepository":{"nameWithOwner":"Owner/Repo"},"baseRefName":"main","state":"OPEN","isDraft":false,"url":"https://github.com/owner/repo/pull/7","updatedAt":"t"}]"#).unwrap().remove(0);
        assert!(head_in_same_repository(&pr));
        pr.head_repository = Some("someone/repo".into());
        assert!(!head_in_same_repository(&pr));
        pr.head_repository = None;
        assert!(!head_in_same_repository(&pr), "a deleted fork");
    }

    #[test]
    fn parses_gh_json() {
        let json = br#"[
          {"number":7,"title":"Merged one","headRefName":"a","headRefOid":"abc123","headRepository":{"nameWithOwner":"owner/repo"},"baseRefName":"main","state":"MERGED","isDraft":false,
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
        assert_eq!(prs[1].head_oid, "abc123");
        assert_eq!(prs[1].head_repository.as_deref(), Some("owner/repo"));
        assert!(prs[0].head_oid.is_empty());
    }
}
