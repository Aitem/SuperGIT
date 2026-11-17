//! Coding agents, independent of any one provider and of the UI.
//!
//! - [`Provider`]: which agent (Claude Code, Codex); sessions are identified
//!   by `(provider, session id)`.
//! - [`chat`]: the conversation model every adapter folds its events into.
//! - [`process`]: what the UI needs from a running agent.
//! - Claude Code session discovery (this module), read from the transcripts
//!   Claude Code already writes to `~/.claude/projects/<encoded
//!   cwd>/<session-id>.jsonl`. Codex discovery lives in `sg_codex`. Nothing in
//!   the user's agent configuration has to change for this to work.
//!
//! Status is a heuristic over the last conversational entry and the file's
//! modification time; precise state (hooks over a socket) is a later step.

pub mod chat;
pub mod process;

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

/// A coding agent SuperGIT can drive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    #[default]
    Claude,
    Codex,
}

impl Provider {
    pub const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    /// Stable name for persisted state (`claude`, `codex`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// Parse a persisted name; empty means Claude (state written before
    /// providers existed).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "" | "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }

    /// Marks the provider on tabs, rails and lists.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Claude => "✦",
            Self::Codex => "◆",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentState {
    /// A tool call is waiting for the user (permission prompt). Ordered first:
    /// the most urgent state wins when several sessions are combined.
    NeedsAttention,
    /// Claude is producing output or running a tool right now.
    Working,
    /// The turn ended; Claude is waiting for your next message.
    Finished,
    /// Nothing recent.
    Idle,
}

impl AgentState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::NeedsAttention => "needs you",
            Self::Finished => "done",
            Self::Idle => "idle",
        }
    }

    /// Whether this state should draw the user's eye in lists.
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Idle)
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub provider: Provider,
    pub id: String,
    /// Where the provider keeps the conversation (Claude transcript, Codex
    /// rollout).
    pub transcript: PathBuf,
    /// AI title, else the last prompt, else the session id.
    pub title: String,
    pub last_prompt: Option<String>,
    pub git_branch: Option<String>,
    /// Where the agent works now (the last `cwd` in the transcript).
    pub cwd: Option<PathBuf>,
    /// Where the agent was started: resume must run from here.
    pub start_cwd: Option<PathBuf>,
    pub modified: SystemTime,
    pub state: AgentState,
}

impl Session {
    /// Session ids are unique per provider only.
    pub fn is(&self, provider: Provider, id: &str) -> bool {
        self.provider == provider && self.id == id
    }
}

/// Assign sessions to the worktree that contains their `cwd` (longest match
/// wins: worktrees may nest).
pub fn owning_worktree(worktrees: &[PathBuf], cwd: &Path) -> Option<usize> {
    worktrees
        .iter()
        .enumerate()
        .filter(|(_, w)| cwd.starts_with(w))
        .max_by_key(|(_, w)| w.as_os_str().len())
        .map(|(ix, _)| ix)
}

/// `~/.claude`, honouring `CLAUDE_CONFIG_DIR`.
pub fn claude_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude"))
}

/// Claude Code stores a project's transcripts under its cwd with every
/// non-alphanumeric character replaced by `-`.
pub fn encode_project_path(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

pub fn project_dir(worktree: &Path) -> Option<PathBuf> {
    Some(
        claude_home()?
            .join("projects")
            .join(encode_project_path(worktree)),
    )
}

/// Caches parsed transcripts by (size, mtime) so polling dozens of sessions
/// every few seconds only re-reads files that actually changed.
#[derive(Default)]
pub struct SessionIndex {
    cache: HashMap<PathBuf, Cached>,
}

struct Cached {
    len: u64,
    mtime: SystemTime,
    last_kind: LastKind,
    session: Session,
}

impl SessionIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claude sessions for each of `worktrees` (same order), most recently
    /// active first.
    ///
    /// A transcript lives under the directory Claude was *started* in, and
    /// that name is ambiguous (`repo/box` and a sibling worktree `repo-box`
    /// encode the same). So every project directory whose name starts with
    /// a worktree's encoded path is a candidate, and each session goes to the
    /// worktree that contains its actual `cwd` (longest match wins).
    pub fn sessions_by_worktree(&mut self, worktrees: &[PathBuf]) -> Vec<Vec<Session>> {
        let mut out = vec![Vec::new(); worktrees.len()];
        let Some(projects) = claude_home().map(|h| h.join("projects")) else {
            return out;
        };
        let Ok(dirs) = std::fs::read_dir(&projects) else {
            return out;
        };
        let encoded: Vec<String> = worktrees.iter().map(|w| encode_project_path(w)).collect();
        let now = SystemTime::now();
        for dir in dirs.flatten() {
            let name = dir.file_name().to_string_lossy().into_owned();
            let Some(home_ix) = encoded.iter().position(|e| name == *e) else {
                if !encoded.iter().any(|e| name.starts_with(e.as_str())) {
                    continue;
                }
                self.collect_dir(&dir.path(), None, worktrees, now, &mut out);
                continue;
            };
            self.collect_dir(&dir.path(), Some(home_ix), worktrees, now, &mut out);
        }
        for list in &mut out {
            list.sort_by_key(|s| std::cmp::Reverse(s.modified));
        }
        out
    }

    fn collect_dir(
        &mut self,
        dir: &Path,
        home_ix: Option<usize>,
        worktrees: &[PathBuf],
        now: SystemTime,
        out: &mut [Vec<Session>],
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let Some(session) = self.session(&path, now) else {
                continue;
            };
            let owner = match &session.cwd {
                Some(cwd) => owning_worktree(worktrees, cwd),
                None => home_ix,
            };
            if let Some(ix) = owner {
                out[ix].push(session);
            }
        }
    }

    /// Parse (or reuse) one transcript.
    fn session(&mut self, path: &Path, now: SystemTime) -> Option<Session> {
        let meta = std::fs::metadata(path).ok()?;
        let (len, mtime) = (
            meta.len(),
            meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        );
        if let Some(c) = self.cache.get(path)
            && c.len == len
            && c.mtime == mtime
        {
            let mut s = c.session.clone();
            // Time moves on even when the file does not.
            s.state = classify(c.last_kind, age(now, mtime));
            return Some(s);
        }
        let (session, last_kind) = parse_session(path, mtime, now)?;
        self.cache.insert(
            path.to_path_buf(),
            Cached {
                len,
                mtime,
                last_kind,
                session: session.clone(),
            },
        );
        Some(session)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LastKind {
    AssistantEndTurn,
    AssistantToolUse,
    UserOrToolResult,
    Unknown,
}

/// How long ago a transcript was written.
pub fn age(now: SystemTime, mtime: SystemTime) -> Duration {
    now.duration_since(mtime).unwrap_or_default()
}

fn classify(last: LastKind, age: Duration) -> AgentState {
    let s = age.as_secs();
    match last {
        LastKind::AssistantEndTurn if s < 3_600 => AgentState::Finished,
        LastKind::AssistantToolUse if s < 20 => AgentState::Working,
        LastKind::AssistantToolUse if s < 3_600 => AgentState::NeedsAttention,
        LastKind::UserOrToolResult if s < 120 => AgentState::Working,
        LastKind::Unknown if s < 20 => AgentState::Working,
        _ => AgentState::Idle,
    }
}

const TAIL_BYTES: u64 = 512 * 1024;
const HEAD_BYTES: u64 = 256 * 1024;
const START_BYTES: u64 = 64 * 1024;

fn parse_session(path: &Path, mtime: SystemTime, now: SystemTime) -> Option<(Session, LastKind)> {
    let id = path.file_stem()?.to_string_lossy().into_owned();
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();

    let tail = read_range(&mut file, len.saturating_sub(TAIL_BYTES), len)?;
    let mut ai_title = None;
    let mut last_prompt = None;
    let mut git_branch = None;
    let mut cwd = None;
    let mut last_kind = None;

    // Walk the tail backwards; the first line may be cut, which just fails to parse.
    for line in tail.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ty = v.get("type").and_then(Value::as_str).unwrap_or_default();
        match ty {
            "ai-title" if ai_title.is_none() => {
                ai_title = v.get("aiTitle").and_then(Value::as_str).map(clean);
            }
            "last-prompt" if last_prompt.is_none() => {
                last_prompt = v.get("lastPrompt").and_then(Value::as_str).map(clean);
            }
            "user" | "assistant" => {
                if git_branch.is_none() {
                    git_branch = v.get("gitBranch").and_then(Value::as_str).map(String::from);
                }
                if cwd.is_none() {
                    cwd = v.get("cwd").and_then(Value::as_str).map(PathBuf::from);
                }
                if last_kind.is_none() && v.get("isSidechain") != Some(&Value::Bool(true)) {
                    last_kind = Some(kind_of(ty, &v));
                }
                if ty == "user" && last_prompt.is_none() {
                    last_prompt = prompt_text(&v);
                }
            }
            _ => {}
        }
        if ai_title.is_some()
            && last_prompt.is_some()
            && last_kind.is_some()
            && git_branch.is_some()
            && cwd.is_some()
        {
            break;
        }
    }

    // Short or brand-new sessions: take the first prompt from the head.
    if ai_title.is_none()
        && last_prompt.is_none()
        && len > TAIL_BYTES
        && let Some(head) = read_range(&mut file, 0, HEAD_BYTES.min(len))
    {
        last_prompt = head
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v.get("type").and_then(Value::as_str) == Some("user"))
            .find_map(|v| prompt_text(&v));
    }

    // The first `cwd` is where Claude was launched; resume needs exactly it.
    let start_cwd = read_range(&mut file, 0, START_BYTES.min(len)).and_then(|head| {
        head.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find_map(|v| v.get("cwd").and_then(Value::as_str).map(PathBuf::from))
    });

    // Transcripts with no conversation (e.g. only metadata) are not sessions.
    let last_kind = last_kind?;

    let title = ai_title
        .clone()
        .or_else(|| last_prompt.clone())
        .unwrap_or_else(|| id.clone());
    let session = Session {
        provider: Provider::Claude,
        id,
        transcript: path.to_path_buf(),
        title,
        last_prompt,
        git_branch,
        cwd,
        start_cwd,
        modified: mtime,
        state: classify(last_kind, age(now, mtime)),
    };
    Some((session, last_kind))
}

fn kind_of(ty: &str, v: &Value) -> LastKind {
    if ty == "assistant" {
        match v.pointer("/message/stop_reason").and_then(Value::as_str) {
            Some("end_turn") | Some("stop_sequence") | Some("max_tokens") | Some("refusal") => {
                LastKind::AssistantEndTurn
            }
            Some("tool_use") => LastKind::AssistantToolUse,
            _ => LastKind::Unknown,
        }
    } else {
        LastKind::UserOrToolResult
    }
}

/// Text of a real user prompt (not a tool result or a meta message).
fn prompt_text(v: &Value) -> Option<String> {
    if v.get("isMeta") == Some(&Value::Bool(true)) || v.get("toolUseResult").is_some() {
        return None;
    }
    let content = v.pointer("/message/content")?;
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let text = clean(&text);
    // Slash-command and hook wrappers are not what the user typed.
    (!text.is_empty() && !text.starts_with('<')).then_some(text)
}

/// One line of at most 140 characters, for titles and prompts in lists.
pub fn clean(s: &str) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > 140 {
        let cut: String = one_line.chars().take(140).collect();
        format!("{cut}…")
    } else {
        one_line
    }
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

    #[test]
    fn encodes_like_claude_code() {
        assert_eq!(
            encode_project_path(Path::new("/Users/me/Work/my-app")),
            "-Users-me-Work-my-app"
        );
        assert_eq!(encode_project_path(Path::new("/a/.b_c")), "-a--b-c");
    }

    #[test]
    fn classifies_by_last_entry_and_age() {
        let secs = Duration::from_secs;
        assert_eq!(
            classify(LastKind::AssistantToolUse, secs(3)),
            AgentState::Working
        );
        assert_eq!(
            classify(LastKind::AssistantToolUse, secs(90)),
            AgentState::NeedsAttention
        );
        assert_eq!(
            classify(LastKind::AssistantEndTurn, secs(90)),
            AgentState::Finished
        );
        assert_eq!(
            classify(LastKind::UserOrToolResult, secs(30)),
            AgentState::Working
        );
        assert_eq!(
            classify(LastKind::UserOrToolResult, secs(600)),
            AgentState::Idle
        );
        assert_eq!(
            classify(LastKind::AssistantEndTurn, secs(7_200)),
            AgentState::Idle
        );
    }

    #[test]
    fn attributes_sessions_by_cwd_not_dir_name() {
        let home = std::env::temp_dir().join(format!("sg_agents_home_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        // `/w/repo/box` (subdir of main) and `/w/repo-box` (a worktree) encode the same.
        let dir = home.join("projects").join("-w-repo-box");
        std::fs::create_dir_all(&dir).unwrap();
        let line = |cwd: &str| {
            format!(
                r#"{{"type":"assistant","cwd":"{cwd}","message":{{"stop_reason":"end_turn","content":[]}}}}"#
            )
        };
        std::fs::write(dir.join("a.jsonl"), line("/w/repo/box") + "\n").unwrap();
        std::fs::write(dir.join("b.jsonl"), line("/w/repo-box/src") + "\n").unwrap();
        std::fs::write(dir.join("c.jsonl"), line("/elsewhere") + "\n").unwrap();
        // SAFETY: tests in this crate do not read CLAUDE_CONFIG_DIR concurrently.
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &home) };
        let worktrees = vec![PathBuf::from("/w/repo"), PathBuf::from("/w/repo-box")];
        let by = SessionIndex::new().sessions_by_worktree(&worktrees);
        // SAFETY: as above.
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        std::fs::remove_dir_all(&home).ok();
        assert_eq!(
            by[0].iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["a"]
        );
        assert_eq!(
            by[1].iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["b"]
        );
    }

    #[test]
    fn parses_transcript() {
        let dir = std::env::temp_dir().join(format!("sg_agents_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc-123.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"user","gitBranch":"feat","message":{"role":"user","content":"Fix the   login bug"}}"#, "\n",
                r#"{"type":"assistant","gitBranch":"feat","message":{"stop_reason":"tool_use","content":[]}}"#, "\n",
                r#"{"type":"user","toolUseResult":{},"message":{"content":[{"type":"tool_result"}]}}"#, "\n",
                r#"{"type":"ai-title","aiTitle":"Login bug fix"}"#, "\n",
                r#"{"type":"assistant","gitBranch":"feat","message":{"stop_reason":"end_turn","content":[]}}"#, "\n",
            ),
        )
        .unwrap();
        let now = SystemTime::now();
        let (s, _) = parse_session(&path, now, now).unwrap();
        assert_eq!(s.id, "abc-123");
        assert_eq!(s.title, "Login bug fix");
        assert_eq!(s.last_prompt.as_deref(), Some("Fix the login bug"));
        assert_eq!(s.git_branch.as_deref(), Some("feat"));
        assert_eq!(s.state, AgentState::Finished);
        std::fs::remove_dir_all(&dir).ok();
    }
}
