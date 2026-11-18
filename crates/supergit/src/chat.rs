//! The native agent chat: one `ChatSession` per conversation, shown in the
//! right-hand panel. Drives Claude Code (`sg_claude`, stream-json) or Codex
//! (`sg_codex`, app-server) and renders the shared chat model
//! (`sg_agents::chat`): messages, tool calls with inline diffs, and
//! permission prompts as cards. Each provider keeps its own controls: Claude's
//! permission modes, Codex's approval + sandbox policies.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::agent_context::{Attachment, MAX_ATTACHMENTS};
use crate::attention::{Attention, Kind};
use futures::StreamExt as _;
use gpui_kit::base::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffState};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use serde_json::Value;
use sg_agents::Provider;
use sg_agents::chat::{
    Conversation, Decision, InputResolution, Item, PermissionRequest, Status, ToolCall,
};
use sg_agents::process::{AgentProcess, ProcessEvent};
use sg_claude::conversation::Reply;
use sg_claude::{ClaudeEvents as _, ClaudeProcess, PermissionMode, SpawnOptions};
use sg_codex::{CodexFold, CodexProcess, Policy};

mod questions;

mod turn_plan;

/// Max lines of tool output shown when a tool row is expanded.
const OUTPUT_LINES: usize = 40;

/// Claude chats start in auto mode unless the user picks another. Where auto
/// mode is unavailable (plan, model, settings) the CLI starts in `default`
/// and says so in `system/init`, which the switcher follows.
const DEFAULT_MODE: PermissionMode = PermissionMode::Auto;

/// The provider-specific half of a conversation.
enum Backend {
    Claude {
        process: Option<Arc<ClaudeProcess>>,
        /// Picked by the user or last reported by the CLI; always passed
        /// when the process starts.
        mode: PermissionMode,
        /// A `set_permission_mode` awaiting the CLI's answer, with the mode
        /// to show again if it refuses.
        mode_request: Option<(String, PermissionMode)>,
    },
    Codex {
        process: Option<Arc<CodexProcess>>,
        fold: CodexFold,
        /// Sent with every turn: Codex applies policies per turn.
        policy: Policy,
        /// History is being read; sending waits for it so new turns land
        /// after it.
        loading: bool,
        history: Option<Task<()>>,
    },
}

impl Backend {
    fn new(provider: Provider, convo: &Conversation) -> Self {
        match provider {
            Provider::Claude => Self::Claude {
                process: None,
                mode: convo
                    .permission_mode
                    .as_deref()
                    .and_then(PermissionMode::from_wire)
                    .unwrap_or(DEFAULT_MODE),
                mode_request: None,
            },
            Provider::Codex => Self::Codex {
                process: None,
                fold: CodexFold::new(),
                policy: Policy::default(),
                loading: false,
                history: None,
            },
        }
    }

    fn provider(&self) -> Provider {
        match self {
            Self::Claude { .. } => Provider::Claude,
            Self::Codex { .. } => Provider::Codex,
        }
    }

    fn agent(&self) -> Option<Arc<dyn AgentProcess>> {
        match self {
            Self::Claude { process, .. } => process.clone().map(|p| p as Arc<dyn AgentProcess>),
            Self::Codex { process, .. } => process.clone().map(|p| p as Arc<dyn AgentProcess>),
        }
    }

    fn clear_process(&mut self) -> Option<Arc<dyn AgentProcess>> {
        match self {
            Self::Claude { process, .. } => process.take().map(|p| p as Arc<dyn AgentProcess>),
            Self::Codex { process, .. } => process.take().map(|p| p as Arc<dyn AgentProcess>),
        }
    }
}

fn placeholder(provider: Provider) -> String {
    format!(
        "Ask {}…  (Enter to send, Shift+Enter for a new line)",
        provider.label()
    )
}

pub struct ChatSession {
    git_busy: bool,
    worktree: PathBuf,
    attachments: Vec<Attachment>,
    context_preview: Option<usize>,
    attention: Attention,
    permission_focus: FocusHandle,
    /// Where the agent runs (a resumed session must run where it started).
    cwd: PathBuf,
    local_id: String,
    transcript: Option<PathBuf>,
    convo: Conversation,
    backend: Backend,
    input: Entity<TextareaState>,
    scroller: Entity<MessageScrollerState>,
    diffs: HashMap<String, (Entity<DiffState>, usize)>,
    question_forms: HashMap<String, Vec<questions::AnswerDraft>>,
    expanded: HashSet<usize>,
    stderr_tail: Vec<String>,
    reader: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ChatSession {
    /// A fresh conversation in `worktree`.
    pub fn new(
        worktree: PathBuf,
        provider: Provider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(
            worktree.clone(),
            worktree,
            provider,
            Conversation::new(),
            window,
            cx,
        )
    }

    /// Continue an existing session, showing its history.
    pub fn resume(
        worktree: PathBuf,
        session: &sg_agents::Session,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut convo = match session.provider {
            Provider::Claude => sg_claude::from_transcript(&session.transcript).unwrap_or_default(),
            Provider::Codex => Conversation::new(),
        };
        convo.session_id = Some(session.id.clone());
        if convo.title.is_none() {
            convo.title = Some(session.title.clone());
        }
        let cwd = session
            .start_cwd
            .clone()
            .unwrap_or_else(|| worktree.clone());
        let mut chat = Self::build(worktree, cwd, session.provider, convo, window, cx);
        chat.transcript = Some(session.transcript.clone());
        chat.load_history(cx);
        chat
    }

    /// Restore visible history and an unsent draft, without starting an agent.
    pub fn restore(
        worktree: PathBuf,
        saved: &crate::workspace::SavedConversation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let provider = Provider::parse(&saved.provider).unwrap_or_default();
        let transcript_exists = saved.transcript.as_ref().is_some_and(|p| p.is_file());
        let mut convo = match provider {
            Provider::Claude => saved
                .transcript
                .as_ref()
                .and_then(|path| sg_claude::from_transcript(path).ok())
                .unwrap_or_default(),
            Provider::Codex => Conversation::new(),
        };
        // Codex keeps threads it has not written a rollout for yet.
        if transcript_exists || (provider == Provider::Codex && saved.transcript.is_none()) {
            convo.session_id = saved.session_id.clone();
        }
        if convo.title.is_none() && !saved.title.is_empty() && saved.title != "New conversation" {
            convo.title = Some(saved.title.clone());
        }
        let cwd = if saved.cwd.is_dir() {
            saved.cwd.clone()
        } else {
            worktree.clone()
        };
        let mut chat = Self::build(worktree, cwd, provider, convo, window, cx);
        if !saved.local_id.is_empty() {
            chat.local_id = saved.local_id.clone();
        }
        chat.transcript = saved.transcript.clone().filter(|p| p.is_file());
        if let (Backend::Codex { policy, .. }, Some(saved_policy)) =
            (&mut chat.backend, Policy::parse(&saved.policy))
        {
            *policy = saved_policy;
        }
        chat.attention = saved.attention.clone().restored();
        chat.attachments = saved
            .attachments
            .iter()
            .filter(|item| item.worktree == chat.worktree)
            .take(MAX_ATTACHMENTS)
            .cloned()
            .map(Attachment::bounded)
            .collect();
        chat.input.update(cx, |input, cx| {
            input.set_value(saved.draft.clone(), window, cx)
        });
        chat.load_history(cx);
        chat
    }

    pub fn snapshot(&self, cx: &App) -> crate::workspace::SavedConversation {
        let transcript = match self.provider() {
            Provider::Claude => self.transcript.clone().or_else(|| {
                Some(
                    sg_agents::project_dir(&self.cwd)?
                        .join(format!("{}.jsonl", self.convo.session_id.as_ref()?)),
                )
            }),
            Provider::Codex => self.transcript.clone(),
        };
        crate::workspace::SavedConversation {
            attachments: self.attachments.clone(),
            attention: self.attention.clone(),
            local_id: self.local_id.clone(),
            provider: self.provider().as_str().into(),
            session_id: self.convo.session_id.clone(),
            transcript,
            cwd: self.cwd.clone(),
            title: self.title(),
            draft: self.input.read(cx).value().to_string(),
            policy: match &self.backend {
                Backend::Codex { policy, .. } => policy.as_str().into(),
                Backend::Claude { .. } => String::new(),
            },
        }
    }

    fn build(
        worktree: PathBuf,
        cwd: PathBuf,
        provider: Provider,
        convo: Conversation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 12)
                .submit_on_enter(true)
                .placeholder(placeholder(provider))
        });
        let sub = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                this.send(window, cx);
            } else if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let count = convo.items.len();
        let scroller = cx.new(|cx| MessageScrollerState::new(count, cx));
        let backend = Backend::new(provider, &convo);
        Self {
            git_busy: false,
            worktree,
            attachments: Vec::new(),
            context_preview: None,
            attention: Attention::default(),
            permission_focus: cx.focus_handle(),
            cwd,
            local_id: format!(
                "draft-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            transcript: None,
            convo,
            backend,
            input,
            scroller,
            diffs: HashMap::new(),
            question_forms: HashMap::new(),
            expanded: HashSet::new(),
            stderr_tail: Vec::new(),
            reader: None,
            _subscriptions: vec![sub],
        }
    }

    pub fn provider(&self) -> Provider {
        self.backend.provider()
    }

    /// Switch an untouched conversation to another agent.
    pub fn set_provider(
        &mut self,
        provider: Provider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if provider == self.provider() || !self.is_fresh() {
            return;
        }
        self.backend = Backend::new(provider, &self.convo);
        self.input.update(cx, |input, cx| {
            input.set_placeholder(placeholder(provider), window, cx)
        });
        let mut settings = crate::settings::load();
        settings.agent = provider.as_str().into();
        crate::settings::save(&settings);
        cx.notify();
    }

    /// Nothing sent yet and no session behind it: the agent can still change.
    pub fn is_fresh(&self) -> bool {
        self.convo.items.is_empty()
            && self.convo.session_id.is_none()
            && self.backend.agent().is_none()
    }

    pub fn status(&self) -> Status {
        self.convo.status
    }

    pub fn set_git_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        if self.git_busy == busy {
            return;
        }
        self.git_busy = busy;
        cx.notify();
    }

    pub fn take_idle_process(
        &mut self,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<Option<Arc<dyn AgentProcess>>> {
        anyhow::ensure!(
            !self.convo.status.is_busy(),
            "An agent is still running or waiting for approval"
        );
        self.reader = None;
        let process = self.backend.clear_process();
        if process.is_some() {
            self.convo.status = Status::Exited;
            cx.notify();
        }
        Ok(process)
    }

    pub fn local_id(&self) -> &str {
        &self.local_id
    }

    pub fn attention(&self) -> &Attention {
        &self.attention
    }

    pub fn mark_seen(&mut self, cx: &mut Context<Self>) {
        if self.attention.unread {
            self.attention.unread = false;
            cx.notify();
        }
    }

    pub fn focus_attention(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mark_seen(cx);
        let approval = self.convo.items.iter().rposition(
            |item| matches!(item, Item::Permission(request) if request.decision.is_none()),
        );
        let question = || {
            self.convo.items.iter().rposition(|item| {
            matches!(item, Item::UserInput(request) if request.resolution == InputResolution::Pending)
        })
        };
        if let Some(ix) = approval.or_else(question) {
            self.scroller.update(cx, |scroller, cx| {
                scroller.scroll_to_item(ix, cx);
            });
            self.permission_focus.focus(window, cx);
        } else {
            self.scroller
                .update(cx, |scroller, cx| scroller.scroll_to_end(cx));
            self.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        self.convo.session_id.as_deref()
    }

    pub fn title(&self) -> String {
        self.convo
            .title
            .clone()
            .unwrap_or_else(|| "New conversation".into())
    }

    pub fn input(&self) -> Entity<TextareaState> {
        self.input.clone()
    }

    pub fn follows_tail(&self, cx: &App) -> bool {
        self.scroller.read(cx).is_following_tail()
    }

    pub fn has_draft(&self, cx: &App) -> bool {
        !self.input.read(cx).value().is_empty() || !self.attachments.is_empty()
    }

    pub fn attach(
        &mut self,
        attachment: Attachment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        attachment.validate_destination(&self.worktree, &self.cwd)?;
        anyhow::ensure!(
            self.attachments.len() < MAX_ATTACHMENTS,
            "Remove an attachment before adding more (maximum {MAX_ATTACHMENTS})"
        );
        self.attachments.push(attachment.bounded());
        self.context_preview = Some(self.attachments.len() - 1);
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
        Ok(())
    }

    // ---- history ---------------------------------------------------------

    /// Codex history comes from Codex itself (`thread/turns/list` through a
    /// short-lived app server); Claude's was read from its transcript.
    fn load_history(&mut self, cx: &mut Context<Self>) {
        let Backend::Codex {
            loading, history, ..
        } = &mut self.backend
        else {
            return;
        };
        let Some(thread_id) = self.convo.session_id.clone() else {
            return;
        };
        let cwd = self.cwd.clone();
        *loading = true;
        // A thread of its own: the shared background pool may be busy with
        // slow Git and GitHub calls, and history should appear at once.
        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(sg_codex::read_history(None, &cwd, &thread_id));
        });
        *history = Some(cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("the history reader stopped")));
            let _ = this.update(cx, |this, cx| this.on_history(result, cx));
        }));
    }

    fn on_history(&mut self, result: anyhow::Result<sg_codex::History>, cx: &mut Context<Self>) {
        let Backend::Codex {
            loading, history, ..
        } = &mut self.backend
        else {
            return;
        };
        *loading = false;
        *history = None;
        let before = self.convo.items.len();
        match result {
            Ok(h) => {
                let title = self.convo.title.take();
                CodexFold::new().apply_history(&mut self.convo, &h);
                if self.convo.title.is_none() {
                    self.convo.title = title;
                }
                if self.transcript.is_none() {
                    self.transcript = h
                        .thread
                        .get("path")
                        .and_then(Value::as_str)
                        .map(PathBuf::from);
                }
            }
            Err(e) => self
                .convo
                .push_notice(format!("Couldn't load the Codex history: {e:#}"), true),
        }
        self.items_changed(before, cx);
    }

    fn history_loading(&self) -> bool {
        matches!(self.backend, Backend::Codex { loading: true, .. })
    }

    // ---- process ---------------------------------------------------------

    fn ensure_process(&mut self, cx: &mut Context<Self>) -> anyhow::Result<Arc<dyn AgentProcess>> {
        if let Some(p) = self.backend.agent()
            && self.convo.status != Status::Exited
        {
            return Ok(p);
        }
        let resume = self.convo.session_id.clone();
        let mut events = match &mut self.backend {
            Backend::Claude { process, mode, .. } => {
                let (p, events) = ClaudeProcess::spawn(&SpawnOptions {
                    cwd: self.cwd.clone(),
                    resume,
                    permission_mode: Some(*mode),
                    ..Default::default()
                })?;
                *process = Some(Arc::new(p));
                events
            }
            Backend::Codex {
                process, policy, ..
            } => {
                let (p, events) = CodexProcess::spawn(&sg_codex::SpawnOptions {
                    cwd: self.cwd.clone(),
                    resume,
                    policy: *policy,
                    ..Default::default()
                });
                *process = Some(Arc::new(p));
                events
            }
        };
        self.stderr_tail.clear();
        self.reader = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                if this
                    .update(cx, |this, cx| this.on_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
        self.backend
            .agent()
            .ok_or_else(|| anyhow::anyhow!("the agent did not start"))
    }

    fn on_event(&mut self, event: ProcessEvent, cx: &mut Context<Self>) {
        let before = self.convo.items.len();
        let was_busy = self.convo.status.is_busy();
        match event {
            ProcessEvent::Message(v) => {
                let outcome = match &mut self.backend {
                    Backend::Claude {
                        process,
                        mode,
                        mode_request,
                    } => {
                        let reported = self.convo.permission_mode.clone();
                        match self.convo.apply(&v) {
                            Some(Reply::Unsupported {
                                request_id,
                                subtype,
                            }) => {
                                if let Some(p) = process {
                                    let _ = p.control_error(
                                        &request_id,
                                        &format!("SuperGIT does not handle {subtype}"),
                                    );
                                }
                            }
                            Some(Reply::ControlResponse { request_id, error }) => on_mode_response(
                                &mut self.convo,
                                mode,
                                mode_request,
                                &request_id,
                                error,
                            ),
                            None => {}
                        }
                        // The CLI changed modes (our request, a settings
                        // default at start, an approved plan): follow it.
                        if self.convo.permission_mode != reported
                            && let Some(reported) = self
                                .convo
                                .permission_mode
                                .as_deref()
                                .and_then(PermissionMode::from_wire)
                        {
                            *mode = reported;
                        }
                        (v.get("type").and_then(Value::as_str) == Some("result"))
                            .then(|| v.get("is_error").and_then(Value::as_bool).unwrap_or(false))
                    }
                    Backend::Codex { process, fold, .. } => {
                        match fold.apply(&mut self.convo, &v) {
                            Some(sg_codex::Reply::Unsupported { request_id, method }) => {
                                if let Some(p) = process {
                                    let _ = p.reject(
                                        &request_id,
                                        &format!("SuperGIT does not handle {method}"),
                                    );
                                }
                            }
                            Some(sg_codex::Reply::Respond { request_id, result }) => {
                                if let Some(p) = process {
                                    let _ = p.respond(&request_id, result);
                                }
                            }
                            None => {}
                        }
                        codex_outcome(&v)
                    }
                };
                if let Some(is_error) = outcome {
                    self.attention.set(
                        if is_error {
                            Kind::Failed
                        } else {
                            Kind::Completed
                        },
                        None,
                    );
                } else {
                    self.sync_pending_attention();
                }
            }
            ProcessEvent::Stderr(line) => {
                self.stderr_tail.push(line);
                if self.stderr_tail.len() > 20 {
                    self.stderr_tail.remove(0);
                }
                return;
            }
            ProcessEvent::Exited => {
                if was_busy {
                    self.attention.set(Kind::Failed, None);
                }
                let label = self.provider().label();
                self.convo.on_exit(self.stderr_tail.last().cloned(), label);
                self.backend.clear_process();
                if !was_busy {
                    self.sync_pending_attention();
                }
            }
        }
        self.items_changed(before, cx);
    }

    fn sync_pending_attention(&mut self) {
        if let Some(request) = self.convo.pending_permission() {
            self.attention
                .set(Kind::Approval, Some(request.request_id.clone()));
        } else if let Some(request) = self.convo.pending_user_input() {
            self.attention
                .set(Kind::Question, Some(request.request_id.clone()));
        } else if self.convo.status.is_busy() {
            self.attention.set(Kind::Running, None);
        } else if matches!(self.attention.kind, Kind::Approval | Kind::Question) {
            self.attention.set(Kind::Idle, None);
        }
    }

    fn items_changed(&mut self, before: usize, cx: &mut Context<Self>) {
        self.question_forms.retain(|id, _| {
            self.convo
                .user_input(id)
                .is_some_and(|request| request.resolution == InputResolution::Pending)
        });
        let after = self.convo.items.len();
        self.scroller.update(cx, |s, cx| {
            if after > before {
                s.append(after - before, cx);
            }
            // Streaming text grows, tool results attach to earlier rows.
            s.remeasure(cx);
        });
        cx.notify();
    }

    // ---- user actions ----------------------------------------------------

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        if let Some(error) = self
            .attachments
            .iter()
            .find_map(|item| item.validate_destination(&self.worktree, &self.cwd).err())
        {
            self.convo.push_notice(error.to_string(), true);
            self.items_changed(self.convo.items.len().saturating_sub(1), cx);
            return;
        }
        let contexts = self
            .attachments
            .iter()
            .map(Attachment::prompt)
            .collect::<Vec<_>>()
            .join("\n");
        let mut prompt = text;
        if !contexts.is_empty() {
            prompt.push_str(crate::agent_context::CONTEXT_MARKER);
            prompt.push_str(&contexts);
        }
        if self.send_text(&prompt, cx) {
            self.input
                .update(cx, |state, cx| state.set_value("", window, cx));
            self.attachments.clear();
            self.context_preview = None;
        }
    }

    /// Send a turn (also used by automation in screenshot builds).
    pub fn send_text(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        if self.git_busy || self.history_loading() {
            return false;
        }
        self.attention.set(Kind::Running, None);
        let text = text.to_string();
        let before = self.convo.items.len();
        self.convo.push_user(&text);
        let result = self.ensure_process(cx).and_then(|p| p.send_user(&text));
        if let Err(e) = &result {
            self.convo.push_notice(format!("{e:#}"), true);
            self.convo.status = Status::Idle;
            self.attention.set(Kind::Failed, None);
        }
        self.items_changed(before, cx);
        result.is_ok()
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = self.backend.agent() {
            let _ = p.interrupt();
        }
        cx.notify();
    }

    /// Claude: applies at once to a running session (reverted with a notice
    /// if the CLI refuses), otherwise when the next turn starts the process.
    fn set_mode(&mut self, wanted: PermissionMode, cx: &mut Context<Self>) {
        if let Backend::Claude {
            process,
            mode,
            mode_request,
        } = &mut self.backend
        {
            let previous = std::mem::replace(mode, wanted);
            if let Some(p) = process
                && let Ok(request_id) = p.set_permission_mode(wanted)
            {
                *mode_request = Some((request_id, previous));
            }
        }
        cx.notify();
    }

    /// Codex: applies from the next turn on.
    pub fn set_policy(&mut self, wanted: Policy, cx: &mut Context<Self>) {
        if let Backend::Codex {
            process, policy, ..
        } = &mut self.backend
        {
            *policy = wanted;
            if let Some(p) = process {
                p.set_policy(wanted);
            }
        }
        cx.notify();
    }

    fn decide(&mut self, request_id: String, decision: Decision, cx: &mut Context<Self>) {
        let Some(request) = self.convo.permission(&request_id).cloned() else {
            return;
        };
        let Some(process) = self.backend.agent() else {
            return;
        };
        if decision == Decision::AllowedForSession
            && let Backend::Claude { mode, .. } = &mut self.backend
            && request
                .suggestions
                .iter()
                .any(|s| s.get("mode").and_then(|m| m.as_str()) == Some("acceptEdits"))
        {
            *mode = PermissionMode::AcceptEdits;
        }
        match process.answer(&request, decision) {
            Ok(()) => {
                self.convo.resolve_permission(&request_id, decision);
                self.sync_pending_attention();
            }
            Err(e) => self.convo.push_notice(format!("{e:#}"), true),
        }
        let len = self.convo.items.len();
        self.items_changed(len, cx);
    }

    fn toggle_expanded(&mut self, ix: usize, cx: &mut Context<Self>) {
        if !self.expanded.remove(&ix) {
            self.expanded.insert(ix);
        }
        let len = self.convo.items.len();
        self.items_changed(len, cx);
    }

    /// A cached diff view for an edit (by tool id / permission id).
    fn diff_for(
        &mut self,
        key: &str,
        patch: &str,
        cx: &mut Context<Self>,
    ) -> Option<(Entity<DiffState>, usize)> {
        if let Some(d) = self.diffs.get(key) {
            return Some(d.clone());
        }
        let files = crate::syntax::parse_patch(patch)
            .ok()
            .filter(|f| !f.is_empty())?;
        let lines = patch
            .lines()
            .filter(|l| {
                l.starts_with(['+', '-', ' ']) && !l.starts_with("+++") && !l.starts_with("---")
            })
            .count();
        let state = cx.new(|cx| DiffState::new(files, cx));
        self.diffs.insert(key.to_string(), (state.clone(), lines));
        Some((state, lines))
    }

    // ---- provider presentation -------------------------------------------

    fn tool_summary(&self, tool: &ToolCall) -> String {
        match self.provider() {
            Provider::Claude => sg_claude::tools::summary(&tool.name, &tool.input, Some(&self.cwd)),
            Provider::Codex => sg_codex::tools::summary(tool, Some(&self.cwd)),
        }
    }

    /// The diff of an applied edit.
    fn tool_patch(&self, tool: &ToolCall) -> Option<String> {
        if tool.result.as_ref().is_none_or(|r| r.is_error) {
            return None;
        }
        match self.provider() {
            Provider::Claude => sg_claude::tools::is_edit(&tool.name)
                .then(|| {
                    sg_claude::tools::edit_patch(
                        &tool.name,
                        &tool.input,
                        tool.result.as_ref().and_then(|r| r.meta.as_ref()),
                        Some(&self.cwd),
                    )
                })
                .flatten(),
            Provider::Codex => sg_codex::tools::is_edit(tool)
                .then(|| sg_codex::tools::edit_patch(tool, Some(&self.cwd)))
                .flatten(),
        }
    }

    /// What an approval card shows: a diff for edits, else text.
    fn permission_preview(&self, p: &PermissionRequest) -> (Option<String>, Option<String>) {
        match self.provider() {
            Provider::Claude if sg_claude::tools::is_edit(&p.tool_name) => (
                sg_claude::tools::edit_patch(&p.tool_name, &p.input, None, Some(&self.cwd)),
                None,
            ),
            Provider::Claude => (
                None,
                match p.tool_name.as_str() {
                    "Bash" => p
                        .input
                        .get("command")
                        .and_then(|c| c.as_str())
                        .map(String::from),
                    _ => serde_json::to_string_pretty(&p.input).ok(),
                },
            ),
            Provider::Codex => (
                sg_codex::tools::permission_patch(p, Some(&self.cwd)),
                sg_codex::tools::permission_text(p),
            ),
        }
    }

    // ---- rendering -------------------------------------------------------

    fn render_item(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(item) = self.convo.items.get(ix).cloned() else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let body = match item {
            Item::User { text } => {
                let attached = text.split_once(crate::agent_context::CONTEXT_MARKER);
                let expanded = self.expanded.contains(&ix);
                let display = if expanded {
                    text.clone()
                } else {
                    attached
                        .map(|(question, _)| question.to_string())
                        .unwrap_or_else(|| text.clone())
                };
                h_flex()
                    .justify_end()
                    .child(
                        div()
                            .max_w(relative(0.88))
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .bg(theme.secondary)
                            .text_color(theme.secondary_foreground)
                            .child(display)
                            .when(attached.is_some(), |bubble| {
                                bubble.child(
                                    Button::new(("message-context", ix))
                                        .label(if expanded {
                                            "Hide diff context"
                                        } else {
                                            "Show diff context"
                                        })
                                        .xsmall()
                                        .ghost()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.toggle_expanded(ix, cx)
                                        })),
                                )
                            }),
                    )
                    .into_any_element()
            }
            Item::Text { text, streaming } => div()
                .text_sm()
                .child(TextView::markdown(("md", ix), text))
                .when(streaming, |el| {
                    el.child(div().text_color(theme.muted_foreground).child("▍"))
                })
                .into_any_element(),
            // Finished thinking is noise in the transcript; show it only live.
            Item::Thinking { streaming: false } => return div().into_any_element(),
            Item::Thinking { streaming: true } => div()
                .text_xs()
                .italic()
                .text_color(theme.muted_foreground)
                .child("Thinking…")
                .into_any_element(),
            Item::Tool(tool) => self.render_tool(ix, &tool, &theme, cx).into_any_element(),
            Item::Permission(p) => self.render_permission(&p, &theme, cx).into_any_element(),
            Item::UserInput(request) => self.render_user_input(&request, window, cx),
            Item::Plan(plan) => turn_plan::render(&plan, &theme),
            Item::Notice { text, is_error } => div()
                .text_xs()
                .text_color(if is_error {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .child(text)
                .into_any_element(),
            Item::TurnEnd {
                duration_ms,
                is_error,
                message,
            } => {
                let secs = duration_ms.map(|ms| format!("{:.1}s", ms as f64 / 1000.0));
                let text = match (is_error, message) {
                    (true, Some(m)) => format!("✗ {m}"),
                    (true, None) => "✗ failed".into(),
                    _ => format!(
                        "✓ done{}",
                        secs.map(|s| format!(" · {s}")).unwrap_or_default()
                    ),
                };
                div()
                    .text_xs()
                    .text_color(if is_error {
                        theme.danger
                    } else {
                        theme.muted_foreground
                    })
                    .child(text)
                    .into_any_element()
            }
        };
        div().px_4().py_1p5().child(body).into_any_element()
    }

    fn render_tool(
        &mut self,
        ix: usize,
        tool: &ToolCall,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let summary = self.tool_summary(tool);
        let (dot, label_color) = match &tool.result {
            None if tool.checking => (theme.warning, theme.foreground),
            None => (theme.blue, theme.foreground),
            Some(r) if r.is_error => (theme.danger, theme.danger),
            Some(_) => (theme.success, theme.foreground),
        };
        let expanded = self.expanded.contains(&ix);
        // While the edit awaits approval the permission card shows the diff.
        let diff = self
            .tool_patch(tool)
            .and_then(|patch| self.diff_for(&tool.id, &patch, cx));
        let output = tool
            .result
            .as_ref()
            .filter(|_| expanded && diff.is_none())
            .map(|r| {
                let mut lines: Vec<&str> = r.text.lines().take(OUTPUT_LINES).collect();
                if r.text.lines().count() > OUTPUT_LINES {
                    lines.push("…");
                }
                lines.join("\n")
            });

        v_flex()
            .gap_1()
            .when(tool.parent.is_some(), |el| el.pl_4())
            .child(
                h_flex()
                    .id(("tool", ix))
                    .gap_2()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_expanded(ix, cx)))
                    .child(div().size(px(7.)).flex_none().rounded_full().bg(dot))
                    .child(
                        div()
                            .flex_none()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(label_color)
                            .child(tool.name.clone()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(summary),
                    )
                    .when(tool.checking, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(theme.warning)
                                .child("safety check…"),
                        )
                    }),
            )
            .when_some(diff, |el, (state, lines)| {
                el.child(diff_box(&state, lines, theme))
            })
            .when_some(output, |el, text| {
                el.child(
                    div()
                        .ml_4()
                        .p_2()
                        .rounded_md()
                        .bg(theme.muted)
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(text),
                )
            })
    }

    fn render_permission(
        &mut self,
        p: &PermissionRequest,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pending = p.decision.is_none();
        let (patch, text) = self.permission_preview(p);
        let preview: Option<AnyElement> = match patch
            .and_then(|patch| self.diff_for(&format!("perm:{}", p.request_id), &patch, cx))
        {
            Some((state, lines)) => Some(diff_box(&state, lines, theme).into_any_element()),
            None => text.map(|t| {
                let t: String = t.lines().take(20).collect::<Vec<_>>().join("\n");
                div()
                    .p_2()
                    .rounded_md()
                    .bg(theme.muted)
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .child(t)
                    .into_any_element()
            }),
        };
        let rid = p.request_id.clone();
        let rid2 = p.request_id.clone();
        let rid3 = p.request_id.clone();
        let session_label = p.session_label.clone();

        v_flex()
            .p_3()
            .gap_2()
            .rounded_lg()
            .border_1()
            .border_color(if pending { theme.warning } else { theme.border })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("Allow {}?", p.display_name)),
                    )
                    .when_some(p.description.clone(), |el, d| {
                        el.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(d),
                        )
                    }),
            )
            .when_some(p.reason.clone().filter(|_| pending), |el, reason| {
                el.child(div().text_xs().text_color(theme.warning).child(reason))
            })
            .children(preview)
            .child(if pending {
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new(("allow", rid.len()))
                            .label("Allow")
                            .small()
                            .primary()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.decide(rid.clone(), Decision::Allowed, cx)
                            })),
                    )
                    .when_some(session_label, |el, label| {
                        el.child(Button::new("allow-session").label(label).small().on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.decide(rid2.clone(), Decision::AllowedForSession, cx)
                            }),
                        ))
                    })
                    .child(
                        Button::new("deny")
                            .label("Deny")
                            .small()
                            .ghost()
                            .when(
                                self.convo
                                    .pending_permission()
                                    .is_some_and(|pending| pending.request_id == p.request_id),
                                |button| button.track_focus(&self.permission_focus),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.decide(rid3.clone(), Decision::Denied, cx)
                            })),
                    )
                    .into_any_element()
            } else {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(match p.decision {
                        Some(Decision::Denied) => "Denied",
                        Some(Decision::AllowedForSession) => "Allowed for this session",
                        Some(Decision::Expired) => "No longer needed",
                        _ => "Allowed",
                    })
                    .into_any_element()
            })
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let busy = self.convo.status.is_busy();
        let modes: Vec<AnyElement> = match &self.backend {
            Backend::Claude { mode: current, .. } => PermissionMode::ALL
                .into_iter()
                .enumerate()
                .map(|(i, mode)| {
                    let active = mode == *current;
                    Button::new(("mode", i))
                        .label(mode.label())
                        .xsmall()
                        .when(active, |b| b.secondary())
                        .when(!active, |b| b.ghost())
                        .tooltip(match mode {
                            PermissionMode::Default => "Ask before edits and commands",
                            PermissionMode::AcceptEdits => "Auto-accept file edits",
                            PermissionMode::Plan => "Plan only, no changes",
                            PermissionMode::Auto => {
                                "Claude decides; a safety check blocks risky actions"
                            }
                            PermissionMode::BypassPermissions => "Never ask (careful)",
                        })
                        .on_click(cx.listener(move |this, _, _, cx| this.set_mode(mode, cx)))
                        .into_any_element()
                })
                .collect(),
            Backend::Codex {
                policy: current, ..
            } => Policy::ALL
                .into_iter()
                .enumerate()
                .map(|(i, policy)| {
                    let active = policy == *current;
                    Button::new(("policy", i))
                        .label(policy.label())
                        .xsmall()
                        .when(active, |b| b.secondary())
                        .when(!active, |b| b.ghost())
                        .tooltip(format!(
                            "{} · approvals {}, sandbox {} · applies from the next turn",
                            policy.description(),
                            policy.approval_policy(),
                            policy.sandbox_mode()
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| this.set_policy(policy, cx)))
                        .into_any_element()
                })
                .collect(),
        };
        let loading = self.history_loading();
        v_flex()
            .p_2()
            .gap_2()
            .border_t_1()
            .border_color(theme.border)
            .when(!self.attachments.is_empty(), |composer| {
                composer.child(
                    v_flex()
                        .id("agent-contexts")
                        .max_h(px(220.))
                        .overflow_y_scroll()
                        .gap_1()
                        .children(self.attachments.iter().enumerate().map(|(ix, item)| {
                            v_flex()
                                .gap_1()
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child(
                                            Button::new(("inspect-context", ix))
                                                .label(format!(
                                                    "{} · {}{}{}",
                                                    item.path,
                                                    item.selection,
                                                    if item.truncated {
                                                        " · truncated"
                                                    } else {
                                                        ""
                                                    },
                                                    if item.unavailable {
                                                        " · metadata only"
                                                    } else {
                                                        ""
                                                    }
                                                ))
                                                .tooltip(format!(
                                                    "Snapshot · {} · {}",
                                                    item.worktree.display(),
                                                    item.comparison
                                                ))
                                                .xsmall()
                                                .ghost()
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.context_preview =
                                                        if this.context_preview == Some(ix) {
                                                            None
                                                        } else {
                                                            Some(ix)
                                                        };
                                                    cx.notify();
                                                })),
                                        )
                                        .child(
                                            Button::new(("remove-context", ix))
                                                .label("×")
                                                .xsmall()
                                                .ghost()
                                                .tooltip("Remove attachment")
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.attachments.remove(ix);
                                                    this.context_preview = None;
                                                    cx.notify();
                                                })),
                                        ),
                                )
                                .when(self.context_preview == Some(ix), |row| {
                                    row.child(
                                        v_flex()
                                            .gap_1()
                                            .p_2()
                                            .bg(theme.muted)
                                            .rounded_md()
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child(format!(
                                                        "Snapshot · {} · {}",
                                                        item.worktree.display(),
                                                        item.comparison
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .id(("context-preview", ix))
                                                    .max_h(px(140.))
                                                    .overflow_y_scroll()
                                                    .text_xs()
                                                    .font_family(theme.mono_font_family.clone())
                                                    .child(item.body.clone()),
                                            ),
                                    )
                                })
                        })),
                )
            })
            .child(Textarea::new(&self.input))
            .child(
                h_flex()
                    .justify_between()
                    .child(h_flex().gap_0p5().children(modes))
                    .child(if busy {
                        Button::new("stop")
                            .label("Stop")
                            .small()
                            .danger()
                            .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                    } else {
                        Button::new("send")
                            .label(if self.git_busy {
                                "Updating base…"
                            } else if loading {
                                "Loading history…"
                            } else {
                                "Send"
                            })
                            .disabled(self.git_busy || loading)
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
                    }),
            )
    }
}

impl Render for ChatSession {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let status = match self.convo.status {
            Status::Idle => None,
            Status::Starting => Some(("starting…", theme.muted_foreground)),
            Status::Working => Some(("working…", theme.blue)),
            Status::NeedsPermission => Some(("needs your approval", theme.warning)),
            Status::NeedsInput => Some(("needs your answer", theme.warning)),
            Status::Exited => Some((
                "stopped — your next message resumes",
                theme.muted_foreground,
            )),
        };
        let info = [
            Some(self.provider().label().to_string()),
            self.convo.model.clone(),
            self.convo.total_cost_usd.map(|c| format!("${c:.2}")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_4()
                    .py_1()
                    .gap_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(div().flex_1().min_w_0().truncate().child(info))
                    .when_some(status, |el, (label, color)| {
                        el.child(div().text_color(color).child(label))
                    }),
            )
            .child(if self.convo.items.is_empty() {
                let provider = self.provider();
                let fresh = self.is_fresh();
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .px_6()
                    .text_color(theme.muted_foreground)
                    .child(div().text_lg().child(format!(
                        "{} {}",
                        provider.glyph(),
                        provider.label()
                    )))
                    .when(fresh, |el| {
                        el.child(h_flex().gap_1().children(Provider::ALL.into_iter().map(
                            |choice| {
                                let active = choice == provider;
                                Button::new(("agent-choice", choice as usize))
                                    .label(format!("{} {}", choice.glyph(), choice.label()))
                                    .small()
                                    .when(active, |b| b.secondary())
                                    .when(!active, |b| b.ghost())
                                    .tooltip(format!(
                                        "Talk to {} in this conversation",
                                        choice.label()
                                    ))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.set_provider(choice, window, cx);
                                        let input = this.input.clone();
                                        input.update(cx, |input, cx| input.focus(window, cx));
                                    }))
                            },
                        )))
                    })
                    .child(
                        div()
                            .text_sm()
                            .text_center()
                            .child(if self.history_loading() {
                                "Loading the conversation…".to_string()
                            } else {
                                format!("Working in {}", self.cwd.display())
                            }),
                    )
                    .into_any_element()
            } else {
                div()
                    .flex_1()
                    .min_h_0()
                    .child(MessageScroller::new(
                        "chat-messages",
                        self.scroller.clone(),
                        cx.processor(|this, ix: usize, window, cx| {
                            this.render_item(ix, window, cx)
                        }),
                    ))
                    .into_any_element()
            })
            .child(self.render_composer(cx))
    }
}

fn diff_box(state: &Entity<DiffState>, lines: usize, theme: &Theme) -> impl IntoElement {
    // The diff view is virtualized and needs a definite height.
    let height = (lines.min(14) as f32) * 21. + 68.;
    div()
        .h(px(height))
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .child(Diff::new(state).line_number(true))
}

/// The CLI answered a `set_permission_mode`; on refusal show the mode it is
/// actually in.
fn on_mode_response(
    convo: &mut Conversation,
    mode: &mut PermissionMode,
    mode_request: &mut Option<(String, PermissionMode)>,
    request_id: &str,
    error: Option<String>,
) {
    let Some((pending, previous)) = mode_request.take() else {
        return;
    };
    if pending != request_id {
        *mode_request = Some((pending, previous));
        return;
    }
    let Some(error) = error else { return };
    let wanted = mode.label();
    *mode = convo
        .permission_mode
        .as_deref()
        .and_then(PermissionMode::from_wire)
        .unwrap_or(previous);
    convo.push_notice(format!("Can't switch to {wanted}: {error}"), true);
}

/// Whether a Codex event ended the turn, and if so whether it failed.
fn codex_outcome(v: &Value) -> Option<bool> {
    if v.get("method").and_then(Value::as_str) == Some("turn/completed") {
        return Some(v.pointer("/params/turn/status").and_then(Value::as_str) == Some("failed"));
    }
    let startup = matches!(
        v.get("response").and_then(Value::as_str),
        Some("initialize" | "account/read" | "thread/start" | "thread/resume" | "turn/start")
    );
    (startup && v.get("error").is_some()).then_some(true)
}
