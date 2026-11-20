//! What the agent is doing right now, in one line above the composer: the
//! chat's sign of life between a prompt and the first word of the answer,
//! and through thinking, long tool calls and compaction, when nothing new
//! appears in the conversation itself.

use std::time::{Duration, Instant};

use gpui_kit::component::shimmer::ShimmerText;
use gpui_kit::component::spinner::Spinner;
use sg_agents::chat::{Conversation, Item, Status};

use super::look::{self, GUTTER, STEP_TEXT};
use super::*;

/// How far back to look for what is running: the steps of the turn's tail.
const TAIL: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Activity {
    Starting,
    Thinking,
    Writing,
    /// A tool is running: its name as its row shows it.
    Running(String),
    Working,
    Compacting,
    NeedsApproval,
    NeedsAnswer,
}

impl Activity {
    /// What the conversation is busy with, `None` when it is not. `name`:
    /// a tool call's name as its row shows it.
    pub(super) fn of(convo: &Conversation, name: &dyn Fn(&ToolCall) -> String) -> Option<Self> {
        Some(match convo.status {
            Status::Idle | Status::Exited => return None,
            Status::NeedsPermission => Self::NeedsApproval,
            Status::NeedsInput => Self::NeedsAnswer,
            _ if convo.compacting => Self::Compacting,
            Status::Starting => Self::Starting,
            Status::Working => convo
                .items
                .iter()
                .rev()
                .take(TAIL)
                .take_while(|item| !matches!(item, Item::User { .. } | Item::TurnEnd { .. }))
                .find_map(|item| match item {
                    Item::Thinking { streaming: true } => Some(Self::Thinking),
                    Item::Text {
                        streaming: true, ..
                    } => Some(Self::Writing),
                    // A subagent's calls run inside its own call.
                    Item::Tool(tool) if tool.result.is_none() && tool.parent.is_none() => {
                        Some(Self::Running(name(tool)))
                    }
                    _ => None,
                })
                .unwrap_or(Self::Working),
        })
    }

    fn label(&self) -> String {
        match self {
            Self::Starting => "Starting…".into(),
            Self::Thinking => "Thinking…".into(),
            Self::Writing => "Writing…".into(),
            Self::Running(tool) => format!("Running {tool}…"),
            Self::Working => "Working…".into(),
            Self::Compacting => "Compacting the conversation…".into(),
            Self::NeedsApproval => "Waiting for your approval".into(),
            Self::NeedsAnswer => "Waiting for your answer".into(),
        }
    }

    /// The agent waits for the user: nothing moves until they act.
    fn waits(&self) -> bool {
        matches!(self, Self::NeedsApproval | Self::NeedsAnswer)
    }
}

impl ChatSession {
    /// Keep the activity line's clock running while the agent is busy; call
    /// after anything that may have changed that.
    pub(super) fn track_activity(&mut self, cx: &mut Context<Self>) {
        let busy = self.convo.status.is_busy();
        if busy == self.busy_since.is_some() {
            return;
        }
        if !busy {
            self.busy_since = None;
            self.busy_clock = None;
            return;
        }
        self.busy_since = Some(Instant::now());
        self.busy_clock = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let busy = this.update(cx, |this, cx| {
                    cx.notify();
                    this.convo.status.is_busy()
                });
                if !busy.unwrap_or(false) {
                    break;
                }
            }
        }));
    }

    /// The line itself, level with the timeline's steps: a spinner where
    /// their marks are, what the agent is doing, for how long.
    pub(super) fn render_activity(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let activity = Activity::of(&self.convo, &|tool| self.tool_label(tool))?;
        let theme = cx.theme().clone();
        let waits = activity.waits();
        let mut facts = Vec::new();
        if !waits && let Some(since) = self.busy_since {
            facts.push(look::elapsed_text(since.elapsed()));
        }
        if activity == Activity::Thinking
            && let Some(tokens) = self.convo.thinking_tokens.filter(|tokens| *tokens >= 100)
        {
            facts.push(format!("{} tokens", look::count_text(tokens)));
        }
        let mark = if waits {
            div()
                .size(px(7.))
                .rounded_full()
                .bg(theme.warning)
                .into_any_element()
        } else {
            Spinner::new()
                .xsmall()
                .color(theme.muted_foreground)
                .into_any_element()
        };
        let label = if waits {
            div()
                .text_color(theme.warning)
                .child(activity.label())
                .into_any_element()
        } else {
            ShimmerText::new(activity.label())
                .text_color(theme.muted_foreground)
                .into_any_element()
        };
        Some(
            h_flex()
                .id("chat-activity")
                .flex_none()
                .px_3()
                .pb_2()
                .items_center()
                .text_size(px(STEP_TEXT))
                .child(
                    h_flex()
                        .flex_none()
                        .w(px(GUTTER))
                        .justify_center()
                        .child(mark),
                )
                .child(div().min_w_0().truncate().child(label))
                .when(!facts.is_empty(), |line| {
                    line.child(
                        div()
                            .flex_none()
                            .pl_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .opacity(0.8)
                            .child(facts.join(" · ")),
                    )
                })
                // Waiting for the user: the line leads to what they must do.
                .when(waits, |line| {
                    line.cursor_pointer().on_click(
                        cx.listener(|this, _, window, cx| this.focus_attention(window, cx)),
                    )
                })
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's own `test` attribute would shadow `#[test]`.
    use super::Activity;
    use serde_json::Value;
    use sg_agents::chat::{Conversation, Item, Status, ToolCall};

    fn activity(convo: &Conversation) -> Option<Activity> {
        Activity::of(convo, &|tool| tool.name.clone())
    }

    fn call(id: &str, parent: Option<&str>) -> Item {
        Item::Tool(ToolCall {
            id: id.into(),
            name: if parent.is_some() { "Read" } else { "Bash" }.into(),
            input: Value::Null,
            result: None,
            parent: parent.map(String::from),
            checking: false,
        })
    }

    #[test]
    fn says_what_the_agent_is_doing() {
        let mut convo = Conversation::new();
        assert_eq!(activity(&convo), None);
        convo.push_user("go");
        assert_eq!(activity(&convo), Some(Activity::Starting));
        convo.status = Status::Working;
        assert_eq!(activity(&convo), Some(Activity::Working));
        convo.start_thinking();
        assert_eq!(activity(&convo), Some(Activity::Thinking));
        convo.complete_thinking();
        convo.items.push(call("t1", None));
        assert_eq!(activity(&convo), Some(Activity::Running("Bash".into())));
        // A subagent's call runs inside the agent's own.
        convo.items.push(call("t2", Some("t1")));
        assert_eq!(activity(&convo), Some(Activity::Running("Bash".into())));
        convo.start_text();
        assert_eq!(activity(&convo), Some(Activity::Writing));
        convo.compacting = true;
        assert_eq!(activity(&convo), Some(Activity::Compacting));
        convo.status = Status::NeedsPermission;
        assert_eq!(activity(&convo), Some(Activity::NeedsApproval));
        convo.end_turn(None, false, None);
        assert_eq!(activity(&convo), None);
    }

    #[test]
    fn a_call_of_an_earlier_turn_is_not_running() {
        let mut convo = Conversation::new();
        convo.push_user("go");
        convo.items.push(call("t1", None));
        convo.interrupt_turn();
        convo.push_user("again");
        convo.status = Status::Working;
        assert_eq!(activity(&convo), Some(Activity::Working));
    }
}
