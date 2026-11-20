//! The terminal panel under the review, as in VS Code (`cmd-j`, the status
//! bar's Terminal button or the "⋯" menu): the user's shell, one per
//! worktree, started in the worktree's folder the first time the panel
//! shows that worktree. Shells keep running while the panel is
//! hidden or another worktree (or project) is shown. A shell that exits is
//! gone, and the panel hides if it was the one shown; the next `cmd-j`
//! starts a new one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName;
use gpui_kit::base::Selectable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::resizable::{ResizableState, resizable_panel, v_resizable};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{LayoutPanel, SuperGitApp, ToggleTerminal, elastic};
use crate::MOD;
use crate::terminal::{TerminalEvent, TerminalView};

/// The review keeps at least this height above the panel.
const REVIEW_MIN: f32 = 200.;
/// The panel's lowest height.
const PANEL_MIN: f32 = 100.;
/// Title bar and status bar, which the panel cannot take.
const CHROME: f32 = 80.;

pub(super) struct TerminalPanel {
    open: bool,
    shells: HashMap<PathBuf, Shell>,
    /// Review above, terminal below.
    layout: Entity<ResizableState>,
}

struct Shell {
    view: Entity<TerminalView>,
    /// The project of the worktree: closing the project ends the shell.
    root: PathBuf,
    _subscription: Subscription,
}

impl TerminalPanel {
    pub(super) fn new(cx: &mut App) -> Self {
        Self {
            open: false,
            shells: HashMap::new(),
            layout: cx.new(|_| ResizableState::default()),
        }
    }

    /// Another project's saved height applies (it is per project).
    pub(super) fn reset_layout(&mut self, cx: &mut App) {
        self.layout = cx.new(|_| ResizableState::default());
    }
}

impl SuperGitApp {
    /// `cmd-j`, the status bar's Terminal button, "⋯" and View ▸ Toggle
    /// Terminal.
    pub fn toggle_terminal(
        &mut self,
        _: &ToggleTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminal_panel.open {
            self.hide_terminal(window, cx);
        } else {
            self.show_terminal(window, cx);
        }
    }

    /// Show the selected worktree's shell (starting it) and focus it.
    fn show_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.repo.is_none() {
            return;
        }
        let Some(path) = self.selected.clone() else {
            self.flash("Select a worktree to open a terminal in it", true, cx);
            return;
        };
        if self.cleanup_paths.contains(&path) {
            self.flash("This worktree is being cleaned up", true, cx);
            return;
        }
        self.terminal_panel.open = true;
        let view = self.ensure_shell(&path, window, cx);
        view.read(cx).focus_handle().focus(window, cx);
        cx.notify();
    }

    fn hide_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal_panel.open = false;
        // Focus leaves with the panel instead of typing into a hidden shell.
        let focused = self
            .terminal_panel
            .shells
            .values()
            .any(|shell| shell.view.read(cx).focus_handle().is_focused(window));
        if focused {
            self.focus_handle.focus(window, cx);
        }
        cx.notify();
    }

    /// The worktree's shell, started now if it has none.
    fn ensure_shell(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalView> {
        if let Some(shell) = self.terminal_panel.shells.get(path) {
            return shell.view.clone();
        }
        let spawn = crate::terminal::shell_in(path);
        let view = cx.new(|cx| TerminalView::new(spawn, window, cx));
        let subscription = cx.subscribe_in(&view, window, Self::on_shell_event);
        let root = self
            .repo
            .as_ref()
            .map(|repo| repo.root.clone())
            .unwrap_or_default();
        self.terminal_panel.shells.insert(
            path.to_path_buf(),
            Shell {
                view: view.clone(),
                root,
                _subscription: subscription,
            },
        );
        view
    }

    fn on_shell_event(
        &mut self,
        view: &Entity<TerminalView>,
        event: &TerminalEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only the end matters here: the view repaints its own output.
        if !matches!(event, TerminalEvent::Exited(_)) {
            return;
        }
        let error = view.read(cx).error().map(str::to_string);
        let Some(path) = self
            .terminal_panel
            .shells
            .iter()
            .find(|(_, shell)| shell.view == *view)
            .map(|(path, _)| path.clone())
        else {
            return;
        };
        self.end_shell(&path, window, cx);
        if let Some(error) = error {
            self.flash(format!("Terminal: {error}"), true, cx);
        }
    }

    /// Hang up a worktree's shell (and what runs in it); the panel hides if
    /// it showed that shell.
    fn end_shell(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal_panel.open && self.selected.as_deref() == Some(path) {
            self.hide_terminal(window, cx);
        }
        // Dropping the view drops its PTY, which hangs up the shell.
        self.terminal_panel.shells.remove(path);
        cx.notify();
    }

    /// Worktrees that are gone (cleaned up) take their shells with them.
    pub(super) fn drop_shells(&mut self, removed: impl Fn(&Path) -> bool) {
        self.terminal_panel.shells.retain(|path, _| !removed(path));
    }

    /// A closed project's shells end.
    pub(super) fn drop_project_shells(&mut self, root: &Path) {
        self.terminal_panel
            .shells
            .retain(|_, shell| shell.root != root);
    }

    /// The review with the terminal panel under it, while the panel is open.
    pub(super) fn with_terminal(
        &mut self,
        review: AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.terminal_panel.open {
            return review;
        }
        let Some(path) = self.selected.clone() else {
            return review;
        };
        if self.cleanup_paths.contains(&path) {
            return review;
        }
        let view = self.ensure_shell(&path, window, cx);
        let max = (window.viewport_size().height.as_f32() - CHROME - REVIEW_MIN).max(PANEL_MIN);
        let height = self.repository_view().terminal_height.min(max);
        let panel = self.render_terminal_panel(&path, view, cx);
        v_flex()
            .size_full()
            .min_h_0()
            .child(
                v_resizable("terminal-layout")
                    .with_state(&self.terminal_panel.layout)
                    .on_resize(cx.listener(|this, state, _, cx| {
                        this.remember_layout(LayoutPanel::Terminal, state, cx)
                    }))
                    .child(
                        resizable_panel()
                            .size_range(px(REVIEW_MIN)..Pixels::MAX)
                            .child(elastic(review)),
                    )
                    .child(
                        resizable_panel()
                            .size(px(height))
                            .size_range(px(PANEL_MIN)..px(max))
                            .fixed()
                            .child(panel),
                    ),
            )
            .into_any_element()
    }

    fn render_terminal_panel(
        &self,
        path: &Path,
        view: Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let kill = path.to_path_buf();
        v_flex()
            .size_full()
            .min_h_0()
            .border_t_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1p5()
                            .px_1()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .child(
                                Icon::new(IconName::SquareTerminal)
                                    .small()
                                    .text_color(theme.muted_foreground),
                            )
                            .child("Terminal"),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("terminal-kill")
                            .label("Kill")
                            .xsmall()
                            .ghost()
                            .tooltip("End this shell and what runs in it")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.end_shell(&kill, window, cx)
                            })),
                    )
                    .child(
                        Button::new("terminal-hide")
                            .icon(IconName::ChevronDown)
                            .xsmall()
                            .ghost()
                            .tooltip(format!(
                                "Hide the terminal ({MOD}-j); the shell keeps running"
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_terminal(&ToggleTerminal, window, cx)
                            })),
                    ),
            )
            .child(div().flex_1().min_h_0().child(view))
            .into_any_element()
    }

    /// The status bar's Terminal button, right under where the panel opens:
    /// the panel's icon, lit while open.
    pub(super) fn terminal_button(&self, cx: &mut Context<Self>) -> Button {
        Button::new("toggle-terminal")
            .icon(IconName::PanelBottom)
            .label("Terminal")
            .xsmall()
            .ghost()
            .selected(self.terminal_panel.open && self.selected.is_some())
            .tooltip(format!("Show or hide the worktree's terminal ({MOD}-j)"))
            .on_click(
                cx.listener(|this, _, window, cx| {
                    this.toggle_terminal(&ToggleTerminal, window, cx)
                }),
            )
    }
}
