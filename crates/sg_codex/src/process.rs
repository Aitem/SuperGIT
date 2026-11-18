//! `codex app-server` over stdio: one process per conversation.
//!
//! The wire format is JSON-RPC without the `jsonrpc` field, one message per
//! line. A driver thread owns the child's stdin and every piece of protocol
//! state (request ids, the thread and turn ids), so the UI-facing handle
//! never blocks: turns sent before the handshake finishes, or while a turn
//! runs, are queued and started in order.
//!
//! Events reach the host as [`ProcessEvent::Message`]: notifications and
//! server requests verbatim, and answers to our own requests as
//! `{"response": "<method>", "result" | "error": …}` (see
//! [`crate::conversation`]).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use serde_json::{Value, json};
use sg_agents::chat::{Decision, PermissionRequest};
use sg_agents::process::{AgentProcess, ProcessEvent};

use crate::install;

/// How much Codex may do without asking: its approval policy plus sandbox,
/// mirroring the CLI's `/approvals` presets. These are Codex's own concepts —
/// they do not map onto Claude's permission modes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Policy {
    /// Read-only sandbox; asks before edits, commands that write, network.
    ReadOnly,
    /// Edits and commands inside the workspace sandbox; asks to go beyond it
    /// (writes elsewhere, network, `.git`).
    #[default]
    Auto,
    /// No sandbox, never asks.
    FullAccess,
}

impl Policy {
    pub const ALL: [Self; 3] = [Self::ReadOnly, Self::Auto, Self::FullAccess];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Auto => "auto",
            Self::FullAccess => "full-access",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "Read only",
            Self::Auto => "Auto",
            Self::FullAccess => "Full access",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::ReadOnly => "Read-only sandbox: asks before edits and commands that write",
            Self::Auto => "Edits and runs commands in the worktree sandbox; asks to go beyond it",
            Self::FullAccess => "No sandbox, never asks (careful)",
        }
    }

    /// `approvalPolicy` (`AskForApproval`).
    pub fn approval_policy(self) -> &'static str {
        match self {
            Self::ReadOnly | Self::Auto => "on-request",
            Self::FullAccess => "never",
        }
    }

    /// `sandbox` for `thread/start` / `thread/resume` (`SandboxMode`).
    pub fn sandbox_mode(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Auto => "workspace-write",
            Self::FullAccess => "danger-full-access",
        }
    }

    /// `sandboxPolicy` for `turn/start` (`SandboxPolicy`).
    pub fn sandbox_policy(self) -> Value {
        match self {
            Self::ReadOnly => json!({ "type": "readOnly", "networkAccess": false }),
            Self::Auto => json!({
                "type": "workspaceWrite",
                "writableRoots": [],
                "networkAccess": false,
                "excludeTmpdirEnvVar": false,
                "excludeSlashTmp": false,
            }),
            Self::FullAccess => json!({ "type": "dangerFullAccess" }),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SpawnOptions {
    /// Working directory of a new thread. A resumed thread keeps the one it
    /// was started in (Codex stores it).
    pub cwd: PathBuf,
    /// Continue an existing thread by id.
    pub resume: Option<String>,
    pub policy: Policy,
    pub model: Option<String>,
    /// The CLI binary; found with [`install::locate`] by default.
    pub program: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnOptions {
    pub model: Option<String>,
    pub effort: Option<String>,
}

enum Input {
    Line(Value),
    Closed,
    Send(String, TurnOptions),
    Interrupt,
    Respond { id: Value, result: Value },
    Fail { id: Value, message: String },
    Kill,
}

/// A running `codex app-server` hosting one thread. Dropping it kills the
/// process.
pub struct CodexProcess {
    inbox: Sender<Input>,
    child: Arc<Mutex<Option<Child>>>,
    killed: Arc<AtomicBool>,
    policy: Arc<Mutex<Policy>>,
}

impl CodexProcess {
    /// Start the driver: it finds and checks the CLI, starts the app server,
    /// performs the handshake and starts or resumes the thread, all off the
    /// caller's thread. Failures arrive as events (an error response, then
    /// [`ProcessEvent::Exited`]).
    pub fn spawn(opts: &SpawnOptions) -> (Self, UnboundedReceiver<ProcessEvent>) {
        let (events, rx) = unbounded();
        let (inbox, inputs) = mpsc::channel();
        let child = Arc::new(Mutex::new(None));
        let killed = Arc::new(AtomicBool::new(false));
        let policy = Arc::new(Mutex::new(opts.policy));
        let driver = Driver {
            opts: opts.clone(),
            events,
            inbox: inbox.clone(),
            child: child.clone(),
            killed: killed.clone(),
            policy: policy.clone(),
            stdin: None,
            stderr_done: None,
            next_id: 1,
            pending: HashMap::new(),
            thread_id: None,
            turn_id: None,
            turn_active: false,
            interrupt_pending: false,
            ready: false,
            queued: VecDeque::new(),
        };
        std::thread::spawn(move || driver.run(inputs));
        (
            Self {
                inbox,
                child,
                killed,
                policy,
            },
            rx,
        )
    }

    pub fn send_turn(&self, text: &str, options: TurnOptions) -> Result<()> {
        self.input(Input::Send(text.to_string(), options))
    }

    /// Applies from the next turn on (Codex takes policy overrides per turn).
    pub fn set_policy(&self, policy: Policy) {
        *self.policy.lock().unwrap_or_else(|e| e.into_inner()) = policy;
    }

    /// Answer a server request directly (requests the fold resolves without
    /// asking the user).
    pub fn respond(&self, request_id: &str, result: Value) -> Result<()> {
        self.input(Input::Respond {
            id: request_id_value(request_id),
            result,
        })
    }

    /// Send a response to an interactive question using its original request ID.
    pub fn respond_to_questions(
        &self,
        request_id: &str,
        answers: &BTreeMap<String, String>,
    ) -> Result<()> {
        self.respond(request_id, crate::questions::response(answers))
    }

    fn input(&self, input: Input) -> Result<()> {
        self.inbox
            .send(input)
            .map_err(|_| anyhow!("Codex is not running"))
    }
}

impl AgentProcess for CodexProcess {
    fn send_user(&self, text: &str) -> Result<()> {
        self.send_turn(text, TurnOptions::default())
    }

    fn interrupt(&self) -> Result<()> {
        self.input(Input::Interrupt)
    }

    fn answer(&self, request: &PermissionRequest, decision: Decision) -> Result<()> {
        let options = request
            .suggestions
            .first()
            .context("this prompt has no answers")?;
        let key = match decision {
            Decision::Allowed => "allow",
            Decision::AllowedForSession => "session",
            Decision::Denied | Decision::Expired => "deny",
        };
        let result = options
            .get(key)
            .filter(|v| !v.is_null())
            .cloned()
            .with_context(|| format!("Codex did not offer “{key}” here"))?;
        self.input(Input::Respond {
            id: request_id_value(&request.request_id),
            result,
        })
    }

    fn reject(&self, request_id: &str, error: &str) -> Result<()> {
        self.input(Input::Fail {
            id: request_id_value(request_id),
            message: error.to_string(),
        })
    }

    fn kill(&self) {
        self.killed.store(true, Ordering::SeqCst);
        if let Some(mut child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self.inbox.send(Input::Kill);
    }
}

impl Drop for CodexProcess {
    fn drop(&mut self) {
        AgentProcess::kill(self);
    }
}

/// Server request ids are JSON values (numbers or strings); the chat model
/// keeps them as their JSON text.
pub fn request_id_text(id: &Value) -> String {
    id.to_string()
}

fn request_id_value(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

struct Driver {
    opts: SpawnOptions,
    events: UnboundedSender<ProcessEvent>,
    inbox: Sender<Input>,
    child: Arc<Mutex<Option<Child>>>,
    killed: Arc<AtomicBool>,
    policy: Arc<Mutex<Policy>>,
    stdin: Option<ChildStdin>,
    /// Disconnects when the stderr reader is finished.
    stderr_done: Option<Receiver<()>>,
    next_id: u64,
    pending: HashMap<u64, &'static str>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    /// `turn/start` sent and the turn has not completed.
    turn_active: bool,
    /// Stop pressed before the turn id was known.
    interrupt_pending: bool,
    /// The thread is started or resumed: turns can go out.
    ready: bool,
    queued: VecDeque<(String, TurnOptions)>,
}

impl Driver {
    fn run(mut self, inputs: Receiver<Input>) {
        if let Err(e) = self.start() {
            self.emit_error("initialize", &format!("{e:#}"));
            self.emit(ProcessEvent::Exited);
            return;
        }
        while let Ok(input) = inputs.recv() {
            match input {
                Input::Line(v) => self.on_line(v),
                Input::Closed => break,
                Input::Send(text, options) => {
                    self.queued.push_back((text, options));
                    self.start_next_turn();
                }
                Input::Interrupt => self.interrupt(),
                Input::Respond { id, result } => self.write(&json!({ "id": id, "result": result })),
                Input::Fail { id, message } => self.write(&json!({
                    "id": id,
                    "error": { "code": -32601, "message": message },
                })),
                Input::Kill => break,
            }
            if self.killed.load(Ordering::SeqCst) {
                break;
            }
        }
        self.stdin = None;
        if let Some(mut child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Let the last stderr lines (often the reason for a crash) arrive
        // first; a grandchild holding the pipe open must not stall the exit.
        if let Some(done) = self.stderr_done.take() {
            let _ = done.recv_timeout(Duration::from_millis(500));
        }
        self.emit(ProcessEvent::Exited);
    }

    fn start(&mut self) -> Result<()> {
        let program = match &self.opts.program {
            Some(program) => install::locate_in(std::slice::from_ref(program))?.program,
            None => install::locate()?.program,
        };
        let mut child = app_server(&program, &self.opts.cwd)?;
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;
        self.stdin = Some(child.stdin.take().context("no stdin")?);
        *self.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
        if self.killed.load(Ordering::SeqCst) {
            bail!("stopped");
        }
        let inbox = self.inbox.clone();
        let events = self.events.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let sent = match serde_json::from_str::<Value>(&line) {
                    Ok(v) => inbox.send(Input::Line(v)).is_ok(),
                    Err(_) => events.unbounded_send(ProcessEvent::Stderr(line)).is_ok(),
                };
                if !sent {
                    return;
                }
            }
            let _ = inbox.send(Input::Closed);
        });
        let events = self.events.clone();
        let (done, stderr_done) = mpsc::channel::<()>();
        self.stderr_done = Some(stderr_done);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if !line.trim().is_empty()
                    && events.unbounded_send(ProcessEvent::Stderr(line)).is_err()
                {
                    break;
                }
            }
            drop(done);
        });
        self.request("initialize", initialize_params(true));
        Ok(())
    }

    fn emit(&self, event: ProcessEvent) {
        let _ = self.events.unbounded_send(event);
    }

    fn emit_error(&self, method: &str, message: &str) {
        self.emit(ProcessEvent::Message(json!({
            "response": method,
            "error": { "message": message },
        })));
    }

    fn write(&mut self, v: &Value) {
        let Some(stdin) = self.stdin.as_mut() else {
            return;
        };
        let mut line = v.to_string();
        line.push('\n');
        if stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .is_err()
        {
            // The reader sees stdout close and ends the session.
            self.stdin = None;
        }
    }

    fn request(&mut self, method: &'static str, params: Value) {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, method);
        self.write(&json!({ "id": id, "method": method, "params": params }));
    }

    fn on_line(&mut self, v: Value) {
        let method = v.get("method").and_then(Value::as_str).map(String::from);
        match (method, v.get("id")) {
            // A response to one of our requests.
            (None, Some(id)) => {
                let Some(method) = id.as_u64().and_then(|id| self.pending.remove(&id)) else {
                    return;
                };
                let error = v.get("error").map(|e| {
                    e.get("message")
                        .and_then(Value::as_str)
                        .map(String::from)
                        .unwrap_or_else(|| e.to_string())
                });
                self.on_response(method, v.get("result"), error.as_deref());
                let mut event = json!({ "response": method });
                match (v.get("result"), error) {
                    (_, Some(error)) => event["error"] = json!({ "message": error }),
                    (Some(result), None) => event["result"] = result.clone(),
                    (None, None) => {}
                }
                self.emit(ProcessEvent::Message(event));
            }
            // Notifications and server requests go to the host as they are.
            (Some(method), _) => {
                match method.as_str() {
                    "turn/started" => {
                        self.turn_active = true;
                        self.turn_id = v
                            .pointer("/params/turn/id")
                            .and_then(Value::as_str)
                            .map(String::from);
                        if std::mem::take(&mut self.interrupt_pending) {
                            self.interrupt();
                        }
                    }
                    "turn/completed" => {
                        self.turn_active = false;
                        self.turn_id = None;
                    }
                    _ => {}
                }
                self.emit(ProcessEvent::Message(v));
                if method == "turn/completed" {
                    self.start_next_turn();
                }
            }
            (None, None) => {}
        }
    }

    fn on_response(&mut self, method: &str, result: Option<&Value>, error: Option<&str>) {
        let fatal = error.is_some();
        match method {
            "initialize" if !fatal => {
                self.write(&json!({ "method": "initialized" }));
                self.request("account/read", json!({ "refreshToken": false }));
            }
            "account/read" if !fatal => {
                let signed_out = result.is_some_and(|r| {
                    r.get("account").is_none_or(Value::is_null)
                        && r.get("requiresOpenaiAuth").and_then(Value::as_bool) == Some(true)
                });
                if signed_out {
                    self.emit_error(
                        "account/read",
                        "Codex is not signed in. Run `codex login` in a terminal, then send again.",
                    );
                    self.stop();
                } else {
                    self.open_thread();
                }
            }
            "thread/start" | "thread/resume" if !fatal => {
                self.thread_id = result
                    .and_then(|r| r.pointer("/thread/id"))
                    .and_then(Value::as_str)
                    .map(String::from);
                self.ready = self.thread_id.is_some();
                self.start_next_turn();
            }
            "turn/start" => {
                if let Some(id) = result
                    .and_then(|r| r.pointer("/turn/id"))
                    .and_then(Value::as_str)
                {
                    self.turn_id = Some(id.to_string());
                    if std::mem::take(&mut self.interrupt_pending) {
                        self.interrupt();
                    }
                }
                if fatal {
                    self.turn_active = false;
                    self.start_next_turn();
                }
            }
            // Without a session nothing else can work.
            "initialize" | "account/read" | "thread/start" | "thread/resume" => self.stop(),
            _ => {}
        }
    }

    fn open_thread(&mut self) {
        let policy = *self.policy.lock().unwrap_or_else(|e| e.into_inner());
        let mut params = json!({
            "approvalPolicy": policy.approval_policy(),
            "sandbox": policy.sandbox_mode(),
        });
        if let Some(model) = &self.opts.model {
            params["model"] = json!(model);
        }
        match self.opts.resume.clone() {
            Some(id) => {
                params["threadId"] = json!(id);
                params["excludeTurns"] = json!(true);
                self.request("thread/resume", params);
            }
            None => {
                params["cwd"] = json!(self.opts.cwd);
                self.request("thread/start", params);
            }
        }
    }

    fn start_next_turn(&mut self) {
        if !self.ready || self.turn_active {
            return;
        }
        let (Some(thread_id), Some((text, options))) =
            (self.thread_id.clone(), self.queued.pop_front())
        else {
            return;
        };
        let policy = *self.policy.lock().unwrap_or_else(|e| e.into_inner());
        self.turn_active = true;
        self.turn_id = None;
        let mut params = json!({
            "threadId": thread_id,
            "input": [{ "type": "text", "text": text, "text_elements": [] }],
            "approvalPolicy": policy.approval_policy(),
            "sandboxPolicy": policy.sandbox_policy(),
        });
        if let Some(model) = options.model {
            params["model"] = json!(model);
        }
        if let Some(effort) = options.effort {
            params["effort"] = json!(effort);
        }
        self.request("turn/start", params);
    }

    fn interrupt(&mut self) {
        if self.turn_active {
            match (self.thread_id.clone(), self.turn_id.clone()) {
                (Some(thread_id), Some(turn_id)) => self.request(
                    "turn/interrupt",
                    json!({ "threadId": thread_id, "turnId": turn_id }),
                ),
                _ => self.interrupt_pending = true,
            }
        } else if !self.queued.is_empty() {
            // Stopped while still starting up: drop the waiting turns.
            self.queued.clear();
            self.emit(ProcessEvent::Message(json!({
                "method": "turn/completed",
                "params": { "turn": { "status": "interrupted", "error": null } },
            })));
        }
    }

    fn stop(&mut self) {
        self.killed.store(true, Ordering::SeqCst);
        self.queued.clear();
    }
}

fn initialize_params(experimental_api: bool) -> Value {
    json!({
        "clientInfo": {
            "name": "supergit",
            "title": "SuperGIT",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "capabilities": { "experimentalApi": experimental_api, "requestAttestation": false },
    })
}

fn app_server(program: &Path, cwd: &Path) -> Result<Child> {
    let mut cmd = Command::new(program);
    cmd.arg("app-server")
        .current_dir(cwd)
        .env("PWD", cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.spawn()
        .with_context(|| format!("cannot start `{} app-server`", program.display()))
}

/// A finished thread, read for display without starting it.
#[derive(Clone, Debug, Default)]
pub struct History {
    /// `Thread` metadata (`name`, `preview`, `cwd`, `model`, `path`, …).
    pub thread: Value,
    /// Turns, oldest first, with their items.
    pub turns: Vec<Value>,
}

/// How long one request of a history read may take.
const HISTORY_TIMEOUT: Duration = Duration::from_secs(30);
/// Turns per page, and a bound on how much history is loaded.
const HISTORY_PAGE: u64 = 50;
const HISTORY_PAGES: usize = 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub label: String,
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
    pub is_default: bool,
}

/// Read the configured provider's catalog without starting a thread.
pub fn list_models(cwd: &Path) -> Result<Vec<Model>> {
    let program = install::locate()?.program;
    let mut client = Client::start(&program, cwd)?;
    client.call("initialize", initialize_params())?;
    client.notify(&json!({ "method": "initialized" }))?;
    let mut models = Vec::new();
    let mut cursor = Value::Null;
    loop {
        let result = client.call(
            "model/list",
            json!({ "cursor": cursor, "includeHidden": false }),
        )?;
        let page = result
            .get("data")
            .and_then(Value::as_array)
            .context("Missing Codex model catalog")?;
        models.extend(page.iter().filter_map(|model| {
            let id = model.get("model")?.as_str()?.to_string();
            Some(Model {
                label: model
                    .get("displayName")
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                    .to_string(),
                id,
                efforts: model
                    .get("supportedReasoningEfforts")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|item| item.get("reasoningEffort")?.as_str().map(str::to_string))
                    .collect(),
                default_effort: model
                    .get("defaultReasoningEffort")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                is_default: model
                    .get("isDefault")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        }));
        let next = result.get("nextCursor").filter(|value| !value.is_null());
        match next {
            Some(next) if *next != cursor => cursor = next.clone(),
            _ => return Ok(models),
        }
    }
}

/// Read a thread's history through a short-lived app server: the thread is
/// not resumed and no agent runs. Blocking — call off the UI thread.
pub fn read_history(program: Option<&Path>, cwd: &Path, thread_id: &str) -> Result<History> {
    let program = match program {
        Some(program) => install::locate_in(&[program.to_path_buf()])?.program,
        None => install::locate()?.program,
    };
    let mut client = Client::start(&program, cwd)?;
    client.call("initialize", initialize_params(false))?;
    client.notify(&json!({ "method": "initialized" }))?;
    let thread = client
        .call(
            "thread/read",
            json!({ "threadId": thread_id, "includeTurns": false }),
        )?
        .get("thread")
        .cloned()
        .unwrap_or(Value::Null);
    let mut turns = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..HISTORY_PAGES {
        let mut params = json!({
            "threadId": thread_id,
            "sortDirection": "desc",
            "itemsView": "full",
            "limit": HISTORY_PAGE,
        });
        if let Some(cursor) = &cursor {
            params["cursor"] = json!(cursor);
        }
        let page = match client.call("thread/turns/list", params) {
            Ok(page) => page,
            // Threads without paginated history: read them whole.
            Err(_) if cursor.is_none() => {
                let full = client.call(
                    "thread/read",
                    json!({ "threadId": thread_id, "includeTurns": true }),
                )?;
                let turns = full
                    .pointer("/thread/turns")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                return Ok(History { thread, turns });
            }
            Err(e) => return Err(e),
        };
        turns.extend(
            page.get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
        cursor = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(String::from);
        if cursor.is_none() {
            break;
        }
    }
    turns.reverse();
    Ok(History { thread, turns })
}

/// A blocking JSON-RPC client for one-off reads.
struct Client {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<Value>,
    next_id: u64,
}

impl Client {
    fn start(program: &Path, cwd: &Path) -> Result<Self> {
        let mut child = app_server(program, cwd)?;
        let stdout = child.stdout.take().context("no stdout")?;
        let stdin = child.stdin.take().context("no stdin")?;
        // stderr is not read: drop it so the child never blocks on it.
        drop(child.stderr.take());
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    return;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines,
            next_id: 1,
        })
    }

    fn notify(&mut self, v: &Value) -> Result<()> {
        let mut line = v.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.flush()?;
        Ok(())
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.notify(&json!({ "id": id, "method": method, "params": params }))?;
        let deadline = Instant::now() + HISTORY_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let v = match self.lines.recv_timeout(left) {
                Ok(v) => v,
                Err(RecvTimeoutError::Timeout) => bail!("Codex did not answer {method}"),
                Err(RecvTimeoutError::Disconnected) => bail!("Codex exited during {method}"),
            };
            if v.get("method").is_some() || v.get("id").and_then(Value::as_u64) != Some(id) {
                continue; // notifications, other traffic
            }
            if let Some(e) = v.get("error") {
                let message = e.get("message").and_then(Value::as_str).unwrap_or("error");
                bail!("{method}: {message}");
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policies_round_trip_and_differ() {
        for p in Policy::ALL {
            assert_eq!(Policy::parse(p.as_str()), Some(p));
        }
        assert_eq!(Policy::default(), Policy::Auto);
        assert_eq!(Policy::FullAccess.approval_policy(), "never");
        assert_eq!(Policy::ReadOnly.sandbox_policy()["type"], "readOnly");
        assert_eq!(Policy::Auto.sandbox_mode(), "workspace-write");
    }

    #[test]
    fn request_ids_keep_their_json_type() {
        assert_eq!(request_id_value(&request_id_text(&json!(0))), json!(0));
        assert_eq!(
            request_id_value(&request_id_text(&json!("a1"))),
            json!("a1")
        );
    }
}
