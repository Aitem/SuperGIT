//! Folding Claude Code events into the shared chat model.
//!
//! The same fold handles live stream-json events and transcript lines from
//! `~/.claude/projects/…/<session>.jsonl` (used to show history on resume):
//! both carry Anthropic-style `message` objects.

use std::fs::File;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use serde_json::Value;
use sg_agents::chat::INTERRUPTED;
pub use sg_agents::chat::{
    Answers, Conversation, Decision, InputResolution, Item, PermissionRequest, Status, ToolCall,
    ToolResult, UserInputRequest, strip_ansi,
};

/// Label for the "allow and remember" button, if the CLI offered a rule:
/// what it allows and for how long, since the rule may be narrower than the
/// tool ("exit 3", not all of Bash) and may be written to a settings file.
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
        "addRules" => {
            let rules = s.get("rules").and_then(Value::as_array);
            let rule = rules
                .and_then(|rules| rules.first())
                .and_then(|rule| rule.get("ruleContent"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|rule| !rule.is_empty());
            let what = match rule {
                Some(rule) => format!("“{}”", cut(rule, 40)),
                None => display_name.to_string(),
            };
            let more = match rules.map_or(0, |rules| rules.len().saturating_sub(1)) {
                0 => String::new(),
                more => format!(" +{more}"),
            };
            Some(match s.get("destination").and_then(Value::as_str) {
                Some("session") => format!("Allow {what}{more} for this session"),
                Some("localSettings" | "projectSettings") => {
                    format!("Always allow {what}{more} in this project")
                }
                Some("userSettings") => format!("Always allow {what}{more} everywhere"),
                _ => format!("Always allow {what}{more}"),
            })
        }
        _ => Some("Allow for this session".into()),
    }
}

/// At most `max` characters of one line.
fn cut(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() > max || text.lines().nth(1).is_some() {
        format!("{}…", line.chars().take(max).collect::<String>())
    } else {
        line.to_string()
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
        "assistant" => on_assistant(c, v, live),
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
        // Mode changes (our `set_permission_mode`, a plan approved, …) and
        // what the CLI is busy with: "requesting", "compacting", null.
        "status" => {
            if let Some(mode) = s("permissionMode") {
                c.permission_mode = Some(mode);
            }
            if let Some(status) = v.get("status") {
                c.compacting = status.as_str() == Some("compacting");
                if !status.is_null() && c.status == Status::Starting {
                    c.status = Status::Working;
                }
            }
        }
        "compact_boundary" => {
            c.compacting = false;
            c.push_notice(compacted_notice(v), false);
        }
        "thinking_tokens" => {
            if let Some(tokens) = v.get("estimated_tokens").and_then(Value::as_u64) {
                c.thinking_tokens = Some(tokens);
            }
        }
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
            let tool = crate::tools::display_name(&s("tool_name").unwrap_or_else(|| "tool".into()));
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
        // A task that outlived its tool call (a background command or
        // agent) is over: say so, the turn it wakes follows. A task in the
        // foreground ends with its tool's result instead: its call is still
        // running here, and its summary is a bare description.
        "task_notification" => {
            let summary = s("summary").unwrap_or_default();
            let background = is_task_sentence(&summary)
                && s("tool_use_id")
                    .is_none_or(|id| c.tool(&id).is_none_or(|tool| tool.result.is_some()));
            if background {
                let status = s("status").unwrap_or_default();
                if let Some(text) = task_notice(&status, &summary) {
                    c.push_notice(text, status == "failed");
                }
            }
        }
        _ => {}
    }
}

/// "Conversation compacted · 32.8k → 9.2k tokens", with the sizes when the
/// CLI gives them (live `compact_metadata`, a transcript's `compactMetadata`).
fn compacted_notice(v: &Value) -> String {
    let meta = v
        .get("compact_metadata")
        .or_else(|| v.get("compactMetadata"));
    let tokens = |snake: &str, camel: &str| {
        meta.and_then(|meta| meta.get(snake).or_else(|| meta.get(camel)))
            .and_then(Value::as_u64)
            .filter(|tokens| *tokens > 0)
    };
    match (
        tokens("pre_tokens", "preTokens"),
        tokens("post_tokens", "postTokens"),
    ) {
        (Some(before), Some(after)) => format!(
            "Conversation compacted · {} → {} tokens",
            compact_count(before),
            compact_count(after)
        ),
        (Some(before), None) => format!(
            "Conversation compacted · was {} tokens",
            compact_count(before)
        ),
        _ => "Conversation compacted".into(),
    }
}

/// 32764 → "32.8k", 1200000 → "1.2M".
fn compact_count(n: u64) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..1_000_000 => format!("{:.1}k", n as f64 / 1000.),
        _ => format!("{:.1}M", n as f64 / 1_000_000.),
    }
}

/// What to say about a background task that ended, if anything. The CLI's
/// summary is usually a sentence of its own ("Background command "Build"
/// failed with exit code 1"); a bare description gets the outcome in front.
fn task_notice(status: &str, summary: &str) -> Option<String> {
    let summary = summary.trim().lines().next().unwrap_or_default().trim();
    if summary.is_empty() {
        return None;
    }
    if is_task_sentence(summary) {
        return Some(summary.to_string());
    }
    Some(match status {
        "completed" => format!("Background task finished: {summary}"),
        "failed" => format!("Background task failed: {summary}"),
        "killed" | "stopped" => format!("Background task stopped: {summary}"),
        _ => format!("Background task: {summary}"),
    })
}

/// Whether a task's summary is the CLI's own sentence about a background
/// task ("Background command "Build" failed…", "Agent "Explore" finished").
fn is_task_sentence(summary: &str) -> bool {
    let summary = summary.trim_start();
    summary.starts_with("Background ") || summary.starts_with("Agent ")
}

fn on_stream_event(c: &mut Conversation, v: &Value) {
    if !v.get("parent_tool_use_id").is_none_or(Value::is_null) {
        return; // subagent internals stay folded into their Task call
    }
    let Some(ev) = v.get("event") else { return };
    match ev.get("type").and_then(Value::as_str).unwrap_or_default() {
        "content_block_start" => {
            c.status = Status::Working;
            let block = ev.get("content_block");
            match block.and_then(|b| b.get("type")).and_then(Value::as_str) {
                Some("text") => c.start_text(),
                Some("thinking") => c.start_thinking(),
                // The call shows as soon as Claude names the tool: its
                // input (a whole file for Write) may take long to arrive.
                Some("tool_use") => {
                    let field = |key: &str| block.and_then(|b| b.get(key)).and_then(Value::as_str);
                    if let (Some(id), Some(name)) = (field("id"), field("name"))
                        && name != crate::questions::TOOL
                        && c.tool(id).is_none()
                    {
                        c.items.push(Item::Tool(ToolCall {
                            id: id.to_string(),
                            name: name.to_string(),
                            input: Value::Null,
                            result: None,
                            parent: None,
                            checking: false,
                        }));
                    }
                }
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

fn on_assistant(c: &mut Conversation, v: &Value, live: bool) {
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
    // The CLI's own message about a failed request, not Claude's words.
    let api_error = v.get("error").is_some_and(|error| !error.is_null())
        || v.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true);
    for block in blocks {
        match block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" if parent.is_none() => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if api_error && !text.trim().is_empty() {
                    c.complete_text("");
                    c.push_notice(text.trim(), true);
                } else {
                    c.complete_text(text);
                }
            }
            "thinking" | "redacted_thinking" if parent.is_none() => c.complete_thinking(),
            "tool_use" => {
                let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                // Questions are a form, not a tool row: live, the permission
                // request brings it; in history, the call itself does.
                if name == crate::questions::TOOL
                    && parent.is_none()
                    && let Some(form) = crate::questions::request(id, &input)
                {
                    if !live {
                        c.push_user_input(form);
                    }
                    continue;
                }
                match c.tool_mut(id) {
                    // Shown since Claude named it; here is its input.
                    Some(tool) => {
                        if tool.input.is_null() {
                            tool.input = input;
                        }
                    }
                    None => c.items.push(Item::Tool(ToolCall {
                        id: id.to_string(),
                        name: name.to_string(),
                        input,
                        result: None,
                        parent: parent.clone(),
                        checking: false,
                    })),
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
    let result = v.get("tool_use_result").or_else(|| v.get("toolUseResult"));
    let meta = result
        .and_then(|result| result.get("structuredPatch"))
        .map(|patch| serde_json::json!({ "structuredPatch": patch }));
    let mut prompt: Vec<&str> = Vec::new();
    let mut images = 0;
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
                let is_error = block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if let Some(t) = c.tool_mut(id) {
                    t.result = Some(ToolResult {
                        text: crate::tools::result_text(
                            block.get("content").unwrap_or(&Value::Null),
                        ),
                        is_error,
                        meta: meta.clone(),
                    });
                    t.checking = false;
                } else if !live {
                    answer_history_question(c, id, result, is_error);
                }
            }
            "text" if !live => prompt.extend(block.get("text").and_then(Value::as_str)),
            "image" if !live => images += 1,
            _ => {}
        }
    }
    if !prompt.is_empty() || images > 0 {
        let mut text = prompt.join("\n");
        // A pasted image leaves its own "[Image #1]" in the text.
        if images > 0 && !text.contains("[Image #") {
            let marks = vec!["[image]"; images].join(" ");
            text = if text.trim().is_empty() {
                marks
            } else {
                format!("{}\n{marks}", text.trim_end())
            };
        }
        push_history_prompt(c, &text);
    }
}

/// A question form replayed from history gets the answers its tool call
/// returned (`answers`: question → label, or labels of a multi-select).
fn answer_history_question(
    c: &mut Conversation,
    request_id: &str,
    result: Option<&Value>,
    is_error: bool,
) {
    let Some(form) = c.user_input(request_id) else {
        return;
    };
    if is_error {
        c.resolve_user_input(request_id, InputResolution::Declined);
        return;
    }
    let given = result.and_then(|result| result.get("answers"));
    let answers: Answers = form
        .questions
        .iter()
        .filter_map(|question| {
            let values = match given?.get(&question.id)? {
                Value::String(answer) => vec![answer.clone()],
                Value::Array(answers) => answers
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect(),
                _ => return None,
            };
            (!values.is_empty()).then(|| (question.id.clone(), values))
        })
        .collect();
    c.record_answers(request_id, answers);
}

/// A user-role transcript text: the user's prompt, a slash command, or one
/// of the CLI's own notes to Claude (most of which are not for the reader).
fn push_history_prompt(c: &mut Conversation, text: &str) {
    let trimmed = text.trim();
    if trimmed.starts_with("[Request interrupted") {
        c.push_notice(INTERRUPTED, false);
    } else if let Some(command) = slash_command(trimmed) {
        c.push_history_user(&command);
    } else if trimmed.starts_with("<task-notification>") {
        let status = tag(trimmed, "status").unwrap_or_default();
        if let Some(text) = task_notice(status, tag(trimmed, "summary").unwrap_or_default()) {
            c.push_notice(text, status == "failed");
        }
    } else if !trimmed.is_empty() && !trimmed.starts_with('<') {
        c.push_history_user(trimmed);
    }
}

/// The text between `<name>` and `</name>`.
fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&format!("</{name}>"))? + start;
    Some(text[start..end].trim())
}

/// "/compact", "/model opus": a slash command as the CLI records it
/// (`<command-name>/compact</command-name>…<command-args>…</command-args>`).
fn slash_command(text: &str) -> Option<String> {
    if !text.starts_with("<command-name>") {
        return None;
    }
    let name = tag(text, "command-name").filter(|name| !name.is_empty())?;
    Some(
        match tag(text, "command-args").filter(|args| !args.is_empty()) {
            Some(args) => format!("{name} {args}"),
            None => name.to_string(),
        },
    )
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
    let display_name =
        crate::tools::display_name(&s("display_name").unwrap_or_else(|| tool_name.clone()));
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

/// Whether a `result` event ends a turn the user stopped. The CLI reports
/// that as an error (`error_during_execution`); it is not one.
pub fn was_stopped(result: &Value) -> bool {
    matches!(
        result.get("terminal_reason").and_then(Value::as_str),
        Some("aborted_streaming" | "aborted_tools")
    )
}

fn on_result(c: &mut Conversation, v: &Value) {
    if let Some(cost) = v.get("total_cost_usd").and_then(Value::as_f64) {
        c.total_cost_usd = Some(cost);
    }
    let reason = v.get("terminal_reason").and_then(Value::as_str);
    if was_stopped(v) {
        c.interrupt_turn();
        return;
    }
    let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    c.end_turn(
        v.get("duration_ms").and_then(Value::as_u64),
        is_error,
        is_error.then(|| failure(v, reason)),
    );
}

/// Why a turn failed, in words: the CLI's text when it has one, else what
/// its reason codes mean.
fn failure(v: &Value, reason: Option<&str>) -> String {
    let said = v
        .get("result")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .or_else(|| {
            // `errors` also carries diagnostics ("[ede_diagnostic] …").
            v.get("errors")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::trim)
                .find(|error| !error.is_empty() && !error.starts_with('['))
        });
    if let Some(said) = said {
        return strip_ansi(said);
    }
    let subtype = v.get("subtype").and_then(Value::as_str);
    match (reason, subtype) {
        (Some("max_turns"), _) | (_, Some("error_max_turns")) => "Reached the turn limit",
        (Some("budget_exhausted"), _) | (_, Some("error_max_budget_usd")) => {
            "Reached the spending limit"
        }
        (Some("prompt_too_long"), _) => "The conversation no longer fits the model's context",
        (Some("blocking_limit" | "rapid_refill_breaker"), _) => "Usage limit reached",
        (Some("api_error" | "model_error"), _) => "The request to Claude failed",
        (Some("stop_hook_prevented" | "hook_stopped"), _) => "A hook stopped the turn",
        _ => "The turn failed",
    }
    .to_string()
}

/// A replayed turn, to end it the way a live one ends (`result` events are
/// not in transcripts).
#[derive(Default)]
struct Turn {
    started: Option<SystemTime>,
    /// When Claude's last message of the turn was written, once it has
    /// stopped by itself (`stop_reason: end_turn`).
    ended: Option<SystemTime>,
    /// Claude said something in it.
    active: bool,
    done: bool,
}

impl Turn {
    /// The turn's end, if it has one, starting over.
    fn take_end(&mut self, duration_ms: Option<u64>) -> Option<Item> {
        let turn = std::mem::take(self);
        (turn.done || (turn.active && duration_ms.is_some())).then(|| Item::TurnEnd {
            duration_ms: duration_ms.or_else(|| {
                let spent = turn.ended?.duration_since(turn.started?).ok()?;
                u64::try_from(spent.as_millis()).ok()
            }),
            is_error: false,
            message: None,
        })
    }
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
    let mut custom_title = None;
    let mut turn = Turn::default();
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
        if ty == "custom-title" {
            custom_title = v
                .get("customTitle")
                .and_then(Value::as_str)
                .map(String::from);
            continue;
        }
        let skip = ["isSidechain", "isMeta", "isCompactSummary"]
            .iter()
            .any(|k| v.get(*k).and_then(Value::as_bool) == Some(true));
        if skip {
            continue;
        }
        let at = line_time(&v);
        match ty {
            // A message sent while Claude worked: the CLI hands it over with
            // a tool result and records it only here.
            "attachment" => {
                if let Some(prompt) = queued_prompt(&v) {
                    convo.replay_at(at);
                    push_history_prompt(&mut convo, prompt);
                }
                continue;
            }
            "system" => {
                convo.replay_at(at);
                history_system(&mut convo, &v, &mut turn);
                continue;
            }
            "user" | "assistant" => {}
            _ => continue,
        }
        if convo.cwd.is_none() {
            convo.cwd = v.get("cwd").and_then(Value::as_str).map(PathBuf::from);
        }
        if ty == "assistant" && convo.model.is_none() {
            convo.model = v
                .pointer("/message/model")
                .and_then(Value::as_str)
                .filter(|model| !model.starts_with('<'))
                .map(String::from);
        }
        convo.replay_at(at);
        let before = convo.items.len();
        apply_inner(&mut convo, &v, false);
        if ty == "assistant" {
            turn.active = true;
            turn.done = matches!(
                v.pointer("/message/stop_reason").and_then(Value::as_str),
                Some("end_turn" | "stop_sequence")
            );
            if turn.done {
                turn.ended = at;
            }
            continue;
        }
        match convo.items.get(before) {
            // A new prompt: the turn before it is over, the next begins.
            Some(Item::User { .. }) => {
                if let Some(end) = turn.take_end(None) {
                    convo.items.insert(before, end);
                }
                turn = Turn {
                    started: at,
                    ..Turn::default()
                };
            }
            Some(Item::Notice { text, .. }) if text == INTERRUPTED => turn = Turn::default(),
            _ => {}
        }
    }
    convo.items.extend(turn.take_end(None));
    convo.go_live();
    // The user's name (`/rename`) wins over Claude's.
    if let Some(title) = custom_title.or(ai_title) {
        convo.title = Some(title);
    }
    convo.finish_streaming();
    // Nothing waits for an answer in a conversation read from its history.
    let unanswered: Vec<String> = convo
        .items
        .iter()
        .filter_map(|item| match item {
            Item::UserInput(form) if form.resolution == InputResolution::Pending => {
                Some(form.request_id.clone())
            }
            _ => None,
        })
        .collect();
    for request_id in unanswered {
        convo.resolve_user_input(&request_id, InputResolution::Expired);
    }
    convo.status = Status::Idle;
    Ok(convo)
}

/// The text of a prompt the CLI queued while Claude worked (`attachment`
/// records of type `queued_command`): the user's, or a task's notification.
fn queued_prompt(v: &Value) -> Option<&str> {
    let attachment = v.get("attachment")?;
    if attachment.get("type").and_then(Value::as_str) != Some("queued_command") {
        return None;
    }
    // Other agents' messages are not the user's words.
    let origin = attachment.pointer("/origin/kind").and_then(Value::as_str);
    if !matches!(origin, None | Some("human" | "task-notification")) {
        return None;
    }
    attachment.get("prompt").and_then(Value::as_str)
}

/// The CLI's own transcript records worth showing: where the conversation
/// was compacted, what a slash command printed, how long a turn took.
fn history_system(convo: &mut Conversation, v: &Value, turn: &mut Turn) {
    match v.get("subtype").and_then(Value::as_str).unwrap_or_default() {
        "compact_boundary" => convo.push_notice(compacted_notice(v), false),
        "local_command" => {
            let content = v.get("content").and_then(Value::as_str).unwrap_or_default();
            if let Some(command) = slash_command(content.trim()) {
                convo.push_history_user(&command);
            } else if let Some(output) = tag(content, "local-command-stdout") {
                convo.complete_text(&strip_ansi(output));
            }
        }
        "turn_duration" => {
            let duration = v.get("durationMs").and_then(Value::as_u64);
            convo.items.extend(turn.take_end(duration));
        }
        _ => {}
    }
}

/// A transcript line's `timestamp` (RFC 3339, UTC).
fn line_time(v: &Value) -> Option<SystemTime> {
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

    #[test]
    fn a_stop_is_not_a_failed_turn() {
        let mut c = Conversation::new();
        c.push_user("run it");
        feed(
            &mut c,
            &[
                json!({"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"sleep 30"}}]}}),
                json!({"type":"user","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}),
                json!({"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_streaming","result":null,"errors":["[ede_diagnostic] result_type=user"],"duration_ms":3000,"total_cost_usd":0.01}),
            ],
        );
        assert_eq!(c.status, Status::Idle);
        assert!(matches!(
            c.items.last(),
            Some(Item::Notice { text, is_error: false }) if text == "Interrupted"
        ));
        assert!(!c.items.iter().any(|i| matches!(i, Item::TurnEnd { .. })));
        assert_eq!(c.total_cost_usd, Some(0.01));
    }

    #[test]
    fn failed_turns_say_why_in_words() {
        let end = |result: Value| {
            let mut c = Conversation::new();
            c.push_user("go");
            c.apply(&result);
            match c.items.last() {
                Some(Item::TurnEnd {
                    is_error: true,
                    message: Some(message),
                    ..
                }) => message.clone(),
                other => panic!("no failed turn end: {other:?}"),
            }
        };
        assert_eq!(
            end(
                json!({"type":"result","subtype":"error_max_turns","is_error":true,"terminal_reason":"max_turns","result":null})
            ),
            "Reached the turn limit"
        );
        assert_eq!(
            end(
                json!({"type":"result","subtype":"error_during_execution","is_error":true,"errors":["[ede_diagnostic] x","Credit balance is too low"]})
            ),
            "Credit balance is too low"
        );
        assert_eq!(
            end(
                json!({"type":"result","subtype":"success","is_error":true,"result":"API Error: 529"})
            ),
            "API Error: 529"
        );
        assert_eq!(
            end(json!({"type":"result","subtype":"error_during_execution","is_error":true})),
            "The turn failed"
        );
    }

    #[test]
    fn compaction_and_thinking_show_as_they_happen() {
        let mut c = Conversation::new();
        c.push_user("/compact");
        c.apply(&json!({"type":"system","subtype":"status","status":"compacting"}));
        assert!(c.compacting);
        assert_eq!(c.status, Status::Working);
        feed(
            &mut c,
            &[
                json!({"type":"system","subtype":"status","status":null,"compact_result":"success"}),
                json!({"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"manual","pre_tokens":32764,"post_tokens":9236}}),
            ],
        );
        assert!(!c.compacting);
        assert!(matches!(
            c.items.last(),
            Some(Item::Notice { text, is_error: false })
                if text == "Conversation compacted · 32.8k → 9.2k tokens"
        ));

        // A mode change says nothing about compaction either way.
        c.apply(&json!({"type":"system","subtype":"status","status":"compacting"}));
        c.apply(&json!({"type":"system","subtype":"status","permissionMode":"plan"}));
        assert!(c.compacting);

        feed(
            &mut c,
            &[
                json!({"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}}),
                json!({"type":"system","subtype":"thinking_tokens","estimated_tokens":450,"estimated_tokens_delta":250}),
            ],
        );
        assert_eq!(c.thinking_tokens, Some(450));
        c.apply(&json!({"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"thinking","thinking":""}]}}));
        assert_eq!(c.thinking_tokens, None);
    }

    #[test]
    fn a_tool_call_shows_before_its_input_arrives() {
        let mut c = Conversation::new();
        c.push_user("write it");
        c.apply(&json!({"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"Write","input":{}}}}));
        assert!(matches!(
            c.items.last(),
            Some(Item::Tool(t)) if t.name == "Write" && t.input.is_null() && t.result.is_none()
        ));
        feed(
            &mut c,
            &[
                json!({"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\""}}}),
                json!({"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"tool_use","id":"t1","name":"Write","input":{"file_path":"/r/a.txt","content":"x"}}]}}),
            ],
        );
        assert_eq!(c.items.len(), 2);
        assert_eq!(c.tool("t1").unwrap().input["file_path"], "/r/a.txt");

        // A question is a form: it never gets a tool row.
        c.apply(&json!({"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{}}}}));
        c.apply(&json!({"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{"questions":[{"question":"Tabs?","options":[{"label":"Yes"}]}]}}]}}));
        assert_eq!(c.items.len(), 2);
    }

    #[test]
    fn remembered_rules_say_what_they_allow_and_where() {
        let label = |suggestion: Value| session_allow_label("Bash", &[suggestion]);
        assert_eq!(
            label(
                json!({"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"exit 3"}],"behavior":"allow","destination":"localSettings"})
            )
            .as_deref(),
            Some("Always allow “exit 3” in this project")
        );
        assert_eq!(
            label(
                json!({"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"cargo test:*"},{"toolName":"Bash","ruleContent":"cargo check:*"}],"destination":"session"})
            )
            .as_deref(),
            Some("Allow “cargo test:*” +1 for this session")
        );
        assert_eq!(
            label(
                json!({"type":"addRules","rules":[{"toolName":"Bash"}],"destination":"userSettings"})
            )
            .as_deref(),
            Some("Always allow Bash everywhere")
        );
        let long = "x".repeat(60);
        let cut =
            label(json!({"type":"addRules","rules":[{"toolName":"Bash","ruleContent":long}]}))
                .unwrap();
        assert_eq!(cut.chars().count(), "Always allow “”".chars().count() + 41);
    }

    #[test]
    fn background_tasks_and_api_errors_get_notices() {
        let mut c = Conversation::new();
        c.push_user("go");
        feed(
            &mut c,
            &[
                json!({"type":"assistant","parent_tool_use_id":null,"message":{"content":[
                    {"type":"tool_use","id":"fg","name":"Bash","input":{"command":"sleep 5"}},
                    {"type":"tool_use","id":"bg","name":"Bash","input":{"command":"sleep 99","run_in_background":true}}]}}),
                json!({"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"bg","content":"Command running in background"}]}}),
                // In the foreground: its tool result says it all.
                json!({"type":"system","subtype":"task_notification","task_id":"a","tool_use_id":"fg","status":"completed","summary":"Wait"}),
            ],
        );
        assert!(!c.items.iter().any(|i| matches!(i, Item::Notice { .. })));
        // Nor does a bare description, whatever the order of events.
        c.apply(&json!({"type":"system","subtype":"task_notification","task_id":"b","tool_use_id":"bg","status":"completed","summary":"Run the server"}));
        assert!(!c.items.iter().any(|i| matches!(i, Item::Notice { .. })));
        c.apply(&json!({"type":"system","subtype":"task_notification","task_id":"b","tool_use_id":"bg","status":"failed","summary":"Background command \"Run the server\" failed with exit code 1\nmore"}));
        assert!(matches!(
            c.items.last(),
            Some(Item::Notice { text, is_error: true })
                if text == "Background command \"Run the server\" failed with exit code 1"
        ));

        c.apply(&json!({"type":"assistant","parent_tool_use_id":null,"error":"rate_limit","message":{"content":[{"type":"text","text":"API Error: 429 rate limited"}]}}));
        assert!(matches!(
            c.items.last(),
            Some(Item::Notice { text, is_error: true }) if text == "API Error: 429 rate limited"
        ));
    }

    #[test]
    fn history_keeps_what_the_cli_records_outside_messages() {
        let dir = std::env::temp_dir().join(format!("sg_claude_tr2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sess-2.jsonl");
        let lines = [
            json!({"type":"user","timestamp":"2026-11-19T08:00:00Z","message":{"role":"user","content":"Run the tests"}}),
            json!({"type":"assistant","timestamp":"2026-11-19T08:00:05Z","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}),
            // Sent while Claude worked: recorded as an attachment only.
            json!({"type":"queue-operation","operation":"enqueue","content":"Also run clippy"}),
            json!({"type":"user","toolUseResult":{"stdout":"ok"},"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}),
            json!({"type":"attachment","timestamp":"2026-11-19T08:00:20Z","attachment":{"type":"queued_command","prompt":"Also run clippy","commandMode":"prompt","origin":{"kind":"human"}}}),
            json!({"type":"attachment","attachment":{"type":"queued_command","prompt":"<agent-message from=\"x\">hi</agent-message>","commandMode":"prompt","origin":{"kind":"peer"}}}),
            json!({"type":"attachment","attachment":{"type":"edited_text_file","filename":"a.rs"}}),
            json!({"type":"assistant","timestamp":"2026-11-19T08:01:00Z","message":{"stop_reason":"end_turn","content":[{"type":"thinking","thinking":""}]}}),
            json!({"type":"assistant","timestamp":"2026-11-19T08:01:30Z","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"Both pass."}]}}),
            // A slash command, its note to Claude and its output.
            json!({"type":"user","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>x</local-command-caveat>"}}),
            json!({"type":"user","timestamp":"2026-11-19T09:00:00Z","message":{"role":"user","content":"<command-name>/context</command-name>\n  <command-message>context</command-message>\n  <command-args></command-args>"}}),
            json!({"type":"system","subtype":"local_command","content":"<local-command-stdout>## Context Usage\n\n15.6k / 1m</local-command-stdout>"}),
            json!({"type":"user","message":{"role":"user","content":"<command-name>/model</command-name><command-args>opus</command-args>"}}),
            json!({"type":"system","subtype":"compact_boundary","content":"Conversation compacted","compactMetadata":{"trigger":"manual","preTokens":569113,"postTokens":19251}}),
            json!({"type":"user","isCompactSummary":true,"message":{"role":"user","content":"This session is being continued…"}}),
            // An image with its own mark in the text, and one without.
            json!({"type":"user","timestamp":"2026-11-19T10:00:00Z","message":{"role":"user","content":[{"type":"text","text":"Like this [Image #1]"},{"type":"image","source":{}}]}}),
            json!({"type":"assistant","timestamp":"2026-11-19T10:00:09Z","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"Got it."}]}}),
            json!({"type":"system","subtype":"turn_duration","durationMs":9500}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"image","source":{}}]}}),
            json!({"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"sleep 99"}}]}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}),
            json!({"type":"user","message":{"role":"user","content":"<task-notification>\n<task-id>b1</task-id>\n<status>stopped</status>\n<summary>Background shell command didn't finish</summary>\n</task-notification>"}}),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let c = from_transcript(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        let shown: Vec<String> = c
            .items
            .iter()
            .map(|i| match i {
                Item::User { text, .. } => format!("user: {text}"),
                Item::Text { text, .. } => format!("text: {}", text.lines().next().unwrap_or("")),
                Item::Thinking { .. } => "thinking".into(),
                Item::Tool(t) => format!("tool: {}", t.name),
                Item::Notice { text, .. } => format!("notice: {text}"),
                Item::TurnEnd { duration_ms, .. } => format!("end: {duration_ms:?}"),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(
            shown,
            [
                "user: Run the tests",
                "tool: Bash",
                "user: Also run clippy",
                "thinking",
                "text: Both pass.",
                // From the prompt to Claude's last word.
                "end: Some(90000)",
                "user: /context",
                "text: ## Context Usage",
                "user: /model opus",
                "notice: Conversation compacted · 569.1k → 19.3k tokens",
                "user: Like this [Image #1]",
                "text: Got it.",
                // The CLI's own figure.
                "end: Some(9500)",
                "user: [image]",
                "tool: Bash",
                "notice: Interrupted",
                "notice: Background shell command didn't finish",
            ]
        );
        assert_eq!(c.status, Status::Idle);
    }

    #[test]
    fn history_shows_questions_with_their_answers() {
        let dir = std::env::temp_dir().join(format!("sg_claude_tr3_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sess-3.jsonl");
        let ask = |id: &str, question: &str| json!({"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":id,"name":"AskUserQuestion","input":{"questions":[{"question":question,"header":"Q","options":[{"label":"Tabs","description":""},{"label":"Spaces","description":""}],"multiSelect":question.starts_with("Which")}]}}]}});
        let lines = [
            json!({"type":"user","message":{"role":"user","content":"Ask me"}}),
            ask("q1", "Tabs or spaces?"),
            json!({"type":"user","toolUseResult":{"questions":[],"answers":{"Tabs or spaces?":"Tabs"}},"message":{"content":[{"type":"tool_result","tool_use_id":"q1","content":"User has answered your questions"}]}}),
            ask("q2", "Which ones?"),
            json!({"type":"user","toolUseResult":{"answers":{"Which ones?":["Tabs","Spaces"]}},"message":{"content":[{"type":"tool_result","tool_use_id":"q2","content":"answered"}]}}),
            ask("q3", "Sure?"),
            json!({"type":"user","toolUseResult":"Error","message":{"content":[{"type":"tool_result","tool_use_id":"q3","content":"The user declined to answer these questions.","is_error":true}]}}),
            ask("q4", "Still there?"),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let c = from_transcript(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert!(!c.items.iter().any(|i| matches!(i, Item::Tool(_))));
        let forms: Vec<_> = c
            .items
            .iter()
            .filter_map(|i| match i {
                Item::UserInput(form) => Some((
                    form.resolution,
                    form.answers.values().flatten().cloned().collect::<Vec<_>>(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            forms,
            [
                (InputResolution::Answered, vec!["Tabs".to_string()]),
                (
                    InputResolution::Answered,
                    vec!["Tabs".to_string(), "Spaces".to_string()]
                ),
                (InputResolution::Declined, vec![]),
                (InputResolution::Expired, vec![]),
            ]
        );
        assert_eq!(c.status, Status::Idle);
    }
}
