//! Folding `codex app-server` events into the shared chat model.
//!
//! Live events are what [`crate::process::CodexProcess`] emits: v2
//! notifications (`item/started`, `item/agentMessage/delta`,
//! `turn/completed`, …), server requests (approvals), and answers to the
//! driver's own requests as `{"response": "<method>", "result" | "error"}`.
//! History is the same `ThreadItem`s, read with `thread/turns/list`.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{Value, json};
use sg_agents::chat::{
    Conversation, Decision, InputResolution, Item, PermissionRequest, PlanStep, PlanStepStatus,
    Status, ToolCall, TurnPlan, short_title, strip_ansi,
};

use crate::process::{History, request_id_text};
use crate::{TokenUsage, tools};

/// What the host must do after an event.
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    /// A server request SuperGIT does not handle: answer with an error so
    /// Codex does not wait forever.
    Unsupported { request_id: String, method: String },
    /// A server request answered without asking the user.
    Respond { request_id: String, result: Value },
}

/// Fold state that is not part of the conversation: where each streamed
/// item lives in it.
#[derive(Debug, Default)]
pub struct CodexFold {
    texts: HashMap<String, usize>,
    thinking: HashMap<String, usize>,
    active_turn: Option<String>,
    active_plan: Option<usize>,
    usage: Option<Box<TokenUsage>>,
    usage_turn: Option<String>,
}

impl CodexFold {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn token_usage(&self) -> Option<&TokenUsage> {
        self.usage.as_deref()
    }

    /// Apply one live event.
    pub fn apply(&mut self, c: &mut Conversation, v: &Value) -> Option<Reply> {
        if let Some(method) = v.get("response").and_then(Value::as_str) {
            self.on_response(c, method, v);
            return None;
        }
        let method = v.get("method").and_then(Value::as_str)?;
        let params = v.get("params").unwrap_or(&Value::Null);
        match v.get("id") {
            Some(id) => self.on_request(c, &request_id_text(id), method, params),
            None => {
                self.on_notification(c, method, params);
                None
            }
        }
    }

    /// Show a thread read with [`crate::process::read_history`].
    pub fn apply_history(&mut self, c: &mut Conversation, history: &History) {
        let s = |k: &str| {
            history
                .thread
                .get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        if let Some(id) = s("id") {
            c.session_id = Some(id);
        }
        c.cwd = s("cwd").map(PathBuf::from).or(c.cwd.take());
        c.model = s("model").or(c.model.take());
        for turn in &history.turns {
            let at = |key: &str| {
                turn.get(key)
                    .and_then(Value::as_u64)
                    .map(|secs| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            };
            let (started, completed) = (at("startedAt"), at("completedAt"));
            for item in turn
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let user = item.get("type").and_then(Value::as_str) == Some("userMessage");
                c.replay_at(if user { started } else { completed.or(started) });
                self.on_item(c, item, false, false);
            }
            match turn.get("status").and_then(Value::as_str) {
                Some("interrupted") => c.push_notice(sg_agents::chat::INTERRUPTED, false),
                Some("failed") => c.push_notice(turn_error(turn), true),
                _ => {}
            }
        }
        if let Some(name) = s("name") {
            c.title = Some(short_title(&name));
        }
        c.go_live();
        c.finish_streaming();
        self.texts.clear();
        self.thinking.clear();
        self.active_turn = None;
        self.active_plan = None;
        self.usage = None;
        self.usage_turn = None;
        c.status = Status::Idle;
    }

    fn on_response(&mut self, c: &mut Conversation, method: &str, v: &Value) {
        if let Some(error) = v.pointer("/error/message").and_then(Value::as_str) {
            let error = strip_ansi(error);
            let text = match method {
                "thread/resume" => format!("Can't resume this Codex thread: {error}"),
                "turn/interrupt" => format!("Can't stop the turn: {error}"),
                "initialize" | "account/read" | "thread/start" | "turn/start" => error,
                other => format!("{other}: {error}"),
            };
            c.push_notice(text, true);
            if method != "turn/interrupt" && c.status.is_busy() {
                // The turn never started; the next message tries again.
                c.finish_streaming();
                c.status = Status::Idle;
            }
            return;
        }
        if matches!(method, "thread/start" | "thread/resume") {
            let result = v.get("result").unwrap_or(&Value::Null);
            self.on_thread(c, result.get("thread").unwrap_or(&Value::Null));
            if let Some(model) = result.get("model").and_then(Value::as_str) {
                c.model = Some(model.to_string());
            }
        }
    }

    fn on_thread(&mut self, c: &mut Conversation, thread: &Value) {
        let s = |k: &str| thread.get(k).and_then(Value::as_str).map(String::from);
        if let Some(id) = s("id") {
            if c.session_id.as_ref() != Some(&id) {
                self.usage = None;
                self.usage_turn = None;
            }
            c.session_id = Some(id);
        }
        if let Some(cwd) = s("cwd") {
            c.cwd = Some(PathBuf::from(cwd));
        }
        if let Some(model) = s("model") {
            c.model = Some(model);
        }
    }

    fn on_notification(&mut self, c: &mut Conversation, method: &str, p: &Value) {
        let s = |k: &str| p.get(k).and_then(Value::as_str);
        match method {
            "thread/started" => self.on_thread(c, p.get("thread").unwrap_or(&Value::Null)),
            "turn/started" => {
                let turn = p.pointer("/turn/id").and_then(Value::as_str);
                if let Some(turn) = turn {
                    self.usage_turn = Some(turn.to_owned());
                }
                if self.active_turn.as_deref() != turn {
                    if let Some(index) = self.active_plan.take()
                        && let Some(Item::Plan(previous)) = c.items.get_mut(index)
                    {
                        previous.finished = true;
                    }
                    self.active_turn = turn.map(str::to_owned);
                }
                if !matches!(c.status, Status::NeedsPermission | Status::NeedsInput) {
                    c.status = Status::Working;
                }
            }
            "turn/completed" => {
                let turn = p.get("turn").unwrap_or(&Value::Null);
                let duration = turn.get("durationMs").and_then(Value::as_u64);
                match turn.get("status").and_then(Value::as_str) {
                    // The user's own Stop, not a failure.
                    Some("interrupted") => c.interrupt_turn(),
                    Some("failed") => c.end_turn(duration, true, Some(turn_error(turn))),
                    _ => c.end_turn(duration, false, None),
                }
                self.texts.clear();
                self.thinking.clear();
                self.active_turn = None;
                self.active_plan = None;
            }
            "turn/plan/updated" => self.on_plan(c, p),
            "thread/tokenUsage/updated" => {
                if s("threadId").is_some()
                    && s("threadId") == c.session_id.as_deref()
                    && self
                        .usage_turn
                        .as_deref()
                        .is_none_or(|turn| s("turnId") == Some(turn))
                    && let Some(raw) = p.get("tokenUsage")
                    && let Ok(usage) = serde_json::from_value::<TokenUsage>(raw.clone())
                {
                    self.usage = Some(Box::new(usage));
                }
            }
            "item/started" => self.on_item(c, p.get("item").unwrap_or(&Value::Null), true, true),
            "item/completed" => self.on_item(c, p.get("item").unwrap_or(&Value::Null), false, true),
            "item/agentMessage/delta" => {
                if let (Some(id), Some(delta)) = (s("itemId"), s("delta"))
                    && let Some(&ix) = self.texts.get(id)
                    && let Some(Item::Text { text, .. }) = c.items.get_mut(ix)
                {
                    text.push_str(delta);
                }
            }
            "item/fileChange/patchUpdated" => {
                if let (Some(id), Some(changes)) = (s("itemId"), p.get("changes"))
                    && let Some(tool) = c.tool_mut(id)
                {
                    tool.input["changes"] = changes.clone();
                }
            }
            "serverRequest/resolved" => {
                if let Some(id) = p.get("requestId").map(request_id_text) {
                    if c.permission(&id)
                        .is_some_and(|request| request.decision.is_none())
                    {
                        c.resolve_permission(&id, Decision::Expired);
                    }
                    if c.user_input(&id).is_some() {
                        c.resolve_user_input(&id, InputResolution::Expired);
                    }
                }
            }
            "error" => {
                let message = p
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(strip_ansi)
                    .unwrap_or_else(|| "Codex reported an error".into());
                if p.get("willRetry").and_then(Value::as_bool) == Some(true) {
                    c.push_notice(format!("{message} — retrying…"), false);
                } else {
                    c.push_notice(message, true);
                }
            }
            "model/rerouted" => {
                if let Some(model) = s("toModel") {
                    c.model = Some(model.to_string());
                }
            }
            "thread/name/updated" => {
                if let Some(name) = s("threadName").filter(|n| !n.is_empty()) {
                    c.title = Some(short_title(name));
                }
            }
            "thread/compacted"
                if s("threadId").is_none_or(|thread| c.session_id.as_deref() == Some(thread)) =>
            {
                self.usage = None;
                self.usage_turn = None;
                c.push_notice("Conversation compacted", false);
            }
            _ => {}
        }
    }

    /// An item started or completed, live or from history.
    fn on_plan(&mut self, c: &mut Conversation, params: &Value) {
        let thread = params.get("threadId").and_then(Value::as_str);
        let turn = params.get("turnId").and_then(Value::as_str);
        if turn.is_none()
            || turn != self.active_turn.as_deref()
            || c.session_id.as_deref().is_some_and(|id| thread != Some(id))
            || !c.status.is_busy()
        {
            return;
        }
        let Some(items) = params.get("plan").and_then(Value::as_array) else {
            return;
        };
        let steps = items
            .iter()
            .filter_map(|item| {
                let text = item.get("step")?.as_str()?.trim();
                if text.is_empty() {
                    return None;
                }
                let status = match item.get("status")?.as_str()? {
                    "pending" => PlanStepStatus::Pending,
                    "inProgress" => PlanStepStatus::InProgress,
                    "completed" => PlanStepStatus::Completed,
                    _ => return None,
                };
                Some(PlanStep {
                    text: text.to_owned(),
                    status,
                })
            })
            .collect();
        let explanation = params
            .get("explanation")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned);
        let plan = TurnPlan {
            explanation,
            steps,
            finished: false,
        };
        if let Some(index) = self.active_plan
            && let Some(Item::Plan(previous)) = c.items.get_mut(index)
        {
            *previous = plan;
        } else if !plan.steps.is_empty() {
            self.active_plan = Some(c.items.len());
            c.items.push(Item::Plan(plan));
        }
    }

    fn on_item(&mut self, c: &mut Conversation, item: &Value, started: bool, live: bool) {
        let ty = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let text = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or_default();
        if live && c.status == Status::Starting {
            c.status = Status::Working;
        }
        match ty {
            // Live turns are echoed locally when sent.
            "userMessage" => {
                if !live && !started {
                    let prompt = user_text(item);
                    if !prompt.trim().is_empty() {
                        c.push_history_user(prompt.trim());
                    }
                }
            }
            "agentMessage" => match (started, self.texts.get(&id).copied()) {
                (true, None) => {
                    c.items.push(Item::Text {
                        text: text("text").to_string(),
                        streaming: true,
                        at: c.stamp(),
                    });
                    self.texts.insert(id, c.items.len() - 1);
                }
                (false, Some(ix)) => {
                    if let Some(Item::Text {
                        text: t, streaming, ..
                    }) = c.items.get_mut(ix)
                    {
                        *t = text("text").to_string();
                        *streaming = false;
                    }
                }
                (false, None) if !text("text").trim().is_empty() => c.items.push(Item::Text {
                    text: text("text").to_string(),
                    streaming: false,
                    at: c.stamp(),
                }),
                _ => {}
            },
            "reasoning" => match (started, self.thinking.get(&id).copied()) {
                (true, None) => {
                    c.items.push(Item::Thinking { streaming: true });
                    self.thinking.insert(id, c.items.len() - 1);
                }
                (false, Some(ix)) => {
                    if let Some(Item::Thinking { streaming }) = c.items.get_mut(ix) {
                        *streaming = false;
                    }
                }
                _ => {}
            },
            "plan" if !started && !text("text").trim().is_empty() => c.items.push(Item::Text {
                text: format!("**Plan**\n\n{}", text("text")),
                streaming: false,
                at: c.stamp(),
            }),
            "contextCompaction" if !started => c.push_notice("Conversation compacted", false),
            "enteredReviewMode" if !started => c.push_notice(
                format!("Reviewing {}", text("review")).trim().to_string(),
                false,
            ),
            "exitedReviewMode" if !started && !text("review").trim().is_empty() => {
                c.items.push(Item::Text {
                    text: text("review").to_string(),
                    streaming: false,
                    at: c.stamp(),
                })
            }
            _ if tools::is_tool(ty) => {
                let result = (!started).then(|| tools::result(item));
                // A command's output is the result's text: keep one copy.
                let mut input = item.clone();
                if let Some(fields) = input.as_object_mut() {
                    fields.remove("aggregatedOutput");
                }
                match c.tool_mut(&id) {
                    Some(tool) => {
                        tool.input = input;
                        if result.is_some() {
                            tool.result = result;
                        }
                    }
                    None => c.items.push(Item::Tool(ToolCall {
                        id,
                        name: tools::name(item),
                        input,
                        result,
                        parent: None,
                        checking: false,
                    })),
                }
            }
            _ => {}
        }
    }

    fn on_request(
        &mut self,
        c: &mut Conversation,
        request_id: &str,
        method: &str,
        p: &Value,
    ) -> Option<Reply> {
        let s = |k: &str| {
            p.get(k)
                .and_then(Value::as_str)
                .map(|s| strip_ansi(s).trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let item_id = s("itemId");
        let request = match method {
            "item/tool/requestUserInput" => {
                if let Some(request) = crate::questions::request(request_id, p) {
                    c.push_user_input(request);
                    return None;
                }
                return Some(Reply::Unsupported {
                    request_id: request_id.to_string(),
                    method: "malformed user-input request".to_string(),
                });
            }
            "item/commandExecution/requestApproval" => {
                let offered: Vec<Value> = p
                    .get("availableDecisions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_else(|| vec![json!("accept"), json!("acceptForSession")]);
                let has = |d: &str| offered.iter().any(|o| o.as_str() == Some(d));
                let (session, session_label) = if has("acceptForSession") {
                    (
                        json!("acceptForSession"),
                        Some("Allow for this session".into()),
                    )
                } else if let Some(rule) = offered
                    .iter()
                    .find(|o| o.get("acceptWithExecpolicyAmendment").is_some())
                {
                    let prefix = rule
                        .pointer("/acceptWithExecpolicyAmendment/execpolicy_amendment")
                        .and_then(Value::as_array)
                        .map(|words| {
                            words
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or_default();
                    (
                        rule.clone(),
                        Some(format!("Always allow `{}`", first_chars(&prefix, 40))),
                    )
                } else {
                    (Value::Null, None)
                };
                let deny = if has("decline") || !has("cancel") {
                    "decline"
                } else {
                    "cancel"
                };
                let network = p
                    .pointer("/networkApprovalContext/host")
                    .and_then(Value::as_str)
                    .map(|host| format!("Network access to {host}"));
                let stdin = p.get("kind").and_then(Value::as_str) == Some("writeStdin");
                PermissionRequest {
                    request_id: request_id.to_string(),
                    tool_name: "Shell".into(),
                    display_name: if stdin {
                        "input to a running command".into()
                    } else {
                        "command".into()
                    },
                    description: s("cwd").map(|cwd| {
                        tools::short_path(&tools::file_url_path(&cwd), c.cwd.as_deref())
                    }),
                    reason: s("reason").or(network),
                    input: json!({ "command": p.get("command"), "cwd": p.get("cwd") }),
                    suggestions: vec![json!({
                        "allow": { "decision": "accept" },
                        "session": if session.is_null() { Value::Null } else { json!({ "decision": session }) },
                        "deny": { "decision": deny },
                    })],
                    session_label,
                    tool_use_id: item_id,
                    decision: None,
                }
            }
            "item/fileChange/requestApproval" => {
                let changes = item_id
                    .as_deref()
                    .and_then(|id| c.tool(id))
                    .and_then(|t| t.input.get("changes").cloned())
                    .unwrap_or(Value::Null);
                PermissionRequest {
                    request_id: request_id.to_string(),
                    tool_name: "Edit".into(),
                    display_name: "file changes".into(),
                    description: s("grantRoot").map(|root| format!("writes under {root}")),
                    reason: s("reason"),
                    input: json!({ "changes": changes }),
                    suggestions: vec![json!({
                        "allow": { "decision": "accept" },
                        "session": { "decision": "acceptForSession" },
                        "deny": { "decision": "decline" },
                    })],
                    session_label: Some("Allow edits this session".into()),
                    tool_use_id: item_id,
                    decision: None,
                }
            }
            "item/permissions/requestApproval" => {
                let requested = p.get("permissions").cloned().unwrap_or(json!({}));
                let mut granted = serde_json::Map::new();
                for key in ["network", "fileSystem"] {
                    if let Some(v) = requested.get(key).filter(|v| !v.is_null()) {
                        granted.insert(key.into(), v.clone());
                    }
                }
                let granted = Value::Object(granted);
                PermissionRequest {
                    request_id: request_id.to_string(),
                    tool_name: "Permissions".into(),
                    display_name: "more access".into(),
                    description: None,
                    reason: s("reason"),
                    input: requested,
                    suggestions: vec![json!({
                        "allow": { "permissions": granted, "scope": "turn" },
                        "session": { "permissions": granted, "scope": "session" },
                        "deny": { "permissions": {}, "scope": "turn" },
                    })],
                    session_label: Some("Allow for this session".into()),
                    tool_use_id: item_id,
                    decision: None,
                }
            }
            "mcpServer/elicitation/request" => {
                c.push_notice(
                    format!(
                        "Declined a request for input from MCP server {}",
                        s("serverName").unwrap_or_else(|| "?".into())
                    ),
                    false,
                );
                return Some(Reply::Respond {
                    request_id: request_id.to_string(),
                    result: json!({ "action": "decline", "content": null, "_meta": null }),
                });
            }
            _ => {
                return Some(Reply::Unsupported {
                    request_id: request_id.to_string(),
                    method: method.to_string(),
                });
            }
        };
        c.push_permission(request);
        None
    }
}

fn user_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn turn_error(turn: &Value) -> String {
    turn.pointer("/error/message")
        .and_then(Value::as_str)
        .map(strip_ansi)
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "failed".into())
}

fn first_chars(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests;
