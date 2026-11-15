//! Talks to the real `claude` CLI (needs Claude Code installed and logged in,
//! spends a few cents). Run with: cargo test -p sg_claude -- --ignored

use futures::StreamExt as _;
use sg_claude::conversation::Reply;
use sg_claude::{ClaudeProcess, Conversation, Decision, Item, ProcessEvent, SpawnOptions, Status};

#[test]
#[ignore]
fn permission_round_trip_with_real_claude() {
    let dir = std::env::temp_dir().join(format!("sg_claude_live_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let opts = SpawnOptions {
        cwd: dir.clone(),
        ..Default::default()
    };
    let (process, mut events) = ClaudeProcess::spawn(&opts).expect("spawn claude");
    let mut convo = Conversation::new();
    let prompt = "Create a file named note.txt containing the word ok using the Write tool, then reply with just: done";
    convo.push_user(prompt);
    process.send_user(prompt).unwrap();

    futures::executor::block_on(async {
        while let Some(ev) = events.next().await {
            match ev {
                ProcessEvent::Message(v) => {
                    if let Some(Reply::Unsupported {
                        request_id,
                        subtype,
                    }) = convo.apply(&v)
                    {
                        process
                            .control_error(&request_id, &format!("unsupported: {subtype}"))
                            .unwrap();
                    }
                    if let Some(p) = convo.pending_permission().cloned() {
                        process.allow(&p.request_id, &p.input, None).unwrap();
                        convo.resolve_permission(&p.request_id, Decision::Allowed);
                    }
                    if convo.status == Status::Idle {
                        break;
                    }
                }
                ProcessEvent::Stderr(_) => {}
                ProcessEvent::Exited => panic!("claude exited early"),
            }
        }
    });

    assert!(convo.session_id.is_some());
    assert!(
        convo
            .items
            .iter()
            .any(|i| matches!(i, Item::Permission(p) if p.tool_name == "Write"))
    );
    assert!(
        convo
            .items
            .iter()
            .any(|i| matches!(i, Item::Tool(t) if t.name == "Write" && t.result.is_some()))
    );
    assert!(convo.items.iter().any(|i| matches!(
        i,
        Item::TurnEnd {
            is_error: false,
            ..
        }
    )));
    assert_eq!(
        std::fs::read_to_string(dir.join("note.txt"))
            .unwrap()
            .trim(),
        "ok"
    );
    process.kill();
    std::fs::remove_dir_all(&dir).ok();
}
