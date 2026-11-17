//! Codex as a native chat.
//!
//! SuperGIT drives the installed Codex CLI through its structured
//! `codex app-server` protocol over stdio (JSON-RPC, one message per line) —
//! the protocol Codex's own IDE integrations use. Each conversation is a
//! Codex thread: `thread/start` or `thread/resume`, then `turn/start` per user
//! message; items stream back as notifications and approvals arrive as server
//! requests. Threads are ordinary Codex sessions under `~/.codex/sessions`,
//! resumable here or with `codex resume`; the user's Codex login is used.
//!
//! This crate is UI-agnostic: [`install`] finds a compatible CLI,
//! [`process`] owns the app server and the wire protocol, [`conversation`]
//! folds events into the shared chat model (`sg_agents::chat`), [`tools`]
//! summarizes Codex items, [`sessions`] discovers existing sessions.
#![forbid(unsafe_code)]

use std::path::PathBuf;

pub mod conversation;
pub mod install;
pub mod process;
pub mod sessions;
pub mod tools;

pub use conversation::{CodexFold, Reply};
pub use process::{
    CodexProcess, History, Model, Policy, SpawnOptions, TurnOptions, list_models, read_history,
};
pub use sessions::CodexSessionIndex;

/// `~/.codex`, honouring `CODEX_HOME`.
pub fn codex_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_HOME") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex"))
}
