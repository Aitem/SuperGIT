use anyhow::{Context as _, Result, ensure};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use sg_git::BranchPublication;
use sg_github::PublishBase;

use super::*;

#[derive(Clone)]
struct Preview {
    publication: BranchPublication,
    repository: String,
    base: PublishBase,
    suggested_title: String,
    commits: Option<(usize, Vec<String>)>,
}

impl Preview {
    fn load(path: &Path) -> Result<Self> {
        let publication = sg_git::branch_publication(path)?;
        let repository = sg_github::publish_repository(path, publication.push_url())?;
        let base = repository
            .default_branch_ref
            .context("This GitHub repository has no default branch")?;
        ensure!(
            publication.remote_branch != base.name,
            "Switch to a feature branch to create a pull request into {}",
            base.name
        );
        let commits = sg_git::publication_commits(path, &publication, &base.target.oid)?;
        let subject = sg_git::git(path, ["show", "-s", "--format=%s", &publication.head])?;
        let title = subject.trim();
        let suggested_title = if title.is_empty() {
            publication.branch.clone()
        } else {
            title.to_string()
        };
        Ok(Self {
            publication,
            repository: repository.name_with_owner,
            base,
            suggested_title,
            commits,
        })
    }
}

struct Published {
    number: u64,
    url: String,
    details: Option<PullRequest>,
    existing: bool,
}

impl Published {
    fn existing(pr: PullRequest) -> Self {
        Self {
            number: pr.number,
            url: pr.url.clone(),
            details: Some(pr),
            existing: true,
        }
    }

    fn title(&self) -> String {
        if self.existing {
            format!("Pull request #{} already exists", self.number)
        } else {
            format!("Draft PR #{} created", self.number)
        }
    }
}

fn publish(path: &Path, preview: &Preview, title: &str, body: &str) -> Result<Published> {
    ensure!(
        sg_git::branch_publication(path)? == preview.publication,
        "The branch tip or push destination changed; reopen the preview"
    );
    let target = sg_github::publish_repository(path, preview.publication.push_url())?;
    let target_base = target
        .default_branch_ref
        .context("The default branch is no longer available")?;
    ensure!(
        target
            .name_with_owner
            .eq_ignore_ascii_case(&preview.repository)
            && target_base.name == preview.base.name
            && target_base.target.oid == preview.base.target.oid,
        "The repository or base tip changed; reopen the preview"
    );
    let head = &preview.publication.remote_branch;
    let base = &preview.base.name;
    let repository = &preview.repository;
    let find_existing = || sg_github::open_pr_from_branch(path, repository, head, base);
    if let Some(pr) = find_existing()? {
        return Ok(Published::existing(pr));
    }
    sg_git::publish_branch(path, &preview.publication)?;
    let created = match sg_github::create_draft_pr(path, repository, head, base, title, body) {
        Ok(created) => created,
        Err(error) => {
            if let Ok(Some(pr)) = find_existing() {
                return Ok(Published::existing(pr));
            }
            return Err(error.context("Branch pushed. GitHub did not confirm a draft PR"));
        }
    };
    let details = sg_github::view_pr(path, created.number)
        .ok()
        .filter(|pr| pr.url == created.url);
    Ok(Published {
        number: created.number,
        url: created.url,
        details,
        existing: false,
    })
}

pub(super) struct PublishDialog {
    path: PathBuf,
    previous_focus: Option<FocusHandle>,
    title: Entity<InputState>,
    body: Entity<TextareaState>,
    preview: LoadState<Preview>,
    running: bool,
    error: Option<String>,
    result: Option<Published>,
    _subscription: Subscription,
}

impl SuperGitApp {
    pub(super) fn render_publish_button(
        &self,
        info: &WorktreeInfo,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let path = info.wt.path.clone();
        let ready = info
            .status
            .as_ref()
            .is_ok_and(|status| status.branch.is_some() && status.head_oid.is_some());
        Button::new("publish-worktree-pr")
            .label("New draft PR…")
            .small()
            .outline()
            .tooltip(if ready {
                "Publish this worktree’s branch and open a draft pull request"
            } else {
                "Select a worktree on a branch with a commit"
            })
            .disabled(!ready || self.pulls.publish_task.is_some())
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_publish_dialog(path.clone(), window, cx);
            }))
            .into_any_element()
    }

    #[cfg(feature = "screenshot")]
    pub(super) fn debug_publish_dialog(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.pulls.debug_publish_opened || std::env::var_os("SUPERGIT_DEBUG_NEW_PR").is_none() {
            return;
        }
        let Some(window) = cx.windows().first().copied() else {
            return;
        };
        self.pulls.debug_publish_opened = true;
        let app = cx.entity();
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                app.update(cx, |this, cx| this.open_publish_dialog(path, window, cx));
            });
        });
    }

    fn open_publish_dialog(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.pulls.dialog_open() || self.pulls.publish_task.is_some() {
            return;
        }
        let title = cx.new(|cx| InputState::new(window, cx).placeholder("Pull request title"));
        let body = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 6)
                .placeholder("What changed and how was it tested?")
        });
        let subscription = cx.subscribe(&title, |_, _, _: &InputEvent, cx| cx.notify());
        self.pulls.publish = Some(PublishDialog {
            path: path.clone(),
            previous_focus: window.focused(cx),
            title: title.clone(),
            body,
            preview: LoadState::Loading,
            running: false,
            error: None,
            result: None,
            _subscription: subscription,
        });
        title.update(cx, |input, cx| input.focus(window, cx));
        let task = cx.spawn_in(window, async move |this, cx| {
            let request = path.clone();
            let preview = cx
                .background_executor()
                .spawn(async move { Preview::load(&request).map_err(|error| format!("{error:#}")) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.pulls.publish_task = None;
                if let Some(dialog) = &mut this.pulls.publish
                    && dialog.path == path
                {
                    if let Ok(preview) = &preview
                        && dialog.title.read(cx).value().trim().is_empty()
                    {
                        dialog.title.update(cx, |input, cx| {
                            input.set_value(preview.suggested_title.clone(), window, cx)
                        });
                    }
                    dialog.preview = LoadState::from_result(preview);
                }
                cx.notify();
            });
        });
        self.pulls.publish_task = Some(task);
        cx.notify();
    }

    fn close_publish_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .pulls
            .publish
            .as_ref()
            .is_some_and(|dialog| dialog.running)
        {
            return;
        }
        if let Some(dialog) = self.pulls.publish.take() {
            dialog
                .previous_focus
                .unwrap_or_else(|| self.focus_handle.clone())
                .focus(window, cx);
            cx.notify();
        }
    }

    fn confirm_publish_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pulls.publish_task.is_some() {
            return;
        }
        let Some(dialog) = self.pulls.publish.as_mut() else {
            return;
        };
        let title = dialog.title.read(cx).value().trim().to_string();
        if title.is_empty() || dialog.result.is_some() {
            return;
        }
        let Some(preview) = dialog.preview.value().cloned() else {
            return;
        };
        dialog.running = true;
        dialog.error = None;
        let body = dialog.body.read(cx).value().to_string();
        let path = dialog.path.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let request = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    publish(&request, &preview, &title, &body).map_err(|error| format!("{error:#}"))
                })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.pulls.publish_task = None;
                if let Some(dialog) = &mut this.pulls.publish
                    && dialog.path == path
                {
                    dialog.running = false;
                    match result {
                        Ok(pr) => dialog.result = Some(pr),
                        Err(error) => dialog.error = Some(error),
                    }
                    cx.notify();
                } else {
                    match result {
                        Ok(pr) => this.flash(format!("{} · {}", pr.title(), pr.url), false, cx),
                        Err(error) => this.flash(error, true, cx),
                    }
                }
            });
        });
        self.pulls.publish_task = Some(task);
        cx.notify();
    }

    fn show_published_pr(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.pulls.publish.as_ref() else {
            return;
        };
        let Some(pr) = dialog
            .result
            .as_ref()
            .and_then(|result| result.details.clone())
        else {
            return;
        };
        let path = dialog.path.clone();
        self.close_publish_dialog(window, cx);
        if self.prs.iter().any(|listed| listed.number == pr.number) {
            self.refresh_listed_pr(&pr);
        } else {
            self.prs.push(pr.clone());
        }
        self.pulls.worktree.remove(&path);
        if self.selected.as_ref() != Some(&path) {
            self.select_worktree(path.clone(), cx);
        }
        self.pulls.tab = DetailsTab::Conversation;
        self.set_pr_scope(path.clone(), true, cx);
        let search = self.pulls.search.clone();
        search.update(cx, |input, cx| input.set_value("", window, cx));
        self.select_pr(path, pr.number, cx);
    }

    pub(super) fn render_publish_dialog(
        &self,
        dialog: &PublishDialog,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let content = match (&dialog.result, &dialog.preview) {
            (Some(result), _) => self.render_publish_success(result, cx),
            (None, LoadState::Ready(preview)) => self.render_publish_form(dialog, preview, cx),
            (None, LoadState::Loading) => div()
                .text_color(theme.muted_foreground)
                .child("Loading the branch and GitHub base…")
                .into_any_element(),
            (None, LoadState::Failed { error, .. }) => div()
                .text_sm()
                .text_color(theme.danger)
                .child(error.clone())
                .into_any_element(),
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(72.))
            .bg(gpui_kit::black().opacity(0.35))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_publish_dialog(window, cx)),
            )
            .child(
                v_flex()
                    .w(px(574.))
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
                            this.close_publish_dialog(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("New draft pull request"),
                    )
                    .child(content)
                    .when(
                        dialog.result.is_none() && !matches!(dialog.preview, LoadState::Ready(_)),
                        |panel| {
                            panel.child(
                                h_flex().justify_end().child(
                                    Button::new("close-publish-preview")
                                        .label("Close")
                                        .small()
                                        .ghost()
                                        .disabled(dialog.running)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.close_publish_dialog(window, cx)
                                        })),
                                ),
                            )
                        },
                    ),
            )
            .into_any_element()
    }

    fn render_publish_form(
        &self,
        dialog: &PublishDialog,
        preview: &Preview,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let branch = &preview.publication.remote_branch;
        let remote = &preview.publication.remote;
        let base = &preview.base.name;
        let short: String = preview.publication.head.chars().take(8).collect();
        let commit_list = match &preview.commits {
            Some((count, commits)) => {
                let label = match count {
                    0 => "This branch has no commits ahead of the base".to_string(),
                    1 => "1 commit in this branch".to_string(),
                    count => format!("{count} commits in this branch"),
                };
                v_flex()
                    .gap_0p5()
                    .child(div().text_xs().text_color(theme.muted_foreground).child(label))
                    .children(commits.iter().map(|subject| div().truncate().text_xs().child(subject.clone())))
                    .into_any_element()
            }
            None => div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("The remote base is outside the local history. Fetch it to preview the commit list.")
                .into_any_element(),
        };
        let disabled = dialog.running
            || dialog.title.read(cx).value().trim().is_empty()
            || preview
                .commits
                .as_ref()
                .is_some_and(|(count, _)| *count == 0);
        let cancel = Button::new("cancel-publish-pr")
            .label("Cancel")
            .small()
            .ghost()
            .disabled(dialog.running)
            .on_click(cx.listener(|this, _, window, cx| this.close_publish_dialog(window, cx)));
        let confirm = Button::new("confirm-publish-pr")
            .label(if dialog.running {
                "Publishing…"
            } else {
                "Publish & create draft"
            })
            .small()
            .primary()
            .disabled(disabled)
            .on_click(cx.listener(|this, _, window, cx| this.confirm_publish_dialog(window, cx)));
        v_flex()
            .gap_3()
            .text_sm()
            .child(
                v_flex()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(preview.repository.clone()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(theme.mono_font_family.clone())
                            .child(format!("{remote}/{branch} → {base} · {short}")),
                    ),
            )
            .child(commit_list)
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_xs().child("Title"))
                    .child(Input::new(&dialog.title).disabled(dialog.running)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_xs().child("Description"))
                    .child(Textarea::new(&dialog.body).disabled(dialog.running)),
            )
            .when_some(dialog.error.clone(), |body, error| {
                body.child(div().text_color(theme.danger).child(error))
            })
            .child(h_flex().justify_end().gap_2().child(cancel).child(confirm))
            .into_any_element()
    }

    fn render_publish_success(&self, result: &Published, cx: &mut Context<Self>) -> AnyElement {
        let url = result.url.clone();
        v_flex()
            .gap_3()
            .text_sm()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(result.title()),
            )
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(result.url.clone()),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("done-publish-pr")
                            .label("Done")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_publish_dialog(window, cx)
                            })),
                    )
                    .child(
                        Button::new("github-publish-pr")
                            .label("Open on GitHub")
                            .small()
                            .outline()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.report(launch::open_url(&url), "Opened on GitHub", cx)
                            })),
                    )
                    .when(result.details.is_some(), |row| {
                        row.child(
                            Button::new("show-published-pr")
                                .label("Show in SuperGIT")
                                .small()
                                .primary()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.show_published_pr(window, cx)
                                })),
                        )
                    }),
            )
            .into_any_element()
    }
}
