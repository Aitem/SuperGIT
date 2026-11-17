//! Reply to a review thread or change its resolution from the PR conversation.

use gpui_kit::component::input::{Textarea, TextareaState};
use sg_github::ReviewThreadState;

use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Target {
    root: PathBuf,
    number: u64,
    thread: String,
}

struct ReplyDraft {
    input: Entity<TextareaState>,
    _subscription: Subscription,
}

enum Action {
    Reply(String),
    Resolve(bool),
}

enum Updated {
    Reply(Comment),
    Thread(ReviewThreadState),
}

struct Pending {
    target: Target,
    reply: bool,
    _task: Task<()>,
}

#[derive(Default)]
pub(super) struct ThreadActions {
    drafts: HashMap<Target, ReplyDraft>,
    errors: HashMap<Target, String>,
    pending: Option<Pending>,
}

impl SuperGitApp {
    fn fresh_review_thread(&self, target: &Target) -> Option<&ReviewThread> {
        if self.active_root() != Some(target.root.as_path())
            || self.pulls.shown != Some(target.number)
        {
            return None;
        }
        let loaded = self.pulls.details.get(&target.number)?;
        let LoadState::Ready(details) = &loaded.details else {
            return None;
        };
        details
            .threads
            .iter()
            .find(|thread| thread.id == target.thread)
    }

    fn open_thread_reply(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .fresh_review_thread(&target)
            .is_some_and(|thread| thread.permissions.viewer_can_reply)
        {
            return;
        }
        let draft = self
            .pulls
            .thread_actions
            .drafts
            .entry(target)
            .or_insert_with(|| {
                let input = cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .placeholder("Write a reply…")
                        .rows(3)
                });
                let subscription = cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                });
                ReplyDraft {
                    input,
                    _subscription: subscription,
                }
            });
        draft.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn send_thread_reply(&mut self, target: Target, cx: &mut Context<Self>) {
        let Some(draft) = self.pulls.thread_actions.drafts.get(&target) else {
            return;
        };
        let reply = draft.input.read(cx).value().trim().to_string();
        if !reply.is_empty() {
            self.update_review_thread(target, Action::Reply(reply), cx);
        }
    }

    fn update_review_thread(&mut self, target: Target, action: Action, cx: &mut Context<Self>) {
        if self.pulls.thread_actions.pending.is_some() {
            return;
        }
        let Some(thread) = self.fresh_review_thread(&target) else {
            return;
        };
        let allowed = match &action {
            Action::Reply(_) => thread.permissions.viewer_can_reply,
            Action::Resolve(true) => !thread.resolved && thread.permissions.viewer_can_resolve,
            Action::Resolve(false) => thread.resolved && thread.permissions.viewer_can_unresolve,
        };
        if !allowed {
            return;
        }
        self.pulls.thread_actions.errors.remove(&target);
        let request = target.clone();
        let pending = target.clone();
        let reply = matches!(action, Action::Reply(_));
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match action {
                        Action::Reply(reply) => {
                            sg_github::reply_to_thread(&request.root, &request.thread, &reply)
                                .map(Updated::Reply)
                        }
                        Action::Resolve(resolved) => {
                            sg_github::set_thread_resolved(&request.root, &request.thread, resolved)
                                .map(Updated::Thread)
                        }
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.apply_review_thread_update(target, result, cx)
            })
            .ok();
        });
        self.pulls.thread_actions.pending = Some(Pending {
            target: pending,
            reply,
            _task: task,
        });
        cx.notify();
    }

    fn apply_review_thread_update(
        &mut self,
        target: Target,
        result: anyhow::Result<Updated>,
        cx: &mut Context<Self>,
    ) {
        self.pulls.thread_actions.pending = None;
        let here = self.active_root() == Some(target.root.as_path());
        let update = match result {
            Ok(update) => update,
            Err(error) => {
                let error = format!("{error:#}");
                if here && self.pulls.shown == Some(target.number) {
                    self.pulls.thread_actions.errors.insert(target, error);
                } else {
                    self.flash(
                        format!("Review action in PR #{} failed: {error}", target.number),
                        true,
                        cx,
                    );
                }
                cx.notify();
                return;
            }
        };
        let message = match &update {
            Updated::Reply(_) => {
                self.pulls.thread_actions.drafts.remove(&target);
                "Reply sent"
            }
            Updated::Thread(state) if state.is_resolved => "Thread resolved",
            Updated::Thread(_) => "Thread reopened",
        };
        if here {
            if let Some(loaded) = self.pulls.details.get_mut(&target.number) {
                let details = match &mut loaded.details {
                    LoadState::Ready(details) => Some(details),
                    LoadState::Failed { previous, .. } => previous.as_mut(),
                    LoadState::Loading => None,
                };
                if let Some(thread) = details.and_then(|details| {
                    details
                        .threads
                        .iter_mut()
                        .find(|thread| thread.id == target.thread)
                }) {
                    match update {
                        Updated::Reply(comment) => {
                            if !thread.comments.iter().any(|item| item.url == comment.url) {
                                thread.comments.push(comment);
                            }
                        }
                        Updated::Thread(state) => {
                            thread.resolved = state.is_resolved;
                            thread.permissions = state.permissions;
                            if state.is_resolved {
                                self.pulls.toggled_threads.insert(target.thread);
                            } else {
                                self.pulls.toggled_threads.remove(&target.thread);
                            }
                        }
                    }
                }
            }
            if self
                .pulls
                .details_task
                .as_ref()
                .is_some_and(|(number, _)| *number == target.number)
            {
                self.pulls.details_task = None;
            }
            if self.pulls.shown == Some(target.number) {
                self.load_pr_details(target.root, target.number, cx);
            }
        }
        self.flash(message, false, cx);
        cx.notify();
    }

    pub(super) fn render_thread_actions(
        &self,
        pr: &PullRequest,
        ix: usize,
        thread: &ReviewThread,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ask = self.review_context_button(pr, ix, thread, cx);
        let mut bar = h_flex().flex_wrap().justify_end().gap_1().child(ask);
        let Some(root) = self.active_root() else {
            return bar.into_any_element();
        };
        if thread.id.is_empty() {
            return bar.into_any_element();
        }
        let target = Target {
            root: root.to_path_buf(),
            number: pr.number,
            thread: thread.id.clone(),
        };
        let actions = &self.pulls.thread_actions;
        let draft = actions.drafts.get(&target);
        let fresh = self.fresh_review_thread(&target).is_some();
        let busy = actions.pending.is_some();
        let pending_here = actions
            .pending
            .as_ref()
            .filter(|pending| pending.target == target);
        let sending = pending_here.is_some_and(|pending| pending.reply);
        let resolving = pending_here.is_some_and(|pending| !pending.reply);
        if thread.permissions.viewer_can_reply && draft.is_none() {
            let reply_target = target.clone();
            bar = bar.child(
                Button::new(("pr-thread-reply", ix))
                    .label("Reply")
                    .xsmall()
                    .ghost()
                    .disabled(!fresh)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_thread_reply(reply_target.clone(), window, cx)
                    })),
            );
        }
        if (!thread.resolved && thread.permissions.viewer_can_resolve)
            || (thread.resolved && thread.permissions.viewer_can_unresolve)
        {
            let resolve = !thread.resolved;
            let resolve_target = target.clone();
            bar = bar.child(
                Button::new(("pr-thread-resolve", ix))
                    .label(match (resolving, resolve) {
                        (true, true) => "Resolving…",
                        (true, false) => "Reopening…",
                        (false, true) => "Resolve",
                        (false, false) => "Reopen",
                    })
                    .xsmall()
                    .ghost()
                    .disabled(!fresh || busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_review_thread(
                            resolve_target.clone(),
                            Action::Resolve(resolve),
                            cx,
                        )
                    })),
            );
        }
        v_flex()
            .gap_2()
            .child(bar)
            .when_some(actions.errors.get(&target).cloned(), |panel, error| {
                panel.child(div().text_xs().text_color(cx.theme().danger).child(error))
            })
            .when_some(draft, |panel, draft| {
                let empty = draft.input.read(cx).value().trim().is_empty();
                let cancel_target = target.clone();
                panel
                    .child(Textarea::new(&draft.input).disabled(sending))
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_1()
                            .child(
                                Button::new(("pr-thread-reply-cancel", ix))
                                    .label("Cancel")
                                    .xsmall()
                                    .ghost()
                                    .disabled(sending)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.pulls.thread_actions.drafts.remove(&cancel_target);
                                        this.pulls.thread_actions.errors.remove(&cancel_target);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new(("pr-thread-reply-send", ix))
                                    .label(if sending { "Sending…" } else { "Send reply" })
                                    .xsmall()
                                    .primary()
                                    .disabled(
                                        !fresh
                                            || busy
                                            || empty
                                            || !thread.permissions.viewer_can_reply,
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.send_thread_reply(target.clone(), cx)
                                    })),
                            ),
                    )
            })
            .into_any_element()
    }
}
