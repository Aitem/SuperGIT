use std::rc::Rc;

use sg_github::ReviewSide;

use super::*;

pub(super) struct ThreadJump {
    pub(super) number: u64,
    pub(super) path: String,
    pub(super) line: Option<(DiffSide, usize)>,
}

pub(super) fn thread_line(thread: &ReviewThread) -> Option<(DiffSide, usize)> {
    let line = usize::try_from(thread.line?).ok()?;
    if thread.outdated || line == 0 {
        return None;
    }
    let side = match thread.side {
        ReviewSide::Original => DiffSide::Original,
        ReviewSide::Modified => DiffSide::Modified,
    };
    Some((side, line))
}

struct InlineReview {
    annotation: DiffAnnotation,
    thread: ReviewThread,
}

impl SuperGitApp {
    fn inline_review(&self, cx: &App) -> Option<(PullRequest, Vec<InlineReview>)> {
        let (number, rendered_head) = self.pulls.diff_shown.as_ref()?;
        if self.pulls.shown != Some(*number) {
            return None;
        }
        let details = self.pulls.details.get(number)?.details.value()?;
        if &details.pr.head_oid != rendered_head {
            return None;
        }
        let patch = self.pulls.patches.get(number)?;
        if &patch.head != rendered_head || details.base_oid != patch.base_oid {
            return None;
        }
        let diff = self.pulls.diff.read(cx);
        let threads: Vec<_> = details
            .threads
            .iter()
            .filter_map(|thread| {
                if thread.outdated
                    || !diff
                        .files()
                        .iter()
                        .any(|file| file.path().as_ref() == thread.path)
                {
                    return None;
                }
                let chunk = patch.file_patch(&thread.path);
                let note = SharedString::from(format!("inline-pr-thread-{}", thread.id));
                let annotation = match thread_line(thread) {
                    Some((side, line))
                        if chunk.is_some_and(|chunk| {
                            agent_context::hunk_patch(chunk, side == DiffSide::Original, line)
                                .is_some()
                        }) =>
                    {
                        DiffAnnotation::line(
                            note,
                            DiffLinePosition::new(thread.path.clone(), side, line),
                        )
                    }
                    _ => DiffAnnotation::file(note, thread.path.clone()),
                };
                Some(InlineReview {
                    annotation,
                    thread: thread.clone(),
                })
            })
            .collect();
        (!threads.is_empty()).then(|| (details.pr.clone(), threads))
    }

    pub(in crate::app) fn with_review_cards(
        &self,
        diff: Diff,
        list: FileList,
        cx: &Context<Self>,
    ) -> Diff {
        let submodules = self.submodules(list);
        let inline = (list == FileList::Pull)
            .then(|| self.inline_review(cx))
            .flatten();
        if submodules.is_empty() && inline.is_none() {
            return diff;
        }
        let submodules = Rc::new(submodules.to_vec());
        let cards = submodules.clone();
        let tags = submodules.clone();
        let notes = submodules
            .iter()
            .map(|module| {
                DiffAnnotation::file(crate::submodules::card_id(module), module.path.clone())
            })
            .chain(
                inline
                    .iter()
                    .flat_map(|(_, threads)| threads.iter().map(|entry| entry.annotation.clone())),
            )
            .collect::<Vec<_>>();
        let owner = cx.entity().downgrade();
        diff.annotations(notes)
            .render_annotation(move |note, _, cx| {
                if let Some((pr, threads)) = &inline
                    && let Some((index, entry)) = threads
                        .iter()
                        .enumerate()
                        .find(|(_, entry)| entry.annotation.id() == note.id())
                {
                    return owner
                        .update(cx, |this, cx| {
                            let theme = cx.theme().clone();
                            this.render_thread(pr, index, &entry.thread, true, &theme, cx)
                        })
                        .unwrap_or_else(|_| div().into_any_element());
                }
                cards
                    .iter()
                    .find(|module| module.path == note.path().as_ref())
                    .map(|module| crate::submodules::card(module, cx.theme()))
                    .unwrap_or_else(div)
                    .into_any_element()
            })
            .render_header_title_suffix(move |file, _, cx| {
                if tags
                    .iter()
                    .any(|module| module.path == file.path().as_ref())
                {
                    crate::submodules::tag(cx.theme())
                } else {
                    div()
                }
            })
    }
}
