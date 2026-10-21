//! Closed-set intent classification; target fallback and execution stay in code.
use crate::{agent::AgentInfo, config::Routing};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Operation {
    SendToAgent,
    AnswerQuestion,
    ReadAgent,
    RepeatLast,
    Clarify,
    Approve,
    Deny,
    Cancel,
}
#[derive(Debug)]
pub(crate) struct Decision {
    pub operation: Operation,
    pub agent: Option<AgentInfo>,
    pub clarification: Option<String>,
}
pub(crate) struct Router {
    http: reqwest::Client,
    settings: Routing,
}
impl Router {
    pub fn new(settings: &Routing) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(settings.timeout_seconds))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            settings: settings.clone(),
        })
    }
    #[cfg(test)]
    pub async fn classify(
        &self,
        text: &str,
        agents: &[AgentInfo],
        active: Option<&str>,
    ) -> Result<(Decision, Value)> {
        self.classify_with_questions(text, agents, active, json!([]))
            .await
    }
    pub async fn classify_with_questions(
        &self,
        text: &str,
        agents: &[AgentInfo],
        active: Option<&str>,
        questions: Value,
    ) -> Result<(Decision, Value)> {
        let mut body = payload(&self.settings.model, text, agents);
        body["state"]["pending_questions"] = operation_questions(&questions);
        let result = self.request(&body).await?;
        let target = pending_target(&result, &questions)?.or(active);
        let decision = resolve(&result, agents, target)?;
        Ok((decision, result))
    }
    pub async fn answer_question(
        &self,
        utterance: &str,
        question: &crate::question::Question,
    ) -> Result<(crate::question::Answer, Value)> {
        let mode = self
            .request(&crate::question::mode_payload(
                &self.settings.model,
                utterance,
                question,
            ))
            .await?;
        match choice(&mode, "answer")? {
            "custom" | "clarify" => {
                let answer = crate::question::parse(&mode, question, utterance)?;
                Ok((answer, json!({"interpretation":mode})))
            }
            "choices" => {
                let body = crate::question::payload(&self.settings.model, utterance, question);
                let result = self.request(&body).await?;
                let answer = crate::question::parse_selection(&result, question)?;
                Ok((answer, json!({"interpretation":mode,"selection":result})))
            }

            _ => bail!("unknown question answer interpretation"),
        }
    }

    pub(crate) fn model(&self) -> &str {
        &self.settings.model
    }

    pub(crate) async fn request(&self, body: &Value) -> Result<Value> {
        let mut request = self.http.post(&self.settings.endpoint).json(body);
        if !self.settings.api_key.is_empty() {
            request = request.bearer_auth(&self.settings.api_key);
        }
        let response = request
            .send()
            .await
            .map_err(|error| error.without_url())
            .context("call routing service")?;
        if !response.status().is_success() {
            bail!("routing service returned HTTP {}", response.status());
        }
        let result: Value = response.json().await.context("decode routing response")?;
        Ok(result)
    }
}
fn pending_target<'a>(response: &Value, questions: &'a Value) -> Result<Option<&'a str>> {
    let operation = choice(response, "operation")?;
    if !matches!(operation, "answer_question" | "approve" | "deny")
        || choice(response, "explicit_target")? != "unspecified"
    {
        return Ok(None);
    }
    let mut targets = std::collections::HashSet::new();
    if let Some(questions) = questions.as_array() {
        for question in questions {
            if matches!(operation, "approve" | "deny")
                && question["question"]["question_kind"] != "permission"
            {
                continue;
            }
            if let Some(id) = question["agent_id"].as_str() {
                targets.insert(id);
            }
        }
    }
    Ok(if targets.len() == 1 {
        targets.into_iter().next()
    } else {
        None
    })
}
fn operation_questions(questions: &Value) -> Value {
    // A pending permission's command/body must not turn a new work request into
    // approval merely because its words match. Keep only permission presence
    // and scope/option numbers here. Answer selection gets the full question.
    json!(questions.as_array().into_iter().flatten().map(|item| {
        if item["question"]["question_kind"] != "permission" { return item.clone(); }
        let options = item["question"]["options"].as_array().into_iter().flatten().enumerate()
            .map(|(index,option)| json!({"number":index+1,"effect":match option["description"].as_str() {
                Some("allow_once")=>"allow_once",Some("allow_persistent")=>"allow_persistent",Some("deny")=>"deny",_=>"unknown"
            }})).collect::<Vec<_>>();
        json!({"agent_id":item["agent_id"],"agent_name":item["agent_name"],"question":{
            "question_kind":"permission","options":options,"allow_custom":false}})
    }).collect::<Vec<_>>())
}
fn names(agents: &[AgentInfo]) -> Vec<String> {
    let mut names = Vec::new();
    for agent in agents {
        let name = if agent.display_name.ends_with(" Agent") {
            agent.display_name.clone()
        } else {
            format!("{} Agent", agent.display_name)
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}
pub(crate) fn payload(model: &str, text: &str, agents: &[AgentInfo]) -> Value {
    let names = names(agents);
    let mut targets = serde_json::Map::new();
    for (i, name) in names.iter().enumerate() {
        targets.insert(format!("name_{i}"), json!(format!("meaning: The utterance addresses or asks about the agent named {name}. agent_name: {name}")));
    }
    targets.insert("unspecified".into(), json!("meaning: The utterance names no agent. References such as it, the agent, and you are not explicit agent names."));
    targets.insert("unknown".into(), json!("meaning: The utterance names an agent whose name is absent from available_agent_names."));
    json!({"model":model,"state":{"utterance":text,"available_agent_names":names},"questions":{
        "operation":{"type":"choice","instructions":"What kind of request is expressed in utterance? Classify the user's speech act, not the task described by a pending question. Pending questions provide context for answer_question only. A request to give an agent work is send_to_agent, even when that work needs permission. Never treat 'Ask Builder to run the tests' as approval. Approve and deny require a permission decision; agent availability is handled separately.","criteria":{
            "answer_question":"Answer a pending question in pending_questions: select offered choices, give a custom reply, or correct what the agent understood. Examples: Option two., Both the first and third., I meant check my email, not the code. Explicitly selecting persistent permission (Always allow, do not ask again) is answer_question, not generic approve. Use only when a question is pending.",
            "send_to_agent":"meaning: Give a coding agent work, a question to investigate, or feedback about its task. excludes: Asking for the agent's existing progress or results; replaying Orchard speech. examples: Check my emails., Run the tests again., That label is fine., What is HTTP?",
            "read_agent":"meaning: Retrieve a coding agent's existing progress, current activity, or results. excludes: Asking the agent to perform new work or investigate a new question. examples: What is Reviewer doing?, Has it finished?, What did it find?",
            "repeat_last":"meaning: Replay what Orchard just said because the user wants to hear it again. excludes: Asking an agent to repeat a task. examples: Say that again., I did not hear you.",
            "clarify":"meaning: The utterance contains no identifiable task, question, task feedback, status request, or speech replay request. examples: Hello!",
            "approve":"Allow an agent's pending permission request once. Examples: Approve it., Allow that tool call., Allow it once. Explicit persistent permission selection belongs to answer_question. A request to perform work or task feedback belongs to send_to_agent.",
            "deny":"Reject an agent's pending permission request. Examples: Deny that permission., Do not allow that tool call.",
            "cancel":"Stop the selected agent's current work. Examples: Cancel its task., Tell Reviewer to stop working. Replaying speech belongs to repeat_last."
        }},
        "explicit_target":{"type":"choice","instructions":"Which agent name is used as the addressee or subject of `utterance`? Compare with `available_agent_names`.","criteria":targets}
    }})
}
pub(crate) fn choice<'a>(result: &'a Value, question: &str) -> Result<&'a str> {
    let answer = &result["answers"][question];
    if answer["type"] != "choice" {
        bail!("routing service returned an invalid {question} answer");
    }
    answer["choice"]
        .as_str()
        .with_context(|| format!("routing service omitted {question} choice"))
}
pub(crate) fn resolve(
    result: &Value,
    agents: &[AgentInfo],
    active: Option<&str>,
) -> Result<Decision> {
    let operation: Operation = serde_json::from_value(json!(choice(result, "operation")?))
        .context("unknown routing operation")?;
    let mut decision = Decision {
        operation,
        agent: None,
        clarification: None,
    };
    if operation == Operation::RepeatLast {
        return Ok(decision);
    }
    if operation == Operation::Clarify {
        decision.clarification = Some("What would you like me to ask an agent to do?".into());
        return Ok(decision);
    }
    let target = choice(result, "explicit_target")?;
    let matching: Vec<&AgentInfo> = match target {
        "unknown" => Vec::new(),
        "unspecified" if agents.len() == 1 => vec![&agents[0]],
        "unspecified" => agents
            .iter()
            .filter(|a| Some(a.name.as_str()) == active)
            .collect(),
        _ => {
            let names = names(agents);
            let index = target
                .strip_prefix("name_")
                .and_then(|i| i.parse::<usize>().ok())
                .context("invalid routing target")?;
            let name = names
                .get(index)
                .context("routing target is outside the roster")?;
            agents
                .iter()
                .filter(|a| a.display_name == *name || format!("{} Agent", a.display_name) == *name)
                .collect()
        }
    };
    if matching.len() == 1 {
        decision.agent = Some(matching[0].clone());
    } else {
        decision.operation = Operation::Clarify;
        decision.clarification = Some(if agents.is_empty() {
            "No agents are connected. Open an agent session first.".into()
        } else {
            format!(
                "Which agent do you mean? Connected agents are {}.",
                agents
                    .iter()
                    .map(|a| a.display_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
    }
    Ok(decision)
}

pub(crate) fn acknowledgement(operation: Operation, name: &str, receipt: &Value) -> String {
    if receipt["delivery"] != "accepted" {
        if let Some(error) = receipt["error"].as_str() {
            return format!(
                "Delivery to {name} is unconfirmed: {}. I haven't retried it.",
                crate::intent::redact(error)
            );
        }
        return format!("I couldn't confirm delivery to {name}. I haven't retried it.");
    }
    match (operation, receipt["outcome"].as_str().unwrap_or_default()) {
        (Operation::SendToAgent, "started") => {
            format!("Sent your request to {name}; it started working.")
        }
        (Operation::SendToAgent, _) => {
            format!("{name} accepted your request. Completion is not confirmed yet.")
        }
        (Operation::Cancel, "stopped") => format!("{name} stopped working."),
        (Operation::AnswerQuestion, "answer_recorded") => {
            format!("{name} recorded your answer.")
        }
        (Operation::Approve | Operation::Deny | Operation::AnswerQuestion, "response_observed") => {
            format!("{name} accepted your answer and left its waiting state.")
        }
        _ => format!("{name} accepted the command. Its outcome is not confirmed yet."),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "calls the configured System One service; synthetic questions only, never sends agent input"]
    async fn configured_service_routes_herdr_controls() {
        let config = crate::config::Config::load(None).unwrap();
        let router = Router::new(&config.routing).unwrap();
        let roster = vec![agent("pane", "Builder")];
        let pending = json!([{"agent_id":"pane","agent_name":"Builder","question":{
            "question_id":"synthetic-permission","question_kind":"permission","title":"Run a command?",
            "body":"Run the tests?","options":[{"id":"once","label":"Allow once","description":"allow_once"},
                {"id":"always","label":"Always allow","description":"allow_persistent"},
                {"id":"deny","label":"Deny","description":"deny"}],"multiple":false,"allow_custom":false}}]);
        for (utterance, expected) in [
            ("Approve it", Operation::Approve),
            ("Deny it", Operation::Deny),
            ("Option two", Operation::AnswerQuestion),
            ("Always allow", Operation::AnswerQuestion),
            ("Run the tests again", Operation::SendToAgent),
            ("Ask Builder to check the build", Operation::SendToAgent),
            ("Cancel Builder", Operation::Cancel),
            ("Ask Builder to run the tests", Operation::SendToAgent),
        ] {
            let (decision, _) = router
                .classify_with_questions(utterance, &roster, None, pending.clone())
                .await
                .unwrap();
            println!("{utterance}: {:?}", decision.operation);
            assert_eq!(decision.operation, expected);
        }
    }

    #[test]
    fn operation_classification_does_not_see_a_pending_permissions_command_text() {
        let raw = json!([{"agent_id":"one","agent_name":"Builder","question":{"question_kind":"permission",
            "body":"Run PRIVATE-COMMAND?","title":"PRIVATE-TITLE","options":[
                {"label":"Always allow PRIVATE-COMMAND","description":"allow_persistent"}]}}]);
        let value = operation_questions(&raw);
        assert!(!value.to_string().contains("PRIVATE"));
        assert_eq!(
            value[0]["question"]["options"][0]["effect"],
            "allow_persistent"
        );
        assert_eq!(value[0]["question"]["options"][0]["number"], 1);
    }

    #[test]
    fn a_reply_can_target_the_only_pending_question_without_a_previous_send() {
        let response = |operation| {
            json!({"answers":{"operation":{"type":"choice","choice":operation},
            "explicit_target":{"type":"choice","choice":"unspecified"}}})
        };
        let questions = json!([{"agent_id":"one","question":{"question_kind":"permission"}}]);
        assert_eq!(
            pending_target(&response("approve"), &questions).unwrap(),
            Some("one")
        );
        assert_eq!(
            pending_target(&response("send_to_agent"), &questions).unwrap(),
            None
        );
        let questions = json!([{"agent_id":"one","question":{"question_kind":"question"}},
            {"agent_id":"two","question":{"question_kind":"permission"}}]);
        assert_eq!(
            pending_target(&response("answer_question"), &questions).unwrap(),
            None
        );
        assert_eq!(
            pending_target(&response("deny"), &questions).unwrap(),
            Some("two")
        );
    }

    fn agent(id: &str, name: &str) -> AgentInfo {
        AgentInfo {
            name: id.into(),
            display_name: name.into(),
            instance_id: id.into(),
            kind: None,
            cwd: None,
            status: "idle".into(),
            state_change_seq: None,
        }
    }
    fn result(op: &str, target: &str) -> Value {
        json!({"answers":{"operation":{"type":"choice","choice":op},"explicit_target":{"type":"choice","choice":target}}})
    }
    #[test]
    fn choice_order_matches_the_evaluated_request() {
        let body = payload(
            "kev-latest",
            "Check tests",
            &[agent("a", "Coder"), agent("b", "Reviewer")],
        );
        let choices = body["questions"]["operation"]["criteria"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            choices,
            vec![
                "answer_question",
                "send_to_agent",
                "read_agent",
                "repeat_last",
                "clarify",
                "approve",
                "deny",
                "cancel"
            ]
        );
        let targets = body["questions"]["explicit_target"]["criteria"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(targets, vec!["name_0", "name_1", "unspecified", "unknown"]);
    }
    #[tokio::test]
    async fn routing_http_uses_current_input_and_resolves_known_target() {
        let response = result("send_to_agent", "name_0").to_string();
        let (settings, captured) = crate::speech_api::tests::server("200 OK", &response).await;
        let router = Router::new(&Routing {
            endpoint: format!("{}systemone", settings.base_url),
            model: "configured-decision-model".into(),
            api_key: "test-decision-key".into(),
            ..Routing::default()
        })
        .unwrap();
        let (decision, _) = router
            .classify("Check the tests.", &[agent("a", "Coder")], None)
            .await
            .unwrap();
        assert_eq!(decision.agent.unwrap().name, "a");
        let bytes = captured.await.unwrap();
        let request = String::from_utf8(bytes).unwrap();
        assert!(request
            .to_lowercase()
            .contains("authorization: bearer test-decision-key\r\n"));
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["model"], "configured-decision-model");
        assert!(!body.to_string().contains("test-decision-key"));
        assert_eq!(
            body["state"],
            json!({"utterance":"Check the tests.","available_agent_names":["Coder Agent"],"pending_questions":[]})
        );
        assert_eq!(
            body["questions"]["operation"]["criteria"]
                .as_object()
                .unwrap()
                .len(),
            8
        );
    }
    #[tokio::test]
    async fn question_interpretation_uses_kev_with_only_current_input() {
        let (settings, captured) = crate::speech_api::tests::server(
            "200 OK",
            &json!({"answers":{"answer":{"type":"choice","choice":"clarify"}}}).to_string(),
        )
        .await;
        let router = Router::new(&Routing {
            endpoint: format!("{}systemone", settings.base_url),
            ..Routing::default()
        })
        .unwrap();
        let question: crate::question::Question = serde_json::from_value(json!({
            "question_id":"current", "question_kind":"question", "title":"Scope", "body":"Which scope?",
            "options":[{"id":"code-id","label":"Code"},{"id":"mail-id","label":"Email"}],
            "multiple":false,"allow_custom":false,
        })).unwrap();
        let (answer, _) = router.answer_question("Second", &question).await.unwrap();
        assert!(matches!(answer, crate::question::Answer::Clarify));
        let request = String::from_utf8(captured.await.unwrap()).unwrap();
        assert!(!request.to_lowercase().contains("authorization:"));
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["state"]["utterance"], "Second");
        assert_eq!(
            body["state"]["pending_question"]["options"][1],
            json!({"number":2,"label":"Email","description":""})
        );
        assert!(body.get("messages").is_none());
        assert!(body["questions"]["answer"]["criteria"]
            .get("custom")
            .is_none());
    }
    #[tokio::test]
    async fn routing_failure_does_not_produce_a_decision() {
        let (settings, captured) =
            crate::speech_api::tests::server("503 Service Unavailable", "private server error")
                .await;
        let router = Router::new(&Routing {
            endpoint: format!("{}systemone", settings.base_url),
            ..Routing::default()
        })
        .unwrap();
        let error = router.classify("Check tests", &[], None).await.unwrap_err();
        assert!(error.to_string().contains("503"));
        assert!(!error.to_string().contains("private server error"));
        captured.await.unwrap();
    }
    #[test]
    fn fallback_never_overrides_unknown_or_duplicate_names() {
        let agents = vec![agent("a", "Coder"), agent("b", "Reviewer")];
        assert_eq!(
            resolve(&result("send_to_agent", "unspecified"), &agents, Some("b"))
                .unwrap()
                .agent
                .unwrap()
                .name,
            "b"
        );
        for active in [None, Some("gone")] {
            assert_eq!(
                resolve(&result("send_to_agent", "unspecified"), &agents, active)
                    .unwrap()
                    .operation,
                Operation::Clarify
            );
        }
        assert_eq!(
            resolve(&result("send_to_agent", "unknown"), &agents, Some("b"))
                .unwrap()
                .operation,
            Operation::Clarify
        );
        assert_eq!(
            resolve(&result("send_to_agent", "unspecified"), &agents[..1], None)
                .unwrap()
                .agent
                .unwrap()
                .name,
            "a"
        );
        let duplicates = vec![agent("a", "Reviewer"), agent("b", "Reviewer")];
        assert_eq!(
            resolve(&result("send_to_agent", "name_0"), &duplicates, Some("b"))
                .unwrap()
                .operation,
            Operation::Clarify
        );
        assert!(resolve(&result("send_to_agent", "name_20"), &agents, None).is_err());
        assert!(resolve(&result("invented_tool", "unspecified"), &agents, None).is_err());
    }
    #[test]
    fn repeat_does_not_need_a_target_and_receipts_do_not_claim_completion() {
        assert_eq!(
            resolve(&result("repeat_last", "garbage"), &[], None)
                .unwrap()
                .operation,
            Operation::RepeatLast
        );
        let speech = acknowledgement(
            Operation::SendToAgent,
            "Coder",
            &json!({"delivery":"unconfirmed","outcome":"started"}),
        );
        assert!(speech.contains("couldn't confirm"));
        let speech = acknowledgement(
            Operation::SendToAgent,
            "Coder",
            &json!({"delivery":"accepted","outcome":"delivered"}),
        );
        assert!(speech.contains("Completion is not confirmed"));
    }
}
