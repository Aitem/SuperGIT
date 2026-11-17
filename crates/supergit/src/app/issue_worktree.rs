//! Open an issue in a new local worktree and seed an editable conversation.

use sg_github::{Issue, IssueReference};

use super::projects::project_name;
use super::*;

pub(super) struct IssueWorktree {
    root: PathBuf,
    base_branch: String,
    base_tip: String,
    input: Entity<InputState>,
    issue: Option<Issue>,
    lookup: Option<Task<()>>,
    creating: bool,
    error: Option<String>,
    previous_focus: Option<FocusHandle>,
    _subscription: Subscription,
}

impl SuperGitApp {
    pub(super) fn open_issue_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.active_root().map(Path::to_path_buf) else {
            return;
        };
        let Some(base) = self
            .creation_base
            .value()
            .filter(|base| base.root == root && self.creation_base.error().is_none())
        else {
            self.flash(
                "Wait for the base branch to load, or refresh the project",
                true,
                cx,
            );
            return;
        };
        let (Some(base_branch), Some(base_tip)) = (base.branch.clone(), base.tip.clone()) else {
            self.flash("Choose an existing local base branch first", true, cx);
            return;
        };
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("#42 or a GitHub issue URL"));
        let subscription = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.submit_issue_worktree(window, cx);
            }
        });
        self.issue_worktree = Some(IssueWorktree {
            root,
            base_branch,
            base_tip,
            input: input.clone(),
            issue: None,
            lookup: None,
            creating: false,
            error: None,
            previous_focus: window.focused(cx),
            _subscription: subscription,
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    fn close_issue_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .issue_worktree
            .as_ref()
            .is_some_and(|dialog| dialog.creating)
        {
            return;
        }
        if let Some(focus) = self
            .issue_worktree
            .take()
            .and_then(|dialog| dialog.previous_focus)
        {
            focus.focus(window, cx);
        }
        cx.notify();
    }

    fn submit_issue_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.issue_worktree else {
            return;
        };
        if dialog.creating || dialog.lookup.is_some() {
            return;
        }
        let value = dialog.input.read(cx).value().trim().to_string();
        dialog.error = None;
        if dialog.issue.is_some() {
            self.create_issue_worktree(value, window, cx);
            return;
        }
        let reference = match IssueReference::parse(&value) {
            Ok(reference) => reference,
            Err(error) => {
                dialog.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let root = dialog.root.clone();
        let input = dialog.input.clone();
        dialog.lookup = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { sg_github::view_issue(&root, &reference) })
                .await;
            this.update_in(cx, |this, window, cx| {
                let Some(dialog) = this
                    .issue_worktree
                    .as_mut()
                    .filter(|dialog| dialog.input == input)
                else {
                    return;
                };
                dialog.lookup = None;
                match result {
                    Ok(issue) => {
                        let branch = issue_branch(&issue);
                        dialog.issue = Some(issue);
                        input.update(cx, |state, cx| state.set_value(branch, window, cx));
                    }
                    Err(error) => dialog.error = Some(format!("{error:#}")),
                }
                input.update(cx, |state, cx| state.focus(window, cx));
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn create_issue_worktree(
        &mut self,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((repo, dialog)) = self.repo.as_ref().zip(self.issue_worktree.as_mut()) else {
            return;
        };
        if repo.root != dialog.root || dialog.creating || dialog.lookup.is_some() {
            return;
        }
        if self.action_task.is_some() || self.cleanup_task.is_some() {
            dialog.error = Some("Another Git operation is in progress".into());
            cx.notify();
            return;
        }
        let Some(issue) = dialog.issue.clone() else {
            return;
        };
        let path = model::default_worktree_path(repo, &branch);
        let root = dialog.root.clone();
        let tip = dialog.base_tip.clone();
        let input = dialog.input.clone();
        dialog.creating = true;
        self.action_task = Some(cx.spawn_in(window, async move |this, cx| {
            let target = path.clone();
            let request = root.clone();
            let result = cx
                .background_executor()
                .spawn(async move { create_at_tip(&request, &target, &branch, &tip) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.action_task = None;
                let here = this.active_root() == Some(root.as_path());
                let dialog = this
                    .issue_worktree
                    .as_mut()
                    .filter(|dialog| dialog.input == input);
                match result {
                    Ok(()) => {
                        if dialog.is_some() {
                            this.issue_worktree = None;
                        }
                        if here {
                            this.select_worktree(path.clone(), cx);
                            if this.selected.as_ref() == Some(&path) {
                                this.new_chat(window, cx);
                                if let Some(chat) = this.active_chat_entity() {
                                    chat.read(cx).input().update(cx, |state, cx| {
                                        state.set_value(issue_draft(&issue), window, cx);
                                    });
                                }
                            }
                        }
                        this.flash(
                            format!("Created a worktree for issue #{}", issue.number),
                            false,
                            cx,
                        );
                        this.start_polling(cx);
                    }
                    Err(error) => {
                        let error = format!("{error:#}");
                        if let Some(dialog) = dialog {
                            dialog.creating = false;
                            dialog.error = Some(error);
                            input.update(cx, |state, cx| state.focus(window, cx));
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

    pub(super) fn render_issue_worktree(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.issue_worktree else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let busy = dialog.lookup.is_some() || dialog.creating;
        let base_tip = &dialog.base_tip[..dialog.base_tip.len().min(8)];
        let action = if dialog.creating {
            "Creating worktree…"
        } else if dialog.lookup.is_some() {
            "Looking up issue…"
        } else if dialog.issue.is_some() {
            "Create worktree"
        } else {
            "Look up issue"
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
                cx.listener(|this, _, window, cx| {
                    this.close_issue_worktree(window, cx);
                }),
            )
            .child(
                v_flex()
                    .track_focus(&self.issue_handle)
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
                            this.close_issue_worktree(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("Worktree from issue"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "{} · Base {} at {base_tip}",
                                project_name(&dialog.root),
                                dialog.base_branch
                            )),
                    )
                    .when_some(dialog.issue.as_ref(), |panel, issue| {
                        panel.child(
                            v_flex()
                                .gap_1()
                                .border_1()
                                .border_color(theme.border)
                                .rounded_md()
                                .p_3()
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(format!("#{} {}", issue.number, issue.title)),
                                )
                                .child(div().text_xs().text_color(theme.muted_foreground).child(
                                    format!("{} · {}", issue.state.to_lowercase(), issue.url),
                                ))
                                .when(!issue.body.is_empty(), |card| {
                                    card.child(
                                        div()
                                            .id("issue-description")
                                            .max_h(px(150.))
                                            .overflow_y_scroll()
                                            .text_sm()
                                            .child(issue.body.clone()),
                                    )
                                }),
                        )
                    })
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().child(if dialog.issue.is_some() {
                                "New branch"
                            } else {
                                "Issue number or URL"
                            }))
                            .child(Input::new(&dialog.input).disabled(busy)),
                    )
                    .when_some(dialog.error.clone(), |panel, error| {
                        panel.child(div().text_sm().text_color(theme.danger).child(error))
                    })
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("cancel-issue-worktree")
                                    .label("Cancel")
                                    .small()
                                    .ghost()
                                    .disabled(dialog.creating)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_issue_worktree(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("submit-issue-worktree")
                                    .label(action)
                                    .small()
                                    .primary()
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_issue_worktree(window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

fn create_at_tip(root: &Path, path: &Path, branch: &str, tip: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !branch.is_empty() && !branch.starts_with('-'),
        "Enter a valid new branch name"
    );
    sg_git::git(root, ["check-ref-format", &format!("refs/heads/{branch}")])
        .map_err(|_| anyhow::anyhow!("Invalid Git branch name '{branch}'"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    sg_git::add_worktree(root, path, Some(branch), Some(tip))
}

fn issue_branch(issue: &Issue) -> String {
    let slug: String = issue
        .title
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(12)
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase()
        .chars()
        .take(48)
        .collect();
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        format!("issue/{}", issue.number)
    } else {
        format!("issue/{}-{slug}", issue.number)
    }
}

fn issue_draft(issue: &Issue) -> String {
    let mut draft = format!(
        "GitHub issue #{} · {}\n{}\n\n{}",
        issue.number, issue.title, issue.url, issue.body
    );
    if draft.len() > agent_context::MAX_BYTES {
        draft.truncate(draft.floor_char_boundary(agent_context::MAX_BYTES));
        draft.push_str("\n\n[Description truncated. See the GitHub issue for the full text.]");
    }
    draft
}

#[cfg(test)]
mod tests {
    use super::{Issue, agent_context, issue_branch, issue_draft};

    #[test]
    fn issue_title_produces_editable_branch_and_unicode_safe_context() {
        let mut issue = Issue {
            number: 12,
            title: "🧪 Fix cache / Сбой... AGAIN?".into(),
            body: "🧪".repeat(agent_context::MAX_BYTES),
            state: "OPEN".into(),
            url: "https://github.com/owner/repo/issues/12".into(),
        };
        assert_eq!(issue_branch(&issue), "issue/12-fix-cache-сбой-again");
        let draft = issue_draft(&issue);
        assert!(draft.starts_with("GitHub issue #12"));
        assert!(draft.contains(&issue.url));
        assert!(draft.ends_with("full text.]"));
        issue.title = "🧪".into();
        assert_eq!(issue_branch(&issue), "issue/12");
    }
}
