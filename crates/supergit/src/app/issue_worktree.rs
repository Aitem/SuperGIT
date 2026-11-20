//! Create a worktree from a selected commit or a GitHub issue and pinned base.

use sg_github::{Issue, IssueReference};

use super::projects::project_name;
use super::*;

enum Source {
    Issue {
        base_branch: String,
    },
    Commit {
        subject: String,
    },
    ForkPull {
        number: u64,
        subject: String,
        repository: String,
    },
}

pub(super) struct WorktreeDialog {
    root: PathBuf,
    source: Source,
    base_tip: String,
    input: Entity<InputState>,
    issue: Option<Issue>,
    lookup: Option<Task<()>>,
    creating: bool,
    error: Option<String>,
    previous_focus: Option<FocusHandle>,
    _subscription: Subscription,
}

impl WorktreeDialog {
    fn ready_to_create(&self) -> bool {
        self.issue.is_some() || !matches!(self.source, Source::Issue { .. })
    }
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
        self.open_worktree_dialog(root, base_tip, Source::Issue { base_branch }, window, cx);
    }

    pub(super) fn open_commit_worktree(
        &mut self,
        sha: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root().map(Path::to_path_buf) else {
            return;
        };
        let Some(commit) = self
            .commit_details
            .as_ref()
            .map(|details| &details.commit)
            .filter(|commit| commit.sha == sha && self.selected_commit.as_deref() == Some(sha))
        else {
            return;
        };
        let source = Source::Commit {
            subject: commit.subject.clone(),
        };
        self.open_worktree_dialog(root, sha.to_owned(), source, window, cx);
    }

    pub(super) fn open_fork_pr_worktree(
        &mut self,
        pr: &PullRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root().map(Path::to_path_buf) else {
            return;
        };
        let Some(repository) = sg_github::pull_request_repository(pr) else {
            return;
        };
        if pr.head_oid.is_empty() {
            self.flash("Wait for the pull request commit to load", true, cx);
            return;
        }
        let source = Source::ForkPull {
            number: pr.number,
            subject: pr.title.clone(),
            repository,
        };
        self.open_worktree_dialog(root, pr.head_oid.clone(), source, window, cx);
    }

    fn open_worktree_dialog(
        &mut self,
        root: PathBuf,
        base_tip: String,
        source: Source,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.worktree_dialog.is_some() || self.git_operation_running() {
            return;
        }
        let placeholder = match source {
            Source::Issue { .. } => "#42 or a GitHub issue URL",
            Source::Commit { .. } | Source::ForkPull { .. } => "feature/new-branch",
        };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        if let Source::ForkPull { number, .. } = &source {
            input.update(cx, |state, cx| {
                state.set_value(format!("review/pr-{number}"), window, cx)
            });
        }
        let subscription = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.submit_worktree_dialog(window, cx);
            }
        });
        self.worktree_dialog = Some(WorktreeDialog {
            root,
            source,
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

    fn close_worktree_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .worktree_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.creating)
        {
            return;
        }
        if let Some(focus) = self
            .worktree_dialog
            .take()
            .and_then(|dialog| dialog.previous_focus)
        {
            focus.focus(window, cx);
        }
        cx.notify();
    }

    fn submit_worktree_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.worktree_dialog else {
            return;
        };
        if dialog.creating || dialog.lookup.is_some() {
            return;
        }
        let value = dialog.input.read(cx).value().trim().to_string();
        dialog.error = None;
        if dialog.ready_to_create() {
            self.create_worktree_from_dialog(value, window, cx);
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
                    .worktree_dialog
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

    fn create_worktree_from_dialog(
        &mut self,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let busy = self.git_operation_running();
        let Some((repo, dialog)) = self.repo.as_ref().zip(self.worktree_dialog.as_mut()) else {
            return;
        };
        if repo.root != dialog.root || dialog.creating || dialog.lookup.is_some() {
            return;
        }
        if busy {
            dialog.error = Some("Another Git operation is in progress".into());
            cx.notify();
            return;
        }
        let issue = dialog.issue.clone();
        let pull = match &dialog.source {
            Source::ForkPull {
                number, repository, ..
            } => Some((*number, repository.clone())),
            _ => None,
        };
        let pull_number = pull.as_ref().map(|(number, _)| *number);
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
                .spawn(async move { create_at_tip(&request, &target, &branch, &tip, pull) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.action_task = None;
                let here = this.active_root() == Some(root.as_path());
                let dialog = this
                    .worktree_dialog
                    .as_mut()
                    .filter(|dialog| dialog.input == input);
                match result {
                    Ok(()) => {
                        if dialog.is_some() {
                            this.worktree_dialog = None;
                        }
                        if here {
                            this.select_worktree(path.clone(), cx);
                            if let Some(number) = pull_number {
                                this.set_pr_link(path.clone(), number, true, cx);
                            }
                            if this.selected.as_ref() == Some(&path)
                                && let Some(issue) = &issue
                            {
                                this.new_chat_with(default_provider(), window, cx);
                                if let Some(chat) = this.active_chat_entity() {
                                    chat.update(cx, |chat, cx| {
                                        chat.set_draft(issue_draft(issue), window, cx);
                                    });
                                }
                            }
                        }
                        let message = issue
                            .map(|issue| format!("Created a worktree for issue #{}", issue.number))
                            .or_else(|| {
                                pull_number.map(|number| {
                                    format!("Created a review worktree for PR #{number}")
                                })
                            })
                            .unwrap_or_else(|| {
                                "Created a worktree from the selected commit".into()
                            });
                        this.flash(message, false, cx);
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

    pub(super) fn render_worktree_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.worktree_dialog else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let busy = dialog.lookup.is_some() || dialog.creating;
        let base_tip = &dialog.base_tip[..dialog.base_tip.len().min(8)];
        let (title, origin) = match &dialog.source {
            Source::Issue { base_branch } => (
                "Worktree from issue",
                format!("Base {base_branch} at {base_tip}"),
            ),
            Source::Commit { subject } => {
                ("Worktree from commit", format!("{base_tip} · {subject}"))
            }
            Source::ForkPull {
                number, subject, ..
            } => (
                "Review fork pull request",
                format!("#{number} · {subject} · {base_tip}"),
            ),
        };
        let action = if dialog.creating {
            "Creating worktree…"
        } else if dialog.lookup.is_some() {
            "Looking up issue…"
        } else if dialog.ready_to_create() {
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
                    this.close_worktree_dialog(window, cx);
                }),
            )
            .child(
                v_flex()
                    .track_focus(&self.worktree_dialog_handle)
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
                            this.close_worktree_dialog(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(div().text_lg().font_weight(FontWeight::BOLD).child(title))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(format!("{} · {origin}", project_name(&dialog.root),)),
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
                            .child(div().text_sm().child(if dialog.ready_to_create() {
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
                                        this.close_worktree_dialog(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("submit-issue-worktree")
                                    .label(action)
                                    .small()
                                    .primary()
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_worktree_dialog(window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

fn github_fetch_url(root: &Path, repository: &str) -> anyhow::Result<Option<String>> {
    let names = sg_git::git(root, ["remote"])?;
    // Prefer origin when several remotes point to the repository as seen by GitHub.
    let mut remotes = names.lines().collect::<Vec<_>>();
    remotes.sort_by_key(|remote| *remote != "origin");
    for name in remotes {
        if let Ok(urls) = sg_git::git(root, ["config", "--get-all", &format!("remote.{name}.url")])
            && let Some(url) = urls.lines().next()
            && sg_github::repository_identity(url).as_deref() == Some(repository)
        {
            return Ok(Some(url.to_owned()));
        }
    }
    Ok(None)
}

fn create_at_tip(
    root: &Path,
    path: &Path,
    branch: &str,
    tip: &str,
    pull: Option<(u64, String)>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !branch.is_empty() && !branch.starts_with('-'),
        "Enter a valid new branch name"
    );
    sg_git::git(root, ["check-ref-format", &format!("refs/heads/{branch}")])
        .map_err(|_| anyhow::anyhow!("Invalid Git branch name '{branch}'"))?;
    if let Some((number, repository)) = pull {
        let remote = github_fetch_url(root, &repository)?;
        sg_git::require_pull_commit(root, remote.as_deref(), number, tip)?;
    }
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
