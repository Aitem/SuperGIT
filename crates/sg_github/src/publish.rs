//! Draft PR creation targeting the repository resolved from the push URL.

use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;

use crate::{FIELDS, PullRequest, gh, head_in_same_repository, parse_prs, repository_identity};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishRepository {
    pub name_with_owner: String,
    pub default_branch_ref: Option<PublishBase>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PublishBase {
    pub name: String,
    pub target: PublishTip,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PublishTip {
    pub oid: String,
}

const PUBLISH_REPO_QUERY: &str = "query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    nameWithOwner defaultBranchRef { name target { oid } }
  }
}";

pub fn publish_repository(path: &Path, push_url: &str) -> Result<PublishRepository> {
    let slug = repository_identity(push_url)
        .context("The push remote needs a github.com repository URL")?;
    let bytes = gh(
        path,
        [
            "api",
            "graphql",
            "--hostname",
            "github.com",
            "-F",
            "owner={owner}",
            "-F",
            "name={repo}",
            "-f",
            &format!("query={PUBLISH_REPO_QUERY}"),
        ],
    )?;
    #[derive(Deserialize)]
    struct Response {
        data: Data,
    }
    #[derive(Deserialize)]
    struct Data {
        repository: Option<PublishRepository>,
    }
    let response: Response = serde_json::from_slice(&bytes)?;
    let repository = response
        .data
        .repository
        .context("GitHub did not return a repository")?;
    ensure!(
        repository.name_with_owner.eq_ignore_ascii_case(&slug),
        "The push remote points to {slug}, but this project is showing PRs for {}",
        repository.name_with_owner
    );
    Ok(repository)
}

pub fn open_pr_from_branch(
    path: &Path,
    repository: &str,
    head: &str,
    base: &str,
) -> Result<Option<PullRequest>> {
    let target = format!("github.com/{repository}");
    let bytes = gh(
        path,
        [
            "pr", "list", "--repo", &target, "--state", "open", "--head", head, "--base", base,
            "--json", FIELDS,
        ],
    )?;
    Ok(parse_prs(&bytes)?
        .into_iter()
        .find(|pr| pr.head_branch == head && pr.base_branch == base && head_in_same_repository(pr)))
}

#[derive(Debug)]
pub struct DraftPullRequest {
    pub number: u64,
    pub url: String,
}

pub fn create_draft_pr(
    path: &Path,
    repository: &str,
    head: &str,
    base: &str,
    title: &str,
    body: &str,
) -> Result<DraftPullRequest> {
    ensure!(!title.trim().is_empty(), "Enter a pull request title");
    let target = format!("github.com/{repository}");
    // An explicit head tells gh to use the published branch as-is.
    let bytes = gh(
        path,
        [
            "pr", "create", "--repo", &target, "--head", head, "--base", base, "--title", title,
            "--body", body, "--draft",
        ],
    )?;
    parse_created_pr(&bytes, repository)
}

fn parse_created_pr(bytes: &[u8], repository: &str) -> Result<DraftPullRequest> {
    String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .find_map(|line| {
            let url = line.trim();
            let (repo, number) = url.split_once("/pull/")?;
            let repo = repository_identity(repo)?;
            let number = number.parse::<u64>().ok()?;
            (number > 0 && repo.eq_ignore_ascii_case(repository)).then(|| DraftPullRequest {
                number,
                url: url.to_string(),
            })
        })
        .context("GitHub did not return a pull request URL for the push repository")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_response_must_point_into_the_confirmed_repository() {
        let created = parse_created_pr(
            b"notice\nhttps://github.com/Owner/Repo/pull/19\n",
            "owner/repo",
        )
        .unwrap();
        assert_eq!(created.number, 19);
        assert!(parse_created_pr(b"https://github.com/fork/repo/pull/19", "owner/repo").is_err());
        assert!(parse_created_pr(b"https://github.com/owner/repo/pull/0", "owner/repo").is_err());
    }
}
