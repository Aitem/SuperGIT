//! Mutations explicitly triggered for one existing GitHub review thread.

use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::{Comment, RawComment};
use crate::gh_with_input;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct ThreadPermissions {
    pub viewer_can_reply: bool,
    pub viewer_can_resolve: bool,
    pub viewer_can_unresolve: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThreadState {
    pub id: String,
    pub is_resolved: bool,
    #[serde(flatten)]
    pub permissions: ThreadPermissions,
}

const REPLY: &str = "mutation($id:ID!, $body:String!) {
  addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:$id, body:$body}) {
    comment { author { login } body createdAt url }
  }
}";
const RESOLVE: &str = "mutation($id:ID!) {
  resolveReviewThread(input:{threadId:$id}) {
    thread { id isResolved viewerCanReply viewerCanResolve viewerCanUnresolve }
  }
}";
const REOPEN: &str = "mutation($id:ID!) {
  unresolveReviewThread(input:{threadId:$id}) {
    thread { id isResolved viewerCanReply viewerCanResolve viewerCanUnresolve }
  }
}";

pub fn reply_to_thread(repo: &Path, id: &str, reply: &str) -> Result<Comment> {
    ensure!(!id.is_empty(), "Review thread ID is missing");
    ensure!(!reply.trim().is_empty(), "Write a reply first");
    let raw: RawComment = mutate(
        repo,
        REPLY,
        json!({"id":id,"body":reply}),
        "addPullRequestReviewThreadReply",
        "comment",
    )?;
    Ok(raw.into_comment())
}

pub fn set_thread_resolved(repo: &Path, id: &str, resolved: bool) -> Result<ReviewThreadState> {
    ensure!(!id.is_empty(), "Review thread ID is missing");
    let (query, mutation) = if resolved {
        (RESOLVE, "resolveReviewThread")
    } else {
        (REOPEN, "unresolveReviewThread")
    };
    let state: ReviewThreadState = mutate(repo, query, json!({"id":id}), mutation, "thread")?;
    ensure!(
        state.id == id && state.is_resolved == resolved,
        "GitHub returned an unexpected thread state"
    );
    Ok(state)
}

fn mutate<T: DeserializeOwned>(
    repo: &Path,
    query: &str,
    variables: Value,
    mutation: &str,
    field: &str,
) -> Result<T> {
    let request = serde_json::to_vec(&json!({"query": query, "variables": variables}))?;
    let output = gh_with_input(repo, ["api", "graphql", "--input", "-"], Some(&request))?;
    parse_mutation(&output, mutation, field)
}

fn parse_mutation<T: DeserializeOwned>(output: &[u8], mutation: &str, field: &str) -> Result<T> {
    let mut payload: Value = serde_json::from_slice(output).context("Invalid GitHub response")?;
    if let Some(error) = payload
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|errors| errors.first())
    {
        anyhow::bail!(
            "{}",
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("GitHub rejected the review action")
        );
    }
    let result = payload
        .get_mut("data")
        .and_then(|data| data.get_mut(mutation))
        .and_then(|mutation| mutation.get_mut(field))
        .filter(|value| !value.is_null())
        .context("GitHub did not return the updated review thread")?;
    Ok(serde_json::from_value(result.take())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reply_and_thread_state() {
        let data = br#"{"data":{"addPullRequestReviewThreadReply":{"comment":{"author":{"login":"liz"},"body":"fixed\nthanks!","url":"https://github.com/o/r/pull/1#discussion_r1"}}}}"#;
        let comment: RawComment =
            parse_mutation(data, "addPullRequestReviewThreadReply", "comment").unwrap();
        assert_eq!(comment.into_comment().body, "fixed\nthanks!");
        let data = br#"{"data":{"resolveReviewThread":{"thread":{"id":"T1","isResolved":true,"viewerCanUnresolve":true}}}}"#;
        let thread: ReviewThreadState =
            parse_mutation(data, "resolveReviewThread", "thread").unwrap();
        assert!(thread.is_resolved && thread.permissions.viewer_can_unresolve);
    }

    #[test]
    fn reports_graphql_errors_and_rejects_empty_payloads() {
        assert!(
            parse_mutation::<Value>(
                br#"{"errors":[{"message":"Forbidden"}]}"#,
                "resolveReviewThread",
                "thread"
            )
            .unwrap_err()
            .to_string()
            .contains("Forbidden")
        );
        assert!(
            parse_mutation::<Value>(
                br#"{"data":{"resolveReviewThread":{"thread":null}}}"#,
                "resolveReviewThread",
                "thread"
            )
            .is_err()
        );
    }
}
