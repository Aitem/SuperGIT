//! Base choice and updates stay separate from creating a task.
use super::*;
use gpui_kit::component::menu::DropdownMenu as _;

/// Choosing the branch to rebase a worktree onto, previewing it, confirming.
pub(super) struct RebaseDialog {
    path: PathBuf,
    branch: String,
    onto: String,
    /// The PR's base and the sidebar's base, then every other local branch.
    suggested: Vec<String>,
    others: Vec<String>,
    plan: LoadState<sg_git::RebasePlan>,
    plan_task: Option<Task<()>>,
    previous_focus: Option<FocusHandle>,
}

fn plural(count: u32, word: &str) -> String {
    if count == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

impl SuperGitApp {
    pub(super) fn start_base_polling(&mut self, cx: &mut Context<Self>) {
        self.creation_base_task = None;
        if self.repo.is_none() {
            return;
        }
        self.creation_base_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(Some((root, base))) = this.read_with(cx, |this, _| {
                    this.repo.as_ref().map(|repo| {
                        (
                            repo.root.clone(),
                            this.repository_view().base_branch.clone(),
                        )
                    })
                }) else {
                    return;
                };
                let request_root = root.clone();
                let request_base = base.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        sg_git::load_base(&request_root, request_base.as_deref())
                            .map_err(model::git_error_message)
                    })
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if this.repo.as_ref().is_some_and(|repo| repo.root == root)
                            && this.repository_view().base_branch == base
                        {
                            this.creation_base.update(LoadState::from_result(result));
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_secs(30))
                    .await;
            }
        }));
    }

    fn choose_creation_base(&mut self, branch: String, cx: &mut Context<Self>) {
        if self.action_task.is_some() || self.cleanup_task.is_some() {
            return;
        }
        let Some(repo) = &self.repo else {
            return;
        };
        self.persisted
            .repositories
            .entry(repo.root.clone())
            .or_default()
            .base_branch = Some(branch);
        self.creation_base = LoadState::Loading;
        self.base_update_error = None;
        self.queue_persist(cx);
        self.start_base_polling(cx);
        cx.notify();
    }

    pub(super) fn update_creation_base(
        &mut self,
        _: &UpdateBase,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.folder.is_some() {
            self.flash("A folder without Git has no worktree base", true, cx);
            return;
        }
        if self.action_task.is_some() || self.cleanup_task.is_some() {
            self.flash("Another Git operation is in progress", true, cx);
            return;
        }
        let Some(info) = self
            .creation_base
            .value()
            .filter(|_| self.creation_base.error().is_none())
            .cloned()
        else {
            self.flash(
                "Base is unavailable. Refresh and choose a local branch",
                true,
                cx,
            );
            return;
        };
        for checkout in &info.checkouts {
            let safe = self
                .worktrees
                .iter()
                .find(|wt| wt.path() == checkout.path)
                .is_some_and(|wt| {
                    wt.status.as_ref().is_ok_and(|status| status.is_clean())
                        && !self.worktree_busy(wt, cx)
                });
            if !safe {
                self.flash("Base worktree is busy, dirty or unavailable. Finish its Git/agent activity and refresh",true,cx);
                return;
            }
        }
        let label = info.branch.clone().unwrap_or_else(|| "base".into());
        let root = info.root.clone();
        self.base_update_error = None;
        let paths: Vec<_> = info.checkouts.iter().map(|wt| wt.path.clone()).collect();
        self.base_update_paths.extend(paths.iter().cloned());
        for path in &paths {
            for chat in self.chats.get(path).into_iter().flatten() {
                chat.update(cx, |chat, cx| chat.set_git_busy(true, cx));
            }
        }
        self.flash(
            format!("Updating {label} from its configured upstream…"),
            false,
            cx,
        );
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { sg_git::update_base(&info).map_err(model::git_error_message) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.action_task = None;
                for path in paths {
                    this.base_update_paths.remove(&path);
                    for chat in this.chats.get(&path).into_iter().flatten() {
                        chat.update(cx, |chat, cx| chat.set_git_busy(false, cx));
                    }
                }
                if this.repo.as_ref().is_some_and(|repo| repo.root == root) {
                    match result {
                        Ok(()) => this.flash(format!("{label} is up to date"), false, cx),
                        Err(error) => {
                            this.base_update_error = Some(error.clone());
                            this.flash(format!("Update {label} stopped: {error}"), true, cx);
                        }
                    }
                }
                this.start_polling(cx);
            });
        }));
    }

    /// The sidebar's base branch, when it has a commit.
    fn creation_branch(&self) -> Option<String> {
        let info = self.creation_base.value()?;
        info.tip.as_ref()?;
        info.branch.clone()
    }

    /// Why `info` cannot be rebased right now.
    fn rebase_blocker(&self, info: &WorktreeInfo, cx: &App) -> Option<String> {
        let Ok(status) = &info.status else {
            return Some("its status is unavailable".into());
        };
        if self.action_task.is_some() || self.cleanup_task.is_some() {
            return Some("another Git operation is in progress".into());
        }
        if self.cleanup_paths.contains(info.path()) || self.base_update_paths.contains(info.path())
        {
            return Some("a Git operation is in progress here".into());
        }
        if status
            .files
            .iter()
            .any(|file| file.unstaged != Some(FileState::Untracked))
        {
            return Some("commit or discard its changes first".into());
        }
        if self.worktree_busy(info, cx) {
            return Some("an agent is working here; wait until it finishes".into());
        }
        None
    }

    /// Ask where to rebase a worktree's branch. Nothing changes until the
    /// user picks a branch, reads the preview and confirms.
    pub(super) fn open_rebase_dialog(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(info) = self.worktrees.iter().find(|info| info.path() == path) else {
            return;
        };
        let Some(branch) = info.wt.branch.clone() else {
            self.flash(
                "HEAD is detached; check out a branch before rebasing",
                true,
                cx,
            );
            return;
        };
        if let Some(reason) = self.rebase_blocker(info, cx) {
            self.flash(format!("Cannot rebase {branch}: {reason}"), true, cx);
            return;
        }
        let branches = self
            .creation_base
            .value()
            .map(|base| base.branches.clone())
            .unwrap_or_default();
        let mut suggested: Vec<String> = Vec::new();
        let pr_base = self
            .pr_for_branch(Some(&branch))
            .map(|pr| pr.base_branch.clone());
        for candidate in pr_base.into_iter().chain(self.creation_branch()) {
            if candidate != branch
                && branches.contains(&candidate)
                && !suggested.contains(&candidate)
            {
                suggested.push(candidate);
            }
        }
        let others: Vec<String> = branches
            .into_iter()
            .filter(|name| *name != branch && !suggested.contains(name))
            .collect();
        let Some(onto) = suggested.first().or(others.first()).cloned() else {
            self.flash(
                format!("No other local branch to rebase {branch} onto"),
                true,
                cx,
            );
            return;
        };
        if self.cleanup_preview.is_some() {
            self.close_cleanup_preview(window, cx);
        }
        self.cleanup_results_open = false;
        self.close_switcher(&CloseSwitcher, window, cx);
        self.rebase_dialog = Some(RebaseDialog {
            path,
            branch,
            onto,
            suggested,
            others,
            plan: LoadState::Loading,
            plan_task: None,
            previous_focus: window.focused(cx),
        });
        self.load_rebase_plan(cx);
        self.rebase_handle.focus(window, cx);
        cx.notify();
    }

    fn choose_rebase_target(&mut self, onto: String, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.rebase_dialog
            && dialog.onto != onto
        {
            dialog.onto = onto;
            self.load_rebase_plan(cx);
        }
    }

    fn load_rebase_plan(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.rebase_dialog else {
            return;
        };
        let (path, onto) = (dialog.path.clone(), dialog.onto.clone());
        dialog.plan = LoadState::Loading;
        dialog.plan_task = Some(cx.spawn(async move |this, cx| {
            let request = (path.clone(), onto.clone());
            let result = cx
                .background_executor()
                .spawn(async move {
                    sg_git::plan_rebase(&request.0, &request.1).map_err(model::git_error_message)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(dialog) = &mut this.rebase_dialog
                    && dialog.path == path
                    && dialog.onto == onto
                {
                    dialog.plan = LoadState::from_result(result);
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    pub(super) fn close_rebase_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self.rebase_dialog.take() {
            dialog
                .previous_focus
                .unwrap_or_else(|| self.focus_handle.clone())
                .focus(window, cx);
            cx.notify();
        }
    }

    /// Run the rebase the dialog previewed. `sg_git::rebase` refuses if the
    /// branch or the target moved since, and aborts on conflicts.
    fn confirm_rebase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(plan) = self
            .rebase_dialog
            .as_ref()
            .and_then(|dialog| {
                dialog
                    .plan
                    .value()
                    .filter(|_| dialog.plan.error().is_none())
            })
            .filter(|plan| !plan.is_up_to_date())
            .cloned()
        else {
            return;
        };
        let Some(info) = self
            .worktrees
            .iter()
            .find(|info| info.path() == plan.worktree)
        else {
            return;
        };
        if let Some(reason) = self.rebase_blocker(info, cx) {
            self.flash(format!("Cannot rebase {}: {reason}", plan.branch), true, cx);
            return;
        }
        let Some(root) = self.repo.as_ref().map(|repo| repo.root.clone()) else {
            return;
        };
        self.close_rebase_dialog(window, cx);
        let path = plan.worktree.clone();
        let (branch, onto) = (plan.branch.clone(), plan.onto.clone());
        self.base_update_paths.insert(path.clone());
        for chat in self.chats.get(&path).into_iter().flatten() {
            chat.update(cx, |chat, cx| chat.set_git_busy(true, cx));
        }
        self.flash(format!("Rebasing {branch} onto {onto}…"), false, cx);
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { sg_git::rebase(&plan).map_err(model::git_error_message) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.action_task = None;
                this.base_update_paths.remove(&path);
                for chat in this.chats.get(&path).into_iter().flatten() {
                    chat.update(cx, |chat, cx| chat.set_git_busy(false, cx));
                }
                if this.repo.as_ref().is_some_and(|repo| repo.root == root) {
                    match result {
                        Ok(sg_git::RebaseOutcome::UpToDate) => {
                            this.flash(format!("{branch} already contains {onto}"), false, cx)
                        }
                        Ok(sg_git::RebaseOutcome::Rebased { commits: 0 }) => {
                            this.flash(format!("{branch} moved to {onto}"), false, cx)
                        }
                        Ok(sg_git::RebaseOutcome::Rebased { commits }) => this.flash(
                            format!(
                                "Rebased {branch} onto {onto}: {commits} {} replayed",
                                plural(commits, "commit")
                            ),
                            false,
                            cx,
                        ),
                        Err(error) => {
                            this.flash(format!("Rebase {branch} stopped: {error}"), true, cx)
                        }
                    }
                }
                this.start_polling(cx);
            });
        }));
        cx.notify();
    }

    /// Screenshot builds only: `SUPERGIT_DEBUG_REBASE=<branch>` opens the
    /// Rebase dialog for that branch's worktree once the base is loaded.
    #[cfg(feature = "screenshot")]
    pub(super) fn debug_rebase_once(&mut self, cx: &mut Context<Self>) {
        let Ok(branch) = std::env::var("SUPERGIT_DEBUG_REBASE") else {
            return;
        };
        if self.creation_base.value().is_none()
            || std::env::var_os("SUPERGIT_DEBUG_REBASE_DONE").is_some()
        {
            return;
        }
        let Some(path) = self
            .worktrees
            .iter()
            .find(|info| info.wt.branch.as_deref() == Some(branch.as_str()))
            .map(|info| info.wt.path.clone())
        else {
            return;
        };
        // SAFETY: isolated screenshot builds only, on the main thread.
        unsafe { std::env::set_var("SUPERGIT_DEBUG_REBASE_DONE", "1") };
        let Some(window) = cx.windows().first().copied() else {
            return;
        };
        let this = cx.entity();
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                this.update(cx, |this, cx| this.open_rebase_dialog(path, window, cx));
            });
        });
    }

    /// The worktree menu's rebase item: its label, and whether it opens the
    /// dialog (otherwise the label says why not).
    pub(super) fn rebase_menu_item(&self, info: &WorktreeInfo, cx: &App) -> Option<(String, bool)> {
        let branch = info.wt.branch.as_ref()?;
        if info.status.is_err() || self.creation_branch().as_ref() == Some(branch) {
            return None;
        }
        if self.base_update_paths.contains(info.path()) {
            return Some((format!("Rebasing {branch}…"), false));
        }
        Some(match self.rebase_blocker(info, cx) {
            Some(reason) => (format!("Rebase… ({reason})"), false),
            None => ("Rebase onto…".to_string(), true),
        })
    }

    pub(super) fn render_rebase_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.rebase_dialog else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let branch = dialog.branch.clone();
        let onto = dialog.onto.clone();
        let blocker = self
            .worktrees
            .iter()
            .find(|info| info.path() == dialog.path)
            .and_then(|info| self.rebase_blocker(info, cx));
        let note = |text: String, color: Hsla| div().text_sm().text_color(color).child(text);
        let mut lines: Vec<AnyElement> = Vec::new();
        let mut ready = false;
        match &dialog.plan {
            LoadState::Loading => lines.push(
                note(
                    format!("Comparing {branch} with {onto}…"),
                    theme.muted_foreground,
                )
                .into_any_element(),
            ),
            LoadState::Failed { error, .. } => {
                lines.push(note(error.clone(), theme.danger).into_any_element())
            }
            LoadState::Ready(plan) if plan.is_up_to_date() => lines.push(
                note(
                    format!("{branch} already contains {onto}. Nothing to rebase."),
                    theme.muted_foreground,
                )
                .into_any_element(),
            ),
            LoadState::Ready(plan) => {
                ready = true;
                let behind = format!("{} {}", plan.behind, plural(plan.behind, "commit"));
                lines.push(if plan.replayed == 0 {
                    note(
                        format!(
                            "{branch} has no commits of its own that {onto} lacks: it will simply move to {onto} (+{behind}). Its work is probably merged already."
                        ),
                        theme.warning,
                    )
                    .into_any_element()
                } else {
                    note(
                        format!(
                            "{} {} of {branch} will be replayed on top of {onto}, which has {behind} that {branch} lacks. Replayed commits get new SHAs.",
                            plan.replayed,
                            plural(plan.replayed, "commit")
                        ),
                        theme.foreground,
                    )
                    .into_any_element()
                });
                match (&plan.upstream, plan.upstream_gone) {
                    (Some(upstream), true) => lines.push(
                        note(
                            format!("Its upstream {upstream} is gone (deleted on the remote)."),
                            theme.muted_foreground,
                        )
                        .into_any_element(),
                    ),
                    (Some(upstream), false) => lines.push(
                        note(
                            format!("{branch} is pushed to {upstream}: publishing the rebased branch needs a force push."),
                            theme.warning,
                        )
                        .into_any_element(),
                    ),
                    (None, _) => {}
                }
                lines.push(
                    note(
                        format!(
                            "{}The current tip {} stays in the reflog.",
                            if plan.replayed > 0 {
                                "Conflicts abort the rebase and leave the branch unchanged. "
                            } else {
                                ""
                            },
                            &plan.head[..plan.head.len().min(8)]
                        ),
                        theme.muted_foreground,
                    )
                    .into_any_element(),
                );
            }
        }
        if let Some(reason) = &blocker {
            lines.push(
                note(format!("Cannot rebase now: {reason}."), theme.danger).into_any_element(),
            );
        }
        let confirm_tooltip = match (&blocker, &dialog.plan) {
            (Some(reason), _) => format!("Cannot rebase now: {reason}"),
            (None, LoadState::Loading) => format!("Wait for the comparison with {onto}"),
            (None, LoadState::Failed { .. }) => "The comparison failed: see above".to_string(),
            (None, LoadState::Ready(plan)) if plan.is_up_to_date() => {
                format!("{branch} already contains {onto}")
            }
            (None, LoadState::Ready(plan)) if plan.replayed == 0 => {
                format!("Move {branch} to {onto}")
            }
            (None, LoadState::Ready(_)) => {
                format!("Replay {branch}'s own commits on top of {onto}")
            }
        };
        let owner = cx.entity();
        let suggested = dialog.suggested.clone();
        let others = dialog.others.clone();
        let current = onto.clone();
        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(80.))
            .bg(gpui_kit::black().opacity(0.35))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_rebase_dialog(window, cx)),
            )
            .child(
                v_flex()
                    .track_focus(&self.rebase_handle)
                    .w(px(520.))
                    .max_w_full()
                    .p_4()
                    .gap_3()
                    .rounded_lg()
                    .shadow_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            this.close_rebase_dialog(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child(format!("Rebase {branch}")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(dialog.path.display().to_string()),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().text_sm().child("Onto"))
                            .child(
                                Button::new("rebase-onto")
                                    .small()
                                    .outline()
                                    .label(format!("{onto} ▾"))
                                    .tooltip("The local branch to replay this branch on top of")
                                    .dropdown_menu(move |mut menu, _, _| {
                                        menu = menu.scrollable(true).max_h(px(320.));
                                        let item = |menu: PopupMenu, name: &String| {
                                            let owner = owner.clone();
                                            let name = name.clone();
                                            menu.item(
                                                PopupMenuItem::new(name.clone())
                                                    .checked(name == current)
                                                    .on_click(move |_, _, cx| {
                                                        owner.update(cx, |this, cx| {
                                                            this.choose_rebase_target(
                                                                name.clone(),
                                                                cx,
                                                            )
                                                        });
                                                    }),
                                            )
                                        };
                                        if !suggested.is_empty() {
                                            menu = menu.label("Suggested");
                                            for name in &suggested {
                                                menu = item(menu, name);
                                            }
                                            if !others.is_empty() {
                                                menu = menu.separator().label("Local branches");
                                            }
                                        }
                                        for name in &others {
                                            menu = item(menu, name);
                                        }
                                        menu
                                    }),
                            ),
                    )
                    .child(v_flex().gap_1().children(lines))
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("close-rebase-dialog")
                                    .label("Cancel")
                                    .small()
                                    .ghost()
                                    .tooltip("Close without rebasing (Esc)")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_rebase_dialog(window, cx)
                                    })),
                            )
                            .child(
                                // The target shows in the Onto picker: a long
                                // branch name must not widen the button.
                                Button::new("confirm-rebase")
                                    .label("Rebase")
                                    .small()
                                    .primary()
                                    .disabled(!ready || blocker.is_some())
                                    .tooltip(confirm_tooltip)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm_rebase(window, cx)
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The new worktree dialog's base: the branch new branches start from,
    /// updating it, and what is known of its upstream.
    pub(super) fn render_creation_base(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let info = self.creation_base.value();
        let branch = info
            .and_then(|info| info.branch.as_deref())
            .unwrap_or("Choose base");
        let current = info.and_then(|info| info.branch.clone());
        let options = info.map(|info| info.branches.clone()).unwrap_or_default();
        let owner = cx.entity();
        let disabled = self.action_task.is_some() || self.cleanup_task.is_some();
        let status = if let Some(error) = self.creation_base.error() {
            format!("Base unavailable: {error}")
        } else if let Some(info) = info {
            if info.tip.is_none() {
                "No local base commit; choose an existing branch".into()
            } else if let Some(upstream) = &info.upstream {
                let counts = info
                    .ahead_behind
                    .map(|(ahead, behind)| format!(" · {behind} behind · {ahead} ahead"))
                    .unwrap_or_else(|| " · fetch to compare".into());
                format!(
                    "Last known: {}/{}{}",
                    upstream.remote,
                    upstream.source.trim_start_matches("refs/heads/"),
                    counts
                )
            } else {
                "No tracking branch; creation works, update needs an upstream".into()
            }
        } else {
            "Loading base…".into()
        };
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().child("Base"))
                    .child(
                        Button::new("choose-creation-base")
                            .small()
                            .outline()
                            .label(format!("{branch} ▾"))
                            .tooltip("The branch a new branch starts from")
                            .disabled(disabled || options.is_empty())
                            .dropdown_menu(move |mut menu, _, _| {
                                menu = menu.scrollable(true).max_h(px(320.));
                                for branch in &options {
                                    let branch = branch.clone();
                                    let owner = owner.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(branch.clone())
                                            .checked(current.as_ref() == Some(&branch))
                                            .on_click(move |_, _, cx| {
                                                owner.update(cx, |this, cx| {
                                                    this.choose_creation_base(branch.clone(), cx)
                                                });
                                            }),
                                    );
                                }
                                menu
                            }),
                    )
                    .child(
                        Button::new("update-creation-base")
                            .small()
                            .ghost()
                            .label("Update")
                            .tooltip(format!(
                                "Fetch the upstream of {branch} and fast-forward it"
                            ))
                            .disabled(
                                disabled
                                    || info.is_none_or(|info| {
                                        info.tip.is_none() || info.upstream.is_none()
                                    })
                                    || self.creation_base.error().is_some(),
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.update_creation_base(&UpdateBase, window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(status),
            )
            .when_some(self.base_update_error.clone(), |panel, error| {
                panel.child(
                    div()
                        .id("base-update-error")
                        .max_h(px(90.))
                        .overflow_y_scroll()
                        .text_xs()
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .into_any_element()
    }
}
