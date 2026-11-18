use std::collections::BTreeMap;

use gpui_kit::base::Selectable as _;
use gpui_kit::component::input::{Input, InputState};
use sg_agents::chat::{InputResolution, UserInputRequest, UserQuestion, strip_ansi};

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Choice {
    Option(usize),
    Other,
}

pub(super) struct AnswerDraft {
    choice: Option<Choice>,
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
            choice: open_ended.then_some(Choice::Other),
            input,
            _subscription: subscription,
        }
    }

    fn is_answered(&self, cx: &App) -> bool {
        match self.choice {
            Some(Choice::Option(_)) => true,
            Some(Choice::Other) => self
                .input
                .as_ref()
                .is_some_and(|input| !input.read(cx).value().trim().is_empty()),
            None => false,
        }
    }

    fn value(&self, question: &UserQuestion, cx: &App) -> Option<String> {
        match self.choice? {
            Choice::Option(option) => Some(question.options.get(option)?.label.clone()),
            Choice::Other => {
                let value = self.input.as_ref()?.read(cx).value();
                (!value.trim().is_empty()).then(|| value.to_string())
            }
        }
    }
}

fn answers(
    request: &UserInputRequest,
    drafts: &[AnswerDraft],
    cx: &App,
) -> Option<BTreeMap<String, String>> {
    if request.questions.len() != drafts.len() {
        return None;
    }
    request
        .questions
        .iter()
        .zip(drafts)
        .map(|(question, draft)| Some((question.id.clone(), draft.value(question, cx)?)))
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
        let Some(draft) = self
            .question_forms
            .get_mut(id)
            .and_then(|drafts| drafts.get_mut(index))
        else {
            return;
        };
        draft.choice = Some(choice);
        if choice == Choice::Other
            && let Some(input) = &draft.input
        {
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        self.items_changed(self.convo.items.len(), cx);
    }

    fn answer_question_request(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(request) = self
            .convo
            .user_input(id)
            .filter(|request| request.resolution == InputResolution::Pending)
        else {
            return;
        };
        let Some(values) = self
            .question_forms
            .get(id)
            .and_then(|draft| answers(request, draft, cx))
        else {
            return;
        };
        let Backend::Codex {
            process: Some(process),
            ..
        } = &self.backend
        else {
            return;
        };
        let before = self.convo.items.len();
        match process.respond_to_questions(id, &values) {
            Ok(()) => {
                self.convo.resolve_user_input(id, InputResolution::Answered);
                self.sync_pending_attention();
            }
            Err(error) => self
                .convo
                .push_notice(format!("Couldn't submit answers: {error:#}"), true),
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
        let header = match request.resolution {
            InputResolution::Pending if request.questions.len() == 1 => "Codex has a question",
            InputResolution::Pending => "Codex has questions",
            InputResolution::Answered => "Answers sent",
            InputResolution::Expired => "Questions closed",
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
                    div()
                        .text_color(theme.muted_foreground)
                        .child(strip_ansi(&question.prompt))
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
            card = card.child(render_question(
                &request.request_id,
                index,
                question,
                draft,
                &theme,
                cx,
            ));
        }
        let ready = drafts.iter().all(|draft| draft.is_answered(cx));
        let id = request.request_id.clone();
        card.child(
            h_flex().justify_end().child(
                Button::new("submit-codex-answers")
                    .label("Submit answers")
                    .small()
                    .primary()
                    .disabled(!ready)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.answer_question_request(&id, cx)),
                    ),
            ),
        )
        .into_any_element()
    }
}

fn render_question(
    request_id: &str,
    index: usize,
    question: &UserQuestion,
    draft: &AnswerDraft,
    theme: &Theme,
    cx: &mut Context<ChatSession>,
) -> AnyElement {
    let mut panel = v_flex()
        .gap_1()
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
    for (choice, option) in question.options.iter().enumerate() {
        let id = request_id.to_string();
        let selected = draft.choice == Some(Choice::Option(choice));
        let button = Button::new(SharedString::from(format!("codex-answer-{index}-{choice}")))
            .label(strip_ansi(&option.label))
            .small()
            .outline()
            .selected(selected)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.choose_question_answer(&id, index, Choice::Option(choice), window, cx);
            }));
        panel = panel.child(v_flex().gap_0p5().items_start().child(button).when(
            !option.description.is_empty(),
            |row| {
                row.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(strip_ansi(&option.description)),
                )
            },
        ));
    }
    if question.allow_other && !question.options.is_empty() {
        let id = request_id.to_string();
        panel = panel.child(
            h_flex().child(
                Button::new(("codex-answer-other", index))
                    .label("Other…")
                    .small()
                    .ghost()
                    .selected(draft.choice == Some(Choice::Other))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.choose_question_answer(&id, index, Choice::Other, window, cx);
                    })),
            ),
        );
    }
    if draft.choice == Some(Choice::Other)
        && let Some(input) = &draft.input
    {
        panel = panel.child(Input::new(input).small());
    }
    panel.into_any_element()
}
