//! Conservative completion classification; uncertainty keeps work visible.

#[derive(Clone, Default)]
pub struct Completion<'a> {
    pub main: bool,
    pub kept: bool,
    pub locked: bool,
    pub available: bool,
    pub clean: bool,
    pub busy: bool,
    pub unread: bool,
    pub pending_draft: bool,
    pub prs_fresh: bool,
    pub merged: bool,
    pub local_tip: Option<&'a str>,
    pub merged_tip: Option<&'a str>,
}

impl Completion<'_> {
    pub fn completed(&self) -> bool {
        !self.main
            && !self.kept
            && !self.locked
            && self.available
            && self.clean
            && !self.busy
            && !self.unread
            && !self.pending_draft
            && self.prs_fresh
            && self.merged
            && self
                .local_tip
                .is_some_and(|tip| !tip.is_empty() && Some(tip) == self.merged_tip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged() -> Completion<'static> {
        Completion {
            available: true,
            clean: true,
            prs_fresh: true,
            merged: true,
            local_tip: Some("abc"),
            merged_tip: Some("abc"),
            ..Default::default()
        }
    }

    #[test]
    fn dirty_running_kept_locked_missing_and_offline_tasks_stay_active() {
        assert!(merged().completed());
        for change in [
            |state: &mut Completion| state.clean = false,
            |state: &mut Completion| state.busy = true,
            |state: &mut Completion| state.kept = true,
            |state: &mut Completion| state.locked = true,
            |state: &mut Completion| state.available = false,
            |state: &mut Completion| state.prs_fresh = false,
            |state: &mut Completion| state.unread = true,
            |state: &mut Completion| state.pending_draft = true,
            |state: &mut Completion| state.main = true,
        ] {
            let mut state = merged();
            change(&mut state);
            assert!(!state.completed());
        }
    }

    #[test]
    fn a_reused_name_or_unknown_tip_is_not_completion_evidence() {
        let mut state = merged();
        state.local_tip = Some("new-commit");
        assert!(!state.completed());
        state.local_tip = None;
        assert!(!state.completed());
        state.local_tip = Some("");
        state.merged_tip = Some("");
        assert!(!state.completed());
    }
}
