//! The provider-neutral chat model: what a conversation with a coding agent
//! looks like on screen, whatever agent produced it.
//!
//! Adapters (`sg_claude`, `sg_codex`) fold their own wire events and
//! transcripts into a [`Conversation`]; the UI only reads it. Tool calls keep
//! the provider's raw `input`/`meta` — each adapter also owns the helpers that
//! summarize them for display.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
    /// The provider's structured result (Claude's `structuredPatch` for
    /// edits, Codex's completed item, …).
    pub meta: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
    pub result: Option<ToolResult>,
    /// Set when a subagent made the call.
    pub parent: Option<String>,
    /// A safety check is taking a while to judge this call.
    pub checking: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    /// Allowed and remembered (a session rule, a policy amendment, …).
    AllowedForSession,
    Denied,
    /// The agent stopped waiting without our answer (turn interrupted, an
    /// automatic reviewer decided).
    Expired,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PermissionRequest {
    /// The adapter's handle for answering (a control request id, a JSON-RPC
    /// request id).
    pub request_id: String,
    pub tool_name: String,
    pub display_name: String,
    pub description: Option<String>,
    /// Why the agent asks instead of deciding itself.
    pub reason: Option<String>,
    pub input: Value,
    /// Provider data needed to answer "allow and remember" (Claude's
    /// `permission_suggestions`, Codex's offered decision).
    pub suggestions: Vec<Value>,
    /// Label of the "allow and remember" button; `None` hides it.
    pub session_label: Option<String>,
    pub tool_use_id: Option<String>,
    pub decision: Option<Decision>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserQuestion {
    pub id: String,
    pub header: String,
    pub prompt: String,
    pub options: Vec<QuestionOption>,
    pub allow_other: bool,
    pub secret: bool,
    /// Any number of the options may be picked (checkboxes, not radios).
    pub multi_select: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputResolution {
    Pending,
    Answered,
    /// The user refused to answer (the agent was told so).
    Declined,
    Expired,
}

/// Answers by question id: the chosen option labels and/or typed text, in
/// the order the user gave them.
pub type Answers = BTreeMap<String, Vec<String>>;

#[derive(Clone, Debug, PartialEq)]
pub struct UserInputRequest {
    pub request_id: String,
    pub questions: Vec<UserQuestion>,
    pub blocking: bool,
    pub resolution: InputResolution,
    /// Provider data the answer must echo (Claude's `AskUserQuestion`
    /// input); `Null` when not needed.
    pub input: Value,
    /// What the user answered, once sent.
    pub answers: Answers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanStep {
    pub text: String,
    pub status: PlanStepStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnPlan {
    pub explanation: Option<String>,
    pub steps: Vec<PlanStep>,
    pub finished: bool,
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
    UserInput(UserInputRequest),
    Plan(TurnPlan),
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
    /// A permission prompt is waiting for the user.
    NeedsPermission,
    /// An interactive question is blocking the agent.
    NeedsInput,
    /// The process ended (the next turn starts it again and resumes).
    Exited,
}

impl Status {
    pub fn is_busy(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Working | Self::NeedsPermission | Self::NeedsInput
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct Conversation {
    pub items: Vec<Item>,
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<PathBuf>,
    /// The mode the agent reports (Claude's permission mode wire name).
    pub permission_mode: Option<String>,
    pub status: Status,
    pub total_cost_usd: Option<f64>,
    streaming_text: Option<usize>,
    streaming_thinking: Option<usize>,
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

    /// A user turn replayed from history (no status change).
    pub fn push_history_user(&mut self, text: &str) {
        if self.title.is_none() {
            self.title = Some(short_title(text));
        }
        self.items.push(Item::User {
            text: text.to_string(),
        });
    }

    pub fn push_notice(&mut self, text: impl Into<String>, is_error: bool) {
        self.items.push(Item::Notice {
            text: text.into(),
            is_error,
        });
    }

    pub fn pending_permission(&self) -> Option<&PermissionRequest> {
        self.items.iter().rev().find_map(|i| match i {
            Item::Permission(p) if p.decision.is_none() => Some(p),
            _ => None,
        })
    }

    pub fn permission(&self, request_id: &str) -> Option<&PermissionRequest> {
        self.items.iter().rev().find_map(|i| match i {
            Item::Permission(p) if p.request_id == request_id => Some(p),
            _ => None,
        })
    }

    /// Show a permission prompt and wait for the user.
    pub fn push_permission(&mut self, request: PermissionRequest) {
        self.items.push(Item::Permission(request));
        self.status = Status::NeedsPermission;
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
        self.settle_pending_requests();
    }

    pub fn pending_user_input(&self) -> Option<&UserInputRequest> {
        self.items.iter().rev().find_map(|item| match item {
            Item::UserInput(request) if request.resolution == InputResolution::Pending => {
                Some(request)
            }
            _ => None,
        })
    }

    pub fn user_input(&self, request_id: &str) -> Option<&UserInputRequest> {
        self.items.iter().rev().find_map(|item| match item {
            Item::UserInput(request) if request.request_id == request_id => Some(request),
            _ => None,
        })
    }

    pub fn push_user_input(&mut self, request: UserInputRequest) {
        if self
            .user_input(&request.request_id)
            .is_some_and(|current| current.resolution == InputResolution::Pending)
        {
            return;
        }
        self.items.push(Item::UserInput(request));
        self.settle_pending_requests();
    }

    /// The user's answers reached the agent.
    pub fn record_answers(&mut self, request_id: &str, answers: Answers) {
        for item in self.items.iter_mut().rev() {
            if let Item::UserInput(request) = item
                && request.request_id == request_id
                && request.resolution == InputResolution::Pending
            {
                request.answers = answers;
                break;
            }
        }
        self.resolve_user_input(request_id, InputResolution::Answered);
    }

    pub fn resolve_user_input(&mut self, request_id: &str, resolution: InputResolution) {
        for item in self.items.iter_mut().rev() {
            if let Item::UserInput(request) = item
                && request.request_id == request_id
                && request.resolution == InputResolution::Pending
            {
                request.resolution = resolution;
                break;
            }
        }
        self.settle_pending_requests();
    }

    fn settle_pending_requests(&mut self) {
        if self.pending_permission().is_some() {
            self.status = Status::NeedsPermission;
        } else if self.items.iter().any(|item| matches!(item,
            Item::UserInput(request) if request.blocking && request.resolution == InputResolution::Pending)) {
            self.status = Status::NeedsInput;
        } else if matches!(self.status, Status::NeedsPermission | Status::NeedsInput) {
            self.status = Status::Working;
        }
    }

    pub fn tool(&self, id: &str) -> Option<&ToolCall> {
        self.items.iter().find_map(|i| match i {
            Item::Tool(t) if t.id == id => Some(t),
            _ => None,
        })
    }

    pub fn tool_mut(&mut self, id: &str) -> Option<&mut ToolCall> {
        self.items.iter_mut().rev().find_map(|i| match i {
            Item::Tool(t) if t.id == id => Some(t),
            _ => None,
        })
    }

    /// Mark a tool call as being (or no longer being) safety-checked.
    pub fn set_checking(&mut self, tool_use_id: &str, checking: bool) {
        if let Some(t) = self.tool_mut(tool_use_id) {
            t.checking = checking;
        }
    }

    /// Start a streaming text block (the current one until completed).
    pub fn start_text(&mut self) {
        self.items.push(Item::Text {
            text: String::new(),
            streaming: true,
        });
        self.streaming_text = Some(self.items.len() - 1);
    }

    /// Append to the current streaming text block, if any.
    pub fn append_text(&mut self, delta: &str) {
        if let Some(ix) = self.streaming_text
            && let Some(Item::Text { text, .. }) = self.items.get_mut(ix)
        {
            text.push_str(delta);
        }
    }

    /// The final text of a block: replaces the streamed one, or adds it when
    /// nothing was streamed.
    pub fn complete_text(&mut self, full: &str) {
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

    pub fn start_thinking(&mut self) {
        self.items.push(Item::Thinking { streaming: true });
        self.streaming_thinking = Some(self.items.len() - 1);
    }

    pub fn complete_thinking(&mut self) {
        match self.streaming_thinking.take() {
            Some(ix) => {
                if let Some(Item::Thinking { streaming }) = self.items.get_mut(ix) {
                    *streaming = false;
                }
            }
            None => self.items.push(Item::Thinking { streaming: false }),
        }
    }

    /// Nothing streams any more (turn ended, process exited).
    pub fn finish_streaming(&mut self) {
        self.streaming_text = None;
        self.streaming_thinking = None;
        for item in &mut self.items {
            match item {
                Item::Text { streaming, .. } | Item::Thinking { streaming } => *streaming = false,
                Item::Plan(plan) => plan.finished = true,
                _ => {}
            }
        }
    }

    /// The turn ended: record it, and settle what it left open — prompts
    /// nobody answered and pending safety checks are moot now.
    pub fn end_turn(&mut self, duration_ms: Option<u64>, is_error: bool, message: Option<String>) {
        self.finish_streaming();
        self.items.push(Item::TurnEnd {
            duration_ms,
            is_error,
            message,
        });
        for item in &mut self.items {
            match item {
                Item::Permission(p) if p.decision.is_none() => p.decision = Some(Decision::Denied),
                Item::UserInput(request) if request.resolution == InputResolution::Pending => {
                    request.resolution = InputResolution::Expired;
                }
                Item::Tool(t) => t.checking = false,
                _ => {}
            }
        }
        self.status = Status::Idle;
    }

    /// The process exited (crash, `/exit`, killed).
    pub fn on_exit(&mut self, reason: Option<String>, agent: &str) {
        self.finish_streaming();
        if self.status.is_busy() {
            self.push_notice(reason.unwrap_or_else(|| format!("{agent} exited")), true);
        }
        for item in &mut self.items {
            match item {
                Item::Permission(p) if p.decision.is_none() => p.decision = Some(Decision::Expired),
                Item::UserInput(request) if request.resolution == InputResolution::Pending => {
                    request.resolution = InputResolution::Expired;
                }
                _ => {}
            }
        }
        self.status = Status::Exited;
    }
}

/// A conversation title from its first prompt: one line, at most 48 chars.
pub fn short_title(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 48 {
        format!("{}…", line.chars().take(47).collect::<String>())
    } else {
        line
    }
}

/// Drop ANSI escape sequences and other control characters: texts written
/// for a terminal UI may be coloured.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters up to a final byte in `@`..=`~`.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC (hyperlinks, titles): up to BEL or ESC `\`.
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_and_completes_text() {
        let mut c = Conversation::new();
        c.push_user("hi");
        assert_eq!(c.status, Status::Starting);
        c.start_text();
        c.append_text("Hel");
        c.append_text("lo");
        assert_eq!(
            c.items[1],
            Item::Text {
                text: "Hello".into(),
                streaming: true
            }
        );
        c.complete_text("Hello!");
        c.complete_text("  ");
        assert_eq!(c.items.len(), 2);
        assert_eq!(
            c.items[1],
            Item::Text {
                text: "Hello!".into(),
                streaming: false
            }
        );
    }

    #[test]
    fn turn_end_settles_open_prompts() {
        let mut c = Conversation::new();
        c.push_permission(PermissionRequest {
            request_id: "r1".into(),
            tool_name: "Bash".into(),
            display_name: "Bash".into(),
            description: None,
            reason: None,
            input: Value::Null,
            suggestions: Vec::new(),
            session_label: None,
            tool_use_id: None,
            decision: None,
        });
        assert_eq!(c.status, Status::NeedsPermission);
        c.end_turn(Some(5), false, None);
        assert_eq!(c.status, Status::Idle);
        assert_eq!(c.permission("r1").unwrap().decision, Some(Decision::Denied));
    }

    #[test]
    fn blocking_and_optional_questions_have_independent_lifetimes() {
        let mut conversation = Conversation {
            status: Status::Working,
            ..Conversation::default()
        };
        let optional = UserInputRequest {
            request_id: "optional".into(),
            questions: Vec::new(),
            blocking: false,
            resolution: InputResolution::Pending,
            input: Value::Null,
            answers: Answers::new(),
        };
        let blocking = UserInputRequest {
            request_id: "blocking".into(),
            blocking: true,
            ..optional.clone()
        };
        conversation.push_user_input(optional);
        assert_eq!(conversation.status, Status::Working);
        conversation.push_user_input(blocking.clone());
        conversation.push_user_input(blocking);
        assert_eq!(conversation.items.len(), 2);
        assert_eq!(conversation.status, Status::NeedsInput);
        let answers = Answers::from([("q".to_string(), vec!["a".to_string(), "b".to_string()])]);
        conversation.record_answers("blocking", answers.clone());
        assert_eq!(conversation.status, Status::Working);
        assert_eq!(
            conversation.user_input("blocking").unwrap().answers,
            answers
        );
        conversation.end_turn(None, false, None);
        assert_eq!(
            conversation.user_input("optional").unwrap().resolution,
            InputResolution::Expired
        );
        assert_eq!(
            conversation.user_input("blocking").unwrap().resolution,
            InputResolution::Answered
        );
    }

    #[test]
    fn exit_while_busy_reports() {
        let mut c = Conversation::new();
        c.push_user("go");
        c.on_exit(Some("boom".into()), "Claude");
        assert_eq!(c.status, Status::Exited);
        assert!(matches!(
            c.items.last(),
            Some(Item::Notice { is_error: true, text }) if text == "boom"
        ));
        let mut idle = Conversation::new();
        idle.on_exit(None, "Codex");
        assert!(idle.items.is_empty());
    }

    #[test]
    fn strips_terminal_escapes() {
        assert_eq!(
            strip_ansi(
                "\u{1b}[31mred\u{1b}[0m \u{1b}]8;;https://x\u{7}link\u{1b}]8;;\u{1b}\\ ok\u{7}\n"
            ),
            "red link ok\n"
        );
    }

    #[test]
    fn titles_cut_by_characters() {
        let long = "Исправь ".repeat(10);
        let title = short_title(&long);
        assert_eq!(title.chars().count(), 48);
        assert!(title.ends_with('…'));
    }
}
