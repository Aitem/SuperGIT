//! Claude Code as a native chat.
//!
//! SuperGIT drives the `claude` CLI the same way the official Agent SDKs and
//! the VS Code extension do: `claude -p --input-format stream-json
//! --output-format stream-json --permission-prompt-tool stdio`. User turns go
//! to stdin as JSON lines, events come back on stdout, and tool permission
//! prompts arrive as `control_request { subtype: "can_use_tool" }` that the
//! host answers with `control_response`. Sessions are ordinary Claude Code
//! sessions: the same transcripts, resumable here or in a terminal.
//!
//! This crate is UI-agnostic: [`process`] owns the child process and the wire
//! protocol, [`conversation`] folds events into displayable [`Item`]s.

pub mod conversation;
pub mod process;
pub mod tools;

pub use conversation::{
    Conversation, Decision, Item, PermissionRequest, Status, ToolCall, ToolResult,
};
pub use process::{ClaudeProcess, PermissionMode, ProcessEvent, SpawnOptions};
