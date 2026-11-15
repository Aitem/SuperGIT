//! The `claude` child process and the stream-json control protocol.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use serde_json::{Value, json};

/// How Claude asks before using tools (`--permission-mode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PermissionMode {
    /// Ask for anything that is not allowed by settings.
    #[default]
    Default,
    /// Auto-accept file edits, still ask for commands.
    AcceptEdits,
    /// Read-only planning; Claude proposes a plan.
    Plan,
    /// Claude decides on its own; a safety check (the auto-mode classifier)
    /// reviews actions first, blocks risky ones (`system/permission_denied`)
    /// and escalates the ones it cannot judge as ordinary prompts.
    Auto,
    /// Never ask.
    BypassPermissions,
}

impl PermissionMode {
    pub const ALL: [Self; 5] = [
        Self::Default,
        Self::AcceptEdits,
        Self::Plan,
        Self::Auto,
        Self::BypassPermissions,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::Auto => "auto",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    /// Parse a mode the CLI reports (`system/init`, `system/status`).
    /// `manual` is the CLI's newer alias for `default`.
    pub fn from_wire(s: &str) -> Option<Self> {
        let s = if s == "manual" { "default" } else { s };
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Ask",
            Self::AcceptEdits => "Edits",
            Self::Plan => "Plan",
            Self::Auto => "Auto",
            Self::BypassPermissions => "Bypass",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SpawnOptions {
    /// Working directory; for `resume` it must be where the session started.
    pub cwd: PathBuf,
    /// Continue an existing session by id.
    pub resume: Option<String>,
    /// `None` leaves the choice to the CLI (`permissions.defaultMode` in the
    /// user's settings, else `default`).
    pub permission_mode: Option<PermissionMode>,
    pub model: Option<String>,
    /// The CLI binary, `claude` from PATH by default.
    pub program: Option<PathBuf>,
}

#[derive(Debug)]
pub enum ProcessEvent {
    /// One JSON line from stdout.
    Message(Value),
    /// A line on stderr (kept for error reporting).
    Stderr(String),
    /// stdout closed: the process ended.
    Exited,
}

/// A running `claude` in stream-json mode. Dropping it kills the process.
pub struct ClaudeProcess {
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    next_request: AtomicU64,
}

impl ClaudeProcess {
    /// Start `claude`, send the `initialize` handshake and return the event
    /// stream (read on background threads).
    pub fn spawn(opts: &SpawnOptions) -> Result<(Self, UnboundedReceiver<ProcessEvent>)> {
        let program = opts
            .program
            .clone()
            .unwrap_or_else(|| PathBuf::from("claude"));
        let mut cmd = Command::new(&program);
        cmd.args([
            "-p",
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
        ]);
        if let Some(mode) = opts.permission_mode {
            cmd.args(["--permission-mode", mode.as_str()]);
        }
        if let Some(id) = &opts.resume {
            cmd.arg(format!("--resume={id}"));
        }
        if let Some(model) = &opts.model {
            cmd.args(["--model", model]);
        }
        cmd.current_dir(&opts.cwd)
            .env("PWD", &opts.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().with_context(|| {
            format!(
                "cannot start `{}` — is Claude Code installed?",
                program.display()
            )
        })?;

        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;
        let stdin = child.stdin.take().context("no stdin")?;
        let (tx, rx) = unbounded();
        spawn_stdout_reader(stdout, tx.clone());
        spawn_stderr_reader(stderr, tx);

        let process = Self {
            child: Mutex::new(child),
            stdin: Mutex::new(Some(stdin)),
            next_request: AtomicU64::new(1),
        };
        process.control_request(json!({ "subtype": "initialize", "hooks": null }))?;
        Ok((process, rx))
    }

    fn write(&self, value: &Value) -> Result<()> {
        let mut guard = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        let stdin = guard.as_mut().context("Claude is not running")?;
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        stdin.write_all(line.as_bytes())?;
        stdin.flush()?;
        Ok(())
    }

    fn control_request(&self, request: Value) -> Result<String> {
        let id = format!("sg_{}", self.next_request.fetch_add(1, Ordering::Relaxed));
        self.write(&json!({ "type": "control_request", "request_id": id, "request": request }))?;
        Ok(id)
    }

    /// Send a user turn. Turns sent while Claude is busy are queued by the CLI.
    pub fn send_user(&self, text: &str) -> Result<()> {
        self.write(&json!({
            "type": "user",
            "session_id": "",
            "parent_tool_use_id": null,
            "message": { "role": "user", "content": text },
        }))
    }

    /// Answer a `can_use_tool` request.
    pub fn allow(
        &self,
        request_id: &str,
        input: &Value,
        updated_permissions: Option<Vec<Value>>,
    ) -> Result<()> {
        let mut response = json!({ "behavior": "allow", "updatedInput": input });
        if let Some(perms) = updated_permissions {
            response["updatedPermissions"] = Value::Array(perms);
        }
        self.control_success(request_id, response)
    }

    pub fn deny(&self, request_id: &str, message: &str) -> Result<()> {
        self.control_success(
            request_id,
            json!({ "behavior": "deny", "message": message }),
        )
    }

    fn control_success(&self, request_id: &str, response: Value) -> Result<()> {
        self.write(&json!({
            "type": "control_response",
            "response": { "subtype": "success", "request_id": request_id, "response": response },
        }))
    }

    /// Reply to a control request we do not support (hooks, MCP, …).
    pub fn control_error(&self, request_id: &str, error: &str) -> Result<()> {
        self.write(&json!({
            "type": "control_response",
            "response": { "subtype": "error", "request_id": request_id, "error": error },
        }))
    }

    /// Stop the current turn (like pressing Esc in the terminal UI).
    pub fn interrupt(&self) -> Result<()> {
        self.control_request(json!({ "subtype": "interrupt" }))
            .map(drop)
    }

    /// Switch the mode of the running session. Returns the request id: the
    /// CLI answers with a `control_response` (an error when it refuses, e.g.
    /// `auto_mode_unavailable` or `bypass_not_launched`) and, on success, a
    /// `system/status` carrying the new `permissionMode`.
    pub fn set_permission_mode(&self, mode: PermissionMode) -> Result<String> {
        self.control_request(json!({ "subtype": "set_permission_mode", "mode": mode.as_str() }))
    }

    pub fn kill(&self) {
        // Closing stdin lets the CLI exit cleanly; kill makes sure.
        self.stdin.lock().unwrap_or_else(|e| e.into_inner()).take();
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for ClaudeProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

fn spawn_stdout_reader(stdout: std::process::ChildStdout, tx: UnboundedSender<ProcessEvent>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let event = match serde_json::from_str::<Value>(&line) {
                Ok(v) => ProcessEvent::Message(v),
                Err(_) => ProcessEvent::Stderr(line),
            };
            if tx.unbounded_send(event).is_err() {
                return;
            }
        }
        let _ = tx.unbounded_send(ProcessEvent::Exited);
    });
}

fn spawn_stderr_reader(stderr: std::process::ChildStderr, tx: UnboundedSender<ProcessEvent>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if !line.trim().is_empty() && tx.unbounded_send(ProcessEvent::Stderr(line)).is_err() {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::PermissionMode;

    #[test]
    fn permission_modes_round_trip_the_wire_names() {
        for mode in PermissionMode::ALL {
            assert_eq!(PermissionMode::from_wire(mode.as_str()), Some(mode));
        }
        assert_eq!(
            PermissionMode::from_wire("auto"),
            Some(PermissionMode::Auto)
        );
        assert_eq!(
            PermissionMode::from_wire("manual"),
            Some(PermissionMode::Default)
        );
        assert_eq!(PermissionMode::from_wire("dontAsk"), None);
    }
}
