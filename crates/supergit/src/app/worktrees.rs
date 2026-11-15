//! Worktree organization and cleanup previews.

use super::*;

#[derive(Clone)]
struct CleanupTarget {
    path: PathBuf,
    branch: Option<String>,
    tip: Option<String>,
    reason: Option<String>,
}

pub(super) struct CleanupPreview {
    repository: PathBuf,
    targets: Vec<CleanupTarget>,
    previous_focus: Option<FocusHandle>,
}

impl SuperGitApp {
    pub(super) fn kept_active(&self, path: &Path) -> bool {
        self.repository_view()
            .worktrees
            .get(path)
            .is_some_and(|view| view.kept)
    }

    pub(super) fn worktree_busy(&self, info: &WorktreeInfo, cx: &App) -> bool {
        matches!(
            self.agent_state_for(info, cx),
            Some(AgentState::Working | AgentState::NeedsAttention)
        )
    }

    pub(super) fn worktree_has_draft(&self, path: &Path, cx: &App) -> bool {
        let saved = self.repository_view().conversations.get(path);
        let native = self
            .chats
            .get(path)
            .map(|chats| chats.iter().any(|chat| chat.read(cx).has_draft(cx)))
            .unwrap_or_else(|| {
                saved
                    .into_iter()
                    .flatten()
                    .filter(|chat| chat.provider.is_empty() || chat.provider == "claude")
                    .any(|chat| !chat.draft.is_empty() || !chat.attachments.is_empty())
            });
        native
            || saved
                .into_iter()
                .flatten()
                .filter(|chat| !chat.provider.is_empty() && chat.provider != "claude")
                .any(|chat| !chat.draft.is_empty() || !chat.attachments.is_empty())
    }

    pub(super) fn is_completed(&self, info: &WorktreeInfo, cx: &App) -> bool {
        let pr = self.pr_for_branch(info.wt.branch.as_deref());
        crate::lifecycle::Completion {
            main: info.wt.is_main,
            kept: self.kept_active(info.path()),
            locked: info.wt.locked.is_some(),
            available: !info.wt.is_bare && info.wt.prunable.is_none() && info.status.is_ok(),
            clean: info.status.as_ref().is_ok_and(|status| status.is_clean()),
            busy: self.worktree_busy(info, cx),
            unread: matches!(
                self.native_attention_for(info.path(), cx),
                Some(Kind::Completed | Kind::Failed)
            ),
            pending_draft: self.worktree_has_draft(info.path(), cx),
            prs_fresh: self.prs_error.is_none()
                && self
                    .prs_fetched_at
                    .is_some_and(|time| time.elapsed() < PR_POLL_INTERVAL * 2),
            merged: pr.is_some_and(|pr| pr.state == PrState::Merged),
            local_tip: info
                .status
                .as_ref()
                .ok()
                .and_then(|status| status.head_oid.as_deref()),
            merged_tip: pr.map(|pr| pr.head_oid.as_str()),
        }
        .completed()
    }

    pub(super) fn set_completed_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        if let Some(repo) = &self.repo {
            self.persisted
                .repositories
                .entry(repo.root.clone())
                .or_default()
                .completed_collapsed = collapsed;
            self.queue_persist(cx);
            cx.notify();
        }
    }

    pub(super) fn toggle_keep_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(repo) = &self.repo {
            let view = self
                .persisted
                .repositories
                .entry(repo.root.clone())
                .or_default()
                .worktrees
                .entry(path)
                .or_default();
            view.kept = !view.kept;
            self.queue_persist(cx);
            cx.notify();
        }
    }

    pub(super) fn toggle_keep_worktree(
        &mut self,
        _: &ToggleKeepWorktree,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = self.selected.clone() {
            self.toggle_keep_path(path, cx);
        }
    }

    pub(super) fn preview_cleanup(
        &mut self,
        _: &PreviewCleanup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = self.selected.clone() {
            self.open_cleanup_preview(vec![path], window, cx);
        }
    }

    pub(super) fn open_cleanup_preview(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repo.as_ref().map(|repo| repo.root.clone()) else {
            return;
        };
        self.close_switcher(&CloseSwitcher, window, cx);
        let targets = paths
            .iter()
            .filter_map(|path| self.worktrees.iter().find(|info| info.path() == path))
            .map(|info| CleanupTarget {
                path: info.wt.path.clone(),
                branch: info.wt.branch.clone(),
                tip: info.wt.head.clone(),
                reason: if info.wt.is_main {
                    Some("The main worktree is protected")
                } else if info.wt.locked.is_some() {
                    Some("This worktree is locked")
                } else if info.wt.prunable.is_some() || info.status.is_err() {
                    Some("Worktree status is unavailable")
                } else if self.kept_active(info.path()) {
                    Some("Kept active")
                } else if self.worktree_has_draft(info.path(), cx) {
                    Some("An unsent conversation draft must be preserved")
                } else if self.worktree_busy(info, cx) {
                    Some("An agent is running or waiting for approval")
                } else if info.status.as_ref().is_ok_and(|status| !status.is_clean()) {
                    Some("Local changes must be preserved")
                } else {
                    None
                }
                .map(str::to_string),
            })
            .collect();
        self.cleanup_preview = Some(CleanupPreview {
            repository,
            targets,
            previous_focus: Some(self.focus_handle.clone()),
        });
        self.cleanup_handle.focus(window, cx);
        cx.notify();
    }

    pub(super) fn close_cleanup_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(preview) = self.cleanup_preview.take() {
            preview
                .previous_focus
                .unwrap_or_else(|| self.focus_handle.clone())
                .focus(window, cx);
            cx.notify();
        }
    }

    pub(super) fn render_cleanup_preview(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(preview) = &self.cleanup_preview else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
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
                cx.listener(|this, _, window, cx| this.close_cleanup_preview(window, cx)),
            )
            .child(
                v_flex()
                    .track_focus(&self.cleanup_handle)
                    .w(px(560.))
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
                            this.close_cleanup_preview(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("Cleanup preview"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(preview.repository.display().to_string()),
                    )
                    .child(
                        v_flex()
                            .id("cleanup-targets")
                            .max_h(px(360.))
                            .overflow_y_scroll()
                            .gap_3()
                            .children(preview.targets.iter().map(|target| {
                                v_flex()
                                    .gap_1()
                                    .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                                            "Local branch: {} · {}",
                                            target.branch.as_deref().unwrap_or("Detached HEAD"),
                                            target
                                                .tip
                                                .as_deref()
                                                .map(|tip| &tip[..tip.len().min(8)])
                                                .unwrap_or("unknown")
                                        )))
                                    .child(div().text_sm().child(target.path.display().to_string()))
                                    .when_some(target.reason.clone(), |row, reason| {
                                        row.child(
                                            div().text_sm().text_color(theme.warning).child(reason),
                                        )
                                    })
                            })),
                    )
                    .child(
                        Button::new("close-cleanup-preview")
                            .label("Done")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_cleanup_preview(window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }
}
