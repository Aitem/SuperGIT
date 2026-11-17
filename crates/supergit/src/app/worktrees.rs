//! Worktree organization and cleanup previews.

use super::*;

#[derive(Clone)]
struct CleanupTarget {
    path: PathBuf,
    branch: Option<String>,
    tip: Option<String>,
    reason: Option<String>,
    plan: Option<sg_git::CleanupPlan>,
}

#[derive(Clone)]
pub(super) struct ObservedPr {
    identity: sg_git::WorktreeIdentity,
    path: PathBuf,
    branch: String,
    tip: String,
    repository: String,
    invalidated: bool,
}

pub(super) struct CleanupPreview {
    repository: PathBuf,
    targets: Vec<CleanupTarget>,
    previous_focus: Option<FocusHandle>,
}

impl SuperGitApp {
    pub(super) fn toggle_auto_cleanup(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = &self.repo else {
            return;
        };
        let view = self
            .persisted
            .repositories
            .entry(repo.root.clone())
            .or_default();
        view.auto_cleanup = !view.auto_cleanup;
        self.observed_open_prs.clear();
        self.cleanup_attempted.clear();
        self.queue_persist(cx);
        self.observe_cleanup_prs(cx);
        cx.notify();
    }

    pub(super) fn observe_cleanup_prs(&mut self, cx: &mut Context<Self>) {
        if !self.repository_view().auto_cleanup || self.prs_error.is_some() {
            return;
        }
        let mut plans = Vec::new();
        let mut proof = Vec::new();
        for pr in &self.prs {
            let Some(info) = self
                .worktrees
                .iter()
                .find(|info| info.wt.branch.as_deref() == Some(pr.head_branch.as_str()))
            else {
                continue;
            };
            let Some(identity) = info.cleanup_identity.clone() else {
                continue;
            };
            let Some(tip) = info
                .status
                .as_ref()
                .ok()
                .and_then(|status| status.head_oid.clone())
            else {
                continue;
            };
            let Some(repository) = sg_github::pull_request_repository(pr) else {
                continue;
            };
            if matches!(pr.state, PrState::Open | PrState::Draft) {
                if tip != pr.head_oid
                    || pr
                        .head_repository
                        .as_deref()
                        .is_none_or(|head| !head.eq_ignore_ascii_case(&repository))
                    || pr
                        .created_at
                        .is_none_or(|created| !identity.predates(created))
                {
                    continue;
                }
                let invalidated = self.observed_open_prs.get(&pr.number).is_some_and(|old| {
                    old.invalidated
                        || old.path != info.wt.path
                        || old.branch != pr.head_branch
                        || old.repository != repository
                        || !old.identity.same_lifecycle(&identity)
                });
                self.observed_open_prs.insert(
                    pr.number,
                    ObservedPr {
                        identity,
                        path: info.wt.path.clone(),
                        branch: pr.head_branch.clone(),
                        tip,
                        repository,
                        invalidated,
                    },
                );
            } else if pr.state == PrState::Merged {
                let token = format!("{}:{}:{}", pr.number, tip, info.path().display());
                let eligible = self.observed_open_prs.get(&pr.number).is_some_and(|old| {
                    !old.invalidated
                        && old.path == info.wt.path
                        && old.branch == pr.head_branch
                        && old.repository == repository
                        && old.identity == identity
                        && old.tip == tip
                }) && self.selected.as_deref() != Some(info.path())
                    && self.is_completed(info, cx)
                    && !self.cleanup_attempted.contains(&token);
                if eligible {
                    let target = self.cleanup_target(info, cx);
                    if target.reason.is_none()
                        && let Some(plan) = target.plan
                    {
                        plans.push(plan);
                        proof.push(pr.clone());
                    }
                }
            }
        }
        if plans.is_empty() || self.cleanup_task.is_some() || self.action_task.is_some() {
            return;
        }
        for (plan, pr) in plans.iter().zip(&proof) {
            self.cleanup_attempted.insert(format!(
                "{}:{}:{}",
                pr.number,
                plan.tip,
                plan.path.display()
            ));
        }
        self.begin_cleanup(plans, true, Some(proof), cx);
    }

    fn toggle_cleanup_protection(&mut self, branch: &str, cx: &mut Context<Self>) {
        let Some(repo) = &self.repo else {
            return;
        };
        let view = self
            .persisted
            .repositories
            .entry(repo.root.clone())
            .or_default();
        if view.protected_branches.iter().any(|name| name == branch) {
            view.protected_branches.retain(|name| name != branch);
        } else {
            view.protected_branches.push(branch.to_string());
        }
        self.queue_persist(cx);
        cx.notify();
    }

    pub(super) fn render_cleanup_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.repo.is_none() {
            return div().into_any_element();
        }
        let view = self.repository_view();
        let mut branches = view.protected_branches.clone();
        branches.extend(
            self.worktrees
                .iter()
                .filter_map(|info| info.wt.branch.clone()),
        );
        branches.sort();
        branches.dedup();
        let theme = cx.theme().clone();
        v_flex().gap_2()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Worktree cleanup · this repository"))
            .child(h_flex().justify_between().child("After a PR merges")
                .child(Button::new("auto-cleanup").small().ghost().label(if view.auto_cleanup {"On"} else {"Off"})
                    .on_click(cx.listener(|this,_,_,cx| this.toggle_auto_cleanup(cx)))))
            .child(div().text_sm().text_color(theme.muted_foreground).child(
                "Only PRs observed open in this app session qualify. Selected, dirty, busy, kept, protected or unread work stays. Squash and rebase merges need manual review."))
            .child(div().text_sm().child("Protected local branches (the main worktree and default base are always protected)"))
            .child(v_flex().id("cleanup-protected-branches").max_h(px(130.)).overflow_y_scroll().gap_1()
                .children(branches.into_iter().enumerate().map(|(ix,branch)| {
                    let protected = view.protected_branches.contains(&branch);
                    h_flex().justify_between().gap_2().child(div().truncate().child(branch.clone()))
                        .child(Button::new(("cleanup-protection",ix)).small().ghost().label(if protected {"Protected"} else {"Protect"})
                            .on_click(cx.listener(move |this,_,_,cx| this.toggle_cleanup_protection(&branch,cx))))
                })))
            .child(self.render_pending_cleanup(cx)).into_any_element()
    }

    pub(super) fn cleanup_policy(&self, automatic: bool) -> sg_git::CleanupPolicy {
        let view = self.repository_view();
        let mut protected = view.protected_branches.clone();
        protected.extend(
            self.worktrees
                .iter()
                .filter(|info| self.kept_active(info.path()))
                .filter_map(|info| info.wt.branch.clone()),
        );
        sg_git::CleanupPolicy {
            protected_branches: protected,
            base_branch: view.base_branch.clone(),
            automatic,
        }
    }

    fn cleanup_target(&self, info: &WorktreeInfo, cx: &App) -> CleanupTarget {
        let plan = self
            .repo
            .as_ref()
            .zip(info.status.as_ref().ok())
            .zip(info.cleanup_identity.clone())
            .and_then(|((repo, status), identity)| {
                sg_git::CleanupPlan::snapshot(repo, &info.wt, status, identity).ok()
            });
        let reason = if info.wt.is_main {
            Some("The main worktree is protected")
        } else if info.wt.locked.is_some() {
            Some("This worktree is locked")
        } else if info.wt.prunable.is_some() || info.status.is_err() {
            Some("Worktree status is unavailable")
        } else if self.cleanup_paths.contains(info.path()) {
            Some("Cleanup is already in progress")
        } else if self.kept_active(info.path()) {
            Some("Kept active")
        } else if info
            .wt
            .branch
            .as_ref()
            .is_some_and(|branch| self.repository_view().protected_branches.contains(branch))
        {
            Some("This local branch is protected")
        } else if self.worktree_has_draft(info.path(), cx) {
            Some("An unsent conversation draft must be preserved")
        } else if self.worktree_busy(info, cx) {
            Some("An agent is running or waiting for approval")
        } else if info.status.as_ref().is_ok_and(|status| !status.is_clean()) {
            Some("Local changes must be preserved")
        } else if plan.is_none() {
            Some("Cleanup identity is unavailable. Refresh, or create a branch for detached work.")
        } else {
            None
        }
        .map(str::to_string);
        CleanupTarget {
            path: info.wt.path.clone(),
            branch: info.wt.branch.clone(),
            tip: info
                .status
                .as_ref()
                .ok()
                .and_then(|status| status.head_oid.clone()),
            reason,
            plan,
        }
    }

    pub(super) fn confirm_cleanup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(preview) = &self.cleanup_preview else {
            return;
        };
        if self
            .repo
            .as_ref()
            .is_none_or(|repo| repo.root != preview.repository)
        {
            return;
        }
        let plans: Vec<_> = preview
            .targets
            .iter()
            .filter(|target| target.reason.is_none())
            .filter_map(|target| target.plan.clone())
            .collect();
        if plans.len() != preview.targets.len() || plans.is_empty() {
            return;
        }
        self.close_cleanup_preview(window, cx);
        self.begin_cleanup(plans, false, None, cx);
    }

    fn begin_cleanup(
        &mut self,
        plans: Vec<sg_git::CleanupPlan>,
        automatic: bool,
        proof: Option<Vec<PullRequest>>,
        cx: &mut Context<Self>,
    ) {
        if plans.is_empty() || self.cleanup_task.is_some() || self.action_task.is_some() {
            return;
        }
        for plan in &plans {
            let valid = self
                .worktrees
                .iter()
                .find(|info| info.path() == plan.path)
                .is_some_and(|info| self.cleanup_target(info, cx).reason.is_none());
            if !valid {
                self.flash(
                    "Cleanup stopped: a target changed. Preview it again.",
                    true,
                    cx,
                );
                return;
            }
        }
        let policy = self.cleanup_policy(automatic);
        let root = plans[0].root.clone();
        self.cleanup_paths
            .extend(plans.iter().map(|plan| plan.path.clone()));
        if self
            .selected
            .as_ref()
            .is_some_and(|path| self.cleanup_paths.contains(path))
        {
            let fallback = self
                .worktrees
                .iter()
                .find(|info| {
                    !self.cleanup_paths.contains(info.path())
                        && !info.wt.is_bare
                        && info.wt.prunable.is_none()
                        && info.status.is_ok()
                })
                .map(|info| info.wt.path.clone());
            if let Some(fallback) = fallback {
                self.select_worktree(fallback, cx);
            } else {
                self.cache_review(cx);
                self.selected = None;
                self.reset_details(cx);
            }
        }
        let mut processes = Vec::new();
        for plan in &plans {
            if let Some(chats) = self.chats.get(&plan.path) {
                for chat in chats {
                    match chat.update(cx, |chat, cx| chat.take_idle_process(cx)) {
                        Ok(Some(process)) => processes.push(process),
                        Ok(None) => {}
                        Err(error) => {
                            self.cleanup_paths.clear();
                            self.flash(error.to_string(), true, cx);
                            return;
                        }
                    }
                }
            }
        }
        self.flash(
            format!("Cleaning up {} worktree(s)…", plans.len()),
            false,
            cx,
        );
        self.cleanup_task=Some(cx.spawn(async move |this,cx| {
            let results=cx.background_executor().spawn(async move {
                for process in processes {process.kill();}
                let fetch=proof.map(|proof| (|| -> anyhow::Result<()> {
                    let origin=sg_git::git(&root,["remote","get-url","origin"])?;
                    anyhow::ensure!(plans.iter().all(|plan| proof.iter().any(|pr|
                        sg_github::merged_pr_matches(pr,origin.trim(),&plan.branch,&plan.tip,std::time::SystemTime::now()))),
                        "Merged PR proof no longer matches the repository and local branch");
                    sg_git::fetch_cleanup_base(&root,&policy,origin.trim())
                })().map_err(model::git_error_message));
                let results=plans.into_iter().map(|plan| {
                    let result=if let Some(Err(error))=&fetch {Err(error.clone())}
                        else {sg_git::cleanup_worktree(&plan,&policy).map_err(model::git_error_message)};
                    (plan,result)
                }).collect::<Vec<_>>();
                (root,results)
            }).await;
            let _=this.update(cx,|this,cx| {
                this.cleanup_task=None;
                let (root,results)=results;
                let mut rows=Vec::new();
                let mut removed=std::collections::HashSet::new();
                let view=this.persisted.repositories.entry(root.clone()).or_default();
                for (plan,result) in results {
                    this.cleanup_paths.remove(&plan.path);
                    match result {
                        Ok(outcome)=>{
                            removed.insert(plan.path.clone());
                            view.worktrees.remove(&plan.path);view.conversations.remove(&plan.path);view.active_conversations.remove(&plan.path);
                            view.pending_branch_cleanup.retain(|pending| pending.deletion.branch != plan.branch);
                            match outcome {
                                sg_git::CleanupOutcome::Complete=>rows.push((format!("{}: worktree and local branch '{}' removed",plan.path.display(),plan.branch),false)),
                                sg_git::CleanupOutcome::BranchPreserved {pending,error}=>{
                                    rows.push((format!("{}: worktree removed; branch '{}' kept. {error}",plan.path.display(),plan.branch),true));
                                    view.pending_branch_cleanup.push(workspace::PendingBranchCleanup {deletion:pending,error});
                                }
                            }
                        }
                        Err(error)=>rows.push((format!("{}: cleanup stopped. {error}",plan.path.display()),true)),
                    }
                }
                workspace::save_repository(&root,view);
                let same_repository=this.repo.as_ref().is_some_and(|repo| repo.root == root);
                if same_repository {
                    for path in &removed {this.chats.remove(path);this.active_chat.remove(path);this.chat_projects.remove(path);this.review_views.remove(path);}
                    this.worktrees.retain(|info| !removed.contains(info.path()));
                }
                let errors=rows.iter().filter(|(_,error)| *error).count();
                this.cleanup_results=rows;
                this.cleanup_results_open=!automatic && errors>0 && same_repository;
                this.flash(if errors>0 {format!("Cleanup finished with {errors} preserved or incomplete target(s). Open Cleanup details.")}
                    else {format!("Cleaned up {} worktree(s) and their local branches",removed.len())},errors>0,cx);
                if this.cleanup_results_open && let Some(window)=cx.windows().first().copied() {
                    let focus=this.cleanup_handle.clone();let _=window.update(cx,|_,window,cx| focus.focus(window,cx));
                }
                this.queue_persist(cx);this.start_polling(cx);
            });
        }));
    }

    pub(super) fn retry_cleanup_branch(
        &mut self,
        pending: sg_git::BranchDeletion,
        cx: &mut Context<Self>,
    ) {
        if self.cleanup_task.is_some() || self.action_task.is_some() {
            return;
        }
        let policy = self.cleanup_policy(false);
        self.flash(
            format!("Retrying local branch '{}'…", pending.branch),
            false,
            cx,
        );
        self.cleanup_task = Some(cx.spawn(async move |this, cx| {
            let copy = pending.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    sg_git::delete_cleanup_branch(&copy, &policy).map_err(model::git_error_message)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.cleanup_task = None;
                let view = this
                    .persisted
                    .repositories
                    .entry(pending.root.clone())
                    .or_default();
                let error = result.as_ref().err().cloned();
                if let Some(error) = &error {
                    if let Some(item) = view
                        .pending_branch_cleanup
                        .iter_mut()
                        .find(|item| item.deletion == pending)
                    {
                        item.error = error.clone();
                    }
                } else {
                    view.pending_branch_cleanup
                        .retain(|item| item.deletion != pending);
                }
                workspace::save_repository(&pending.root, view);
                let message = match error {
                    Some(error) => {
                        format!("Local branch '{}' was preserved: {error}", pending.branch)
                    }
                    None => format!(
                        "Local branch '{}' removed; no worktree was touched",
                        pending.branch
                    ),
                };
                this.cleanup_results = vec![(message.clone(), result.is_err())];
                this.flash(message, result.is_err(), cx);
                this.start_polling(cx);
            });
        }));
    }

    pub(super) fn keep_cleanup_branch(
        &mut self,
        pending: sg_git::BranchDeletion,
        cx: &mut Context<Self>,
    ) {
        if self.cleanup_task.is_some() {
            return;
        }
        if let Some(view) = self.persisted.repositories.get_mut(&pending.root) {
            view.pending_branch_cleanup
                .retain(|item| item.deletion != pending);
            workspace::save_repository(&pending.root, view);
            cx.notify();
        }
    }

    pub(super) fn render_pending_cleanup(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let rows = self
            .repository_view()
            .pending_branch_cleanup
            .iter()
            .enumerate()
            .map(|(ix, item)| {
                let retry = item.deletion.clone();
                let keep = item.deletion.clone();
                v_flex()
                    .gap_1()
                    .p_2()
                    .border_1()
                    .border_color(theme.border)
                    .rounded_md()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("Local branch '{}' was kept", item.deletion.branch)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(item.error.clone()),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new(("retry-branch-cleanup", ix))
                                    .label("Retry branch only")
                                    .small()
                                    .ghost()
                                    .disabled(self.cleanup_task.is_some())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.retry_cleanup_branch(retry.clone(), cx)
                                    })),
                            )
                            .child(
                                Button::new(("keep-cleanup-branch", ix))
                                    .label("Keep branch")
                                    .small()
                                    .ghost()
                                    .disabled(self.cleanup_task.is_some())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.keep_cleanup_branch(keep.clone(), cx)
                                    })),
                            ),
                    )
            });
        v_flex()
            .id("pending-branch-cleanup")
            .max_h(px(180.))
            .overflow_y_scroll()
            .gap_2()
            .children(rows)
            .into_any_element()
    }

    pub(super) fn close_cleanup_results(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cleanup_results_open = false;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    pub(super) fn render_cleanup_results(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
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
                cx.listener(|this, _, window, cx| this.close_cleanup_results(window, cx)),
            )
            .child(
                v_flex()
                    .track_focus(&self.cleanup_handle)
                    .w(px(600.))
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
                            this.close_cleanup_results(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("Cleanup results"),
                    )
                    .child(
                        v_flex()
                            .id("cleanup-results")
                            .max_h(px(300.))
                            .overflow_y_scroll()
                            .gap_2()
                            .children(self.cleanup_results.iter().map(|(message, error)| {
                                div()
                                    .text_sm()
                                    .text_color(if *error {
                                        theme.warning
                                    } else {
                                        theme.foreground
                                    })
                                    .child(message.clone())
                            })),
                    )
                    .child(self.render_pending_cleanup(cx))
                    .child(
                        Button::new("close-cleanup-results")
                            .label("Done")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_cleanup_results(window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }
    pub(super) fn kept_active(&self, path: &Path) -> bool {
        self.repository_view()
            .worktrees
            .get(path)
            .is_some_and(|view| view.kept)
    }

    pub(super) fn worktree_busy(&self, info: &WorktreeInfo, cx: &App) -> bool {
        matches!(
            self.agent_state_for(info, cx),
            Some(AgentState::Working | AgentState::NeedsAttention)
        )
    }

    pub(super) fn worktree_has_draft(&self, path: &Path, cx: &App) -> bool {
        let saved = self.repository_view().conversations.get(path);
        let native = self
            .chats
            .get(path)
            .map(|chats| chats.iter().any(|chat| chat.read(cx).has_draft(cx)))
            .unwrap_or_else(|| {
                saved
                    .into_iter()
                    .flatten()
                    .filter(|chat| native_provider(&chat.provider))
                    .any(|chat| !chat.draft.is_empty() || !chat.attachments.is_empty())
            });
        native
            || saved
                .into_iter()
                .flatten()
                .filter(|chat| !native_provider(&chat.provider))
                .any(|chat| !chat.draft.is_empty() || !chat.attachments.is_empty())
    }

    pub(super) fn is_completed(&self, info: &WorktreeInfo, cx: &App) -> bool {
        let pr = self.pr_for_branch(info.wt.branch.as_deref());
        crate::lifecycle::Completion {
            main: info.wt.is_main,
            kept: self.kept_active(info.path()),
            locked: info.wt.locked.is_some(),
            available: !info.wt.is_bare && info.wt.prunable.is_none() && info.status.is_ok(),
            clean: info.status.as_ref().is_ok_and(|status| status.is_clean()),
            busy: self.worktree_busy(info, cx),
            unread: matches!(
                self.native_attention_for(info.path(), cx),
                Some(Kind::Completed | Kind::Failed)
            ),
            pending_draft: self.worktree_has_draft(info.path(), cx),
            prs_fresh: self.prs_error.is_none()
                && self
                    .prs_fetched_at
                    .is_some_and(|time| time.elapsed() < PR_POLL_INTERVAL * 2),
            merged: pr.is_some_and(|pr| pr.state == PrState::Merged),
            local_tip: info
                .status
                .as_ref()
                .ok()
                .and_then(|status| status.head_oid.as_deref()),
            merged_tip: pr.map(|pr| pr.head_oid.as_str()),
        }
        .completed()
    }

    pub(super) fn set_completed_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        if let Some(repo) = &self.repo {
            self.persisted
                .repositories
                .entry(repo.root.clone())
                .or_default()
                .completed_collapsed = collapsed;
            self.queue_persist(cx);
            cx.notify();
        }
    }

    pub(super) fn toggle_keep_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.cleanup_paths.contains(&path) {
            return;
        }
        if let Some(repo) = &self.repo {
            let view = self
                .persisted
                .repositories
                .entry(repo.root.clone())
                .or_default()
                .worktrees
                .entry(path)
                .or_default();
            view.kept = !view.kept;
            self.queue_persist(cx);
            cx.notify();
        }
    }

    pub(super) fn toggle_keep_worktree(
        &mut self,
        _: &ToggleKeepWorktree,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = self.selected.clone() {
            self.toggle_keep_path(path, cx);
        }
    }

    pub(super) fn clean_up_worktree(
        &mut self,
        _: &CleanUpWorktree,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = self.selected.clone() {
            self.open_cleanup_preview(vec![path], window, cx);
        }
    }

    pub(super) fn open_cleanup_preview(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.cleanup_task.is_some() || self.action_task.is_some() {
            self.flash("Another Git operation is in progress", true, cx);
            return;
        }
        let Some(repository) = self.repo.as_ref().map(|repo| repo.root.clone()) else {
            return;
        };
        self.cleanup_results_open = false;
        self.settings_open = false;
        self.close_switcher(&CloseSwitcher, window, cx);
        let targets = paths
            .iter()
            .filter_map(|path| self.worktrees.iter().find(|info| info.path() == path))
            .map(|info| self.cleanup_target(info, cx))
            .collect();
        self.cleanup_preview = Some(CleanupPreview {
            repository,
            targets,
            previous_focus: Some(self.focus_handle.clone()),
        });
        self.cleanup_handle.focus(window, cx);
        cx.notify();
    }

    pub(super) fn close_cleanup_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(preview) = self.cleanup_preview.take() {
            preview
                .previous_focus
                .unwrap_or_else(|| self.focus_handle.clone())
                .focus(window, cx);
            cx.notify();
        }
    }

    pub(super) fn render_cleanup_preview(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(preview) = &self.cleanup_preview else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(80.))
            .bg(gpui_kit::black().opacity(0.35))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_cleanup_preview(window, cx)),
            )
            .child(
                v_flex()
                    .track_focus(&self.cleanup_handle)
                    .w(px(560.))
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
                            this.close_cleanup_preview(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("Clean up worktree and local branch?"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(preview.repository.display().to_string()),
                    )
                    .child(div().text_sm().text_color(theme.muted_foreground).child("Removes the listed local directories and safely deletes their local branches. Idle agent sessions are stopped. Local changes block cleanup."))
                    .child(
                        v_flex()
                            .id("cleanup-targets")
                            .max_h(px(360.))
                            .overflow_y_scroll()
                            .gap_3()
                            .children(preview.targets.iter().map(|target| {
                                v_flex()
                                    .gap_1()
                                    .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                                            "Local branch: {} · {}",
                                            target.branch.as_deref().unwrap_or("Detached HEAD"),
                                            target
                                                .tip
                                                .as_deref()
                                                .map(|tip| &tip[..tip.len().min(8)])
                                                .unwrap_or("unknown")
                                        )))
                                    .child(div().text_sm().child(target.path.display().to_string()))
                                    .when_some(target.reason.clone(), |row, reason| {
                                        row.child(
                                            div().text_sm().text_color(theme.warning).child(reason),
                                        )
                                    })
                            })),
                    )
                    .child(h_flex().justify_end().gap_2().child(
                        Button::new("close-cleanup-preview")
                            .label("Cancel")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_cleanup_preview(window, cx)
                            })))
                        .child(Button::new("confirm-worktree-cleanup").label("Clean up").small().danger()
                            .disabled(preview.targets.is_empty() || preview.targets.iter().any(|target| target.reason.is_some() || target.plan.is_none()))
                            .on_click(cx.listener(|this,_,window,cx| this.confirm_cleanup(window,cx))))),
            )
            .into_any_element()
    }
}
