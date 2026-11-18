use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{gh, gh_with_input};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FileViewedState {
    Unviewed,
    Viewed,
    /// GitHub found new changes in a file the viewer had marked as viewed.
    Dismissed,
}

#[derive(Clone, Debug)]
pub struct FileReview {
    revision: Revision,
    pub files: HashMap<String, FileViewedState>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Revision {
    id: String,
    head_ref_oid: String,
    base_ref_oid: String,
}

#[derive(Deserialize)]
struct Page {
    #[serde(flatten)]
    revision: Revision,
    files: Files,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Files {
    total_count: usize,
    nodes: Vec<File>,
    page_info: PageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct File {
    path: String,
    viewer_viewed_state: FileViewedState,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    end_cursor: Option<String>,
    has_next_page: bool,
}

const FILES: &str = "query($owner:String!, $name:String!, $number:Int!, $cursor:String) {
  repository(owner:$owner, name:$name) {
    pullRequest(number:$number) {
      id headRefOid baseRefOid
      files(first:100, after:$cursor) {
        totalCount nodes { path viewerViewedState } pageInfo { endCursor hasNextPage }
      }
    }
  }
}";

const REVISION: &str = "query($id:ID!) {
  node(id:$id) { ... on PullRequest { id headRefOid baseRefOid } }
}";

pub fn pr_file_review(repo: &Path, number: u64, head: &str, base: &str) -> Result<FileReview> {
    load_pages(head, base, |cursor| {
        let mut args = vec![
            "api".to_string(),
            "graphql".into(),
            "-F".into(),
            "owner={owner}".into(),
            "-F".into(),
            "name={repo}".into(),
            "-F".into(),
            format!("number={number}"),
            "-f".into(),
            format!("query={FILES}"),
        ];
        if let Some(cursor) = cursor {
            args.extend(["-f".into(), format!("cursor={cursor}")]);
        }
        data_at(&gh(repo, args)?, "/repository/pullRequest")
    })
}

fn load_pages(
    head: &str,
    base: &str,
    mut page: impl FnMut(Option<&str>) -> Result<Page>,
) -> Result<FileReview> {
    let first = page(None)?;
    let revision = first.revision;
    ensure!(
        revision.head_ref_oid == head && revision.base_ref_oid == base,
        "The pull request revision changed. Refresh the diff before reviewing files."
    );
    let mut files = HashMap::new();
    let mut cursors = HashSet::new();
    let mut next = first.files;
    let count = next.total_count;
    loop {
        files.extend(
            next.nodes
                .into_iter()
                .map(|file| (file.path, file.viewer_viewed_state)),
        );
        if !next.page_info.has_next_page {
            break;
        }
        let cursor = next
            .page_info
            .end_cursor
            .context("GitHub did not return the next file cursor")?;
        ensure!(
            cursors.insert(cursor.clone()),
            "GitHub repeated a file cursor"
        );
        let more = page(Some(&cursor))?;
        ensure!(
            revision == more.revision && more.files.total_count == count,
            "The pull request changed while loading file review progress"
        );
        next = more.files;
    }
    ensure!(
        files.len() == count,
        "GitHub returned an incomplete file list"
    );
    Ok(FileReview { revision, files })
}

/// Change one path on the GitHub revision that supplied the viewed state.
/// GitHub accepts the path and PR node ID, so check the revision before and
/// after the mutation; the mutation itself has no revision precondition.
pub fn set_pr_file_viewed(
    repo: &Path,
    review: &FileReview,
    path: &str,
    viewed: bool,
) -> Result<()> {
    ensure!(
        review.files.contains_key(path),
        "This path is missing from the pull request"
    );
    let expected = &review.revision;
    let current: Revision = request(repo, REVISION, json!({"id":expected.id}), "/node")?;
    ensure!(
        current == *expected,
        "The pull request revision changed. Refresh the diff first."
    );
    let name = if viewed {
        "markFileAsViewed"
    } else {
        "unmarkFileAsViewed"
    };
    let query = format!(
        "mutation($id:ID!, $path:String!) {{
        {name}(input:{{pullRequestId:$id, path:$path}}) {{
            pullRequest {{ id headRefOid baseRefOid }}
        }}
    }}"
    );
    let updated: Revision = request(
        repo,
        &query,
        json!({"id":expected.id, "path":path}),
        &format!("/{name}/pullRequest"),
    )?;
    ensure!(
        updated == *expected,
        "The pull request changed during the update. Refresh to see the current viewed state."
    );
    Ok(())
}

fn request<T: DeserializeOwned>(
    repo: &Path,
    query: &str,
    variables: Value,
    field: &str,
) -> Result<T> {
    let input = serde_json::to_vec(&json!({"query":query,"variables":variables}))?;
    data_at(
        &gh_with_input(repo, ["api", "graphql", "--input", "-"], Some(&input))?,
        field,
    )
}

fn data_at<T: DeserializeOwned>(bytes: &[u8], pointer: &str) -> Result<T> {
    let mut value: Value = serde_json::from_slice(bytes).context("Invalid GitHub response")?;
    if let Some(message) = value.pointer("/errors/0/message").and_then(Value::as_str) {
        anyhow::bail!("{message}");
    }
    let result = value
        .get_mut("data")
        .and_then(|data| data.pointer_mut(pointer))
        .filter(|value| !value.is_null())
        .context("GitHub did not return file review data")?;
    Ok(serde_json::from_value(result.take())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(head: &str, name: &str, state: &str, next: bool) -> Page {
        let payload = json!({ "data": { "pullRequest": {
            "id": "PR_123", "headRefOid": head, "baseRefOid": "base",
            "files": { "totalCount": 2, "nodes": [{"path":name, "viewerViewedState":state}],
                       "pageInfo": {"endCursor":"page2","hasNextPage":next}}
        }}});
        data_at(&serde_json::to_vec(&payload).unwrap(), "/pullRequest").unwrap()
    }

    #[test]
    fn loads_each_page_and_preserves_changes_since_viewed() {
        let review = load_pages("head", "base", |cursor| {
            Ok(match cursor {
                None => page("head", "src/a.rs", "VIEWED", true),
                Some("page2") => page("head", "src/b.rs", "DISMISSED", false),
                _ => panic!("unexpected cursor"),
            })
        })
        .unwrap();
        assert_eq!(review.files["src/a.rs"], FileViewedState::Viewed);
        assert_eq!(review.files["src/b.rs"], FileViewedState::Dismissed);
    }

    #[test]
    fn rejects_a_revision_change_between_pages_and_repeated_cursors() {
        let moved = load_pages("head", "base", |cursor| {
            Ok(match cursor {
                None => page("head", "a", "VIEWED", true),
                Some(_) => page("moved", "b", "UNVIEWED", false),
            })
        })
        .unwrap_err();
        assert!(moved.to_string().contains("changed while loading"));
        let repeated =
            load_pages("head", "base", |_| Ok(page("head", "a", "VIEWED", true))).unwrap_err();
        assert!(repeated.to_string().contains("repeated a file cursor"));
    }
}
