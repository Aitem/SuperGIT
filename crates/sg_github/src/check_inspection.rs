//! On-demand diagnostics for a single check run on a pull request.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::gh;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckInspection {
    pub summary: Option<String>,
    annotations: Connection<CheckAnnotation>,
    steps: Connection<CheckStep>,
}

impl CheckInspection {
    pub fn annotations(&self) -> impl Iterator<Item = &CheckAnnotation> {
        let mut seen = HashSet::new();
        self.annotations
            .nodes
            .iter()
            .flatten()
            .filter(move |annotation| seen.insert(*annotation))
    }

    pub fn annotations_remaining(&self) -> usize {
        self.annotations
            .total_count
            .saturating_sub(self.annotations.nodes.iter().flatten().count())
    }

    pub fn failed_steps(&self) -> impl Iterator<Item = &str> {
        self.steps.nodes.iter().flatten().filter_map(|step| {
            matches!(
                step.conclusion.as_deref(),
                Some("FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE")
            )
            .then_some(step.name.as_str())
        })
    }
}

#[derive(Debug, Deserialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CheckAnnotation {
    pub annotation_level: Option<String>,
    pub path: Option<String>,
    pub title: Option<String>,
    pub message: String,
    location: Location,
}

impl CheckAnnotation {
    pub fn line(&self) -> Option<u64> {
        (self.location.start.line > 0).then_some(self.location.start.line)
    }
}

#[derive(Debug, Deserialize, Eq, Hash, PartialEq)]
struct Location {
    start: Position,
}

#[derive(Debug, Deserialize, Eq, Hash, PartialEq)]
struct Position {
    line: u64,
}

#[derive(Debug, Deserialize)]
struct CheckStep {
    name: String,
    conclusion: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    total_count: usize,
    nodes: Vec<Option<T>>,
}

const QUERY: &str = "query($id: ID!) {
  node(id: $id) {
    ... on CheckRun {
      summary
      annotations(first: 30) {
        totalCount
        nodes { annotationLevel path title message location { start { line } } }
      }
      steps(first: 100) { totalCount nodes { name conclusion } }
    }
  }
}";

/// Fetch a GitHub check run by the opaque node ID from the PR rollup.
pub fn check_inspection(repo: &Path, id: &str) -> Result<CheckInspection> {
    let out = gh(
        repo,
        [
            "api",
            "graphql",
            "-f",
            &format!("id={id}"),
            "-f",
            &format!("query={QUERY}"),
        ],
    )?;
    parse(&out)
}

fn parse(out: &[u8]) -> Result<CheckInspection> {
    #[derive(Deserialize)]
    struct Response {
        data: Data,
    }
    #[derive(Deserialize)]
    struct Data {
        node: Option<CheckInspection>,
    }
    let response: Response =
        serde_json::from_slice(out).context("Invalid check-run details from GitHub")?;
    response
        .data
        .node
        .context("GitHub no longer has details for this check run")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancelled_runner_exposes_its_annotation_without_a_failed_step() {
        let details = parse(
            br#"{"data":{"node":{
          "summary":null,
          "annotations":{"totalCount":3,"nodes":[null,{
            "annotationLevel":"FAILURE","path":".github","title":null,
            "message":"The job was not acquired by Runner of type hosted",
            "location":{"start":{"line":1}}
          }]},
          "steps":{"totalCount":1,"nodes":[{"name":"Set up job","conclusion":"CANCELLED"}]}
        }}}"#,
        )
        .unwrap();
        let annotation = details.annotations().next().unwrap();
        assert!(annotation.message.contains("not acquired by Runner"));
        assert_eq!(annotation.line(), Some(1));
        assert_eq!(details.annotations_remaining(), 2);
        assert_eq!(details.failed_steps().count(), 0);
    }

    #[test]
    fn failed_steps_omit_success_and_skips() {
        let details = parse(
            br#"{"data":{"node":{
          "summary":"Two jobs ran",
          "annotations":{"totalCount":0,"nodes":[]},
          "steps":{"totalCount":3,"nodes":[
            {"name":"Checkout","conclusion":"SUCCESS"},
            {"name":"Build","conclusion":"FAILURE"},
            {"name":"Upload","conclusion":"SKIPPED"}
          ]}
        }}}"#,
        )
        .unwrap();
        assert_eq!(details.failed_steps().collect::<Vec<_>>(), ["Build"]);
        assert_eq!(details.annotations_remaining(), 0);
        assert!(parse(br#"{"data":{"node":null}}"#).is_err());
    }
}
