//! The Projects panel: several repositories in one window. Only the active
//! project polls Git and GitHub; the others keep their last snapshot, so
//! switching back is instant, and their native conversations keep running.

use super::*;
use gpui_kit::component::menu::DropdownMenu as _;

/// What a project showed when it was last active.
pub(super) struct ProjectSnapshot {
    worktrees: Vec<WorktreeInfo>,
    prs: Vec<PullRequest>,
    prs_error: Option<String>,
    prs_loaded: bool,
    prs_fetched_at: Option<std::time::Instant>,
    observed_open_prs: HashMap<u64, worktrees::ObservedPr>,
    cleanup_attempted: HashSet<String>,
    creation_base: LoadState<sg_git::BaseInfo>,
}

/// Native conversation activity across one project's worktrees.
#[derive(Default)]
struct Activity {
    agent: Option<AgentState>,
    /// The most urgent attention, how many conversations have it, and one
    /// of them to open: `(worktree, conversation id)`.
    attention: Option<(Kind, usize, PathBuf, String)>,
}

const ATTENTION_ORDER: [Kind; 4] = [Kind::Approval, Kind::Failed, Kind::Completed, Kind::Running];

fn wants_attention(kind: Kind, state: &crate::attention::Attention) -> bool {
    state.kind == kind && (matches!(kind, Kind::Running | Kind::Approval) || state.unread)
}

/// The repository's folder name.
fn project_name(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string())
}

/// `root` with the home directory shortened to `~`.
fn short_path(root: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if root.starts_with(&home) => match root.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => root.display().to_string(),
        },
        _ => root.display().to_string(),
    }
}

impl SuperGitApp {
    pub(super) fn active_root(&self) -> Option<&Path> {
        self.repo.as_ref().map(|repo| repo.root.as_path())
    }

    /// Switching projects in the middle of a worktree operation would apply
    /// its result to the wrong repository.
    pub(super) fn git_operation_running(&self) -> bool {
        self.action_task.is_some()
            || self.cleanup_task.is_some()
            || !self.base_update_paths.is_empty()
    }

    /// Park the active project's data before another one takes over.
    pub(super) fn stash_project(&mut self) {
        let Some(root) = self.active_root().map(Path::to_path_buf) else {
            return;
        };
        if self.worktrees.is_empty() {
            return;
        }
        self.project_cache.insert(
            root,
            ProjectSnapshot {
                worktrees: std::mem::take(&mut self.worktrees),
                prs: std::mem::take(&mut self.prs),
                prs_error: self.prs_error.take(),
                prs_loaded: self.prs_loaded,
                prs_fetched_at: self.prs_fetched_at,
                observed_open_prs: std::mem::take(&mut self.observed_open_prs),
                cleanup_attempted: std::mem::take(&mut self.cleanup_attempted),
                creation_base: std::mem::replace(&mut self.creation_base, LoadState::Loading),
            },
        );
    }

    /// Show the active project's last snapshot right away (worktree list,
    /// selection, cached diffs); the restarted polling then refreshes it.
    pub(super) fn restore_project(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.active_root().map(Path::to_path_buf) else {
            return;
        };
        let Some(snapshot) = self.project_cache.remove(&root) else {
            return;
        };
        self.prs = snapshot.prs;
        self.prs_error = snapshot.prs_error;
        self.prs_loaded = snapshot.prs_loaded;
        self.prs_fetched_at = snapshot.prs_fetched_at;
        self.observed_open_prs = snapshot.observed_open_prs;
        self.cleanup_attempted = snapshot.cleanup_attempted;
        self.creation_base = snapshot.creation_base;
        self.apply_worktrees(Ok(snapshot.worktrees), cx);
    }

    pub(super) fn switch_project(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if self.active_root() == Some(root.as_path()) {
            return;
        }
        self.open_path(root, cx);
    }

    /// Remove a project from the panel. Its conversations are saved (adding
    /// the project again restores them) and their idle processes stop.
    fn remove_project(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let busy = self.project_chats(&root).any(|(_, chat)| {
            matches!(
                chat.read(cx).status(),
                sg_claude::Status::Working
                    | sg_claude::Status::Starting
                    | sg_claude::Status::NeedsPermission
            )
        });
        if busy {
            self.flash(
                format!(
                    "Claude is working in {} — stop it first",
                    project_name(&root)
                ),
                true,
                cx,
            );
            return;
        }
        let active = self.active_root() == Some(root.as_path());
        if active && self.git_operation_running() {
            self.flash("Wait for the running Git operation first", true, cx);
            return;
        }
        self.persist_workspace(cx);
        let paths: Vec<PathBuf> = self
            .chat_projects
            .iter()
            .filter(|(_, owner)| **owner == root)
            .map(|(path, _)| path.clone())
            .collect();
        for path in &paths {
            self.chats.remove(path);
            self.active_chat.remove(path);
            self.chat_projects.remove(path);
            self.review_views.remove(path);
        }
        if let Some(snapshot) = self.project_cache.remove(&root) {
            for info in &snapshot.worktrees {
                self.review_views.remove(info.path());
            }
        }
        let roots = &mut self.persisted.projects.roots;
        let ix = roots.iter().position(|known| *known == root);
        roots.retain(|known| *known != root);
        if active {
            let next = ix.and_then(|ix| roots.get(ix).or(roots.last()).cloned());
            for info in &self.worktrees {
                self.review_views.remove(info.path());
            }
            if !next.is_some_and(|next| self.open_path(next, cx)) {
                self.leave_project(cx);
            }
        }
        self.queue_persist(cx);
        cx.notify();
    }

    /// Live conversations that belong to `root`'s worktrees.
    fn project_chats<'a>(
        &'a self,
        root: &'a Path,
    ) -> impl Iterator<Item = (&'a PathBuf, &'a Entity<ChatSession>)> + 'a {
        self.chats
            .iter()
            .filter(move |(path, _)| self.chat_projects.get(*path).is_some_and(|r| r == root))
            .flat_map(|(path, chats)| chats.iter().map(move |chat| (path, chat)))
    }

    /// Exact activity of the project's native conversations: live ones, and
    /// saved ones (unread results) for worktrees not opened this run.
    fn project_activity(&self, root: &Path, cx: &App) -> Activity {
        let mut states: Vec<(PathBuf, String, crate::attention::Attention)> = self
            .project_chats(root)
            .map(|(path, chat)| {
                let chat = chat.read(cx);
                (
                    path.clone(),
                    chat.local_id().to_string(),
                    chat.attention().clone(),
                )
            })
            .collect();
        if let Some(view) = self.persisted.repositories.get(root) {
            for (path, saved) in &view.conversations {
                if self.chats.contains_key(path) {
                    continue;
                }
                states.extend(
                    saved
                        .iter()
                        .filter(|chat| chat.provider.is_empty() || chat.provider == "claude")
                        .map(|chat| (path.clone(), chat.local_id.clone(), chat.attention.clone())),
                );
            }
        }
        let agent = self
            .project_chats(root)
            .filter_map(|(_, chat)| match chat.read(cx).status() {
                sg_claude::Status::NeedsPermission => Some(AgentState::NeedsAttention),
                sg_claude::Status::Working | sg_claude::Status::Starting => {
                    Some(AgentState::Working)
                }
                _ => None,
            })
            .min();
        let attention = ATTENTION_ORDER.into_iter().find_map(|kind| {
            let mut matching = states
                .iter()
                .filter(|(_, _, state)| wants_attention(kind, state));
            let (path, id, _) = matching.next()?;
            Some((kind, matching.count() + 1, path.clone(), id.clone()))
        });
        Activity { agent, attention }
    }

    /// Open the conversation behind a project's attention badge.
    fn open_project_attention(
        &mut self,
        root: PathBuf,
        worktree: PathBuf,
        conversation: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_notification(
            notifications::Target {
                repository: root,
                worktree,
                provider: "claude".into(),
                conversation,
            },
            window,
            cx,
        );
    }

    /// The panel is shown once there is a project to list.
    pub(super) fn projects_visible(&self) -> bool {
        self.repo.is_some() || !self.persisted.projects.roots.is_empty()
    }

    /// Width the Projects panel takes from the window.
    pub(super) fn projects_width(&self, cx: &App) -> f32 {
        if !self.projects_visible() {
            0.
        } else if self.persisted.projects.collapsed {
            RAIL_WIDTH
        } else {
            self.projects_layout
                .read(cx)
                .sizes()
                .first()
                .map(|width| width.as_f32())
                .unwrap_or(self.persisted.projects.width)
                .clamp(160., 360.)
        }
    }

    pub(super) fn remember_projects_width(
        &mut self,
        state: &Entity<ResizableState>,
        cx: &mut Context<Self>,
    ) {
        let Some(width) = state.read(cx).sizes().first().map(|width| width.as_f32()) else {
            return;
        };
        self.persisted.projects.width = width;
        self.persisted.projects.sanitize();
        self.queue_persist(cx);
    }

    /// Collapse the Projects panel to a rail of initials, or expand it.
    pub(super) fn toggle_projects(
        &mut self,
        _: &ToggleProjects,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let projects = &mut self.persisted.projects;
        projects.collapsed = !projects.collapsed;
        self.queue_persist(cx);
        cx.notify();
    }

    /// "+": recent repositories not in the panel yet, and the folder dialog.
    fn add_project_button(&self, id: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = cx.entity().downgrade();
        let recent: Vec<PathBuf> = self
            .recent
            .iter()
            .filter(|root| !self.persisted.projects.roots.contains(root))
            .cloned()
            .collect();
        Button::new(id)
            .icon(IconName::Plus)
            .xsmall()
            .ghost()
            .tooltip("Add a project (cmd-o)")
            .dropdown_menu(move |mut menu, _, _| {
                for root in &recent {
                    let owner = owner.clone();
                    let target = root.clone();
                    menu = menu.item(
                        PopupMenuItem::new(format!("{}  {}", project_name(root), short_path(root)))
                            .on_click(move |_, _, cx| {
                                let _ =
                                    owner.update(cx, |this, cx| this.open_path(target.clone(), cx));
                            }),
                    );
                }
                if !recent.is_empty() {
                    menu = menu.separator();
                }
                let owner = owner.clone();
                menu.item(
                    PopupMenuItem::new("Open Folder…").on_click(move |_, window, cx| {
                        let _ = owner.update(cx, |this, cx| {
                            this.open_repository(&OpenRepository, window, cx)
                        });
                    }),
                )
            })
    }

    fn project_menu(
        &self,
        row: Stateful<Div>,
        root: PathBuf,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let owner = cx.entity().downgrade();
        row.context_menu(move |menu, _, _| {
            let reveal = root.clone();
            let remove_owner = owner.clone();
            let remove = root.clone();
            menu.item(
                PopupMenuItem::new("Reveal in Finder").on_click(move |_, _, _| {
                    let _ = launch::reveal_in_finder(&reveal);
                }),
            )
            .item(
                PopupMenuItem::new("Remove from Projects").on_click(move |_, _, cx| {
                    let _ =
                        remove_owner.update(cx, |this, cx| this.remove_project(remove.clone(), cx));
                }),
            )
        })
        .into_any_element()
    }

    pub(super) fn render_projects(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let roots = self.persisted.projects.roots.clone();
        let rows: Vec<AnyElement> = roots
            .iter()
            .enumerate()
            .map(|(ix, root)| {
                let active = self.active_root() == Some(root.as_path());
                let activity = self.project_activity(root, cx);
                let target = root.clone();
                let mut row = v_flex()
                    .id(("project", ix))
                    .px_3()
                    .py_2()
                    .gap_0p5()
                    .cursor_pointer()
                    .border_l_2()
                    .border_color(if active {
                        theme.primary
                    } else {
                        transparent_black()
                    })
                    .when(active, |el| el.bg(theme.sidebar_accent))
                    .hover(|el| el.bg(theme.sidebar_accent))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.switch_project(target.clone(), cx)),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(agent_dot(activity.agent, &theme))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(project_name(root)),
                            )
                            .when(active && !self.worktrees.is_empty(), |el| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(self.worktrees.len().to_string()),
                                )
                            }),
                    )
                    .child(
                        div()
                            .pl_4()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(short_path(root)),
                    );
                if let Some((kind, count, worktree, conversation)) = activity.attention {
                    let root = root.clone();
                    row = row.child(
                        h_flex().pl_3().child(
                            Button::new(("project-attention", ix))
                                .label(if count > 1 {
                                    format!("{count} {}", kind.label())
                                } else {
                                    format!("Claude {}", kind.label())
                                })
                                .text_color(attention_color(kind, &theme))
                                .xsmall()
                                .ghost()
                                .tooltip("Open the conversation")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.open_project_attention(
                                        root.clone(),
                                        worktree.clone(),
                                        conversation.clone(),
                                        window,
                                        cx,
                                    );
                                })),
                        ),
                    );
                }
                self.project_menu(row, root.clone(), cx)
            })
            .collect();

        v_flex()
            .w_full()
            .min_w_0()
            .h_full()
            .flex_none()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .justify_between()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("PROJECTS · {}", roots.len()))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(self.add_project_button("add-project", cx))
                            .child(
                                Button::new("collapse-projects")
                                    .icon(IconName::PanelLeftClose)
                                    .xsmall()
                                    .ghost()
                                    .tooltip("Collapse the project list (cmd-shift-b)")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_projects(&ToggleProjects, window, cx)
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .id("projects")
                    .flex_1()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .into_any_element()
    }

    /// The collapsed project list: initials and a status dot per project.
    pub(super) fn render_projects_rail(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let roots = self.persisted.projects.roots.clone();
        let cells: Vec<AnyElement> = roots
            .iter()
            .enumerate()
            .map(|(ix, root)| {
                let active = self.active_root() == Some(root.as_path());
                let activity = self.project_activity(root, cx);
                let name = project_name(root);
                let tooltip = match &activity.attention {
                    Some((kind, count, _, _)) => {
                        format!("{name} · {count} {}\n{}", kind.label(), short_path(root))
                    }
                    None => format!("{name}\n{}", short_path(root)),
                };
                let dot = match &activity.attention {
                    Some((kind, ..)) => div()
                        .size(px(8.))
                        .flex_none()
                        .rounded_full()
                        .bg(attention_color(*kind, &theme))
                        .into_any_element(),
                    None => agent_dot(activity.agent, &theme).into_any_element(),
                };
                let target = root.clone();
                let cell = v_flex()
                    .id(("rail-project", ix))
                    .w_full()
                    .py_1p5()
                    .gap_1()
                    .items_center()
                    .cursor_pointer()
                    .border_l_2()
                    .border_color(if active {
                        theme.primary
                    } else {
                        transparent_black()
                    })
                    .when(active, |el| el.bg(theme.sidebar_accent))
                    .hover(|el| el.bg(theme.sidebar_accent))
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.switch_project(target.clone(), cx)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(initials(&name)),
                    )
                    .child(dot);
                self.project_menu(cell, root.clone(), cx)
            })
            .collect();
        v_flex()
            .w(px(RAIL_WIDTH))
            .flex_none()
            .h_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex().justify_center().py_1().child(
                    Button::new("expand-projects")
                        .icon(IconName::PanelLeftOpen)
                        .xsmall()
                        .ghost()
                        .tooltip("Expand the project list (cmd-shift-b)")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_projects(&ToggleProjects, window, cx)
                        })),
                ),
            )
            .child(
                v_flex()
                    .id("project-rail")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(cells),
            )
            .child(
                h_flex()
                    .justify_center()
                    .py_1()
                    .child(self.add_project_button("add-project-rail", cx)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's glob import brings its own `test` attribute.
    use super::{project_name, short_path};
    use std::path::{Path, PathBuf};

    #[test]
    fn project_paths_are_shortened_under_home() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        assert_eq!(short_path(&home), "~");
        assert_eq!(short_path(&home.join("src/repo")), "~/src/repo");
        assert_eq!(short_path(Path::new("/opt/repo")), "/opt/repo");
        assert_eq!(project_name(Path::new("/opt/repo")), "repo");
    }
}
