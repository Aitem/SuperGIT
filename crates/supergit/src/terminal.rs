//! The embedded terminal: an agent's own TUI (`claude`, `codex`) or the
//! user's shell (the terminal panel, `cmd-j`) on a PTY, emulated by
//! Ghostty's libghostty-vt (`sg_terminal`) and drawn here with GPUI, in the
//! app's colors (`theme`).
//!
//! The terminal lives on the main thread (libghostty handles are `!Send`);
//! a reader thread only moves bytes. Output is fed in batches with one
//! repaint per batch, and while a program draws a synchronized frame (mode
//! 2026) the previous frame stays on screen.

pub mod theme;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sg_terminal::{
    CursorShape, KeyInput, ProgramState, Pty, PtyEvent, Rgb, Screen, Size, Spawn, Terminal,
    UnderlineStyle as TermUnderline,
};

actions!(terminal, [Copy, Paste, SelectAll]);

/// The key context terminal views set: `main.rs` binds `Copy`/`Paste` in
/// it, and gives it the keys other bindings would take (Tab, and Linux's
/// `ctrl-` app shortcuts).
pub const KEY_CONTEXT: &str = "Terminal";

/// Space around the grid.
const PADDING: f32 = 6.;
/// How often a selection drag past an edge scrolls by a line.
const AUTOSCROLL_EVERY: Duration = Duration::from_millis(60);
/// The longest a program may hold a synchronized frame back.
const HOLD_TIMEOUT: Duration = Duration::from_secs(1);

/// Variables that describe *our* surroundings, not the program's: a
/// terminal SuperGIT was started from, or a Claude Code session it runs
/// inside (developing SuperGIT with Claude). Inherited, they mislead a TUI
/// (an agent, or one started from a shell here).
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

/// `program args` in `cwd` on our PTY, told it runs in SuperGIT's terminal
/// (`TERM`, true color) and without the variables of SuperGIT's own
/// surroundings.
pub fn spawn_in(program: PathBuf, args: Vec<OsString>, cwd: &Path) -> Spawn {
    let env = [
        ("TERM", "xterm-256color"),
        ("COLORTERM", "truecolor"),
        ("TERM_PROGRAM", "SuperGIT"),
    ]
    .into_iter()
    .map(|(k, v)| (OsString::from(k), OsString::from(v)))
    .chain([(OsString::from("PWD"), cwd.as_os_str().to_owned())])
    .collect();
    Spawn {
        program,
        args,
        cwd: cwd.to_path_buf(),
        env,
        env_remove: FOREIGN_ENV.iter().map(OsString::from).collect(),
        size: Size::default(),
    }
}

/// The user's interactive shell (`$SHELL`) in `cwd`; a login shell on
/// macOS, as Terminal.app and VS Code start it.
pub fn shell_in(cwd: &Path) -> Spawn {
    let shell = std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let fallback = if cfg!(target_os = "macos") {
                "/bin/zsh"
            } else {
                "/bin/bash"
            };
            if Path::new(fallback).exists() {
                fallback.into()
            } else {
                "/bin/sh".into()
            }
        });
    let args = if cfg!(target_os = "macos") {
        vec![OsString::from("-l")]
    } else {
        Vec::new()
    };
    spawn_in(shell, args, cwd)
}

/// The font of the user's Ghostty (`font-family`, `font-size`): the
/// embedded terminal looks like their own. Until it is looked up, and
/// without one installed, the theme's monospace font.
#[derive(Clone, Default)]
pub struct TerminalFont {
    family: Option<SharedString>,
    size: Option<Pixels>,
}

impl Global for TerminalFont {}

/// Look up the terminal font off the main thread (config files, and the
/// installed families: Ghostty's list may name fonts this machine lacks).
pub fn load_font(cx: &mut App) {
    cx.set_global(TerminalFont::default());
    let text_system = cx.text_system().clone();
    cx.spawn(async move |cx| {
        let font = cx
            .background_executor()
            .spawn(async move {
                let config = sg_terminal::config::font();
                let family = if config.families.is_empty() {
                    None
                } else {
                    let installed = text_system.all_font_names();
                    config
                        .families
                        .into_iter()
                        .find(|family| installed.iter().any(|name| name == family))
                };
                TerminalFont {
                    family: family.map(SharedString::from),
                    size: config.size.map(px),
                }
            })
            .await;
        cx.update(|cx| cx.set_global(font));
    })
    .detach();
}

pub enum TerminalEvent {
    /// The program wrote something.
    Output,
    TitleChanged(String),
    Bell,
    /// The program's own status report (Claude Code sends them). Its
    /// desktop notifications (OSC 9/777) are not forwarded: the app notifies
    /// from the status already.
    Status(ProgramState),
    Exited(Option<i32>),
}

/// Font and cell size, measured when laying out.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Metrics {
    font_size: Pixels,
    cell_width: Pixels,
    line_height: Pixels,
}

pub struct TerminalView {
    terminal: Terminal,
    /// Started at the first layout, when the size is known.
    pending: Option<Spawn>,
    pty: Option<Pty>,
    reader: Option<Task<()>>,
    focus: FocusHandle,
    size: Size,
    /// The last frame drawn (kept while a program holds its output).
    screen: Screen,
    held_since: Option<Instant>,
    hold_timer: Option<Task<()>>,
    exited: Option<Option<i32>>,
    error: Option<String>,
    title: String,
    /// Wheel movement not yet worth a whole line.
    scroll_remainder: Pixels,
    last_output: Option<Instant>,
    last_input: Option<Instant>,
    dark: bool,
    /// Where the grid was last laid out: the view's origin in the window and
    /// its cell geometry, for turning mouse positions into cells.
    layout: Option<(Point<Pixels>, sg_terminal::term::Surface)>,
    /// A selection drag is in progress (left button held since a press here).
    selecting: bool,
    /// The last drag position in view pixels (autoscroll extends to it).
    drag_at: (f32, f32),
    autoscroll: Option<(sg_terminal::term::SelectScroll, Task<()>)>,
    /// A button pressed while the program tracks the mouse: its motion and
    /// release go to the program too.
    reporting: Option<sg_terminal::MouseButton>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl TerminalView {
    /// A terminal that runs `spawn` once it is first laid out.
    pub fn new(spawn: Spawn, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let size = Size {
            cols: 80,
            rows: 24,
            cell_width: 8,
            cell_height: 16,
        };
        let colors = theme::terminal_colors(cx);
        let error;
        let terminal = match Terminal::new(size, &colors) {
            Ok(terminal) => {
                error = None;
                terminal
            }
            Err(e) => {
                error = Some(format!("Cannot start the terminal: {e:#}"));
                // A second attempt with defaults cannot fail differently, but
                // keep the view constructible.
                Terminal::new(size, &sg_terminal::Colors::default())
                    .expect("a default terminal always initializes")
            }
        };
        let focus = cx.focus_handle();
        let subscriptions = vec![
            cx.on_focus(&focus, window, |this, _, cx| this.focus_changed(true, cx)),
            cx.on_blur(&focus, window, |this, _, cx| this.focus_changed(false, cx)),
            cx.observe_global::<gpui_kit::component::Theme>(|this, cx| this.theme_changed(cx)),
            cx.observe_global::<TerminalFont>(|_, cx| cx.notify()),
        ];
        Self {
            terminal,
            pending: Some(spawn),
            pty: None,
            reader: None,
            focus,
            size,
            screen: Screen::default(),
            held_since: None,
            hold_timer: None,
            exited: None,
            error,
            title: String::new(),
            scroll_remainder: Pixels::ZERO,
            last_output: None,
            last_input: None,
            dark: colors.dark,
            layout: None,
            selecting: false,
            drag_at: (0., 0.),
            autoscroll: None,
            reporting: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn exit_code(&self) -> Option<Option<i32>> {
        self.exited
    }

    /// Why the program could not start (or the terminal could not).
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Output within `window`, not just the echo of a key just typed.
    pub fn active_within(&self, window: Duration) -> bool {
        let Some(output) = self.last_output else {
            return false;
        };
        let echo = self
            .last_input
            .is_some_and(|input| output.duration_since(input) < Duration::from_millis(300));
        !echo && output.elapsed() < window
    }

    /// Send text as if pasted (bracketed when the program asked).
    pub fn paste_text(&mut self, text: &str) {
        if let Ok(bytes) = self.terminal.encode_paste(text) {
            self.write(bytes);
        }
    }

    /// Type text and press Enter.
    pub fn submit_text(&mut self, text: &str) {
        self.paste_text(text);
        self.write(b"\r".to_vec());
    }

    /// Hang up the program (as closing a terminal window does).
    pub fn hang_up(&mut self) {
        if let Some(pty) = &self.pty {
            pty.hang_up();
        }
    }

    fn start(&mut self, spawn: Spawn, cx: &mut Context<Self>) {
        match Pty::spawn(&Spawn {
            size: self.size,
            ..spawn
        }) {
            Ok((pty, mut events)) => {
                self.pty = Some(pty);
                self.reader = Some(cx.spawn(async move |this, cx| {
                    while let Some(first) = events.next().await {
                        // Everything already waiting, in one repaint.
                        let mut batch = vec![first];
                        while let Ok(event) = events.try_recv() {
                            batch.push(event);
                        }
                        if this.update(cx, |this, cx| this.on_pty(batch, cx)).is_err() {
                            break;
                        }
                    }
                }));
            }
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                self.exited = Some(None);
                cx.emit(TerminalEvent::Exited(None));
            }
        }
        cx.notify();
    }

    fn on_pty(&mut self, batch: Vec<PtyEvent>, cx: &mut Context<Self>) {
        let mut wrote = false;
        for event in batch {
            match event {
                PtyEvent::Output(bytes) => {
                    wrote = true;
                    let effects = self.terminal.feed(&bytes);
                    self.write(effects.reply);
                    if effects.title_changed {
                        self.title = self.terminal.title();
                        cx.emit(TerminalEvent::TitleChanged(self.title.clone()));
                    }
                    if effects.bell {
                        cx.emit(TerminalEvent::Bell);
                    }
                    if let Some(text) = effects.clipboard {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    if let Some(state) = effects.status {
                        cx.emit(TerminalEvent::Status(state));
                    }
                }
                PtyEvent::Exited(code) => {
                    self.exited = Some(code);
                    self.pty = None;
                    cx.emit(TerminalEvent::Exited(code));
                }
            }
        }
        if wrote {
            self.last_output = Some(Instant::now());
            cx.emit(TerminalEvent::Output);
        }
        self.schedule_hold_timeout(cx);
        cx.notify();
    }

    /// A program that never ends its synchronized frame must not freeze the
    /// view: after `HOLD_TIMEOUT` the hold is lifted.
    fn schedule_hold_timeout(&mut self, cx: &mut Context<Self>) {
        if !self.terminal.render_held() {
            self.held_since = None;
            self.hold_timer = None;
            return;
        }
        if self.held_since.is_none() {
            self.held_since = Some(Instant::now());
            self.hold_timer = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(HOLD_TIMEOUT).await;
                let _ = this.update(cx, |this, cx| {
                    if this.terminal.render_held() {
                        this.terminal.end_render_hold();
                    }
                    this.held_since = None;
                    cx.notify();
                });
            }));
        }
    }

    fn write(&mut self, bytes: Vec<u8>) {
        if let Some(pty) = &self.pty {
            pty.write(bytes);
        }
    }

    fn focus_changed(&mut self, focused: bool, cx: &mut Context<Self>) {
        let bytes = self.terminal.encode_focus(focused);
        self.write(bytes);
        cx.notify();
    }

    fn theme_changed(&mut self, cx: &mut Context<Self>) {
        let colors = theme::terminal_colors(cx);
        self.dark = colors.dark;
        if self.terminal.set_colors(&colors).is_ok() {
            let reply = self.terminal.take_reply();
            self.write(reply);
        }
        theme::refresh_claude_theme(cx);
        cx.notify();
    }

    /// Layout found room for `size`: resize the grid and the program.
    fn set_size(&mut self, size: Size, cx: &mut Context<Self>) {
        if size == self.size && self.pending.is_none() {
            return;
        }
        self.size = size;
        let _ = self.terminal.resize(size);
        if let Some(pty) = &self.pty {
            let _ = pty.resize(size);
        }
        if let Some(spawn) = self.pending.take() {
            self.start(spawn, cx);
        }
    }

    /// The frame to draw: the live screen, or the last one while a
    /// program holds its output.
    fn frame(&mut self) -> Screen {
        if !self.terminal.render_held()
            && let Ok(screen) = self.terminal.screen()
        {
            self.screen = screen;
        }
        self.screen.clone()
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let m = keystroke.modifiers;
        // Command shortcuts belong to the app (Copy/Paste are actions).
        if m.platform {
            return;
        }
        if self.pty.is_none() {
            return;
        }
        let input = KeyInput {
            key: keystroke.key.clone(),
            text: keystroke.key_char.clone(),
            shift: m.shift,
            ctrl: m.control,
            alt: m.alt,
            cmd: false,
            repeat: event.is_held,
            option_composes: cfg!(target_os = "macos"),
        };
        match self.terminal.encode_key(&input) {
            Ok(bytes) if !bytes.is_empty() => {
                // As in Ghostty: typing drops the selection.
                self.terminal.clear_selection();
                self.terminal.scroll_to_bottom();
                self.last_input = Some(Instant::now());
                self.write(bytes);
                cx.stop_propagation();
                cx.notify();
            }
            _ => {}
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.terminal.clear_selection();
            self.terminal.scroll_to_bottom();
            self.last_input = Some(Instant::now());
            self.paste_text(&text);
            cx.notify();
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Ok(Some(text)) = self.terminal.selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        let _ = self.terminal.select_all();
        cx.notify();
    }

    /// A window position in the view's pixels, with the grid's geometry.
    fn local(&self, position: Point<Pixels>) -> Option<(f32, f32, sg_terminal::term::Surface)> {
        let (origin, surface) = self.layout?;
        let local = position - origin;
        Some((f32::from(local.x), f32::from(local.y), surface))
    }

    /// The program gets mouse events while it tracks the mouse; Shift
    /// selects text anyway (as in other terminals).
    fn reports_mouse(&self, modifiers: Modifiers) -> bool {
        self.pty.is_some() && self.terminal.tracks_mouse() && !modifiers.shift
    }

    fn report_mouse(
        &mut self,
        action: sg_terminal::MouseAction,
        button: Option<sg_terminal::MouseButton>,
        position: Point<Pixels>,
        modifiers: Modifiers,
    ) {
        let Some((x, y, surface)) = self.local(position) else {
            return;
        };
        let input = sg_terminal::MouseInput {
            action,
            button,
            x,
            y,
            shift: modifiers.shift,
            ctrl: modifiers.control,
            alt: modifiers.alt,
            any_pressed: self.reporting.is_some(),
        };
        if let Ok(bytes) = self.terminal.encode_mouse(&input, &surface) {
            self.write(bytes);
        }
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        if self.reports_mouse(event.modifiers) {
            let Some(button) = term_button(event.button) else {
                return;
            };
            self.reporting = Some(button);
            self.report_mouse(
                sg_terminal::MouseAction::Press,
                Some(button),
                event.position,
                event.modifiers,
            );
            cx.stop_propagation();
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        let Some((x, y, surface)) = self.local(event.position) else {
            return;
        };
        if self.terminal.select_press(x, y, &surface).is_ok() {
            self.selecting = true;
            self.drag_at = (x, y);
        }
        cx.notify();
    }

    fn mouse_drag(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.selecting {
            // Programs that track the mouse: drags with a held button
            // anywhere, plain movement only over the terminal (the encoder
            // drops what the program's mode does not ask for).
            if self.pty.is_some() && self.terminal.tracks_mouse() {
                let over = self.local(event.position).is_some_and(|(x, y, surface)| {
                    x >= 0. && y >= 0. && x < surface.width && y < surface.height
                });
                if self.reporting.is_some() || over {
                    self.report_mouse(
                        sg_terminal::MouseAction::Motion,
                        self.reporting,
                        event.position,
                        event.modifiers,
                    );
                }
            }
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            // The button came up outside the window.
            self.end_selection(event.position, cx);
            return;
        }
        let Some((x, y, surface)) = self.local(event.position) else {
            return;
        };
        self.drag_at = (x, y);
        let m = event.modifiers;
        // Block selection: Option on macOS, Ctrl/Super+Alt elsewhere (as in
        // Ghostty).
        let rectangle = if cfg!(target_os = "macos") {
            m.alt
        } else {
            m.alt && (m.control || m.platform)
        };
        match self.terminal.select_drag(x, y, rectangle, &surface) {
            Ok(Some(direction)) => self.start_autoscroll(direction, cx),
            _ => self.autoscroll = None,
        }
        cx.notify();
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.selecting && event.button == MouseButton::Left {
            self.end_selection(event.position, cx);
        }
        if let Some(button) = self.reporting
            && term_button(event.button) == Some(button)
        {
            self.reporting = None;
            self.report_mouse(
                sg_terminal::MouseAction::Release,
                Some(button),
                event.position,
                event.modifiers,
            );
        }
    }

    fn end_selection(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.selecting = false;
        self.autoscroll = None;
        if let Some((x, y, surface)) = self.local(position) {
            let _ = self.terminal.select_release(x, y, &surface);
        }
        cx.notify();
    }

    /// Dragging past the top or bottom scrolls a line at a time, extending
    /// the selection, until the pointer comes back or the button is up.
    fn start_autoscroll(
        &mut self,
        direction: sg_terminal::term::SelectScroll,
        cx: &mut Context<Self>,
    ) {
        if self
            .autoscroll
            .as_ref()
            .is_some_and(|(running, _)| *running == direction)
        {
            return;
        }
        let task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(AUTOSCROLL_EVERY).await;
                let going = this.update(cx, |this, cx| {
                    let Some((_, surface)) = this.layout else {
                        return false;
                    };
                    if !this.selecting {
                        return false;
                    }
                    let (x, y) = this.drag_at;
                    let _ = this.terminal.select_autoscroll(direction, x, y, &surface);
                    cx.notify();
                    true
                });
                if !matches!(going, Ok(true)) {
                    break;
                }
            }
        });
        self.autoscroll = Some((direction, task));
    }

    fn scroll_wheel(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let line_height = px(self.size.cell_height.max(1) as f32);
        self.scroll_remainder += event.delta.pixel_delta(line_height).y;
        let lines = (self.scroll_remainder / line_height).trunc();
        if lines == 0. {
            return;
        }
        self.scroll_remainder -= line_height * lines;
        if self.reports_mouse(event.modifiers) {
            // One wheel "click" per line, up or down.
            let button = if lines > 0. {
                sg_terminal::MouseButton::WheelUp
            } else {
                sg_terminal::MouseButton::WheelDown
            };
            for _ in 0..(lines.abs() as usize) {
                self.report_mouse(
                    sg_terminal::MouseAction::Press,
                    Some(button),
                    event.position,
                    event.modifiers,
                );
            }
            cx.stop_propagation();
            return;
        }
        // Content moving down (positive delta) shows earlier lines.
        let bytes = self.terminal.scroll(-(lines as isize));
        self.write(bytes);
        cx.stop_propagation();
        cx.notify();
    }

    /// Screenshot builds: `SUPERGIT_DEBUG_TERMINAL=select` drags a selection
    /// over the first rows, as the mouse would (press, drag, release).
    #[cfg(feature = "screenshot")]
    pub fn debug_select(&mut self, cx: &mut Context<Self>) {
        let Some((_, surface)) = self.layout else {
            return;
        };
        let at = |col: f32, row: f32| {
            (
                surface.padding + col * surface.cell_width,
                surface.padding + (row + 0.5) * surface.cell_height,
            )
        };
        let (x, y) = at(0.2, 0.);
        let (to_x, to_y) = at(24.8, 2.);
        let _ = self.terminal.select_press(x, y, &surface);
        let _ = self.terminal.select_drag(to_x, to_y, false, &surface);
        let _ = self.terminal.select_release(to_x, to_y, &surface);
        cx.notify();
    }

    /// Screenshot builds: `SUPERGIT_DEBUG_TERMINAL=menu` right-clicks the
    /// selection; this is where, in window coordinates.
    #[cfg(feature = "screenshot")]
    pub fn debug_menu_position(&self) -> Option<Point<Pixels>> {
        let (origin, surface) = self.layout?;
        Some(
            origin
                + point(
                    px(surface.padding + 10. * surface.cell_width),
                    px(surface.padding + 1.5 * surface.cell_height),
                ),
        )
    }

    /// Run a program again in this terminal (after the last one exited).
    pub fn restart(&mut self, spawn: Spawn, cx: &mut Context<Self>) {
        self.exited = None;
        self.error = None;
        self.pty = None;
        self.reader = None;
        self.start(spawn, cx);
    }
}

/// GPUI's mouse button as the terminal reports it (back/forward: none).
fn term_button(button: MouseButton) -> Option<sg_terminal::MouseButton> {
    match button {
        MouseButton::Left => Some(sg_terminal::MouseButton::Left),
        MouseButton::Right => Some(sg_terminal::MouseButton::Right),
        MouseButton::Middle => Some(sg_terminal::MouseButton::Middle),
        _ => None,
    }
}

fn hsla(c: Rgb) -> Hsla {
    rgb(c.to_u32()).into()
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let chosen = cx.try_global::<TerminalFont>().cloned().unwrap_or_default();
        let family = chosen
            .family
            .unwrap_or_else(|| theme.mono_font_family.clone());
        let rem = window.rem_size();
        let size = chosen.size.unwrap_or(theme.mono_font_size);
        let font_size = rem * (f32::from(size) / 16.);
        let background = hsla(self.screen.background);
        let selection = theme.selection;
        let focused = self.focus.is_focused(window);
        // Clicks go to a program that tracks the mouse: the arrow, not the
        // text cursor.
        let tracking = self.pty.is_some() && self.terminal.tracks_mouse();
        let view = cx.entity();
        let grid = canvas(
            {
                let view = view.clone();
                let family = family.clone();
                move |bounds, window, cx| {
                    let text = window.text_system();
                    let font_id = text.resolve_font(&font(family.clone()));
                    let cell_width = text
                        .ch_advance(font_id, font_size)
                        .unwrap_or(font_size * 0.6);
                    let line_height = (font_size * 1.3).round();
                    let metrics = Metrics {
                        font_size,
                        cell_width,
                        line_height,
                    };
                    let inner = bounds.size.width - px(PADDING * 2.);
                    let cols = (inner / cell_width).floor().max(2.) as u16;
                    let rows = ((bounds.size.height - px(PADDING * 2.)) / line_height)
                        .floor()
                        .max(1.) as u16;
                    let size = Size {
                        cols,
                        rows,
                        cell_width: f32::from(cell_width).round() as u16,
                        cell_height: f32::from(line_height).round() as u16,
                    };
                    let surface = sg_terminal::term::Surface {
                        padding: PADDING,
                        cell_width: f32::from(cell_width),
                        cell_height: f32::from(line_height),
                        width: f32::from(bounds.size.width),
                        height: f32::from(bounds.size.height),
                    };
                    let screen = view.update(cx, |view, cx| {
                        view.layout = Some((bounds.origin, surface));
                        view.set_size(size, cx);
                        view.frame()
                    });
                    (metrics, screen)
                }
            },
            move |bounds, (metrics, screen), window, cx| {
                paint_screen(
                    bounds, &metrics, &screen, &family, focused, selection, window, cx,
                );
                // Drags keep extending the selection outside the view (and
                // the window), so these listen to the whole window.
                let drag = view.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        drag.update(cx, |view, cx| view.mouse_drag(event, cx));
                    }
                });
                let up = view.clone();
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        up.update(cx, |view, cx| view.mouse_up(event, cx));
                    }
                });
            },
        )
        .size_full();

        let terminal = div()
            .id("terminal")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .bg(background)
            .when(tracking, |el| el.cursor_default())
            .when(!tracking, |el| el.cursor_text())
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::select_all))
            .on_scroll(cx.listener(Self::scroll_wheel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::mouse_down))
            .child(grid);
        // While a program tracks the mouse, right clicks are its own.
        if tracking {
            return terminal.into_any_element();
        }
        let focus = self.focus.clone();
        let this = cx.entity();
        terminal
            .context_menu(move |menu, _, cx| {
                let selected = this.read(cx).terminal.has_selection();
                context_menu(menu, &focus, selected)
            })
            .into_any_element()
    }
}

/// Right click: what the terminal's own shortcuts do.
fn context_menu(menu: PopupMenu, focus: &FocusHandle, selected: bool) -> PopupMenu {
    menu.action_context(focus.clone())
        .menu_with_disabled("Copy", Box::new(Copy), !selected)
        .menu("Paste", Box::new(Paste))
        .separator()
        .menu("Select All", Box::new(SelectAll))
}

#[expect(clippy::too_many_arguments, reason = "one paint pass over the grid")]
fn paint_screen(
    bounds: Bounds<Pixels>,
    m: &Metrics,
    screen: &Screen,
    family: &SharedString,
    focused: bool,
    selection: Hsla,
    window: &mut Window,
    cx: &mut App,
) {
    let origin = bounds.origin + point(px(PADDING), px(PADDING));
    let cell = |col: u16, row: usize| {
        point(
            origin.x + m.cell_width * f32::from(col),
            origin.y + m.line_height * row as f32,
        )
    };
    // Backgrounds first, then text on top.
    for (row, runs) in screen.lines.iter().enumerate() {
        for run in runs {
            let fill_color = if run.selected {
                Some(selection)
            } else {
                run.bg.map(hsla)
            };
            if let Some(color) = fill_color {
                window.paint_quad(fill(
                    Bounds::new(
                        cell(run.col, row),
                        size(m.cell_width * f32::from(run.width), m.line_height),
                    ),
                    color,
                ));
            }
        }
    }
    let cursor = screen
        .cursor
        .filter(|c| usize::from(c.row) < screen.lines.len());
    let block_cursor = cursor.filter(|c| focused && c.shape == CursorShape::Block);
    if let Some(c) = cursor {
        let color = hsla(c.color.unwrap_or(screen.foreground));
        let at = cell(c.col, usize::from(c.row));
        let width = m.cell_width * if c.wide { 2. } else { 1. };
        let thin = px(2.);
        match (c.shape, focused) {
            (CursorShape::Block, true) => {
                window.paint_quad(fill(Bounds::new(at, size(width, m.line_height)), color));
            }
            (CursorShape::Bar, _) => {
                window.paint_quad(fill(Bounds::new(at, size(thin, m.line_height)), color));
            }
            (CursorShape::Underline, _) => window.paint_quad(fill(
                Bounds::new(point(at.x, at.y + m.line_height - thin), size(width, thin)),
                color,
            )),
            _ => window.paint_quad(outline(
                Bounds::new(at, size(width, m.line_height)),
                color,
                BorderStyle::Solid,
            )),
        }
    }
    for (row, runs) in screen.lines.iter().enumerate() {
        for run in runs {
            if run.text.trim().is_empty() {
                continue;
            }
            let mut color = hsla(run.fg);
            // Block elements (Claude's logo, bars) as solid shapes: glyphs
            // leave gaps between rows taller than the font.
            if let Some(rects) = single_char(&run.text).and_then(block_element) {
                let at = cell(run.col, row);
                for [x0, y0, x1, y1, alpha] in rects {
                    let mut fill_color = color;
                    fill_color.a *= alpha;
                    window.paint_quad(fill(
                        Bounds::new(
                            point(at.x + m.cell_width * x0, at.y + m.line_height * y0),
                            size(m.cell_width * (x1 - x0), m.line_height * (y1 - y0)),
                        ),
                        fill_color,
                    ));
                }
                continue;
            }
            if run.faint {
                color.a *= 0.6;
            }
            let font = Font {
                weight: if run.bold {
                    FontWeight::BOLD
                } else {
                    FontWeight::NORMAL
                },
                style: if run.italic {
                    FontStyle::Italic
                } else {
                    FontStyle::Normal
                },
                ..font(family.clone())
            };
            let text_run = TextRun {
                len: run.text.len(),
                font,
                color,
                background_color: None,
                underline: (run.underline != TermUnderline::None).then(|| UnderlineStyle {
                    thickness: px(1.),
                    color: Some(color),
                    wavy: run.underline == TermUnderline::Curly,
                }),
                strikethrough: run.strikethrough.then(|| StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(color),
                }),
            };
            // ASCII runs snap to the grid; single wide or symbol cells keep
            // their own advance inside their cells.
            let force = run.text.is_ascii().then_some(m.cell_width);
            let line = window.text_system().shape_line(
                SharedString::from(run.text.clone()),
                m.font_size,
                &[text_run],
                force,
            );
            let _ = line.paint(
                cell(run.col, row),
                m.line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }
    // The character under a block cursor, in the background color.
    if let Some(c) = block_cursor
        && let Some(runs) = screen.lines.get(usize::from(c.row))
        && let Some(run) = runs
            .iter()
            .find(|run| c.col >= run.col && c.col < run.col + run.width)
    {
        let index = usize::from(c.col - run.col);
        let glyph: String = if run.text.is_ascii() {
            run.text.chars().skip(index).take(1).collect()
        } else {
            run.text.clone()
        };
        if !glyph.trim().is_empty() {
            let color = hsla(run.bg.unwrap_or(screen.background));
            let text_run = TextRun {
                len: glyph.len(),
                font: Font {
                    weight: if run.bold {
                        FontWeight::BOLD
                    } else {
                        FontWeight::NORMAL
                    },
                    ..font(family.clone())
                },
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window.text_system().shape_line(
                SharedString::from(glyph),
                m.font_size,
                &[text_run],
                None,
            );
            let _ = line.paint(
                cell(
                    if run.text.is_ascii() { c.col } else { run.col },
                    usize::from(c.row),
                ),
                m.line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }
    // Where the view is in the scrollback, while scrolled back.
    if screen.scroll.is_scrolled_back() && screen.scroll.total > 0 {
        let track = bounds.size.height - px(4.);
        let total = screen.scroll.total as f32;
        let top = track * (screen.scroll.offset as f32 / total);
        let height = (track * (screen.scroll.visible as f32 / total)).max(px(12.));
        let mut thumb = hsla(screen.foreground);
        thumb.a = 0.35;
        window.paint_quad(fill(
            Bounds::new(
                point(bounds.right() - px(5.), bounds.top() + px(2.) + top),
                size(px(3.), height),
            ),
            thumb,
        ));
    }
}

fn single_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

/// U+2580–U+259F as rectangles `[x0, y0, x1, y1, alpha]` in cell fractions.
fn block_element(c: char) -> Option<Vec<[f32; 5]>> {
    const H: f32 = 0.5;
    let eighth = |n: u32| n as f32 / 8.;
    let quadrants = |ul: bool, ur: bool, ll: bool, lr: bool| {
        [
            (ul, [0., 0., H, H, 1.]),
            (ur, [H, 0., 1., H, 1.]),
            (ll, [0., H, H, 1., 1.]),
            (lr, [H, H, 1., 1., 1.]),
        ]
        .into_iter()
        .filter_map(|(on, rect)| on.then_some(rect))
        .collect()
    };
    let code = u32::from(c);
    Some(match code {
        0x2580 => vec![[0., 0., 1., H, 1.]],
        // Lower one eighth … seven eighths.
        0x2581..=0x2587 => vec![[0., 1. - eighth(code - 0x2580), 1., 1., 1.]],
        0x2588 => vec![[0., 0., 1., 1., 1.]],
        // Left seven eighths … one eighth.
        0x2589..=0x258f => vec![[0., 0., eighth(0x2590 - code), 1., 1.]],
        0x2590 => vec![[H, 0., 1., 1., 1.]],
        0x2591 => vec![[0., 0., 1., 1., 0.25]],
        0x2592 => vec![[0., 0., 1., 1., 0.5]],
        0x2593 => vec![[0., 0., 1., 1., 0.75]],
        0x2594 => vec![[0., 0., 1., eighth(1), 1.]],
        0x2595 => vec![[1. - eighth(1), 0., 1., 1., 1.]],
        0x2596 => quadrants(false, false, true, false),
        0x2597 => quadrants(false, false, false, true),
        0x2598 => quadrants(true, false, false, false),
        0x2599 => quadrants(true, false, true, true),
        0x259a => quadrants(true, false, false, true),
        0x259b => quadrants(true, true, true, false),
        0x259c => quadrants(true, true, false, true),
        0x259d => quadrants(false, true, false, false),
        0x259e => quadrants(false, true, true, false),
        0x259f => quadrants(false, true, true, true),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{block_element, single_char};

    #[test]
    fn block_elements_are_rectangles() {
        assert_eq!(block_element('█'), Some(vec![[0., 0., 1., 1., 1.]]));
        assert_eq!(block_element('▀'), Some(vec![[0., 0., 1., 0.5, 1.]]));
        assert_eq!(block_element('▁'), Some(vec![[0., 0.875, 1., 1., 1.]]));
        assert_eq!(block_element('▏'), Some(vec![[0., 0., 0.125, 1., 1.]]));
        assert_eq!(block_element('▛').map(|r| r.len()), Some(3));
        assert_eq!(block_element('a'), None);
        assert_eq!(single_char("▐"), Some('▐'));
        assert_eq!(single_char("ab"), None);
    }
}
