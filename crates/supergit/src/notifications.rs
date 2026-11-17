//! Desktop notifications carry an exact provider-scoped conversation destination.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{deliver, init, request_permission};
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod freedesktop;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub use freedesktop::{deliver, init, request_permission};
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "freebsd")))]
mod other;
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "freebsd")))]
pub use other::{deliver, init, request_permission};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Target {
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub provider: String,
    pub conversation: String,
}

#[cfg(test)]
mod tests {
    use super::Target;

    #[test]
    fn activation_keeps_repository_worktree_provider_and_conversation() {
        let target = Target {
            repository: "/work/repo".into(),
            worktree: "/work/任务".into(),
            provider: "claude".into(),
            conversation: "draft:1".into(),
        };
        let id = serde_json::to_string(&(target, 7_u64)).unwrap();
        let (restored, revision): (Target, u64) = serde_json::from_str(&id).unwrap();
        assert_eq!(restored.worktree, PathBuf::from("/work/任务"));
        assert_eq!(restored.provider, "claude");
        assert_eq!(restored.conversation, "draft:1");
        assert_eq!(revision, 7);
    }
    use std::path::PathBuf;
}
