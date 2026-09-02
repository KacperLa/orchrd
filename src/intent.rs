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
    pub reply: Option<String>,
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
            r#"You are Orchard, a concise voice interface that orchestrates coding agents in Herdr.
Carry on a natural conversation and resolve references from recent dialogue, including references to things you previously said. Use tools whenever current Herdr or project information is needed. You may answer unrelated general questions directly. For detailed questions about a project, first read its agent output; if that is insufficient, send the coding agent a focused question. Do not pretend you inspected something unless you used a tool.
Use start_session when the user only wants a new empty coding session. Use resume_session when they want the most recent saved Codex conversation for that project. Use send_to_agent only when there is a concrete request or question to send.
The request passed to send_to_agent must be a faithful, cleaned interpretation of what the user said. Correct obvious speech-recognition errors and remove filler, but preserve the user's meaning, scope, uncertainty, and level of detail. Do not add implementation ideas, likely files or technologies, examples, rationale, priorities, acceptance criteria, or investigative steps that the user did not state. Resolve a pronoun or reference from recent dialogue only when its meaning is explicit; otherwise ask a short clarification. The routing context is only for choosing a tool and project. Never copy or infer requirements from it into the request.
When the user approves or denies a pending request, you MUST call respond_to_agent. Interpret natural replies such as approved, go ahead, allow it, deny it, or no from the recent conversation; do not use exact phrase matching. Never claim that a request was approved or denied unless respond_to_agent returned ok true. If the tool fails, briefly explain that the response was not accepted.
Keep spoken replies brief and natural, normally one or two sentences, with no Markdown, lists, headings, or internal metadata. Codex is the coding worker: delegate repository inspection and code changes instead of attempting them yourself. Ask a short clarification when project or intent cannot be inferred. Never invent project IDs. Risky instructions are separately approval-gated by Orchard.
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
                "tool_choice": "auto"
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
        let reply = message
            .get("content")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned);
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
        if reply.is_none() && tool_calls.is_empty() {
            bail!("conversational model returned neither speech nor a tool call")
        }
        Ok(ModelTurn {
            message,
            reply,
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
        {"type":"function","function":{"name":"list_projects","description":"List Orchard's discovered project IDs and names for routing. This intentionally omits project descriptions and paths so they cannot become inferred task requirements.","parameters":{"type":"object","properties":{},"additionalProperties":false}}},
        {"type":"function","function":{"name":"list_agents","description":"List current Herdr coding-agent sessions and states.","parameters":{"type":"object","properties":{},"additionalProperties":false}}},
        {"type":"function","function":{"name":"read_agent","description":"Read recent terminal output from a project's Herdr agent.","parameters":{"type":"object","properties":{"project_id":{"type":"string"},"lines":{"type":"integer","minimum":10,"maximum":300}},"required":["project_id"],"additionalProperties":false}}},
        {"type":"function","function":{"name":"start_session","description":"Create a new empty Codex session for a project without sending a task or creating a run.","parameters":{"type":"object","properties":{"project_id":{"type":"string"}},"required":["project_id"],"additionalProperties":false}}},
        {"type":"function","function":{"name":"resume_session","description":"Resume the most recent saved Codex session for a project through Herdr's generic agent start API.","parameters":{"type":"object","properties":{"project_id":{"type":"string"}},"required":["project_id"],"additionalProperties":false}}},
        {"type":"function","function":{"name":"send_to_agent","description":"Send a faithful cleaned interpretation of the user's concrete request to a project's Codex agent, starting one if necessary. The request may correct transcription errors and remove filler, but must not add guessed requirements, implementation ideas, examples, rationale, or acceptance criteria.","parameters":{"type":"object","properties":{"project_id":{"type":"string"},"request":{"type":"string","description":"A concise semantic transcription containing only what the user expressed, plus unambiguous reference resolution from recent dialogue."},"fresh_session":{"type":"boolean"}},"required":["project_id","request","fresh_session"],"additionalProperties":false}}},
        {"type":"function","function":{"name":"latest_result","description":"Get Orchard's latest completed or active task record for a project.","parameters":{"type":"object","properties":{"project_id":{"type":"string"}},"required":["project_id"],"additionalProperties":false}}},
        {"type":"function","function":{"name":"respond_to_agent","description":"Required operation for approving or denying a pending Orchard safety gate or blocked coding-agent confirmation. Call this for any natural-language approval or denial; do not merely reply that it was handled.","parameters":{"type":"object","properties":{"project_id":{"type":"string","description":"The project whose pending request the user is answering."},"approve":{"type":"boolean","description":"True to approve the request, false to deny it."}},"required":["project_id","approve"],"additionalProperties":false}}},
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
pub fn is_risky(text: &str) -> bool {
    Regex::new(r"(?i)\b(push|deploy|publish|force[- ]?push|delete|destroy|drop database|credential|secret|purchase|sudo|bypass)\b").unwrap().is_match(text)
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
    fn detects_risk() {
        assert!(is_risky("push it to origin"));
        assert!(!is_risky("fix the parser"));
    }
    #[test]
    fn hides_tokens() {
        assert!(!redact("api_key=abcd1234").contains("abcd1234"));
    }
    #[test]
    fn removes_internal_project_markers() {
        assert_eq!(
            strip_internal_context("[project: orchard]\nReady to work."),
            "Ready to work."
        );
    }
    #[test]
    fn routing_context_excludes_goals_and_herdr_internals() {
        let projects = vec![Project {
            id: "orchard".into(),
            name: "Orchard".into(),
            path: "/private/project/path".into(),
            goal: "A potentially biasing project description".into(),
        }];
        let sessions = vec![AgentSession {
            project_id: "orchard".into(),
            workspace_id: "w1".into(),
            pane_id: "w1:p1".into(),
            agent_name: "internal_agent_name".into(),
            status: "working".into(),
        }];
        let serialized = routing_context(&projects, &sessions, Some("orchard")).to_string();
        assert!(serialized.contains("orchard"));
        assert!(serialized.contains("working"));
        assert!(!serialized.contains("biasing"));
        assert!(!serialized.contains("private"));
        assert!(!serialized.contains("internal_agent_name"));
        assert!(!serialized.contains("w1:p1"));
    }
}
