//! GitHub issue context for worktree creation.

use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;

use crate::{gh, repository_identity};

#[derive(Clone, Debug, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueReference {
    pub number: u64,
    repository: Option<String>,
}

impl IssueReference {
    /// Use a number from the active project or a GitHub issue URL.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let (repository, digits) = if let Some((repo, suffix)) = text.split_once("/issues/") {
            let repository =
                repository_identity(repo).context("Enter an issue number or a GitHub issue URL")?;
            let digits = suffix
                .split(['?', '#'])
                .next()
                .unwrap_or_default()
                .trim_end_matches('/');
            (Some(repository), digits)
        } else {
            (None, text.strip_prefix('#').unwrap_or(text))
        };
        let number = digits
            .parse::<u64>()
            .ok()
            .filter(|number| *number > 0)
            .context("Enter an issue number or a GitHub issue URL")?;
        Ok(Self { number, repository })
    }
}

pub fn view_issue(path: &Path, reference: &IssueReference) -> Result<Issue> {
    let bytes = gh(
        path,
        [
            "issue",
            "view",
            &reference.number.to_string(),
            "--json",
            "number,title,body,state,url",
        ],
    )?;
    parse_issue(&bytes, reference)
}

fn parse_issue(bytes: &[u8], reference: &IssueReference) -> Result<Issue> {
    let issue: Issue = serde_json::from_slice(bytes).context("Invalid issue data from GitHub")?;
    let returned = IssueReference::parse(&issue.url)?;
    ensure!(
        returned.number == reference.number && issue.number == reference.number,
        "GitHub returned a different issue"
    );
    if let Some(repository) = &reference.repository {
        ensure!(
            returned.repository.as_ref() == Some(repository),
            "This issue belongs to {repository}. Open that project first."
        );
    }
    Ok(issue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_issue_numbers_and_github_urls() {
        assert_eq!(IssueReference::parse(" #42 ").unwrap().number, 42);
        let full = IssueReference::parse("https://github.com/Owner/Repo/issues/42/#issuecomment-9")
            .unwrap();
        assert_eq!(full.number, 42);
        assert_eq!(full.repository.as_deref(), Some("owner/repo"));
        assert!(IssueReference::parse("https://example.com/Owner/Repo/issues/42").is_err());
        assert!(IssueReference::parse("https://github.com/Owner/Repo/pull/42").is_err());
        assert!(IssueReference::parse("0").is_err());
    }

    #[test]
    fn verifies_remote_matches_a_pasted_issue_url() {
        let fixture = br#"{"number":42,"title":"Fix status","body":"Repro steps","state":"OPEN","url":"https://github.com/owner/repo/issues/42"}"#;
        let local = IssueReference::parse("42").unwrap();
        assert_eq!(parse_issue(fixture, &local).unwrap().title, "Fix status");
        let other = IssueReference::parse("https://github.com/another/repo/issues/42").unwrap();
        assert!(parse_issue(fixture, &other).is_err());
        assert!(parse_issue(fixture, &IssueReference::parse("41").unwrap()).is_err());
    }
}
