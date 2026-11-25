use std::sync::Arc;

use gpui_kit::component::input::{Textarea, TextareaState};
use sg_git::CommitPreview;

use super::*;

pub(super) struct StagedCommit {
    path: PathBuf,
    message: Entity<TextareaState>,
    preview: LoadState<Arc<CommitPreview>>,
    loading: Option<Task<()>>,
    submitting: bool,
    error: Option<String>,
    previous_focus: Option<FocusHandle>,
    _message_subscription: Subscription,
}

impl SuperGitApp {
    pub(super) fn commit_staged_button(&self, cx: &mut Context<Self>) -> Option<Button> {
        let path = self.selected.as_ref()?;
        let status = self.details.status.value()?;
        if self.comparison_base().is_some()
            || self.details.status.error().is_some()
            || self.details.path != *path
            || status.staged_count() == 0
        {
            return None;
        }
        let blocked = self.action_task.is_some()
            || self.cleanup_task.is_some()
            || self.base_update_paths.contains(path);
        Some(
            Button::new("commit-staged")
                .label(format!("Commit staged · {}", status.staged_count()))
                .xsmall()
                .outline()
                .disabled(blocked)
                .tooltip(if blocked {
                    "Wait for the running Git operation to finish"
                } else {
                    "Preview the staged changes and record them with a message"
                })
                .on_click(cx.listener(|this, _, window, cx| this.open_staged_commit(window, cx))),
        )
    }

    fn open_staged_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        let message = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Commit message")
                .rows(3)
        });
        let subscription = cx.subscribe(&message, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        self.staged_commit = Some(StagedCommit {
            path,
            message: message.clone(),
            preview: LoadState::Loading,
            loading: None,
            submitting: false,
            error: None,
            previous_focus: window.focused(cx),
            _message_subscription: subscription,
        });
        self.reload_staged_commit(cx);
        message.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    fn reload_staged_commit(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.staged_commit else {
            return;
        };
        if dialog.submitting || dialog.loading.is_some() {
            return;
        }
        dialog.error = None;
        dialog.preview = LoadState::Loading;
        let message = dialog.message.clone();
        let path = dialog.path.clone();
        dialog.loading = Some(cx.spawn(async move |this, cx| {
            let preview = cx
                .background_executor()
                .spawn(
                    async move { sg_git::preview_commit(&path).map_err(model::git_error_message) },
                )
                .await;
            this.update(cx, |this, cx| {
                if let Some(dialog) = this
                    .staged_commit
                    .as_mut()
                    .filter(|dialog| dialog.message == message)
                {
                    dialog.loading = None;
                    dialog.preview = LoadState::from_result(preview.map(Arc::new));
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn close_staged_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .staged_commit
            .as_ref()
            .is_some_and(|dialog| dialog.submitting)
        {
            return;
        }
        if let Some(focus) = self
            .staged_commit
            .take()
            .and_then(|dialog| dialog.previous_focus)
        {
            focus.focus(window, cx);
        }
        cx.notify();
    }

    fn submit_staged_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.staged_commit else {
            return;
        };
        if self.selected.as_ref() != Some(&dialog.path)
            || dialog.submitting
            || dialog.error.is_some()
        {
            return;
        }
        if self.action_task.is_some()
            || self.cleanup_task.is_some()
            || self.base_update_paths.contains(&dialog.path)
        {
            self.flash("Another Git operation is in progress", true, cx);
            return;
        }
        let message = dialog.message.read(cx).value().trim().to_string();
        if message.is_empty() {
            return;
        }
        let Some(preview) = dialog.preview.value().cloned() else {
            return;
        };
        let path = dialog.path.clone();
        let input = dialog.message.clone();
        dialog.submitting = true;
        self.action_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    sg_git::commit_staged(&path, &preview, &message)
                        .map_err(model::git_error_message)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.action_task = None;
                let dialog = this
                    .staged_commit
                    .as_mut()
                    .filter(|dialog| dialog.message == input);
                match result {
                    Ok(()) => {
                        if let Some(dialog) = dialog {
                            dialog.submitting = false;
                            this.close_staged_commit(window, cx);
                        }
                        this.flash("Created commit", false, cx);
                        this.start_polling(cx);
                    }
                    Err(error) => {
                        if let Some(dialog) = dialog {
                            dialog.submitting = false;
                            dialog.error = Some(error);
                        } else {
                            this.flash(error, true, cx);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub(super) fn render_staged_commit(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.staged_commit else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let can_commit = self.selected.as_ref() == Some(&dialog.path)
            && !dialog.submitting
            && dialog.error.is_none()
            && dialog.preview.value().is_some()
            && !dialog.message.read(cx).value().trim().is_empty();
        let commit_tooltip = if dialog.submitting {
            "Wait for the commit to finish"
        } else if self.selected.as_ref() != Some(&dialog.path) {
            "Select this worktree again to commit here"
        } else if dialog.error.is_some() || dialog.preview.error().is_some() {
            "Reload the preview to try again"
        } else if dialog.preview.value().is_none() {
            "Wait for the preview to load"
        } else if dialog.message.read(cx).value().trim().is_empty() {
            "Write a commit message first"
        } else {
            "Record exactly the staged changes shown above as a new commit"
        };
        let preview = if let Some(snapshot) = dialog.preview.value() {
            let branch = snapshot.branch().unwrap_or("Detached HEAD");
            let head = snapshot
                .head()
                .map_or("initial commit", |head| &head[..head.len().min(8)]);
            v_flex()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "{branch} · {head} · {} staged {}",
                            snapshot.files,
                            if snapshot.files == 1 { "path" } else { "paths" },
                        )),
                )
                .child(
                    div()
                        .id("commit-staged-stat")
                        .max_h(px(80.))
                        .overflow_y_scroll()
                        .p_2()
                        .bg(theme.muted)
                        .rounded_md()
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .child(snapshot.stat.clone()),
                )
                .child(match &snapshot.patch {
                    Some(patch) => div()
                        .id("commit-staged-patch")
                        .max_h(px(190.))
                        .overflow_y_scroll()
                        .p_2()
                        .border_1()
                        .border_color(theme.border)
                        .rounded_md()
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
                        .child(patch.clone()).into_any_element(),
                    None => div().text_sm().text_color(theme.muted_foreground).child(
                        "Preview exceeds 256 KiB. Review the full patch with git diff --cached.",
                    ).into_any_element(),
                })
        } else {
            v_flex().when(dialog.loading.is_some(), |panel| {
                panel.child(div().child("Loading staged changes…"))
            })
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(48.))
            .p_2()
            .bg(gpui_kit::black().opacity(0.35))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_staged_commit(window, cx)),
            )
            .child(
                v_flex()
                    .track_focus(&self.staged_commit_handle)
                    .w(px(640.))
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
                            this.close_staged_commit(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("Commit staged changes"),
                    )
                    .child(preview)
                    .when_some(
                        dialog
                            .error
                            .as_deref()
                            .or_else(|| dialog.preview.error())
                            .map(str::to_string),
                        |panel, error| {
                            panel.child(div().text_sm().text_color(theme.danger).child(error))
                        },
                    )
                    .child(Textarea::new(&dialog.message).disabled(dialog.submitting))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("reload-staged-commit")
                                    .label("Reload preview")
                                    .small()
                                    .ghost()
                                    .disabled(dialog.submitting || dialog.loading.is_some())
                                    .tooltip(if dialog.submitting {
                                        "Wait for the commit to finish"
                                    } else if dialog.loading.is_some() {
                                        "The staged changes are loading"
                                    } else {
                                        "Read the staged changes again"
                                    })
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.reload_staged_commit(cx)),
                                    ),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new("cancel-staged-commit")
                                    .label("Cancel")
                                    .small()
                                    .ghost()
                                    .disabled(dialog.submitting)
                                    .tooltip(if dialog.submitting {
                                        "Wait for the commit to finish"
                                    } else {
                                        "Close without committing (Esc)"
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_staged_commit(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("confirm-staged-commit")
                                    .label(if dialog.submitting {
                                        "Committing"
                                    } else {
                                        "Commit"
                                    })
                                    .small()
                                    .primary()
                                    .disabled(!can_commit)
                                    .tooltip(commit_tooltip)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_staged_commit(window, cx)
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}
