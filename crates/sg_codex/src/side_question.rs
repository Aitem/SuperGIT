//! One question in an ephemeral fork. The private driver owns the child's event
//! stream and process, so its turns and approvals never enter the parent chat.
use anyhow::{Context, Result, bail};
use futures::StreamExt as _;
use serde_json::Value;
use sg_agents::process::ProcessEvent;

use crate::{CodexProcess, Policy, SpawnOptions, TurnOptions};

pub async fn ask_side_question(
    opts: SpawnOptions,
    parent: String,
    question: String,
    options: TurnOptions,
) -> Result<String> {
    let (process, mut events) = CodexProcess::spawn_driver(
        &SpawnOptions {
            cwd: opts.cwd,
            program: opts.program,
            policy: Policy::ReadOnly,
            model: options.model.clone(),
            ..Default::default()
        },
        Some(parent),
    );
    process.send_turn(&question, options)?;
    let mut thread = None;
    let mut answer = String::new();
    let mut stderr = None;
    while let Some(event) = events.next().await {
        match event {
            ProcessEvent::Message(v) => {
                if let Some(error) = v.pointer("/error/message").and_then(Value::as_str) {
                    bail!("{error}");
                }
                if v["response"] == "thread/fork" {
                    thread = v
                        .pointer("/result/thread/id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    continue;
                }
                if let Some(source) = v.pointer("/params/threadId").and_then(Value::as_str)
                    && Some(source) != thread.as_deref()
                {
                    continue;
                }
                if v.get("id").is_some() {
                    bail!("Codex requested an interactive action during a side question");
                }
                match v.get("method").and_then(Value::as_str) {
                    Some("item/completed")
                        if v.pointer("/params/item/type").and_then(Value::as_str)
                            == Some("agentMessage") =>
                    {
                        if let Some(text) = v.pointer("/params/item/text").and_then(Value::as_str) {
                            if !answer.is_empty() {
                                answer.push_str("\n\n");
                            }
                            answer.push_str(text);
                        }
                    }
                    Some("turn/completed") => {
                        let turn = &v["params"]["turn"];
                        if turn["status"] != "completed" {
                            bail!(
                                "{}",
                                turn.pointer("/error/message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("Side question interrupted")
                            );
                        }
                        anyhow::ensure!(!answer.trim().is_empty(), "Codex returned no answer");
                        return Ok(answer);
                    }
                    _ => {}
                }
            }
            ProcessEvent::Stderr(line) => stderr = Some(line),
            ProcessEvent::Exited => break,
        }
    }
    Err(anyhow::anyhow!(
        "{}",
        stderr.unwrap_or_else(|| "Codex exited".into())
    ))
    .context("Side question ended before an answer arrived")
}
