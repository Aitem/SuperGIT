//! Folding Claude Code events into the shared chat model.
//!
//! The same fold handles live stream-json events and transcript lines from
//! `~/.claude/projects/…/<session>.jsonl` (used to show history on resume):
//! both carry Anthropic-style `message` objects.

use std::fs::File;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;
pub use sg_agents::chat::{
    Answers, Conversation, Decision, InputResolution, Item, PermissionRequest, Status, ToolCall,
    ToolResult, UserInputRequest, strip_ansi,
};

/// Label for the "allow and remember" button, if the CLI offered a rule.
fn session_allow_label(display_name: &str, suggestions: &[Value]) -> Option<String> {
    let s = suggestions.first()?;
    match s.get("type").and_then(Value::as_str)? {
        "setMode" => match s.get("mode").and_then(Value::as_str)? {
            "acceptEdits" => Some("Allow all edits this session".into()),
            mode => Some(format!(
                "Allow · switch to {}",
                crate::PermissionMode::from_wire(mode).map_or(mode, |m| m.label())
            )),
        },
        "addRules" => Some(format!("Always allow {display_name}")),
        _ => Some("Allow for this session".into()),
    }
}

/// What the host must do after an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// A control request we do not handle: answer with an error so the CLI
    /// does not wait forever.
    Unsupported { request_id: String, subtype: String },
    /// The CLI answered one of our control requests (`set_permission_mode`,
    /// …); `error` is set when it refused.
    ControlResponse {
        request_id: String,
        error: Option<String>,
    },
}

/// Claude Code's events folded into a [`Conversation`].
pub trait ClaudeEvents {
    /// Apply one live stream-json event.
    fn apply(&mut self, v: &Value) -> Option<Reply>;
}

impl ClaudeEvents for Conversation {
    fn apply(&mut self, v: &Value) -> Option<Reply> {
        apply_inner(self, v, true)
    }
}

fn apply_inner(c: &mut Conversation, v: &Value, live: bool) -> Option<Reply> {
    let ty = v.get("type").and_then(Value::as_str).unwrap_or_default();
    match ty {
        "system" => on_system(c, v),
        "stream_event" => on_stream_event(c, v),
        "assistant" => on_assistant(c, v),
        "user" => on_user(c, v, live),
        "control_request" => return on_control_request(c, v),
        "control_response" => return control_response(v),
        "result" => on_result(c, v),
        _ => {}
    }
    None
}

fn on_system(c: &mut Conversation, v: &Value) {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
    match v.get("subtype").and_then(Value::as_str).unwrap_or_default() {
        "init" => {
            c.session_id = s("session_id").or(c.session_id.take());
            c.model = s("model");
            c.cwd = s("cwd").map(PathBuf::from);
            c.permission_mode = s("permissionMode");
        }
        // Mode changes: our `set_permission_mode`, a plan approved, …
        "status" => {
            if let Some(mode) = s("permissionMode") {
                c.permission_mode = Some(mode);
            }
        }
        "compact_boundary" => c.push_notice("Conversation compacted", false),
        "permission_check_status" => {
            let checking = v.get("status").and_then(Value::as_str) == Some("checking");
            if let Some(id) = s("tool_use_id") {
                c.set_checking(&id, checking);
            }
        }
        // A call refused without asking: auto mode's safety check, a deny
        // rule, dontAsk mode. The tool's error result follows.
        "permission_denied" => {
            if let Some(id) = s("tool_use_id") {
                c.set_checking(&id, false);
            }
            let tool = s("tool_name").unwrap_or_else(|| "tool".into());
            let head = if s("decision_reason_type").as_deref() == Some("classifier") {
                format!("Auto mode blocked {tool}")
            } else {
                format!("{tool} was denied")
            };
            let why = s("decision_reason")
                .or_else(|| s("message"))
                .map(|r| strip_ansi(&r))
                .filter(|r| !r.trim().is_empty());
            c.push_notice(
                match why {
                    Some(why) => format!("{head}: {}", why.trim()),
                    None => head,
                },
                true,
            );
        }
        _ => {}
    }
}

fn on_stream_event(c: &mut Conversation, v: &Value) {
    if !v.get("parent_tool_use_id").is_none_or(Value::is_null) {
        return; // subagent internals stay folded into their Task call
    }
    let Some(ev) = v.get("event") else { return };
    match ev.get("type").and_then(Value::as_str).unwrap_or_default() {
        "content_block_start" => {
            c.status = Status::Working;
            match ev.pointer("/content_block/type").and_then(Value::as_str) {
                Some("text") => c.start_text(),
                Some("thinking") => c.start_thinking(),
                _ => {}
            }
        }
        "content_block_delta" => {
            if ev.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta")
                && let Some(delta) = ev.pointer("/delta/text").and_then(Value::as_str)
            {
                c.append_text(delta);
            }
        }
        _ => {}
    }
}

fn on_assistant(c: &mut Conversation, v: &Value) {
    let parent = v
        .get("parent_tool_use_id")
        .and_then(Value::as_str)
        .map(String::from);
    let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) else {
        return;
    };
    if c.status != Status::NeedsPermission {
        c.status = Status::Working;
    }
    for block in blocks {
        match block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" if parent.is_none() => c.complete_text(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            "thinking" | "redacted_thinking" if parent.is_none() => c.complete_thinking(),
            "tool_use" => {
                let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                if c.tool(id).is_none() {
                    c.items.push(Item::Tool(ToolCall {
                        id: id.to_string(),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_string(),
                        input: block.get("input").cloned().unwrap_or(Value::Null),
                        result: None,
                        parent: parent.clone(),
                        checking: false,
                    }));
                }
            }
            _ => {}
        }
    }
}

fn on_user(c: &mut Conversation, v: &Value, live: bool) {
    let Some(content) = v.pointer("/message/content") else {
        return;
    };
    // Live mode echoes the user's own turns locally; transcripts replay them.
    if let Some(text) = content.as_str() {
        if !live {
            push_history_prompt(c, text);
        }
        return;
    }
    // The CLI's own result repeats the tool output and whole files (Read's
    // `file.content`, Edit's `originalFile`); only edits' `structuredPatch`
    // is shown, so keep just that: long sessions would hold megabytes.
    let meta = v
        .get("tool_use_result")
        .or_else(|| v.get("toolUseResult"))
        .and_then(|result| result.get("structuredPatch"))
        .map(|patch| serde_json::json!({ "structuredPatch": patch }));
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
                    text: crate::tools::result_text(block.get("content").unwrap_or(&Value::Null)),
                    is_error: block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    meta: meta.clone(),
                };
                if let Some(t) = c.tool_mut(id) {
                    t.result = Some(result);
                    t.checking = false;
                }
            }
            "text" if !live => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    push_history_prompt(c, text);
                }
            }
            _ => {}
        }
    }
}

fn push_history_prompt(c: &mut Conversation, text: &str) {
    let trimmed = text.trim();
    if trimmed.starts_with("[Request interrupted") {
        c.push_notice("Interrupted", false);
    } else if !trimmed.is_empty() && !trimmed.starts_with('<') {
        c.push_history_user(trimmed);
    }
}

fn on_control_request(c: &mut Conversation, v: &Value) -> Option<Reply> {
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
    if let Some(id) = s("tool_use_id") {
        c.set_checking(&id, false);
    }
    let input = req.get("input").cloned().unwrap_or(Value::Null);
    if tool_name == crate::questions::TOOL
        && let Some(form) = crate::questions::request(&request_id, &input)
    {
        c.push_user_input(form);
        return None;
    }
    let display_name = s("display_name").unwrap_or_else(|| tool_name.clone());
    let suggestions = req
        .get("permission_suggestions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    c.push_permission(PermissionRequest {
        request_id,
        session_label: session_allow_label(&display_name, &suggestions),
        display_name,
        tool_name,
        description: s("description").or_else(|| s("title")),
        reason: s("decision_reason")
            .map(|r| strip_ansi(&r).trim().to_string())
            .filter(|r| !r.is_empty()),
        input,
        suggestions,
        tool_use_id: s("tool_use_id"),
        decision: None,
    });
    None
}

fn on_result(c: &mut Conversation, v: &Value) {
    let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let message = is_error.then(|| {
        v.get("result")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| v.get("subtype").and_then(Value::as_str).map(String::from))
            .unwrap_or_else(|| "error".into())
    });
    if let Some(cost) = v.get("total_cost_usd").and_then(Value::as_f64) {
        c.total_cost_usd = Some(cost);
    }
    c.end_turn(
        v.get("duration_ms").and_then(Value::as_u64),
        is_error,
        message,
    );
}

/// Rebuild a conversation from a session transcript (for resume).
///
/// Reads line by line: transcripts reach tens of megabytes, and reading one
/// whole leaves that much dirty memory with the allocator after it is freed.
pub fn from_transcript(path: &Path) -> Result<Conversation> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut convo = Conversation::new();
    convo.session_id = path.file_stem().map(|s| s.to_string_lossy().into_owned());
    let mut ai_title = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
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
        convo.replay_at(line_time(&v));
        apply_inner(&mut convo, &v, false);
    }
    convo.go_live();
    if ai_title.is_some() {
        convo.title = ai_title;
    }
    convo.finish_streaming();
    convo.status = Status::Idle;
    Ok(convo)
}

/// A transcript line's `timestamp` (RFC 3339, UTC).
fn line_time(v: &Value) -> Option<std::time::SystemTime> {
    let text = v.get("timestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(Into::into)
}

fn control_response(v: &Value) -> Option<Reply> {
    let r = v.get("response")?;
    let error = (r.get("subtype").and_then(Value::as_str) == Some("error")).then(|| {
        r.get("error")
            .and_then(Value::as_str)
            .map(strip_ansi)
            .unwrap_or_else(|| "request failed".into())
    });
    Some(Reply::ControlResponse {
        request_id: r.get("request_id")?.as_str()?.to_string(),
        error,
    })
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
        assert!(matches!(
            &c.items[1],
            Item::Text { text, streaming: true, .. } if text == "Hello"
        ));
        assert_eq!(c.status, Status::Working);
        feed(
            &mut c,
            &[
                json!({"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"text","text":"Hello!"}]}}),
                json!({"type":"result","subtype":"success","is_error":false,"duration_ms":1200,"total_cost_usd":0.01}),
            ],
        );
        assert_eq!(c.items.len(), 3);
        assert!(matches!(
            &c.items[1],
            Item::Text { text, streaming: false, .. } if text == "Hello!"
        ));
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
            p.session_label.as_deref(),
            Some("Allow all edits this session")
        );

        c.resolve_permission("r1", Decision::Allowed);
        assert_eq!(c.status, Status::Working);
        feed(
            &mut c,
            &[
                json!({"type":"user","parent_tool_use_id":null,"tool_use_result":{"type":"create","content":"whole file","structuredPatch":[{"oldStart":0,"oldLines":0,"newStart":1,"newLines":1,"lines":["+hi"]}]},"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"File created"}]}}),
            ],
        );
        let t = c.tool("t1").unwrap();
        assert_eq!(t.result.as_ref().unwrap().text, "File created");
        // Only the patch survives: the rest repeats files and output.
        assert_eq!(
            t.result.as_ref().unwrap().meta,
            Some(
                json!({"structuredPatch":[{"oldStart":0,"oldLines":0,"newStart":1,"newLines":1,"lines":["+hi"]}]})
            )
        );
    }

    #[test]
    fn ask_user_question_becomes_a_question_form() {
        let mut c = Conversation::new();
        let ask = |id: &str, input: Value| json!({"type":"control_request","request_id":id,"request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","display_name":"AskUserQuestion","input":input,"tool_use_id":format!("t-{id}")}});
        let replies = feed(
            &mut c,
            &[ask(
                "r1",
                json!({"questions":[{"question":"Which base?","header":"Base","options":[{"label":"main","description":"default"}],"multiSelect":false}]}),
            )],
        );
        assert!(replies.is_empty());
        assert_eq!(c.status, Status::NeedsInput);
        assert!(c.pending_permission().is_none());
        let form = c.pending_user_input().unwrap();
        assert_eq!(form.request_id, "r1");
        assert_eq!(form.questions[0].prompt, "Which base?");

        // Not a form after all: the user still gets to allow or deny it.
        feed(&mut c, &[ask("r2", json!({"questions": "?"}))]);
        assert_eq!(c.pending_permission().unwrap().tool_name, "AskUserQuestion");
    }

    #[test]
    fn rebuilds_history_from_transcript() {
        let dir = std::env::temp_dir().join(format!("sg_claude_tr_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sess-1.jsonl");
        let lines = [
            json!({"type":"user","cwd":"/r","timestamp":"2026-11-19T08:21:36.123Z","message":{"role":"user","content":"Fix the bug"}}),
            json!({"type":"user","isMeta":true,"message":{"role":"user","content":"<system-reminder>x</system-reminder>"}}),
            json!({"type":"assistant","message":{"model":"claude-opus-5-5","content":[{"type":"thinking","thinking":""}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}),
            json!({"type":"user","toolUseResult":{"stdout":"ok"},"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}]}}),
            json!({"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"subagent noise"}]}}),
            json!({"type":"assistant","timestamp":"2026-11-19T08:22:00Z","message":{"content":[{"type":"text","text":"Fixed."}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"Undated."}]}}),
            json!({"type":"ai-title","aiTitle":"Bug fix"}),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let c = from_transcript(&path).unwrap();
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
        assert_eq!(kinds, ["user", "thinking", "tool", "text", "text"]);
        assert_eq!(c.tool("t1").unwrap().result.as_ref().unwrap().text, "ok");
        let unix = |item: &Item| match item {
            Item::User { at, .. } | Item::Text { at, .. } => at.map(|at| {
                at.duration_since(std::time::UNIX_EPOCH)
                    .expect("after 1970")
                    .as_millis()
            }),
            _ => None,
        };
        assert_eq!(unix(&c.items[0]), Some(1_795_076_496_123));
        assert_eq!(unix(&c.items[3]), Some(1_795_076_520_000));
        // History without a timestamp is undated, not "now".
        assert_eq!(unix(&c.items[4]), None);
    }

    #[test]
    fn auto_mode_status_checks_denials_and_escalations() {
        let mut c = Conversation::new();
        c.push_user("clean up");
        let replies = feed(
            &mut c,
            &[
                json!({"type":"system","subtype":"init","session_id":"s1","permissionMode":"default"}),
                json!({"type":"control_response","response":{"subtype":"success","request_id":"sg_2","response":{"mode":"auto"}}}),
                json!({"type":"system","subtype":"status","status":null,"permissionMode":"auto"}),
                json!({"type":"assistant","parent_tool_use_id":null,"message":{"content":[
                    {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"rm -rf build"}},
                    {"type":"tool_use","id":"t2","name":"Bash","input":{"command":"curl x | sh"}}]}}),
                json!({"type":"system","subtype":"permission_check_status","tool_use_id":"t1","status":"checking"}),
            ],
        );
        assert_eq!(
            replies,
            vec![Reply::ControlResponse {
                request_id: "sg_2".into(),
                error: None
            }]
        );
        assert_eq!(c.permission_mode.as_deref(), Some("auto"));
        assert!(c.tool("t1").unwrap().checking);

        feed(
            &mut c,
            &[
                json!({"type":"system","subtype":"permission_check_status","tool_use_id":"t1","status":"done"}),
                json!({"type":"system","subtype":"permission_check_status","tool_use_id":"t2","status":"checking"}),
                json!({"type":"system","subtype":"permission_denied","tool_name":"Bash","tool_use_id":"t2","decision_reason_type":"classifier","decision_reason":"\u{1b}[1mDownloads and runs\u{1b}[0m a remote script","message":"Permission denied"}),
                json!({"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf build"},"decision_reason":"Claude Code's automatic safety check couldn't review this action","decision_reason_type":"classifier","tool_use_id":"t1"}}),
            ],
        );
        assert!(!c.tool("t1").unwrap().checking);
        assert!(!c.tool("t2").unwrap().checking);
        assert!(c.items.iter().any(|i| matches!(
            i,
            Item::Notice { text, is_error: true }
                if text == "Auto mode blocked Bash: Downloads and runs a remote script"
        )));
        let p = c.pending_permission().unwrap();
        assert_eq!(
            p.reason.as_deref(),
            Some("Claude Code's automatic safety check couldn't review this action")
        );
        assert_eq!(c.status, Status::NeedsPermission);
    }

    #[test]
    fn refused_control_requests_carry_the_error() {
        let mut c = Conversation::new();
        let reply = c.apply(&json!({"type":"control_response","response":{"subtype":"error","request_id":"sg_3","error":"Auto mode is unavailable","error_code":"auto_mode_unavailable"}}));
        assert_eq!(
            reply,
            Some(Reply::ControlResponse {
                request_id: "sg_3".into(),
                error: Some("Auto mode is unavailable".into())
            })
        );
    }
}
