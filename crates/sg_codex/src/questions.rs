//! Conversion between Codex's interactive questions and the shared chat model.

use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;
use serde_json::{Value, json};
use sg_agents::chat::{InputResolution, QuestionOption, UserInputRequest, UserQuestion};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Params {
    questions: Vec<Question>,
    is_blocking: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Question {
    id: String,
    header: String,
    question: String,
    options: Option<Vec<OptionItem>>,
    #[serde(default)]
    is_other: bool,
    #[serde(default)]
    is_secret: bool,
}

#[derive(Deserialize)]
struct OptionItem {
    label: String,
    description: String,
}

pub(crate) fn request(request_id: &str, value: &Value) -> Option<UserInputRequest> {
    let params = Params::deserialize(value).ok()?;
    let mut ids = HashSet::new();
    if params.questions.is_empty()
        || params.questions.iter().any(|question| {
            question.id.is_empty()
                || question.question.trim().is_empty()
                || !ids.insert(&question.id)
        })
    {
        return None;
    }
    Some(UserInputRequest {
        request_id: request_id.to_string(),
        blocking: params.is_blocking.unwrap_or(true),
        resolution: InputResolution::Pending,
        questions: params
            .questions
            .into_iter()
            .map(|question| UserQuestion {
                id: question.id,
                header: question.header,
                prompt: question.question,
                allow_other: question.is_other,
                secret: question.is_secret,
                options: question
                    .options
                    .unwrap_or_default()
                    .into_iter()
                    .map(|option| QuestionOption {
                        label: option.label,
                        description: option.description,
                    })
                    .collect(),
            })
            .collect(),
    })
}

pub(crate) fn response(answers: &BTreeMap<String, String>) -> Value {
    let answers: serde_json::Map<_, _> = answers
        .iter()
        .map(|(id, answer)| (id.clone(), json!({"answers": [answer]})))
        .collect();
    json!({"answers": answers})
}

#[cfg(test)]
mod tests {
    use super::{request, response};
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn maps_metadata_for_choices_and_free_text() {
        let input = request("42", &json!({"isBlocking": false, "questions": [
            {"id":"base","header":"Base","question":"Which branch?","isOther":true,
             "options":[{"label":"main","description":"Default branch"}]},
            {"id":"code","header":"Code","question":"Enter the code", "isSecret":true,"options":null}
        ]})).unwrap();
        assert!(!input.blocking);
        assert!(input.questions[0].allow_other);
        assert_eq!(input.questions[0].options[0].label, "main");
        assert!(input.questions[1].secret && input.questions[1].options.is_empty());
        assert_eq!(
            response(&BTreeMap::from([("base".to_string(), "main".to_string())])),
            json!({"answers":{"base":{"answers":["main"]}}})
        );
    }
}
