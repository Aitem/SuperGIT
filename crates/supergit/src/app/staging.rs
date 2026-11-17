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
        let actions = [(Staging::Unstage, "Unstage"), (Staging::Stage, "Stage")];
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
