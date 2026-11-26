//! The selected commit under History's commit tree: its card, then its
//! changes (the diff's bar, the file list when shown, the diff), scrolling
//! as one pane.
//!
//! The diff scrolls itself (a virtualized list that must have a bounded
//! height), so the pane holds the card and, under it, the changes exactly
//! as tall as the pane: scrolling the pane by the card's height brings the
//! diff to fill it. A wheel turn over the pane first moves the card out of
//! view, then the diff; turning back, the diff returns to its top before
//! the card comes back. `ScrollChain` remembers where the diff stood in the
//! last frame to decide.

use std::cell::Cell;
use std::rc::Rc;

use super::*;

/// The commit pane's scroll and what its wheel handler needs to know about
/// the scrollers inside it.
pub(super) struct ScrollChain {
    /// The pane: the card above the changes.
    pub(super) pane: ScrollHandle,
    /// The commit's file list, when it shows.
    pub(super) files: ScrollHandle,
    /// The diff body's top edge in the last frame (window coordinates).
    diff_top: Cell<Pixels>,
    /// The first file header's top edge, when the last frame drew it.
    first_header: Cell<Option<Pixels>>,
    /// The pane's height in the last frame: the changes are as tall (a
    /// scroll container lays its content out unbounded, so a relative
    /// height would not do).
    height: Cell<Pixels>,
}

impl ScrollChain {
    pub(super) fn new() -> Self {
        Self {
            pane: ScrollHandle::new(),
            files: ScrollHandle::new(),
            diff_top: Cell::new(Pixels::ZERO),
            first_header: Cell::new(None),
            height: Cell::new(Pixels::ZERO),
        }
    }

    /// Back to the card, for a newly selected commit.
    pub(super) fn reset(&self) {
        self.pane.set_offset(point(Pixels::ZERO, Pixels::ZERO));
        self.files.set_offset(point(Pixels::ZERO, Pixels::ZERO));
    }

    /// Whatever scrolls under `position` shows its top: the file list, or
    /// the diff (difftastic's when given).
    fn inner_at_top(
        &self,
        position: Point<Pixels>,
        difft: Option<&Entity<DifftView>>,
        cx: &App,
    ) -> bool {
        if self.files.bounds().contains(&position) {
            return self.files.offset().y >= Pixels::ZERO;
        }
        match difft {
            Some(view) => view.read(cx).at_top(),
            None => self
                .first_header
                .get()
                .is_some_and(|top| top >= self.diff_top.get() - px(0.5)),
        }
    }

    /// Records where the diff body starts; drawn inside it.
    pub(super) fn diff_top_probe(chain: &Rc<Self>) -> impl IntoElement {
        let chain = chain.clone();
        canvas(
            move |bounds, _, _| chain.diff_top.set(bounds.origin.y),
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_0()
    }

    /// Records where the first file's header starts; drawn in that header
    /// (an absolute child, so the header's layout does not change).
    pub(super) fn header_probe(chain: &Rc<Self>) -> AnyElement {
        let chain = chain.clone();
        canvas(
            move |bounds, _, _| chain.first_header.set(Some(bounds.origin.y)),
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_0()
        .into_any_element()
    }

    /// Covers the pane, behind its content, and takes the wheel turns that
    /// move the card: down while the card shows, up once the scroller under
    /// the pointer is at its top. Drawn first, so its handler runs before
    /// the diff's own in the capture phase.
    fn wheel(chain: &Rc<Self>, difft: Option<Entity<DifftView>>) -> impl IntoElement {
        let reset = chain.clone();
        let chain = chain.clone();
        canvas(
            move |bounds, window, _| {
                // The header sets it again if this frame draws it.
                reset.first_header.set(None);
                // A new height lays the changes out again, once.
                if reset.height.replace(bounds.size.height) != bounds.size.height {
                    window.refresh();
                }
                window.insert_hitbox(bounds, HitboxBehavior::Normal)
            },
            move |_, hitbox, window, _| {
                let view = window.current_view();
                window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture || !hitbox.should_handle_scroll(window) {
                        return;
                    }
                    let delta = event.delta.pixel_delta(window.line_height());
                    if delta.y == Pixels::ZERO || delta.x.abs() > delta.y.abs() {
                        return;
                    }
                    let offset = chain.pane.offset();
                    let max = chain.pane.max_offset().y;
                    let next = if delta.y < Pixels::ZERO {
                        if offset.y <= -max {
                            return;
                        }
                        (offset.y + delta.y).max(-max)
                    } else {
                        if offset.y >= Pixels::ZERO
                            || !chain.inner_at_top(event.position, difft.as_ref(), cx)
                        {
                            return;
                        }
                        (offset.y + delta.y).min(Pixels::ZERO)
                    };
                    chain.pane.set_offset(point(offset.x, next));
                    cx.stop_propagation();
                    cx.notify(view);
                });
            },
        )
        .absolute()
        .size_full()
    }
}

impl SuperGitApp {
    /// The selected commit under the commit tree: the squash form once its
    /// button opened it, the card, then the changes, as tall as the pane.
    /// `width` is the pane's.
    pub(super) fn render_commit_pane(&self, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let squash = self
            .history
            .commits
            .value()
            .map(|commits| self.render_squash(commits, cx));
        let notice = if self.commit_patch.is_loading() {
            Some(self.render_data_notice("Loading commit diff…", None, false, cx))
        } else {
            self.commit_patch.error().map(|error| {
                self.render_data_notice(
                    if self.commit_patch.value().is_some() {
                        "Commit diff may be out of date — showing the last successful data"
                    } else {
                        "Commit diff unavailable"
                    },
                    Some(error),
                    true,
                    cx,
                )
            })
        };
        let card = self
            .commit_details
            .as_ref()
            .filter(|d| self.selected_commit.as_deref() == Some(d.commit.sha.as_str()))
            .map(|details| self.render_commit_card(details, cx));
        let chain = self.commit_chain.clone();
        // Until the first frame measures the pane: about what it gets.
        let height = Some(chain.height.get())
            .filter(|height| *height > Pixels::ZERO)
            .unwrap_or(px(400.));
        let changes = self.commit_patch.value().map(|patch| {
            if patch.is_empty() {
                self.render_data_notice("No changes in this commit", None, false, cx)
            } else {
                // As tall as the pane: scrolled past the card, it fills it.
                div()
                    .flex_none()
                    .h(height)
                    .child(self.render_commit_changes(width, cx))
                    .into_any_element()
            }
        });
        let difft = self.difftastic_active().then(|| self.commit_difft.clone());
        div()
            .relative()
            .size_full()
            .child(ScrollChain::wheel(&chain, difft))
            .child(
                v_flex()
                    .id("commit-pane")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&chain.pane)
                    .children(squash.map(|form| div().flex_none().child(form)))
                    .children(notice.map(|notice| div().flex_none().child(notice)))
                    .children(card)
                    .children(changes),
            )
            .into_any_element()
    }

    /// A commit's changes: the diff's bar across them (the file list's
    /// toggle first), then the file list beside the diff when it shows.
    fn render_commit_changes(&self, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let body = if self.repository_view().commit_files_open {
            let theme = cx.theme().clone();
            let files: Vec<FileChange> = self
                .commit_diff
                .read(cx)
                .files()
                .iter()
                .map(commit_file)
                .collect();
            let list_max = (width - 180.).clamp(140., 600.);
            h_resizable("commit-files-layout")
                .with_state(&self.commit_files_layout)
                .on_resize(cx.listener(|this, state, _, cx| {
                    this.remember_layout(LayoutPanel::CommitFiles, state, cx)
                }))
                .child(
                    resizable_panel()
                        .size(px(self.repository_view().commit_files_width.min(list_max)))
                        .size_range(px(140.)..px(list_max))
                        .fixed()
                        .child(self.render_file_list(FileList::Commit, &files, &theme, cx)),
                )
                .child(
                    resizable_panel()
                        .size_range(px(180.)..Pixels::MAX)
                        .child(elastic(self.render_diff_body(FileList::Commit, cx))),
                )
                .into_any_element()
        } else {
            self.render_diff_body(FileList::Commit, cx)
        };
        v_flex()
            .size_full()
            .min_w_0()
            .child(self.render_diff_toolbar(FileList::Commit, cx))
            .child(v_flex().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    /// Shows or hides the commit's file list beside its diff.
    pub(super) fn toggle_commit_files(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.repository_view_mut() else {
            return;
        };
        view.commit_files_open = !view.commit_files_open;
        self.queue_persist(cx);
        cx.notify();
    }

    /// Back to the commit tree alone: no commit selected.
    pub(super) fn close_commit(&mut self, cx: &mut Context<Self>) {
        if self.selected_commit.is_none() {
            return;
        }
        self.capture_saved_review(cx);
        self.selected_commit = None;
        self.commit_task = None;
        self.commit_generation += 1;
        self.commit_patch = LoadState::Loading;
        self.commit_submodules.clear();
        self.commit_details = None;
        self.commit_diff
            .update(cx, |state, cx| state.set_files(Vec::new(), cx));
        self.commit_difft.update(cx, |view, cx| view.clear(cx));
        if let Some(path) = self.selected.clone()
            && let Some(view) = self.repository_view_mut()
        {
            let worktree = view.worktrees.entry(path).or_default();
            worktree.commit = None;
            worktree.commit_position = None;
        }
        self.queue_persist(cx);
        cx.notify();
    }
}
