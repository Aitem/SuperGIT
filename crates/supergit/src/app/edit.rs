use super::*;
use gpui_kit::component::input::{Textarea, TextareaState};
use sg_git::{IndexEdit, IndexHunk, WorktreeStatus};

#[derive(Default)]
pub(super) struct GitEditor {
    pub staging: bool,
    drafts: HashMap<PathBuf, Entity<TextareaState>>,
    draft_subscriptions: HashMap<PathBuf, Subscription>,
    selected_file: Option<(String, IndexEdit)>,
    hunks: LoadState<Vec<IndexHunk>>,
    hunks_task: Option<Task<()>>,
    squash_target: Option<(PathBuf, String, String, usize)>,
    squash_message: Option<Entity<TextareaState>>,
}

impl SuperGitApp {
    /// The header's actions on the worktree's branch: fetch, pull, push,
    /// rebase, and for a linked worktree, delete.
    pub(super) fn render_branch_actions(
        &self,
        info: &WorktreeInfo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let path = info.wt.path.clone();
        let branch = info.wt.branch.clone();
        let has_upstream = info.status.as_ref().is_ok_and(|st| st.upstream.is_some());
        let blocked = self.action_task.is_some()
            || self.cleanup_task.is_some()
            || self.base_update_paths.contains(&path)
            || self.cleanup_paths.contains(&path);
        let action = |id: &'static str, icon: IconName, label: &'static str| {
            Button::new(id).icon(icon).label(label).small().ghost()
        };
        let fetch_path = path.clone();
        let pull_path = path.clone();
        h_flex()
            .gap_1()
            .flex_none()
            .child(
                action("worktree-fetch", IconName::RefreshCw, "Fetch")
                    .disabled(info.wt.is_bare || blocked)
                    .tooltip("Download new commits from all remotes; your files stay as they are")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let path = fetch_path.clone();
                        this.run_git_action("Fetch".into(), None, move || sg_git::fetch(&path), cx);
                    })),
            )
            .child(
                action("worktree-pull", IconName::ArrowDownToLine, "Pull")
                    .disabled(blocked || !has_upstream || branch.is_none())
                    .tooltip(if has_upstream {
                        "Fast-forward this branch to its upstream"
                    } else {
                        "No upstream yet: publish the branch with Push"
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(branch) = branch.clone() {
                            this.run_worktree_edit(
                                pull_path.clone(),
                                "Pull",
                                None,
                                move |path| sg_git::pull(path, &branch),
                                cx,
                            );
                        }
                    })),
            )
            .child(
                action("worktree-push", IconName::ArrowUpFromLine, "Push")
                    .disabled(self.git_operation_running())
                    .tooltip("Publish this branch to a remote, with a preview")
                    .on_click(cx.listener(|this, _, window, cx| this.open_sync_dialog(window, cx))),
            )
            .child(
                action("worktree-rebase", IconName::GitBranch, "Rebase")
                    .disabled(blocked || info.wt.branch.is_none())
                    .tooltip("Replay this branch's commits onto another branch")
                    .on_click(cx.listener({
                        let path = path.clone();
                        move |this, _, window, cx| this.open_rebase_dialog(path.clone(), window, cx)
                    })),
            )
            .when(!info.wt.is_main, |el| {
                el.child(
                    action("worktree-delete", IconName::Trash, "Delete")
                        .disabled(blocked)
                        .tooltip("Remove this worktree and its local branch, after a check")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.clean_up_worktree(&CleanUpWorktree, window, cx)
                        })),
                )
            })
    }

    pub(super) fn reset_edit_selection(&mut self) {
        self.git_editor.selected_file = None;
        self.git_editor.hunks = LoadState::Loading;
        self.git_editor.hunks_task = None;
        self.git_editor.squash_target = None;
        self.git_editor.squash_message = None;
    }

    fn run_worktree_edit(
        &mut self,
        path: PathBuf,
        label: &'static str,
        clear: Option<Entity<TextareaState>>,
        op: impl FnOnce(&Path) -> anyhow::Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.action_task.is_some()
            || self.cleanup_task.is_some()
            || self.base_update_paths.contains(&path)
            || self.cleanup_paths.contains(&path)
        {
            self.flash("Another Git operation is in progress", true, cx);
            return;
        }
        if self
            .selected_info()
            .filter(|info| info.path() == path)
            .is_some_and(|info| self.worktree_busy(info, cx))
        {
            self.flash("An agent is still working in this worktree", true, cx);
            return;
        }
        let submitted = clear
            .as_ref()
            .map(|input| input.read(cx).value().to_string());
        self.base_update_paths.insert(path.clone());
        for chat in self.chats.get(&path).into_iter().flatten() {
            chat.update(cx, |chat, cx| chat.set_git_busy(true, cx));
        }
        self.flash(format!("{label}…"), false, cx);
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let target = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { op(&target) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.action_task = None;
                this.base_update_paths.remove(&path);
                for chat in this.chats.get(&path).into_iter().flatten() {
                    chat.update(cx, |chat, cx| chat.set_git_busy(false, cx));
                }
                if result.is_ok()
                    && let Some(input) = clear
                    && this.git_editor.drafts.get(&path) == Some(&input)
                    && submitted.as_deref() == Some(input.read(cx).value().as_ref())
                {
                    this.git_editor.drafts.remove(&path);
                    this.git_editor.draft_subscriptions.remove(&path);
                }
                if this.selected.as_ref() == Some(&path) {
                    if result.is_ok() && label == "Squash" {
                        this.git_editor.squash_target = None;
                        this.git_editor.squash_message = None;
                    }
                    this.details_task = None;
                    this.refresh_index_hunks(cx);
                }
                this.report(result, &format!("{label} complete"), cx);
                this.start_polling(cx);
            });
        }));
    }

    fn refresh_index_hunks(&mut self, cx: &mut Context<Self>) {
        self.git_editor.hunks_task = None;
        let (Some(worktree), Some((path, edit))) =
            (self.selected.clone(), self.git_editor.selected_file.clone())
        else {
            return;
        };
        self.git_editor.hunks = LoadState::Loading;
        self.git_editor.hunks_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    sg_git::index_hunks(&worktree, &path, edit).map_err(model::git_error_message)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.git_editor.hunks_task = None;
                this.git_editor.hunks = LoadState::from_result(result);
                cx.notify();
            });
        }));
        cx.notify();
    }

    #[cfg(feature = "screenshot")]
    pub(super) fn debug_staging_once(&mut self, cx: &mut Context<Self>) {
        if let Ok(path) = std::env::var("SUPERGIT_DEBUG_STAGING")
            && std::env::var_os("SUPERGIT_DEBUG_STAGING_DONE").is_none()
        {
            // SAFETY: isolated screenshot build, called from the UI thread.
            unsafe { std::env::set_var("SUPERGIT_DEBUG_STAGING_DONE", "1") };
            self.git_editor.staging = true;
            self.git_editor.selected_file = Some((path, IndexEdit::Stage));
            self.refresh_index_hunks(cx);
        }
    }

    pub(super) fn render_staging_switch(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_3()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("show-working-tree")
                    .label("Review changes")
                    .small()
                    .ghost()
                    .selected(!self.git_editor.staging)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.git_editor.staging = false;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("show-index-editor")
                    .label("Stage & commit")
                    .small()
                    .ghost()
                    .selected(self.git_editor.staging)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.git_editor.staging = true;
                        this.refresh_index_hunks(cx);
                    })),
            )
    }

    pub(super) fn render_index_editor(
        &mut self,
        info: &WorktreeInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Ok(status) = &info.status else {
            return self.render_data_notice(
                "Git index unavailable",
                info.status.as_ref().err().map(String::as_str),
                false,
                cx,
            );
        };
        let path = info.wt.path.clone();
        let input = if let Some(input) = self.git_editor.drafts.get(&path) {
            input.clone()
        } else {
            let input = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 5)
                    .placeholder("Commit message")
            });
            let subscription = cx.subscribe(&input, |_, _, _: &InputEvent, cx| cx.notify());
            self.git_editor.drafts.insert(path.clone(), input.clone());
            self.git_editor
                .draft_subscriptions
                .insert(path.clone(), subscription);
            input
        };
        let blocked = self.action_task.is_some()
            || self.cleanup_task.is_some()
            || self.base_update_paths.contains(&path)
            || self.cleanup_paths.contains(&path);
        let has_message = !input.read(cx).value().trim().is_empty();
        let staged = status.staged_count();
        let theme = cx.theme().clone();
        let sections = v_flex()
            .w(px(255.))
            .max_w(relative(0.45))
            .flex_none()
            .h_full()
            .min_h_0()
            .overflow_hidden()
            .border_r_1()
            .border_color(theme.border)
            .child(
                v_flex()
                    .id("index-file-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.render_index_files(status, IndexEdit::Stage, blocked, cx))
                    .child(self.render_index_files(status, IndexEdit::Unstage, blocked, cx)),
            );
        let controls = [false, true].map(|amend| {
            let branch = status.branch.clone();
            let head = status.head_oid.clone();
            let input = input.clone();
            let path = path.clone();
            let label = if amend { "Amend HEAD" } else { "Commit" };
            Button::new(if amend { "amend-head" } else { "commit-index" })
                .label(label)
                .small()
                .when(!amend, |button| button.primary())
                .disabled(
                    blocked
                        || if amend {
                            head.is_none() || (!has_message && staged == 0)
                        } else {
                            !has_message || staged == 0
                        },
                )
                .tooltip(if amend {
                    "Replace HEAD with the staged changes. Leave the message blank to keep it"
                } else {
                    "Commit staged changes"
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    let message = input.read(cx).value().trim().to_string();
                    let (branch, head) = (branch.clone(), head.clone());
                    this.run_worktree_edit(
                        path.clone(),
                        label,
                        Some(input.clone()),
                        move |path| {
                            sg_git::commit(
                                path,
                                branch.as_deref(),
                                head.as_deref(),
                                &message,
                                amend,
                            )
                        },
                        cx,
                    );
                }))
        });
        v_flex()
            .size_full()
            .min_h_0()
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(sections)
                    .child(self.render_index_hunks(&path, blocked, cx)),
            )
            .child(
                v_flex()
                    .flex_none()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(Textarea::new(&input))
                    .child(
                        h_flex()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("{staged} staged")),
                            )
                            .child(h_flex().gap_1().children(controls)),
                    ),
            )
            .into_any_element()
    }

    fn render_index_files(
        &self,
        status: &WorktreeStatus,
        edit: IndexEdit,
        blocked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let staged = edit == IndexEdit::Unstage;
        let files: Vec<_> = status
            .files
            .iter()
            .filter(|file| {
                if staged {
                    file.staged.is_some()
                } else {
                    file.unstaged.is_some()
                }
            })
            .collect();
        let label = if staged { "Unstage file" } else { "Stage file" };
        let path = self.selected.clone();
        v_flex()
            .min_w_0()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "{} · {}",
                        if staged { "Staged" } else { "Unstaged" },
                        files.len()
                    )),
            )
            .children(files.into_iter().enumerate().map(|(ix, file)| {
                let key = (file.path.clone(), edit);
                let selected = self.git_editor.selected_file.as_ref() == Some(&key);
                let target = path.clone();
                let file_edit = file.clone();
                h_flex()
                    .id((
                        "index-file",
                        ix + if staged { status.files.len() } else { 0 },
                    ))
                    .px_2()
                    .gap_1()
                    .min_w_0()
                    .cursor_pointer()
                    .when(selected, |row| row.bg(theme.accent))
                    .hover(|row| row.bg(theme.accent))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.git_editor.selected_file = Some(key.clone());
                        this.refresh_index_hunks(cx);
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .child(file.path.clone()),
                    )
                    .child(
                        Button::new("edit-index-file")
                            .label(if staged { "−" } else { "+" })
                            .small()
                            .ghost()
                            .tooltip(label)
                            .disabled(blocked)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                if let Some(path) = target.clone() {
                                    let file = file_edit.clone();
                                    this.run_worktree_edit(
                                        path,
                                        label,
                                        None,
                                        move |path| sg_git::edit_file(path, &file, edit),
                                        cx,
                                    );
                                }
                            })),
                    )
            }))
            .into_any_element()
    }

    fn render_index_hunks(
        &self,
        worktree: &Path,
        blocked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let notice = |message: &str| {
            div()
                .p_3()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(message.to_string())
        };
        let base = v_flex()
            .id("index-hunks")
            .flex_1()
            .min_w_0()
            .h_full()
            .min_h_0()
            .overflow_x_scroll()
            .overflow_y_scroll();
        let Some((path, edit)) = &self.git_editor.selected_file else {
            return base
                .child(notice("Select a file to review its hunks."))
                .into_any_element();
        };
        let base = base.child(
            div()
                .px_3()
                .py_2()
                .truncate()
                .font_weight(FontWeight::SEMIBOLD)
                .child(path.clone()),
        );
        let Some(hunks) = self.git_editor.hunks.value() else {
            return base
                .child(notice(
                    self.git_editor.hunks.error().unwrap_or("Loading hunks…"),
                ))
                .into_any_element();
        };
        let edit = *edit;
        let label = if edit == IndexEdit::Stage {
            "Stage hunk"
        } else {
            "Unstage hunk"
        };
        base.when(hunks.is_empty(), |el| {
            el.child(notice(
                "No text hunks here. Use the file action for this change.",
            ))
        })
        .children(hunks.iter().enumerate().map(|(ix, hunk)| {
            let (worktree, path, patch) = (worktree.to_path_buf(), path.clone(), hunk.clone());
            let title = hunk.text.lines().next().unwrap_or_default().to_string();
            v_flex()
                .id(("index-hunk", ix))
                .min_w_0()
                .border_t_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .p_2()
                        .gap_2()
                        .bg(theme.muted)
                        .child(div().flex_1().min_w_0().truncate().text_xs().child(title))
                        .child(
                            Button::new("edit-index-hunk")
                                .label(label)
                                .small()
                                .disabled(blocked)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let (path, patch) = (path.clone(), patch.clone());
                                    this.run_worktree_edit(
                                        worktree.clone(),
                                        label,
                                        None,
                                        move |worktree| {
                                            sg_git::edit_hunk(worktree, &path, &patch, edit)
                                        },
                                        cx,
                                    );
                                })),
                        ),
                )
                .children(hunk.text.lines().skip(1).map(|line| {
                    div()
                        .px_2()
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
                        .whitespace_nowrap()
                        .when(line.starts_with('+'), |row| {
                            row.bg(theme.success.opacity(0.12))
                        })
                        .when(line.starts_with('-'), |row| {
                            row.bg(theme.danger.opacity(0.12))
                        })
                        .child(line.to_string())
                }))
        }))
        .into_any_element()
    }

    pub(super) fn render_squash(&self, commits: &[Commit], cx: &mut Context<Self>) -> AnyElement {
        let Some(status) = self
            .selected_info()
            .and_then(|info| info.status.as_ref().ok())
        else {
            return div().into_any_element();
        };
        let (Some(path), Some(branch), Some(head), Some(selected)) = (
            &self.selected,
            &status.branch,
            &status.head_oid,
            &self.selected_commit,
        ) else {
            return div().into_any_element();
        };
        let by_sha: HashMap<_, _> = commits
            .iter()
            .map(|commit| (commit.sha.as_str(), commit))
            .collect();
        let mut next = head.as_str();
        let mut count = 0;
        let oldest = loop {
            let Some(commit) = by_sha.get(next) else {
                return div().into_any_element();
            };
            if commit.parents.len() > 1 {
                return div().into_any_element();
            }
            count += 1;
            if &commit.sha == selected {
                break commit;
            }
            let Some(parent) = commit.parents.first() else {
                return div().into_any_element();
            };
            next = parent;
        };
        if count < 2 {
            return div().into_any_element();
        }
        let (path, branch, head) = (path.clone(), branch.clone(), head.clone());
        let candidate = (path.clone(), branch.clone(), head.clone(), count);
        let published = status.upstream.is_some()
            && usize::try_from(status.ahead).is_ok_and(|ahead| ahead < count);
        let theme = cx.theme().clone();
        if let Some(input) = self.git_editor.squash_message.as_ref()
            && self.git_editor.squash_target.as_ref() == Some(&candidate)
        {
            let input = input.clone();
            return v_flex()
                .p_2()
                .gap_2()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(div().text_sm().child(format!(
                    "Replace the latest {count} commits on {branch} with one commit."
                )))
                .when(published, |panel| {
                    panel.child(div().text_xs().text_color(theme.warning).child(
                        "This range includes commits that may already be on the upstream branch.",
                    ))
                })
                .child(Textarea::new(&input))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("cancel-squash")
                                .label("Cancel")
                                .small()
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.git_editor.squash_target = None;
                                    this.git_editor.squash_message = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("confirm-squash")
                                .label(format!("Squash {count} commits"))
                                .small()
                                .primary()
                                .disabled(self.action_task.is_some())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let message = input.read(cx).value().trim().to_string();
                                    let (branch, head) = (branch.clone(), head.clone());
                                    this.run_worktree_edit(
                                        path.clone(),
                                        "Squash",
                                        None,
                                        move |path| {
                                            sg_git::squash(path, &branch, &head, count, &message)
                                        },
                                        cx,
                                    );
                                })),
                        ),
                )
                .into_any_element();
        }
        let message = oldest.subject.clone();
        h_flex()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("preview-squash")
                    .label(format!("Squash last {count} commits"))
                    .small()
                    .ghost()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.git_editor.squash_target = Some(candidate.clone());
                        let input = cx.new(|cx| {
                            TextareaState::new(window, cx)
                                .auto_grow(2, 5)
                                .default_value(message.clone())
                                .placeholder("Squash commit message")
                        });
                        input.update(cx, |input, cx| input.focus(window, cx));
                        this.git_editor.squash_message = Some(input);
                        cx.notify();
                    })),
            )
            .into_any_element()
    }
}
