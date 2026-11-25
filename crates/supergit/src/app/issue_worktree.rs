//! Create a worktree: on a branch from the project's base ("+" on a
//! project), from a GitHub issue and that base, from a selected commit, or
//! from a fork's pull request.

use sg_github::{Issue, IssueReference};

use super::projects::project_name;
use super::*;

enum Source {
    /// A new branch from the base the dialog shows; a branch that exists is
    /// checked out as it is.
    Branch,
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
    /// "+" on a project: a worktree on a branch of the user's choice.
    /// Worktrees are created in the active project, so another one opens
    /// first; the dialog shows its base as it loads.
    pub(super) fn open_new_worktree(
        &mut self,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.worktree_dialog.is_some() {
            return;
        }
        if self.git_operation_running() {
            self.flash("Wait for the running Git operation first", true, cx);
            return;
        }
        if self.active_root() != Some(root.as_path()) {
            self.persisted.projects.set_folded(&root, false);
            self.switch_project(root.clone(), cx);
            // Refused (and said why), or the folder is no repository now.
            if self.active_root() != Some(root.as_path()) {
                return;
            }
        }
        self.open_worktree_dialog(root, String::new(), Source::Branch, window, cx);
    }

    /// "From issue" in the new worktree dialog: the same project and base,
    /// a GitHub issue names the branch and drafts the first message.
    fn worktree_dialog_from_issue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let blocked = self.git_operation_running();
        let Some(dialog) = &mut self.worktree_dialog else {
            return;
        };
        if dialog.creating || dialog.lookup.is_some() {
            return;
        }
        let root = dialog.root.clone();
        let base = match self
            .creation_base
            .value()
            .filter(|base| base.root == root && self.creation_base.error().is_none())
        {
            _ if blocked => Err("Wait for the running Git operation first"),
            None => Err("Wait for the base branch to load, or refresh the project"),
            Some(base) => match (base.branch.clone(), base.tip.clone()) {
                (Some(branch), Some(tip)) => Ok((branch, tip)),
                _ => Err("Choose an existing local base branch first"),
            },
        };
        let (base_branch, base_tip) = match base {
            Ok(base) => base,
            Err(error) => {
                dialog.error = Some(error.into());
                cx.notify();
                return;
            }
        };
        let previous_focus = self
            .worktree_dialog
            .take()
            .and_then(|dialog| dialog.previous_focus);
        self.open_worktree_dialog(root, base_tip, Source::Issue { base_branch }, window, cx);
        if let Some(dialog) = &mut self.worktree_dialog {
            dialog.previous_focus = previous_focus;
        }
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
            Source::Branch | Source::Commit { .. } | Source::ForkPull { .. } => {
                "feature/new-branch"
            }
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
        // A new branch starts from the base the dialog shows.
        let base = self
            .creation_base
            .value()
            .filter(|_| self.creation_base.error().is_none())
            .map(|info| info.creation_ref().map_err(|error| error.to_string()))
            .unwrap_or_else(|| {
                Err("Base is unavailable. Refresh or choose a local branch first".into())
            });
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
        let from_base = matches!(dialog.source, Source::Branch).then_some(base);
        let done = match (&dialog.source, &issue) {
            (_, Some(issue)) => format!("Created a worktree for issue #{}", issue.number),
            (Source::ForkPull { number, .. }, None) => {
                format!("Created a review worktree for PR #{number}")
            }
            (Source::Commit { .. }, None) => "Created a worktree from the selected commit".into(),
            (Source::Branch | Source::Issue { .. }, None) => {
                format!("Created a worktree for {branch}")
            }
        };
        dialog.creating = true;
        self.action_task = Some(cx.spawn_in(window, async move |this, cx| {
            let target = path.clone();
            let request = root.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    match from_base {
                        Some(base) => create_from_base(&request, &target, &branch, base),
                        None => create_at_tip(&request, &target, &branch, &tip, pull),
                    }
                })
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
                        this.flash(done, false, cx);
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
        // Updating the base, for one: creating waits for it.
        let blocked = self.git_operation_running();
        let branch_mode = matches!(dialog.source, Source::Branch);
        let base_tip = &dialog.base_tip[..dialog.base_tip.len().min(8)];
        let (title, origin) = match &dialog.source {
            Source::Branch => ("New worktree", None),
            Source::Issue { base_branch } => (
                "Worktree from issue",
                Some(format!("Base {base_branch} at {base_tip}")),
            ),
            Source::Commit { subject } => (
                "Worktree from commit",
                Some(format!("{base_tip} · {subject}")),
            ),
            Source::ForkPull {
                number, subject, ..
            } => (
                "Review fork pull request",
                Some(format!("#{number} · {subject} · {base_tip}")),
            ),
        };
        let project = project_name(&dialog.root);
        let origin = match origin {
            Some(origin) => format!("{project} · {origin}"),
            None => project,
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
        let action_tooltip = if dialog.creating {
            "Wait for the worktree to be created"
        } else if dialog.lookup.is_some() {
            "Wait for GitHub to return the issue"
        } else if blocked {
            "Wait for the running Git operation to finish"
        } else if dialog.ready_to_create() {
            "Check out the new branch in a folder of its own (Return)"
        } else {
            "Read the issue from GitHub to name the branch after it (Return)"
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
                            .child(origin),
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
                            .child(div().text_sm().child(if branch_mode {
                                "Branch"
                            } else if dialog.ready_to_create() {
                                "New branch"
                            } else {
                                "Issue number or URL"
                            }))
                            .child(Input::new(&dialog.input).disabled(busy)),
                    )
                    .when(branch_mode, |panel| {
                        panel.child(self.render_creation_base(cx))
                    })
                    .when_some(dialog.error.clone(), |panel, error| {
                        panel.child(div().text_sm().text_color(theme.danger).child(error))
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .when(branch_mode, |row| {
                                row.child(
                                    Button::new("worktree-from-issue")
                                        .label("From issue")
                                        .small()
                                        .ghost()
                                        .tooltip(
                                            "Name the branch after a GitHub issue and draft it for an agent",
                                        )
                                        .disabled(busy || blocked)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.worktree_dialog_from_issue(window, cx);
                                        })),
                                )
                            })
                            .child(div().flex_1())
                            .child(
                                Button::new("cancel-issue-worktree")
                                    .label("Cancel")
                                    .small()
                                    .ghost()
                                    .disabled(dialog.creating)
                                    .tooltip(if dialog.creating {
                                        "Wait for the worktree to be created"
                                    } else {
                                        "Close without creating a worktree (Esc)"
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_worktree_dialog(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("submit-issue-worktree")
                                    .label(action)
                                    .small()
                                    .primary()
                                    .disabled(busy || blocked)
                                    .tooltip(action_tooltip)
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
    check_branch_name(root, branch)?;
    if let Some((number, repository)) = pull {
        let remote = github_fetch_url(root, &repository)?;
        sg_git::require_pull_commit(root, remote.as_deref(), number, tip)?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    sg_git::add_worktree(root, path, Some(branch), Some(tip))
}

fn check_branch_name(root: &Path, branch: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!branch.is_empty(), "Type a branch name first");
    anyhow::ensure!(
        !branch.starts_with('-'),
        "Invalid Git branch name '{branch}'"
    );
    sg_git::git(root, ["check-ref-format", &format!("refs/heads/{branch}")])
        .map_err(|_| anyhow::anyhow!("Invalid Git branch name '{branch}'"))?;
    Ok(())
}

/// A worktree on `branch`: a branch that exists keeps its own history, a new
/// one starts from `base`.
fn create_from_base(
    root: &Path,
    path: &Path,
    branch: &str,
    base: Result<String, String>,
) -> anyhow::Result<()> {
    check_branch_name(root, branch)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let exists = sg_git::git(
        root,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok();
    if exists {
        sg_git::add_worktree(root, path, None, Some(branch))
    } else {
        let base = base.map_err(anyhow::Error::msg)?;
        sg_git::add_worktree(root, path, Some(branch), Some(&base))
    }
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
    use super::{Issue, agent_context, create_from_base, issue_branch, issue_draft};
    use std::path::{Path, PathBuf};

    /// A temporary folder, removed with everything in it.
    struct Sandbox(PathBuf);

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args([
                "-c",
                "user.name=SuperGIT",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn head(dir: &Path, rev: &str) -> String {
        sg_git::git(dir, ["rev-parse", rev]).unwrap()
    }

    #[test]
    fn new_worktree_starts_a_branch_from_the_base_or_checks_one_out_as_it_is() {
        let sandbox = Sandbox(
            std::env::temp_dir().join(format!("supergit_new_worktree_{}", std::process::id())),
        );
        let repo = sandbox.0.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
        git(&repo, &["branch", "existing"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "second"]);

        let fresh = sandbox.0.join("fresh");
        create_from_base(&repo, &fresh, "feature/fresh", Ok("main".into())).unwrap();
        assert_eq!(head(&fresh, "HEAD"), head(&repo, "main"));

        // The base is not needed (nor applied) for a branch that exists.
        let kept = sandbox.0.join("kept");
        create_from_base(&repo, &kept, "existing", Err("no base".into())).unwrap();
        assert_eq!(head(&kept, "HEAD"), head(&repo, "existing"));
        assert_ne!(head(&kept, "HEAD"), head(&repo, "main"));

        let missing = sandbox.0.join("missing");
        let error = create_from_base(&repo, &missing, "new", Err("no base".into())).unwrap_err();
        assert_eq!(error.to_string(), "no base");
        for bad in ["", "two words", "-x"] {
            assert!(create_from_base(&repo, &missing, bad, Ok("main".into())).is_err());
        }
        assert!(!missing.exists());
    }

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
