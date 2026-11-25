//! A conversation shown as the agent's own TUI in the embedded terminal
//! (`Settings::agent_view`). The tab, attention and persistence stay those
//! of `ChatSession`; the agent is `claude` / `codex` on a PTY instead of the
//! stream-json / app-server process.
//!
//! Status comes from the program itself where it reports it (Claude Code
//! speaks OSC 7501: working, blocked on a permission or a question, done),
//! otherwise from output activity.

use std::ffi::OsString;
use std::time::Duration;

use gpui_kit::base::{Disableable as _, Selectable as _};
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sg_agents::Provider;
use sg_agents::chat::Status;
use sg_terminal::{BlockedOn, ProgramState, Spawn};

use super::ChatSession;
use crate::attention::Kind;
use crate::terminal::{TerminalEvent, TerminalView};

/// Without status reports: output quiet this long after working = done.
const QUIET: Duration = Duration::from_secs(3);
/// Ready for typed input: the program drew its screen and then went quiet
/// this long (Claude Code reports no state until a turn starts).
const SETTLED: Duration = Duration::from_millis(800);

/// Terminal-mode state of a conversation.
#[derive(Default)]
pub(super) struct TerminalMode {
    /// `None` until started: once on screen it starts on its own
    /// (`auto_start_terminal`).
    pub(super) view: Option<Entity<TerminalView>>,
    pub(super) status: Status,
    /// Started on its own once on screen, or tried to.
    auto_started: bool,
    /// The Claude Code process that has the session open elsewhere, which
    /// kept it from starting on its own.
    elsewhere: Option<u32>,
    /// The program sends status reports: no guessing from output.
    reports_status: bool,
    /// Ready for input (said so, or had time to start).
    ready: bool,
    /// Text to type once the program is ready (an issue draft, context),
    /// and whether to press Enter after it.
    queued: Vec<(String, bool)>,
    quiet_check: Option<Task<()>>,
    settle_check: Option<Task<()>>,
    title: String,
    _subscription: Option<Subscription>,
}

impl ChatSession {
    pub fn is_terminal(&self) -> bool {
        self.terminal.is_some()
    }

    /// Show an untouched conversation as a chat or as a terminal.
    pub fn set_view(&mut self, view: crate::settings::AgentView, cx: &mut Context<Self>) {
        if !self.is_fresh() {
            return;
        }
        let terminal = view == crate::settings::AgentView::Terminal;
        if terminal != self.is_terminal() {
            self.terminal = terminal.then(TerminalMode::default);
            cx.notify();
        }
    }

    /// Start (or restart) the agent's TUI and type into it.
    pub fn start_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.launch_terminal(window, cx) {
            self.focus(window, cx);
        }
    }

    /// A terminal conversation with history on screen resumes its TUI on
    /// its own, once: the panel shows the session, not a Resume button. Not
    /// a Claude session another process has open (another SuperGIT, a
    /// terminal): two processes would write one transcript. A new one waits
    /// for its agent and view to be picked, then Start. The focus stays
    /// where it is.
    pub(super) fn auto_start_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending = self
            .terminal
            .as_ref()
            .is_some_and(|t| t.view.is_none() && !t.auto_started);
        if !pending || self.git_busy || !self.has_history() {
            return;
        }
        let session = self
            .convo
            .session_id
            .clone()
            .filter(|_| self.provider() == Provider::Claude && self.has_history());
        if let Some(terminal) = &mut self.terminal {
            terminal.auto_started = true;
        }
        cx.spawn_in(window, async move |this, cx| {
            let elsewhere = match session {
                Some(id) => {
                    cx.background_executor()
                        .spawn(async move { sg_agents::claude_session_process(&id) })
                        .await
                }
                None => None,
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(terminal) = &mut this.terminal else {
                    return;
                };
                terminal.elsewhere = elsewhere;
                // Started meanwhile (a click, New): leave it as it is.
                if elsewhere.is_none() && terminal.view.is_none() {
                    this.launch_terminal(window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Start (or restart) the agent's TUI; false while Git holds the
    /// worktree.
    fn launch_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.git_busy {
            return false;
        }
        let spawn = self.terminal_spawn(cx);
        let terminal = self.terminal.get_or_insert_with(TerminalMode::default);
        terminal.status = Status::Starting;
        terminal.ready = false;
        terminal.reports_status = false;
        if let Some(view) = terminal.view.clone() {
            view.update(cx, |view, cx| view.restart(spawn, cx));
        } else {
            let view = cx.new(|cx| TerminalView::new(spawn, window, cx));
            terminal._subscription = Some(cx.subscribe(&view, Self::on_terminal_event));
            terminal.view = Some(view);
        }
        self.attention.set(Kind::Running, None);
        cx.notify();
        true
    }

    /// What to run: a new session with an id we choose (so it can be
    /// resumed and recognized), or the existing one.
    fn terminal_spawn(&mut self, cx: &App) -> Spawn {
        let resume = self.convo.session_id.clone().filter(|_| self.has_history());
        let initial_prompt = crate::initial_prompt::for_worktree(&self.worktree, cx);
        let (program, args) = match self.provider() {
            Provider::Claude => {
                let mut args: Vec<String> = match resume {
                    Some(id) => vec!["--resume".into(), id],
                    None => {
                        let id = uuid::Uuid::new_v4().to_string();
                        self.convo.session_id = Some(id.clone());
                        vec!["--session-id".into(), id]
                    }
                };
                // Every start: a resumed session does not keep it.
                if let Some(prompt) = initial_prompt {
                    args.extend(["--append-system-prompt".into(), prompt]);
                }
                args.extend(crate::terminal::theme::claude_args(cx));
                ("claude".into(), args)
            }
            Provider::Codex => {
                let program = sg_codex::install::locate()
                    .map(|found| found.program)
                    .unwrap_or_else(|_| "codex".into());
                let args = match resume {
                    Some(id) => vec!["resume".into(), id],
                    // A new thread keeps its instructions. A config
                    // override; its value a TOML string (JSON's escapes
                    // are TOML's).
                    None => initial_prompt
                        .map(|prompt| {
                            vec![
                                "-c".into(),
                                format!(
                                    "developer_instructions={}",
                                    serde_json::Value::from(prompt)
                                ),
                            ]
                        })
                        .unwrap_or_default(),
                };
                (program, args)
            }
        };
        crate::terminal::spawn_in(
            program,
            args.into_iter().map(OsString::from).collect(),
            &self.cwd,
        )
    }

    /// The session has a transcript to resume from (`--resume` fails
    /// without one).
    fn has_history(&self) -> bool {
        match self.provider() {
            Provider::Claude => self.transcript_path().is_some_and(|path| path.is_file()),
            Provider::Codex => self.convo.session_id.is_some(),
        }
    }

    /// Where Claude Code keeps this session's transcript.
    fn transcript_path(&self) -> Option<std::path::PathBuf> {
        self.transcript.clone().or_else(|| {
            Some(
                sg_agents::project_dir(&self.cwd)?
                    .join(format!("{}.jsonl", self.convo.session_id.as_ref()?)),
            )
        })
    }

    /// The session's own name (`/rename`, else Claude's) read from its
    /// transcript now rather than at the next poll of the sessions.
    fn refresh_title(&mut self, cx: &mut Context<Self>) {
        if self.provider() != Provider::Claude {
            return;
        }
        let Some(path) = self.transcript_path() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let title = cx
                .background_executor()
                .spawn(async move { sg_agents::transcript_title(&path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(title) = title
                    && this.convo.title.as_deref() != Some(title.as_str())
                {
                    this.convo.title = Some(title);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn on_terminal_event(
        &mut self,
        view: Entity<TerminalView>,
        event: &TerminalEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        let mut renamed = false;
        match event {
            TerminalEvent::Status(state) => {
                terminal.reports_status = true;
                let (status, kind) = match state {
                    ProgramState::Working => (Status::Working, Some(Kind::Running)),
                    ProgramState::Blocked(BlockedOn::Permission) => {
                        (Status::NeedsPermission, Some(Kind::Approval))
                    }
                    ProgramState::Blocked(_) => (Status::NeedsInput, Some(Kind::Question)),
                    ProgramState::Done => (Status::Idle, Some(Kind::Completed)),
                    ProgramState::Error => (Status::Idle, Some(Kind::Failed)),
                    // At rest: keep an unseen "done" until it is seen.
                    ProgramState::Idle | ProgramState::Cleared => (
                        Status::Idle,
                        self.attention.kind.is_live().then_some(Kind::Idle),
                    ),
                };
                terminal.status = status;
                if let Some(kind) = kind {
                    self.attention.set(kind, None);
                }
                if matches!(state, ProgramState::Idle | ProgramState::Done) {
                    self.terminal_ready(cx);
                }
            }
            TerminalEvent::Output => {
                if !terminal.ready {
                    // Settled = no output for a moment after drawing.
                    terminal.settle_check = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(SETTLED).await;
                        let _ = this.update(cx, |this, cx| this.terminal_ready(cx));
                    }));
                } else if !terminal.reports_status
                    && view.read(cx).active_within(Duration::from_millis(500))
                {
                    terminal.status = Status::Working;
                    self.attention.set(Kind::Running, None);
                    terminal.quiet_check = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(QUIET).await;
                        let _ = this.update(cx, |this, cx| this.check_quiet(cx));
                    }));
                }
            }
            TerminalEvent::Bell => {
                if !terminal.reports_status {
                    terminal.status = Status::NeedsInput;
                    self.attention.set(Kind::Question, None);
                }
            }
            TerminalEvent::TitleChanged(title) => {
                let title = clean_title(title);
                // Another title on screen may be another name (`/rename`).
                renamed = title != terminal.title;
                terminal.title = title;
            }
            TerminalEvent::Exited(code) => {
                terminal.status = Status::Exited;
                terminal.quiet_check = None;
                let failed = code.is_some_and(|code| code != 0);
                self.attention
                    .set(if failed { Kind::Failed } else { Kind::Idle }, None);
            }
        }
        if renamed {
            self.refresh_title(cx);
        }
        cx.notify();
    }

    fn check_quiet(&mut self, cx: &mut Context<Self>) {
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        let active = terminal
            .view
            .as_ref()
            .is_some_and(|view| view.read(cx).active_within(QUIET));
        if !active && terminal.status == Status::Working {
            terminal.status = Status::Idle;
            self.attention.set(Kind::Completed, None);
            cx.notify();
        }
    }

    /// The program can take input: type what was queued for it.
    fn terminal_ready(&mut self, cx: &mut Context<Self>) {
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        terminal.settle_check = None;
        if terminal.ready {
            return;
        }
        terminal.ready = true;
        if terminal.status == Status::Starting {
            terminal.status = Status::Idle;
            if self.attention.kind == Kind::Running {
                self.attention.set(Kind::Idle, None);
            }
        }
        let queued = std::mem::take(&mut terminal.queued);
        #[cfg(feature = "screenshot")]
        if let Ok(debug) = std::env::var("SUPERGIT_DEBUG_TERMINAL")
            && let Some(view) = terminal.view.clone()
        {
            // `select`: drag a selection; `menu`: and right-click it.
            view.update(cx, |view, cx| view.debug_select(cx));
            if debug == "menu"
                && let Some(position) = view.read(cx).debug_menu_position()
                && let Some(window) = cx.windows().first().copied()
            {
                // A real click: press, then release a moment later (the
                // menu must stay open).
                cx.spawn(async move |_, cx| {
                    let event = |down: bool| {
                        if down {
                            PlatformInput::MouseDown(MouseDownEvent {
                                button: MouseButton::Right,
                                position,
                                modifiers: Modifiers::default(),
                                click_count: 1,
                                first_mouse: false,
                            })
                        } else {
                            PlatformInput::MouseUp(MouseUpEvent {
                                button: MouseButton::Right,
                                position,
                                modifiers: Modifiers::default(),
                                click_count: 1,
                            })
                        }
                    };
                    for down in [true, false] {
                        let _ = window.update(cx, |_, window, cx| {
                            window.dispatch_event(event(down), cx);
                        });
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(150))
                            .await;
                    }
                })
                .detach();
            }
        }
        if let Some(view) = terminal.view.clone() {
            view.update(cx, |view, _| {
                for (text, submit) in queued {
                    if submit {
                        view.submit_text(&text);
                    } else {
                        view.paste_text(&text);
                    }
                }
            });
        }
        cx.notify();
    }

    /// Put text into the agent's prompt (pasted; with `submit`, then Enter),
    /// now or once it is ready.
    pub fn type_into_terminal(&mut self, text: String, submit: bool, cx: &mut Context<Self>) {
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        match terminal.view.clone() {
            Some(view) if terminal.ready => view.update(cx, |view, _| {
                if submit {
                    view.submit_text(&text);
                } else {
                    view.paste_text(&text);
                }
            }),
            _ => terminal.queued.push((text, submit)),
        }
    }

    pub(super) fn terminal_status(&self) -> Option<Status> {
        self.terminal.as_ref().map(|t| t.status)
    }

    pub(super) fn terminal_title(&self) -> Option<&str> {
        self.terminal
            .as_ref()
            .map(|t| t.title.as_str())
            .filter(|t| !t.is_empty() && !is_program_name(t))
    }

    /// A new conversation's choices, each a segmented control under its
    /// label: the agent, and the view it opens in (`Chat | Terminal`, the
    /// same setting as in Settings: New and Resume use it from then on).
    pub(super) fn render_new_choices(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::AgentView;
        let muted = cx.theme().muted_foreground;
        let provider = self.provider();
        let view = if self.is_terminal() {
            AgentView::Terminal
        } else {
            AgentView::Chat
        };
        let choice = |label: &'static str, group: ButtonGroup| {
            v_flex()
                .items_center()
                .gap_1p5()
                .child(div().text_xs().text_color(muted).child(label))
                .child(group)
        };
        let agents = ButtonGroup::new("new-agent")
            .outline()
            .children(Provider::ALL.map(|agent| {
                Button::new(("new-agent", agent as usize))
                    .icon(crate::icons::agent_icon(agent))
                    .label(agent.label())
                    .w(px(NEW_SESSION_WIDTH / 2.))
                    .selected(agent == provider)
                    .tooltip(match agent {
                        Provider::Claude => "Use Anthropic's Claude Code in this conversation",
                        Provider::Codex => "Use OpenAI's Codex in this conversation",
                    })
            }))
            .on_click(cx.listener(|this, clicked: &Vec<usize>, window, cx| {
                if let Some(&agent) = clicked.first().and_then(|&ix| Provider::ALL.get(ix)) {
                    this.set_provider(agent, window, cx);
                    this.focus(window, cx);
                }
            }));
        let views = ButtonGroup::new("new-view")
            .outline()
            .children([AgentView::Chat, AgentView::Terminal].map(|choice| {
                Button::new(("new-view", choice as usize))
                    .label(choice.label())
                    .w(px(NEW_SESSION_WIDTH / 2.))
                    .selected(choice == view)
                    .tooltip(match choice {
                        AgentView::Chat => "Talk in SuperGIT's chat",
                        AgentView::Terminal => "Use the agent's own terminal UI",
                    })
            }))
            .on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
                let choice = if clicked.first() == Some(&1) {
                    AgentView::Terminal
                } else {
                    AgentView::Chat
                };
                this.set_view(choice, cx);
                cx.emit(super::ChatEvent::PickedView(choice));
            }));
        v_flex()
            .items_center()
            .gap_3()
            .child(choice("Agent", agents))
            .child(choice("View", views))
    }

    /// Start (or Resume): the one thing to do on a new conversation's
    /// screen, in either view, as wide as the choices above it.
    pub(super) fn render_start(&self, label: &'static str, cx: &mut Context<Self>) -> Button {
        let tooltip = if self.git_busy {
            "Wait for the Git operation on this worktree to finish"
        } else if !self.is_terminal() {
            "Open the chat to type the first message"
        } else if self.convo.session_id.is_some() && self.has_history() {
            "Reopen this session in the agent's terminal UI"
        } else {
            "Launch the agent's terminal UI in this worktree"
        };
        Button::new("start")
            .label(label)
            .large()
            .w(px(NEW_SESSION_WIDTH))
            .primary()
            .disabled(self.git_busy)
            .tooltip(tooltip)
            .on_click(cx.listener(|this, _, window, cx| this.start(window, cx)))
    }

    /// A terminal conversation's title from its session as the Agents tab
    /// reads it: SuperGIT never sees what is typed into the TUI, whose
    /// window title stays "Claude Code" until Claude names the session.
    /// Claude's name wins; until then the first prompt seen stays.
    pub fn adopt_session_title(&mut self, sessions: &[sg_agents::Session], cx: &mut Context<Self>) {
        if !self.is_terminal() {
            return;
        }
        let Some(id) = self.convo.session_id.as_deref() else {
            return;
        };
        let Some(session) = sessions.iter().find(|s| s.is(self.provider(), id)) else {
            return;
        };
        // The session's title is Claude's name, else the last prompt, else
        // the id.
        let named =
            session.title != session.id && session.last_prompt.as_ref() != Some(&session.title);
        let title = if named {
            session.title.clone()
        } else if self.convo.title.is_none()
            && let Some(prompt) = &session.last_prompt
        {
            sg_agents::chat::short_title(prompt)
        } else {
            return;
        };
        if self.convo.title.as_deref() != Some(title.as_str()) {
            self.convo.title = Some(title);
            cx.notify();
        }
    }

    pub(super) fn focus_terminal(&self, window: &mut Window, cx: &mut App) -> bool {
        match self.terminal.as_ref().and_then(|t| t.view.as_ref()) {
            Some(view) => {
                view.read(cx).focus_handle().focus(window, cx);
                true
            }
            None => false,
        }
    }

    /// Before a Git operation that removes the worktree: an agent that is
    /// working refuses, an idle one is hung up.
    pub(super) fn release_terminal(&mut self, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let Some(terminal) = &mut self.terminal else {
            return Ok(());
        };
        anyhow::ensure!(
            !terminal.status.is_busy(),
            "An agent is still running in a terminal tab here"
        );
        if let Some(view) = &terminal.view {
            view.update(cx, |view, _| view.hang_up());
        }
        Ok(())
    }

    pub(super) fn render_terminal(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let provider = self.provider();
        let Some(terminal) = &self.terminal else {
            return div().into_any_element();
        };
        let Some(view) = terminal.view.clone() else {
            // Not started: new (the agent and the view still to pick), Git
            // holds the worktree, or the session is open in another process.
            let fresh = self.is_fresh();
            let resume = self.convo.session_id.is_some() && self.has_history();
            // The panel's header names the agent: here only the choices,
            // then the one thing to do, a button as wide as they are.
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_8()
                .px_6()
                .text_color(theme.muted_foreground)
                .when(fresh, |el| el.child(self.render_new_choices(cx)))
                .child(
                    v_flex()
                        .items_center()
                        .gap_2()
                        .child(self.render_start(if resume { "Resume" } else { "Start" }, cx))
                        .when_some(terminal.elsewhere, |el, pid| {
                            el.child(div().max_w(px(320.)).text_xs().text_center().child(format!(
                                "Open in another Claude Code process (pid {pid}): \
                                     resuming here too would have two processes write \
                                     this session"
                            )))
                        }),
                )
                .into_any_element();
        };
        let exited = view.read(cx).exit_code();
        v_flex()
            .size_full()
            .child(div().flex_1().min_h_0().child(view))
            .when_some(exited, |el, code| {
                let label = match code {
                    Some(0) => format!("{} exited", provider.label()),
                    Some(code) => format!("{} exited with code {code}", provider.label()),
                    None => format!("{} stopped", provider.label()),
                };
                el.child(
                    h_flex()
                        .flex_none()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .border_t_1()
                        .border_color(theme.border)
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(div().flex_1().child(label))
                        .child(
                            Button::new("terminal-restart")
                                .label(if self.has_history() {
                                    "Resume"
                                } else {
                                    "Start again"
                                })
                                .xsmall()
                                .disabled(self.git_busy)
                                .tooltip(if self.git_busy {
                                    "Wait for the Git operation on this worktree to finish"
                                } else if self.has_history() {
                                    "Reopen this session in the agent's terminal UI"
                                } else {
                                    "Launch the agent's terminal UI again, in a new session"
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.start_terminal(window, cx);
                                })),
                        ),
                )
            })
            .into_any_element()
    }
}

/// The program's window title as a tab title: without the spinner or
/// status glyph TUIs put in front (`✳ Fix login`, `⠂ Thinking`).
fn clean_title(title: &str) -> String {
    title
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim()
        .to_string()
}

/// The width of a new conversation's controls: the choices (two segments
/// each) and Start line up.
pub(super) const NEW_SESSION_WIDTH: f32 = 224.;

/// A TUI's own name as its window title ("Claude Code" until Claude names
/// the session): it says nothing about the conversation.
pub(super) fn is_program_name(title: &str) -> bool {
    ["Claude Code", "Claude", "Codex"]
        .iter()
        .any(|name| title.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::{clean_title, is_program_name};

    #[test]
    fn program_names_are_not_titles() {
        assert!(is_program_name(&clean_title("⠂ Claude Code")));
        assert!(is_program_name("codex"));
        assert!(!is_program_name("Fix the login bug"));
    }

    #[test]
    fn titles_lose_their_status_glyphs() {
        assert_eq!(clean_title("✳ Fix the login bug"), "Fix the login bug");
        assert_eq!(clean_title("⠂ Claude Code"), "Claude Code");
        assert_eq!(clean_title("Plain"), "Plain");
        assert_eq!(clean_title("✳ "), "");
    }
}
