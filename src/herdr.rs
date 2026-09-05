use crate::{config::Project, db::AgentSession};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, process::Stdio};
use tokio::process::Command;
use tokio::time::{sleep, Duration};

const PANE_READY_RETRIES: usize = 40;
const PANE_READY_RETRY_DELAY: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct HerdrClient {
    command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HerdrSessionInfo {
    pub name: String,
    pub running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrAgentInfo {
    pub name: String,
    pub kind: Option<String>,
    pub workspace_id: String,
    pub pane_id: String,
    pub cwd: Option<PathBuf>,
    pub status: String,
    pub focused: bool,
}

impl HerdrClient {
    pub fn new(command: String) -> Self {
        Self { command }
    }
    async fn run(&self, session: &str, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.command)
            .arg("--session")
            .arg(session)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await
            .with_context(|| format!("run {} {}", self.command, args.join(" ")))?;
        if !out.status.success() {
            bail!(
                "herdr {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )
        };
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
    pub async fn list_sessions(&self) -> Result<Vec<HerdrSessionInfo>> {
        let out = Command::new(&self.command)
            .args(["session", "list", "--json"])
            .stdin(Stdio::null())
            .output()
            .await
            .with_context(|| format!("run {} session list", self.command))?;
        if !out.status.success() {
            bail!(
                "herdr session list failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        }
        let value: Value =
            serde_json::from_slice(&out.stdout).context("parse Herdr session list")?;
        let sessions = value
            .get("sessions")
            .and_then(Value::as_array)
            .context("Herdr session list omitted sessions")?;
        sessions
            .iter()
            .map(|session| {
                Ok(HerdrSessionInfo {
                    name: session
                        .get("name")
                        .and_then(Value::as_str)
                        .context("Herdr session omitted name")?
                        .to_owned(),
                    running: session
                        .get("running")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .collect()
    }
    pub async fn ping(&self, session: &str) -> Result<()> {
        self.run(session, &["api", "snapshot"]).await.map(|_| ())
    }
    pub async fn create_session(&self, session: &str, p: &Project) -> Result<AgentSession> {
        self.create_session_with_args(session, p, &[]).await
    }
    pub async fn resume_session(&self, session: &str, p: &Project) -> Result<AgentSession> {
        self.create_session_with_args(session, p, &["resume", "--last"])
            .await
    }
    async fn create_session_with_args(
        &self,
        session: &str,
        p: &Project,
        agent_args: &[&str],
    ) -> Result<AgentSession> {
        let raw = self
            .run(
                session,
                &[
                    "workspace",
                    "create",
                    "--cwd",
                    &p.path.to_string_lossy(),
                    "--label",
                    &p.name,
                    "--no-focus",
                ],
            )
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parse Herdr workspace response")?;
        let workspace =
            find_string(&v, "workspace_id").context("Herdr response omitted workspace_id")?;
        let pane = find_string(&v, "pane_id").context("Herdr response omitted pane_id")?;
        let agent = agent_name(&p.id);
        let mut start_args = vec!["agent", "start", &agent, "--kind", "codex", "--pane", &pane];
        if !agent_args.is_empty() {
            start_args.push("--");
            start_args.extend_from_slice(agent_args);
        }
        for attempt in 0..PANE_READY_RETRIES {
            match self.run(session, &start_args).await {
                Ok(_) => break,
                Err(error)
                    if attempt + 1 < PANE_READY_RETRIES && pane_is_still_starting(&error) =>
                {
                    sleep(PANE_READY_RETRY_DELAY).await;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(AgentSession {
            herdr_session: session.to_owned(),
            project_id: p.id.clone(),
            workspace_id: workspace,
            pane_id: pane,
            agent_name: agent,
            status: "idle".into(),
        })
    }
    pub async fn prompt(&self, session: &str, agent: &str, text: &str) -> Result<()> {
        self.run(session, &["agent", "prompt", agent, text])
            .await
            .map(|_| ())
    }
    pub async fn agent_list(&self, session: &str) -> Result<Value> {
        let raw = self.run(session, &["agent", "list"]).await?;
        serde_json::from_str(&raw).context("parse Herdr agent list")
    }
    pub async fn read(&self, session: &str, agent: &str, lines: usize) -> Result<String> {
        self.run(
            session,
            &[
                "agent",
                "read",
                agent,
                "--source",
                "recent-unwrapped",
                "--lines",
                &lines.to_string(),
                "--format",
                "text",
            ],
        )
        .await
    }
    pub async fn send_keys(&self, session: &str, agent: &str, keys: &[&str]) -> Result<()> {
        let mut args = vec!["agent", "send-keys", agent];
        args.extend(keys);
        self.run(session, &args).await.map(|_| ())
    }
}

fn pane_is_still_starting(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.contains("agent_pane_busy") || message.contains("not an available shell")
}

fn agent_name(id: &str) -> String {
    let clean: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let mut h = Sha256::new();
    h.update(id);
    let suffix = format!("{:x}", h.finalize());
    format!(
        "orchrd_{}_{}",
        clean.chars().take(12).collect::<String>(),
        &suffix[..6]
    )
    .chars()
    .take(32)
    .collect()
}
fn find_string(v: &Value, key: &str) -> Option<String> {
    match v {
        Value::Object(m) => m
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| m.values().find_map(|v| find_string(v, key))),
        Value::Array(a) => a.iter().find_map(|v| find_string(v, key)),
        _ => None,
    }
}
pub fn agent_states(v: &Value) -> Vec<(String, String)> {
    agent_infos(v)
        .into_iter()
        .map(|agent| (agent.name, agent.status))
        .collect()
}
pub fn agent_infos(v: &Value) -> Vec<HerdrAgentInfo> {
    let mut out = vec![];
    collect_agents(v, &mut out);
    out.sort_by(|left, right| left.pane_id.cmp(&right.pane_id));
    out.dedup_by(|left, right| left.pane_id == right.pane_id);
    out
}
fn collect_agents(v: &Value, out: &mut Vec<HerdrAgentInfo>) {
    match v {
        Value::Object(m) => {
            let name = m
                .get("name")
                .or_else(|| m.get("agent_name"))
                .and_then(Value::as_str);
            let status = m
                .get("state")
                .or_else(|| m.get("agent_status"))
                .and_then(Value::as_str);
            let pane = m.get("pane_id").and_then(Value::as_str);
            let workspace = m.get("workspace_id").and_then(Value::as_str);
            if let (Some(status), Some(pane_id), Some(workspace_id)) = (status, pane, workspace) {
                out.push(HerdrAgentInfo {
                    name: name.unwrap_or(pane_id).to_owned(),
                    kind: m.get("agent").and_then(Value::as_str).map(str::to_owned),
                    workspace_id: workspace_id.to_owned(),
                    pane_id: pane_id.to_owned(),
                    cwd: m
                        .get("foreground_cwd")
                        .or_else(|| m.get("cwd"))
                        .and_then(Value::as_str)
                        .map(PathBuf::from),
                    status: status.to_owned(),
                    focused: m.get("focused").and_then(Value::as_bool).unwrap_or(false),
                })
            }
            for x in m.values() {
                collect_agents(x, out)
            }
        }
        Value::Array(a) => {
            for x in a {
                collect_agents(x, out)
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_agent_names() {
        assert_eq!(agent_name("orchrd"), agent_name("orchrd"));
        assert!(agent_name("a-long-project-id").len() <= 32)
    }
    #[test]
    fn nested_ids() {
        let v = serde_json::json!({"result":{"workspace":{"workspace_id":"w1"}}});
        assert_eq!(find_string(&v, "workspace_id").as_deref(), Some("w1"));
    }
    #[test]
    fn reads_adoptable_agent_metadata() {
        let value = serde_json::json!({"result":{"agents":[{
            "name":"reviewer",
            "agent":"codex",
            "agent_status":"working",
            "workspace_id":"w2",
            "pane_id":"w2:p1",
            "foreground_cwd":"/projects/orchrd/src",
            "focused":true
        }]}});
        assert_eq!(
            agent_infos(&value),
            vec![HerdrAgentInfo {
                name: "reviewer".into(),
                kind: Some("codex".into()),
                workspace_id: "w2".into(),
                pane_id: "w2:p1".into(),
                cwd: Some("/projects/orchrd/src".into()),
                status: "working".into(),
                focused: true,
            }]
        );
    }
    #[test]
    fn detects_a_shell_that_is_still_starting() {
        let error = anyhow::anyhow!(
            "herdr agent start failed: agent_pane_busy: pane is not an available shell"
        );
        assert!(pane_is_still_starting(&error));
        assert!(!pane_is_still_starting(&anyhow::anyhow!("agent failed")));
    }
}
