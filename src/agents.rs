//! Select a backend for agent observation and bound input delivery.
use crate::{
    agent::AgentInfo,
    agent_bridge::AgentBridge,
    config::{AgentBackend, Config},
    coordinator::Message,
    herdr::Herdr,
};
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use tokio::sync::mpsc;

#[derive(Clone)]
pub(crate) enum Agents {
    Bridge(AgentBridge),
    Herdr(Herdr),
}
impl Agents {
    pub fn new(config: &Config) -> Self {
        match config.agents.backend {
            AgentBackend::Bridge => Self::Bridge(AgentBridge::default()),
            AgentBackend::Herdr => Self::Herdr(Herdr::new(config.agents.clone())),
        }
    }
    pub fn herdr(&self) -> Option<&Herdr> {
        if let Self::Herdr(h) = self {
            Some(h)
        } else {
            None
        }
    }
    pub async fn serve(&self, messages: mpsc::Sender<Message>) -> Result<()> {
        match self {
            Self::Bridge(bridge) => bridge.serve(messages).await,
            Self::Herdr(_) => std::future::pending().await,
        }
    }
    pub async fn snapshot(&self) -> Result<Vec<AgentInfo>> {
        match self {
            Self::Bridge(b) => Ok(b.snapshot().await),
            Self::Herdr(h) => h.poll().await,
        }
    }
    pub async fn read(&self, id: &str) -> Result<String> {
        match self {
            Self::Bridge(b) => b.read(id).await,
            Self::Herdr(h) => h.read(id).await,
        }
    }
    pub async fn question(&self, id: &str) -> Option<crate::question::Question> {
        match self {
            Self::Bridge(b) => b.question(id).await,
            Self::Herdr(h) => h.question(id).await,
        }
    }
    pub async fn question_ids(&self) -> HashMap<String, String> {
        match self {
            Self::Bridge(b) => b.question_ids().await,
            Self::Herdr(h) => h.question_ids().await,
        }
    }
    pub async fn questions(&self) -> Value {
        match self {
            Self::Bridge(b) => b.questions().await,
            Self::Herdr(h) => h.routing_questions().await,
        }
    }
    pub async fn command(&self, expected: &AgentInfo, action: &str, value: Value) -> Result<()> {
        match self {
            Self::Bridge(b) => b.command(&expected.name, action, value).await,
            Self::Herdr(h) => h.command(expected, action, value).await,
        }
    }
}
