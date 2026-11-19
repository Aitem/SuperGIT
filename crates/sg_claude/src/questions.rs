//! Claude's `AskUserQuestion` tool as a question form.
//!
//! The CLI asks permission for the tool with `can_use_tool`; its input holds
//! the questions (`question`, `header`, `options` of `{label, description}`,
//! `multiSelect`, optionally `kind: "text" | "number"` without options). The
//! host answers by allowing the tool with the same input plus `answers`:
//! question text → the chosen label or typed text, an array of them for a
//! multi-select question. The CLI adds no "Other" option itself: the form
//! offers free text for every question.

use std::collections::HashSet;

use serde_json::{Map, Value};
use sg_agents::chat::{Answers, InputResolution, QuestionOption, UserInputRequest, UserQuestion};

/// The tool whose permission prompt is a question form.
pub const TOOL: &str = "AskUserQuestion";

/// The form for an `AskUserQuestion` permission request, or `None` when the
/// input does not look like one (then it stays an ordinary prompt).
pub fn request(request_id: &str, input: &Value) -> Option<UserInputRequest> {
    let questions = input.get("questions")?.as_array()?;
    let mut seen = HashSet::new();
    let questions = questions
        .iter()
        .map(|question| {
            let text = |key: &str| question.get(key).and_then(Value::as_str);
            let prompt = text("question")?.trim();
            // Answers are keyed by the question text: it must be unique.
            if prompt.is_empty() || !seen.insert(prompt) {
                return None;
            }
            let options: Vec<_> = question
                .get("options")
                .and_then(Value::as_array)
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|option| {
                            let label = option.get("label")?.as_str()?.trim();
                            (!label.is_empty()).then(|| QuestionOption {
                                label: label.to_string(),
                                description: option
                                    .get("description")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(UserQuestion {
                // The original text, untrimmed: it is the key of the answer.
                id: text("question")?.to_string(),
                header: text("header").unwrap_or_default().to_string(),
                prompt: prompt.to_string(),
                multi_select: question.get("multiSelect").and_then(Value::as_bool) == Some(true)
                    && !options.is_empty(),
                allow_other: true,
                secret: false,
                options,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    if questions.is_empty() {
        return None;
    }
    Some(UserInputRequest {
        request_id: request_id.to_string(),
        questions,
        blocking: true,
        resolution: InputResolution::Pending,
        input: input.clone(),
        answers: Answers::new(),
    })
}

/// The tool input to allow the request with: the original input plus
/// `answers`, a string per question (an array for multi-select ones).
pub fn answered_input(request: &UserInputRequest, answers: &Answers) -> Value {
    let mut input = request.input.as_object().cloned().unwrap_or_default();
    let answers: Map<String, Value> = request
        .questions
        .iter()
        .filter_map(|question| {
            let values = answers.get(&question.id).filter(|v| !v.is_empty())?;
            let value = if question.multi_select {
                Value::from(values.clone())
            } else {
                Value::from(values.join(", "))
            };
            Some((question.id.clone(), value))
        })
        .collect();
    input.insert("answers".into(), Value::Object(answers));
    Value::Object(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input() -> Value {
        json!({
            "questions": [
                {
                    "question": "Кого указать в Co-Authored-By?",
                    "header": "Co-author",
                    "options": [
                        {"label": "Andrey Listopadov", "description": "автор $evaluate-measure"},
                        {"label": "Без соавтора-человека", "description": "Только трейлер Claude"}
                    ],
                    "multiSelect": false
                },
                {
                    "question": "Which checks should run?",
                    "header": "Checks",
                    "options": [{"label": "fmt", "description": ""}, {"label": "clippy", "description": ""}],
                    "multiSelect": true
                },
                {"question": "Anything else?", "header": "Notes", "kind": "text"}
            ],
            "metadata": {"source": "test"}
        })
    }

    #[test]
    fn reads_the_questions_with_free_text_for_every_one() {
        let form = request("r1", &input()).unwrap();
        assert_eq!(form.request_id, "r1");
        assert!(form.blocking);
        let [single, multi, text] = form.questions.as_slice() else {
            panic!("three questions");
        };
        assert_eq!(single.id, "Кого указать в Co-Authored-By?");
        assert_eq!(single.header, "Co-author");
        assert_eq!(single.options[1].label, "Без соавтора-человека");
        assert!(!single.multi_select && single.allow_other);
        assert!(multi.multi_select);
        assert!(text.options.is_empty() && !text.multi_select);
    }

    #[test]
    fn answers_echo_the_input_keyed_by_question_text() {
        let form = request("r1", &input()).unwrap();
        let answers = Answers::from([
            (
                "Кого указать в Co-Authored-By?".to_string(),
                vec!["Andrey Listopadov".to_string()],
            ),
            (
                "Which checks should run?".to_string(),
                vec!["fmt".to_string(), "only on CI".to_string()],
            ),
            ("Anything else?".to_string(), vec!["Нет".to_string()]),
        ]);
        let answered = answered_input(&form, &answers);
        assert_eq!(answered["questions"], input()["questions"]);
        assert_eq!(answered["metadata"], json!({"source": "test"}));
        assert_eq!(
            answered["answers"],
            json!({
                "Кого указать в Co-Authored-By?": "Andrey Listopadov",
                "Which checks should run?": ["fmt", "only on CI"],
                "Anything else?": "Нет"
            })
        );
    }

    #[test]
    fn malformed_input_stays_a_permission_prompt() {
        assert!(request("r", &json!({})).is_none());
        assert!(request("r", &json!({"questions": []})).is_none());
        assert!(request("r", &json!({"questions": [{"header": "x"}]})).is_none());
        let twice = json!({"questions": [{"question": "Same?"}, {"question": "Same?"}]});
        assert!(request("r", &twice).is_none());
    }
}
