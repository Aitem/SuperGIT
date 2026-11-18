use sg_git::Staging;

use super::*;

impl SuperGitApp {
    pub(super) fn staging_buttons(
        &self,
        list: FileList,
        file: &FileChange,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if list != FileList::Changes
            || self.comparison_base().is_some()
            || self.details.base.is_some()
        {
            return None;
        }
        let path = self.selected.as_ref()?;
        let status = self.details.status.value()?;
        if self.details.status.error().is_some() || self.details.path != *path {
            return None;
        }
        if sg_git::submodule_of(&self.details.submodules, &file.path)
            .is_some_and(|module| module.path != file.path)
        {
            return None;
        }
        let scope = self.details.scope;
        let disabled = self.action_task.is_some()
            || self.cleanup_task.is_some()
            || self.base_update_paths.contains(path);
        let stage_label = if file.unstaged == Some(FileState::Conflicted) {
            "Mark resolved"
        } else {
            "Stage"
        };
        let actions = [(Staging::Unstage, "Unstage"), (Staging::Stage, stage_label)];
        let buttons: Vec<_> = actions
            .into_iter()
            .filter_map(|(action, label)| {
                if !action.available(file) {
                    return None;
                }
                let file = file.clone();
                let path = path.clone();
                let head = status.head_oid.clone();
                let branch = status.branch.clone();
                Some(
                    Button::new(label)
                        .label(label)
                        .xsmall()
                        .ghost()
                        .tooltip(format!("{label} {}", file.path))
                        .disabled(disabled)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            let Some(current) = this.details.status.value() else {
                                return;
                            };
                            if this.selected.as_ref() != Some(&path)
                                || this.comparison_base().is_some()
                                || this.base_update_paths.contains(&path)
                                || current.head_oid != head
                                || current.branch != branch
                                || this.change_scope() != scope
                            {
                                this.flash("The worktree changed. Review the file again", true, cx);
                                return;
                            }
                            let Some(file) = current.files.iter().find(|entry| {
                                entry.path == file.path
                                    && scope.project(entry).as_ref() == Some(&file)
                                    && action.available(entry)
                            }) else {
                                this.flash("The file changed. Review the diff again", true, cx);
                                return;
                            };
                            let file = file.clone();
                            let snapshot = current.clone();
                            let target = path.clone();
                            this.run_git_action(
                                format!("{label} {}", file.path),
                                None,
                                move || sg_git::change_staging(&target, &snapshot, &file, action),
                                cx,
                            );
                        })),
                )
            })
            .collect();
        (!buttons.is_empty()).then(|| {
            h_flex()
                .gap_0p5()
                .flex_none()
                .children(buttons)
                .into_any_element()
        })
    }
}

struct IndexHunk {
    worktree: PathBuf,
    snapshot: sg_git::WorktreeStatus,
    file: FileChange,
    action: Staging,
    source: String,
    original: bool,
    line: usize,
}

impl SuperGitApp {
    fn selected_index_hunk(&self, action: Staging, cx: &App) -> Option<IndexHunk> {
        let expected_scope = match action {
            Staging::Stage => ChangeScope::Unstaged,
            Staging::Unstage => ChangeScope::Staged,
        };
        if self.tab != MainTab::Changes
            || self.change_scope() != expected_scope
            || self.details.scope != expected_scope
            || self.difftastic_active()
        {
            return None;
        }
        let path = self.selected.as_ref()?;
        if self.details.path != *path
            || self.details.base.is_some()
            || self.details.patch.error().is_some()
            || self.details.status.error().is_some()
            || self.load_error.is_some()
        {
            return None;
        }
        let status = self.details.status.value()?;
        let state = self.changes_diff.read(cx);
        let range = state.selected_lines()?;
        let parsed = state
            .files()
            .iter()
            .find(|file| file.path() == range.path())?;
        if parsed.is_truncated() || parsed.is_binary() {
            return None;
        }
        let file = status
            .files
            .iter()
            .find(|file| file.path == range.path().as_ref() && action.hunk_available(file))?;
        let patch = self.details.patch.value()?;
        let source = agent_context::file_patch(patch, &file.path)?;
        let original = range.side() == DiffSide::Original;
        let selected = sg_git::text_hunk(source, original, range.start())?;
        if range.start() != range.end() || range.side() != range.end_side() {
            let end =
                sg_git::text_hunk(source, range.end_side() == DiffSide::Original, range.end())?;
            if selected != end {
                return None;
            }
        }
        Some(IndexHunk {
            worktree: path.clone(),
            snapshot: sg_git::WorktreeStatus {
                branch: status.branch.clone(),
                head_oid: status.head_oid.clone(),
                ..Default::default()
            },
            file: file.clone(),
            action,
            source: source.to_owned(),
            original,
            line: range.start(),
        })
    }

    pub(super) fn hunk_action_button(
        &self,
        list: FileList,
        cx: &mut Context<Self>,
    ) -> Option<Button> {
        if list != FileList::Changes || self.difftastic_active() || self.comparison_base().is_some()
        {
            return None;
        }
        let (action, name) = match self.change_scope() {
            ChangeScope::Staged => (Staging::Unstage, "Unstage hunk"),
            ChangeScope::Unstaged => (Staging::Stage, "Stage hunk"),
            ChangeScope::All => return None,
        };
        let selected = self.selected_index_hunk(action, cx);
        let disabled = self.action_task.is_some()
            || self.cleanup_task.is_some()
            || selected
                .as_ref()
                .is_none_or(|hunk| self.base_update_paths.contains(&hunk.worktree));
        Some(
            Button::new("change-index-hunk")
                .label(name)
                .xsmall()
                .outline()
                .tooltip("Select line numbers inside one hunk of a tracked text file")
                .disabled(disabled)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.apply_selected_index_hunk(action, name, cx);
                })),
        )
    }

    fn apply_selected_index_hunk(
        &mut self,
        action: Staging,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(selection) = self.selected_index_hunk(action, cx) else {
            return;
        };
        if self.base_update_paths.contains(&selection.worktree) {
            return;
        }
        self.run_git_action(
            format!("{label} in {}", selection.file.path),
            None,
            move || {
                sg_git::change_staging_hunk(
                    &selection.worktree,
                    &selection.snapshot,
                    &selection.file,
                    selection.action,
                    &selection.source,
                    (selection.original, selection.line),
                )
            },
            cx,
        );
    }
}
