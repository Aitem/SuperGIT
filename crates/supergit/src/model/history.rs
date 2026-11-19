//! Reuse a History snapshot while its worktree and the relevant refs agree.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sg_git::{Commit, Fork, WorktreeStatus};

use super::{LoadState, git_error_message};

/// Git config and reflog-only changes will be reconciled at the next full read.
const RECONCILE_HISTORY_AFTER: Duration = Duration::from_secs(30);
pub(super) const HISTORY_PAGE: usize = 400;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistorySource {
    common_dir: PathBuf,
    head: Option<String>,
    branch: Option<String>,
    upstream: Option<String>,
}

impl HistorySource {
    pub fn new(common_dir: &Path, status: &WorktreeStatus) -> Self {
        Self {
            common_dir: common_dir.to_path_buf(),
            head: status.head_oid.clone(),
            branch: status.branch.clone(),
            upstream: status.upstream.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct HistoryKey {
    worktree: PathBuf,
    source: HistorySource,
    refs: Vec<u8>,
    shallow: Option<Vec<u8>>,
    grafts: Option<Vec<u8>>,
}

impl HistoryKey {
    fn read(worktree: &Path, source: &HistorySource) -> anyhow::Result<Self> {
        Ok(Self {
            worktree: worktree.to_path_buf(),
            source: source.clone(),
            refs: sg_git::history_refs(worktree)?,
            shallow: optional_file(&source.common_dir.join("shallow"))?,
            grafts: optional_file(&source.common_dir.join("info/grafts"))?,
        })
    }
}

fn optional_file(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Clone, Debug, Default)]
pub struct History {
    pub commits: LoadState<Arc<Vec<Commit>>>,
    pub fork: Option<Fork>,
    loaded_limit: Option<usize>,
    pub has_more: bool,
    cached: Option<(Arc<HistoryKey>, Instant)>,
}

impl History {
    pub fn limit(&self) -> usize {
        self.loaded_limit.unwrap_or(HISTORY_PAGE)
    }

    pub fn next_limit(&self) -> usize {
        self.limit().saturating_add(HISTORY_PAGE)
    }

    pub fn load(&self, path: &Path, source: Option<&HistorySource>, limit: usize) -> Self {
        let key = source.and_then(|source| HistoryKey::read(path, source).ok());
        if let Some((cached, when)) = &self.cached
            && self.loaded_limit == Some(limit)
            && when.elapsed() < RECONCILE_HISTORY_AFTER
            && key.as_ref() == Some(cached.as_ref())
        {
            return self.clone();
        }
        // The commit walk and the fork point read different refs: both at once.
        let (commits, fork) = sg_git::parallel::join(
            || sg_git::history(path, limit.saturating_add(1)),
            || sg_git::fork(path),
        );
        let mut has_more = false;
        let commits = commits
            .map(|mut commits| {
                has_more = commits.len() > limit;
                commits.truncate(limit);
                Arc::new(commits)
            })
            .map_err(git_error_message);
        // Read the refs on both sides of a miss. If a fetch changes the graph
        // during the walk, refresh it again next time instead of caching it.
        let cached = if commits.is_ok() && fork.is_ok() {
            key.filter(|before| {
                HistoryKey::read(path, &before.source).is_ok_and(|after| after == *before)
            })
            .map(|key| (Arc::new(key), Instant::now()))
        } else {
            None
        };
        Self {
            loaded_limit: commits.as_ref().ok().map(|_| limit),
            commits: LoadState::from_result(commits),
            fork: fork.ok().flatten(),
            has_more,
            cached,
        }
    }

    pub fn update(&mut self, next: Self) {
        if next.commits.error().is_none() {
            self.fork = next.fork;
            self.loaded_limit = next.loaded_limit;
            self.has_more = next.has_more;
        }
        self.commits.update(next.commits);
        self.cached = next.cached;
    }

    pub fn invalidate(&mut self) {
        self.cached = None;
    }
}

pub fn search_terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

pub fn matches_commit(commit: &Commit, terms: &[String]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let subject = commit.subject.to_lowercase();
    let author = commit.author.to_lowercase();
    let email = commit.email.to_lowercase();
    let refs = commit
        .refs
        .iter()
        .map(|label| label.name.to_lowercase())
        .collect::<Vec<_>>();
    terms.iter().all(|term| {
        commit.sha.contains(term)
            || subject.contains(term)
            || author.contains(term)
            || email.contains(term)
            || refs.iter().any(|name| name.contains(term))
    })
}

#[cfg(test)]
mod tests {
    use super::{matches_commit, search_terms};
    use sg_git::{Commit, RefKind, RefLabel};

    #[test]
    fn terms_can_match_different_commit_fields() {
        let commit = Commit {
            sha: "abc4901234".into(),
            parents: Vec::new(),
            author: "Éloïse Swift".into(),
            email: "eloise@example.com".into(),
            time: 0,
            subject: "Fix startup latency".into(),
            refs: vec![RefLabel {
                kind: RefKind::Tag,
                name: "Ship/v1".into(),
                current: false,
            }],
            agent: None,
        };
        assert!(matches_commit(
            &commit,
            &search_terms("  ABC49  ÉLOÏSE example.com  LATENCY Ship/V1\t")
        ));
        assert!(matches_commit(&commit, &search_terms("  ")));
        assert!(!matches_commit(&commit, &search_terms("startup crash")));
    }
}
