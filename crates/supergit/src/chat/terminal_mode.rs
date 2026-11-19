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

use gpui_kit::base::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
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

/// Variables that describe *our* surroundings, not the agent's: a terminal
/// SuperGIT was started from, or a Claude Code session it runs inside
/// (developing SuperGIT with Claude). Inherited, they mislead the TUI.
const FOREIGN_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_CHILD_NOTES_DIR",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "TMUX",
    "TMUX_PANE",
    "STY",
    "ITERM_SESSION_ID",
    "ITERM_PROFILE",
    "KITTY_WINDOW_ID",
    "KITTY_PID",
    "WEZTERM_PANE",
    "WEZTERM_UNIX_SOCKET",
    "GHOSTTY_RESOURCES_DIR",
    "GHOSTTY_BIN_DIR",
    "GHOSTTY_SHELL_FEATURES",
    "ALACRITTY_WINDOW_ID",
    "ALACRITTY_SOCKET",
    "KONSOLE_VERSION",
    "VTE_VERSION",
    "WT_SESSION",
    "TERMINAL_EMULATOR",
    "VSCODE_INJECTION",
];

/// Terminal-mode state of a conversation.
#[derive(Default)]
pub(super) struct TerminalMode {
    /// `None` until started (a restored conversation waits for the user).
    pub(super) view: Option<Entity<TerminalView>>,
    pub(super) status: Status,
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

    /// Start (or restart) the agent's TUI.
    pub fn start_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.git_busy {
            return;
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
        self.focus(window, cx);
        cx.notify();
    }

    /// What to run: a new session with an id we choose (so it can be
    /// resumed and recognized), or the existing one.
    fn terminal_spawn(&mut self, cx: &App) -> Spawn {
        let resume = self.convo.session_id.clone().filter(|_| self.has_history());
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
                args.extend(crate::terminal::theme::claude_args(cx));
                ("claude".into(), args)
            }
            Provider::Codex => {
                let program = sg_codex::install::locate()
                    .map(|found| found.program)
                    .unwrap_or_else(|_| "codex".into());
                let args = resume.map_or_else(Vec::new, |id| vec!["resume".into(), id]);
                (program, args)
            }
        };
        let env = [
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("TERM_PROGRAM", "SuperGIT"),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .chain([(OsString::from("PWD"), self.cwd.clone().into_os_string())])
        .collect();
        Spawn {
            program,
            args: args.into_iter().map(OsString::from).collect(),
            cwd: self.cwd.clone(),
            env,
            env_remove: FOREIGN_ENV.iter().map(OsString::from).collect(),
            size: Default::default(),
        }
    }

    /// The session has a transcript to resume from (`--resume` fails
    /// without one).
    fn has_history(&self) -> bool {
        match self.provider() {
            Provider::Claude => self
                .transcript
                .clone()
                .or_else(|| {
                    Some(
                        sg_agents::project_dir(&self.cwd)?
                            .join(format!("{}.jsonl", self.convo.session_id.as_ref()?)),
                    )
                })
                .is_some_and(|path| path.is_file()),
            Provider::Codex => self.convo.session_id.is_some(),
        }
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
                terminal.title = clean_title(title);
            }
            TerminalEvent::Exited(code) => {
                terminal.status = Status::Exited;
                terminal.quiet_check = None;
                let failed = code.is_some_and(|code| code != 0);
                self.attention
                    .set(if failed { Kind::Failed } else { Kind::Idle }, None);
            }
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
            .filter(|t| !t.is_empty())
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
            // Not started: restored, or a fresh conversation waiting for a
            // click (selecting a worktree never starts an agent).
            let fresh = self.is_fresh();
            let resume = self.convo.session_id.is_some() && self.has_history();
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .px_6()
                .text_color(theme.muted_foreground)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_lg()
                        .child(crate::icons::agent_icon(provider).size_5())
                        .child(format!("{} in a terminal", provider.label())),
                )
                .when(fresh, |el| {
                    el.child(
                        h_flex()
                            .gap_1()
                            .children(Provider::ALL.into_iter().map(|choice| {
                                let active = choice == provider;
                                Button::new(("terminal-agent", choice as usize))
                                    .icon(crate::icons::agent_icon(choice))
                                    .label(choice.label())
                                    .small()
                                    .when(active, |b| b.secondary())
                                    .when(!active, |b| b.ghost())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.set_provider(choice, window, cx);
                                    }))
                            })),
                    )
                })
                .child(
                    Button::new("terminal-start")
                        .label(if resume { "Resume" } else { "Start" })
                        .small()
                        .primary()
                        .disabled(self.git_busy)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.start_terminal(window, cx);
                        })),
                )
                .child(
                    div()
                        .text_sm()
                        .text_center()
                        .child(format!("Working in {}", self.cwd.display())),
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

#[cfg(test)]
mod tests {
    use super::clean_title;

    #[test]
    fn titles_lose_their_status_glyphs() {
        assert_eq!(clean_title("✳ Fix the login bug"), "Fix the login bug");
        assert_eq!(clean_title("⠂ Claude Code"), "Claude Code");
        assert_eq!(clean_title("Plain"), "Plain");
        assert_eq!(clean_title("✳ "), "");
    }
}
