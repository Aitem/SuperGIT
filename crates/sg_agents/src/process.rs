//! What the UI needs from a running agent, whatever its wire protocol.

use serde_json::Value;

use crate::chat::{Answers, Decision, PermissionRequest, UserInputRequest};

/// One event from an agent process, read on a background thread.
#[derive(Debug)]
pub enum ProcessEvent {
    /// One JSON message from the agent (its adapter knows the shape).
    Message(Value),
    /// A line on stderr (kept for error reporting).
    Stderr(String),
    /// The process ended (or never started).
    Exited,
}

/// A running agent conversation: Claude Code (`sg_claude`) or Codex
/// (`sg_codex`). Dropping the implementor kills the process.
pub trait AgentProcess: Send + Sync {
    /// Send a user turn. Turns sent while the agent works are queued.
    fn send_user(&self, text: &str) -> anyhow::Result<()>;
    /// Stop the current turn (like Esc in a terminal UI).
    fn interrupt(&self) -> anyhow::Result<()>;
    /// Answer a permission prompt the adapter put in the conversation.
    fn answer(&self, request: &PermissionRequest, decision: Decision) -> anyhow::Result<()>;
    /// Send the answers to a question form the adapter put in the
    /// conversation.
    fn answer_questions(&self, request: &UserInputRequest, answers: &Answers)
    -> anyhow::Result<()>;
    /// Tell the agent the user will not answer. Agents that cannot hear
    /// "no" to a question (Codex) refuse; stop the turn instead.
    fn decline_questions(&self, request: &UserInputRequest) -> anyhow::Result<()> {
        let _ = request;
        anyhow::bail!("This agent's questions can't be declined; stop the turn instead")
    }
    /// Refuse a request the host does not support, so the agent does not
    /// wait forever.
    fn reject(&self, request_id: &str, error: &str) -> anyhow::Result<()>;
    fn kill(&self);
}
