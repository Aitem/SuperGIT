//! The native Claude chat: one `ChatSession` per conversation, shown in the
//! right-hand panel. Drives `claude` through `sg_claude` (stream-json), renders
//! messages, tool calls with inline diffs, and permission prompts as cards.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffFile, DiffState};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use sg_claude::conversation::Reply;
use sg_claude::{
    ClaudeProcess, Conversation, Decision, Item, PermissionMode, PermissionRequest, ProcessEvent,
    SpawnOptions, Status, ToolCall, tools,
};

/// Max lines of tool output shown when a tool row is expanded.
const OUTPUT_LINES: usize = 40;

pub struct ChatSession {
    /// Where `claude` runs (a resumed session must run where it started).
    cwd: PathBuf,
    local_id: String,
    transcript: Option<PathBuf>,
    convo: Conversation,
    process: Option<Arc<ClaudeProcess>>,
    mode: PermissionMode,
    input: Entity<TextareaState>,
    scroller: Entity<MessageScrollerState>,
    diffs: HashMap<String, (Entity<DiffState>, usize)>,
    expanded: HashSet<usize>,
    stderr_tail: Vec<String>,
    reader: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ChatSession {
    /// A fresh conversation in `worktree`.
    pub fn new(worktree: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(worktree.clone(), worktree, Conversation::new(), window, cx)
    }

    /// Continue an existing Claude Code session, showing its history.
    pub fn resume(
        worktree: PathBuf,
        session: &sg_agents::Session,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut convo = Conversation::from_transcript(&session.transcript).unwrap_or_default();
        convo.session_id = Some(session.id.clone());
        if convo.title.is_none() {
            convo.title = Some(session.title.clone());
        }
        let cwd = session
            .start_cwd
            .clone()
            .unwrap_or_else(|| worktree.clone());
        let mut chat = Self::build(worktree, cwd, convo, window, cx);
        chat.transcript = Some(session.transcript.clone());
        chat
    }

    /// Restore visible history and an unsent draft, without starting a process.
    pub fn restore(
        worktree: PathBuf,
        saved: &crate::workspace::SavedConversation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let history = saved
            .transcript
            .as_ref()
            .and_then(|path| Conversation::from_transcript(path).ok());
        let mut convo = history.unwrap_or_default();
        if saved.transcript.as_ref().is_some_and(|p| p.is_file()) {
            convo.session_id = saved.session_id.clone();
        }
        if convo.title.is_none() && !saved.title.is_empty() && saved.title != "New conversation" {
            convo.title = Some(saved.title.clone());
        }
        let cwd = if saved.cwd.is_dir() {
            saved.cwd.clone()
        } else {
            worktree.clone()
        };
        let mut chat = Self::build(worktree, cwd, convo, window, cx);
        if !saved.local_id.is_empty() {
            chat.local_id = saved.local_id.clone();
        }
        chat.transcript = saved.transcript.clone().filter(|p| p.is_file());
        chat.input.update(cx, |input, cx| {
            input.set_value(saved.draft.clone(), window, cx)
        });
        chat
    }

    pub fn snapshot(&self, cx: &App) -> crate::workspace::SavedConversation {
        crate::workspace::SavedConversation {
            local_id: self.local_id.clone(),
            provider: "claude".into(),
            session_id: self.convo.session_id.clone(),
            transcript: self.transcript.clone().or_else(|| {
                Some(
                    sg_agents::project_dir(&self.cwd)?
                        .join(format!("{}.jsonl", self.convo.session_id.as_ref()?)),
                )
            }),
            cwd: self.cwd.clone(),
            title: self.title(),
            draft: self.input.read(cx).value().to_string(),
        }
    }

    fn build(
        worktree: PathBuf,
        cwd: PathBuf,
        convo: Conversation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 12)
                .submit_on_enter(true)
                .placeholder("Ask Claude…  (Enter to send, Shift+Enter for a new line)")
        });
        let sub = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                this.send(window, cx);
            } else if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let count = convo.items.len();
        let scroller = cx.new(|cx| MessageScrollerState::new(count, cx));
        let mode = convo
            .permission_mode
            .as_deref()
            .and_then(PermissionMode::from_wire)
            .unwrap_or_default();
        let _ = worktree;
        Self {
            cwd,
            local_id: format!(
                "draft-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            transcript: None,
            convo,
            process: None,
            mode,
            input,
            scroller,
            diffs: HashMap::new(),
            expanded: HashSet::new(),
            stderr_tail: Vec::new(),
            reader: None,
            _subscriptions: vec![sub],
        }
    }

    pub fn status(&self) -> Status {
        self.convo.status
    }

    pub fn session_id(&self) -> Option<&str> {
        self.convo.session_id.as_deref()
    }

    pub fn title(&self) -> String {
        self.convo
            .title
            .clone()
            .unwrap_or_else(|| "New conversation".into())
    }

    pub fn input(&self) -> Entity<TextareaState> {
        self.input.clone()
    }

    pub fn is_empty(&self) -> bool {
        self.convo.items.is_empty()
    }

    // ---- process ---------------------------------------------------------

    fn ensure_process(&mut self, cx: &mut Context<Self>) -> anyhow::Result<Arc<ClaudeProcess>> {
        if let Some(p) = &self.process
            && self.convo.status != Status::Exited
        {
            return Ok(p.clone());
        }
        let opts = SpawnOptions {
            cwd: self.cwd.clone(),
            resume: self.convo.session_id.clone(),
            permission_mode: self.mode,
            ..Default::default()
        };
        let (process, mut events) = ClaudeProcess::spawn(&opts)?;
        let process = Arc::new(process);
        self.process = Some(process.clone());
        self.stderr_tail.clear();
        self.reader = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                if this
                    .update(cx, |this, cx| this.on_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
        Ok(process)
    }

    fn on_event(&mut self, event: ProcessEvent, cx: &mut Context<Self>) {
        let before = self.convo.items.len();
        match event {
            ProcessEvent::Message(v) => {
                if let Some(Reply::Unsupported {
                    request_id,
                    subtype,
                }) = self.convo.apply(&v)
                    && let Some(p) = &self.process
                {
                    let _ = p
                        .control_error(&request_id, &format!("SuperGIT does not handle {subtype}"));
                }
            }
            ProcessEvent::Stderr(line) => {
                self.stderr_tail.push(line);
                if self.stderr_tail.len() > 20 {
                    self.stderr_tail.remove(0);
                }
                return;
            }
            ProcessEvent::Exited => {
                self.convo.on_exit(self.stderr_tail.last().cloned());
                self.process = None;
            }
        }
        self.items_changed(before, cx);
    }

    fn items_changed(&mut self, before: usize, cx: &mut Context<Self>) {
        let after = self.convo.items.len();
        self.scroller.update(cx, |s, cx| {
            if after > before {
                s.append(after - before, cx);
            }
            // Streaming text grows, tool results attach to earlier rows.
            s.remeasure(cx);
        });
        cx.notify();
    }

    // ---- user actions ----------------------------------------------------

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.send_text(&text, cx);
    }

    /// Send a turn (also used by automation in screenshot builds).
    pub fn send_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let text = text.to_string();
        let before = self.convo.items.len();
        self.convo.push_user(&text);
        if let Err(e) = self.ensure_process(cx).and_then(|p| p.send_user(&text)) {
            self.convo.items.push(Item::Notice {
                text: format!("{e:#}"),
                is_error: true,
            });
            self.convo.status = Status::Idle;
        }
        self.items_changed(before, cx);
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = &self.process {
            let _ = p.interrupt();
        }
        cx.notify();
    }

    fn set_mode(&mut self, mode: PermissionMode, cx: &mut Context<Self>) {
        self.mode = mode;
        if let Some(p) = &self.process {
            let _ = p.set_permission_mode(mode);
        }
        cx.notify();
    }

    fn decide(&mut self, request_id: String, decision: Decision, cx: &mut Context<Self>) {
        let Some(request) = self.convo.items.iter().find_map(|i| match i {
            Item::Permission(p) if p.request_id == request_id => Some(p.clone()),
            _ => None,
        }) else {
            return;
        };
        let Some(process) = self.process.clone() else {
            return;
        };
        let sent = match decision {
            Decision::Allowed => process.allow(&request_id, &request.input, None),
            Decision::AllowedForSession => {
                if request
                    .suggestions
                    .iter()
                    .any(|s| s.get("mode").and_then(|m| m.as_str()) == Some("acceptEdits"))
                {
                    self.mode = PermissionMode::AcceptEdits;
                }
                process.allow(
                    &request_id,
                    &request.input,
                    Some(request.suggestions.clone()),
                )
            }
            Decision::Denied => process.deny(&request_id, "The user denied this action."),
        };
        if sent.is_ok() {
            self.convo.resolve_permission(&request_id, decision);
        }
        let len = self.convo.items.len();
        self.items_changed(len, cx);
    }

    fn toggle_expanded(&mut self, ix: usize, cx: &mut Context<Self>) {
        if !self.expanded.remove(&ix) {
            self.expanded.insert(ix);
        }
        let len = self.convo.items.len();
        self.items_changed(len, cx);
    }

    /// A cached diff view for an edit (by tool id / permission id).
    fn diff_for(
        &mut self,
        key: &str,
        patch: &str,
        cx: &mut Context<Self>,
    ) -> Option<(Entity<DiffState>, usize)> {
        if let Some(d) = self.diffs.get(key) {
            return Some(d.clone());
        }
        let files = DiffFile::parse(patch).ok().filter(|f| !f.is_empty())?;
        let lines = patch
            .lines()
            .filter(|l| {
                l.starts_with(['+', '-', ' ']) && !l.starts_with("+++") && !l.starts_with("---")
            })
            .count();
        let state = cx.new(|cx| DiffState::new(files, cx));
        self.diffs.insert(key.to_string(), (state.clone(), lines));
        Some((state, lines))
    }

    // ---- rendering -------------------------------------------------------

    fn render_item(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(item) = self.convo.items.get(ix).cloned() else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let body = match item {
            Item::User { text } => h_flex()
                .justify_end()
                .child(
                    div()
                        .max_w(relative(0.88))
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(theme.secondary)
                        .text_color(theme.secondary_foreground)
                        .child(text),
                )
                .into_any_element(),
            Item::Text { text, streaming } => div()
                .text_sm()
                .child(TextView::markdown(("md", ix), text))
                .when(streaming, |el| {
                    el.child(div().text_color(theme.muted_foreground).child("▍"))
                })
                .into_any_element(),
            // Finished thinking is noise in the transcript; show it only live.
            Item::Thinking { streaming: false } => return div().into_any_element(),
            Item::Thinking { streaming: true } => div()
                .text_xs()
                .italic()
                .text_color(theme.muted_foreground)
                .child("Thinking…")
                .into_any_element(),
            Item::Tool(tool) => self.render_tool(ix, &tool, &theme, cx).into_any_element(),
            Item::Permission(p) => self.render_permission(&p, &theme, cx).into_any_element(),
            Item::Notice { text, is_error } => div()
                .text_xs()
                .text_color(if is_error {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .child(text)
                .into_any_element(),
            Item::TurnEnd {
                duration_ms,
                is_error,
                message,
            } => {
                let secs = duration_ms.map(|ms| format!("{:.1}s", ms as f64 / 1000.0));
                let text = match (is_error, message) {
                    (true, Some(m)) => format!("✗ {m}"),
                    (true, None) => "✗ failed".into(),
                    _ => format!(
                        "✓ done{}",
                        secs.map(|s| format!(" · {s}")).unwrap_or_default()
                    ),
                };
                div()
                    .text_xs()
                    .text_color(if is_error {
                        theme.danger
                    } else {
                        theme.muted_foreground
                    })
                    .child(text)
                    .into_any_element()
            }
        };
        div().px_4().py_1p5().child(body).into_any_element()
    }

    fn render_tool(
        &mut self,
        ix: usize,
        tool: &ToolCall,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let summary = tools::summary(&tool.name, &tool.input, Some(&self.cwd));
        let (dot, label_color) = match &tool.result {
            None => (theme.blue, theme.foreground),
            Some(r) if r.is_error => (theme.danger, theme.danger),
            Some(_) => (theme.success, theme.foreground),
        };
        let expanded = self.expanded.contains(&ix);
        // While the edit awaits approval the permission card shows the diff.
        let diff =
            if tools::is_edit(&tool.name) && tool.result.as_ref().is_some_and(|r| !r.is_error) {
                tools::edit_patch(
                    &tool.name,
                    &tool.input,
                    tool.result.as_ref().and_then(|r| r.meta.as_ref()),
                    Some(&self.cwd),
                )
                .and_then(|patch| self.diff_for(&tool.id, &patch, cx))
            } else {
                None
            };
        let output = tool
            .result
            .as_ref()
            .filter(|_| expanded && diff.is_none())
            .map(|r| {
                let mut lines: Vec<&str> = r.text.lines().take(OUTPUT_LINES).collect();
                if r.text.lines().count() > OUTPUT_LINES {
                    lines.push("…");
                }
                lines.join("\n")
            });

        v_flex()
            .gap_1()
            .when(tool.parent.is_some(), |el| el.pl_4())
            .child(
                h_flex()
                    .id(("tool", ix))
                    .gap_2()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_expanded(ix, cx)))
                    .child(div().size(px(7.)).flex_none().rounded_full().bg(dot))
                    .child(
                        div()
                            .flex_none()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(label_color)
                            .child(tool.name.clone()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(summary),
                    ),
            )
            .when_some(diff, |el, (state, lines)| {
                el.child(diff_box(&state, lines, theme))
            })
            .when_some(output, |el, text| {
                el.child(
                    div()
                        .ml_4()
                        .p_2()
                        .rounded_md()
                        .bg(theme.muted)
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(text),
                )
            })
    }

    fn render_permission(
        &mut self,
        p: &PermissionRequest,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pending = p.decision.is_none();
        let preview: Option<AnyElement> = if tools::is_edit(&p.tool_name) {
            tools::edit_patch(&p.tool_name, &p.input, None, Some(&self.cwd))
                .and_then(|patch| self.diff_for(&format!("perm:{}", p.request_id), &patch, cx))
                .map(|(state, lines)| diff_box(&state, lines, theme).into_any_element())
        } else {
            let text = match p.tool_name.as_str() {
                "Bash" => p
                    .input
                    .get("command")
                    .and_then(|c| c.as_str())
                    .map(String::from),
                _ => serde_json::to_string_pretty(&p.input).ok(),
            };
            text.map(|t| {
                let t: String = t.lines().take(20).collect::<Vec<_>>().join("\n");
                div()
                    .p_2()
                    .rounded_md()
                    .bg(theme.muted)
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .child(t)
                    .into_any_element()
            })
        };
        let rid = p.request_id.clone();
        let rid2 = p.request_id.clone();
        let rid3 = p.request_id.clone();
        let session_label = p.session_allow_label();

        v_flex()
            .p_3()
            .gap_2()
            .rounded_lg()
            .border_1()
            .border_color(if pending { theme.warning } else { theme.border })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("Allow {}?", p.display_name)),
                    )
                    .when_some(p.description.clone(), |el, d| {
                        el.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(d),
                        )
                    }),
            )
            .children(preview)
            .child(if pending {
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new(("allow", rid.len()))
                            .label("Allow")
                            .small()
                            .primary()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.decide(rid.clone(), Decision::Allowed, cx)
                            })),
                    )
                    .when_some(session_label, |el, label| {
                        el.child(Button::new("allow-session").label(label).small().on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.decide(rid2.clone(), Decision::AllowedForSession, cx)
                            }),
                        ))
                    })
                    .child(
                        Button::new("deny")
                            .label("Deny")
                            .small()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.decide(rid3.clone(), Decision::Denied, cx)
                            })),
                    )
                    .into_any_element()
            } else {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(match p.decision {
                        Some(Decision::Denied) => "Denied",
                        Some(Decision::AllowedForSession) => "Allowed for this session",
                        _ => "Allowed",
                    })
                    .into_any_element()
            })
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let busy = self.convo.status.is_busy();
        let modes = PermissionMode::ALL
            .into_iter()
            .enumerate()
            .map(|(i, mode)| {
                let active = mode == self.mode;
                Button::new(("mode", i))
                    .label(mode.label())
                    .xsmall()
                    .when(active, |b| b.secondary())
                    .when(!active, |b| b.ghost())
                    .tooltip(match mode {
                        PermissionMode::Default => "Ask before edits and commands",
                        PermissionMode::AcceptEdits => "Auto-accept file edits",
                        PermissionMode::Plan => "Plan only, no changes",
                        PermissionMode::BypassPermissions => "Never ask (careful)",
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.set_mode(mode, cx)))
            });
        v_flex()
            .p_2()
            .gap_2()
            .border_t_1()
            .border_color(theme.border)
            .child(Textarea::new(&self.input))
            .child(
                h_flex()
                    .justify_between()
                    .child(h_flex().gap_0p5().children(modes))
                    .child(if busy {
                        Button::new("stop")
                            .label("Stop")
                            .small()
                            .danger()
                            .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                    } else {
                        Button::new("send")
                            .label("Send")
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
                    }),
            )
    }
}

impl Render for ChatSession {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let status = match self.convo.status {
            Status::Idle => None,
            Status::Starting => Some(("starting…", theme.muted_foreground)),
            Status::Working => Some(("working…", theme.blue)),
            Status::NeedsPermission => Some(("needs your approval", theme.warning)),
            Status::Exited => Some((
                "stopped — your next message resumes",
                theme.muted_foreground,
            )),
        };
        let info = [
            self.convo.model.clone(),
            self.convo.total_cost_usd.map(|c| format!("${c:.2}")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_4()
                    .py_1()
                    .gap_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(div().flex_1().min_w_0().truncate().child(info))
                    .when_some(status, |el, (label, color)| {
                        el.child(div().text_color(color).child(label))
                    }),
            )
            .child(if self.convo.items.is_empty() {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .px_6()
                    .text_color(theme.muted_foreground)
                    .child(div().text_lg().child("✦ Claude"))
                    .child(
                        div()
                            .text_sm()
                            .text_center()
                            .child(format!("Working in {}", self.cwd.display())),
                    )
                    .into_any_element()
            } else {
                div()
                    .flex_1()
                    .min_h_0()
                    .child(MessageScroller::new(
                        "chat-messages",
                        self.scroller.clone(),
                        cx.processor(|this, ix: usize, window, cx| {
                            this.render_item(ix, window, cx)
                        }),
                    ))
                    .into_any_element()
            })
            .child(self.render_composer(cx))
    }
}

fn diff_box(state: &Entity<DiffState>, lines: usize, theme: &Theme) -> impl IntoElement {
    // The diff view is virtualized and needs a definite height.
    let height = (lines.min(14) as f32) * 21. + 68.;
    div()
        .h(px(height))
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .child(Diff::new(state).line_number(true))
}
