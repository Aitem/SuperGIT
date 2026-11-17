//! One pull request in depth — description, conversation, review threads,
//! checks and mergeability in a single GraphQL call — and merging it.

use std::path::Path;
use std::time::SystemTime;

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::{Checks, PrState, PullRequest, gh, parse_timestamp};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    pub const ALL: [Self; 3] = [Self::Merge, Self::Squash, Self::Rebase];

    pub fn label(self) -> &'static str {
        match self {
            Self::Merge => "Create a merge commit",
            Self::Squash => "Squash and merge",
            Self::Rebase => "Rebase and merge",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::Merge => "Merge",
            Self::Squash => "Squash",
            Self::Rebase => "Rebase",
        }
    }

    fn flag(self) -> &'static str {
        match self {
            Self::Merge => "--merge",
            Self::Squash => "--squash",
            Self::Rebase => "--rebase",
        }
    }

    fn from_graphql(value: &str) -> Option<Self> {
        match value {
            "MERGE" => Some(Self::Merge),
            "SQUASH" => Some(Self::Squash),
            "REBASE" => Some(Self::Rebase),
            _ => None,
        }
    }
}

/// GitHub's `mergeStateStatus`, with conflicts folded in from `mergeable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeStatus {
    /// Every requirement is met.
    Clean,
    /// Mergeable, but checks that are not required fail.
    Unstable,
    /// Mergeable; the base has pre-receive hooks.
    HasHooks,
    /// Required reviews or checks are missing.
    Blocked,
    /// The base requires the head branch to be up to date first.
    Behind,
    Conflicts,
    Draft,
    /// GitHub is still computing it (right after a push or a merge).
    Unknown,
}

impl MergeStatus {
    fn parse(state: &str, mergeable: &str) -> Self {
        if mergeable == "CONFLICTING" {
            return Self::Conflicts;
        }
        match state {
            "CLEAN" => Self::Clean,
            "UNSTABLE" => Self::Unstable,
            "HAS_HOOKS" => Self::HasHooks,
            "BLOCKED" => Self::Blocked,
            "BEHIND" => Self::Behind,
            "DIRTY" => Self::Conflicts,
            "DRAFT" => Self::Draft,
            _ => Self::Unknown,
        }
    }

    /// GitHub would merge it right now.
    pub fn can_merge(self) -> bool {
        matches!(self, Self::Clean | Self::Unstable | Self::HasHooks)
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::Clean => "Ready to merge",
            Self::Unstable => "Mergeable, but some checks that are not required fail",
            Self::HasHooks => "Ready to merge (the base runs pre-receive hooks)",
            Self::Blocked => "Blocked: required reviews or checks are missing",
            Self::Behind => "The head branch is behind the base and must be updated first",
            Self::Conflicts => "Merge conflicts with the base branch",
            Self::Draft => "Draft: mark it ready for review on GitHub first",
            Self::Unknown => "GitHub is still checking mergeability",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub author: String,
    pub body: String,
    pub created_at: Option<SystemTime>,
    pub url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

impl ReviewState {
    fn parse(value: &str) -> Self {
        match value {
            "APPROVED" => Self::Approved,
            "CHANGES_REQUESTED" => Self::ChangesRequested,
            "DISMISSED" => Self::Dismissed,
            "PENDING" => Self::Pending,
            _ => Self::Commented,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::ChangesRequested => "requested changes",
            Self::Commented => "reviewed",
            Self::Dismissed => "review dismissed",
            Self::Pending => "pending review",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Review {
    pub author: String,
    pub body: String,
    pub state: ReviewState,
    pub submitted_at: Option<SystemTime>,
    pub url: String,
}

/// Comments on a line of the diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewThread {
    pub id: String,
    pub path: String,
    /// The line in the current diff; `None` once outdated.
    pub line: Option<u64>,
    pub resolved: bool,
    pub outdated: bool,
    /// The diff around the commented line, from the first comment.
    pub diff_hunk: String,
    pub comments: Vec<Comment>,
}

impl ReviewThread {
    pub fn created_at(&self) -> Option<SystemTime> {
        self.comments.first().and_then(|c| c.created_at)
    }
}

/// Ordered for display: what needs attention first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CheckStatus {
    Failed,
    Pending,
    Passed,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    /// The Actions workflow a job belongs to.
    pub workflow: Option<String>,
    pub status: CheckStatus,
    /// `failure`, `in progress`, a status context's description…
    pub detail: String,
    pub required: bool,
    pub url: Option<String>,
    pub started_at: Option<SystemTime>,
    pub completed_at: Option<SystemTime>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoMerge {
    pub method: Option<MergeMethod>,
    pub enabled_by: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrDetails {
    /// The pull request itself, fresher than the list it was picked from.
    pub pr: PullRequest,
    /// The base commit the PR's diff starts from (with the head's merge
    /// base): the base tip while open, the base when it merged.
    pub base_oid: String,
    pub body: String,
    pub merge_status: MergeStatus,
    pub merged_by: Option<String>,
    pub closed_at: Option<SystemTime>,
    pub auto_merge: Option<AutoMerge>,
    pub commits: u64,
    pub changed_files: u64,
    /// The latest conversation comments (at most 100) and how many exist.
    pub comments: Vec<Comment>,
    pub comments_total: u64,
    pub reviews: Vec<Review>,
    pub threads: Vec<ReviewThread>,
    pub checks: Vec<Check>,
    /// Methods the repository allows, in GitHub's order.
    pub merge_methods: Vec<MergeMethod>,
    pub default_method: MergeMethod,
    pub auto_merge_allowed: bool,
}

const DETAILS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed autoMergeAllowed
    viewerDefaultMergeMethod
    pullRequest(number: $number) {
      number title body url state isDraft createdAt updatedAt mergedAt closedAt
      author { login }
      headRefName baseRefName headRefOid baseRefOid
      headRepository { nameWithOwner }
      additions deletions changedFiles
      mergeable mergeStateStatus reviewDecision
      autoMergeRequest { mergeMethod enabledBy { login } }
      mergedBy { login }
      commits(last: 1) {
        totalCount
        nodes { commit { statusCheckRollup { contexts(first: 100) { nodes {
          __typename
          ... on CheckRun {
            name status conclusion detailsUrl startedAt completedAt
            isRequired(pullRequestNumber: $number)
            checkSuite { workflowRun { workflow { name } } }
          }
          ... on StatusContext {
            context state targetUrl description createdAt
            isRequired(pullRequestNumber: $number)
          }
        } } } } }
      }
      comments(last: 100) { totalCount nodes { author { login } body createdAt url } }
      reviews(last: 100) { nodes { author { login } body state submittedAt url } }
      reviewThreads(last: 100) { nodes {
        id isResolved isOutdated path line originalLine
        comments(first: 50) { nodes { author { login } body createdAt url diffHunk } }
      } }
    }
  }
}";

/// Everything the PR panel shows about one pull request.
pub fn pr_details(repo: &Path, number: u64) -> Result<PrDetails> {
    let json = gh(
        repo,
        [
            "api".to_string(),
            "graphql".into(),
            "-F".into(),
            "owner={owner}".into(),
            "-F".into(),
            "name={repo}".into(),
            "-F".into(),
            format!("number={number}"),
            "-f".into(),
            format!("query={DETAILS_QUERY}"),
        ],
    )?;
    parse_details(&json)
}

/// Merge on GitHub. `head_oid` (when known) makes GitHub refuse if the
/// branch moved since it was shown; `auto` queues the merge until the
/// requirements are met instead. The branch is kept: worktrees still use it.
pub fn merge_pr(
    repo: &Path,
    number: u64,
    method: MergeMethod,
    head_oid: &str,
    auto: bool,
) -> Result<()> {
    let mut args = vec![
        "pr".to_string(),
        "merge".into(),
        number.to_string(),
        method.flag().into(),
    ];
    if !head_oid.is_empty() {
        args.extend(["--match-head-commit".into(), head_oid.to_string()]);
    }
    if auto {
        args.push("--auto".into());
    }
    gh(repo, args).map(drop)
}

pub fn disable_auto_merge(repo: &Path, number: u64) -> Result<()> {
    gh(
        repo,
        [
            "pr".to_string(),
            "merge".into(),
            number.to_string(),
            "--disable-auto".into(),
        ],
    )
    .map(drop)
}

// ---- parsing ---------------------------------------------------------------

#[derive(Deserialize)]
struct Response {
    data: Option<Data>,
}

#[derive(Deserialize)]
struct Data {
    repository: Option<Repository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repository {
    #[serde(default)]
    merge_commit_allowed: bool,
    #[serde(default)]
    squash_merge_allowed: bool,
    #[serde(default)]
    rebase_merge_allowed: bool,
    #[serde(default)]
    auto_merge_allowed: bool,
    viewer_default_merge_method: Option<String>,
    pull_request: Option<RawDetails>,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
struct Nodes<T> {
    #[serde(default = "Vec::new")]
    nodes: Vec<T>,
    #[serde(default, rename = "totalCount")]
    total_count: u64,
}

impl<T> Default for Nodes<T> {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            total_count: 0,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDetails {
    number: u64,
    title: String,
    #[serde(default)]
    body: String,
    url: String,
    state: String,
    #[serde(default)]
    is_draft: bool,
    created_at: Option<String>,
    #[serde(default)]
    updated_at: String,
    merged_at: Option<String>,
    closed_at: Option<String>,
    author: Option<Login>,
    head_ref_name: String,
    base_ref_name: String,
    #[serde(default)]
    head_ref_oid: String,
    #[serde(default)]
    base_ref_oid: String,
    head_repository: Option<NameWithOwner>,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    #[serde(default)]
    changed_files: u64,
    #[serde(default)]
    mergeable: String,
    #[serde(default)]
    merge_state_status: String,
    review_decision: Option<String>,
    auto_merge_request: Option<RawAutoMerge>,
    merged_by: Option<Login>,
    #[serde(default)]
    commits: Nodes<RawCommitNode>,
    #[serde(default)]
    comments: Nodes<RawComment>,
    #[serde(default)]
    reviews: Nodes<RawReview>,
    #[serde(default)]
    review_threads: Nodes<RawThread>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NameWithOwner {
    name_with_owner: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAutoMerge {
    merge_method: Option<String>,
    enabled_by: Option<Login>,
}

#[derive(Deserialize)]
struct RawCommitNode {
    commit: RawCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCommit {
    status_check_rollup: Option<RawRollup>,
}

#[derive(Deserialize)]
struct RawRollup {
    #[serde(default)]
    contexts: Nodes<RawContext>,
}

/// A `CheckRun` or a `StatusContext`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawContext {
    // CheckRun
    name: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    details_url: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    check_suite: Option<RawSuite>,
    // StatusContext
    context: Option<String>,
    state: Option<String>,
    target_url: Option<String>,
    description: Option<String>,
    created_at: Option<String>,
    #[serde(default)]
    is_required: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSuite {
    workflow_run: Option<RawRun>,
}

#[derive(Deserialize)]
struct RawRun {
    workflow: Option<RawWorkflow>,
}

#[derive(Deserialize)]
struct RawWorkflow {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawComment {
    author: Option<Login>,
    #[serde(default)]
    body: String,
    created_at: Option<String>,
    #[serde(default)]
    url: String,
    #[serde(default)]
    diff_hunk: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReview {
    author: Option<Login>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    state: String,
    submitted_at: Option<String>,
    #[serde(default)]
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThread {
    #[serde(default)]
    id: String,
    #[serde(default)]
    is_resolved: bool,
    #[serde(default)]
    is_outdated: bool,
    #[serde(default)]
    path: String,
    line: Option<u64>,
    original_line: Option<u64>,
    #[serde(default)]
    comments: Nodes<RawComment>,
}

fn login(author: Option<Login>) -> String {
    // Deleted accounts come back as `null`; GitHub shows them as "ghost".
    author.map_or_else(|| "ghost".to_string(), |a| a.login)
}

fn time(value: Option<&str>) -> Option<SystemTime> {
    value.and_then(parse_timestamp)
}

impl RawComment {
    fn into_comment(self) -> Comment {
        Comment {
            author: login(self.author),
            body: self.body,
            created_at: time(self.created_at.as_deref()),
            url: self.url,
        }
    }
}

impl RawContext {
    fn into_check(self) -> Check {
        let created_at = time(self.created_at.as_deref());
        if let Some(name) = self.name {
            let running = self.status.as_deref().is_some_and(|s| s != "COMPLETED");
            let conclusion = self.conclusion.unwrap_or_default();
            let status = if running {
                CheckStatus::Pending
            } else {
                match conclusion.as_str() {
                    "SUCCESS" => CheckStatus::Passed,
                    "NEUTRAL" | "SKIPPED" => CheckStatus::Skipped,
                    "" => CheckStatus::Pending,
                    _ => CheckStatus::Failed,
                }
            };
            let detail = if running {
                self.status.unwrap_or_default()
            } else {
                conclusion
            };
            Check {
                name,
                workflow: self
                    .check_suite
                    .and_then(|s| s.workflow_run)
                    .and_then(|r| r.workflow)
                    .map(|w| w.name),
                status,
                detail: detail.to_lowercase().replace('_', " "),
                required: self.is_required,
                url: self.details_url.filter(|u| !u.is_empty()),
                started_at: time(self.started_at.as_deref()),
                completed_at: time(self.completed_at.as_deref()),
            }
        } else {
            let state = self.state.unwrap_or_default();
            let status = match state.as_str() {
                "SUCCESS" => CheckStatus::Passed,
                "PENDING" | "EXPECTED" => CheckStatus::Pending,
                _ => CheckStatus::Failed,
            };
            Check {
                name: self.context.unwrap_or_default(),
                workflow: None,
                status,
                detail: self
                    .description
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| state.to_lowercase()),
                required: self.is_required,
                url: self.target_url.filter(|u| !u.is_empty()),
                started_at: created_at,
                completed_at: None,
            }
        }
    }
}

fn parse_details(json: &[u8]) -> Result<PrDetails> {
    let response: Response = serde_json::from_slice(json)?;
    let Some(repository) = response.data.and_then(|d| d.repository) else {
        bail!("GitHub returned no repository");
    };
    let Some(raw) = repository.pull_request else {
        bail!("GitHub returned no pull request");
    };
    let mut merge_methods = Vec::new();
    for (allowed, method) in [
        (repository.merge_commit_allowed, MergeMethod::Merge),
        (repository.squash_merge_allowed, MergeMethod::Squash),
        (repository.rebase_merge_allowed, MergeMethod::Rebase),
    ] {
        if allowed {
            merge_methods.push(method);
        }
    }
    let default_method = repository
        .viewer_default_merge_method
        .as_deref()
        .and_then(MergeMethod::from_graphql)
        .filter(|m| merge_methods.contains(m))
        .or(merge_methods.first().copied())
        .unwrap_or(MergeMethod::Merge);

    let mut checks: Vec<Check> = raw
        .commits
        .nodes
        .into_iter()
        .flat_map(|node| node.commit.status_check_rollup)
        .flat_map(|rollup| rollup.contexts.nodes)
        .map(RawContext::into_check)
        .collect();
    // Stable: failures first, then pending, keeping GitHub's order within.
    checks.sort_by_key(|c| c.status);
    let mut counts = Checks::default();
    for check in &checks {
        match check.status {
            CheckStatus::Passed | CheckStatus::Skipped => counts.passed += 1,
            CheckStatus::Failed => counts.failed += 1,
            CheckStatus::Pending => counts.pending += 1,
        }
    }

    let state = match raw.state.as_str() {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ if raw.is_draft => PrState::Draft,
        _ => PrState::Open,
    };
    let pr = PullRequest {
        created_at: time(raw.created_at.as_deref()),
        merged_at: time(raw.merged_at.as_deref()),
        number: raw.number,
        title: raw.title,
        head_branch: raw.head_ref_name,
        head_oid: raw.head_ref_oid,
        head_repository: raw.head_repository.map(|r| r.name_with_owner),
        base_branch: raw.base_ref_name,
        state,
        url: raw.url,
        author: raw.author.map(|a| a.login).unwrap_or_default(),
        review_decision: raw.review_decision.unwrap_or_default(),
        checks: counts,
        additions: raw.additions,
        deletions: raw.deletions,
        updated_at: raw.updated_at,
    };
    Ok(PrDetails {
        pr,
        base_oid: raw.base_ref_oid,
        body: raw.body,
        // GitHub may report a draft as merely BLOCKED; it never merges one.
        merge_status: if raw.is_draft {
            MergeStatus::Draft
        } else {
            MergeStatus::parse(&raw.merge_state_status, &raw.mergeable)
        },
        merged_by: raw.merged_by.map(|a| a.login),
        closed_at: time(raw.closed_at.as_deref()),
        auto_merge: raw.auto_merge_request.map(|a| AutoMerge {
            method: a
                .merge_method
                .as_deref()
                .and_then(MergeMethod::from_graphql),
            enabled_by: login(a.enabled_by),
        }),
        commits: raw.commits.total_count,
        changed_files: raw.changed_files,
        comments_total: raw.comments.total_count,
        comments: raw
            .comments
            .nodes
            .into_iter()
            .map(RawComment::into_comment)
            .collect(),
        reviews: raw
            .reviews
            .nodes
            .into_iter()
            .map(|r| Review {
                author: login(r.author),
                body: r.body,
                state: ReviewState::parse(&r.state),
                submitted_at: time(r.submitted_at.as_deref()),
                url: r.url,
            })
            .collect(),
        threads: raw
            .review_threads
            .nodes
            .into_iter()
            .map(|t| {
                let diff_hunk = t
                    .comments
                    .nodes
                    .first()
                    .map(|c| c.diff_hunk.clone())
                    .unwrap_or_default();
                ReviewThread {
                    id: t.id,
                    path: t.path,
                    line: t.line.or(t.original_line),
                    resolved: t.is_resolved,
                    outdated: t.is_outdated,
                    diff_hunk,
                    comments: t
                        .comments
                        .nodes
                        .into_iter()
                        .map(RawComment::into_comment)
                        .collect(),
                }
            })
            .collect(),
        checks,
        merge_methods,
        default_method,
        auto_merge_allowed: repository.auto_merge_allowed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"data":{"repository":{
      "mergeCommitAllowed":false,"squashMergeAllowed":true,"rebaseMergeAllowed":true,
      "autoMergeAllowed":true,"viewerDefaultMergeMethod":"MERGE",
      "pullRequest":{"number":42,"title":"Каталог: поиск","body":"Fixes things","url":"https://github.com/o/r/pull/42",
        "state":"OPEN","isDraft":false,"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z",
        "mergedAt":null,"closedAt":null,"author":{"login":"alice"},"headRefName":"search","baseRefName":"main",
        "headRefOid":"abc","baseRefOid":"def","headRepository":{"nameWithOwner":"o/r"},"additions":10,"deletions":2,"changedFiles":3,
        "mergeable":"MERGEABLE","mergeStateStatus":"BLOCKED","reviewDecision":"CHANGES_REQUESTED",
        "autoMergeRequest":null,"mergedBy":null,
        "commits":{"totalCount":4,"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[
          {"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://ci/1",
           "startedAt":"2026-01-01T00:00:00Z","completedAt":"2026-01-01T00:02:00Z","isRequired":true,
           "checkSuite":{"workflowRun":{"workflow":{"name":"CI"}}}},
          {"__typename":"CheckRun","name":"lint","status":"COMPLETED","conclusion":"FAILURE","detailsUrl":"https://ci/2",
           "startedAt":null,"completedAt":null,"isRequired":false,"checkSuite":{"workflowRun":null}},
          {"__typename":"CheckRun","name":"build","status":"IN_PROGRESS","conclusion":null,"detailsUrl":"",
           "startedAt":null,"completedAt":null,"isRequired":true,"checkSuite":null},
          {"__typename":"StatusContext","context":"deploy","state":"PENDING","targetUrl":null,
           "description":"Waiting","createdAt":"2026-01-01T00:00:00Z","isRequired":false},
          {"__typename":"CheckRun","name":"docs","status":"COMPLETED","conclusion":"SKIPPED","detailsUrl":null,
           "startedAt":null,"completedAt":null,"isRequired":false,"checkSuite":null}
        ]}}}}]},
        "comments":{"totalCount":120,"nodes":[{"author":{"login":"bob"},"body":"LGTM?","createdAt":"2026-01-01T01:00:00Z","url":"u1"}]},
        "reviews":{"nodes":[{"author":null,"body":"","state":"CHANGES_REQUESTED","submittedAt":"2026-01-01T02:00:00Z","url":"u2"}]},
        "reviewThreads":{"nodes":[{"id":"T1","isResolved":true,"isOutdated":true,"path":"src/lib.rs","line":null,"originalLine":7,
          "comments":{"nodes":[{"author":{"login":"bob"},"body":"nit","createdAt":"2026-01-01T03:00:00Z","url":"u3","diffHunk":"@@ -1 +1 @@\n-a\n+b"}]}}]}
      }}}}"#;

    #[test]
    fn parses_the_details_query() {
        let d = parse_details(SAMPLE.as_bytes()).unwrap();
        assert_eq!(d.pr.number, 42);
        assert_eq!(d.pr.title, "Каталог: поиск");
        assert_eq!(d.pr.state, PrState::Open);
        assert_eq!(d.pr.head_repository.as_deref(), Some("o/r"));
        assert_eq!(d.base_oid, "def");
        assert_eq!(d.merge_status, MergeStatus::Blocked);
        assert_eq!(d.merge_methods, [MergeMethod::Squash, MergeMethod::Rebase]);
        // The viewer's default is not allowed here: the first allowed one wins.
        assert_eq!(d.default_method, MergeMethod::Squash);
        assert!(d.auto_merge_allowed);
        assert_eq!(d.commits, 4);
        assert_eq!(d.comments_total, 120);
        assert_eq!(d.comments[0].author, "bob");
        assert_eq!(d.reviews[0].author, "ghost");
        assert_eq!(d.reviews[0].state, ReviewState::ChangesRequested);
        let thread = &d.threads[0];
        assert_eq!(
            thread.line,
            Some(7),
            "outdated threads keep the original line"
        );
        assert!(thread.resolved && thread.outdated);
        assert_eq!(thread.diff_hunk, "@@ -1 +1 @@\n-a\n+b");
    }

    #[test]
    fn checks_sort_failures_first_and_count_like_the_list() {
        let d = parse_details(SAMPLE.as_bytes()).unwrap();
        let order: Vec<_> = d.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(order, ["lint", "build", "deploy", "test", "docs"]);
        assert_eq!(
            d.pr.checks,
            Checks {
                passed: 2,
                failed: 1,
                pending: 2
            }
        );
        let build = &d.checks[1];
        assert_eq!(build.detail, "in progress");
        assert!(build.required);
        assert_eq!(build.url, None, "empty URLs are dropped");
        assert_eq!(d.checks[2].detail, "Waiting");
        assert_eq!(d.checks[3].workflow.as_deref(), Some("CI"));
    }

    #[test]
    fn merge_status_folds_in_conflicts() {
        assert_eq!(MergeStatus::parse("CLEAN", "MERGEABLE"), MergeStatus::Clean);
        assert_eq!(
            MergeStatus::parse("UNKNOWN", "CONFLICTING"),
            MergeStatus::Conflicts
        );
        assert_eq!(
            MergeStatus::parse("DIRTY", "UNKNOWN"),
            MergeStatus::Conflicts
        );
        assert_eq!(MergeStatus::parse("", ""), MergeStatus::Unknown);
        let draft = SAMPLE.replace(r#""isDraft":false"#, r#""isDraft":true"#);
        let d = parse_details(draft.as_bytes()).unwrap();
        assert_eq!(
            d.merge_status,
            MergeStatus::Draft,
            "BLOCKED drafts stay drafts"
        );
        assert_eq!(d.pr.state, PrState::Draft);
        assert!(MergeStatus::Unstable.can_merge());
        assert!(!MergeStatus::Blocked.can_merge());
    }

    #[test]
    fn missing_pull_requests_are_errors() {
        assert!(parse_details(br#"{"data":{"repository":{"pullRequest":null}}}"#).is_err());
        assert!(parse_details(br#"{"data":null}"#).is_err());
    }

    /// Against the real GitHub through `gh` (network, auth): this
    /// repository's PR #36, merged with green checks.
    #[test]
    #[ignore = "network: needs gh auth"]
    fn live_details_of_a_merged_pr() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let d = pr_details(repo, 36).unwrap();
        assert_eq!(d.pr.state, PrState::Merged);
        assert_eq!(d.pr.head_branch, "FaceLifting");
        assert!(d.checks.iter().all(|c| c.status == CheckStatus::Passed));
        assert!(!d.checks.is_empty());
        assert!(!d.merge_methods.is_empty());
        let branch = crate::list_branch_prs(repo, "FaceLifting", 50).unwrap();
        assert!(branch.iter().any(|pr| pr.number == 36));
        let found = crate::search_prs(repo, "Wide diffs", 10).unwrap();
        assert!(found.iter().any(|pr| pr.number == 36));
        assert_eq!(crate::view_pr(repo, 36).unwrap().number, 36);
        let diff = crate::pr_diff(repo, 36).unwrap();
        assert!(diff.starts_with("diff --git"), "{diff}");
    }
}
