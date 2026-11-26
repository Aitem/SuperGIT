use gpui_kit::component::input::{Textarea, TextareaState};
use sg_github::{ReviewLine, ReviewSide};

use super::*;

pub(super) struct LineComment {
    root: PathBuf,
    target: ReviewLine,
    preview: String,
    text: Entity<TextareaState>,
    error: Option<String>,
    previous_focus: Option<FocusHandle>,
    _subscription: Subscription,
}

/// GitHub accepts left-side deletions and right-side additions/context.
fn github_line(hunk: &str, side: DiffSide, number: usize) -> Option<(ReviewSide, usize, &str)> {
    let mut lines = hunk.lines().skip_while(|line| !line.starts_with("@@ "));
    let mut header = lines.next()?.split_whitespace().skip(1);
    let start = |field: &str| field.get(1..)?.split(',').next()?.parse::<usize>().ok();
    let mut left = start(header.next()?)?;
    let mut right = start(header.next()?)?;
    for line in lines {
        let content = line.get(1..)?;
        match line.as_bytes()[0] {
            b'-' => {
                if side == DiffSide::Original && number == left {
                    return Some((ReviewSide::Original, left, content));
                }
                left += 1;
            }
            b'+' => {
                if side == DiffSide::Modified && number == right {
                    return Some((ReviewSide::Modified, right, content));
                }
                right += 1;
            }
            b' ' => {
                let selected = if side == DiffSide::Original {
                    left
                } else {
                    right
                };
                if number == selected {
                    return Some((ReviewSide::Modified, right, content));
                }
                left += 1;
                right += 1;
            }
            b'\\' => {}
            _ => return None,
        }
    }
    None
}

impl SuperGitApp {
    fn pr_comment_patch(&self) -> Option<(&PrDetails, &Patch)> {
        if self.tab != ReviewTab::PullRequests
            || self.pulls.tab != DetailsTab::Files
            || self.difftastic_active() && !self.pr_difft_unavailable()
        {
            return None;
        }
        let (number, head) = self.pulls.diff_shown.as_ref()?;
        if self.pulls.shown != Some(*number) {
            return None;
        }
        let loaded = &self.pulls.details.get(number)?.details;
        let pr = loaded.value()?;
        let patch = self.pulls.patches.get(number)?;
        (loaded.error().is_none()
            && patch.patch.error().is_none()
            && !patch.local
            && pr.pr.is_open()
            && pr.pr.head_oid == *head
            && patch.head == *head
            && !pr.base_oid.is_empty()
            && pr.base_oid == patch.base_oid)
            .then_some((pr, patch))
    }

    pub(in crate::app) fn line_comment_button(&self, cx: &mut Context<Self>) -> Option<Button> {
        let (pr, _) = self.pr_comment_patch()?;
        sg_github::pull_request_repository(&pr.pr)?;
        let selected = self
            .pulls
            .diff
            .read(cx)
            .selected_lines()
            .is_some_and(|range| range.side() == range.end_side() && range.start() == range.end());
        Some(
            Button::new("add-pr-line-comment")
                .label("Comment")
                .xsmall()
                .ghost()
                .tooltip("Select one line number to start a review comment")
                .disabled(!selected || self.pulls.line_comment_task.is_some())
                .on_click(cx.listener(|this, _, window, cx| this.open_line_comment(window, cx))),
        )
    }

    fn line_comment_target(&self, cx: &App) -> Option<(ReviewLine, String)> {
        let (pr, patch) = self.pr_comment_patch()?;
        let range = self.pulls.diff.read(cx).selected_lines()?;
        if range.side() != range.end_side() || range.start() != range.end() {
            return None;
        }
        let path = range.path().as_ref();
        if sg_git::submodule_of(&patch.submodules, path).is_some_and(|module| module.path != path) {
            return None;
        }
        let file = patch.file_patch(path)?;
        let hunk =
            agent_context::hunk_patch(file, range.side() == DiffSide::Original, range.start())?;
        let (side, line, content) = github_line(&hunk, range.side(), range.start())?;
        let target = ReviewLine {
            repository: sg_github::pull_request_repository(&pr.pr)?,
            number: pr.pr.number,
            head: pr.pr.head_oid.clone(),
            base: pr.base_oid.clone(),
            path: path.to_owned(),
            line,
            side,
        };
        Some((target, content.chars().take(300).collect()))
    }

    fn open_line_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pulls.dialog_open()
            || self.pulls.line_comment.is_some()
            || self.pulls.line_comment_task.is_some()
        {
            return;
        }
        let Some(root) = self.active_root().map(Path::to_path_buf) else {
            return;
        };
        let Some((target, preview)) = self.line_comment_target(cx) else {
            self.flash("Select a line in the GitHub diff to comment", true, cx);
            return;
        };
        let text = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Write a review comment")
                .rows(4)
        });
        let subscription = cx.subscribe(&text, |_, _, _: &InputEvent, cx| cx.notify());
        self.pulls.line_comment = Some(LineComment {
            root,
            target,
            preview,
            text: text.clone(),
            error: None,
            previous_focus: window.focused(cx),
            _subscription: subscription,
        });
        text.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn close_line_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pulls.line_comment_task.is_some() {
            return;
        }
        if let Some(focus) = self
            .pulls
            .line_comment
            .take()
            .and_then(|dialog| dialog.previous_focus)
        {
            focus.focus(window, cx);
        }
        cx.notify();
    }

    fn submit_line_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pulls.line_comment_task.is_some() {
            return;
        }
        let Some(dialog) = self.pulls.line_comment.as_ref() else {
            return;
        };
        let text = dialog.text.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        let root = dialog.root.clone();
        let key = dialog.text.clone();
        let target = dialog.target.clone();
        let active = self.active_root() == Some(root.as_path())
            && self.pr_comment_patch().is_some_and(|(pr, _)| {
                pr.pr.number == target.number
                    && pr.pr.head_oid == target.head
                    && pr.base_oid == target.base
            });
        if !active {
            if let Some(dialog) = &mut self.pulls.line_comment {
                dialog.error = Some(
                    "The shown diff changed. Reopen the comment on the current revision.".into(),
                );
            }
            cx.notify();
            return;
        }
        if let Some(dialog) = &mut self.pulls.line_comment {
            dialog.error = None;
        }
        let number = target.number;
        self.pulls.line_comment_task = Some(cx.spawn_in(window, async move |this, cx| {
            let request = root.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    sg_github::post_review_line(&request, &target, &text)
                        .map_err(model::git_error_message)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.pulls.line_comment_task = None;
                let same_dialog = this
                    .pulls
                    .line_comment
                    .as_ref()
                    .is_some_and(|dialog| dialog.text == key);
                match result {
                    Ok(()) => {
                        if same_dialog {
                            this.close_line_comment(window, cx);
                        }
                        if this.active_root() == Some(root.as_path()) {
                            this.load_pr_details(root, number, cx);
                        }
                        this.flash(format!("Review comment posted on PR #{number}"), false, cx);
                    }
                    Err(error) if same_dialog => {
                        if let Some(dialog) = &mut this.pulls.line_comment {
                            dialog.error = Some(error);
                        }
                    }
                    Err(error) => this.flash(error, true, cx),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(in crate::app) fn line_comment_open(&self) -> bool {
        self.pulls.line_comment.is_some()
    }

    pub(in crate::app) fn render_line_comment(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.pulls.line_comment else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let target = &dialog.target;
        let posting = self.pulls.line_comment_task.is_some();
        let empty = dialog.text.read(cx).value().trim().is_empty();
        let side = match target.side {
            ReviewSide::Original => "original",
            ReviewSide::Modified => "modified",
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(64.))
            .bg(gpui_kit::black().opacity(0.35))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_line_comment(window, cx)),
            )
            .child(
                v_flex()
                    .w(px(555.))
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
                            this.close_line_comment(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child(format!("Review comment · PR #{}", target.number)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "{} · {side} line {} · {}",
                                target.path,
                                target.line,
                                &target.head[..target.head.len().min(8)]
                            )),
                    )
                    .child(
                        div()
                            .rounded_md()
                            .p_2()
                            .bg(theme.muted)
                            .text_xs()
                            .font_family(theme.mono_font_family.clone())
                            .child(dialog.preview.clone()),
                    )
                    .child(Textarea::new(&dialog.text).disabled(posting))
                    .when_some(dialog.error.as_ref(), |el, error| {
                        el.child(
                            div()
                                .text_sm()
                                .text_color(theme.danger)
                                .child(error.clone()),
                        )
                    })
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("cancel-pr-line-comment")
                                    .label("Cancel")
                                    .small()
                                    .ghost()
                                    .disabled(posting)
                                    .tooltip(if posting {
                                        "Wait until GitHub has the comment"
                                    } else {
                                        "Close without posting (Esc)"
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_line_comment(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("submit-pr-line-comment")
                                    .label(if posting {
                                        "Posting…"
                                    } else {
                                        "Post comment"
                                    })
                                    .small()
                                    .primary()
                                    .disabled(posting || empty)
                                    .tooltip(if posting {
                                        "Sending the comment to GitHub"
                                    } else if empty {
                                        "Write the comment first"
                                    } else {
                                        "Publish it on this line of the pull request on GitHub"
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_line_comment(window, cx)
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{DiffSide, ReviewSide, github_line};

    #[test]
    fn maps_removed_added_and_shifted_context_lines() {
        let hunk = "@@ -3,4 +8,5 @@\n unchanged\n-old\n+new\n+extra\n final\n tail\n";
        assert_eq!(
            github_line(hunk, DiffSide::Original, 4),
            Some((ReviewSide::Original, 4, "old"))
        );
        assert_eq!(
            github_line(hunk, DiffSide::Modified, 10),
            Some((ReviewSide::Modified, 10, "extra"))
        );
        assert_eq!(
            github_line(hunk, DiffSide::Original, 5),
            Some((ReviewSide::Modified, 11, "final"))
        );
        assert_eq!(github_line(hunk, DiffSide::Modified, 100), None);
    }
}
