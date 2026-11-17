//! The fold against protocol events recorded from `codex app-server` 0.157.1
//! (trimmed to the fields that matter).

use serde_json::json;

use super::*;

const THREAD: &str = "01a1e8a1-0f3f-7a33-a11c-34a74886f792";
const TURN: &str = "01a1e8a1-10aa-76b3-bf21-419db0221c30";

fn feed(fold: &mut CodexFold, c: &mut Conversation, events: &[Value]) -> Vec<Reply> {
    events.iter().filter_map(|e| fold.apply(c, e)).collect()
}

fn item(method: &str, item: Value) -> Value {
    json!({"method": method, "params": {"item": item, "threadId": THREAD, "turnId": TURN}})
}

fn started_session() -> (CodexFold, Conversation) {
    let mut fold = CodexFold::new();
    let mut c = Conversation::new();
    c.push_user("Create a file a.txt containing hello, then cat it.");
    feed(
        &mut fold,
        &mut c,
        &[
            json!({"response": "initialize", "result": {"userAgent": "supergit/0.157.1", "codexHome": "/Users/me/.codex"}}),
            json!({"response": "account/read", "result": {"account": {"type": "chatgpt", "email": null, "planType": "team"}, "requiresOpenaiAuth": true}}),
            json!({"response": "thread/start", "result": {"thread": {"id": THREAD, "cwd": "/tmp/probe", "model": "gpt-6-astra", "turns": []}, "model": "gpt-6-astra", "cwd": "/tmp/probe"}}),
            json!({"method": "thread/started", "params": {"thread": {"id": THREAD, "cwd": "/tmp/probe"}}}),
            json!({"method": "mcpServer/startupStatus/updated", "params": {"threadId": THREAD, "name": "metabase", "status": "failed"}}),
            json!({"response": "turn/start", "result": {"turn": {"id": TURN, "items": [], "status": "inProgress"}}}),
            json!({"method": "turn/started", "params": {"threadId": THREAD, "turn": {"id": TURN, "status": "inProgress"}}}),
            item(
                "item/started",
                json!({"type": "userMessage", "id": "u1", "content": [{"type": "text", "text": "Create a file a.txt containing hello, then cat it."}]}),
            ),
            item(
                "item/completed",
                json!({"type": "userMessage", "id": "u1", "content": [{"type": "text", "text": "Create a file a.txt containing hello, then cat it."}]}),
            ),
        ],
    );
    (fold, c)
}

#[test]
fn streams_a_turn_with_approvals_and_tools() {
    let (mut fold, mut c) = started_session();
    assert_eq!(c.session_id.as_deref(), Some(THREAD));
    assert_eq!(c.model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(c.cwd.as_deref(), Some(std::path::Path::new("/tmp/probe")));
    assert_eq!(c.status, Status::Working);
    // The user's turn is echoed locally only once.
    assert_eq!(c.items.len(), 1);

    let replies = feed(
        &mut fold,
        &mut c,
        &[
            item(
                "item/started",
                json!({"type": "agentMessage", "id": "m1", "text": "", "phase": "commentary"}),
            ),
            json!({"method": "item/agentMessage/delta", "params": {"threadId": THREAD, "turnId": TURN, "itemId": "m1", "delta": "I"}}),
            json!({"method": "item/agentMessage/delta", "params": {"threadId": THREAD, "turnId": TURN, "itemId": "m1", "delta": "’ll create"}}),
        ],
    );
    assert!(replies.is_empty());
    assert_eq!(
        c.items[1],
        Item::Text {
            text: "I’ll create".into(),
            streaming: true
        }
    );

    feed(
        &mut fold,
        &mut c,
        &[
            item(
                "item/completed",
                json!({"type": "agentMessage", "id": "m1", "text": "I’ll create a.txt and read it back.\n", "phase": "commentary"}),
            ),
            item(
                "item/started",
                json!({"type": "fileChange", "id": "exec-a6", "changes": [{"path": "/tmp/probe/a.txt", "kind": {"type": "add"}, "diff": "hello\n"}], "status": "inProgress"}),
            ),
            json!({"method": "thread/status/changed", "params": {"threadId": THREAD, "status": {"type": "active", "activeFlags": ["waitingOnApproval"]}}}),
            json!({"method": "item/fileChange/requestApproval", "id": 0, "params": {"threadId": THREAD, "turnId": TURN, "itemId": "exec-a6", "startedAtMs": 1, "reason": null, "grantRoot": null}}),
        ],
    );
    assert_eq!(
        c.items[1],
        Item::Text {
            text: "I’ll create a.txt and read it back.\n".into(),
            streaming: false
        }
    );
    assert_eq!(c.status, Status::NeedsPermission);
    let p = c.pending_permission().unwrap().clone();
    assert_eq!(p.request_id, "0");
    assert_eq!(p.tool_name, "Edit");
    assert_eq!(p.session_label.as_deref(), Some("Allow edits this session"));
    assert_eq!(p.suggestions[0]["allow"], json!({"decision": "accept"}));
    assert_eq!(p.suggestions[0]["deny"], json!({"decision": "decline"}));
    let preview = tools::permission_patch(&p, c.cwd.as_deref()).unwrap();
    assert!(preview.contains("+++ b/a.txt\n@@ -0,0 +1,1 @@\n+hello\n"));

    // The host answers; Codex confirms and moves on.
    c.resolve_permission("0", Decision::Allowed);
    assert_eq!(c.status, Status::Working);
    feed(
        &mut fold,
        &mut c,
        &[
            json!({"method": "serverRequest/resolved", "params": {"threadId": THREAD, "requestId": 0}}),
            item(
                "item/completed",
                json!({"type": "fileChange", "id": "exec-a6", "changes": [{"path": "/tmp/probe/a.txt", "kind": {"type": "add"}, "diff": "hello\n"}], "status": "completed"}),
            ),
            item(
                "item/started",
                json!({"type": "commandExecution", "id": "exec-c7", "command": "/bin/zsh -lc 'cat a.txt'", "cwd": "/tmp/probe", "status": "inProgress", "commandActions": [], "aggregatedOutput": null, "exitCode": null}),
            ),
            json!({"method": "item/commandExecution/requestApproval", "id": 1, "params": {"kind": "command", "threadId": THREAD, "turnId": TURN, "itemId": "exec-c7", "startedAtMs": 2, "environmentId": "local", "command": "/bin/zsh -lc 'cat a.txt'", "cwd": "/tmp/probe", "proposedExecpolicyAmendment": ["cat", "a.txt"], "availableDecisions": ["accept", {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["cat", "a.txt"]}}, "cancel"]}}),
        ],
    );
    assert_eq!(
        c.permission("0").unwrap().decision,
        Some(Decision::Allowed),
        "an answered prompt stays answered"
    );
    let edit = c.tool("exec-a6").unwrap();
    assert!(!edit.result.as_ref().unwrap().is_error);
    assert!(tools::edit_patch(edit, c.cwd.as_deref()).is_some());

    let p = c.pending_permission().unwrap().clone();
    assert_eq!(p.request_id, "1");
    assert_eq!(p.display_name, "command");
    assert_eq!(tools::permission_text(&p).as_deref(), Some("cat a.txt"));
    assert_eq!(p.session_label.as_deref(), Some("Always allow `cat a.txt`"));
    assert_eq!(
        p.suggestions[0]["session"],
        json!({"decision": {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["cat", "a.txt"]}}})
    );
    // No `decline` offered: denying cancels the turn.
    assert_eq!(p.suggestions[0]["deny"], json!({"decision": "cancel"}));

    c.resolve_permission("1", Decision::AllowedForSession);
    feed(
        &mut fold,
        &mut c,
        &[
            item(
                "item/completed",
                json!({"type": "commandExecution", "id": "exec-c7", "command": "/bin/zsh -lc 'cat a.txt'", "cwd": "/tmp/probe", "status": "completed", "commandActions": [], "aggregatedOutput": "hello\n", "exitCode": 0, "durationMs": 0}),
            ),
            json!({"method": "turn/diff/updated", "params": {"threadId": THREAD, "turnId": TURN, "diff": "diff --git a/a.txt b/a.txt\n"}}),
            item(
                "item/started",
                json!({"type": "reasoning", "id": "r1", "summary": [], "content": []}),
            ),
            item(
                "item/completed",
                json!({"type": "reasoning", "id": "r1", "summary": [], "content": []}),
            ),
            item(
                "item/started",
                json!({"type": "agentMessage", "id": "m2", "text": "", "phase": "final_answer"}),
            ),
            item(
                "item/completed",
                json!({"type": "agentMessage", "id": "m2", "text": "done", "phase": "final_answer"}),
            ),
            json!({"method": "thread/status/changed", "params": {"threadId": THREAD, "status": {"type": "idle"}}}),
            json!({"method": "turn/completed", "params": {"threadId": THREAD, "turn": {"id": TURN, "items": [], "status": "completed", "error": null, "durationMs": 20373}}}),
        ],
    );
    let cat = c.tool("exec-c7").unwrap();
    assert_eq!(cat.name, "Shell");
    assert_eq!(cat.result.as_ref().unwrap().text, "hello\n");
    assert_eq!(tools::summary(cat, c.cwd.as_deref()), "cat a.txt");
    assert_eq!(c.status, Status::Idle);
    assert!(matches!(
        c.items.last(),
        Some(Item::TurnEnd {
            duration_ms: Some(20373),
            is_error: false,
            ..
        })
    ));
    assert!(c.items.iter().any(|i| *i
        == Item::Text {
            text: "done".into(),
            streaming: false
        }));
    assert!(
        !c.items
            .iter()
            .any(|i| matches!(i, Item::Thinking { streaming: true }))
    );
}

#[test]
fn denied_and_unanswered_prompts() {
    let (mut fold, mut c) = started_session();
    feed(
        &mut fold,
        &mut c,
        &[
            item(
                "item/started",
                json!({"type": "commandExecution", "id": "x1", "command": "rm -rf build", "cwd": "/tmp/probe", "status": "inProgress"}),
            ),
            json!({"method": "item/commandExecution/requestApproval", "id": 7, "params": {"kind": "command", "itemId": "x1", "command": "rm -rf build", "cwd": "/tmp/probe", "reason": "outside the sandbox", "availableDecisions": ["accept", "acceptForSession", "decline", "cancel"]}}),
        ],
    );
    let p = c.pending_permission().unwrap().clone();
    assert_eq!(p.reason.as_deref(), Some("outside the sandbox"));
    assert_eq!(p.session_label.as_deref(), Some("Allow for this session"));
    assert_eq!(
        p.suggestions[0]["session"],
        json!({"decision": "acceptForSession"})
    );
    assert_eq!(p.suggestions[0]["deny"], json!({"decision": "decline"}));
    c.resolve_permission("7", Decision::Denied);
    feed(
        &mut fold,
        &mut c,
        &[
            item(
                "item/completed",
                json!({"type": "commandExecution", "id": "x1", "command": "rm -rf build", "status": "declined", "aggregatedOutput": null, "exitCode": null}),
            ),
            // A prompt the turn no longer needs (interrupted, auto-reviewed).
            json!({"method": "item/permissions/requestApproval", "id": 8, "params": {"itemId": "x2", "cwd": "/tmp/probe", "reason": "needs network", "permissions": {"network": {"enabled": true}, "fileSystem": null}}}),
        ],
    );
    let declined = c.tool("x1").unwrap().result.clone().unwrap();
    assert!(declined.is_error);
    assert_eq!(declined.text, "Declined");
    let p = c.pending_permission().unwrap().clone();
    assert_eq!(
        p.suggestions[0]["allow"],
        json!({"permissions": {"network": {"enabled": true}}, "scope": "turn"})
    );
    assert_eq!(p.suggestions[0]["session"]["scope"], "session");
    feed(
        &mut fold,
        &mut c,
        &[json!({"method": "serverRequest/resolved", "params": {"requestId": 8}})],
    );
    assert_eq!(c.permission("8").unwrap().decision, Some(Decision::Expired));
    assert_eq!(c.status, Status::Working);
}

#[test]
fn interruption_failure_errors_and_unsupported_requests() {
    let (mut fold, mut c) = started_session();
    let replies = feed(
        &mut fold,
        &mut c,
        &[
            json!({"method": "item/tool/requestUserInput", "id": "q1", "params": {"questions": []}}),
            json!({"method": "mcpServer/elicitation/request", "id": 9, "params": {"serverName": "linear", "mode": "form", "message": "Pick"}}),
            json!({"method": "error", "params": {"error": {"message": "stream disconnected"}, "willRetry": true}}),
            item(
                "item/started",
                json!({"type": "agentMessage", "id": "m1", "text": "Work"}),
            ),
            json!({"method": "turn/completed", "params": {"turn": {"id": TURN, "status": "interrupted", "error": null, "durationMs": 900}}}),
        ],
    );
    assert_eq!(
        replies,
        vec![
            Reply::Unsupported {
                request_id: "\"q1\"".into(),
                method: "item/tool/requestUserInput".into()
            },
            Reply::Respond {
                request_id: "9".into(),
                result: json!({"action": "decline", "content": null, "_meta": null})
            },
        ]
    );
    assert!(c.items.iter().any(|i| matches!(i,
        Item::Notice { text, is_error: false } if text == "stream disconnected — retrying…")));
    assert!(matches!(
        c.items.last(),
        Some(Item::TurnEnd { is_error: true, message: Some(m), .. }) if m == "interrupted"
    ));
    assert!(!c.items.iter().any(|i| matches!(
        i,
        Item::Text {
            streaming: true,
            ..
        }
    )));
    assert_eq!(c.status, Status::Idle);

    c.push_user("again");
    feed(
        &mut fold,
        &mut c,
        &[
            json!({"method": "turn/started", "params": {"turn": {"id": "t2"}}}),
            json!({"method": "turn/completed", "params": {"turn": {"id": "t2", "status": "failed", "error": {"message": "Usage limit reached"}}}}),
        ],
    );
    assert!(matches!(
        c.items.last(),
        Some(Item::TurnEnd { is_error: true, message: Some(m), .. }) if m == "Usage limit reached"
    ));
}

#[test]
fn startup_failures_return_to_idle() {
    let mut fold = CodexFold::new();
    let mut c = Conversation::new();
    c.push_user("hi");
    feed(
        &mut fold,
        &mut c,
        &[
            json!({"response": "account/read", "error": {"message": "Codex is not signed in. Run `codex login` in a terminal, then send again."}}),
        ],
    );
    assert_eq!(c.status, Status::Idle);
    assert!(
        matches!(c.items.last(), Some(Item::Notice { is_error: true, text }) if text.contains("codex login"))
    );
    // The process exit that follows adds nothing.
    c.on_exit(None, "Codex");
    assert_eq!(c.items.len(), 2);

    let mut c = Conversation::new();
    c.push_user("hi");
    feed(
        &mut fold,
        &mut c,
        &[
            json!({"response": "thread/resume", "error": {"message": "no rollout found for thread id x"}}),
        ],
    );
    assert!(
        matches!(c.items.last(), Some(Item::Notice { is_error: true, text })
        if text == "Can't resume this Codex thread: no rollout found for thread id x")
    );
}

#[test]
fn rebuilds_history_from_turns() {
    let history = History {
        thread: json!({"id": THREAD, "name": "Review the CQL skill", "cwd": "/w/repo", "model": "gpt-6-astra"}),
        turns: vec![
            json!({"id": "t1", "status": "interrupted", "items": [
                {"type": "userMessage", "id": "u1", "content": [{"type": "text", "text": "Review it"}]},
                {"type": "agentMessage", "id": "m1", "text": "Reading the skill first.", "phase": "commentary"},
            ]}),
            json!({"id": "t2", "status": "completed", "items": [
                {"type": "userMessage", "id": "u2", "content": [{"type": "text", "text": "Continue"}]},
                {"type": "reasoning", "id": "r1", "summary": [], "content": []},
                {"type": "commandExecution", "id": "c1", "command": "/bin/zsh -lc 'ls'", "cwd": "/w/repo", "status": "completed", "aggregatedOutput": "a\n", "exitCode": 0},
                {"type": "fileChange", "id": "f1", "status": "completed", "changes": [{"path": "/w/repo/a.txt", "kind": {"type": "update", "move_path": null}, "diff": "@@ -1 +1 @@\n-a\n+b\n"}]},
                {"type": "contextCompaction", "id": "k1"},
                {"type": "agentMessage", "id": "m2", "text": "Done.", "phase": "final_answer"},
            ]}),
            json!({"id": "t3", "status": "failed", "error": {"message": "boom"}, "items": []}),
        ],
    };
    let mut c = Conversation::new();
    CodexFold::new().apply_history(&mut c, &history);
    assert_eq!(c.session_id.as_deref(), Some(THREAD));
    assert_eq!(c.title.as_deref(), Some("Review the CQL skill"));
    assert_eq!(c.model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(c.status, Status::Idle);
    let kinds: Vec<String> = c
        .items
        .iter()
        .map(|i| match i {
            Item::User { text } => format!("user:{text}"),
            Item::Text { text, .. } => format!("text:{text}"),
            Item::Thinking { .. } => "thinking".into(),
            Item::Tool(t) => format!("tool:{}", t.name),
            Item::Notice { text, .. } => format!("notice:{text}"),
            _ => "other".into(),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "user:Review it",
            "text:Reading the skill first.",
            "notice:Interrupted",
            "user:Continue",
            "tool:Shell",
            "tool:Edit",
            "notice:Conversation compacted",
            "text:Done.",
            "notice:boom",
        ]
    );
    let edit = c.tool("f1").unwrap();
    assert!(
        tools::edit_patch(edit, c.cwd.as_deref())
            .unwrap()
            .contains("--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-a\n+b\n")
    );
}
