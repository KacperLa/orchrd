//! Agent-independent dialog detection and grounded, read-only extraction.
use crate::{
    agent::AgentInfo,
    intent::IntentClient,
    question::{OptionItem, Question},
    routing::{choice, Router},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Capture {
    pub agent: AgentInfo,
    pub screen: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Ignore,
    Inspect,
    Modal,
    Conversation,
}

pub(crate) fn route(response: &Value, lifecycle: &str) -> Result<Route> {
    let category = choice(response, "dialog")?;
    let origin = choice(response, "request_origin")?;
    if ![
        "modal_dialog",
        "conversational_question",
        "no_question",
        "uncertain",
    ]
    .contains(&category)
        || ![
            "current_dialog",
            "latest_assistant",
            "historical_or_user",
            "none",
            "uncertain",
        ]
        .contains(&origin)
    {
        bail!("invalid dialog classification")
    }
    Ok(match (category, origin, lifecycle) {
        (_, "historical_or_user" | "none", "blocked") => Route::Inspect,
        (_, "historical_or_user" | "none", _) => Route::Ignore,
        ("modal_dialog", "current_dialog", "blocked") => Route::Modal,
        ("conversational_question", "latest_assistant", "idle" | "done") => Route::Conversation,
        ("no_question", _, status) if status != "blocked" => Route::Ignore,
        _ => Route::Inspect,
    })
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Extracted {
    status: String,
    complete: bool,
    purpose: String,
    input_mode: String,
    question: String,
    options: Vec<ExtractedOption>,
    allows_custom_answer: Option<bool>,
    interaction: Interaction,
    evidence: Vec<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExtractedOption {
    label: String,
    effect: String,
    shortcut: Option<String>,
    selected: Option<bool>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Interaction {
    method: String,
    selected_index: Option<usize>,
    instructions: Option<String>,
}

// Validate the entire strict schema locally as well as asking the provider for it.
fn validate_schema(value: &Value, schema: &Value) -> Result<()> {
    let kinds = match &schema["type"] {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => bail!("invalid extraction schema"),
    };
    if !kinds.iter().any(|kind| match *kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "integer" => value.is_i64() || value.is_u64(),
        "null" => value.is_null(),
        _ => false,
    }) {
        bail!("invalid extraction field type")
    }
    if let Some(allowed) = schema["enum"].as_array() {
        if !allowed.contains(value) {
            bail!("invalid extraction enum")
        }
    }
    if let Some(object) = value.as_object() {
        let properties = schema["properties"]
            .as_object()
            .context("missing schema properties")?;
        if object.len() != properties.len()
            || properties.keys().any(|key| !object.contains_key(key))
        {
            bail!("missing or extra extraction fields")
        }
        for (key, child) in properties {
            validate_schema(&object[key], child)?;
        }
    }
    if let Some(array) = value.as_array() {
        for child in array {
            validate_schema(child, &schema["items"])?;
        }
    }
    Ok(())
}

pub(crate) fn present(value: Value, capture: &Capture, route: Route) -> Result<(Question, Value)> {
    validate_schema(
        &value,
        &serde_json::from_str::<Value>(include_str!("dialog/schema.json"))?,
    )?;
    let extracted: Extracted = serde_json::from_value(value.clone())?;
    let mut quotes: Vec<&str> = extracted.evidence.iter().map(String::as_str).collect();
    quotes.extend(extracted.options.iter().map(|o| o.label.as_str()));
    if !extracted.question.is_empty() {
        quotes.push(&extracted.question);
    }
    if let Some(instructions) = &extracted.interaction.instructions {
        quotes.push(instructions);
    }
    if quotes
        .iter()
        .any(|quote| quote.trim().is_empty() || !capture.screen.contains(quote))
    {
        bail!("extractor returned ungrounded screen text")
    }
    for option in &extracted.options {
        if option
            .shortcut
            .as_ref()
            .is_some_and(|key| key.is_empty() || !capture.screen.contains(key))
        {
            bail!("extractor returned an ungrounded shortcut")
        }
    }
    if extracted
        .interaction
        .selected_index
        .is_some_and(|index| index >= extracted.options.len())
    {
        bail!("invalid selected option")
    }
    if extracted.status != "active"
        || !extracted.complete
        || extracted.question.is_empty()
        || extracted.evidence.is_empty()
    {
        bail!("request is not active and complete")
    }
    if extracted.purpose == "unknown"
        || extracted.input_mode == "unknown"
        || extracted.purpose == "authentication"
    {
        bail!("request requires terminal inspection")
    }
    if route == Route::Conversation && extracted.purpose != "clarification" {
        bail!("conversational permission requires terminal inspection")
    }
    if (extracted.input_mode == "single_choice" && extracted.options.len() < 2)
        || (extracted.input_mode == "multiple_choice" && extracted.options.is_empty())
    {
        bail!("request is missing choices")
    }
    let labels: std::collections::HashSet<_> = extracted.options.iter().map(|o| &o.label).collect();
    if labels.len() != extracted.options.len() {
        bail!("ambiguous duplicate choices")
    }
    let question = Question {
        question_id: uuid::Uuid::new_v4().to_string(),
        question_kind: if extracted.purpose == "permission" {
            "permission"
        } else {
            "question"
        }
        .into(),
        title: if extracted.purpose == "permission" {
            "Permission request"
        } else {
            "Question"
        }
        .into(),
        body: extracted.question,
        options: extracted
            .options
            .into_iter()
            .enumerate()
            .map(|(index, option)| OptionItem {
                id: index.to_string(),
                label: option.label,
                description: option.effect,
            })
            .collect(),
        multiple: extracted.input_mode == "multiple_choice",
        allow_custom: extracted.allows_custom_answer == Some(true)
            || extracted.input_mode == "free_text",
    };
    Ok((question, value))
}

pub(crate) async fn detect(router: &Router, capture: &Capture) -> Result<Route> {
    if capture.screen.len() > 64000 {
        bail!("screen exceeds dialog limit")
    }
    let response = router.request(&json!({"model":router.model(),"state":{
        "current_screen":capture.screen,"lifecycle":capture.agent.status,
        "state_change_seq":capture.agent.state_change_seq,"previous_screen":null,"previous_lifecycle":null
    },"questions":serde_json::from_str::<Value>(include_str!("dialog/questions.json"))?})).await?;
    route(&response, &capture.agent.status)
}

pub(crate) async fn extract(
    intent: &IntentClient,
    capture: &Capture,
    route: Route,
) -> Result<(Question, Value)> {
    let value = intent
        .extract_dialog(&capture.screen, &capture.agent.status)
        .await?;
    present(value, capture, route)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn capture() -> Capture {
        Capture {
            agent: AgentInfo {
                name: "pane".into(),
                display_name: "coder".into(),
                instance_id: "terminal".into(),
                kind: None,
                cwd: None,
                status: "blocked".into(),
                state_change_seq: Some(2),
            },
            screen: "Choose destination:\n> Local\n  Cloud".into(),
        }
    }
    fn extraction() -> Value {
        json!({"status":"active","complete":true,"purpose":"clarification","input_mode":"single_choice",
            "question":"Choose destination:","options":[{"label":"Local","effect":"other","shortcut":null,"selected":null},
            {"label":"Cloud","effect":"other","shortcut":null,"selected":null}],"allows_custom_answer":null,
            "interaction":{"method":"unknown","selected_index":0,"instructions":null},"evidence":["Choose destination:"]})
    }
    #[test]
    fn grounded_complete_questions_can_be_read_without_guessed_controls() {
        let (question, _) = present(extraction(), &capture(), Route::Modal).unwrap();
        assert_eq!(question.options.len(), 2);
        assert!(question.speech().contains("Option 2: Cloud"));
        assert!(!question.allow_custom);
    }
    #[test]
    fn incomplete_invented_or_malformed_extractions_are_not_presented() {
        for (field, bad) in [
            ("complete", json!(false)),
            ("question", json!("Invented")),
            ("input_mode", json!("unknown")),
            ("purpose", json!("authentication")),
            ("evidence", json!([])),
            ("allows_custom_answer", json!("true")),
        ] {
            let mut value = extraction();
            value[field] = bad;
            assert!(present(value, &capture(), Route::Modal).is_err(), "{field}");
        }
        let mut value = extraction();
        value["options"][0]["effect"] = json!("approve");
        assert!(present(value, &capture(), Route::Modal).is_err());
        let mut value = extraction();
        value
            .as_object_mut()
            .unwrap()
            .remove("allows_custom_answer");
        assert!(present(value, &capture(), Route::Modal).is_err());
        let mut value = extraction();
        value["interaction"]["selected_index"] = json!(true);
        assert!(present(value, &capture(), Route::Modal).is_err());
        let mut value = extraction();
        value["purpose"] = json!("permission");
        assert!(present(value, &capture(), Route::Conversation).is_err());
    }
    #[test]
    fn source_and_lifecycle_gate_candidate_extraction() {
        let response = |category, origin| {
            json!({"answers":{
            "dialog":{"type":"choice","choice":category},"request_origin":{"type":"choice","choice":origin}}})
        };
        assert_eq!(
            route(&response("modal_dialog", "current_dialog"), "blocked").unwrap(),
            Route::Modal
        );
        assert_eq!(
            route(&response("modal_dialog", "current_dialog"), "idle").unwrap(),
            Route::Inspect
        );
        assert_eq!(
            route(
                &response("conversational_question", "latest_assistant"),
                "idle"
            )
            .unwrap(),
            Route::Conversation
        );
        assert_eq!(
            route(&response("no_question", "none"), "idle").unwrap(),
            Route::Ignore
        );
        assert_eq!(
            route(&response("modal_dialog", "historical_or_user"), "blocked").unwrap(),
            Route::Inspect
        );
        assert_eq!(
            route(&response("modal_dialog", "latest_assistant"), "blocked").unwrap(),
            Route::Inspect
        );
        assert!(route(&response("unknown", "none"), "idle").is_err());
    }
}
