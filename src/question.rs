//! A live question bound to a current Herdr terminal capture.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Question {
    pub question_id: String,
    pub question_kind: String,
    pub title: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_purpose: Option<String>,
    pub options: Vec<OptionItem>,
    pub multiple: bool,
    pub allow_custom: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct OptionItem {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Answer {
    Choices { option_ids: Vec<String> },
    Custom { text: String },
    Clarify,
}
impl Question {
    pub fn validate(&self, answer: &Answer) -> Result<()> {
        if !matches!(self.question_kind.as_str(), "question" | "permission") {
            bail!("unknown question kind");
        }
        match answer {
            Answer::Choices { option_ids } => {
                if option_ids.is_empty() || (!self.multiple && option_ids.len() != 1) {
                    bail!("invalid number of selected options");
                }
                let mut seen = std::collections::HashSet::new();
                for id in option_ids {
                    if !seen.insert(id) || !self.options.iter().any(|o| &o.id == id) {
                        bail!("invalid question option");
                    }
                }
            }
            Answer::Custom { text } => {
                if self.question_kind == "permission"
                    || !self.allow_custom
                    || text.trim().is_empty()
                    || text.len() > 16_000
                {
                    bail!("custom answer is unavailable or invalid");
                }
            }
            Answer::Clarify => {}
        }
        Ok(())
    }
    pub fn speech(&self) -> String {
        let context = if self.question_kind == "permission" {
            self.command_purpose.as_deref().unwrap_or(
                "The command's purpose is not visible. Check the terminal before approving.",
            )
        } else {
            ""
        };
        let options = self
            .options
            .iter()
            .enumerate()
            .map(|(i, o)| format!("Option {}: {}.", i + 1, o.label))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "{} {} {} {}{}",
            self.body,
            context,
            options,
            if self.multiple {
                "You can choose more than one. "
            } else {
                ""
            },
            if self.allow_custom {
                "You can also give your own answer."
            } else {
                ""
            }
        )
    }
}
pub(crate) fn mode_payload(model: &str, utterance: &str, question: &Question) -> Value {
    let mut body = payload(model, utterance, question);
    body["state"]["pending_question"] = json!({"title":question.title,"body":question.body,"options":question.options.iter().enumerate().map(|(i,o)| json!({"number":i+1,"label":o.label,"description":o.description})).collect::<Vec<_>>()});
    let mut criteria = serde_json::Map::new();
    criteria.insert("choices".into(), json!("The reply names or describes one or more listed options, or gives their numbers. Examples: option two; lint and compile the files; the first and third."));
    if question.allow_custom {
        criteria.insert("custom".into(), json!("The utterance provides a specific alternative answer or correction outside the offered options, such as asking to check email instead of code."));
    }
    criteria.insert(
        "clarify".into(),
        json!("The reply gives no specific answer. Examples: yes; okay; I do not know; whatever."),
    );
    body["questions"] = json!({"answer":{"type":"choice","instructions":"Does utterance identify offered choices, provide a specific alternative answer, or require clarification? A bare yes or okay does not identify an option in a numbered menu. Evaluate only the user's reply to pending_question.","criteria":criteria}});
    body
}
pub(crate) fn payload(model: &str, utterance: &str, question: &Question) -> Value {
    let mut questions = serde_json::Map::new();
    for (i, option) in question.options.iter().enumerate() {
        let number = i + 1;
        let ordinal = [
            "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth",
            "tenth",
        ]
        .get(i)
        .copied()
        .unwrap_or("");
        questions.insert(format!("option_{number}"), json!({"type":"choice","instructions":"Does the utterance select this option?", "criteria":{
            "yes":format!("The selected options include option {number}, the {ordinal} option."),
            "no":format!("The selected options exclude option {number}, the {ordinal} option."),
        }}));
        questions.insert(format!("meaning_{number}"), json!({"type":"choice","instructions":"Does the utterance ask for this option by its label or meaning?", "criteria":{
            "yes":format!("The user asks for: {}. {}", option.label, option.description),
            "no":format!("The user does not ask for: {}. {}", option.label, option.description),
        }}));
    }
    json!({"model":model,"state":{"utterance":utterance},"questions":questions})
}

pub(crate) fn parse_selection(response: &Value, question: &Question) -> Result<Answer> {
    let mut option_ids = Vec::new();
    for (i, option) in question.options.iter().enumerate() {
        let mut selected = false;
        for key in [format!("option_{}", i + 1), format!("meaning_{}", i + 1)] {
            match crate::routing::choice(response, &key)? {
                "yes" => selected = true,
                "no" => {}
                _ => bail!("unknown question option classification"),
            }
        }
        if selected {
            option_ids.push(option.id.clone());
        }
    }
    let answer = if option_ids.is_empty() || (!question.multiple && option_ids.len() > 1) {
        Answer::Clarify
    } else {
        Answer::Choices { option_ids }
    };
    question.validate(&answer)?;
    Ok(answer)
}
pub(crate) fn parse(response: &Value, question: &Question, utterance: &str) -> Result<Answer> {
    let answer = match crate::routing::choice(response, "answer")? {
        "clarify" => Answer::Clarify,
        "custom" if question.allow_custom => Answer::Custom {
            text: utterance.to_owned(),
        },
        _ => bail!("unknown question answer interpretation"),
    };
    question.validate(&answer)?;
    Ok(answer)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires the configured local Kev service"]
    async fn live_kev_selects_question_answers() {
        let router = crate::routing::Router::new(&crate::config::Routing::default()).unwrap();
        let mut question = Question {
            question_id: "evaluation-only".into(),
            command_purpose: None,
            question_kind: "question".into(),
            title: "Scope".into(),
            body: "What do you mean by check the case?".into(),
            options: [
                "Bug-hunt the apple-remote code",
                "Review if/elif cases in mouse script",
                "Check a behavioral scenario/case",
                "Lint/compile check the files",
            ]
            .iter()
            .enumerate()
            .map(|(i, label)| OptionItem {
                id: format!("id-{i}"),
                label: (*label).into(),
                description: String::new(),
            })
            .collect(),
            multiple: false,
            allow_custom: true,
        };
        let mut failures = Vec::new();
        for (utterance, multiple, expected) in [
            (
                "Option two",
                false,
                json!({"kind":"choices","option_ids":["id-1"]}),
            ),
            (
                "Lint and compile the files",
                false,
                json!({"kind":"choices","option_ids":["id-3"]}),
            ),
            (
                "I meant check my email, not a case.",
                false,
                json!({"kind":"custom","text":"I meant check my email, not a case."}),
            ),
            ("yes", false, json!({"kind":"clarify"})),
            (
                "The first and third",
                true,
                json!({"kind":"choices","option_ids":["id-0","id-2"]}),
            ),
            ("The first and third", false, json!({"kind":"clarify"})),
        ] {
            question.multiple = multiple;
            let (answer, evidence) = router.answer_question(utterance, &question).await.unwrap();
            let actual = serde_json::to_value(answer).unwrap();
            println!("{utterance} (multiple={multiple}): {actual}");
            if actual != expected {
                failures.push(format!(
                    "{utterance}: expected {expected}, got {actual}. Evidence: {evidence}"
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
    #[test]
    fn answer_validation_and_spoken_choices() {
        let q = Question {
            question_id: "q1".into(),
            command_purpose: None,
            question_kind: "question".into(),
            title: "Scope".into(),
            body: "What should I check?".into(),
            options: vec![
                OptionItem {
                    id: "a".into(),
                    label: "Code".into(),
                    description: String::new(),
                },
                OptionItem {
                    id: "b".into(),
                    label: "Inbox".into(),
                    description: String::new(),
                },
            ],
            multiple: false,
            allow_custom: true,
        };
        assert!(q
            .validate(&Answer::Choices {
                option_ids: vec!["b".into()]
            })
            .is_ok());
        assert!(q
            .validate(&Answer::Choices {
                option_ids: vec!["missing".into()]
            })
            .is_err());
        assert!(q
            .validate(&Answer::Choices {
                option_ids: vec!["a".into(), "b".into()]
            })
            .is_err());
        assert!(q
            .validate(&Answer::Custom {
                text: "Check my email, not code.".into()
            })
            .is_ok());
        assert!(q.speech().contains("Option 2: Inbox"));
        let response =
            |choice: &str| json!({"answers":{"answer":{"type":"choice","choice":choice}}});
        assert!(parse(&response("unknown"), &q, "second").is_err());
        assert!(
            matches!(parse(&response("custom"), &q, "Check email, not code.").unwrap(), Answer::Custom{text} if text=="Check email, not code.")
        );
        assert!(matches!(
            parse(&response("clarify"), &q, "yes").unwrap(),
            Answer::Clarify
        ));
        let mut q = q;
        q.allow_custom = false;
        assert!(parse(&response("custom"), &q, "email").is_err());
        assert!(
            mode_payload("kev", "email", &q)["questions"]["answer"]["criteria"]
                .get("custom")
                .is_none()
        );
        let mut response = json!({"answers":{"option_1":{"type":"choice","choice":"no"},"option_2":{"type":"choice","choice":"yes"},"meaning_1":{"type":"choice","choice":"no"},"meaning_2":{"type":"choice","choice":"no"}}});
        assert!(
            matches!(parse_selection(&response, &q).unwrap(), Answer::Choices{option_ids} if option_ids==vec!["b"])
        );
        response["answers"]["option_1"]["choice"] = json!("yes");
        assert!(matches!(
            parse_selection(&response, &q).unwrap(),
            Answer::Clarify
        ));
        q.multiple = true;
        assert!(
            matches!(parse_selection(&response, &q).unwrap(), Answer::Choices{option_ids} if option_ids==vec!["a", "b"])
        );
        response["answers"]
            .as_object_mut()
            .unwrap()
            .remove("option_2");
        assert!(parse_selection(&response, &q).is_err());
    }

    #[test]
    fn speech_transport_preserves_all_choices() {
        let q = Question {
            question_id: "q2".into(),
            command_purpose: None,
            question_kind: "question".into(),
            title: "Scope".into(),
            body: "What should I check?".into(),
            options: (0..8)
                .map(|i| OptionItem {
                    id: i.to_string(),
                    label: format!("Review component {i} and report its current behavior"),
                    description: String::new(),
                })
                .collect(),
            multiple: true,
            allow_custom: true,
        };
        let speech = crate::tts::normalize_for_speech(&q.speech());
        assert!(speech.len() > 360);
        assert!(speech.contains("Option 8:"));
        assert!(speech.contains("You can choose more than one."));
        assert!(speech.contains("You can also give your own answer."));
    }
}
