//! Talks to the real Codex CLI (needs Codex installed and signed in). Run
//! with: cargo test -p sg_codex --test live -- --ignored --nocapture
//! `reads_a_real_thread` costs nothing; `answers_a_turn` spends a few cents.

use std::time::{Duration, Instant};

use futures::channel::mpsc::TryRecvError;
use sg_agents::chat::{Conversation, Item, Status};
use sg_agents::process::{AgentProcess, ProcessEvent};
use sg_codex::{CodexFold, CodexProcess, Policy, SpawnOptions};

/// History through `thread/turns/list`, no agent started. Reads the most
/// recent thread under `SUPERGIT_CODEX_CWD` (default: this repository).
#[test]
#[ignore]
fn reads_a_real_thread() {
    let cwd = std::env::var("SUPERGIT_CODEX_CWD")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap());
    let started = Instant::now();
    let install = sg_codex::install::locate().unwrap();
    println!(
        "codex {} at {} ({:?})",
        install.version,
        install.program.display(),
        started.elapsed()
    );
    let sessions =
        sg_codex::CodexSessionIndex::new().sessions_by_worktree(std::slice::from_ref(&cwd));
    let Some(session) = sessions[0].first() else {
        println!("no Codex sessions under {}", cwd.display());
        return;
    };
    let started = Instant::now();
    let history = sg_codex::read_history(None, &cwd, &session.id).unwrap();
    println!(
        "{} turns of “{}” in {:?}",
        history.turns.len(),
        session.title,
        started.elapsed()
    );
    let mut convo = Conversation::new();
    CodexFold::new().apply_history(&mut convo, &history);
    assert_eq!(convo.session_id.as_deref(), Some(session.id.as_str()));
    assert!(convo.items.iter().any(|i| matches!(i, Item::User { .. })));
}

/// One read-only turn in a scratch repository.
#[test]
#[ignore]
fn answers_a_turn() {
    let dir = std::env::temp_dir().join(format!("sg_codex_live_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .status()
        .unwrap();
    let (process, mut events) = CodexProcess::spawn(&SpawnOptions {
        cwd: dir.clone(),
        policy: Policy::ReadOnly,
        ..Default::default()
    });
    let mut fold = CodexFold::new();
    let mut convo = Conversation::new();
    convo.push_user("Reply with the single word: pong");
    process
        .send_user("Reply with the single word: pong")
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    while convo.status != Status::Idle && Instant::now() < deadline {
        match events.try_recv() {
            Ok(ProcessEvent::Message(v)) => {
                fold.apply(&mut convo, &v);
            }
            Ok(ProcessEvent::Stderr(line)) => println!("stderr: {line}"),
            Ok(ProcessEvent::Exited) | Err(TryRecvError::Closed) => break,
            Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    process.kill();
    std::fs::remove_dir_all(&dir).ok();
    println!("{:#?}", convo.items);
    assert!(convo.session_id.is_some());
    assert!(convo.items.iter().any(|i| matches!(i,
        Item::Text { text, .. } if text.to_lowercase().contains("pong"))));
    assert!(matches!(
        convo.items.last(),
        Some(Item::TurnEnd {
            is_error: false,
            ..
        })
    ));
}
