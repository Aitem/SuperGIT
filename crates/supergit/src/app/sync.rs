use gpui_kit::component::menu::DropdownMenu as _;
use sg_git::{BranchPublication, WorktreeStatus};

use super::*;

struct Preview {
    status: WorktreeStatus,
    remotes: Vec<String>,
    push: Result<BranchPublication, String>,
}

pub(super) struct SyncDialog {
    path: PathBuf,
    preview: LoadState<Preview>,
    remote: Option<String>,
    load: Option<Task<()>>,
    running: bool,
    error: Option<String>,
    feedback: Option<String>,
    previous_focus: Option<FocusHandle>,
}

enum Sync {
    Fetch(String),
    Push(BranchPublication),
}

impl Sync {
    fn caption(&self) -> String {
        match self {
            Self::Fetch(remote) => format!("Fetched {remote}"),
            Self::Push(target) => format!("Pushed {}/{}", target.remote, target.remote_branch),
        }
    }

    fn run(self, path: &Path) -> anyhow::Result<()> {
        match self {
            Self::Fetch(remote) => sg_git::fetch_remote(path, &remote),
            Self::Push(target) => sg_git::publish_branch(path, &target),
        }
    }
}

fn load_preview(path: &Path) -> anyhow::Result<Preview> {
    Ok(Preview {
        status: sg_git::status(path)?,
        remotes: sg_git::git(path, ["remote"])?
            .lines()
            .map(str::to_owned)
            .collect(),
        push: sg_git::branch_publication(path).map_err(model::git_error_message),
    })
}

impl Preview {
    fn preferred_remote(&self) -> Option<String> {
        let tracked = self.status.upstream.as_ref().and_then(|upstream| {
            self.remotes
                .iter()
                .filter(|remote| upstream.starts_with(&format!("{remote}/")))
                .max_by_key(|remote| remote.len())
        });
        tracked
            .or_else(|| self.push.as_ref().ok().map(|push| &push.remote))
            .or_else(|| self.remotes.first())
            .cloned()
    }
}

impl SuperGitApp {
    pub(super) fn open_sync_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.git_operation_running() || self.sync_dialog.is_some() {
            return;
        }
        let Some(path) = self.selected.clone() else {
            return;
        };
        self.sync_dialog = Some(SyncDialog {
            path,
            preview: LoadState::Loading,
            remote: None,
            load: None,
            running: false,
            error: None,
            feedback: None,
            previous_focus: window.focused(cx),
        });
        self.sync_focus.focus(window, cx);
        self.refresh_sync_preview(cx);
    }

    fn refresh_sync_preview(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.sync_dialog else {
            return;
        };
        if dialog.load.is_some() || dialog.running {
            return;
        }
        let path = dialog.path.clone();
        dialog.preview = LoadState::Loading;
        dialog.error = None;
        dialog.load = Some(cx.spawn(async move |this, cx| {
            let target = path.clone();
            let preview = cx
                .background_executor()
                .spawn(async move { load_preview(&target).map_err(model::git_error_message) })
                .await;
            this.update(cx, |this, cx| {
                if let Some(dialog) = this
                    .sync_dialog
                    .as_mut()
                    .filter(|dialog| dialog.path == path)
                {
                    if let Ok(preview) = &preview
                        && dialog
                            .remote
                            .as_ref()
                            .is_none_or(|remote| !preview.remotes.contains(remote))
                    {
                        dialog.remote = preview.preferred_remote();
                    }
                    dialog.load = None;
                    dialog.preview = LoadState::from_result(preview);
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn close_sync_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .sync_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.running)
        {
            return;
        }
        if let Some(focus) = self
            .sync_dialog
            .take()
            .and_then(|dialog| dialog.previous_focus)
        {
            focus.focus(window, cx);
        }
        cx.notify();
    }

    fn run_sync(&mut self, action: Sync, cx: &mut Context<Self>) {
        if self.git_operation_running() {
            return;
        }
        let Some(dialog) = &mut self.sync_dialog else {
            return;
        };
        if self.selected.as_ref() != Some(&dialog.path) || dialog.running || dialog.load.is_some() {
            return;
        }
        let path = dialog.path.clone();
        let message = action.caption();
        dialog.running = true;
        dialog.error = None;
        dialog.feedback = None;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let target = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { action.run(&target).map_err(model::git_error_message) })
                .await;
            this.update(cx, |this, cx| {
                this.action_task = None;
                if let Some(dialog) = this
                    .sync_dialog
                    .as_mut()
                    .filter(|dialog| dialog.path == path)
                {
                    dialog.running = false;
                    match result {
                        Ok(()) => {
                            dialog.feedback = Some(message);
                            this.start_polling(cx);
                            this.refresh_sync_preview(cx);
                        }
                        Err(error) => dialog.error = Some(error),
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_sync_preview(
        &self,
        dialog: &SyncDialog,
        preview: &Preview,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let disabled = dialog.running
            || self.git_operation_running()
            || self.selected.as_ref() != Some(&dialog.path);
        let remotes = preview.remotes.clone();
        let selected = dialog.remote.clone();
        let choice = selected.as_ref().map(|remote| format!("{remote} ▾"));
        let owner = cx.entity().downgrade();
        let status = &preview.status;
        let tracking = status
            .upstream
            .as_ref()
            .map(|upstream| {
                format!(
                    "{upstream} · {} ahead · {} behind",
                    status.ahead, status.behind
                )
            })
            .unwrap_or_else(|| "No tracking branch".into());
        let muted = theme.muted_foreground;
        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div().font_weight(FontWeight::SEMIBOLD).child(
                            status
                                .branch
                                .as_deref()
                                .unwrap_or("Detached HEAD")
                                .to_owned(),
                        ),
                    )
                    .child(div().text_sm().text_color(muted).child(tracking)),
            )
            .child(
                if let (Some(remote), Some(choice)) = (selected.clone(), choice) {
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("choose-sync-remote")
                                .label(choice)
                                .small()
                                .outline()
                                .disabled(disabled || remotes.len() < 2)
                                .dropdown_menu(move |mut menu, _, _| {
                                    for remote in &remotes {
                                        let remote = remote.clone();
                                        let owner = owner.clone();
                                        menu = menu.item(
                                            PopupMenuItem::new(remote.clone())
                                                .checked(selected.as_ref() == Some(&remote))
                                                .on_click(move |_, _, cx| {
                                                    let _ = owner.update(cx, |this, cx| {
                                                        if let Some(dialog) = &mut this.sync_dialog
                                                        {
                                                            dialog.remote = Some(remote.clone());
                                                            cx.notify();
                                                        }
                                                    });
                                                }),
                                        );
                                    }
                                    menu
                                }),
                        )
                        .child(
                            Button::new("fetch-sync-remote")
                                .label("Fetch remote")
                                .small()
                                .outline()
                                .disabled(disabled)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.run_sync(Sync::Fetch(remote.clone()), cx)
                                })),
                        )
                        .into_any_element()
                } else {
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child("This repository has no remotes")
                        .into_any_element()
                },
            )
            .child(match &preview.push {
                Ok(target) => {
                    let label = format!(
                        "{} → {}/{}",
                        &target.head[..target.head.len().min(8)],
                        target.remote,
                        target.remote_branch
                    );
                    let target = target.clone();
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .text_color(muted)
                                .child(label),
                        )
                        .child(
                            Button::new("push-sync-branch")
                                .label("Push branch")
                                .small()
                                .primary()
                                .disabled(disabled)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.run_sync(Sync::Push(target.clone()), cx)
                                })),
                        )
                        .into_any_element()
                }
                Err(error) => div()
                    .text_sm()
                    .text_color(muted)
                    .child(error.clone())
                    .into_any_element(),
            })
            .into_any_element()
    }

    pub(super) fn render_sync_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.sync_dialog else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let inner = match &dialog.preview {
            LoadState::Loading => div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Checking branch and remotes…")
                .into_any_element(),
            LoadState::Failed { error, .. } => div()
                .text_sm()
                .text_color(theme.danger)
                .child(error.clone())
                .into_any_element(),
            LoadState::Ready(preview) => self.render_sync_preview(dialog, preview, cx),
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(64.))
            .bg(gpui_kit::black().opacity(0.35))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_sync_dialog(window, cx)),
            )
            .child(
                v_flex()
                    .track_focus(&self.sync_focus)
                    .w(px(510.))
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
                            this.close_sync_dialog(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("Sync worktree"),
                    )
                    .child(inner)
                    .when(dialog.running, |el| {
                        el.child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("Running Git operation…"),
                        )
                    })
                    .when_some(dialog.error.as_ref(), |el, error| {
                        el.child(
                            div()
                                .text_sm()
                                .text_color(theme.danger)
                                .child(error.clone()),
                        )
                    })
                    .when_some(dialog.feedback.as_ref(), |el, message| {
                        el.child(
                            div()
                                .text_sm()
                                .text_color(theme.success)
                                .child(message.clone()),
                        )
                    })
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("reload-sync-preview")
                                    .label("Refresh preview")
                                    .small()
                                    .ghost()
                                    .disabled(dialog.running || dialog.load.is_some())
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.refresh_sync_preview(cx)),
                                    ),
                            )
                            .child(
                                Button::new("close-sync-dialog")
                                    .label("Close")
                                    .small()
                                    .outline()
                                    .disabled(dialog.running)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_sync_dialog(window, cx)
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}
