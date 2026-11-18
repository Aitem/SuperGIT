use std::sync::Arc;

use gpui_kit::component::checkbox::Checkbox;
use sg_github::{FileReview, FileViewedState};

use super::*;

const REFRESH: Duration = Duration::from_secs(30);

#[derive(Clone, PartialEq, Eq)]
struct Target {
    number: u64,
    head: String,
    base: String,
}

struct LoadedReview {
    target: Target,
    at: Instant,
    result: Result<Arc<FileReview>, String>,
}

#[derive(Default)]
pub(super) struct ReviewProgress {
    loaded: Option<LoadedReview>,
    load: Option<(Target, Task<()>)>,
    mutation: Option<Task<()>>,
    mutation_error: Option<(Target, String)>,
    pub(super) hide_viewed: bool,
}

impl SuperGitApp {
    pub(super) fn ensure_pr_file_review(
        &mut self,
        root: PathBuf,
        pr: &PullRequest,
        base: &str,
        cx: &mut Context<Self>,
    ) {
        if pr.head_oid.is_empty() || base.is_empty() || self.pulls.file_review.mutation.is_some() {
            return;
        }
        let state = &mut self.pulls.file_review;
        let target = Target {
            number: pr.number,
            head: pr.head_oid.clone(),
            base: base.into(),
        };
        if state
            .load
            .as_ref()
            .is_some_and(|(pending, _)| *pending == target)
            || state
                .loaded
                .as_ref()
                .is_some_and(|loaded| loaded.target == target && loaded.at.elapsed() < REFRESH)
        {
            return;
        }
        let request = (root.clone(), target.clone());
        let completed = target.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let (root, target) = request;
                    sg_github::pr_file_review(&root, target.number, &target.head, &target.base)
                        .map(Arc::new)
                        .map_err(model::git_error_message)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let state = &mut this.pulls.file_review;
                if state
                    .load
                    .as_ref()
                    .is_some_and(|(target, _)| *target == completed)
                    && this.repo.as_ref().is_some_and(|repo| repo.root == root)
                {
                    state.load = None;
                    state.loaded = Some(LoadedReview {
                        target: completed,
                        at: Instant::now(),
                        result,
                    });
                    cx.notify();
                }
            });
        });
        state.load = Some((target, task));
    }

    fn visible_review(&self) -> Option<&LoadedReview> {
        let loaded = self.pulls.file_review.loaded.as_ref()?;
        let target = &loaded.target;
        if self.tab != MainTab::PullRequests
            || self.pulls.tab != DetailsTab::Files
            || self.pulls.shown != Some(target.number)
        {
            return None;
        }
        let (number, head) = self.pulls.diff_shown.as_ref()?;
        let patch = self.pulls.patches.get(number)?;
        let details = &self.pulls.details.get(number)?.details;
        let current = details.value()?;
        (*number == target.number
            && *head == target.head
            && patch.base_oid == target.base
            && current.pr.head_oid == target.head
            && current.base_oid == target.base
            && details.error().is_none()
            && patch.patch.error().is_none())
        .then_some(loaded)
    }

    pub(super) fn hide_pr_file(&self, path: &str) -> bool {
        self.pulls.file_review.hide_viewed
            && self
                .visible_review()
                .and_then(|loaded| loaded.result.as_ref().ok())
                .and_then(|review| review.files.get(path))
                == Some(&FileViewedState::Viewed)
    }

    pub(in crate::app) fn pr_file_checkbox(
        &self,
        path: &str,
        cx: &mut Context<Self>,
    ) -> Option<Checkbox> {
        let loaded = self.visible_review()?;
        let state = *loaded.result.as_ref().ok()?.files.get(path)?;
        let target = loaded.target.clone();
        let path = path.to_owned();
        let help = match state {
            FileViewedState::Viewed => "Mark as unviewed",
            FileViewedState::Unviewed => "Mark as viewed",
            FileViewedState::Dismissed => "Changed since last viewed",
        };
        Some(
            Checkbox::new(SharedString::from(format!("pr-file-viewed:{path}")))
                .small()
                .flex_none()
                .checked(state == FileViewedState::Viewed)
                .disabled(
                    self.pulls.file_review.mutation.is_some()
                        || self.pulls.file_review.load.is_some(),
                )
                .accessibility_label(format!("Viewed {path}"))
                .tooltip(help)
                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                    cx.stop_propagation();
                    this.set_pr_file_viewed(&target, &path, *checked, cx);
                })),
        )
    }

    fn set_pr_file_viewed(
        &mut self,
        target: &Target,
        path: &str,
        viewed: bool,
        cx: &mut Context<Self>,
    ) {
        if self.pulls.file_review.mutation.is_some() || self.pulls.file_review.load.is_some() {
            return;
        }
        let Some(loaded) = self
            .visible_review()
            .filter(|loaded| &loaded.target == target)
        else {
            return;
        };
        let Some(review) = loaded.result.as_ref().ok().cloned() else {
            return;
        };
        let Some(root) = self.active_root().map(Path::to_path_buf) else {
            return;
        };
        let target = target.clone();
        let path = path.to_owned();
        let request = (root.clone(), path.clone());
        let state = &mut self.pulls.file_review;
        state.mutation_error = None;
        state.mutation = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let (root, path) = request;
                    sg_github::set_pr_file_viewed(&root, &review, &path, viewed)
                        .map_err(model::git_error_message)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let state = &mut this.pulls.file_review;
                if this.repo.as_ref().is_none_or(|repo| repo.root != root) {
                    return;
                }
                state.mutation = None;
                match result {
                    Ok(()) => {
                        if let Some(loaded) = state
                            .loaded
                            .as_mut()
                            .filter(|loaded| loaded.target == target)
                            && let Ok(review) = &mut loaded.result
                        {
                            let viewed_state = if viewed {
                                FileViewedState::Viewed
                            } else {
                                FileViewedState::Unviewed
                            };
                            Arc::make_mut(review).files.insert(path, viewed_state);
                        }
                    }
                    Err(error) => state.mutation_error = Some((target, error)),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn render_pr_review_progress(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let loaded = self.visible_review()?;
        let theme = cx.theme().clone();
        let error = loaded.result.as_ref().err().or_else(|| {
            let (target, error) = self.pulls.file_review.mutation_error.as_ref()?;
            (*target == loaded.target).then_some(error)
        });
        let progress = loaded.result.as_ref().ok().map(|review| {
            let viewed = review
                .files
                .values()
                .filter(|&&state| state == FileViewedState::Viewed)
                .count();
            h_flex()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .child(format!("{viewed} of {} files viewed", review.files.len())),
                )
                .child(
                    Checkbox::new("hide-viewed-pr-files")
                        .small()
                        .label("Hide viewed in list")
                        .checked(self.pulls.file_review.hide_viewed)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.pulls.file_review.hide_viewed = *checked;
                            cx.notify();
                        })),
                )
        });
        Some(
            v_flex()
                .flex_none()
                .gap_1()
                .px_3()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .children(progress)
                .when_some(error, |el, error| {
                    el.child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_xs()
                                    .text_color(theme.warning)
                                    .child(error.clone()),
                            )
                            .child(
                                Button::new("refresh-pr-file-review")
                                    .label("Refresh viewed state")
                                    .xsmall()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.pulls.file_review.loaded = None;
                                        this.pulls.file_review.mutation_error = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                })
                .into_any_element(),
        )
    }
}
