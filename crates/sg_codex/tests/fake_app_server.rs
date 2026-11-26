//! `CodexProcess` + `CodexFold` against a fake `codex` (a shell script that
//! speaks just enough of the app-server protocol). No real Codex runs.
#![expect(clippy::unwrap_used, reason = "tests")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::channel::mpsc::TryRecvError;
use serde_json::{Value, json};
use sg_agents::chat::{Answers, Conversation, Decision, InputResolution, Item, Status};
use sg_agents::process::{AgentProcess, ProcessEvent};
use sg_codex::{
    CodexFold, CodexProcess, CollaborationMode, Policy, Reply, SpawnOptions, TurnOptions,
};

const FAKE: &str = r#"#!/bin/sh
# A fake `codex`: MODE and VERSION are substituted per test.
MODE=__MODE__
LOG="$(dirname "$0")/received-$(basename "$PWD").log"
if [ "$1" = "--version" ]; then echo "codex-cli __VERSION__"; exit 0; fi
[ "$1" = "app-server" ] || exit 2
THREAD="th-$(basename "$PWD")"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$LOG"
  id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9]*\),"method".*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      echo "{\"id\":$id,\"result\":{\"userAgent\":\"fake\",\"codexHome\":\"/tmp\",\"platformFamily\":\"unix\",\"platformOs\":\"macos\"}}" ;;
    *'"method":"account/read"'*)
      if [ "$MODE" = signed-out ]; then
        echo "{\"id\":$id,\"result\":{\"account\":null,\"requiresOpenaiAuth\":true}}"
      else
        echo "{\"id\":$id,\"result\":{\"account\":{\"type\":\"apiKey\"},\"requiresOpenaiAuth\":true}}"
      fi ;;
    *'"method":"thread/start"'*|*'"method":"thread/resume"'*)
      echo "{\"id\":$id,\"result\":{\"thread\":{\"id\":\"$THREAD\",\"cwd\":\"$PWD\"},\"model\":\"fake-model\",\"reasoningEffort\":\"high\"}}" ;;
    *'"method":"thread/fork"'*)
      echo "{\"id\":$id,\"result\":{\"thread\":{\"id\":\"fork-$THREAD\",\"cwd\":\"$PWD\"},\"model\":\"fake-model\",\"reasoningEffort\":\"high\"}}" ;;
    *'"method":"turn/start"'*)
      echo "{\"id\":$id,\"result\":{\"turn\":{\"id\":\"tu-1\",\"status\":\"inProgress\",\"items\":[]}}}"
      echo '{"method":"turn/started","params":{"turn":{"id":"tu-1","status":"inProgress"}}}'
      if [ "$MODE" = crash ]; then echo "fatal: model stream died" >&2; exit 1; fi
      if [ "$MODE" = collaboration ]; then
        echo '{"method":"turn/completed","params":{"turn":{"id":"tu-1","status":"completed","error":null}}}'
        continue
      fi
      if [ "$MODE" = questions ]; then
        echo '{"method":"item/tool/requestUserInput","id":"choice-1","params":{"itemId":"q-1","isBlocking":true,"questions":[{"id":"base","header":"Target","question":"Which branch should receive the PR?","isOther":true,"options":[{"label":"main","description":"Default branch"},{"label":"develop","description":"Integration branch"}]},{"id":"note","header":"Notes","question":"Any rollout notes?","options":null}]}}'
        continue
      fi
      echo '{"method":"item/started","params":{"item":{"type":"commandExecution","id":"c1","command":"/bin/zsh -lc ls","status":"inProgress"}}}'
      echo '{"method":"item/commandExecution/requestApproval","id":0,"params":{"itemId":"c1","command":"/bin/zsh -lc ls","cwd":"/tmp","availableDecisions":["accept","acceptForSession","decline","cancel"]}}' ;;
    *'"method":"turn/interrupt"'*)
      echo "{\"id\":$id,\"result\":{}}"
      echo '{"method":"serverRequest/resolved","params":{"requestId":0}}'
      echo '{"method":"turn/completed","params":{"turn":{"id":"tu-1","status":"interrupted","error":null}}}' ;;

    '{"id":"choice-1","result"'*)
      echo '{"method":"serverRequest/resolved","params":{"requestId":"choice-1"}}'
      echo '{"method":"item/completed","params":{"item":{"type":"agentMessage","id":"m1","text":"Received your answers"}}}'
      echo '{"method":"turn/completed","params":{"turn":{"id":"tu-1","status":"completed","error":null,"durationMs":5}}}' ;;
    '{"id":0,"result"'*)
      case "$line" in *'"decline"'*) status=declined ;; *) status=completed ;; esac
      echo "{\"method\":\"item/completed\",\"params\":{\"item\":{\"type\":\"commandExecution\",\"id\":\"c1\",\"command\":\"ls\",\"status\":\"$status\",\"aggregatedOutput\":\"a.txt\",\"exitCode\":0}}}"
      echo '{"method":"item/completed","params":{"item":{"type":"agentMessage","id":"m1","text":"done"}}}'
      echo '{"method":"turn/completed","params":{"turn":{"id":"tu-1","status":"completed","error":null,"durationMs":5}}}' ;;
  esac
done
"#;

struct Fake {
    dir: PathBuf,
    program: PathBuf,
}

impl Fake {
    fn new(name: &str, mode: &str, version: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("sg_codex_fake_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("codex");
        let script = FAKE
            .replace("__MODE__", mode)
            .replace("__VERSION__", version);
        std::fs::write(&program, script).unwrap();
        let mut perms = std::fs::metadata(&program).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&program, perms).unwrap();
        Self { dir, program }
    }

    /// A worktree the fake runs in; its name keys the thread id and log.
    fn worktree(&self, name: &str) -> PathBuf {
        let wt = self.dir.join(name);
        std::fs::create_dir_all(&wt).unwrap();
        wt
    }

    fn received(&self, worktree: &str) -> Vec<Value> {
        std::fs::read_to_string(self.dir.join(format!("received-{worktree}.log")))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn sent(&self, worktree: &str, method: &str) -> Vec<Value> {
        self.received(worktree)
            .into_iter()
            .filter(|v| v["method"] == method)
            .collect()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

struct Session {
    process: CodexProcess,
    events: futures::channel::mpsc::UnboundedReceiver<ProcessEvent>,
    fold: CodexFold,
    convo: Conversation,
    stderr: Vec<String>,
    exited: bool,
}

impl Session {
    fn start(program: &Path, cwd: PathBuf, resume: Option<&str>) -> Self {
        Self::spawn(program, cwd, resume, false)
    }

    fn spawn(program: &Path, cwd: PathBuf, resume: Option<&str>, fork: bool) -> Self {
        let (process, events) = CodexProcess::spawn(&SpawnOptions {
            cwd,
            resume: resume.map(String::from),
            fork,
            policy: Policy::Auto,
            model: None,
            developer_instructions: Some("You work in a worktree.".into()),
            program: Some(program.to_path_buf()),
        });
        let mut convo = Conversation::new();
        convo.session_id = resume.map(String::from);
        Self {
            process,
            events,
            fold: CodexFold::new(),
            convo,
            stderr: Vec::new(),
            exited: false,
        }
    }

    fn send(&mut self, text: &str) {
        self.convo.push_user(text);
        self.process.send_user(text).unwrap();
    }

    /// Apply events until `done` holds (or fail after a few seconds).
    fn until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            match self.events.try_recv() {
                Ok(ProcessEvent::Message(v)) => match self.fold.apply(&mut self.convo, &v) {
                    Some(Reply::Unsupported { request_id, method }) => {
                        self.process.reject(&request_id, &method).unwrap()
                    }
                    Some(Reply::Respond { .. }) | None => {}
                },
                Ok(ProcessEvent::Stderr(line)) => self.stderr.push(line),
                Ok(ProcessEvent::Exited) | Err(TryRecvError::Closed) => {
                    if !self.exited {
                        self.convo.on_exit(self.stderr.last().cloned(), "Codex");
                    }
                    self.exited = true;
                }
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    fn answer(&mut self, decision: Decision) {
        let request = self.convo.pending_permission().unwrap().clone();
        self.process.answer(&request, decision).unwrap();
        self.convo.resolve_permission(&request.request_id, decision);
    }
}

fn turn_end(c: &Conversation) -> Option<(bool, Option<String>)> {
    c.items.iter().rev().find_map(|i| match i {
        Item::TurnEnd {
            is_error, message, ..
        } => Some((*is_error, message.clone())),
        _ => None,
    })
}

/// The turn ended because the user stopped it.
fn interrupted(c: &Conversation) -> bool {
    matches!(
        c.items.last(),
        Some(Item::Notice { text, is_error: false }) if text == "Interrupted"
    )
}

#[test]
fn collaboration_mode_combines_thread_defaults_and_per_prompt_overrides() {
    let fake = Fake::new("modes", "collaboration", "0.157.1");
    let mut session = Session::start(&fake.program, fake.worktree("wt"), None);
    session.process.set_policy(Policy::ReadOnly);
    session
        .process
        .send_turn_in_mode(
            "Plan the work",
            TurnOptions::default(),
            Some(CollaborationMode::Plan),
        )
        .unwrap();
    session
        .process
        .send_turn_in_mode(
            "Implement it",
            TurnOptions {
                model: Some("second-model".into()),
                effort: Some("low".into()),
            },
            Some(CollaborationMode::Default),
        )
        .unwrap();
    session.until("both turn starts", |_| {
        fake.sent("wt", "turn/start").len() == 2
    });
    let turns = fake.sent("wt", "turn/start");
    for (turn, (mode, model, effort)) in turns.iter().zip([
        ("plan", "fake-model", "high"),
        ("default", "second-model", "low"),
    ]) {
        assert_eq!(
            turn["params"]["collaborationMode"],
            json!({
                "mode": mode,
                "settings": {"model": model, "reasoning_effort": effort, "developer_instructions": null}
            })
        );
        assert_eq!(turn["params"]["sandboxPolicy"]["type"], "readOnly");
        assert_eq!(turn["params"]["approvalPolicy"], "on-request");
    }
    assert_eq!(turns[0]["params"]["input"][0]["text"], "Plan the work");
    assert_eq!(turns[1]["params"]["input"][0]["text"], "Implement it");
}

#[test]
fn answers_continue_the_same_turn_with_an_opaque_request_id() {
    let fake = Fake::new("question", "questions", "0.157.1");
    let mut session = Session::start(&fake.program, fake.worktree("wt"), None);
    session.send("Prepare a release plan");
    session.until("blocking question", |s| {
        s.convo.status == Status::NeedsInput
    });
    let request = session.convo.pending_user_input().unwrap().clone();
    assert_eq!(request.questions.len(), 2);
    let answers = Answers::from([
        ("base".to_string(), vec!["develop".to_string()]),
        ("note".to_string(), vec!["Deploy after tests".to_string()]),
    ]);
    session
        .process
        .respond_to_questions(&request.request_id, &answers)
        .unwrap();
    session
        .convo
        .resolve_user_input(&request.request_id, InputResolution::Answered);
    session.until("original turn finishes", |s| turn_end(&s.convo).is_some());
    assert_eq!(
        session
            .convo
            .user_input(&request.request_id)
            .unwrap()
            .resolution,
        InputResolution::Answered
    );
    assert_eq!(fake.sent("wt", "turn/start").len(), 1);
    assert_eq!(
        fake.sent("wt", "initialize")[0]["params"]["capabilities"]["experimentalApi"],
        true
    );
    assert!(
        fake.received("wt")
            .contains(&json!({"id":"choice-1","result":{
                "answers":{"base":{"answers":["develop"]},"note":{"answers":["Deploy after tests"]}}
            }}))
    );
}

#[test]
fn approved_command_completes_the_turn() {
    let fake = Fake::new("approve", "normal", "0.157.1");
    let mut s = Session::start(&fake.program, fake.worktree("wt"), None);
    // Sent before the handshake finished: queued by the driver.
    s.send("list the files");
    s.until("the approval prompt", |s| {
        s.convo.status == Status::NeedsPermission
    });
    assert_eq!(s.convo.session_id.as_deref(), Some("th-wt"));
    assert_eq!(s.convo.model.as_deref(), Some("fake-model"));
    s.answer(Decision::Allowed);
    s.until("the turn end", |s| turn_end(&s.convo).is_some());
    assert_eq!(turn_end(&s.convo), Some((false, None)));
    let tool = s.convo.tool("c1").unwrap();
    assert_eq!(tool.result.as_ref().unwrap().text, "a.txt");
    assert!(!tool.result.as_ref().unwrap().is_error);
    assert_eq!(s.convo.status, Status::Idle);

    let start = &fake.sent("wt", "thread/start")[0];
    assert_eq!(start["params"]["approvalPolicy"], "on-request");
    assert_eq!(start["params"]["sandbox"], "workspace-write");
    assert_eq!(
        start["params"]["developerInstructions"],
        "You work in a worktree."
    );
    let turn = &fake.sent("wt", "turn/start")[0];
    assert!(turn["params"].get("collaborationMode").is_none());
    assert_eq!(turn["params"]["threadId"], "th-wt");
    assert_eq!(turn["params"]["input"][0]["text"], "list the files");
    assert_eq!(turn["params"]["sandboxPolicy"]["type"], "workspaceWrite");
    assert!(
        fake.received("wt")
            .iter()
            .any(|v| v["id"] == 0 && v["result"]["decision"] == "accept")
    );
    assert_eq!(fake.sent("wt", "initialized").len(), 1);
}

#[test]
fn denied_command_is_declined_and_policy_changes_apply_next_turn() {
    let fake = Fake::new("deny", "normal", "0.157.1");
    let mut s = Session::start(&fake.program, fake.worktree("wt"), None);
    s.send("list");
    s.until("the approval prompt", |s| {
        s.convo.status == Status::NeedsPermission
    });
    s.answer(Decision::Denied);
    s.until("the turn end", |s| turn_end(&s.convo).is_some());
    let result = s.convo.tool("c1").unwrap().result.clone().unwrap();
    assert!(result.is_error);
    assert!(
        fake.received("wt")
            .iter()
            .any(|v| v["id"] == 0 && v["result"]["decision"] == "decline")
    );

    s.process.set_policy(Policy::ReadOnly);
    s.convo.push_user("again");
    s.process
        .send_turn(
            "again",
            TurnOptions {
                model: Some("second-model".into()),
                effort: Some("high".into()),
            },
        )
        .unwrap();
    s.until("the second prompt", |s| {
        s.convo.status == Status::NeedsPermission
    });
    let turns = fake.sent("wt", "turn/start");
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1]["params"]["sandboxPolicy"]["type"], "readOnly");
    assert_eq!(turns[1]["params"]["model"], "second-model");
    assert_eq!(turns[1]["params"]["effort"], "high");
}

#[test]
fn interrupt_stops_the_turn_and_expires_the_prompt() {
    let fake = Fake::new("interrupt", "normal", "0.157.1");
    let mut s = Session::start(&fake.program, fake.worktree("wt"), None);
    s.send("list");
    s.until("the approval prompt", |s| {
        s.convo.status == Status::NeedsPermission
    });
    s.process.interrupt().unwrap();
    s.until("the turn end", |s| interrupted(&s.convo));
    // The user's own Stop is not a failed turn.
    assert_eq!(turn_end(&s.convo), None);
    assert_eq!(s.convo.status, Status::Idle);
    assert_eq!(
        s.convo.permission("0").unwrap().decision,
        Some(Decision::Expired)
    );
    let interrupt = &fake.sent("wt", "turn/interrupt")[0];
    assert_eq!(interrupt["params"]["threadId"], "th-wt");
    assert_eq!(interrupt["params"]["turnId"], "tu-1");
}

#[test]
fn crash_reports_and_the_next_turn_resumes_the_thread() {
    let fake = Fake::new("crash", "crash", "0.157.1");
    let mut s = Session::start(&fake.program, fake.worktree("wt"), None);
    s.send("go");
    s.until("the crash", |s| s.exited);
    assert_eq!(s.convo.status, Status::Exited);
    assert!(s.convo.items.iter().any(|i| matches!(i,
        Item::Notice { text, is_error: true } if text == "fatal: model stream died")));
    // The visible transcript survives; the next message resumes the thread.
    let thread = s.convo.session_id.clone().unwrap();
    let items = s.convo.items.len();
    let mut again = Session::start(&fake.program, fake.worktree("wt"), Some(&thread));
    again.convo = std::mem::take(&mut s.convo);
    again.send("go on");
    again.until("the restart", |s| s.exited);
    assert!(again.convo.items.len() > items);
    let resume = &fake.sent("wt", "thread/resume")[0];
    assert_eq!(resume["params"]["threadId"], "th-wt");
    assert_eq!(resume["params"]["excludeTurns"], true);
    // The thread keeps the instructions it started with.
    assert!(resume["params"].get("developerInstructions").is_none());
}

#[test]
fn a_fork_continues_a_copy_of_the_thread() {
    let fake = Fake::new("fork", "normal", "0.157.1");
    let mut s = Session::spawn(&fake.program, fake.worktree("wt"), Some("th-wt"), true);
    s.send("go on from here");
    s.until("the prompt", |s| s.convo.status == Status::NeedsPermission);
    // The copy has an id of its own; the original is not resumed.
    assert_eq!(s.convo.session_id.as_deref(), Some("fork-th-wt"));
    assert!(fake.sent("wt", "thread/resume").is_empty());
    let fork = &fake.sent("wt", "thread/fork")[0];
    assert_eq!(fork["params"]["threadId"], "th-wt");
    assert_eq!(fork["params"]["excludeTurns"], true);
    assert!(fork["params"]["cwd"].as_str().unwrap().ends_with("wt"));
    let turn = &fake.sent("wt", "turn/start")[0];
    assert_eq!(turn["params"]["threadId"], "fork-th-wt");
}

#[test]
fn simultaneous_sessions_in_different_worktrees_stay_apart() {
    let fake = Fake::new("parallel", "normal", "0.157.1");
    let mut a = Session::start(&fake.program, fake.worktree("wt-a"), None);
    let mut b = Session::start(&fake.program, fake.worktree("wt-b"), None);
    a.send("in a");
    b.send("in b");
    a.until("a's prompt", |s| s.convo.status == Status::NeedsPermission);
    b.until("b's prompt", |s| s.convo.status == Status::NeedsPermission);
    b.answer(Decision::Denied);
    a.answer(Decision::Allowed);
    a.until("a's end", |s| turn_end(&s.convo).is_some());
    b.until("b's end", |s| turn_end(&s.convo).is_some());
    assert_eq!(a.convo.session_id.as_deref(), Some("th-wt-a"));
    assert_eq!(b.convo.session_id.as_deref(), Some("th-wt-b"));
    assert!(
        !a.convo
            .tool("c1")
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .is_error
    );
    assert!(
        b.convo
            .tool("c1")
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .is_error
    );
    assert_eq!(
        fake.sent("wt-a", "turn/start")[0]["params"]["input"][0]["text"],
        "in a"
    );
    assert_eq!(
        fake.sent("wt-b", "turn/start")[0]["params"]["input"][0]["text"],
        "in b"
    );
}

#[test]
fn signed_out_codex_is_reported() {
    let fake = Fake::new("signedout", "signed-out", "0.157.1");
    let mut s = Session::start(&fake.program, fake.worktree("wt"), None);
    s.send("hi");
    s.until("the exit", |s| s.exited);
    assert!(s.convo.items.iter().any(|i| matches!(i,
        Item::Notice { text, is_error: true } if text.contains("codex login"))));
    assert!(fake.sent("wt", "thread/start").is_empty());
    assert_eq!(s.convo.status, Status::Exited);
}

#[test]
fn old_codex_is_refused_before_starting() {
    let fake = Fake::new("old", "normal", "0.120.0");
    let mut s = Session::start(&fake.program, fake.worktree("wt"), None);
    s.send("hi");
    s.until("the exit", |s| s.exited);
    assert!(s.convo.items.iter().any(|i| matches!(i,
        Item::Notice { text, is_error: true } if text.contains("needs 0.157.0 or newer"))));
    assert!(fake.received("wt").is_empty());
}

#[test]
fn stop_before_the_handshake_drops_the_waiting_turn() {
    let fake = Fake::new("early_stop", "normal", "0.157.1");
    let mut s = Session::start(&fake.program, fake.worktree("wt"), None);
    s.send("hi");
    s.process.interrupt().unwrap();
    s.until("the turn end", |s| interrupted(&s.convo));
    assert_eq!(s.convo.status, Status::Idle);
}
