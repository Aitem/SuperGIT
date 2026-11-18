use sg_git::{CherryPickAction, CherryPickPlan, CherryPickState};

use super::*;

pub(super) struct CherryPickDialog {
    path: PathBuf,
    requested: Option<String>,
    state: LoadState<Option<CherryPickPlan>>,
    loading: Option<Task<()>>,
    running: bool,
    abort_confirm: bool,
    result: Option<(String, bool)>,
    previous_focus: Option<FocusHandle>,
}

impl SuperGitApp {
    pub(super) fn open_cherry_pick(
        &mut self,
        requested: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.git_operation_running() || self.cherry_pick.is_some() {
            return;
        }
        let Some(path) = self.selected.clone() else {
            return;
        };
        if requested
            .as_ref()
            .is_some_and(|sha| self.selected_commit.as_ref() != Some(sha))
        {
            return;
        }
        self.cherry_pick = Some(CherryPickDialog {
            path,
            requested,
            state: LoadState::Loading,
            loading: None,
            running: false,
            abort_confirm: false,
            result: None,
            previous_focus: window.focused(cx),
        });
        self.cherry_pick_handle.focus(window, cx);
        self.load_cherry_pick(cx);
    }

    fn load_cherry_pick(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.cherry_pick else {
            return;
        };
        if dialog.loading.is_some() || dialog.running {
            return;
        }
        dialog.abort_confirm = false;
        let path = dialog.path.clone();
        let requested = dialog.requested.clone();
        dialog.loading = Some(cx.spawn(async move |this, cx| {
            let target = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    sg_git::inspect_cherry_pick(&target, requested.as_deref())
                        .map_err(model::git_error_message)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(dialog) = this
                    .cherry_pick
                    .as_mut()
                    .filter(|dialog| dialog.path == path)
                {
                    dialog.loading = None;
                    dialog.state = LoadState::from_result(result);
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn close_cherry_pick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .cherry_pick
            .as_ref()
            .is_some_and(|dialog| dialog.running)
        {
            return;
        }
        if let Some(focus) = self
            .cherry_pick
            .take()
            .and_then(|dialog| dialog.previous_focus)
        {
            focus.focus(window, cx);
        }
        cx.notify();
    }

    fn run_cherry_pick_action(&mut self, action: CherryPickAction, cx: &mut Context<Self>) {
        if self.git_operation_running() {
            return;
        }
        let Some(dialog) = &mut self.cherry_pick else {
            return;
        };
        if dialog.loading.is_some()
            || dialog.running
            || self.selected.as_ref() != Some(&dialog.path)
            || action == CherryPickAction::Abort && !dialog.abort_confirm
        {
            return;
        }
        let Some(plan) = dialog.state.value().and_then(Option::as_ref).cloned() else {
            return;
        };
        let path = dialog.path.clone();
        dialog.result = None;
        dialog.abort_confirm = false;
        dialog.running = true;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let target = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    sg_git::run_cherry_pick(&target, &plan, action)
                        .map_err(model::git_error_message)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.action_task = None;
                if let Some(dialog) = this
                    .cherry_pick
                    .as_mut()
                    .filter(|dialog| dialog.path == path)
                {
                    dialog.running = false;
                    dialog.result = Some(match result {
                        Ok(()) => {
                            dialog.requested = None;
                            let label = if action == CherryPickAction::Abort {
                                "Cherry-pick aborted"
                            } else {
                                "Commit cherry-picked"
                            };
                            (label.into(), false)
                        }
                        Err(error) => (error, true),
                    });
                    this.load_cherry_pick(cx);
                }
                this.start_polling(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn render_cherry_pick_plan(
        &self,
        dialog: &CherryPickDialog,
        plan: &CherryPickPlan,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let branch = plan.branch.as_deref().unwrap_or("Detached HEAD");
        let busy = dialog.running
            || dialog.loading.is_some()
            || self.git_operation_running()
            || self.selected.as_ref() != Some(&dialog.path);
        let muted = theme.muted_foreground;
        let actions = match plan.state {
            CherryPickState::Preview => h_flex().justify_end().child(
                Button::new("start-cherry-pick")
                    .label("Cherry-pick commit")
                    .small()
                    .primary()
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.run_cherry_pick_action(CherryPickAction::Start, cx)
                    })),
            ),
            CherryPickState::Paused { conflicts } => h_flex()
                .justify_between()
                .gap_2()
                .child(
                    Button::new("review-cherry-pick-changes")
                        .label("Review changes")
                        .small()
                        .ghost()
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.close_cherry_pick(window, cx);
                            this.tab = MainTab::Changes;
                            cx.notify();
                        })),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("abort-cherry-pick")
                                .label(if dialog.abort_confirm {
                                    "Confirm abort"
                                } else {
                                    "Abort…"
                                })
                                .small()
                                .danger()
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if this
                                        .cherry_pick
                                        .as_ref()
                                        .is_some_and(|dialog| dialog.abort_confirm)
                                    {
                                        this.run_cherry_pick_action(CherryPickAction::Abort, cx);
                                    } else if let Some(dialog) = &mut this.cherry_pick {
                                        dialog.abort_confirm = true;
                                        cx.notify();
                                    }
                                })),
                        )
                        .child(
                            Button::new("continue-cherry-pick")
                                .label("Continue")
                                .small()
                                .primary()
                                .disabled(busy || conflicts > 0 || dialog.abort_confirm)
                                .tooltip("Commit the staged conflict resolution")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.run_cherry_pick_action(CherryPickAction::Continue, cx)
                                })),
                        ),
                ),
        };
        v_flex().gap_2().rounded_md().border_1().border_color(theme.border).p_3()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(plan.source.subject.clone()))
            .child(div().text_sm().text_color(muted).child(format!("{} → {branch} at {}", plan.source.short_sha(), plan.destination_short_sha())))
            .when_some(match plan.state { CherryPickState::Paused { conflicts } => Some(conflicts), _ => None }, |el, conflicts| {
                el.child(div().text_sm().text_color(theme.warning).child(if conflicts == 0 {
                    "Cherry-pick paused. Review staged changes before continuing.".to_string()
                } else {
                    let count = if conflicts == 1 { "1 conflicted file".to_owned() } else { format!("{conflicts} conflicted files") };
                    format!("{count}. Resolve and stage the changes to continue.")
                }))
            })
            .when(dialog.abort_confirm, |el| el.child(div().text_sm().text_color(theme.danger)
                .child("Aborting returns to the original branch tip and discards staged conflict resolution edits.")))
            .child(actions)
            .into_any_element()
    }

    pub(super) fn render_cherry_pick(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.cherry_pick else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let plan = dialog
            .state
            .value()
            .and_then(Option::as_ref)
            .map(|value| self.render_cherry_pick_plan(dialog, value, cx));
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(64.))
            .bg(gpui_kit::black().opacity(0.35))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_cherry_pick(window, cx)),
            )
            .child(
                v_flex()
                    .track_focus(&self.cherry_pick_handle)
                    .w(px(540.))
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
                            this.close_cherry_pick(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("Cherry-pick"),
                    )
                    .when(dialog.loading.is_some() && plan.is_none(), |el| {
                        el.child(div().text_sm().child("Checking worktree…"))
                    })
                    .when(
                        dialog.state.value().is_some_and(Option::is_none)
                            && dialog.result.is_none(),
                        |el| {
                            el.child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child("Select a commit in History to cherry-pick it."),
                            )
                        },
                    )
                    .when_some(dialog.state.error(), |el, error| {
                        el.child(
                            div()
                                .text_sm()
                                .text_color(theme.danger)
                                .child(error.to_owned()),
                        )
                    })
                    .children(plan)
                    .when_some(dialog.result.as_ref(), |el, (message, failed)| {
                        el.child(
                            div()
                                .id("cherry-pick-feedback")
                                .max_h(px(130.))
                                .overflow_y_scroll()
                                .text_sm()
                                .text_color(if *failed { theme.danger } else { theme.success })
                                .child(message.clone()),
                        )
                    })
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("refresh-cherry-pick")
                                    .label("Refresh")
                                    .small()
                                    .ghost()
                                    .disabled(dialog.running || dialog.loading.is_some())
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.load_cherry_pick(cx)),
                                    ),
                            )
                            .child(
                                Button::new("close-cherry-pick")
                                    .label("Close")
                                    .small()
                                    .outline()
                                    .disabled(dialog.running)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_cherry_pick(window, cx)
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}
