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
