use crate::{
    config::{Config, Project},
    db::{AgentSession, ConversationMessage},
};
use anyhow::{bail, Context, Result};
use regex::Regex;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct ModelTurn {
    pub message: Value,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Clone)]
pub struct IntentClient {
    http: Client,
    base_url: String,
    model: String,
    api_key: String,
}
impl IntentClient {
    pub fn new(cfg: &Config) -> Result<Self> {
        Ok(Self {
            http: Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(cfg.intent.timeout_seconds))
                .build()
                .context("build intent HTTP client")?,
            base_url: cfg.intent.base_url.trim_end_matches('/').to_owned(),
            model: cfg.intent.model.clone(),
            api_key: cfg.api_key()?,
        })
    }
    pub fn conversation_messages(
        &self,
        history: &[ConversationMessage],
        projects: &[Project],
        sessions: &[AgentSession],
        active_project: Option<&str>,
    ) -> Vec<Value> {
        let context = routing_context(projects, sessions, active_project);
        let system = format!(
            r#"You are Orchrd, a concise voice interface that orchestrates coding agents in Herdr.

MANDATORY TOOL ROUTING CONTRACT — follow this before all other instructions:
- If the user asks you to ask, tell, send, message, instruct, or have a coding agent do or answer something, call send_to_agent. Put only the cleaned request intended for that agent in request. Do not answer the question yourself and do not call speak.
- If the user directly requests coding, repository inspection, or a project change, call send_to_agent.
- If the user asks to create an empty session, call start_session. If they ask to resume an existing session, call resume_session.
- If the user approves or denies a pending request, call respond_to_agent.
- If the user asks what an agent is doing or what it said, call read_agent or latest_result first, then call speak with the grounded answer on the following turn.
- Call speak only when answering the user directly from known information or asking a clarification question. speak never performs an action and must never say or imply that you asked, told, sent, started, resumed, approved, denied, cancelled, or changed anything.
- Never narrate an intended action. Perform the correct tool call instead. The controller will acknowledge side-effect tools after they actually succeed.
- When the requested project cannot be resolved, call speak to ask one short clarification. Do not guess a project or action.

Examples: “Ask the Orchrd agent whether I need a flake update” means call send_to_agent for Orchrd with request “Do I need to run a flake update?” “Tell Bike FSD to run its tests” means call send_to_agent for Bike FSD with request “Run the tests.” “What did the Orchrd agent say?” means call read_agent first. “What is Git?” means call speak with the answer.

Carry on a natural conversation and resolve references from recent dialogue, including references to things you previously said. Use tools whenever current Herdr or project information is needed. You may answer unrelated general questions directly. For detailed questions about a project, first read its agent output; if that is insufficient, send the coding agent a focused question. Do not pretend you inspected something unless you used a tool.
Use start_session when the user only wants a new empty coding session. Use resume_session when they want the most recent saved Codex conversation for that project. Use send_to_agent only when there is a concrete request or question to send.
The request passed to send_to_agent must be a faithful, cleaned interpretation of what the user said. Correct obvious speech-recognition errors and remove filler, but preserve the user's meaning, scope, uncertainty, and level of detail. Do not add implementation ideas, likely files or technologies, examples, rationale, priorities, acceptance criteria, or investigative steps that the user did not state. Resolve a pronoun or reference from recent dialogue only when its meaning is explicit; otherwise ask a short clarification. The routing context is only for choosing a tool and project. Never copy or infer requirements from it into the request.
When the user approves or denies a pending request, you MUST call respond_to_agent. Interpret natural replies such as approved, go ahead, allow it, deny it, or no from the recent conversation; do not use exact phrase matching. Never claim that a request was approved or denied unless respond_to_agent returned ok true. If the tool fails, briefly explain that the response was not accepted.
Every turn must use tools. Use speak for an ordinary answer or a clarification question. Never place a user-visible reply directly in assistant content. The controller, not you, acknowledges successful or failed side-effect tools; never call speak to claim that an action was sent, started, resumed, approved, denied, or cancelled.
 Keep spoken replies brief and natural, normally one or two sentences, with no Markdown, lists, headings, or internal metadata. Codex is the coding worker: delegate repository inspection and code changes instead of attempting them yourself. Ask a short clarification when project or intent cannot be inferred. Never invent project IDs. The coding agent is responsible for judging requested actions and requesting any necessary approvals; Orchrd must faithfully route the request without pre-filtering it.
Minimal routing context: {context}"#
        );
        let mut messages = vec![json!({"role":"system","content":system})];
        messages.extend(history.iter().filter_map(|item| {
            if !matches!(item.role.as_str(), "user" | "assistant") {
                return None;
            }
            let content = strip_internal_context(&item.content);
            Some(json!({"role":item.role,"content":content}))
        }));
        messages
    }
    pub async fn next_conversation_turn(&self, messages: &[Value]) -> Result<ModelTurn> {
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&json!({
                "model": self.model,
                "temperature": 0,
                "messages": messages,
                "tools": conversation_tools(),
                "tool_choice": "required"
            }))
            .send()
            .await
            .context("call conversational agent API")?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            bail!(
                "conversational agent API returned {status}: {}",
                truncate(&body, 500)
            )
        }
        let value: Value =
            serde_json::from_str(&body).context("parse conversational API response")?;
        let message = value
            .pointer("/choices/0/message")
            .cloned()
            .context("conversational API response omitted choices[0].message")?;
        let mut tool_calls = vec![];
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .context("tool call omitted id")?;
                let function = call.get("function").context("tool call omitted function")?;
                let name = function
                    .get("name")
                    .and_then(Value::as_str)
                    .context("tool call omitted function name")?;
                let raw = function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                let arguments =
                    serde_json::from_str(raw).context("tool call arguments were not JSON")?;
                tool_calls.push(ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments,
                });
            }
        }
        if tool_calls.is_empty() {
            bail!("conversational model returned no tool call")
        }
        Ok(ModelTurn {
            message,
            tool_calls,
        })
    }
    pub async fn summarize(&self, project: &Project, task: &str, terminal: &str) -> Result<String> {
        let system="Summarize a coding agent result for speech. Return JSON with a single string field named speech. Use at most two short sentences. State outcome first, then tests or blocker. Do not use markdown.";
        let user =
            json!({"project":project.name,"task":task,"agent_output":redact(terminal)}).to_string();
        let content = self.complete(system, &user).await?;
        let v: Value = parse_value(&content)?;
        v.get("speech")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("summary response omitted speech")
    }
    pub async fn describe_blocker(&self, project: &Project, terminal: &str) -> Result<String> {
        let system = "Explain a coding agent's latest question or approval request for speech. Return JSON with one string field named speech. State the project and exactly what input is needed in at most two short sentences. Do not use markdown.";
        let user = json!({"project":project.name,"agent_output":redact(terminal)}).to_string();
        let content = self.complete(system, &user).await?;
        let value: Value = parse_value(&content)?;
        value
            .get("speech")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("blocker response omitted speech")
    }
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let response=self.http.post(format!("{}/chat/completions",self.base_url)).bearer_auth(&self.api_key).json(&json!({"model":self.model,"temperature":0,"messages":[{"role":"system","content":system},{"role":"user","content":user}]})).send().await.context("call intent API")?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            bail!("intent API returned {status}: {}", truncate(&body, 500))
        }
        let v: Value = serde_json::from_str(&body).context("parse intent API response")?;
        v.pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("intent API response omitted choices[0].message.content")
    }
}

fn conversation_tools() -> Value {
    json!([
        {"type":"function","function":{"name":"speak","description":"Speak a direct informational answer from known information or ask a clarification question. This tool performs no action. Never choose it when the user asks to ask, tell, send, message, or assign something to a coding agent, and never use it to narrate an intended or completed action; use the corresponding action tool instead.","parameters":{"type":"object","properties":{"speech":{"type":"string","description":"A brief natural answer or clarification with no Markdown, internal metadata, or claim that an action was performed."},"kind":{"type":"string","enum":["answer","clarification"]}},"required":["speech","kind"],"additionalProperties":false}}},
        {"type":"function","function":{"name":"list_projects","description":"List Orchrd's discovered project IDs and names for routing. This intentionally omits project descriptions and paths so they cannot become inferred task requirements.","parameters":{"type":"object","properties":{},"additionalProperties":false}}},
        {"type":"function","function":{"name":"list_agents","description":"List current Herdr coding-agent sessions and states.","parameters":{"type":"object","properties":{},"additionalProperties":false}}},
        {"type":"function","function":{"name":"read_agent","description":"Read recent terminal output from a project's Herdr agent.","parameters":{"type":"object","properties":{"project_id":{"type":"string"},"lines":{"type":"integer","minimum":10,"maximum":300}},"required":["project_id"],"additionalProperties":false}}},
        {"type":"function","function":{"name":"start_session","description":"Create a new empty Codex session for a project without sending a task or creating a run.","parameters":{"type":"object","properties":{"project_id":{"type":"string"}},"required":["project_id"],"additionalProperties":false}}},
        {"type":"function","function":{"name":"resume_session","description":"Resume the most recent saved Codex session for a project through Herdr's generic agent start API.","parameters":{"type":"object","properties":{"project_id":{"type":"string"}},"required":["project_id"],"additionalProperties":false}}},
        {"type":"function","function":{"name":"send_to_agent","description":"Send a faithful cleaned interpretation of the user's concrete request to a project's Codex agent, starting one if necessary. The request may correct transcription errors and remove filler, but must not add guessed requirements, implementation ideas, examples, rationale, or acceptance criteria.","parameters":{"type":"object","properties":{"project_id":{"type":"string"},"request":{"type":"string","description":"A concise semantic transcription containing only what the user expressed, plus unambiguous reference resolution from recent dialogue."},"fresh_session":{"type":"boolean"}},"required":["project_id","request","fresh_session"],"additionalProperties":false}}},
        {"type":"function","function":{"name":"latest_result","description":"Get Orchrd's latest completed or active task record for a project.","parameters":{"type":"object","properties":{"project_id":{"type":"string"}},"required":["project_id"],"additionalProperties":false}}},
        {"type":"function","function":{"name":"respond_to_agent","description":"Required operation for approving or denying a blocked coding-agent confirmation. Call this for any natural-language approval or denial; do not merely reply that it was handled.","parameters":{"type":"object","properties":{"project_id":{"type":"string","description":"The project whose pending request the user is answering."},"approve":{"type":"boolean","description":"True to approve the request, false to deny it."}},"required":["project_id","approve"],"additionalProperties":false}}},
        {"type":"function","function":{"name":"cancel_agent","description":"Cancel the current operation for a project's coding agent.","parameters":{"type":"object","properties":{"project_id":{"type":"string"}},"required":["project_id"],"additionalProperties":false}}}
    ])
}
fn routing_context(
    projects: &[Project],
    sessions: &[AgentSession],
    active_project: Option<&str>,
) -> Value {
    json!({
        "active_project": active_project,
        "projects": projects.iter().map(|project| json!({
            "id": project.id,
            "name": project.name,
        })).collect::<Vec<_>>(),
        "agent_sessions": sessions.iter().map(|session| json!({
            "project_id": session.project_id,
            "status": session.status,
        })).collect::<Vec<_>>(),
    })
}
fn strip_internal_context(text: &str) -> String {
    if text.trim_start().starts_with("[project:") {
        text.split_once('\n')
            .map(|(_, content)| content.trim().to_owned())
            .unwrap_or_default()
    } else {
        text.to_owned()
    }
}
fn parse_value(s: &str) -> Result<Value> {
    let trimmed = s.trim();
    let clean = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .strip_suffix("```")
        .unwrap_or(trimmed)
        .trim();
    serde_json::from_str(clean).context("model did not return valid JSON")
}
pub fn redact(text: &str) -> String {
    let patterns = [
        r"(?i)(api[_-]?key|token|password|secret)\s*[:=]\s*\S+",
        r"\b(sk-[A-Za-z0-9_-]{12,})\b",
        r"-----BEGIN [A-Z ]+PRIVATE KEY-----",
    ];
    let mut out = text.to_owned();
    for p in patterns {
        out = Regex::new(p)
            .unwrap()
            .replace_all(&out, "[REDACTED]")
            .into_owned()
    }
    out
}
fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hides_tokens() {
        assert!(!redact("api_key=abcd1234").contains("abcd1234"));
    }
    #[test]
    fn removes_internal_project_markers() {
        assert_eq!(
            strip_internal_context("[project: orchrd]\nReady to work."),
            "Ready to work."
        );
    }
    #[test]
    fn routing_context_excludes_goals_and_herdr_internals() {
        let projects = vec![Project {
            id: "orchrd".into(),
            name: "Orchrd".into(),
            path: "/private/project/path".into(),
            goal: "A potentially biasing project description".into(),
        }];
        let sessions = vec![AgentSession {
            project_id: "orchrd".into(),
            workspace_id: "w1".into(),
            pane_id: "w1:p1".into(),
            agent_name: "internal_agent_name".into(),
            status: "working".into(),
        }];
        let serialized = routing_context(&projects, &sessions, Some("orchrd")).to_string();
        assert!(serialized.contains("orchrd"));
        assert!(serialized.contains("working"));
        assert!(!serialized.contains("biasing"));
        assert!(!serialized.contains("private"));
        assert!(!serialized.contains("internal_agent_name"));
        assert!(!serialized.contains("w1:p1"));
    }
    #[test]
    fn system_prompt_explicitly_routes_agent_requests_to_send_tool() {
        let client = IntentClient {
            http: Client::new(),
            base_url: "http://localhost".into(),
            model: "test".into(),
            api_key: "test".into(),
        };
        let messages = client.conversation_messages(&[], &[], &[], None);
        let system = messages[0]["content"].as_str().unwrap();
        assert!(system.contains("MANDATORY TOOL ROUTING CONTRACT"));
        assert!(system.contains(
            "If the user asks you to ask, tell, send, message, instruct, or have a coding agent"
        ));
        assert!(system.contains("call send_to_agent"));
        assert!(system.contains("Do not answer the question yourself"));
    }
}
