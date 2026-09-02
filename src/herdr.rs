use crate::{config::Project, db::AgentSession};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::{sleep, Duration};

const PANE_READY_RETRIES: usize = 40;
const PANE_READY_RETRY_DELAY: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct HerdrClient {
    command: String,
    session: String,
}
impl HerdrClient {
    pub fn new(command: String, session: String) -> Self {
        Self { command, session }
    }
    async fn run(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.command)
            .arg("--session")
            .arg(&self.session)
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
    pub async fn ping(&self) -> Result<()> {
        self.run(&["api", "snapshot"]).await.map(|_| ())
    }
    pub async fn create_session(&self, p: &Project) -> Result<AgentSession> {
        self.create_session_with_args(p, &[]).await
    }
    pub async fn resume_session(&self, p: &Project) -> Result<AgentSession> {
        self.create_session_with_args(p, &["resume", "--last"])
            .await
    }
    async fn create_session_with_args(
        &self,
        p: &Project,
        agent_args: &[&str],
    ) -> Result<AgentSession> {
        let raw = self
            .run(&[
                "workspace",
                "create",
                "--cwd",
                &p.path.to_string_lossy(),
                "--label",
                &p.name,
                "--no-focus",
            ])
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
            match self.run(&start_args).await {
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
            project_id: p.id.clone(),
            workspace_id: workspace,
            pane_id: pane,
            agent_name: agent,
            status: "idle".into(),
        })
    }
    pub async fn prompt(&self, agent: &str, text: &str) -> Result<()> {
        self.run(&["agent", "prompt", agent, text])
            .await
            .map(|_| ())
    }
    pub async fn agent_list(&self) -> Result<Value> {
        let raw = self.run(&["agent", "list"]).await?;
        serde_json::from_str(&raw).context("parse Herdr agent list")
    }
    pub async fn read(&self, agent: &str, lines: usize) -> Result<String> {
        self.run(&[
            "agent",
            "read",
            agent,
            "--source",
            "recent-unwrapped",
            "--lines",
            &lines.to_string(),
            "--format",
            "text",
        ])
        .await
    }
    pub async fn send_keys(&self, agent: &str, keys: &[&str]) -> Result<()> {
        let mut args = vec!["agent", "send-keys", agent];
        args.extend(keys);
        self.run(&args).await.map(|_| ())
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
    let mut out = vec![];
    collect_states(v, &mut out);
    out
}
fn collect_states(v: &Value, out: &mut Vec<(String, String)>) {
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
            if let (Some(n), Some(s)) = (name, status) {
                out.push((n.into(), s.into()))
            }
            for x in m.values() {
                collect_states(x, out)
            }
        }
        Value::Array(a) => {
            for x in a {
                collect_states(x, out)
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
    fn detects_a_shell_that_is_still_starting() {
        let error = anyhow::anyhow!(
            "herdr agent start failed: agent_pane_busy: pane is not an available shell"
        );
        assert!(pane_is_still_starting(&error));
        assert!(!pane_is_still_starting(&anyhow::anyhow!("agent failed")));
    }
}
