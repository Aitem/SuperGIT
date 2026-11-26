//! The sidebar: one tree of the projects open in the window, each with its
//! worktrees. The active project's worktrees are live and carry every action;
//! another project's come from its last snapshot, and a click on one of them
//! opens that project at that worktree.

use super::projects::{ATTENTION_ORDER, Activity, project_name};
use super::tree_menus::{ConversationTarget, WorktreeTarget};
use super::*;
use gpui_kit::component::menu::DropdownMenu as _;

/// A project's row: its padding, Git's mark (`small`) and the gap, then its
/// name. The tree under it starts where the name does.
const ROW_START: f32 = 8.;
const GIT_MARK: f32 = 14.;
const NAME_START: f32 = ROW_START + GIT_MARK + 4.;
/// How far worktree rows sit under their project: their status dots
/// centered under its name's first letter, where the main checkout's
/// conversations have their marks, so they read a level deeper than it.
const WORKTREE_INDENT: f32 = NAME_START - (SIGNAL_CELL - AGENT_MARK) / 2.;
/// The squares a project's "+" and chevron are pressed in.
const FOLD_BUTTON: f32 = 24.;
/// The place of a row's mark, before its name: a dot, or an agent's signal.
const SIGNAL_CELL: f32 = 16.;
/// Rows of the tree: a project's is taller than its worktrees'. A line only
/// between projects, so the rows of one read as a block.
const PROJECT_ROW: f32 = 40.;
const WORKTREE_ROW: f32 = 32.;
/// A conversation's row under its worktree: lower than the worktree's.
const AGENT_ROW: f32 = 24.;
/// A worktree's type icon (branch, house) before its name, and an agent's
/// mark before a conversation's title (both `xsmall`).
const TYPE_ICON: f32 = 12.;
const AGENT_MARK: f32 = 12.;
/// Where the main checkout's conversations start: their marks under the
/// project's name, as linked worktrees' type icons are.
const MAIN_AGENT_INDENT: f32 = NAME_START;

/// Where a worktree's conversations start: their marks under its type
/// icon (past its indent, the signal cell and the gap).
fn worktree_agent_indent(completed: bool) -> f32 {
    let row = if completed {
        WORKTREE_INDENT + SIGNAL_CELL
    } else {
        WORKTREE_INDENT
    };
    row + SIGNAL_CELL + 4. + (TYPE_ICON - AGENT_MARK) / 2.
}

/// The main checkout's changed files and commits ahead and behind, on its
/// project's row (only those that are not zero).
fn main_counts(main: &WorktreeInfo, theme: &gpui_kit::component::Theme) -> AnyElement {
    let status = main.status.as_ref().ok();
    let counts = [
        (IconName::Diff, status.map_or(0, |s| s.files.len() as u64)),
        (IconName::ArrowUp, status.map_or(0, |s| u64::from(s.ahead))),
        (
            IconName::ArrowDown,
            status.map_or(0, |s| u64::from(s.behind)),
        ),
    ];
    h_flex()
        .flex_none()
        .gap_1p5()
        .children(counts.into_iter().filter(|(_, n)| *n > 0).map(|(icon, n)| {
            h_flex()
                .flex_none()
                .gap_0p5()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(Icon::new(icon).xsmall().text_color(theme.muted_foreground))
                .child(n.to_string())
        }))
        .into_any_element()
}
/// Where rows end, from the sidebar's right edge: a project's "+" icon, and
/// under it the marks of its worktrees.
const ROW_END: f32 = 16.;

/// The most urgent agent signal of a worktree or a project (needs approval,
/// needs an answer, failed, unread, running), shown before the name: a blue
/// dot while agents only work, an icon of its own in its color when one
/// wants the user.
struct Signal {
    kind: Kind,
    /// Hover text: which agent, in which state.
    label: String,
    /// The native conversation behind it, opened by a click on the dot:
    /// `(worktree, conversation id, its agent)`.
    open: Option<(PathBuf, String, Provider)>,
}

impl Signal {
    fn urgency(&self) -> usize {
        ATTENTION_ORDER
            .iter()
            .position(|kind| *kind == self.kind)
            .unwrap_or(ATTENTION_ORDER.len())
    }

    /// A process state (live, or estimated from a transcript) as a signal.
    fn from_state(state: AgentState, label: String) -> Option<Self> {
        let kind = match state {
            AgentState::NeedsAttention => Kind::Approval,
            AgentState::Working => Kind::Running,
            AgentState::Finished => Kind::Completed,
            AgentState::Idle => return None,
        };
        Some(Self {
            kind,
            label,
            open: None,
        })
    }

    /// A project's: its most urgent conversation, or its live agents.
    fn of_project(activity: Activity) -> Option<Self> {
        let attention =
            activity
                .attention
                .map(|(kind, count, worktree, conversation, provider)| Self {
                    kind,
                    label: if count > 1 {
                        format!("{count} conversations: {}", kind.label())
                    } else {
                        format!("{} {}", provider.label(), kind.label())
                    },
                    open: Some((worktree, conversation, provider)),
                });
        let live = activity
            .agent
            .and_then(|state| Self::from_state(state, format!("Agent {}", state.label())));
        [attention, live]
            .into_iter()
            .flatten()
            .min_by_key(Self::urgency)
    }
}

impl SuperGitApp {
    /// Collapse the sidebar to a rail of initials, or expand it (cmd-b).
    pub fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        let projects = &mut self.persisted.projects;
        projects.collapsed = !projects.collapsed;
        self.queue_persist(cx);
        cx.notify();
    }

    /// Width the sidebar takes from the window.
    pub(super) fn sidebar_width(&self, cx: &App) -> f32 {
        if !self.projects_visible() {
            0.
        } else if self.persisted.projects.collapsed {
            RAIL_WIDTH
        } else {
            self.sidebar_layout
                .read(cx)
                .sizes()
                .first()
                .map(|width| width.as_f32())
                .unwrap_or(self.persisted.projects.sidebar_width)
                .clamp(SIDEBAR_MIN, SIDEBAR_MAX)
        }
    }

    pub(super) fn remember_sidebar_width(
        &mut self,
        state: &Entity<ResizableState>,
        cx: &mut Context<Self>,
    ) {
        let Some(width) = state.read(cx).sizes().first().map(|width| width.as_f32()) else {
            return;
        };
        self.persisted.projects.sidebar_width = width;
        self.persisted.projects.sanitize();
        self.queue_persist(cx);
    }

    /// The active project, as the sidebar's rows see it.
    pub(super) fn active_scope(&self) -> Scope<'_> {
        Scope {
            view: self.repository_view(),
            prs: &self.prs,
            prs_fresh: self.prs_error.is_none()
                && self
                    .prs_fetched_at
                    .is_some_and(|time| time.elapsed() < PR_POLL_INTERVAL * 2),
            active: true,
        }
    }

    /// Exact native activity; estimates of sessions outside SuperGIT only in
    /// the active project, which polls them (a snapshot's would be stale).
    pub(super) fn row_agent_state(
        &self,
        scope: Scope<'_>,
        info: &WorktreeInfo,
        cx: &App,
    ) -> Option<AgentState> {
        if scope.active {
            self.agent_state_for(info, cx)
        } else {
            self.live_agent_state(info.path(), cx)
        }
    }

    /// Fold or unfold a project's worktrees; unfolding another project
    /// rereads it.
    fn toggle_fold(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let fold = !self.persisted.projects.is_folded(&root);
        self.persisted.projects.set_folded(&root, fold);
        if !fold {
            self.load_project(root, cx);
        }
        self.queue_persist(cx);
        cx.notify();
    }

    /// A click on a project: open it, unfolded; on the active one, fold or
    /// unfold it.
    fn click_project(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if self.active_root() == Some(root.as_path()) {
            self.toggle_fold(root, cx);
            return;
        }
        self.persisted.projects.set_folded(&root, false);
        self.switch_project(root, cx);
    }

    /// Open another project at one of its worktrees.
    fn open_worktree_in(&mut self, root: PathBuf, path: PathBuf, cx: &mut Context<Self>) {
        if self.git_operation_running() {
            self.flash(
                "Wait for the running Git operation before switching projects",
                true,
                cx,
            );
            return;
        }
        // Opening a project selects its saved worktree.
        self.persisted
            .repositories
            .entry(root.clone())
            .or_default()
            .selected = Some(path);
        self.switch_project(root, cx);
    }

    pub(super) fn render_sidebar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let roots = self.persisted.projects.roots.clone();
        let tree: Vec<AnyElement> = roots
            .iter()
            .enumerate()
            .map(|(ix, root)| self.render_project_node(ix, root, &theme, cx))
            .collect();
        // Its buttons are squares as in the right rail, a line between them.
        let square = |id, icon| {
            square_button(id, icon)
                .border_l_1()
                .border_color(theme.sidebar_border)
        };
        // The app's name, only where all of it fits: in less room it wraps
        // onto a second line, out of sight, rather than being cut short.
        let name = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .flex()
            .flex_wrap()
            // Only an item after a line's first one can wrap.
            .child(div().flex_none().h_full())
            .child(
                h_flex()
                    .flex_none()
                    .h_full()
                    .pr_2()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("SuperGIT"),
            );
        // As tall as the rails are wide, like the worktree's header: their
        // lines meet. On macOS the sidebar reaches the window's top edge: the
        // window buttons sit at the row's left, and the row moves the window
        // as a title bar does. Not gpui-component's `TitleBar`, which is
        // never narrower than its content: in a narrow sidebar it pushed the
        // squares out, over the review's header.
        let header = h_flex()
            .h(px(RAIL_WIDTH))
            .flex_none()
            .pl(px(if window_buttons_shown(window) {
                WINDOW_BUTTONS_INSET
            } else {
                12.
            }))
            .border_b_1()
            .border_color(theme.sidebar_border)
            .child(name)
            .child(self.add_project_button(
                square("add-project", IconName::Plus).tooltip(format!("Add a project ({MOD}-o)")),
                cx,
            ))
            .child(self.app_menu_button(
                square("app-menu", IconName::Ellipsis).tooltip("More actions"),
                cx,
            ))
            .child(
                square("collapse-sidebar", IconName::PanelLeftClose)
                    .tooltip(format!("Collapse the sidebar ({MOD}-b)"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_sidebar(&ToggleSidebar, window, cx)
                    })),
            );
        let header = self.window_drag_area(header, cx);
        v_flex()
            .w_full()
            .min_w_0()
            .h_full()
            .flex_none()
            .bg(theme.sidebar)
            // No line of its own at the right: the resize handle draws it.
            .child(header)
            .child(
                v_flex()
                    .id("sidebar-tree")
                    .flex_1()
                    .min_h_0()
                    .pb_2()
                    .overflow_y_scroll()
                    .children(tree),
            )
            .into_any_element()
    }

    /// A project's row and, unfolded, its worktrees.
    fn render_project_node(
        &self,
        ix: usize,
        root: &Path,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.active_root() == Some(root);
        let folded = self.persisted.projects.is_folded(root);
        let snapshot = if active {
            None
        } else {
            self.snapshot_scope(root)
        };
        let signal = Signal::of_project(self.project_activity(root, cx));
        // The project's row is its main checkout: no row of its own below.
        let (scope, worktrees) = if active {
            (Some(self.active_scope()), &self.worktrees[..])
        } else {
            snapshot.map_or((None, &[][..]), |(scope, worktrees)| {
                (Some(scope), worktrees)
            })
        };
        let main = worktrees.iter().find(|info| info.wt.is_main);
        let selected = main.is_some_and(|main| {
            active
                && self.selected.as_deref() == Some(main.path())
                && self.tree_agent_in(main.path(), cx).is_none()
        });
        let main_path = main.map(|main| main.wt.path.clone());
        let target = root.to_path_buf();
        let fold_target = root.to_path_buf();
        let add_target = root.to_path_buf();
        let row = h_flex()
            .id("project")
            .h(px(PROJECT_ROW))
            .flex_none()
            .pl(px(ROW_START))
            // The chevron's square is wider than its 16px icon, which ends
            // at ROW_END.
            .pr(px(ROW_END - (FOLD_BUTTON - 16.) / 2.))
            .gap_1()
            .cursor_pointer()
            .when(selected, |el| el.bg(theme.sidebar_accent))
            .hover(|el| {
                el.bg(if selected {
                    theme.sidebar_accent
                } else {
                    theme.list_hover
                })
            })
            // Selects the main checkout; the chevron (at the right) folds.
            .on_click(cx.listener(move |this, _, _, cx| {
                this.tree_agent = None;
                match main_path.clone() {
                    Some(path) if this.active_root() == Some(target.as_path()) => {
                        this.select_worktree(path, cx)
                    }
                    Some(path) => this.open_worktree_in(target.clone(), path, cx),
                    None => this.click_project(target.clone(), cx),
                }
            }))
            .child(
                div()
                    .id("project-mark")
                    .flex_none()
                    .child(crate::icons::git_mark().small())
                    .tooltip(|window, cx| {
                        Tooltip::new("Git repository: its row is the main checkout")
                            .build(window, cx)
                    })
                    .tooltip_show_delay(crate::TOOLTIP_DELAY),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(project_name(root)),
            )
            .children(main.map(|main| main_counts(main, theme)))
            .when(signal.is_some(), |row| {
                row.child(self.render_signal("project-signal", signal, root, theme, cx))
            })
            .child(
                div()
                    .id("project-new-worktree")
                    .flex_none()
                    .size(px(FOLD_BUTTON))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .hover(|el| el.bg(theme.sidebar_accent))
                    .tooltip(|window, cx| {
                        Tooltip::new("New worktree in this project").build(window, cx)
                    })
                    .tooltip_show_delay(crate::TOOLTIP_DELAY)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.open_new_worktree(add_target.clone(), window, cx);
                    }))
                    .child(
                        Icon::new(IconName::Plus)
                            .with_size(gpui_kit::component::Size::Medium)
                            .text_color(theme.muted_foreground),
                    ),
            )
            // Last, at the right edge: the name and the tree start at the
            // left one.
            .child(
                div()
                    .id("project-fold")
                    .flex_none()
                    .size(px(FOLD_BUTTON))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .hover(|el| el.bg(theme.sidebar_accent))
                    .tooltip(move |window, cx| {
                        Tooltip::new(if folded {
                            "Show this project's worktrees"
                        } else {
                            "Hide this project's worktrees"
                        })
                        .build(window, cx)
                    })
                    .tooltip_show_delay(crate::TOOLTIP_DELAY)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_fold(fold_target.clone(), cx);
                    }))
                    .child(
                        Icon::new(if folded {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .with_size(gpui_kit::component::Size::Medium)
                        .text_color(theme.muted_foreground),
                    ),
            );
        let row = self.repository_menu(row, root, main.map(WorktreeInfo::path), active, cx);

        // The main checkout's conversations, under its row.
        let main_agents = match (folded, scope, main) {
            (false, Some(scope), Some(main)) => {
                self.render_agent_rows(main, MAIN_AGENT_INDENT, scope, root, theme, cx)
            }
            _ => Vec::new(),
        };
        let children = if folded {
            None
        } else if active {
            Some(self.render_active_worktrees(root, theme, cx))
        } else if let Some((scope, worktrees)) = snapshot {
            Some(self.render_worktree_group(root, scope, worktrees, theme, cx))
        } else {
            let (text, color) = match self.project_load_error(root) {
                Some(error) => (error.to_string(), theme.danger),
                None => ("Loading…".to_string(), theme.muted_foreground),
            };
            Some(note(text, color))
        };
        // Element ids repeat across projects; the node keeps them apart.
        v_flex()
            .id(("project-node", ix))
            .when(ix > 0, |node| {
                node.border_t_1().border_color(theme.sidebar_border)
            })
            .child(row)
            .children(main_agents)
            .children(children)
            .into_any_element()
    }

    fn render_active_worktrees(
        &self,
        root: &Path,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(error) = &self.load_error {
            return v_flex()
                .child(note(error.clone(), theme.danger))
                .child(self.render_worktree_group(
                    root,
                    self.active_scope(),
                    &self.worktrees,
                    theme,
                    cx,
                ))
                .into_any_element();
        }
        if self.worktrees.is_empty() {
            return note("Loading…".into(), theme.muted_foreground);
        }
        self.render_worktree_group(root, self.active_scope(), &self.worktrees, theme, cx)
    }

    /// A project's worktrees, the completed ones in a foldable group last.
    fn render_worktree_group(
        &self,
        root: &Path,
        scope: Scope<'_>,
        worktrees: &[WorktreeInfo],
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed = scope.view.completed_collapsed;
        let mut open = Vec::new();
        let mut completed = Vec::new();
        let mut completed_paths = Vec::new();
        for (ix, info) in worktrees.iter().enumerate() {
            if info.wt.is_main {
                continue;
            }
            if self.completed_in(scope, info, cx) {
                completed_paths.push(info.wt.path.clone());
                if !collapsed {
                    completed
                        .push(self.render_worktree_row(ix, info, true, scope, root, theme, cx));
                    completed.extend(self.render_agent_rows(
                        info,
                        worktree_agent_indent(true),
                        scope,
                        root,
                        theme,
                        cx,
                    ));
                }
            } else {
                open.push(self.render_worktree_row(ix, info, false, scope, root, theme, cx));
                open.extend(self.render_agent_rows(
                    info,
                    worktree_agent_indent(false),
                    scope,
                    root,
                    theme,
                    cx,
                ));
            }
        }
        let completed_count = completed_paths.len();
        let fold_root = root.to_path_buf();
        v_flex()
            .children(open)
            .when(completed_count > 0, |list| {
                list.child(
                    h_flex()
                        .id("completed-group")
                        .h(px(WORKTREE_ROW))
                        .flex_none()
                        .pl(px(WORKTREE_INDENT))
                        .pr(px(ROW_END))
                        .gap_1()
                        .cursor_pointer()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .hover(|el| el.bg(theme.list_hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_completed_collapsed_in(fold_root.clone(), !collapsed, cx)
                        }))
                        .child(
                            // In the signals' place: the label starts level
                            // with the worktrees' names.
                            div()
                                .flex_none()
                                .w(px(SIGNAL_CELL))
                                .flex()
                                .justify_center()
                                .child(
                                    Icon::new(if collapsed {
                                        IconName::ChevronRight
                                    } else {
                                        IconName::ChevronDown
                                    })
                                    .small()
                                    .text_color(theme.muted_foreground),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .child(format!("Completed · {completed_count}")),
                        )
                        .when(scope.active, |header| {
                            header.child(
                                Button::new("preview-completed-cleanup")
                                    .label("Clean up")
                                    .tooltip(
                                        "Preview removing these worktrees and their local branches",
                                    )
                                    .xsmall()
                                    .outline()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.open_cleanup_preview(
                                            completed_paths.clone(),
                                            window,
                                            cx,
                                        )
                                    })),
                            )
                        }),
                )
                .children(completed)
            })
            .into_any_element()
    }

    /// One line per worktree: agent status, name, then what differs (tags,
    /// changed files, commits ahead/behind, its pull request). A second line
    /// only for a running operation or an error. `completed`: a row of the
    /// Completed group, a step deeper than the group's label and its name
    /// muted (the selected one keeps its color).
    #[expect(
        clippy::too_many_arguments,
        reason = "a row of the tree: its place, its worktree and the project around it"
    )]
    fn render_worktree_row(
        &self,
        ix: usize,
        info: &WorktreeInfo,
        completed: bool,
        scope: Scope<'_>,
        root: &Path,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // One selection in the tree: the worktree, unless its conversation
        // picked last is on screen (that row shows selected instead).
        let selected = scope.active
            && self.selected.as_deref() == Some(info.path())
            && self.tree_agent_in(info.path(), cx).is_none();
        let path = info.wt.path.clone();
        let pr = scope.pr_for_branch(info.wt.branch.as_deref());
        let signal = self.worktree_signal(scope, info, cx);
        let kept = scope.kept(info.path());
        let rebasing = scope.active && self.base_update_paths.contains(info.path());
        let status = info.status.as_ref().ok();
        let changed = status.map_or(0, |status| status.files.len());
        let ahead = status.map_or(0, |status| status.ahead);
        let behind = status.map_or(0, |status| status.behind);
        let tag = |text: &'static str| {
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        // A count behind its icon, muted: colors are for the agents' signals.
        let count = |icon: IconName, count: String| {
            h_flex()
                .flex_none()
                .gap_0p5()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(Icon::new(icon).xsmall().text_color(theme.muted_foreground))
                .child(count)
        };
        // "feat/agent-rail": what stands before the last slash says what kind
        // of work it is and repeats from row to row, so it is muted; the
        // name after it tells worktrees apart.
        let name = info.wt.display_name();
        let prefix = name.rfind('/').map_or(0, |slash| slash + 1);
        let muted = HighlightStyle {
            color: Some(theme.muted_foreground),
            ..Default::default()
        };
        // A long name truncates: the tooltip has it whole, and the folder.
        let full_name: SharedString = format!("{name}\n{}", info.wt.path.display()).into();
        let name =
            StyledText::new(name).with_highlights((prefix > 0).then_some((0..prefix, muted)));

        // What it is at a glance: a branch's worktree, or a house for one
        // on the main line.
        let on_main_line = matches!(info.wt.branch.as_deref(), Some("main" | "master"));
        let title = h_flex()
            .gap_1()
            .child(self.render_signal("signal", signal, root, theme, cx))
            .child(
                div().flex_none().w(px(TYPE_ICON)).child(
                    Icon::new(if on_main_line {
                        IconName::House
                    } else {
                        IconName::GitBranch
                    })
                    .xsmall()
                    .text_color(theme.muted_foreground),
                ),
            )
            .child(
                div()
                    .id(("worktree-name", ix))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .when(completed && !selected, |el| {
                        el.text_color(theme.muted_foreground)
                    })
                    .tooltip(move |window, cx| Tooltip::new(full_name.clone()).build(window, cx))
                    .tooltip_show_delay(crate::TOOLTIP_DELAY)
                    .child(name),
            )
            .child(
                // Only what differs: the main checkout, a kept worktree,
                // changed files, commits ahead and behind, the pull request.
                h_flex()
                    .flex_none()
                    .pl_1()
                    .gap_1p5()
                    .when(kept, |el| el.child(tag("kept")))
                    .when(changed > 0, |el| {
                        el.child(count(IconName::Diff, changed.to_string()))
                    })
                    .when(ahead > 0, |el| {
                        el.child(count(IconName::ArrowUp, ahead.to_string()))
                    })
                    .when(behind > 0, |el| {
                        el.child(count(IconName::ArrowDown, behind.to_string()))
                    })
                    .when_some(pr, |el, pr| el.child(pr_badge(pr, theme))),
            );

        let mut notes: Vec<AnyElement> = Vec::new();
        if info.status.is_err() {
            notes.push(
                div()
                    .text_color(theme.danger)
                    .child("Status unavailable")
                    .into_any_element(),
            );
        }
        if rebasing {
            notes.push(div().child("Rebasing…").into_any_element());
        }

        let open_root = root.to_path_buf();
        let row = v_flex()
            .id(("worktree", ix))
            .min_h(px(WORKTREE_ROW))
            .justify_center()
            .pl(px(if completed {
                WORKTREE_INDENT + SIGNAL_CELL
            } else {
                WORKTREE_INDENT
            }))
            .pr(px(ROW_END))
            .py_1()
            .cursor_pointer()
            // The selection is a background, as in the right rail.
            .when(selected, |el| el.bg(theme.sidebar_accent))
            .hover(|el| {
                el.bg(if selected {
                    theme.sidebar_accent
                } else {
                    theme.list_hover
                })
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.tree_agent = None;
                if this.active_root() == Some(open_root.as_path()) {
                    this.select_worktree(path.clone(), cx);
                } else {
                    this.open_worktree_in(open_root.clone(), path.clone(), cx);
                }
            }))
            .child(title)
            .when(!notes.is_empty(), |el| {
                el.child(
                    h_flex()
                        .pl(px(18.))
                        .gap_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .children(notes),
                )
            });
        let target = WorktreeTarget {
            path: info.wt.path.clone(),
            branch: info.wt.branch.clone(),
            active: scope.active,
            kept,
            rebase: scope
                .active
                .then(|| self.rebase_menu_item(info, cx))
                .flatten(),
        };
        self.worktree_menu(row, target, cx)
    }

    /// The conversation picked last in the tree, if it is the one on screen
    /// and belongs to `path`.
    fn tree_agent_in(&self, path: &Path, cx: &App) -> Option<&str> {
        let picked = self.tree_agent.as_deref()?;
        if !self.chat_shown() || self.selected.as_deref() != Some(path) {
            return None;
        }
        let ix = self.active_chat.get(path).copied().unwrap_or(0);
        let chat = self.chats.get(path)?.get(ix)?;
        (chat.read(cx).local_id() == picked).then_some(picked)
    }

    /// The worktree's conversations (the ready-to-type draft stays out):
    /// live ones while the worktree has been open, else the saved ones.
    fn agents_in(&self, scope: Scope<'_>, info: &WorktreeInfo, cx: &App) -> Vec<AgentEntry> {
        let path = info.path();
        let picked = scope.active.then(|| self.tree_agent_in(path, cx)).flatten();
        // Busy in another process too (Claude Code's registry).
        let busy_elsewhere = |provider: Provider, id: Option<&str>| {
            id.is_some_and(|id| {
                info.sessions
                    .iter()
                    .any(|s| s.busy && s.provider == provider && s.id == id)
            })
        };
        if let Some(chats) = self.chats.get(path) {
            let on_screen =
                (scope.active && self.chat_shown() && self.selected.as_deref() == Some(path))
                    .then(|| self.active_chat.get(path).copied().unwrap_or(0));
            return chats
                .iter()
                .enumerate()
                .filter_map(|(ix, chat)| {
                    let chat = chat.read(cx);
                    (!chat.is_fresh()).then(|| AgentEntry {
                        provider: chat.provider(),
                        title: chat.title(),
                        local_id: chat.local_id().to_string(),
                        kind: chat.attention().kind,
                        unread: chat.attention().unread,
                        working: chat.is_working()
                            || busy_elsewhere(chat.provider(), chat.session_id()),
                        on_screen: on_screen == Some(ix),
                        selected: picked == Some(chat.local_id()),
                        session_id: chat.session_id().map(str::to_string),
                        cwd: chat.cwd().to_path_buf(),
                        transcript: chat.transcript_file(),
                        can_fork: chat.can_fork(),
                    })
                })
                .collect();
        }
        scope
            .view
            .conversations
            .get(path)
            .into_iter()
            .flatten()
            .filter(|saved| {
                saved.session_id.is_some()
                    || saved.transcript.is_some()
                    || saved.fork_from.is_some()
            })
            .filter_map(|saved| {
                let provider = Provider::parse(&saved.provider)?;
                Some(AgentEntry {
                    provider,
                    title: if saved.display_title().trim().is_empty() {
                        "Conversation".to_string()
                    } else {
                        saved.display_title().to_string()
                    },
                    local_id: saved.local_id.clone(),
                    kind: saved.attention.kind,
                    unread: saved.attention.unread,
                    working: busy_elsewhere(provider, saved.session_id.as_deref()),
                    on_screen: false,
                    selected: false,
                    session_id: saved.session_id.clone(),
                    cwd: saved.cwd.clone(),
                    transcript: saved.transcript.clone(),
                    can_fork: saved.session_id.is_some() || saved.fork_from.is_some(),
                })
            })
            .collect()
    }

    /// One row per conversation under its worktree: the agent's mark (in its
    /// colors while it works), the title, and the state's icon when it wants
    /// the user. A click opens the conversation under the worktree's header.
    /// `indent`: where the parent's name starts, so the marks line up under
    /// it.
    fn render_agent_rows(
        &self,
        info: &WorktreeInfo,
        indent: f32,
        scope: Scope<'_>,
        root: &Path,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.agents_in(scope, info, cx)
            .into_iter()
            .map(|agent| {
                let state = if agent.working {
                    Some(Kind::Running)
                } else {
                    Some(agent.kind)
                        .filter(|kind| *kind != Kind::Idle && (kind.is_live() || agent.unread))
                };
                let mark = if agent.working {
                    crate::icons::agent_icon(agent.provider)
                } else {
                    crate::icons::agent_mark(agent.provider).text_color(theme.muted_foreground)
                };
                let tooltip = format!(
                    "{} · {}\n{}",
                    agent.provider.label(),
                    agent.title,
                    state.map_or("idle", Kind::label)
                );
                let (root, worktree) = (root.to_path_buf(), info.wt.path.clone());
                let (conversation, provider) = (agent.local_id.clone(), agent.provider);
                let target = ConversationTarget {
                    root: root.clone(),
                    worktree: worktree.clone(),
                    local_id: agent.local_id.clone(),
                    provider,
                    session_id: agent.session_id.clone(),
                    cwd: agent.cwd.clone(),
                    transcript: agent.transcript.clone(),
                    can_fork: agent.can_fork,
                    title: agent.title.clone(),
                    unread: agent.unread,
                    working: agent.working,
                    active: scope.active,
                };
                // Being renamed: the title is a field, the row neither
                // opens the conversation nor shows its tooltip meanwhile.
                let field = self.rename_field(&worktree, &agent.local_id, cx);
                let renaming = field.is_some();
                let current = agent.title.clone();
                let row = h_flex()
                    .id(SharedString::from(format!("agent-row-{}", agent.local_id)))
                    .h(px(AGENT_ROW))
                    .pl(px(indent))
                    .pr(px(ROW_END))
                    .gap_1p5()
                    .text_xs()
                    .cursor_pointer()
                    // Picked last and on screen: selected, as a worktree is
                    // (which then shows unselected), kept under the pointer.
                    .when(agent.selected, |el| el.bg(theme.sidebar_accent))
                    .hover(|el| {
                        el.bg(if agent.selected {
                            theme.sidebar_accent
                        } else {
                            theme.list_hover
                        })
                    })
                    .when(!renaming, |row| {
                        row.tooltip(move |window, cx| {
                            Tooltip::new(tooltip.clone()).build(window, cx)
                        })
                        .tooltip_show_delay(crate::TOOLTIP_DELAY)
                        // A click opens it, a double click renames it.
                        .on_click(cx.listener(
                            move |this, event: &ClickEvent, window, cx| {
                                if event.click_count() >= 2 {
                                    this.start_rename(
                                        root.clone(),
                                        worktree.clone(),
                                        conversation.clone(),
                                        current.clone(),
                                        window,
                                        cx,
                                    );
                                    return;
                                }
                                this.tree_agent = Some(conversation.clone());
                                this.open_project_attention(
                                    root.clone(),
                                    worktree.clone(),
                                    conversation.clone(),
                                    provider,
                                    window,
                                    cx,
                                );
                            },
                        ))
                    })
                    .child(div().flex_none().child(mark.xsmall()))
                    .child(field.unwrap_or_else(|| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .when(agent.on_screen, |el| el.font_weight(FontWeight::MEDIUM))
                            .text_color(if agent.working || agent.unread || agent.on_screen {
                                theme.sidebar_foreground
                            } else {
                                theme.muted_foreground
                            })
                            .child(agent.title)
                            .into_any_element()
                    }))
                    // Shapes, not only colors: a spinner-like ring while it
                    // works, ! for an approval, ? for a question, ✓ done, ×
                    // failed.
                    .when_some(state, |el, kind| {
                        let icon = signal_icon(kind).unwrap_or(IconName::LoaderCircle);
                        el.child(
                            div().flex_none().child(
                                Icon::new(icon)
                                    .xsmall()
                                    .text_color(attention_color(kind, theme)),
                            ),
                        )
                    });
                self.conversation_menu(row, target, cx)
            })
            .collect()
    }

    /// A worktree's most urgent signal: its native conversations (exact),
    /// live processes, and in the active project the estimated state of
    /// sessions running outside SuperGIT.
    fn worktree_signal(&self, scope: Scope<'_>, info: &WorktreeInfo, cx: &App) -> Option<Signal> {
        let path = info.path();
        let native =
            self.native_attention_in(scope.view, path, cx)
                .map(|(kind, provider, conversation)| Signal {
                    kind,
                    label: format!("{} {}", provider.label(), kind.label()),
                    open: Some((path.to_path_buf(), conversation, provider)),
                });
        let live = self
            .live_agent_state(path, cx)
            .and_then(|state| Signal::from_state(state, format!("Agent {}", state.label())));
        let external = scope
            .active
            .then(|| self.external_agent(info, cx))
            .flatten()
            .and_then(|(state, provider)| {
                Signal::from_state(
                    state,
                    format!("{} {} (estimated)", provider.label(), state.label()),
                )
            });
        [native, live, external]
            .into_iter()
            .flatten()
            .min_by_key(Signal::urgency)
    }

    /// The mark before a worktree's name (and in a project's header). Every
    /// worktree has one: a gray dot when quiet, a blue one while its agent
    /// works, and an icon in the signal's color when the agent wants the user
    /// (its label on hover; a click opens the conversation behind it).
    fn render_signal(
        &self,
        id: impl Into<ElementId>,
        signal: Option<Signal>,
        root: &Path,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let dot = |color: Hsla| div().size(px(8.)).rounded_full().bg(color);
        let cell = || {
            div()
                .size(px(SIGNAL_CELL))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
        };
        let Some(signal) = signal else {
            // Quiet: gray, but not the border's gray, which vanishes on a
            // selected row's background.
            return cell()
                .child(dot(theme.muted_foreground.opacity(0.6)))
                .into_any_element();
        };
        let color = attention_color(signal.kind, theme);
        let cell = cell()
            .id(id)
            .rounded_sm()
            .map(|cell| match signal_icon(signal.kind) {
                Some(icon) => cell.child(Icon::new(icon).small().text_color(color)),
                None => cell.child(dot(color)),
            });
        let tooltip = if signal.open.is_some() {
            format!("{} · click to open", signal.label)
        } else {
            signal.label
        };
        let cell = cell
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .tooltip_show_delay(crate::TOOLTIP_DELAY);
        let Some((worktree, conversation, provider)) = signal.open else {
            return cell.into_any_element();
        };
        let root = root.to_path_buf();
        cell.cursor_pointer()
            .hover(|el| el.bg(theme.list_hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.open_project_attention(
                    root.clone(),
                    worktree.clone(),
                    conversation.clone(),
                    provider,
                    window,
                    cx,
                );
            }))
            .into_any_element()
    }

    /// The collapsed sidebar, a rail like the right one: squares with a
    /// line under each, the selection a background, names beside them at
    /// once. Expand first, then each project's initials and, under the
    /// active one, its worktrees' (initials take the color of an agent
    /// signal, gray otherwise), then Add a project. The app menu last.
    pub(super) fn render_sidebar_rail(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let roots = self.persisted.projects.roots.clone();
        let mut cells: Vec<AnyElement> = Vec::new();
        for (ix, root) in roots.iter().enumerate() {
            let active = self.active_root() == Some(root.as_path());
            let signal = Signal::of_project(self.project_activity(root, cx));
            let name = project_name(root);
            let color = match &signal {
                Some(signal) => attention_color(signal.kind, &theme),
                None if active => theme.foreground,
                None => theme.muted_foreground,
            };
            let target = root.clone();
            let square = rail_square(("rail-project", ix), theme.sidebar_border)
                .hover(|el| el.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.click_project(target.clone(), cx)))
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color)
                        .child(initials(&name)),
                );
            cells.push(
                rail_label(
                    ("rail-project-label", ix),
                    name.into(),
                    self.project_menu(square, root.clone(), cx),
                    LabelSide::Right,
                    &theme,
                )
                .into_any_element(),
            );
            if active && !self.persisted.projects.is_folded(root) {
                cells.extend(self.render_rail_worktrees(&theme, cx));
            }
        }
        // Expand tops the rail, the app menu sits at its foot; adding a
        // project ends the list, under the projects.
        let (expand, add, menu) = {
            let expand = rail_square("expand-sidebar", theme.sidebar_border)
                .hover(|el| el.bg(theme.list_hover))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.toggle_sidebar(&ToggleSidebar, window, cx)
                }))
                .child(
                    Icon::new(IconName::PanelLeftOpen).with_size(gpui_kit::component::Size::Medium),
                );
            let add = self
                .add_project_button(
                    square_button("add-project-rail", IconName::Plus)
                        .w_full()
                        .border_b_1()
                        .border_color(theme.sidebar_border),
                    cx,
                )
                .into_any_element();
            let menu = self
                .app_menu_button(
                    square_button("app-menu-rail", IconName::Ellipsis)
                        .w_full()
                        .border_t_1()
                        .border_color(theme.sidebar_border),
                    cx,
                )
                .into_any_element();
            let label = |id: &'static str, text: SharedString, item: AnyElement| {
                rail_label(id, text, item, LabelSide::Right, &theme).into_any_element()
            };
            // macOS: the window buttons sit right of this square (the rail is
            // too narrow for them), so its label opens past them.
            let expand = rail_label(
                "expand-sidebar-label",
                format!("Expand the sidebar ({MOD}-b)").into(),
                expand,
                if self.window_buttons_beside_rail(window) {
                    LabelSide::RightPastWindowButtons
                } else {
                    LabelSide::Right
                },
                &theme,
            )
            .into_any_element();
            (
                expand,
                label(
                    "add-project-rail-label",
                    format!("Add a project ({MOD}-o)").into(),
                    add,
                ),
                label("app-menu-rail-label", "More actions".into(), menu),
            )
        };
        cells.push(add);
        v_flex()
            .w(px(RAIL_WIDTH))
            .flex_none()
            .h_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(expand)
            .child(
                v_flex()
                    .id("sidebar-rail")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(cells),
            )
            .child(menu)
            .into_any_element()
    }

    /// "⋯": the window-wide actions that used to sit in the title bar —
    /// conversations needing attention, the switcher, repositories, the
    /// terminal panel, appearance and refresh.
    fn app_menu_button(&self, button: Button, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = cx.entity().downgrade();
        let attention: Vec<(Kind, usize)> = ATTENTION_ORDER
            .into_iter()
            .map(|kind| (kind, self.attention_count(kind, cx)))
            .filter(|(_, count)| *count > 0)
            .collect();
        let has_repo = self.repo.is_some();
        button.dropdown_menu(move |mut menu, window, cx| {
            // Each opens the exact conversation; repeat to cycle.
            for &(kind, count) in &attention {
                let owner = owner.clone();
                let label = format!("{count} {}", kind.label());
                menu = menu.item(
                    PopupMenuItem::element(move |_, cx| {
                        div()
                            .text_color(attention_color(kind, cx.theme()))
                            .child(label.clone())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = owner.update(cx, |this, cx| this.open_attention(kind, window, cx));
                    }),
                );
            }
            menu = menu.separator().item(app_item(
                &owner,
                "Switch Worktree, Conversation or Project…",
                Box::new(OpenSwitcher),
                |this, window, cx| this.open_switcher(&OpenSwitcher, window, cx),
            ));
            menu = menu.separator().item(app_item(
                &owner,
                "Open Repository…",
                Box::new(OpenRepository),
                |this, window, cx| this.open_repository(&OpenRepository, window, cx),
            ));
            if has_repo {
                let clone_owner = owner.clone();
                menu = menu
                    .item(
                        PopupMenuItem::new("Clone Repository…").on_click(move |_, window, cx| {
                            let _ =
                                clone_owner.update(cx, |this, cx| this.open_clone_form(window, cx));
                        }),
                    )
                    .item(app_item(
                        &owner,
                        "Recent Repositories",
                        Box::new(CloseRepository),
                        |this, window, cx| this.close_repository(&CloseRepository, window, cx),
                    ));
            }
            if has_repo {
                menu = menu.separator().item(app_item(
                    &owner,
                    "Toggle Terminal",
                    Box::new(ToggleTerminal),
                    |this, window, cx| this.toggle_terminal(&ToggleTerminal, window, cx),
                ));
            }
            menu.separator()
                .submenu("Appearance", window, cx, |mut menu, _, cx| {
                    let now = crate::appearance::current(cx);
                    for choice in [
                        crate::settings::Appearance::System,
                        crate::settings::Appearance::Light,
                        crate::settings::Appearance::Dark,
                    ] {
                        menu = menu.item(
                            PopupMenuItem::new(choice.label())
                                .checked(now == choice)
                                .on_click(move |_, _, cx| crate::appearance::set(choice, cx)),
                        );
                    }
                    menu
                })
                .item(app_item(
                    &owner,
                    "Refresh",
                    Box::new(Refresh),
                    |this, window, cx| this.refresh(&Refresh, window, cx),
                ))
        })
    }

    /// The active project's worktrees in the rail: their initials, smaller
    /// than a project's, in their signal's color (the name beside on hover);
    /// completed ones while their group is open.
    fn render_rail_worktrees(
        &self,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let scope = self.active_scope();
        self.worktrees
            .iter()
            .enumerate()
            .filter(|(_, info)| {
                !scope.view.completed_collapsed || !self.completed_in(scope, info, cx)
            })
            .map(|(ix, info)| {
                let selected = self.selected.as_deref() == Some(info.path());
                let path = info.wt.path.clone();
                let name = info.wt.display_name();
                let color = self
                    .worktree_signal(scope, info, cx)
                    .map_or(theme.muted_foreground, |signal| {
                        attention_color(signal.kind, theme)
                    });
                let square = rail_square(("rail-worktree", ix), theme.sidebar_border)
                    .when(selected, |el| el.bg(theme.sidebar_accent))
                    .hover(|el| {
                        el.bg(if selected {
                            theme.sidebar_accent
                        } else {
                            theme.list_hover
                        })
                    })
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.select_worktree(path.clone(), cx)),
                    )
                    .child(div().text_xs().text_color(color).child(initials(&name)));
                rail_label(
                    ("rail-worktree-label", ix),
                    name.into(),
                    square,
                    LabelSide::Right,
                    theme,
                )
                .into_any_element()
            })
            .collect()
    }
}

/// The icon of a signal that wants the user, so that those states read apart
/// without their colors; an agent that only works is a dot.
fn signal_icon(kind: Kind) -> Option<IconName> {
    match kind {
        Kind::Approval => Some(IconName::TriangleAlert),
        Kind::Question => Some(IconName::CircleQuestionMark),
        Kind::Completed => Some(IconName::Check),
        Kind::Failed => Some(IconName::CircleX),
        Kind::Running | Kind::Idle => None,
    }
}

/// An item of the "⋯" menu: runs on the app and shows the action's shortcut.
fn app_item(
    owner: &WeakEntity<SuperGitApp>,
    label: &'static str,
    action: Box<dyn Action>,
    run: impl Fn(&mut SuperGitApp, &mut Window, &mut Context<SuperGitApp>) + 'static,
) -> PopupMenuItem {
    let owner = owner.clone();
    PopupMenuItem::new(label)
        .action(action)
        .on_click(move |_, window, cx| {
            let _ = owner.update(cx, |this, cx| run(this, window, cx));
        })
}

/// macOS outside full screen: the window buttons sit in the panels' first
/// row, near the window's left edge.
fn window_buttons_shown(window: &Window) -> bool {
    FULL_HEIGHT_PANELS && !window.is_fullscreen()
}

/// A one-line note under a project (loading, an error).
fn note(text: String, color: Hsla) -> AnyElement {
    div()
        .pl(px(NAME_START))
        .pr_3()
        .py_2()
        .text_xs()
        .text_color(color)
        .child(text)
        .into_any_element()
}

/// A conversation as the tree lists it (see `agents_in`).
struct AgentEntry {
    provider: Provider,
    title: String,
    local_id: String,
    kind: Kind,
    unread: bool,
    /// Busy with a turn here, or in another process.
    working: bool,
    /// The one shown under the worktree's header.
    on_screen: bool,
    /// On screen and picked last in the tree: the tree's selection.
    selected: bool,
    session_id: Option<String>,
    cwd: PathBuf,
    transcript: Option<PathBuf>,
    can_fork: bool,
}
