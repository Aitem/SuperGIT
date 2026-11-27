//! The native agent chat: one `ChatSession` per conversation, shown in the
//! right-hand panel. Drives Claude Code (`sg_claude`, stream-json) or Codex
//! (`sg_codex`, app-server) and renders the shared chat model
//! (`sg_agents::chat`): messages, tool calls with inline diffs, and
//! permission prompts as cards. Each provider keeps its own controls: Claude's
//! permission modes, Codex's approval + sandbox policies.

mod composer_height;
mod options;

pub use composer_height::init;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::agent_context::{Attachment, MAX_ATTACHMENTS};
use crate::attention::{Attention, Kind};
use futures::StreamExt as _;
use gpui_kit::assets::IconName;
use gpui_kit::base::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::diff::{Diff, DiffState};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::shimmer::ShimmerText;
use gpui_kit::component::text::TextView;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, Theme, h_flex, v_flex};
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
use sg_codex::{CodexFold, CodexProcess, CollaborationMode, Policy};

mod activity;
mod look;
mod questions;
mod terminal_mode;
mod timeline;

mod collaboration;
mod turn_plan;
mod usage;

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
        collaboration: Option<CollaborationMode>,
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
                collaboration: None,
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
    format!("Ask {}…", provider.label())
}

/// Where Claude Code keeps the transcript of session `id` started in `cwd`.
fn claude_transcript(cwd: &std::path::Path, id: &str) -> Option<PathBuf> {
    Some(sg_agents::project_dir(cwd)?.join(format!("{id}.jsonl")))
}

/// What a conversation tells the app.
pub enum ChatEvent {
    /// `Chat | Terminal` picked for a new conversation: the default from now
    /// on, as if picked in Settings.
    PickedView(crate::settings::AgentView),
}

impl EventEmitter<ChatEvent> for ChatSession {}

pub struct ChatSession {
    /// Start pressed: a new chat shows its composer (the agent itself starts
    /// with the first message). Until then its screen offers the choices.
    started: bool,
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
    /// A fork of this session that has no id of its own yet: it shows that
    /// session's history, and its agent starts as a copy of it (Claude's
    /// `--fork-session`, Codex's `thread/fork`). The original stays as it is.
    fork_from: Option<String>,
    /// The name the user gave it (Rename): shown instead of the agent's
    /// title, which keeps changing underneath.
    name: Option<String>,
    convo: Conversation,
    backend: Backend,
    options: options::AgentOptions,
    input: Entity<TextareaState>,
    composer: composer_height::ComposerSize,
    scroller: Entity<MessageScrollerState>,
    diffs: HashMap<String, (Entity<DiffState>, usize)>,
    question_forms: HashMap<String, Vec<questions::AnswerDraft>>,
    expanded: HashSet<usize>,
    /// Runs of tool calls the user unfolded, by their first item.
    runs: timeline::Runs,
    open_runs: HashSet<usize>,
    /// The latest user message has scrolled out above the chat: it sticks
    /// to the top.
    sticky_prompt: bool,
    /// Since when the agent has been busy, for the activity line's clock,
    /// and what redraws that clock every second.
    busy_since: Option<Instant>,
    busy_clock: Option<Task<()>>,
    stderr_tail: Vec<String>,
    reader: Option<Task<()>>,
    /// Reading a Claude transcript for the history.
    transcript_load: Option<Task<()>>,
    /// Shown as the agent's own TUI in the embedded terminal instead.
    terminal: Option<terminal_mode::TerminalMode>,
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
        let mut chat = Self::build(
            worktree.clone(),
            worktree,
            provider,
            Conversation::new(),
            window,
            cx,
        );
        if crate::settings::load().agent_view == crate::settings::AgentView::Terminal {
            chat.terminal = Some(Default::default());
        }
        chat
    }

    /// Continue an existing session as its agent's TUI (not started yet).
    pub fn resume_terminal(
        worktree: PathBuf,
        session: &sg_agents::Session,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut convo = Conversation::new();
        convo.session_id = Some(session.id.clone());
        convo.title = Some(session.title.clone());
        let cwd = session
            .start_cwd
            .clone()
            .unwrap_or_else(|| worktree.clone());
        let mut chat = Self::build(worktree, cwd, session.provider, convo, window, cx);
        chat.transcript = Some(session.transcript.clone());
        chat.terminal = Some(Default::default());
        chat
    }

    /// Continue an existing session, showing its history.
    pub fn resume(
        worktree: PathBuf,
        session: &sg_agents::Session,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut convo = Conversation::new();
        convo.session_id = Some(session.id.clone());
        convo.title = Some(session.title.clone());
        let cwd = session
            .start_cwd
            .clone()
            .unwrap_or_else(|| worktree.clone());
        let mut chat = Self::build(worktree, cwd, session.provider, convo, window, cx);
        chat.transcript = Some(session.transcript.clone());
        chat.load_transcript(cx);
        chat.load_history(cx);
        chat
    }

    /// What a fork of this conversation restores from (`restore`): its
    /// history so far, continued from the first message on as a copy of its
    /// session, in the same view (chat or TUI) and with the same options.
    /// `None` while there is no session with history to copy.
    pub fn fork_seed(&self, cx: &App) -> Option<crate::workspace::SavedConversation> {
        let id = self
            .convo
            .session_id
            .clone()
            .or_else(|| self.fork_from.clone())?;
        // `--resume` fails without a transcript: nothing was said yet.
        if self.provider() == Provider::Claude
            && !claude_transcript(&self.cwd, &id).is_some_and(|path| path.is_file())
        {
            return None;
        }
        Some(crate::workspace::SavedConversation {
            local_id: String::new(),
            session_id: None,
            transcript: None,
            fork_from: Some(id),
            title: format!("{} (fork)", self.title()),
            // A named original's fork keeps a name too.
            name: self.name.as_ref().map(|name| format!("{name} (fork)")),
            draft: String::new(),
            attachments: Vec::new(),
            attention: Default::default(),
            ..self.snapshot(cx)
        })
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
        let mut convo = Conversation::new();
        // Codex keeps threads it has not written a rollout for yet; a new
        // terminal session keeps the id it was started with.
        if transcript_exists
            || (provider == Provider::Codex && saved.transcript.is_none())
            || saved.terminal
        {
            convo.session_id = saved.session_id.clone();
        }
        // The transcript's own title replaces it once read. A TUI's name
        // (saved as "Claude Code" by earlier versions) is no title.
        if !saved.title.is_empty()
            && saved.title != "New conversation"
            && !terminal_mode::is_program_name(&saved.title)
        {
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
        // A fork that has not started yet shows its original's history.
        chat.name = saved.name.clone().filter(|name| !name.trim().is_empty());
        chat.fork_from = saved
            .fork_from
            .clone()
            .filter(|_| saved.session_id.is_none());
        chat.transcript = match (&chat.fork_from, provider) {
            (Some(id), Provider::Claude) => claude_transcript(&chat.cwd, id),
            (Some(_), Provider::Codex) => None,
            (None, _) => saved.transcript.clone(),
        }
        .filter(|p| p.is_file());
        if saved.terminal {
            chat.terminal = Some(Default::default());
        }
        if let (Backend::Codex { policy, .. }, Some(saved_policy)) =
            (&mut chat.backend, Policy::parse(&saved.policy))
        {
            *policy = saved_policy;
        }
        if let Backend::Codex { collaboration, .. } = &mut chat.backend {
            *collaboration = saved
                .codex_collaboration_mode
                .as_deref()
                .and_then(CollaborationMode::parse);
        }
        chat.options.model = saved.model.clone();
        chat.options.effort = saved.effort.clone();
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
        // A terminal conversation shows its history in the TUI.
        if !chat.is_terminal() {
            chat.load_transcript(cx);
            chat.load_history(cx);
        }
        chat
    }

    /// The session's transcript (Claude's jsonl, Codex's rollout), if it
    /// has one; a fork has its own only once it started.
    pub fn transcript_file(&self) -> Option<PathBuf> {
        if self.fork_from.is_some() {
            return None;
        }
        match self.provider() {
            Provider::Claude => self
                .transcript
                .clone()
                .or_else(|| claude_transcript(&self.cwd, self.convo.session_id.as_ref()?)),
            Provider::Codex => self.transcript.clone(),
        }
    }

    pub fn snapshot(&self, cx: &App) -> crate::workspace::SavedConversation {
        let transcript = self.transcript_file();
        crate::workspace::SavedConversation {
            attachments: self.attachments.clone(),
            attention: self.attention.clone(),
            local_id: self.local_id.clone(),
            provider: self.provider().as_str().into(),
            session_id: self.convo.session_id.clone(),
            transcript,
            cwd: self.cwd.clone(),
            title: self.agent_title(),
            model: self.options.model.clone(),
            effort: self.options.effort.clone(),
            draft: self.input.read(cx).value().to_string(),
            policy: match &self.backend {
                Backend::Codex { policy, .. } => policy.as_str().into(),
                Backend::Claude { .. } => String::new(),
            },
            codex_collaboration_mode: match &self.backend {
                Backend::Codex { collaboration, .. } => {
                    collaboration.map(|mode| mode.as_str().into())
                }
                Backend::Claude { .. } => None,
            },
            terminal: self.is_terminal(),
            fork_from: self.fork_from.clone(),
            name: self.name.clone(),
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
        // Scrolling only redraws the chat when the sticky prompt shows or hides.
        let sticky = cx.observe(&scroller, |this: &mut Self, _, cx| {
            if let Some(above) = this.prompt_above(cx)
                && above != this.sticky_prompt
            {
                this.sticky_prompt = above;
                cx.notify();
            }
        });
        let backend = Backend::new(provider, &convo);
        Self {
            started: false,
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
            fork_from: None,
            name: None,
            convo,
            backend,
            options: Default::default(),
            input,
            composer: Default::default(),
            scroller,
            diffs: HashMap::new(),
            question_forms: HashMap::new(),
            expanded: HashSet::new(),
            runs: Default::default(),
            open_runs: HashSet::new(),
            sticky_prompt: false,
            busy_since: None,
            busy_clock: None,
            stderr_tail: Vec::new(),
            reader: None,
            transcript_load: None,
            terminal: None,
            _subscriptions: vec![sub, sticky],
        }
    }

    /// Where the agent runs: where its session started.
    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    /// A session with history to fork, or a fork of one that has not started.
    pub fn can_fork(&self) -> bool {
        self.convo.session_id.is_some() || self.fork_from.is_some()
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
        self.options = Default::default();
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
            && self.fork_from.is_none()
            && self.backend.agent().is_none()
            && self.terminal.as_ref().is_none_or(|t| t.view.is_none())
    }

    pub fn status(&self) -> Status {
        self.terminal_status().unwrap_or(self.convo.status)
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
        if self.is_terminal() {
            self.release_terminal(cx)?;
            return Ok(None);
        }
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
        if self.focus_terminal(window, cx) {
            return;
        }
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

    /// The name the user gave it, else the agent's title.
    pub fn title(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.agent_title())
    }

    /// What the agent calls it (its transcript, its TUI), whatever the
    /// user named it.
    fn agent_title(&self) -> String {
        self.convo
            .title
            .clone()
            .or_else(|| self.terminal_title().map(str::to_string))
            .unwrap_or_else(|| "New conversation".into())
    }

    /// Rename: `None` (or blank) gives back the agent's own title.
    pub fn set_name(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        let name = name
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty());
        if self.name != name {
            self.name = name;
            cx.notify();
        }
    }

    /// The agent is busy with a turn here: working, or stopped on a
    /// question or an approval. A TUI that is only starting up is not.
    pub fn is_working(&self) -> bool {
        match self.status() {
            Status::Working | Status::NeedsPermission | Status::NeedsInput => true,
            Status::Starting => !self.is_terminal(),
            Status::Idle | Status::Exited => false,
        }
    }

    /// Put the keyboard focus where the user types: the composer, or the
    /// terminal.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        if !self.focus_terminal(window, cx) {
            self.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// Text for the user to edit and send: in the composer, or typed into
    /// the agent's prompt in a terminal.
    pub fn set_draft(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_terminal() {
            self.type_into_terminal(text, false, cx);
        } else {
            self.input
                .update(cx, |input, cx| input.set_value(text, window, cx));
        }
    }

    pub fn follows_tail(&self, cx: &App) -> bool {
        self.is_terminal() || self.scroller.read(cx).is_following_tail()
    }

    /// A new conversation's screen: the agent and the view still to pick,
    /// then Start. A draft (restored, or typed in by Ask Agent) skips it.
    fn choosing(&self, cx: &App) -> bool {
        self.is_fresh() && !self.started && !self.has_draft(cx)
    }

    /// Start a new conversation the way it was picked: its TUI, or the chat
    /// to type the first message in.
    pub fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_terminal() {
            self.start_terminal(window, cx);
        } else {
            self.started = true;
            self.focus(window, cx);
            cx.notify();
        }
    }

    /// Back to the choices (New on an untouched conversation).
    pub fn offer_choices(&mut self, cx: &mut Context<Self>) {
        if self.started {
            self.started = false;
            cx.notify();
        }
    }

    pub fn has_draft(&self, cx: &App) -> bool {
        !self.is_terminal()
            && (!self.input.read(cx).value().is_empty() || !self.attachments.is_empty())
    }

    pub fn attach(
        &mut self,
        attachment: Attachment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        attachment.validate_destination(&self.worktree, &self.cwd)?;
        if self.is_terminal() {
            self.type_into_terminal(attachment.bounded().prompt(), false, cx);
            self.focus(window, cx);
            return Ok(());
        }
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
        // A fork shows its original's history until it starts.
        let Some(thread_id) = self
            .convo
            .session_id
            .clone()
            .or_else(|| self.fork_from.clone())
        else {
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
                if self.fork_from.is_some() {
                    // The original's id and name: the fork has neither yet.
                    self.convo.session_id = None;
                    self.convo.title = title;
                } else if self.convo.title.is_none() {
                    self.convo.title = title;
                }
                if self.transcript.is_none() && self.fork_from.is_none() {
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
        self.transcript_load.is_some()
            || matches!(self.backend, Backend::Codex { loading: true, .. })
    }

    /// Read a Claude session's history off the main thread: long sessions
    /// have transcripts of tens of megabytes.
    fn load_transcript(&mut self, cx: &mut Context<Self>) {
        if self.provider() != Provider::Claude {
            return;
        }
        let Some(path) = self.transcript.clone() else {
            return;
        };
        // A thread of its own, as for Codex history: the shared background
        // pool may be busy with slow Git and GitHub calls.
        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(sg_claude::from_transcript(&path));
        });
        self.transcript_load = Some(cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("the transcript reader stopped")));
            let _ = this.update(cx, |this, cx| this.on_transcript(result, cx));
        }));
    }

    fn on_transcript(&mut self, result: anyhow::Result<Conversation>, cx: &mut Context<Self>) {
        self.transcript_load = None;
        match result {
            Ok(mut loaded) => {
                // The chat decided which session it continues.
                loaded.session_id = self.convo.session_id.take();
                // A fork keeps its own title ("… (fork)").
                if loaded.title.is_none() || self.fork_from.is_some() {
                    loaded.title = self.convo.title.take();
                }
                // Notices shown meanwhile follow the history.
                loaded.items.append(&mut self.convo.items);
                if let Backend::Claude {
                    process: None,
                    mode,
                    ..
                } = &mut self.backend
                    && let Some(saved) = loaded
                        .permission_mode
                        .as_deref()
                        .and_then(PermissionMode::from_wire)
                {
                    *mode = saved;
                }
                self.runs.clear();
                self.convo = loaded;
            }
            Err(e) => self.convo.push_notice(
                format!("Couldn't read the conversation history: {e:#}"),
                true,
            ),
        }
        let count = self.convo.items.len();
        self.scroller.update(cx, |s, cx| s.reset(count, cx));
        cx.notify();
    }

    // ---- process ---------------------------------------------------------

    fn ensure_process(&mut self, cx: &mut Context<Self>) -> anyhow::Result<Arc<dyn AgentProcess>> {
        if let Some(p) = self.backend.agent()
            && self.convo.status != Status::Exited
        {
            return Ok(p);
        }
        // A fork that has not started continues a copy of its original.
        let fork = self.convo.session_id.is_none() && self.fork_from.is_some();
        let resume = self
            .convo
            .session_id
            .clone()
            .or_else(|| self.fork_from.clone());
        let initial_prompt = crate::initial_prompt::for_worktree(&self.worktree, cx);
        let mut events = match &mut self.backend {
            Backend::Claude { process, mode, .. } => {
                let (p, events) = ClaudeProcess::spawn(&SpawnOptions {
                    cwd: self.cwd.clone(),
                    resume,
                    fork,
                    permission_mode: Some(*mode),
                    model: self.options.model.clone(),
                    effort: self.options.effort.clone(),
                    append_system_prompt: initial_prompt,
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
                    fork,
                    policy: *policy,
                    developer_instructions: initial_prompt,
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
                        (v.get("type").and_then(Value::as_str) == Some("result")).then(|| {
                            if sg_claude::conversation::was_stopped(&v) {
                                Outcome::Stopped
                            } else if v.get("is_error").and_then(Value::as_bool) == Some(true) {
                                Outcome::Failed
                            } else {
                                Outcome::Completed
                            }
                        })
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
                match outcome {
                    Some(Outcome::Completed) => self.attention.set(Kind::Completed, None),
                    Some(Outcome::Failed) => self.attention.set(Kind::Failed, None),
                    // The user's own Stop: nothing to come back to.
                    Some(Outcome::Stopped) => self.attention.set(Kind::Idle, None),
                    None => self.sync_pending_attention(),
                }
                // A fork is its own session once its agent names it.
                if self.fork_from.is_some() && self.convo.session_id.is_some() {
                    self.fork_from = None;
                    self.transcript = None;
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
        self.runs.clear();
        self.track_activity(cx);
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
        if self.is_terminal() {
            self.type_into_terminal(text.to_string(), true, cx);
            return true;
        }
        self.attention.set(Kind::Running, None);
        let text = text.to_string();
        let before = self.convo.items.len();
        self.convo.push_user(&text);
        let result = self
            .ensure_process(cx)
            .and_then(|process| match &self.backend {
                Backend::Codex {
                    process: Some(codex),
                    collaboration,
                    ..
                } => codex.send_turn_in_mode(
                    &text,
                    self.options.codex_turn(self.convo.model.as_deref()),
                    *collaboration,
                ),
                _ => process.send_user(&text),
            });
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

    /// The name a tool row shows.
    fn tool_label(&self, tool: &ToolCall) -> String {
        match self.provider() {
            Provider::Claude => sg_claude::tools::display_name(&tool.name),
            Provider::Codex => tool.name.clone(),
        }
    }

    /// What an unfolded tool row shows above the output: the command itself,
    /// where the row's summary is what the command is for.
    fn tool_detail(&self, tool: &ToolCall) -> Option<String> {
        match self.provider() {
            Provider::Claude => sg_claude::tools::detail(&tool.name, &tool.input),
            Provider::Codex => None,
        }
    }

    /// The plan a plan row unfolds into.
    fn tool_plan(&self, tool: &ToolCall) -> Option<String> {
        if self.provider() != Provider::Claude || tool.name != sg_claude::tools::PLAN {
            return None;
        }
        tool.input.get("plan")?.as_str().map(String::from)
    }

    /// Whether the tool's output is read from its end (a command's).
    fn reads_from_end(&self, tool: &ToolCall) -> bool {
        match self.provider() {
            Provider::Claude => sg_claude::tools::reads_from_end(&tool.name),
            // Codex's calls with output are commands.
            Provider::Codex => true,
        }
    }

    /// The plan an approval prompt asks about (Claude leaving plan mode).
    fn permission_plan(&self, p: &PermissionRequest) -> Option<String> {
        if self.provider() != Provider::Claude || p.tool_name != sg_claude::tools::PLAN {
            return None;
        }
        p.input.get("plan")?.as_str().map(String::from)
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

    /// The latest user message: its row and the prompt (without attached
    /// diff context).
    fn latest_prompt(&self) -> Option<(usize, &str)> {
        self.convo
            .items
            .iter()
            .enumerate()
            .rev()
            .find_map(|(ix, item)| match item {
                Item::User { text, .. } => Some((
                    ix,
                    text.split(crate::agent_context::CONTEXT_MARKER)
                        .next()
                        .unwrap_or_default(),
                )),
                _ => None,
            })
    }

    /// Has the latest user message scrolled out above the chat? `None`
    /// right after a jump to the end, until the list lays out again.
    fn prompt_above(&self, cx: &App) -> Option<bool> {
        let Some((ix, _)) = self.latest_prompt() else {
            return Some(false);
        };
        let scroller = self.scroller.read(cx);
        let top = scroller.logical_scroll_top().item_ix;
        (top < scroller.item_count()).then_some(top > ix)
    }

    /// The latest user message, stuck to the top of the chat while it is
    /// scrolled out of view; a click scrolls back to it. It floats over the
    /// conversation, so it is a raised bar, not one more message card.
    fn render_sticky_prompt(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.sticky_prompt {
            return None;
        }
        let (ix, prompt) = self.latest_prompt()?;
        let line = prompt
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default()
            .to_string();
        let theme = cx.theme().clone();
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .child(
                    // Level with the message cards.
                    div().px_3().pt_2().child(
                        h_flex()
                            .id("sticky-prompt")
                            .gap_2()
                            .items_center()
                            .px_3()
                            .py_1p5()
                            .rounded_lg()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.background)
                            .shadow_md()
                            .text_sm()
                            .cursor_pointer()
                            .hover(|el| el.bg(theme.muted))
                            .tooltip(|window, cx| {
                                Tooltip::new("Scroll to this message").build(window, cx)
                            })
                            .tooltip_show_delay(crate::TOOLTIP_DELAY)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.scroller
                                    .update(cx, |scroller, cx| scroller.scroll_to_item(ix, cx));
                            }))
                            .child(
                                Icon::new(IconName::ArrowUp)
                                    .xsmall()
                                    .flex_none()
                                    .text_color(theme.muted_foreground),
                            )
                            .child(div().min_w_0().truncate().child(line)),
                    ),
                )
                .into_any_element(),
        )
    }

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
        let provider = self.provider();
        let row = timeline::indexed_row(
            &self.convo.items,
            ix,
            &mut |i| {
                self.runs.lookup(&self.convo.items, i, &|item| {
                    Self::groupable(provider, item)
                })
            },
            &self.open_runs,
        );
        let (shape, first, last) = match row {
            timeline::Row::Hidden => return div().into_any_element(),
            timeline::Row::User => return self.render_user_message(ix, item, &theme, cx),
            timeline::Row::Step { shape, first, last } => (shape, first, last),
        };
        let busy = self.convo.status.is_busy();
        // Tool calls run together; messages and cards get some air.
        let (top, bottom) = match (&shape, &item) {
            (timeline::Shape::Run(_), _) => (px(3.), px(3.)),
            (_, Item::Tool(_)) => (px(2.), px(2.)),
            (_, Item::Permission(p)) if p.decision.is_some() => (px(2.), px(2.)),
            (_, Item::Text { .. }) => (px(7.), px(7.)),
            (_, Item::Notice { .. } | Item::TurnEnd { .. }) => (px(4.), px(4.)),
            _ => (px(6.), px(6.)),
        };
        let mark = match &shape {
            timeline::Shape::Run(run) => look::Mark::Dot {
                color: self.run_color(run.clone(), busy, &theme),
                size: 7.,
            },
            timeline::Shape::InRun => look::step_mark(&item, busy, &theme).in_run(),
            timeline::Shape::Single => look::step_mark(&item, busy, &theme),
        };
        let body = match (shape, item) {
            (timeline::Shape::Run(run), _) => self.render_run(ix, run, &theme, cx),
            (
                _,
                Item::Text {
                    text,
                    streaming,
                    at,
                },
            ) => {
                // Only the answer carries its time, not every remark on the
                // way to it.
                let closes = !streaming && timeline::closes_turn(&self.convo.items, ix, busy);
                v_flex()
                    .min_w_0()
                    .gap_1()
                    .text_sm()
                    .child(
                        TextView::markdown(("md", ix), text)
                            .selectable(true)
                            // Words appear as they are written.
                            .stream_fade(streaming)
                            .code_block_actions(|block, _, _| {
                                Clipboard::new("copy").value(block.code()).xsmall()
                            }),
                    )
                    .when(closes, |el| {
                        el.child(look::time_footer(("response", ix), at, &theme))
                    })
                    .into_any_element()
            }
            (_, Item::Tool(tool)) => self.render_tool(ix, &tool, &theme, cx).into_any_element(),
            (_, Item::Permission(p)) => self.render_permission(&p, &theme, cx),
            (_, Item::UserInput(request)) => self.render_user_input(&request, window, cx),
            (_, Item::Plan(plan)) => turn_plan::render(&plan, &theme),
            (_, Item::Notice { text, is_error }) => div()
                .text_xs()
                .text_color(if is_error {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .child(text)
                .into_any_element(),
            (
                _,
                Item::TurnEnd {
                    duration_ms,
                    is_error,
                    message,
                },
            ) => {
                // The mark beside it says how the turn ended.
                let mut facts = vec![match (is_error, message) {
                    (true, Some(message)) => message,
                    (true, None) => "Failed".to_string(),
                    _ => "Done".to_string(),
                }];
                facts.extend(duration_ms.map(|ms| look::duration_text(Duration::from_millis(ms))));
                // What the session has cost so far ends its latest turn.
                if let Some(cost) = self.convo.total_cost_usd
                    && self
                        .convo
                        .items
                        .iter()
                        .rposition(|item| matches!(item, Item::TurnEnd { .. }))
                        == Some(ix)
                {
                    facts.push(format!("${cost:.2} total"));
                }
                div()
                    .text_xs()
                    .text_color(if is_error {
                        theme.danger
                    } else {
                        theme.muted_foreground
                    })
                    .child(facts.join(" · "))
                    .into_any_element()
            }
            // `timeline::row` hides thinking and makes user turns `Row::User`.
            (_, Item::Thinking { .. } | Item::User { .. }) => div().into_any_element(),
        };
        // Not `h_flex`: the gutter stretches to the row's height for its line.
        div()
            .flex()
            .flex_row()
            .child(look::gutter(first, last, top, mark, &theme))
            .child(div().flex_1().min_w_0().pt(top).pb(bottom).child(body))
            .into_any_element()
    }

    /// A user turn: a card across the chat, its time under it. A long one (a
    /// pasted log) shows its first lines until it is unfolded.
    fn render_user_message(
        &mut self,
        ix: usize,
        item: Item,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Item::User { text, at } = item else {
            return div().into_any_element();
        };
        let attached = text.split_once(crate::agent_context::CONTEXT_MARKER);
        let prompt = attached.map_or(text.as_str(), |(question, _)| question);
        let long = prompt.lines().count() > look::PROMPT_LINES || prompt.chars().count() > 1200;
        let expanded = self.expanded.contains(&ix);
        let toggle = match (attached.is_some(), long, expanded) {
            (true, _, false) => Some((
                "Show diff context",
                "Show the diff lines attached to this message",
            )),
            (true, _, true) => Some(("Hide diff context", "Fold the attached diff lines away")),
            (false, true, false) => Some(("Show more", "Unfold the whole message")),
            (false, true, true) => Some(("Show less", "Fold the message to its first lines")),
            (false, false, _) => None,
        };
        let mut body = TextView::html(
            ("user-message", ix),
            plain_message_html(if expanded { &text } else { prompt }),
        )
        .selectable(true);
        if long && !expanded {
            body = body.max_lines(look::PROMPT_LINES);
        }
        v_flex()
            // The first message opens the conversation, later ones a turn.
            .pt(px(if ix == 0 { 0. } else { 14. }))
            .pb(px(6.))
            .gap_1()
            .child(
                v_flex()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.muted)
                    .text_sm()
                    .child(body)
                    .when_some(toggle, |card, (label, tooltip)| {
                        // In a row of its own, so that it keeps its width.
                        card.child(
                            h_flex().child(
                                Button::new(("message-more", ix))
                                    .label(label)
                                    .xsmall()
                                    .ghost()
                                    .tooltip(tooltip)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_expanded(ix, cx)
                                    })),
                            ),
                        )
                    }),
            )
            .child(look::time_footer(("user", ix), at, theme))
            .into_any_element()
    }

    /// Tool calls that may fold into a run: not edits, which show a diff,
    /// nor the calls that are a step of their own (a subagent, a plan).
    fn groupable(provider: Provider, item: &Item) -> bool {
        let Item::Tool(tool) = item else {
            return false;
        };
        match provider {
            Provider::Claude => {
                !sg_claude::tools::is_edit(&tool.name)
                    && !sg_claude::tools::stands_alone(&tool.name)
            }
            Provider::Codex => !sg_codex::tools::is_edit(tool),
        }
    }

    fn run_calls(&self, run: std::ops::Range<usize>) -> Vec<ToolCall> {
        self.convo.items[run.start..run.end.min(self.convo.items.len())]
            .iter()
            .filter_map(|item| match item {
                Item::Tool(tool) => Some(tool.clone()),
                _ => None,
            })
            .collect()
    }

    /// The run's dot: running while any call runs, red after a failed one.
    fn run_color(&self, run: std::ops::Range<usize>, busy: bool, theme: &Theme) -> Hsla {
        let calls = self.run_calls(run);
        let failed = |call: &ToolCall| call.result.as_ref().is_some_and(|r| r.is_error);
        if let Some(running) = calls.iter().find(|call| call.result.is_none())
            && busy
        {
            let checking = calls.iter().find(|call| call.checking);
            look::tool_color(checking.unwrap_or(running), busy, theme)
        } else if calls.iter().any(failed) {
            theme.danger
        } else if let Some(done) = calls.iter().find(|call| call.result.is_some()) {
            look::tool_color(done, busy, theme)
        } else {
            // Stopped with its turn before anything came back.
            calls.first().map_or(theme.muted_foreground, |call| {
                look::tool_color(call, busy, theme)
            })
        }
    }

    /// A folded run of tool calls ("› Bash ×3, Read · the latest call");
    /// unfolded, the first call follows and the rest are their own rows.
    fn render_run(
        &mut self,
        ix: usize,
        run: std::ops::Range<usize>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let calls = self.run_calls(run);
        let open = self.open_runs.contains(&ix);
        let names: Vec<String> = calls.iter().map(|call| self.tool_label(call)).collect();
        let latest = calls
            .last()
            .filter(|_| !open)
            .map(|call| self.tool_summary(call))
            .filter(|summary| !summary.is_empty());
        let checking = calls.iter().any(|call| call.checking);
        // A subagent's calls sit under its own.
        let nested = calls.first().is_some_and(|call| call.parent.is_some());
        let header = h_flex()
            .id(("run", ix))
            .gap_1p5()
            .min_w_0()
            .items_center()
            .when(nested, |el| el.pl_3())
            .cursor_pointer()
            .text_size(px(look::STEP_TEXT))
            .text_color(theme.muted_foreground)
            .hover(|el| el.text_color(theme.foreground))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_run(ix, cx)))
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .xsmall()
                .flex_none(),
            )
            .child(
                div()
                    .flex_none()
                    .max_w(relative(0.62))
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .child(timeline::run_label(names.iter().map(String::as_str))),
            )
            .when_some(latest, |el, summary| {
                el.child(div().min_w_0().truncate().opacity(0.8).child(summary))
            })
            .when(checking, |el| el.child(safety_check(theme)));
        let first = calls.into_iter().next().filter(|_| open);
        v_flex()
            .gap_1()
            .min_w_0()
            .child(header)
            .when_some(first, |el, tool| {
                el.child(self.render_tool(ix, &tool, theme, cx))
            })
            .into_any_element()
    }

    fn toggle_run(&mut self, ix: usize, cx: &mut Context<Self>) {
        if !self.open_runs.remove(&ix) {
            self.open_runs.insert(ix);
        }
        let len = self.convo.items.len();
        self.items_changed(len, cx);
    }

    /// A tool call: its name and what it is about on one line. Unfolded, a
    /// command shows itself and its output, a plan the plan; an edit always
    /// shows its diff, a failed call why it failed.
    fn render_tool(
        &mut self,
        ix: usize,
        tool: &ToolCall,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let name = self.tool_label(tool);
        let summary = self.tool_summary(tool);
        let failed = tool.result.as_ref().is_some_and(|r| r.is_error);
        let expanded = self.expanded.contains(&ix);
        // While the edit awaits approval the permission card shows the diff.
        let diff = match self.diffs.get(&tool.id) {
            Some(diff) => Some(diff.clone()),
            None => self
                .tool_patch(tool)
                .and_then(|patch| self.diff_for(&tool.id, &patch, cx)),
        };
        let plan = self.tool_plan(tool).filter(|_| expanded);
        let detail = self
            .tool_detail(tool)
            .filter(|_| expanded && diff.is_none());
        let output = tool
            .result
            .as_ref()
            .filter(|r| expanded && diff.is_none() && plan.is_none() && !r.text.trim().is_empty())
            .map(|r| look::excerpt(&r.text, self.reads_from_end(tool)));
        // Folded, a failed call still says why.
        let error = tool
            .result
            .as_ref()
            .filter(|r| r.is_error && !expanded)
            .and_then(|r| r.text.lines().map(str::trim).find(|line| !line.is_empty()))
            .map(String::from);

        v_flex()
            .gap_1()
            .min_w_0()
            .when(tool.parent.is_some(), |el| el.pl_3())
            .child(
                h_flex()
                    .id(("tool", ix))
                    .gap_1p5()
                    .min_w_0()
                    .items_center()
                    .cursor_pointer()
                    .text_size(px(look::STEP_TEXT))
                    // Calls are background: muted, red only when one failed
                    // (its state is the timeline dot).
                    .text_color(theme.muted_foreground)
                    .hover(|el| el.text_color(theme.foreground))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_expanded(ix, cx)))
                    .child(
                        div()
                            .flex_none()
                            .max_w(relative(0.62))
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .when(failed, |el| el.text_color(theme.danger))
                            .child(name),
                    )
                    .child(div().min_w_0().truncate().opacity(0.8).child(summary))
                    .when(tool.checking, |el| el.child(safety_check(theme))),
            )
            .when_some(error, |el, line| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(theme.danger)
                        .truncate()
                        .child(line),
                )
            })
            .when_some(diff, |el, (state, lines)| {
                el.child(diff_box(&state, lines, theme))
            })
            .when_some(plan, |el, plan| {
                el.child(plan_box(("plan", ix), plan, theme))
            })
            .when_some(detail, |el, command| {
                el.child(look::code_box(command, theme.foreground, theme))
            })
            .when_some(output, |el, text| {
                el.child(look::code_box(text, theme.muted_foreground, theme))
            })
    }

    /// An approval prompt: what the agent wants to do and what that would
    /// change, until it is answered; then one line saying how.
    fn render_permission(
        &mut self,
        p: &PermissionRequest,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let plan = self.permission_plan(p);
        if let Some(decision) = p.decision {
            let (icon, color, text) = match (decision, plan.is_some()) {
                (Decision::Allowed, true) => {
                    (IconName::Check, theme.success, "Plan approved".into())
                }
                (Decision::Allowed, false) => (
                    IconName::Check,
                    theme.success,
                    format!("Allowed {}", p.display_name),
                ),
                // What was allowed from now on, in the button's own words.
                (Decision::AllowedForSession, _) => (
                    IconName::Check,
                    theme.success,
                    p.session_label
                        .clone()
                        .unwrap_or_else(|| format!("Allowed {} from now on", p.display_name)),
                ),
                (Decision::Denied, true) => (
                    IconName::Close,
                    theme.muted_foreground,
                    "Plan sent back".into(),
                ),
                (Decision::Denied, false) => (
                    IconName::Close,
                    theme.muted_foreground,
                    format!("Denied {}", p.display_name),
                ),
                (Decision::Expired, _) => (
                    IconName::Close,
                    theme.muted_foreground,
                    format!("{} was not answered", p.display_name),
                ),
            };
            return h_flex()
                .gap_1p5()
                .min_w_0()
                .items_center()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(Icon::new(icon).xsmall().flex_none().text_color(color))
                .child(div().min_w_0().truncate().child(text))
                .into_any_element();
        }

        let is_plan = plan.is_some();
        let preview: Option<AnyElement> = match plan {
            Some(plan) => Some(
                plan_box(
                    SharedString::from(format!("plan-{}", p.request_id)),
                    plan,
                    theme,
                )
                .into_any_element(),
            ),
            None => {
                let (patch, text) = self.permission_preview(p);
                match patch
                    .and_then(|patch| self.diff_for(&format!("perm:{}", p.request_id), &patch, cx))
                {
                    Some((state, lines)) => Some(diff_box(&state, lines, theme).into_any_element()),
                    None => text.map(|text| {
                        let text: String = text.lines().take(20).collect::<Vec<_>>().join("\n");
                        look::code_box(text, theme.foreground, theme).into_any_element()
                    }),
                }
            }
        };
        let button = |action: &str| SharedString::from(format!("{action}-{}", p.request_id));
        let rid = p.request_id.clone();
        let rid2 = p.request_id.clone();
        let rid3 = p.request_id.clone();
        let session_label = p.session_label.clone();
        let focused = self
            .convo
            .pending_permission()
            .is_some_and(|pending| pending.request_id == p.request_id);

        v_flex()
            .p_3()
            .gap_2()
            .min_w_0()
            .rounded_lg()
            .border_1()
            .border_color(theme.warning)
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .items_center()
                    .child(
                        Icon::new(if is_plan {
                            IconName::Map
                        } else {
                            IconName::TriangleAlert
                        })
                        .small()
                        .flex_none()
                        .text_color(theme.warning),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if is_plan {
                                "Plan ready".to_string()
                            } else {
                                format!("Allow {}?", p.display_name)
                            }),
                    )
                    .when_some(p.description.clone().filter(|_| !is_plan), |el, d| {
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
            .when_some(p.reason.clone(), |el, reason| {
                el.child(div().text_xs().text_color(theme.warning).child(reason))
            })
            .children(preview)
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new(button("allow"))
                            .label(if is_plan { "Approve plan" } else { "Allow" })
                            .small()
                            .primary()
                            .tooltip(if is_plan {
                                "Let the agent leave planning and carry out this plan"
                            } else {
                                "Let this call run once; the next one asks again"
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.decide(rid.clone(), Decision::Allowed, cx)
                            })),
                    )
                    .when_some(session_label, |el, label| {
                        el.child(
                            Button::new(button("allow-session"))
                                .label(label)
                                .small()
                                .outline()
                                .tooltip("Let this call run, and ones like it from now on without asking")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.decide(rid2.clone(), Decision::AllowedForSession, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new(button("deny"))
                            .label(if is_plan { "Keep planning" } else { "Deny" })
                            .small()
                            .ghost()
                            .tooltip(if is_plan {
                                "Send the plan back; the agent stays in plan mode"
                            } else {
                                "Refuse this call and tell the agent so"
                            })
                            .when(focused, |button| button.track_focus(&self.permission_focus))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.decide(rid3.clone(), Decision::Denied, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    /// The composer, flush with the panel under a top border that resizes
    /// the text box: attachments and the text box, then a row with the mode
    /// on the left and the model, the effort and Send (Stop while the agent
    /// works) on the right.
    fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let busy = self.convo.status.is_busy();
        let loading = self.history_loading();
        let send = if busy {
            Button::new("stop")
                .icon(IconName::Square)
                .rounded_full()
                .tooltip("Interrupt the agent; the conversation stays")
                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
        } else {
            Button::new("send")
                .icon(IconName::ArrowUp)
                .primary()
                .rounded_full()
                .disabled(self.git_busy || loading || !self.has_draft(cx))
                .tooltip(if self.git_busy {
                    "A Git operation is running"
                } else if loading {
                    "Loading the conversation…"
                } else {
                    "Send (Enter)"
                })
                .on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
        };
        let content = v_flex()
            .gap_1()
            .px_3()
            .pt_2()
            .pb_3()
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
                                                .icon(IconName::Close)
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
            .child(self.render_text_box(cx))
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(self.render_mode_options(cx))
                    .children(self.render_codex_collaboration(cx))
                    .child(div().flex_1())
                    .children(self.render_codex_usage(cx).map(|usage| {
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(usage)
                    }))
                    .child(self.render_model_options(cx))
                    .child(send),
            );
        // The edge spans the panel, over the composer's top padding.
        div()
            .flex_none()
            .relative()
            .bg(theme.background)
            .on_drag_move(
                cx.listener(|this, event, window, cx| this.drag_composer_edge(event, window, cx)),
            )
            .child(content)
            // Last, so it lies over the padding it shares the top edge with.
            .child(self.render_composer_edge(cx))
    }
}

impl Render for ChatSession {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // New messages move the latest prompt too, not only scrolling.
        if let Some(above) = self.prompt_above(cx) {
            self.sticky_prompt = above;
        }
        if self.is_terminal() {
            // On screen: a terminal conversation starts its TUI by itself.
            self.auto_start_terminal(window, cx);
            return self.render_terminal(cx);
        }
        self.load_models(cx);
        self.sync_composer_height(window, cx);
        let theme = cx.theme().clone();
        let choosing = self.choosing(cx);
        v_flex()
            .size_full()
            .child(if self.convo.items.is_empty() {
                // The panel's header names the agent: here only the
                // choices and Start (or that the history is loading).
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_8()
                    .px_6()
                    .text_color(theme.muted_foreground)
                    .when(choosing, |el| {
                        el.child(self.render_new_choices(cx))
                            .child(self.render_start("Start", cx))
                    })
                    .when(self.history_loading(), |el| {
                        el.child(ShimmerText::new("Loading the conversation…").text_sm())
                    })
                    .into_any_element()
            } else {
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(
                        MessageScroller::new(
                            "chat-messages",
                            self.scroller.clone(),
                            cx.processor(|this, ix: usize, window, cx| {
                                this.render_item(ix, window, cx)
                            }),
                        )
                        // Its 32px between rows made runs of tool calls sparse;
                        // `render_item` spaces rows by kind instead. Its own
                        // 12px at the sides line the rows up with the panel's
                        // header and the composer, across the panel's width.
                        .with_row_style(StyleRefinement::default().pb(px(0.)))
                        .with_list_style(StyleRefinement::default().pt_3().pb_3())
                        // Scrolled up, the conversation fades out at the
                        // bottom instead of ending mid-line.
                        .with_bottom_fade(theme.background),
                    )
                    .children(self.render_sticky_prompt(cx))
                    .into_any_element()
            })
            .children(self.render_activity(cx))
            .when(!choosing, |el| el.child(self.render_composer(cx)))
            .into_any_element()
    }
}

/// How a turn ended, for the conversation's mark and its notification.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Completed,
    Failed,
    /// The user stopped it.
    Stopped,
}

/// "safety check…": auto mode is taking a while to judge a call.
fn safety_check(theme: &Theme) -> Div {
    div()
        .flex_none()
        .text_xs()
        .text_color(theme.warning)
        .child("safety check…")
}

/// A plan as Claude wrote it (Markdown).
fn plan_box(id: impl Into<ElementId>, plan: String, theme: &Theme) -> Div {
    div()
        .min_w_0()
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .text_sm()
        .child(TextView::markdown(id, plan).selectable(true))
}

fn plain_message_html(text: &str) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\n', "<br>");
    format!("<p>{escaped}</p>")
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

/// Whether a Codex event ended the turn, and if so how.
fn codex_outcome(v: &Value) -> Option<Outcome> {
    if v.get("method").and_then(Value::as_str) == Some("turn/completed") {
        return Some(
            match v.pointer("/params/turn/status").and_then(Value::as_str) {
                Some("failed") => Outcome::Failed,
                Some("interrupted") => Outcome::Stopped,
                _ => Outcome::Completed,
            },
        );
    }
    let startup = matches!(
        v.get("response").and_then(Value::as_str),
        Some("initialize" | "account/read" | "thread/start" | "thread/resume" | "turn/start")
    );
    (startup && v.get("error").is_some()).then_some(Outcome::Failed)
}
