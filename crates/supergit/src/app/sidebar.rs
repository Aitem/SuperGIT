//! The sidebar: one tree of the projects open in the window, each with its
//! worktrees. The active project's worktrees are live and carry every action;
//! another project's come from its last snapshot, and a click on one of them
//! opens that project at that worktree.

use super::projects::{ATTENTION_ORDER, Activity, project_name, short_path};
use super::*;
use gpui_kit::component::menu::DropdownMenu as _;

/// How far worktree rows sit under their project's name.
const WORKTREE_INDENT: f32 = 22.;
/// The indent guide's x: under the project's chevron.
const INDENT_GUIDE_X: f32 = 12.;

/// What a status dot shows: the most urgent agent signal of a worktree or a
/// project, in the title bar's colors (needs approval, failed, unread,
/// running).
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
        let header = h_flex()
            .w_full()
            .min_w_0()
            .pr_2()
            .gap_1()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_weight(FontWeight::BOLD)
                    .child("SuperGIT"),
            )
            .child(self.add_project_button("add-project", cx))
            .child(self.app_menu_button("app-menu", cx))
            .child(
                Button::new("collapse-sidebar")
                    .icon(IconName::PanelLeftClose)
                    .xsmall()
                    .ghost()
                    .tooltip(format!("Collapse the sidebar ({MOD}-b)"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_sidebar(&ToggleSidebar, window, cx)
                    })),
            );
        // On macOS the sidebar reaches the window's top edge: its first row
        // is the title bar (window buttons, dragging, double click).
        let header = if FULL_HEIGHT_PANELS {
            TitleBar::new()
                .when(window.is_fullscreen(), |bar| bar.pl(px(0.)))
                .bg(theme.sidebar)
                .border_color(theme.sidebar_border)
                .child(header)
                .into_any_element()
        } else {
            h_flex().pl_3().py_1().child(header).into_any_element()
        };
        v_flex()
            .w_full()
            .min_w_0()
            .h_full()
            .flex_none()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
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
            .when(self.repo.is_some(), |el| {
                el.child(self.render_worktree_creation(cx))
            })
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
        let count = if active {
            (!self.worktrees.is_empty()).then_some(self.worktrees.len())
        } else {
            snapshot.as_ref().map(|(_, worktrees)| worktrees.len())
        };
        let signal = Signal::of_project(self.project_activity(root, cx));
        let tooltip = short_path(root);
        let target = root.to_path_buf();
        let fold_target = root.to_path_buf();
        let row = h_flex()
            .id("project")
            .pl_1()
            .pr_2()
            .h(px(30.))
            .gap_1()
            .cursor_pointer()
            .bg(theme.secondary)
            .border_y_1()
            .border_color(theme.sidebar_border)
            // Neighbouring headers share one line.
            .when(ix > 0, |el| el.mt(px(-1.)))
            .hover(|el| el.bg(theme.sidebar_accent))
            .on_click(cx.listener(move |this, _, _, cx| this.click_project(target.clone(), cx)))
            .child(
                div()
                    .id("project-fold")
                    .flex_none()
                    .p_0p5()
                    .rounded_sm()
                    .hover(|el| el.bg(theme.list_hover))
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
                        .small()
                        .text_color(theme.muted_foreground),
                    ),
            )
            .child(
                Icon::new(IconName::FolderGit2)
                    .small()
                    .text_color(if active {
                        theme.primary
                    } else {
                        theme.muted_foreground
                    }),
            )
            .child(
                div()
                    .id("project-name")
                    .flex_1()
                    .min_w_0()
                    .pl_1()
                    .truncate()
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .child(project_name(root).to_uppercase()),
            )
            .when(signal.is_some(), |row| {
                row.child(self.render_signal_dot("project-signal", signal, root, theme, cx))
            })
            .when_some(count, |row, count| {
                row.child(
                    div()
                        .px_1p5()
                        .rounded_full()
                        .bg(theme.sidebar)
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(count.to_string()),
                )
            });
        let row = self.project_menu(row, root.to_path_buf(), cx);

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
            Some(note(text, color, theme))
        };
        // Element ids repeat across projects; the node keeps them apart.
        v_flex()
            .id(("project-node", ix))
            .child(row)
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
                .child(note(error.clone(), theme.danger, theme))
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
            return note("Loading…".into(), theme.muted_foreground, theme);
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
            if self.completed_in(scope, info, cx) {
                completed_paths.push(info.wt.path.clone());
                if !collapsed {
                    completed.push(self.render_worktree_row(ix, info, scope, root, theme, cx));
                }
            } else {
                open.push(self.render_worktree_row(ix, info, scope, root, theme, cx));
            }
        }
        let completed_count = completed_paths.len();
        let fold_root = root.to_path_buf();
        v_flex()
            .pb_1()
            .children(open)
            .when(completed_count > 0, |list| {
                list.child(
                    h_flex()
                        .id("completed-group")
                        .relative()
                        .pl(px(WORKTREE_INDENT - 2.))
                        .pr_2()
                        .h(px(26.))
                        .gap_1()
                        .cursor_pointer()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .hover(|el| el.bg(theme.sidebar_accent))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_completed_collapsed_in(fold_root.clone(), !collapsed, cx)
                        }))
                        .child(indent_guide(theme))
                        .child(
                            Icon::new(if collapsed {
                                IconName::ChevronRight
                            } else {
                                IconName::ChevronDown
                            })
                            .xsmall()
                            .text_color(theme.muted_foreground),
                        )
                        .child(
                            div()
                                .flex_1()
                                .child(format!("Completed · {completed_count}")),
                        )
                        .when(scope.active, |header| {
                            header.child(
                                Button::new("preview-completed-cleanup")
                                    .label("Clean up…")
                                    .tooltip(
                                        "Clean up completed worktrees and their local branches",
                                    )
                                    .xsmall()
                                    .ghost()
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
    /// only for a running operation or an error.
    fn render_worktree_row(
        &self,
        ix: usize,
        info: &WorktreeInfo,
        scope: Scope<'_>,
        root: &Path,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = scope.active && self.selected.as_deref() == Some(info.path());
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

        let title = h_flex()
            .gap_1()
            .child(self.render_signal_dot("signal", signal, root, theme, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .when(selected, |el| el.font_weight(FontWeight::SEMIBOLD))
                    .child(info.wt.display_name()),
            )
            .when(info.wt.is_main, |el| el.child(tag("main")))
            .when(kept, |el| el.child(tag("kept")))
            .when(changed > 0, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.warning)
                        .child(format!("±{changed}")),
                )
            })
            .when(ahead > 0, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("↑{ahead}")),
                )
            })
            .when(behind > 0, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("↓{behind}")),
                )
            })
            .when_some(pr, |el, pr| {
                el.child(div().flex_none().child(pr_badge(pr, theme)))
            });

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
            .relative()
            .pl(px(WORKTREE_INDENT))
            .pr_2()
            .py_1()
            .cursor_pointer()
            .when(selected, |el| el.bg(theme.sidebar_accent))
            .hover(|el| el.bg(theme.sidebar_accent))
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.active_root() == Some(open_root.as_path()) {
                    this.select_worktree(path.clone(), cx);
                } else {
                    this.open_worktree_in(open_root.clone(), path.clone(), cx);
                }
            }))
            .child(indent_guide(theme))
            .when(selected, |el| {
                el.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left_0()
                        .w(px(2.))
                        .bg(theme.primary),
                )
            })
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
        if !scope.active {
            return row.into_any_element();
        }
        let rebase = self.rebase_menu_item(info, cx);
        let owner = cx.entity().downgrade();
        let context_path = info.wt.path.clone();
        row.context_menu(move |menu, _, _cx| {
            let menu = match &rebase {
                Some((label, enabled)) => {
                    let rebase_owner = owner.clone();
                    let rebase_path = context_path.clone();
                    menu.item(
                        PopupMenuItem::new(label.clone())
                            .disabled(!enabled)
                            .on_click(move |_, window, cx| {
                                let _ = rebase_owner.update(cx, |this, cx| {
                                    this.open_rebase_dialog(rebase_path.clone(), window, cx)
                                });
                            }),
                    )
                    .separator()
                }
                None => menu,
            };
            let keep_owner = owner.clone();
            let keep_path = context_path.clone();
            let cleanup_owner = owner.clone();
            let cleanup_path = context_path.clone();
            menu.item(
                PopupMenuItem::new("Keep active")
                    .checked(kept)
                    .on_click(move |_, _, cx| {
                        let _ = keep_owner
                            .update(cx, |this, cx| this.toggle_keep_path(keep_path.clone(), cx));
                    }),
            )
            .item(
                PopupMenuItem::new("Clean up worktree…").on_click(move |_, window, cx| {
                    let _ = cleanup_owner.update(cx, |this, cx| {
                        this.open_cleanup_preview(vec![cleanup_path.clone()], window, cx)
                    });
                }),
            )
        })
        .into_any_element()
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

    /// The dot before a worktree's name (and in a project's header): gray
    /// when quiet, otherwise the signal's color with its label on hover; a
    /// click opens the conversation behind it.
    fn render_signal_dot(
        &self,
        id: impl Into<ElementId>,
        signal: Option<Signal>,
        root: &Path,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let color = signal
            .as_ref()
            .map_or(theme.border, |signal| attention_color(signal.kind, theme));
        let cell = div()
            .id(id)
            .size(px(16.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .child(div().size(px(8.)).rounded_full().bg(color));
        let Some(signal) = signal else {
            return cell.into_any_element();
        };
        let tooltip = if signal.open.is_some() {
            format!("{} · click to open", signal.label)
        } else {
            signal.label
        };
        let cell = cell.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx));
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

    /// New worktrees go to the active project.
    fn render_worktree_creation(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let project = self.active_root().map(project_name).unwrap_or_default();
        v_flex()
            .p_2()
            .gap_2()
            .border_t_1()
            .border_color(theme.sidebar_border)
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(format!("NEW WORKTREE · {project}")),
            )
            .child(self.render_creation_base(cx))
            .child(Input::new(&self.branch_input).small())
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("create-worktree")
                            .flex_1()
                            .label("Create worktree")
                            .small()
                            .primary()
                            .disabled(self.action_task.is_some() || self.cleanup_task.is_some())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.create_worktree(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("create-from-issue")
                            .label("From issue…")
                            .small()
                            .outline()
                            .disabled(self.action_task.is_some() || self.cleanup_task.is_some())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_issue_worktree(window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    /// The collapsed sidebar: each project's initials and status, the active
    /// project's worktrees under it.
    pub(super) fn render_sidebar_rail(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let roots = self.persisted.projects.roots.clone();
        let mut cells: Vec<AnyElement> = Vec::new();
        for (ix, root) in roots.iter().enumerate() {
            let active = self.active_root() == Some(root.as_path());
            let signal = Signal::of_project(self.project_activity(root, cx));
            let name = project_name(root);
            let tooltip = match &signal {
                Some(signal) => format!("{name} · {}\n{}", signal.label, short_path(root)),
                None => format!("{name}\n{}", short_path(root)),
            };
            let dot = rail_dot(signal.as_ref(), &theme);
            let target = root.clone();
            let cell = v_flex()
                .id(("rail-project", ix))
                .w_full()
                .py_1p5()
                .gap_1()
                .items_center()
                .cursor_pointer()
                .when(ix > 0, |el| {
                    el.border_t_1().border_color(theme.sidebar_border)
                })
                .hover(|el| el.bg(theme.sidebar_accent))
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                .on_click(cx.listener(move |this, _, _, cx| this.click_project(target.clone(), cx)))
                .child(
                    div()
                        .size(px(26.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .border_1()
                        .border_color(if active {
                            theme.primary
                        } else {
                            theme.sidebar_border
                        })
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .child(initials(&name)),
                )
                .child(dot);
            cells.push(self.project_menu(cell, root.clone(), cx));
            if active && !self.persisted.projects.is_folded(root) {
                cells.extend(self.render_rail_worktrees(&theme, cx));
            }
        }
        v_flex()
            .w(px(RAIL_WIDTH))
            .flex_none()
            .h_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            // Room for the window buttons, which a full-height rail is too
            // narrow to hold: they overlap the next column's padding.
            .when(FULL_HEIGHT_PANELS, |rail| {
                rail.child(
                    TitleBar::new()
                        .pl(px(0.))
                        .bg(theme.sidebar)
                        .border_color(theme.sidebar_border),
                )
            })
            .child(
                h_flex().justify_center().py_1().child(
                    Button::new("expand-sidebar")
                        .icon(IconName::PanelLeftOpen)
                        .xsmall()
                        .ghost()
                        .tooltip(format!("Expand the sidebar ({MOD}-b)"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_sidebar(&ToggleSidebar, window, cx)
                        })),
                ),
            )
            .child(
                v_flex()
                    .id("sidebar-rail")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(cells),
            )
            .child(
                v_flex()
                    .items_center()
                    .py_1()
                    .gap_1()
                    .child(self.app_menu_button("app-menu-rail", cx))
                    .child(self.add_project_button("add-project-rail", cx)),
            )
            .into_any_element()
    }

    /// "⋯": the window-wide actions that used to sit in the title bar —
    /// conversations needing attention, the switcher, repositories,
    /// appearance and refresh.
    fn app_menu_button(&self, id: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = cx.entity().downgrade();
        let attention: Vec<(Kind, usize)> = ATTENTION_ORDER
            .into_iter()
            .map(|kind| (kind, self.attention_count(kind, cx)))
            .filter(|(_, count)| *count > 0)
            .collect();
        let has_repo = self.repo.is_some();
        Button::new(id)
            .icon(IconName::Ellipsis)
            .xsmall()
            .ghost()
            .tooltip("More actions")
            .dropdown_menu(move |mut menu, window, cx| {
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
                            let _ =
                                owner.update(cx, |this, cx| this.open_attention(kind, window, cx));
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
                        .item(PopupMenuItem::new("Clone Repository…").on_click(
                            move |_, window, cx| {
                                let _ = clone_owner
                                    .update(cx, |this, cx| this.open_clone_form(window, cx));
                            },
                        ))
                        .item(app_item(
                            &owner,
                            "Recent Repositories",
                            Box::new(CloseRepository),
                            |this, window, cx| this.close_repository(&CloseRepository, window, cx),
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

    /// The active project's worktrees in the rail: initials and agent status
    /// (the name in the tooltip); completed ones while their group is open.
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
                let initials = initials(&name);
                let signal = self.worktree_signal(scope, info, cx);
                let dot = rail_dot(signal.as_ref(), theme);
                let tooltip = match signal {
                    Some(signal) => format!("{name} · {}", signal.label),
                    None => name,
                };
                v_flex()
                    .id(("rail-worktree", ix))
                    .w_full()
                    .py_1()
                    .gap_1()
                    .items_center()
                    .cursor_pointer()
                    .border_l_2()
                    .border_color(if selected {
                        theme.primary
                    } else {
                        transparent_black()
                    })
                    .when(selected, |el| el.bg(theme.sidebar_accent))
                    .hover(|el| el.bg(theme.sidebar_accent))
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.select_worktree(path.clone(), cx)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(initials),
                    )
                    .child(dot)
                    .into_any_element()
            })
            .collect()
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

/// A signal's dot in the rail (the cell's click selects, so no hit area).
fn rail_dot(signal: Option<&Signal>, theme: &gpui_kit::component::Theme) -> AnyElement {
    div()
        .size(px(8.))
        .flex_none()
        .rounded_full()
        .bg(signal.map_or(theme.border, |signal| attention_color(signal.kind, theme)))
        .into_any_element()
}

/// A one-line note under a project (loading, an error).
fn note(text: String, color: Hsla, theme: &gpui_kit::component::Theme) -> AnyElement {
    div()
        .relative()
        .pl(px(WORKTREE_INDENT))
        .pr_3()
        .py_1()
        .text_xs()
        .text_color(color)
        .child(indent_guide(theme))
        .child(text)
        .into_any_element()
}

/// The tree's vertical line beside a project's worktrees, under its chevron.
fn indent_guide(theme: &gpui_kit::component::Theme) -> Div {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left(px(INDENT_GUIDE_X))
        .w(px(1.))
        .bg(theme.sidebar_border)
}
