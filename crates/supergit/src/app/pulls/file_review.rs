use std::sync::Arc;

use gpui_kit::component::checkbox::Checkbox;
use sg_github::{FileReview, FileViewedState};

use super::*;

const REFRESH: Duration = Duration::from_secs(30);

#[derive(Clone, PartialEq, Eq)]
struct Target {
    number: u64,
    head: String,
    /// Empty while the PR's details (which carry it) are loading.
    base: String,
}

impl Target {
    /// The same revision, as far as both know it.
    fn matches(&self, other: &Target) -> bool {
        self.number == other.number
            && self.head == other.head
            && (self.base == other.base || self.base.is_empty() || other.base.is_empty())
    }
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
    /// Clicks not sent yet, one per path (the last one wins), in click order.
    /// The loaded review already shows them.
    queued: Vec<(String, bool)>,
    /// What GitHub last confirmed for each path changed since: restored when
    /// an update fails.
    unconfirmed: HashMap<String, FileViewedState>,
    /// The update in flight; clicks meanwhile wait in `queued`.
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
        // A refresh must not bring back states GitHub has not confirmed yet.
        if pr.head_oid.is_empty()
            || self.pulls.file_review.mutation.is_some()
            || !self.pulls.file_review.queued.is_empty()
        {
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
            .is_some_and(|(pending, _)| pending.matches(&target))
            || state.loaded.as_ref().is_some_and(|loaded| {
                loaded.target.matches(&target) && loaded.at.elapsed() < REFRESH
            })
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
                    // Started before the details: GitHub named the base.
                    let mut target = completed;
                    if let (true, Ok(review)) = (target.base.is_empty(), &result) {
                        target.base = review.base().to_owned();
                    }
                    state.loaded = Some(LoadedReview {
                        target,
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
                .accessibility_label(format!("Viewed {path}"))
                .tooltip(help)
                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                    cx.stop_propagation();
                    this.set_pr_file_viewed(&target, &path, *checked, cx);
                })),
        )
    }

    /// Show the click right away and send it in the background: GitHub takes
    /// two round trips (about a second) to confirm it. Clicks meanwhile are
    /// queued and sent together once the update in flight is done.
    fn set_pr_file_viewed(
        &mut self,
        target: &Target,
        path: &str,
        viewed: bool,
        cx: &mut Context<Self>,
    ) {
        if self
            .visible_review()
            .is_none_or(|loaded| &loaded.target != target)
        {
            return;
        }
        let state = &mut self.pulls.file_review;
        let Some(Ok(review)) = state.loaded.as_mut().map(|loaded| &mut loaded.result) else {
            return;
        };
        let Some(current) = Arc::make_mut(review).files.get_mut(path) else {
            return;
        };
        let wanted = if viewed {
            FileViewedState::Viewed
        } else {
            FileViewedState::Unviewed
        };
        state.unconfirmed.entry(path.to_owned()).or_insert(*current);
        *current = wanted;
        state.queued.retain(|(queued, _)| queued != path);
        state.queued.push((path.to_owned(), viewed));
        state.mutation_error = None;
        // A refresh read before this click would undo it.
        state.load = None;
        self.send_viewed(cx);
        cx.notify();
    }

    /// Send the queued clicks unless an update is in flight.
    fn send_viewed(&mut self, cx: &mut Context<Self>) {
        let root = self.active_root().map(Path::to_path_buf);
        let state = &mut self.pulls.file_review;
        if state.mutation.is_some() || state.queued.is_empty() {
            return;
        }
        let review = state
            .loaded
            .as_ref()
            .and_then(|loaded| Some((loaded.target.clone(), loaded.result.clone().ok()?)));
        let (Some(root), Some((target, review))) = (root, review) else {
            // The review was dropped ("Refresh viewed state"): nothing to apply to.
            state.queued.clear();
            state.unconfirmed.clear();
            return;
        };
        let batch = std::mem::take(&mut state.queued);
        state.mutation = Some(cx.spawn(async move |this, cx| {
            let request = (root.clone(), batch.clone());
            let result = cx
                .background_executor()
                .spawn(async move {
                    let (root, batch) = request;
                    sg_github::set_pr_files_viewed(&root, &review, &batch)
                        .map_err(model::git_error_message)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.active_root() == Some(root.as_path()) {
                    this.viewed_sent(target, &batch, result, cx);
                }
            });
        }));
    }

    fn viewed_sent(
        &mut self,
        target: Target,
        batch: &[(String, bool)],
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        let state = &mut self.pulls.file_review;
        state.mutation = None;
        match result {
            Ok(()) => {
                for (path, viewed) in batch {
                    if state.queued.iter().any(|(queued, _)| queued == path) {
                        // Clicked again meanwhile: GitHub now has this batch's state.
                        let confirmed = if *viewed {
                            FileViewedState::Viewed
                        } else {
                            FileViewedState::Unviewed
                        };
                        state.unconfirmed.insert(path.clone(), confirmed);
                    } else {
                        state.unconfirmed.remove(path);
                    }
                }
                self.send_viewed(cx);
            }
            Err(error) => {
                // GitHub may have applied part of it: show what it confirmed
                // last, and offer a refresh with the error.
                state.queued.clear();
                let restore = std::mem::take(&mut state.unconfirmed);
                if let Some(Ok(review)) = state
                    .loaded
                    .as_mut()
                    .filter(|loaded| loaded.target == target)
                    .map(|loaded| &mut loaded.result)
                {
                    Arc::make_mut(review).files.extend(restore);
                }
                state.mutation_error = Some((target, error));
            }
        }
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
