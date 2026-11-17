use super::*;

impl SuperGitApp {
    fn review_worktree(&self, pr: &PullRequest) -> Option<&WorktreeInfo> {
        let available = |info: &&WorktreeInfo| {
            info.status.is_ok()
                && !info.wt.is_bare
                && info.wt.prunable.is_none()
                && !self.cleanup_paths.contains(info.path())
                && !self.base_update_paths.contains(info.path())
                && self.pr_linked(info, pr)
        };
        if let Some(selected) = self.selected_info().filter(available) {
            return Some(selected);
        }
        let mut candidates = self.worktrees.iter().filter(available);
        let worktree = candidates.next()?;
        candidates.next().is_none().then_some(worktree)
    }

    fn attach_review_context(
        &mut self,
        pr: &PullRequest,
        thread: &ReviewThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.review_worktree(pr) else {
            self.flash(
                "Select a worktree for this PR before attaching its review",
                true,
                cx,
            );
            return;
        };
        let path = target.wt.path.clone();
        if self.selected.as_ref() != Some(&path) {
            self.select_worktree(path.clone(), cx);
            if self.selected.as_ref() == Some(&path) {
                self.tab = MainTab::PullRequests;
                self.select_pr(path.clone(), pr.number, cx);
            }
        }
        let attachment = agent_context::review_attachment(path, pr, thread);
        self.attach_context(attachment, false, window, cx);
    }

    pub(super) fn review_context_button(
        &self,
        pr: &PullRequest,
        index: usize,
        thread: &ReviewThread,
        cx: &mut Context<Self>,
    ) -> Button {
        let target = self.review_worktree(pr);
        let tooltip = target.map_or_else(
            || "Select a worktree linked to this PR or check out its branch".to_string(),
            |info| format!("Attach this review to a chat in {}", info.wt.display_name()),
        );
        let root = self.active_root().map(Path::to_path_buf);
        let pr = pr.clone();
        let thread = thread.clone();
        Button::new(("pr-thread-ask", index))
            .label("Ask agent about thread")
            .xsmall()
            .outline()
            .tooltip(tooltip)
            .disabled(target.is_none())
            .on_click(cx.listener(move |this, _, window, cx| {
                if this.active_root() == root.as_deref() {
                    this.attach_review_context(&pr, &thread, window, cx);
                }
            }))
    }
}
