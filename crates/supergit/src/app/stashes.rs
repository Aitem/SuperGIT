use gpui_kit::component::checkbox::Checkbox;
use sg_git::{Stash, WorktreeStatus};

use super::*;

#[derive(Clone)]
struct Snapshot {
    status: WorktreeStatus,
    entries: Vec<Stash>,
}

pub(super) struct StashDialog {
    path: PathBuf,
    message: Entity<InputState>,
    snapshot: LoadState<Snapshot>,
    selected: Option<String>,
    include_untracked: bool,
    loading: Option<Task<()>>,
    running: bool,
    result: Option<(String, bool)>,
    previous_focus: Option<FocusHandle>,
}

enum Action {
    Save {
        message: String,
        include_untracked: bool,
    },
    Apply(String),
}

impl Action {
    fn run(self, path: &Path, status: &WorktreeStatus) -> anyhow::Result<&'static str> {
        match self {
            Self::Save {
                message,
                include_untracked,
            } => {
                sg_git::save_stash(path, status, include_untracked, &message)?;
                Ok("Saved changes to a stash")
            }
            Self::Apply(oid) => {
                sg_git::apply_stash(path, status, &oid)?;
                Ok("Applied the selected stash. Its entry is still available.")
            }
        }
    }
}

impl SuperGitApp {
    pub(super) fn open_stashes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stash_dialog.is_some() || self.git_operation_running() {
            return;
        }
        let Some(path) = self.selected.clone() else {
            return;
        };
        let message =
            cx.new(|cx| InputState::new(window, cx).placeholder("Stash message (optional)"));
        self.stash_dialog = Some(StashDialog {
            path,
            message,
            snapshot: LoadState::Loading,
            selected: None,
            include_untracked: false,
            loading: None,
            running: false,
            result: None,
            previous_focus: window.focused(cx),
        });
        self.stash_handle.focus(window, cx);
        self.load_stashes(cx);
    }

    fn load_stashes(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.stash_dialog else {
            return;
        };
        if dialog.loading.is_some() || dialog.running {
            return;
        }
        let path = dialog.path.clone();
        let key = dialog.message.clone();
        dialog.loading = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    (|| {
                        Ok(Snapshot {
                            status: sg_git::status(&path)?,
                            entries: sg_git::list_stashes(&path)?,
                        })
                    })()
                    .map_err(model::git_error_message)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let Some(dialog) = this
                    .stash_dialog
                    .as_mut()
                    .filter(|dialog| dialog.message == key)
                else {
                    return;
                };
                dialog.loading = None;
                if let Ok(snapshot) = &result
                    && dialog
                        .selected
                        .as_ref()
                        .is_none_or(|oid| !snapshot.entries.iter().any(|entry| entry.oid == *oid))
                {
                    dialog.selected = snapshot.entries.first().map(|stash| stash.oid.clone());
                }
                dialog.snapshot.update(LoadState::from_result(result));
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn close_stashes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .stash_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.running)
        {
            return;
        }
        if let Some(focus) = self
            .stash_dialog
            .take()
            .and_then(|dialog| dialog.previous_focus)
        {
            focus.focus(window, cx);
        }
        cx.notify();
    }

    fn run_stash_action(&mut self, save: bool, cx: &mut Context<Self>) {
        if self.git_operation_running() {
            return;
        }
        let Some(dialog) = &mut self.stash_dialog else {
            return;
        };
        if dialog.running
            || dialog.loading.is_some()
            || dialog.snapshot.error().is_some()
            || self.selected.as_ref() != Some(&dialog.path)
        {
            return;
        }
        let Some(snapshot) = dialog.snapshot.value() else {
            return;
        };
        let action = if save {
            Action::Save {
                message: dialog.message.read(cx).value().to_string(),
                include_untracked: dialog.include_untracked,
            }
        } else {
            let Some(oid) = dialog.selected.clone() else {
                return;
            };
            Action::Apply(oid)
        };
        let status = snapshot.status.clone();
        let path = dialog.path.clone();
        let key = dialog.message.clone();
        dialog.running = true;
        dialog.result = None;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { action.run(&path, &status).map_err(model::git_error_message) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.action_task = None;
                if let Some(dialog) = this
                    .stash_dialog
                    .as_mut()
                    .filter(|dialog| dialog.message == key)
                {
                    dialog.running = false;
                    dialog.result = Some(match result {
                        Ok(caption) => (caption.into(), false),
                        Err(error) => (error, true),
                    });
                    this.load_stashes(cx);
                }
                this.start_polling(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn render_stash_snapshot(
        &self,
        dialog: &StashDialog,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let status = &snapshot.status;
        let busy = dialog.running
            || dialog.loading.is_some()
            || self.git_operation_running()
            || self.selected.as_ref() != Some(&dialog.path)
            || dialog.snapshot.error().is_some();
        let untracked = status.untracked_count();
        let save_ready = sg_git::can_save_stash(status, dialog.include_untracked);
        let dirty_submodule = status.files.iter().any(|file| {
            file.submodule
                .is_some_and(|submodule| submodule.modified || submodule.untracked)
        });
        let apply_ready = status.is_clean() && !status.is_unborn() && dialog.selected.is_some();
        let rows = snapshot.entries.iter().enumerate().map(|(ix, stash)| {
            let oid = stash.oid.clone();
            let active = dialog.selected.as_ref() == Some(&oid);
            h_flex()
                .id(("stash-row", ix))
                .gap_2()
                .p_2()
                .rounded_md()
                .cursor_pointer()
                .hover(|el| el.bg(theme.list_hover))
                .when(active, |el| el.bg(theme.list_active))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(stash.subject.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(stash.reference.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(relative_time(stash.time)),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(dialog) = &mut this.stash_dialog
                        && !dialog.running
                    {
                        dialog.selected = Some(oid.clone());
                        cx.notify();
                    }
                }))
        });
        let muted = theme.muted_foreground;
        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .p_3()
                    .child(
                        h_flex()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Save changes"),
                            )
                            .child(div().text_xs().text_color(muted).child(format!(
                                "{} changed · {} staged · {untracked} untracked",
                                status.files.len(),
                                status.staged_count()
                            ))),
                    )
                    .child(
                        Input::new(&dialog.message)
                            .small()
                            .disabled(busy || status.is_clean()),
                    )
                    .when(dirty_submodule, |el| {
                        el.child(
                            div().text_xs().text_color(muted).child(
                                "Uncommitted edits inside submodules stay in their checkouts.",
                            ),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_between()
                            .child(
                                Checkbox::new("stash-include-untracked")
                                    .label("Include untracked files")
                                    .small()
                                    .checked(dialog.include_untracked)
                                    .disabled(busy || untracked == 0)
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        if let Some(dialog) = &mut this.stash_dialog {
                                            dialog.include_untracked = *checked;
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                Button::new("save-worktree-stash")
                                    .label("Save stash")
                                    .small()
                                    .primary()
                                    .disabled(busy || !save_ready)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.run_stash_action(true, cx)
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .p_3()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Repository stashes"),
                    )
                    .child(
                        v_flex()
                            .id("stash-list")
                            .max_h(px(180.))
                            .overflow_y_scroll()
                            .when(snapshot.entries.is_empty(), |el| {
                                el.child(
                                    div().text_sm().text_color(muted).child("No saved stashes"),
                                )
                            })
                            .children(rows),
                    )
                    .when(!snapshot.entries.is_empty(), |el| {
                        el.child(
                            h_flex()
                                .justify_between()
                                .gap_2()
                                .child(div().text_xs().text_color(muted).child(if apply_ready {
                                    "Restores the staged state and keeps the stash"
                                } else {
                                    "Apply to a clean worktree"
                                }))
                                .child(
                                    Button::new("apply-worktree-stash")
                                        .label("Apply selected")
                                        .small()
                                        .outline()
                                        .disabled(busy || !apply_ready)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.run_stash_action(false, cx)
                                        })),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    pub(super) fn render_stashes(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.stash_dialog else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let snapshot = dialog
            .snapshot
            .value()
            .map(|snapshot| self.render_stash_snapshot(dialog, snapshot, cx));
        let branch = dialog
            .snapshot
            .value()
            .and_then(|snapshot| snapshot.status.branch.as_deref())
            .unwrap_or("Detached HEAD");
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
                cx.listener(|this, _, window, cx| this.close_stashes(window, cx)),
            )
            .child(
                v_flex()
                    .track_focus(&self.stash_handle)
                    .w(px(550.))
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
                            this.close_stashes(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("Stashes"),
                    )
                    .when(dialog.snapshot.value().is_some(), |el| {
                        el.child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(branch.to_owned()),
                        )
                    })
                    .when(dialog.loading.is_some() && snapshot.is_none(), |el| {
                        el.child(div().text_sm().child("Loading stashes…"))
                    })
                    .when_some(dialog.snapshot.error(), |el, error| {
                        el.child(
                            div()
                                .text_sm()
                                .text_color(theme.danger)
                                .child(error.to_owned()),
                        )
                    })
                    .children(snapshot)
                    .when_some(dialog.result.as_ref(), |el, (message, failed)| {
                        el.child(
                            div()
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
                                Button::new("refresh-worktree-stashes")
                                    .label("Refresh")
                                    .small()
                                    .ghost()
                                    .disabled(dialog.running || dialog.loading.is_some())
                                    .on_click(cx.listener(|this, _, _, cx| this.load_stashes(cx))),
                            )
                            .child(
                                Button::new("close-worktree-stashes")
                                    .label("Close")
                                    .small()
                                    .outline()
                                    .disabled(dialog.running)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_stashes(window, cx)
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}
