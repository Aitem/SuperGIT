//! Question forms an agent puts in the conversation: Codex's
//! `requestUserInput` and Claude's `AskUserQuestion`. One question takes one
//! option (radios) or any number of them (checkboxes), plus free text under
//! "Other…" where the agent allows it; a question without options is a text
//! box.

use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::radio::Radio;
use sg_agents::chat::{Answers, InputResolution, UserInputRequest, UserQuestion, strip_ansi};

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Choice {
    Option(usize),
    Other,
}

/// What the user picked for one question so far.
pub(super) struct AnswerDraft {
    /// Picked options, in the order they were picked.
    picked: Vec<usize>,
    /// "Other" is picked: the typed text is (part of) the answer. Always
    /// on for a question without options.
    other: bool,
    input: Option<Entity<InputState>>,
    _subscription: Option<Subscription>,
}

impl AnswerDraft {
    fn new(question: &UserQuestion, window: &mut Window, cx: &mut Context<ChatSession>) -> Self {
        let open_ended = question.options.is_empty();
        let input = (open_ended || question.allow_other).then(|| {
            cx.new(|cx| {
                let mut input = InputState::new(window, cx).placeholder("Type your answer");
                if question.secret {
                    input.set_masked(true, window, cx);
                }
                input
            })
        });
        let subscription = input.as_ref().map(|input| {
            cx.subscribe(input, |this, _, _: &InputEvent, cx| {
                this.items_changed(this.convo.items.len(), cx);
            })
        });
        Self {
            picked: Vec::new(),
            other: open_ended,
            input,
            _subscription: subscription,
        }
    }

    fn pick(&mut self, question: &UserQuestion, choice: Choice) {
        match (question.multi_select, choice) {
            (true, Choice::Option(option)) => {
                if let Some(at) = self.picked.iter().position(|&p| p == option) {
                    self.picked.remove(at);
                } else {
                    self.picked.push(option);
                }
            }
            (true, Choice::Other) => self.other = !self.other || question.options.is_empty(),
            (false, Choice::Option(option)) => {
                self.picked = vec![option];
                self.other = false;
            }
            (false, Choice::Other) => {
                self.picked.clear();
                self.other = true;
            }
        }
    }

    fn picked(&self, choice: Choice) -> bool {
        match choice {
            Choice::Option(option) => self.picked.contains(&option),
            Choice::Other => self.other,
        }
    }

    /// The answer, or `None` while it is incomplete (nothing picked, or
    /// "Other" picked with nothing typed).
    fn values(&self, question: &UserQuestion, cx: &App) -> Option<Vec<String>> {
        let mut values: Vec<String> = self
            .picked
            .iter()
            .filter_map(|&option| question.options.get(option))
            .map(|option| option.label.clone())
            .collect();
        if self.other {
            let typed = self.input.as_ref()?.read(cx).value();
            let typed = typed.trim();
            if typed.is_empty() {
                return None;
            }
            values.push(typed.to_string());
        }
        (!values.is_empty()).then_some(values)
    }
}

fn answers(request: &UserInputRequest, drafts: &[AnswerDraft], cx: &App) -> Option<Answers> {
    if request.questions.len() != drafts.len() {
        return None;
    }
    request
        .questions
        .iter()
        .zip(drafts)
        .map(|(question, draft)| Some((question.id.clone(), draft.values(question, cx)?)))
        .collect()
}

impl ChatSession {
    fn choose_question_answer(
        &mut self,
        id: &str,
        index: usize,
        choice: Choice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(question) = self
            .convo
            .user_input(id)
            .and_then(|request| request.questions.get(index))
            .cloned()
        else {
            return;
        };
        let Some(draft) = self
            .question_forms
            .get_mut(id)
            .and_then(|drafts| drafts.get_mut(index))
        else {
            return;
        };
        draft.pick(&question, choice);
        if choice == Choice::Other
            && draft.other
            && let Some(input) = &draft.input
        {
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        self.items_changed(self.convo.items.len(), cx);
    }

    fn pending_question(&self, id: &str) -> Option<UserInputRequest> {
        self.convo
            .user_input(id)
            .filter(|request| request.resolution == InputResolution::Pending)
            .cloned()
    }

    fn answer_question_request(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(request) = self.pending_question(id) else {
            return;
        };
        let Some(values) = self
            .question_forms
            .get(id)
            .and_then(|drafts| answers(&request, drafts, cx))
        else {
            return;
        };
        let Some(process) = self.backend.agent() else {
            return;
        };
        let before = self.convo.items.len();
        match process.answer_questions(&request, &values) {
            Ok(()) => {
                self.convo.record_answers(id, values);
                self.sync_pending_attention();
            }
            Err(error) => self
                .convo
                .push_notice(format!("Couldn't submit answers: {error:#}"), true),
        }
        self.items_changed(before, cx);
    }

    fn decline_question_request(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(request) = self.pending_question(id) else {
            return;
        };
        let Some(process) = self.backend.agent() else {
            return;
        };
        let before = self.convo.items.len();
        match process.decline_questions(&request) {
            Ok(()) => {
                self.convo.resolve_user_input(id, InputResolution::Declined);
                self.sync_pending_attention();
            }
            Err(error) => self.convo.push_notice(format!("{error:#}"), true),
        }
        self.items_changed(before, cx);
    }

    pub(super) fn render_user_input(
        &mut self,
        request: &UserInputRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let agent = self.provider().label();
        let header = match request.resolution {
            InputResolution::Pending if request.questions.len() == 1 => {
                format!("{agent} has a question")
            }
            InputResolution::Pending => format!("{agent} has questions"),
            InputResolution::Answered => "Answers sent".into(),
            InputResolution::Declined => "Declined to answer".into(),
            InputResolution::Expired => "Questions closed".into(),
        };
        let mut card = v_flex()
            .gap_2()
            .p_3()
            .min_w_0()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .text_sm()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(header));
        if request.resolution != InputResolution::Pending {
            return card
                .children(request.questions.iter().map(|question| {
                    let answer = request
                        .answers
                        .get(&question.id)
                        .filter(|values| !values.is_empty())
                        .map(|values| strip_ansi(&values.join(", ")));
                    v_flex()
                        .min_w_0()
                        .child(
                            div()
                                .text_color(theme.muted_foreground)
                                .child(strip_ansi(&question.prompt)),
                        )
                        .when_some(answer, |row, answer| {
                            row.child(div().child(format!("→ {answer}")))
                        })
                }))
                .into_any_element();
        }
        if self.convo.pending_permission().is_none()
            && self
                .convo
                .pending_user_input()
                .is_some_and(|current| current.request_id == request.request_id)
        {
            card = card.track_focus(&self.permission_focus);
        }
        let drafts = self
            .question_forms
            .entry(request.request_id.clone())
            .or_insert_with(|| {
                request
                    .questions
                    .iter()
                    .map(|question| AnswerDraft::new(question, window, cx))
                    .collect()
            });
        for (index, (question, draft)) in request.questions.iter().zip(drafts.iter()).enumerate() {
            card = card.child(
                QuestionView {
                    request_id: &request.request_id,
                    index,
                    question,
                    draft,
                    theme: &theme,
                }
                .render(cx),
            );
        }
        let ready = drafts
            .iter()
            .zip(&request.questions)
            .all(|(draft, question)| draft.values(question, cx).is_some());
        let submit_id = request.request_id.clone();
        let decline_id = request.request_id.clone();
        card.child(
            h_flex()
                .justify_end()
                .gap_2()
                .when(self.provider() == Provider::Claude, |row| {
                    row.child(
                        Button::new("decline-answers")
                            .label("Decline")
                            .small()
                            .ghost()
                            .tooltip(format!("Tell {agent} you won't answer"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.decline_question_request(&decline_id, cx)
                            })),
                    )
                })
                .child(
                    Button::new("submit-answers")
                        .label(if request.questions.len() == 1 {
                            "Submit answer"
                        } else {
                            "Submit answers"
                        })
                        .small()
                        .primary()
                        .disabled(!ready)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.answer_question_request(&submit_id, cx)
                        })),
                ),
        )
        .into_any_element()
    }
}

/// One question of a pending form, as drawn.
struct QuestionView<'a> {
    request_id: &'a str,
    index: usize,
    question: &'a UserQuestion,
    draft: &'a AnswerDraft,
    theme: &'a Theme,
}

impl QuestionView<'_> {
    fn render(&self, cx: &mut Context<ChatSession>) -> AnyElement {
        let (question, theme) = (self.question, self.theme);
        let mut panel = v_flex()
            .gap_1p5()
            .min_w_0()
            .pt_2()
            .border_t_1()
            .border_color(theme.border);
        if !question.header.trim().is_empty() {
            panel = panel.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(strip_ansi(&question.header)),
            );
        }
        panel = panel.child(div().child(strip_ansi(&question.prompt)));
        if question.multi_select {
            panel = panel.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Select all that apply"),
            );
        }
        for (option, item) in question.options.iter().enumerate() {
            panel = panel.child(self.choice_row(
                Choice::Option(option),
                strip_ansi(&item.label),
                strip_ansi(&item.description),
                cx,
            ));
        }
        if question.allow_other && !question.options.is_empty() {
            panel = panel.child(self.choice_row(Choice::Other, "Other…".into(), String::new(), cx));
        }
        if self.draft.other
            && let Some(input) = &self.draft.input
        {
            panel = panel.child(Input::new(input).small());
        }
        panel.into_any_element()
    }

    /// A radio or a checkbox with the option's description under its label.
    fn choice_row(
        &self,
        choice: Choice,
        label: String,
        description: String,
        cx: &mut Context<ChatSession>,
    ) -> AnyElement {
        let index = self.index;
        let key = SharedString::from(match choice {
            Choice::Option(option) => format!("question-{index}-{option}"),
            Choice::Other => format!("question-{index}-other"),
        });
        let id = self.request_id.to_string();
        let on_click = cx.listener(move |this, _: &bool, window, cx| {
            this.choose_question_answer(&id, index, choice, window, cx);
        });
        let checked = self.draft.picked(choice);
        let control = if self.question.multi_select {
            Checkbox::new(key)
                .label(label)
                .small()
                .checked(checked)
                .on_click(on_click)
                .into_any_element()
        } else {
            Radio::new(key)
                .label(label)
                .small()
                .checked(checked)
                .on_click(on_click)
                .into_any_element()
        };
        v_flex()
            .min_w_0()
            .gap_0p5()
            .child(control)
            .when(!description.trim().is_empty(), |row| {
                row.child(
                    div()
                        .pl_6()
                        .text_xs()
                        .text_color(self.theme.muted_foreground)
                        .child(description),
                )
            })
            .into_any_element()
    }
}
