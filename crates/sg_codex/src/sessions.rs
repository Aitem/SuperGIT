//! Codex sessions, read from the rollouts Codex already writes to
//! `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl`.
//!
//! Only stable parts are read: the first line (`session_meta`: id, cwd,
//! source, git) and `event_msg` turn boundaries in the tail. Titles come from
//! `session_index.jsonl` (thread names), else the first prompt. Status is a
//! heuristic over the last turn boundary and the file's modification time.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;
use sg_agents::{AgentState, Provider, Session, age, clean, owning_worktree};

const TAIL_BYTES: u64 = 256 * 1024;
const HEAD_BYTES: u64 = 512 * 1024;
/// The `session_meta` line embeds the base instructions; cap what is read.
const META_BYTES: u64 = 4 * 1024 * 1024;

/// What the first line says; it never changes once written.
#[derive(Clone, Debug)]
struct Head {
    id: String,
    cwd: PathBuf,
    git_branch: Option<String>,
    /// Sub-agent and review threads belong to their parent conversation.
    child: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LastKind {
    /// A turn started and has not ended.
    TurnOpen,
    TurnDone,
    Unknown,
}

struct Tail {
    len: u64,
    mtime: SystemTime,
    last_kind: LastKind,
    cwd: Option<PathBuf>,
    first_prompt: Option<String>,
    last_prompt: Option<String>,
}

/// Caches rollout heads forever and tails by (size, mtime), so polling every
/// few seconds only re-reads files that changed.
#[derive(Default)]
pub struct CodexSessionIndex {
    /// `CODEX_HOME` override (tests); `None` uses [`crate::codex_home`].
    home: Option<PathBuf>,
    heads: HashMap<PathBuf, Option<Head>>,
    tails: HashMap<PathBuf, Tail>,
    names: (Option<SystemTime>, HashMap<String, String>),
}

impl CodexSessionIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// An index over another Codex home directory.
    pub fn with_home(home: PathBuf) -> Self {
        Self {
            home: Some(home),
            ..Self::default()
        }
    }

    /// Codex sessions for each of `worktrees` (same order), most recent first.
    pub fn sessions_by_worktree(&mut self, worktrees: &[PathBuf]) -> Vec<Vec<Session>> {
        let mut out = vec![Vec::new(); worktrees.len()];
        let Some(home) = self.home.clone().or_else(crate::codex_home) else {
            return out;
        };
        self.load_names(&home.join("session_index.jsonl"));
        let now = SystemTime::now();
        let mut files = Vec::new();
        rollouts(&home.join("sessions"), 0, &mut files);
        for path in files {
            if !self.heads.contains_key(&path) {
                let head = read_head(&path);
                self.heads.insert(path.clone(), head);
            }
            let Some(Some(head)) = self.heads.get(&path).cloned() else {
                continue;
            };
            if head.child {
                continue;
            }
            let Some(ix) = owning_worktree(worktrees, &head.cwd) else {
                continue;
            };
            if let Some(session) = self.session(&path, &head, now) {
                out[ix].push(session);
            }
        }
        for list in &mut out {
            list.sort_by_key(|s| std::cmp::Reverse(s.modified));
        }
        out
    }

    fn session(&mut self, path: &Path, head: &Head, now: SystemTime) -> Option<Session> {
        let meta = std::fs::metadata(path).ok()?;
        let (len, mtime) = (
            meta.len(),
            meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        );
        let fresh = self
            .tails
            .get(path)
            .is_some_and(|t| t.len == len && t.mtime == mtime);
        if !fresh {
            let tail = read_tail(path, len, mtime)?;
            self.tails.insert(path.to_path_buf(), tail);
        }
        let tail = self.tails.get(path)?;
        let title = self
            .names
            .1
            .get(&head.id)
            .map(|n| clean(n))
            .or_else(|| tail.first_prompt.clone())
            .or_else(|| tail.last_prompt.clone())?;
        Some(Session {
            provider: Provider::Codex,
            id: head.id.clone(),
            transcript: path.to_path_buf(),
            title,
            last_prompt: tail.last_prompt.clone(),
            git_branch: head.git_branch.clone(),
            cwd: tail.cwd.clone().or_else(|| Some(head.cwd.clone())),
            start_cwd: Some(head.cwd.clone()),
            modified: mtime,
            state: classify(tail.last_kind, age(now, mtime)),
            busy: false,
        })
    }

    /// Thread names set in Codex (`/rename`, generated titles); later lines
    /// win.
    fn load_names(&mut self, path: &Path) {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if mtime.is_none() || mtime == self.names.0 {
            return;
        }
        let mut names = HashMap::new();
        if let Ok(file) = File::open(path) {
            for line in BufReader::new(file).lines().map_while(Result::ok) {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let (Some(id), Some(name)) = (
                    v.get("id").and_then(Value::as_str),
                    v.get("thread_name").and_then(Value::as_str),
                ) && !name.trim().is_empty()
                {
                    names.insert(id.to_string(), name.to_string());
                }
            }
        }
        self.names = (mtime, names);
    }
}

/// Rollout files under `sessions/` (year/month/day directories).
fn rollouts(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir && depth < 3 {
            rollouts(&path, depth + 1, out);
        } else if !is_dir
            && path.extension().is_some_and(|e| e == "jsonl")
            && path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("rollout-"))
        {
            out.push(path);
        }
    }
}

fn classify(last: LastKind, age: Duration) -> AgentState {
    let s = age.as_secs();
    match last {
        // Long reasoning writes nothing for a while; a quiet open turn past
        // that is most likely waiting for an approval in a terminal.
        LastKind::TurnOpen if s < 300 => AgentState::Working,
        LastKind::TurnOpen if s < 3_600 => AgentState::NeedsAttention,
        LastKind::TurnDone if s < 3_600 => AgentState::Finished,
        LastKind::Unknown if s < 20 => AgentState::Working,
        _ => AgentState::Idle,
    }
}

fn read_head(path: &Path) -> Option<Head> {
    let file = File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file.take(META_BYTES))
        .read_line(&mut line)
        .ok()?;
    let v: Value = serde_json::from_str(&line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let p = v.get("payload")?;
    let source_is_child = p.get("source").is_some_and(|s| s.get("subagent").is_some());
    let thread_is_child = p.get("thread_source").and_then(Value::as_str) == Some("subagent");
    Some(Head {
        id: p.get("id").and_then(Value::as_str)?.to_string(),
        cwd: PathBuf::from(p.get("cwd").and_then(Value::as_str)?),
        git_branch: p
            .pointer("/git/branch")
            .and_then(Value::as_str)
            .map(String::from),
        child: source_is_child || thread_is_child,
    })
}

fn read_tail(path: &Path, len: u64, mtime: SystemTime) -> Option<Tail> {
    let mut file = File::open(path).ok()?;
    let tail = read_range(&mut file, len.saturating_sub(TAIL_BYTES), len)?;
    let mut last_kind = None;
    let mut cwd = None;
    let mut last_prompt = None;
    // Walk backwards; the first line may be cut, which just fails to parse.
    for line in tail.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ty = v.get("type").and_then(Value::as_str).unwrap_or_default();
        let payload = v.get("payload").unwrap_or(&Value::Null);
        if ty == "turn_context" && cwd.is_none() {
            cwd = payload
                .get("cwd")
                .and_then(Value::as_str)
                .map(PathBuf::from);
        }
        if ty == "event_msg" {
            if last_kind.is_none() {
                last_kind = match payload.get("type").and_then(Value::as_str) {
                    Some("task_started") => Some(LastKind::TurnOpen),
                    Some("task_complete" | "turn_aborted") => Some(LastKind::TurnDone),
                    _ => None,
                };
            }
            if last_prompt.is_none() {
                last_prompt = prompt(payload);
            }
        }
        if last_kind.is_some() && cwd.is_some() && last_prompt.is_some() {
            break;
        }
    }
    let first_prompt = read_range(&mut file, 0, HEAD_BYTES.min(len)).and_then(|head| {
        head.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v.get("type").and_then(Value::as_str) == Some("event_msg"))
            .find_map(|v| prompt(v.get("payload")?))
    });
    Some(Tail {
        len,
        mtime,
        last_kind: last_kind.unwrap_or(LastKind::Unknown),
        cwd,
        first_prompt,
        last_prompt,
    })
}

/// A prompt the user typed, from an `event_msg` payload: `user_message`
/// (older CLIs) or a completed `UserMessage` item.
fn prompt(payload: &Value) -> Option<String> {
    let text = match payload.get("type").and_then(Value::as_str)? {
        "user_message" => payload.get("message")?.as_str()?.to_string(),
        "item_completed" => {
            let item = payload.get("item")?;
            if item.get("type").and_then(Value::as_str) != Some("UserMessage") {
                return None;
            }
            item.get("content")?
                .as_array()?
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        }
        _ => return None,
    };
    let text = clean(&text);
    (!text.is_empty() && !text.starts_with('<')).then_some(text)
}

fn read_range(file: &mut File, start: u64, end: u64) -> Option<String> {
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = vec![0; (end - start) as usize];
    file.read_exact(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, lines: &[Value]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn classifies_by_last_turn_boundary_and_age() {
        let secs = Duration::from_secs;
        assert_eq!(classify(LastKind::TurnOpen, secs(10)), AgentState::Working);
        assert_eq!(
            classify(LastKind::TurnOpen, secs(900)),
            AgentState::NeedsAttention
        );
        assert_eq!(classify(LastKind::TurnDone, secs(60)), AgentState::Finished);
        assert_eq!(classify(LastKind::TurnDone, secs(7_200)), AgentState::Idle);
    }

    #[test]
    fn discovers_sessions_by_cwd_with_titles() {
        use serde_json::json;
        let home = std::env::temp_dir().join(format!("sg_codex_home_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let day = home.join("sessions/2026/11/17");
        let meta = |id: &str, cwd: &str, source: Value| {
            json!({"type": "session_meta", "payload": {"id": id, "cwd": cwd, "source": source,
                "git": {"branch": "feat"}, "base_instructions": {"text": "long"}}})
        };
        let task = |ty: &str| json!({"type": "event_msg", "payload": {"type": ty}});
        write(
            &day.join("rollout-2026-11-17T09-00-00-a.jsonl"),
            &[
                meta("a", "/w/repo-box/src", json!("vscode")),
                task("task_started"),
                json!({"type": "event_msg", "payload": {"type": "item_completed",
                    "item": {"type": "UserMessage", "content": [{"type": "text", "text": "Fix   the bug"}]}}}),
                task("task_complete"),
            ],
        );
        write(
            &day.join("rollout-2026-11-17T09-01-00-b.jsonl"),
            &[
                meta("b", "/w/repo", json!("cli")),
                json!({"type": "event_msg", "payload": {"type": "user_message", "message": "Old format prompt"}}),
                task("task_started"),
            ],
        );
        write(
            &day.join("rollout-2026-11-17T09-02-00-c.jsonl"),
            &[
                meta("c", "/w/repo", json!({"subagent": "review"})),
                task("task_started"),
            ],
        );
        write(
            &day.join("rollout-2026-11-17T09-03-00-d.jsonl"),
            &[meta("d", "/elsewhere", json!("cli")), task("task_complete")],
        );
        std::fs::write(
            home.join("session_index.jsonl"),
            "{\"id\":\"a\",\"thread_name\":\"Old name\"}\n{\"id\":\"a\",\"thread_name\":\"Bug fix\"}\n",
        )
        .unwrap();
        let worktrees = vec![PathBuf::from("/w/repo"), PathBuf::from("/w/repo-box")];
        let by = CodexSessionIndex::with_home(home.clone()).sessions_by_worktree(&worktrees);
        std::fs::remove_dir_all(&home).ok();

        let ids = |list: &[Session]| list.iter().map(|s| s.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&by[0]), ["b"]);
        assert_eq!(ids(&by[1]), ["a"]);
        let a = &by[1][0];
        assert_eq!(a.provider, Provider::Codex);
        assert_eq!(a.title, "Bug fix");
        assert_eq!(a.last_prompt.as_deref(), Some("Fix the bug"));
        assert_eq!(a.git_branch.as_deref(), Some("feat"));
        assert_eq!(a.start_cwd.as_deref(), Some(Path::new("/w/repo-box/src")));
        assert_eq!(a.state, AgentState::Finished);
        let b = &by[0][0];
        assert_eq!(b.title, "Old format prompt");
        assert_eq!(b.state, AgentState::Working);
    }
}
