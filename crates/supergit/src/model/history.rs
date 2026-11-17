//! Reuse a History snapshot while its worktree and the relevant refs agree.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sg_git::{Commit, Fork, WorktreeStatus};

use super::{LoadState, git_error_message};

/// Git config and reflog-only changes will be reconciled at the next full read.
const RECONCILE_HISTORY_AFTER: Duration = Duration::from_secs(30);

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
    cached: Option<(Arc<HistoryKey>, Instant)>,
}

impl History {
    pub fn load(&self, path: &Path, source: Option<&HistorySource>) -> Self {
        let key = source.and_then(|source| HistoryKey::read(path, source).ok());
        if let Some((cached, when)) = &self.cached
            && when.elapsed() < RECONCILE_HISTORY_AFTER
            && key.as_ref() == Some(cached.as_ref())
        {
            return self.clone();
        }
        let commits = sg_git::history(path, 400)
            .map(Arc::new)
            .map_err(git_error_message);
        let fork = sg_git::fork(path);
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
            commits: LoadState::from_result(commits),
            fork: fork.ok().flatten(),
            cached,
        }
    }

    pub fn update(&mut self, next: Self) {
        if next.commits.error().is_none() {
            self.fork = next.fork;
        }
        self.commits.update(next.commits);
        self.cached = next.cached;
    }

    pub fn invalidate(&mut self) {
        self.cached = None;
    }
}
