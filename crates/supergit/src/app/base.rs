//! Base choice and updates stay separate from creating a task.
use super::*;
use gpui_kit::component::menu::DropdownMenu as _;

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

    /// The local branch worktrees rebase onto: the base new worktrees start from.
    pub(super) fn rebase_base(&self) -> Option<String> {
        let info = self.creation_base.value()?;
        info.tip.as_ref()?;
        info.branch.clone()
    }

    /// Why `info` cannot be rebased onto `base` right now.
    fn rebase_blocker(&self, info: &WorktreeInfo, base: &str, cx: &App) -> Option<String> {
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
            return Some("Claude is working here; wait until it finishes".into());
        }
        match info.behind_base {
            Some(0) => Some(format!("already up to date with {base}")),
            Some(_) => None,
            None => Some("checking how far it is behind…".into()),
        }
    }

    /// Rebase a worktree's branch onto the sidebar's base. Conflicts abort the
    /// rebase in `sg_git`, so the branch ends up rebased or untouched.
    pub(super) fn rebase_worktree(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(base) = self.rebase_base() else {
            self.flash(
                "Base is unavailable. Refresh and choose a local branch",
                true,
                cx,
            );
            return;
        };
        let Some(info) = self.worktrees.iter().find(|info| info.path() == path) else {
            return;
        };
        let branch = info
            .wt
            .branch
            .clone()
            .unwrap_or_else(|| info.wt.display_name());
        if let Some(reason) = self.rebase_blocker(info, &base, cx) {
            self.flash(format!("Cannot rebase {branch}: {reason}"), true, cx);
            return;
        }
        let Some(head) = info
            .status
            .as_ref()
            .ok()
            .and_then(|status| status.head_oid.clone())
        else {
            return;
        };
        let Some(root) = self.repo.as_ref().map(|repo| repo.root.clone()) else {
            return;
        };
        self.base_update_paths.insert(path.clone());
        for chat in self.chats.get(&path).into_iter().flatten() {
            chat.update(cx, |chat, cx| chat.set_git_busy(true, cx));
        }
        self.flash(format!("Rebasing {branch} onto {base}…"), false, cx);
        let request = (path.clone(), base.clone());
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let (path, base) = request;
                    sg_git::rebase_onto(&path, &base, &head).map_err(model::git_error_message)
                })
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
                            this.flash(format!("{branch} already contains {base}"), false, cx)
                        }
                        Ok(sg_git::RebaseOutcome::Rebased { commits: 0 }) => {
                            this.flash(format!("{branch} fast-forwarded to {base}"), false, cx)
                        }
                        Ok(sg_git::RebaseOutcome::Rebased { commits }) => this.flash(
                            format!(
                                "Rebased {branch} onto {base}: {commits} commit{} replayed",
                                if commits == 1 { "" } else { "s" }
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

    /// The row's "Rebase" button: every worktree on a branch other than the
    /// base gets one; the tooltip says what it would do or why it cannot.
    pub(super) fn render_rebase_button(
        &self,
        ix: usize,
        info: &WorktreeInfo,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let base = self.rebase_base()?;
        let branch = info.wt.branch.clone()?;
        if branch == base || info.status.is_err() {
            return None;
        }
        let running = self.base_update_paths.contains(info.path());
        let blocker = self.rebase_blocker(info, &base, cx);
        let tooltip = match (&blocker, info.behind_base) {
            _ if running => format!("Rebasing {branch} onto {base}…"),
            (Some(reason), _) => format!("Rebase onto {base}: {reason}"),
            (None, behind) => {
                let behind = behind.unwrap_or_default();
                format!(
                    "Rebase {branch} onto {base} ({behind} new commit{} on {base}). Conflicts abort the rebase and leave the branch unchanged",
                    if behind == 1 { "" } else { "s" }
                )
            }
        };
        let path = info.wt.path.clone();
        Some(
            Button::new(("worktree-rebase", ix))
                .label(if running { "Rebasing…" } else { "Rebase" })
                .xsmall()
                .ghost()
                .tooltip(tooltip)
                .disabled(running || blocker.is_some())
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.rebase_worktree(path.clone(), cx);
                }))
                .into_any_element(),
        )
    }

    pub(super) fn render_creation_base(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let info = self.creation_base.value();
        let branch = info
            .and_then(|info| info.branch.as_deref())
            .unwrap_or("Choose base");
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
                    .justify_between()
                    .gap_1()
                    .child(
                        Button::new("choose-creation-base")
                            .small()
                            .ghost()
                            .flex_1()
                            .min_w_0()
                            .label(format!("Base: {branch} ▾"))
                            .tooltip(format!("New branches start from {branch}"))
                            .disabled(disabled || options.is_empty())
                            .dropdown_menu(move |mut menu, _, _| {
                                for branch in &options {
                                    let branch = branch.clone();
                                    let owner = owner.clone();
                                    menu = menu.item(PopupMenuItem::new(branch.clone()).on_click(
                                        move |_, _, cx| {
                                            owner.update(cx, |this, cx| {
                                                this.choose_creation_base(branch.clone(), cx)
                                            });
                                        },
                                    ));
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
                                "Update {branch}: fetch its upstream, then fast-forward only"
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
