//! Folding Claude Code events into a list of chat items.
//!
//! The same fold handles live stream-json events and transcript lines from
//! `~/.claude/projects/…/<session>.jsonl` (used to show history on resume):
//! both carry Anthropic-style `message` objects.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
    /// The CLI's structured result (`structuredPatch` for edits, …).
    pub meta: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
    pub result: Option<ToolResult>,
    /// Set when a subagent (Task) made the call.
    pub parent: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    /// Allowed and the CLI's suggested rule applied (e.g. auto-accept edits).
    AllowedForSession,
    Denied,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PermissionRequest {
    pub request_id: String,
    pub tool_name: String,
    pub display_name: String,
    pub description: Option<String>,
    pub input: Value,
    /// `permission_suggestions` from the CLI, sent back as `updatedPermissions`
    /// for "allow for this session".
    pub suggestions: Vec<Value>,
    pub tool_use_id: Option<String>,
    pub decision: Option<Decision>,
}

impl PermissionRequest {
    /// Label for the "allow and remember" button, if the CLI offered a rule.
    pub fn session_allow_label(&self) -> Option<String> {
        let s = self.suggestions.first()?;
        match s.get("type").and_then(Value::as_str)? {
            "setMode" => match s.get("mode").and_then(Value::as_str)? {
                "acceptEdits" => Some("Allow all edits this session".into()),
                mode => Some(format!("Allow · switch to {mode}")),
            },
            "addRules" => Some(format!("Always allow {}", self.display_name)),
            _ => Some("Allow for this session".into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    User {
        text: String,
    },
    Text {
        text: String,
        streaming: bool,
    },
    Thinking {
        streaming: bool,
    },
    Tool(ToolCall),
    Permission(PermissionRequest),
    Notice {
        text: String,
        is_error: bool,
    },
    TurnEnd {
        duration_ms: Option<u64>,
        is_error: bool,
        message: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    /// No turn running.
    #[default]
    Idle,
    /// Process starting / turn submitted, nothing streamed yet.
    Starting,
    Working,
    /// A tool permission prompt is waiting for the user.
    NeedsPermission,
    /// The process ended (it is restarted with `--resume` on the next turn).
    Exited,
}

impl Status {
    pub fn is_busy(self) -> bool {
        matches!(self, Self::Starting | Self::Working | Self::NeedsPermission)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Conversation {
    pub items: Vec<Item>,
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<PathBuf>,
    pub permission_mode: Option<String>,
    pub status: Status,
    pub total_cost_usd: Option<f64>,
    streaming_text: Option<usize>,
    streaming_thinking: Option<usize>,
}

/// What the host must do after an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// A control request we do not handle: answer with an error so the CLI
    /// does not wait forever.
    Unsupported { request_id: String, subtype: String },
}

impl Conversation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Local echo of a turn the user just sent.
    pub fn push_user(&mut self, text: &str) {
        if self.title.is_none() {
            self.title = Some(short_title(text));
        }
        self.items.push(Item::User {
            text: text.to_string(),
        });
        if !self.status.is_busy() {
            self.status = Status::Starting;
        }
    }

    pub fn pending_permission(&self) -> Option<&PermissionRequest> {
        self.items.iter().rev().find_map(|i| match i {
            Item::Permission(p) if p.decision.is_none() => Some(p),
            _ => None,
        })
    }

    pub fn resolve_permission(&mut self, request_id: &str, decision: Decision) {
        for item in self.items.iter_mut().rev() {
            if let Item::Permission(p) = item
                && p.request_id == request_id
            {
                p.decision = Some(decision);
                break;
            }
        }
        if self.pending_permission().is_none() && self.status == Status::NeedsPermission {
            self.status = Status::Working;
        }
    }

    pub fn tool(&self, id: &str) -> Option<&ToolCall> {
        self.items.iter().find_map(|i| match i {
            Item::Tool(t) if t.id == id => Some(t),
            _ => None,
        })
    }

    /// The process exited (crash, `/exit`, killed).
    pub fn on_exit(&mut self, stderr_tail: Option<String>) {
        self.finish_streaming();
        if self.status.is_busy() {
            self.items.push(Item::Notice {
                text: stderr_tail.unwrap_or_else(|| "Claude exited".into()),
                is_error: true,
            });
        }
        self.status = Status::Exited;
    }

    fn finish_streaming(&mut self) {
        for ix in [self.streaming_text.take(), self.streaming_thinking.take()]
            .into_iter()
            .flatten()
        {
            if let Some(Item::Text { streaming, .. } | Item::Thinking { streaming }) =
                self.items.get_mut(ix)
            {
                *streaming = false
            }
        }
    }

    /// Apply one live stream-json event.
    pub fn apply(&mut self, v: &Value) -> Option<Reply> {
        self.apply_inner(v, true)
    }

    fn apply_inner(&mut self, v: &Value, live: bool) -> Option<Reply> {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or_default();
        match ty {
            "system" => self.on_system(v),
            "stream_event" => self.on_stream_event(v),
            "assistant" => self.on_assistant(v),
            "user" => self.on_user(v, live),
            "control_request" => return self.on_control_request(v),
            "result" => self.on_result(v),
            _ => {}
        }
        None
    }

    fn on_system(&mut self, v: &Value) {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
        match v.get("subtype").and_then(Value::as_str).unwrap_or_default() {
            "init" => {
                self.session_id = s("session_id").or(self.session_id.take());
                self.model = s("model");
                self.cwd = s("cwd").map(PathBuf::from);
                self.permission_mode = s("permissionMode");
            }
            "compact_boundary" => self.items.push(Item::Notice {
                text: "Conversation compacted".into(),
                is_error: false,
            }),
            _ => {}
        }
    }

    fn on_stream_event(&mut self, v: &Value) {
        if !v.get("parent_tool_use_id").is_none_or(Value::is_null) {
            return; // subagent internals stay folded into their Task call
        }
        let Some(ev) = v.get("event") else { return };
        match ev.get("type").and_then(Value::as_str).unwrap_or_default() {
            "content_block_start" => {
                self.status = Status::Working;
                match ev.pointer("/content_block/type").and_then(Value::as_str) {
                    Some("text") => {
                        self.items.push(Item::Text {
                            text: String::new(),
                            streaming: true,
                        });
                        self.streaming_text = Some(self.items.len() - 1);
                    }
                    Some("thinking") => {
                        self.items.push(Item::Thinking { streaming: true });
                        self.streaming_thinking = Some(self.items.len() - 1);
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                if ev.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta")
                    && let Some(ix) = self.streaming_text
                    && let Some(Item::Text { text, .. }) = self.items.get_mut(ix)
                    && let Some(delta) = ev.pointer("/delta/text").and_then(Value::as_str)
                {
                    text.push_str(delta);
                }
            }
            _ => {}
        }
    }

    fn on_assistant(&mut self, v: &Value) {
        let parent = v
            .get("parent_tool_use_id")
            .and_then(Value::as_str)
            .map(String::from);
        let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) else {
            return;
        };
        if self.status != Status::NeedsPermission {
            self.status = Status::Working;
        }
        for block in blocks {
            match block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "text" if parent.is_none() => {
                    let full = block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    match self.streaming_text.take() {
                        Some(ix) => {
                            if let Some(Item::Text { text, streaming }) = self.items.get_mut(ix) {
                                *text = full.to_string();
                                *streaming = false;
                            }
                        }
                        None if !full.trim().is_empty() => self.items.push(Item::Text {
                            text: full.to_string(),
                            streaming: false,
                        }),
                        None => {}
                    }
                }
                "thinking" | "redacted_thinking" if parent.is_none() => {
                    match self.streaming_thinking.take() {
                        Some(ix) => {
                            if let Some(Item::Thinking { streaming }) = self.items.get_mut(ix) {
                                *streaming = false;
                            }
                        }
                        None => self.items.push(Item::Thinking { streaming: false }),
                    }
                }
                "tool_use" => {
                    let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                    if self.tool(id).is_none() {
                        self.items.push(Item::Tool(ToolCall {
                            id: id.to_string(),
                            name: block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string(),
                            input: block.get("input").cloned().unwrap_or(Value::Null),
                            result: None,
                            parent: parent.clone(),
                        }));
                    }
                }
                _ => {}
            }
        }
    }

    fn on_user(&mut self, v: &Value, live: bool) {
        let Some(content) = v.pointer("/message/content") else {
            return;
        };
        // Live mode echoes the user's own turns locally; transcripts replay them.
        if let Some(text) = content.as_str() {
            if !live {
                self.push_history_prompt(text);
            }
            return;
        }
        let meta = v.get("tool_use_result").or_else(|| v.get("toolUseResult"));
        for block in content.as_array().into_iter().flatten() {
            match block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "tool_result" => {
                    let id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let result = ToolResult {
                        text: crate::tools::result_text(
                            block.get("content").unwrap_or(&Value::Null),
                        ),
                        is_error: block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        meta: meta.filter(|m| m.is_object()).cloned(),
                    };
                    for item in self.items.iter_mut().rev() {
                        if let Item::Tool(t) = item
                            && t.id == id
                        {
                            t.result = Some(result);
                            break;
                        }
                    }
                }
                "text" if !live => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        self.push_history_prompt(text);
                    }
                }
                _ => {}
            }
        }
    }

    fn push_history_prompt(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.starts_with("[Request interrupted") {
            self.items.push(Item::Notice {
                text: "Interrupted".into(),
                is_error: false,
            });
        } else if !trimmed.is_empty() && !trimmed.starts_with('<') {
            if self.title.is_none() {
                self.title = Some(short_title(trimmed));
            }
            self.items.push(Item::User {
                text: trimmed.to_string(),
            });
        }
    }

    fn on_control_request(&mut self, v: &Value) -> Option<Reply> {
        let request_id = v.get("request_id").and_then(Value::as_str)?.to_string();
        let req = v.get("request")?;
        let subtype = req
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if subtype != "can_use_tool" {
            return Some(Reply::Unsupported {
                request_id,
                subtype: subtype.to_string(),
            });
        }
        let s = |k: &str| req.get(k).and_then(Value::as_str).map(String::from);
        let tool_name = s("tool_name").unwrap_or_else(|| "tool".into());
        self.items.push(Item::Permission(PermissionRequest {
            request_id,
            display_name: s("display_name").unwrap_or_else(|| tool_name.clone()),
            tool_name,
            description: s("description").or_else(|| s("title")),
            input: req.get("input").cloned().unwrap_or(Value::Null),
            suggestions: req
                .get("permission_suggestions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            tool_use_id: s("tool_use_id"),
            decision: None,
        }));
        self.status = Status::NeedsPermission;
        None
    }

    fn on_result(&mut self, v: &Value) {
        self.finish_streaming();
        let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        let message = is_error.then(|| {
            v.get("result")
                .and_then(Value::as_str)
                .map(String::from)
                .or_else(|| v.get("subtype").and_then(Value::as_str).map(String::from))
                .unwrap_or_else(|| "error".into())
        });
        if let Some(cost) = v.get("total_cost_usd").and_then(Value::as_f64) {
            self.total_cost_usd = Some(cost);
        }
        self.items.push(Item::TurnEnd {
            duration_ms: v.get("duration_ms").and_then(Value::as_u64),
            is_error,
            message,
        });
        // Unanswered prompts of a finished turn are moot.
        for item in &mut self.items {
            if let Item::Permission(p) = item
                && p.decision.is_none()
            {
                p.decision = Some(Decision::Denied);
            }
        }
        self.status = Status::Idle;
    }

    /// Rebuild a conversation from a session transcript (for resume).
    pub fn from_transcript(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut convo = Self::new();
        convo.session_id = path.file_stem().map(|s| s.to_string_lossy().into_owned());
        let mut ai_title = None;
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let ty = v.get("type").and_then(Value::as_str).unwrap_or_default();
            if ty == "ai-title" {
                ai_title = v.get("aiTitle").and_then(Value::as_str).map(String::from);
                continue;
            }
            let skip = ["isSidechain", "isMeta", "isCompactSummary"]
                .iter()
                .any(|k| v.get(*k).and_then(Value::as_bool) == Some(true));
            if skip || !matches!(ty, "user" | "assistant") {
                continue;
            }
            if convo.cwd.is_none() {
                convo.cwd = v.get("cwd").and_then(Value::as_str).map(PathBuf::from);
            }
            if ty == "assistant" && convo.model.is_none() {
                convo.model = v
                    .pointer("/message/model")
                    .and_then(Value::as_str)
                    .map(String::from);
            }
            convo.apply_inner(&v, false);
        }
        if ai_title.is_some() {
            convo.title = ai_title;
        }
        convo.finish_streaming();
        convo.status = Status::Idle;
        Ok(convo)
    }
}

fn short_title(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 48 {
        format!("{}…", line.chars().take(47).collect::<String>())
    } else {
        line
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn feed(c: &mut Conversation, events: &[Value]) -> Vec<Reply> {
        events.iter().filter_map(|e| c.apply(e)).collect()
    }

    #[test]
    fn streams_text_then_finalizes_without_duplicates() {
        let mut c = Conversation::new();
        c.push_user("hi there");
        assert_eq!(c.status, Status::Starting);
        feed(
            &mut c,
            &[
                json!({"type":"system","subtype":"init","session_id":"s1","model":"claude-opus-5-5","cwd":"/r","permissionMode":"default"}),
                json!({"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}),
                json!({"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}}),
                json!({"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}}),
            ],
        );
        assert_eq!(
            c.items[1],
            Item::Text {
                text: "Hello".into(),
                streaming: true
            }
        );
        assert_eq!(c.status, Status::Working);
        feed(
            &mut c,
            &[
                json!({"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"text","text":"Hello!"}]}}),
                json!({"type":"result","subtype":"success","is_error":false,"duration_ms":1200,"total_cost_usd":0.01}),
            ],
        );
        assert_eq!(c.items.len(), 3);
        assert_eq!(
            c.items[1],
            Item::Text {
                text: "Hello!".into(),
                streaming: false
            }
        );
        assert!(matches!(
            c.items[2],
            Item::TurnEnd {
                duration_ms: Some(1200),
                is_error: false,
                ..
            }
        ));
        assert_eq!(c.status, Status::Idle);
        assert_eq!(c.session_id.as_deref(), Some("s1"));
        assert_eq!(c.title.as_deref(), Some("hi there"));
    }

    #[test]
    fn tool_calls_permissions_and_results() {
        let mut c = Conversation::new();
        c.push_user("write it");
        let replies = feed(
            &mut c,
            &[
                json!({"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"tool_use","id":"t1","name":"Write","input":{"file_path":"/r/a.txt","content":"x"}}]}}),
                json!({"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Write","display_name":"Write","description":"a.txt","input":{"file_path":"/r/a.txt","content":"x"},"permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}],"tool_use_id":"t1"}}),
                json!({"type":"control_request","request_id":"r2","request":{"subtype":"hook_callback"}}),
            ],
        );
        assert_eq!(
            replies,
            vec![Reply::Unsupported {
                request_id: "r2".into(),
                subtype: "hook_callback".into()
            }]
        );
        assert_eq!(c.status, Status::NeedsPermission);
        let p = c.pending_permission().unwrap();
        assert_eq!(p.description.as_deref(), Some("a.txt"));
        assert_eq!(
            p.session_allow_label().as_deref(),
            Some("Allow all edits this session")
        );

        c.resolve_permission("r1", Decision::Allowed);
        assert_eq!(c.status, Status::Working);
        feed(
            &mut c,
            &[
                json!({"type":"user","parent_tool_use_id":null,"tool_use_result":{"type":"create"},"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"File created"}]}}),
            ],
        );
        let t = c.tool("t1").unwrap();
        assert_eq!(t.result.as_ref().unwrap().text, "File created");
        assert!(t.result.as_ref().unwrap().meta.is_some());
    }

    #[test]
    fn rebuilds_history_from_transcript() {
        let dir = std::env::temp_dir().join(format!("sg_claude_tr_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sess-1.jsonl");
        let lines = [
            json!({"type":"user","cwd":"/r","message":{"role":"user","content":"Fix the bug"}}),
            json!({"type":"user","isMeta":true,"message":{"role":"user","content":"<system-reminder>x</system-reminder>"}}),
            json!({"type":"assistant","message":{"model":"claude-opus-5-5","content":[{"type":"thinking","thinking":""}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}),
            json!({"type":"user","toolUseResult":{"stdout":"ok"},"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}]}}),
            json!({"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"subagent noise"}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"Fixed."}]}}),
            json!({"type":"ai-title","aiTitle":"Bug fix"}),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let c = Conversation::from_transcript(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(c.session_id.as_deref(), Some("sess-1"));
        assert_eq!(c.title.as_deref(), Some("Bug fix"));
        assert_eq!(c.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(c.cwd.as_deref(), Some(Path::new("/r")));
        assert_eq!(c.status, Status::Idle);
        let kinds: Vec<&str> = c
            .items
            .iter()
            .map(|i| match i {
                Item::User { .. } => "user",
                Item::Text { .. } => "text",
                Item::Thinking { .. } => "thinking",
                Item::Tool(_) => "tool",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["user", "thinking", "tool", "text"]);
        assert_eq!(c.tool("t1").unwrap().result.as_ref().unwrap().text, "ok");
    }

    #[test]
    fn exit_while_busy_reports() {
        let mut c = Conversation::new();
        c.push_user("go");
        c.on_exit(Some("boom".into()));
        assert_eq!(c.status, Status::Exited);
        assert!(matches!(
            c.items.last(),
            Some(Item::Notice { is_error: true, .. })
        ));
    }
}
