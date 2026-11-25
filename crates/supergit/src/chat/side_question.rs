//! The side card belongs to this chat view. It is independent of the main
//! transcript, attention state, and the Stop button.
use super::*;

pub(super) fn question(text: &str) -> Option<&str> {
    let rest = text.trim().strip_prefix("/btw")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim())
}

pub(super) struct SideQuestion {
    question: String,
    state: State,
}

enum State {
    Claude(String),
    Codex,
    Answer(String),
    Error(String),
}

impl SideQuestion {
    pub(super) fn failed(question: &str, error: String) -> Self {
        Self {
            question: question.into(),
            state: State::Error(error),
        }
    }

    fn pending(&self) -> bool {
        matches!(self.state, State::Claude(_) | State::Codex)
    }
}

impl ChatSession {
    pub(super) fn side_unavailable(&self, question: &str) -> Option<&'static str> {
        if question.is_empty() {
            Some("Type a question after /btw")
        } else if self.git_busy {
            Some("A Git operation is running")
        } else if self.history_loading() {
            Some("Loading the conversation…")
        } else if self.side.as_ref().is_some_and(SideQuestion::pending) {
            Some("Wait for the side answer or cancel it first")
        } else if self.convo.session_id.is_none() {
            Some("Send a regular message first to give the question context")
        } else {
            None
        }
    }

    pub(super) fn ask_side(&mut self, prompt: &str, cx: &mut Context<Self>) -> bool {
        let label = prompt
            .split(crate::agent_context::CONTEXT_MARKER)
            .next()
            .unwrap_or_default();
        if self.side_unavailable(label).is_some() {
            return false;
        }
        let state = match self.provider() {
            Provider::Claude => {
                let result = self.ensure_process(cx).and_then(|_| match &self.backend {
                    Backend::Claude {
                        process: Some(p), ..
                    } => p.ask_side_question(prompt),
                    _ => anyhow::bail!("Claude Code is unavailable"),
                });
                match result {
                    Ok(id) => State::Claude(id),
                    Err(error) => State::Error(format!("{error:#}")),
                }
            }
            Provider::Codex => {
                let Some(parent) = self.convo.session_id.clone() else {
                    return false;
                };
                let answer = sg_codex::ask_side_question(
                    sg_codex::SpawnOptions {
                        cwd: self.cwd.clone(),
                        ..Default::default()
                    },
                    parent,
                    prompt.into(),
                    self.options.codex_turn(self.convo.model.as_deref()),
                );
                self.side_worker = Some(cx.spawn(async move |this, cx| {
                    let answer = cx.background_executor().spawn(answer).await;
                    let _ = this.update(cx, |this, cx| {
                        this.finish_side(answer.map_err(|e| format!("{e:#}")), cx)
                    });
                }));
                State::Codex
            }
        };
        let accepted = !matches!(state, State::Error(_));
        self.side = Some(SideQuestion {
            question: label.into(),
            state,
        });
        if accepted {
            self.side_timeout = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_secs(600))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.cancel_side();
                    this.finish_side(
                        Err("The side question timed out. Try /btw again.".into()),
                        cx,
                    );
                });
            }));
        }
        cx.notify();
        accepted
    }

    fn cancel_side(&mut self) {
        if let Some(SideQuestion {
            state: State::Claude(id),
            ..
        }) = &self.side
            && let Backend::Claude {
                process: Some(p), ..
            } = &self.backend
        {
            let _ = p.cancel_side_question(id);
        }
        self.side_worker = None;
        self.side_timeout = None;
    }

    pub(super) fn dismiss_side(&mut self) {
        self.cancel_side();
        self.side = None;
    }

    fn finish_side(&mut self, result: Result<String, String>, cx: &mut Context<Self>) {
        if let Some(side) = &mut self.side {
            side.state = match result {
                Ok(text) => State::Answer(text),
                Err(error) => State::Error(error),
            };
        }
        self.side_timeout = None;
        cx.notify();
    }

    pub(super) fn side_event(&mut self, event: &ProcessEvent, cx: &mut Context<Self>) -> bool {
        let Some(SideQuestion {
            state: State::Claude(id),
            ..
        }) = &self.side
        else {
            return false;
        };
        match event {
            ProcessEvent::Exited => {
                self.finish_side(Err("Claude Code exited before answering.".into()), cx);
                false // The main conversation also needs this event.
            }
            ProcessEvent::Message(v)
                if v["type"] == "control_response"
                    && v["response"]["request_id"].as_str() == Some(id.as_str()) =>
            {
                self.finish_side(claude_answer(&v["response"]), cx);
                true
            }
            _ => false,
        }
    }

    pub(super) fn render_side(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let side = self.side.as_ref()?;
        let theme = cx.theme();
        let (text, error) = match &side.state {
            State::Claude(_) | State::Codex => ("Waiting for an answer…", false),
            State::Answer(text) => (text.as_str(), false),
            State::Error(text) => (text.as_str(), true),
        };
        Some(
            v_flex()
                .min_w_0()
                .gap_1()
                .p_2()
                .rounded_md()
                .bg(theme.muted)
                .child(
                    h_flex()
                        .justify_between()
                        .child(div().text_xs().child(if error {
                            "Side question failed"
                        } else {
                            "Side question"
                        }))
                        .child(
                            Button::new("dismiss-side")
                                .xsmall()
                                .ghost()
                                .icon(IconName::Close)
                                .tooltip(if side.pending() {
                                    "Cancel the side question"
                                } else {
                                    "Dismiss the side answer"
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.dismiss_side();
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    v_flex()
                        .id("side-answer")
                        .max_h(px(150.))
                        .overflow_y_scroll()
                        .gap_1()
                        .text_sm()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(side.question.clone()),
                        )
                        .child(if matches!(side.state, State::Answer(_)) {
                            TextView::markdown("side-markdown", text.to_owned())
                                .selectable(true)
                                .into_any_element()
                        } else {
                            div()
                                .when(error, |d| d.text_color(theme.danger))
                                .child(text.to_owned())
                                .into_any_element()
                        }),
                )
                .into_any_element(),
        )
    }
}

fn claude_answer(response: &Value) -> Result<String, String> {
    if response["subtype"] == "error" {
        return Err(response["error"]
            .as_str()
            .unwrap_or("This Claude Code version could not answer the side question.")
            .to_owned());
    }
    response
        .pointer("/response/response")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "Claude Code returned no side answer.".into())
}

#[cfg(test)]
mod tests {
    use super::{claude_answer, question};
    use serde_json::json;

    #[test]
    fn only_a_standalone_command_opens_the_card() {
        assert_eq!(question(" /btw\n Почему? "), Some("Почему?"));
        assert_eq!(question("/btw"), Some(""));
        assert_eq!(question("/btween"), None);
        assert_eq!(question("explain /btw"), None);
    }

    #[test]
    fn null_answers_and_provider_errors_finish_the_card() {
        assert_eq!(
            claude_answer(&json!({"subtype":"success","response":{"response":"Yes"}})),
            Ok("Yes".into())
        );
        assert!(claude_answer(&json!({"subtype":"success","response":{"response":null}})).is_err());
        assert_eq!(
            claude_answer(&json!({"subtype":"error","error":"unknown request"})),
            Err("unknown request".into())
        );
    }
}
