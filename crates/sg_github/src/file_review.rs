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

impl FileReview {
    /// The base commit the viewed state belongs to.
    pub fn base(&self) -> &str {
        &self.revision.base_ref_oid
    }
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

/// The viewer's viewed state of every file at `head` against `base`; an
/// empty `base` (not known yet) takes GitHub's, see [`FileReview::base`].
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
        revision.head_ref_oid == head && (base.is_empty() || revision.base_ref_oid == base),
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

/// Mark (`true`) or unmark paths on the GitHub revision that supplied the
/// viewed state, in one request: GraphQL runs a document's mutations in
/// order, so a later change of the same path wins. GitHub accepts the path
/// and PR node ID, so check the revision before and after the mutation; the
/// mutation itself has no revision precondition.
pub fn set_pr_files_viewed(
    repo: &Path,
    review: &FileReview,
    changes: &[(String, bool)],
) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    ensure!(
        changes
            .iter()
            .all(|(path, _)| review.files.contains_key(path)),
        "This path is missing from the pull request"
    );
    let expected = &review.revision;
    let current: Revision = request(repo, REVISION, json!({"id":expected.id}), "/node")?;
    ensure!(
        current == *expected,
        "The pull request revision changed. Refresh the diff first."
    );
    let (query, variables) = viewed_mutation(&expected.id, changes);
    let updated: HashMap<String, Updated> = request(repo, &query, variables, "")?;
    ensure!(
        updated.len() == changes.len()
            && updated
                .values()
                .all(|updated| updated.pull_request == *expected),
        "The pull request changed during the update. Refresh to see the current viewed state."
    );
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Updated {
    pull_request: Revision,
}

/// One aliased `markFileAsViewed`/`unmarkFileAsViewed` per change.
fn viewed_mutation(id: &str, changes: &[(String, bool)]) -> (String, Value) {
    let mut variables = serde_json::Map::new();
    variables.insert("id".into(), id.into());
    let mut params = vec!["$id:ID!".to_string()];
    let mut fields = String::new();
    for (ix, (path, viewed)) in changes.iter().enumerate() {
        let name = if *viewed {
            "markFileAsViewed"
        } else {
            "unmarkFileAsViewed"
        };
        params.push(format!("$p{ix}:String!"));
        variables.insert(format!("p{ix}"), path.as_str().into());
        fields.push_str(&format!(
            "  m{ix}: {name}(input:{{pullRequestId:$id, path:$p{ix}}}) {{
    pullRequest {{ id headRefOid baseRefOid }}
  }}\n"
        ));
    }
    let query = format!("mutation({}) {{\n{fields}}}", params.join(", "));
    (query, Value::Object(variables))
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
    fn batches_viewed_changes_as_ordered_aliases() {
        let changes = [
            ("a.rs".to_string(), true),
            ("dir/b c.rs".to_string(), false),
        ];
        let (query, variables) = viewed_mutation("PR_1", &changes);
        assert!(query.starts_with("mutation($id:ID!, $p0:String!, $p1:String!)"));
        let first = query.find("m0: markFileAsViewed").unwrap();
        let second = query.find("m1: unmarkFileAsViewed").unwrap();
        assert!(first < second);
        assert_eq!(
            variables,
            json!({"id":"PR_1", "p0":"a.rs", "p1":"dir/b c.rs"})
        );
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
        // Without a known base, GitHub's is taken; the head is still checked.
        let early = load_pages("head", "", |cursor| {
            Ok(match cursor {
                None => page("head", "a", "VIEWED", true),
                Some(_) => page("head", "b", "UNVIEWED", false),
            })
        })
        .unwrap();
        assert_eq!(early.base(), "base");
        assert!(load_pages("moved", "", |_| Ok(page("head", "a", "VIEWED", false))).is_err());
    }
}
