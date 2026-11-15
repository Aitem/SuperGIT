//! Exact native conversation events, separate from transcript heuristics.

use serde::{Deserialize, Serialize};

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum Kind {
    #[default]
    Idle,
    Running,
    Approval,
    Completed,
    Failed,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "● running",
            Self::Approval => "! needs approval",
            Self::Completed => "✓ unread",
            Self::Failed => "× failed",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Attention {
    pub kind: Kind,
    pub revision: u64,
    pub unread: bool,
    token: Option<String>,
}

impl Attention {
    pub fn set(&mut self, kind: Kind, token: Option<String>) {
        if self.kind != kind || self.token != token {
            self.revision += 1;
            self.kind = kind;
            self.token = token;
            self.unread = matches!(kind, Kind::Approval | Kind::Completed | Kind::Failed);
        }
    }

    pub fn restored(mut self) -> Self {
        if matches!(self.kind, Kind::Running | Kind::Approval) {
            self.set(Kind::Idle, None);
        }
        self
    }

    pub fn should_notify(&self, focused: bool, approvals: bool, completions: bool) -> bool {
        self.unread
            && !focused
            && match self.kind {
                Kind::Approval => approvals,
                Kind::Completed | Kind::Failed => completions,
                _ => false,
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_events_deduplicate_and_keep_completion_separate_from_approval() {
        let mut state = Attention::default();
        state.set(Kind::Running, None);
        state.set(Kind::Approval, Some("request-1".into()));
        let revision = state.revision;
        state.set(Kind::Approval, Some("request-1".into()));
        assert_eq!(state.revision, revision);
        assert!(state.should_notify(false, true, false));
        assert!(!state.should_notify(true, true, true));
        state.unread = false;
        assert!(!state.should_notify(false, true, true));
        state.set(Kind::Approval, Some("request-2".into()));
        assert_eq!(state.revision, revision + 1);
        state.set(Kind::Completed, None);
        assert!(!state.should_notify(false, true, false));
        assert!(state.should_notify(false, false, true));
    }

    #[test]
    fn restoration_never_claims_a_process_or_permission_request_is_live() {
        let mut state = Attention::default();
        state.set(Kind::Approval, Some("old".into()));
        assert_eq!(state.restored().kind, Kind::Idle);
        let mut state = Attention::default();
        state.set(Kind::Failed, None);
        assert!(state.restored().unread);
    }
}
