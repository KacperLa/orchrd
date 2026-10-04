use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub display_name: String,
    #[serde(rename = "agent_name")]
    pub name: String,
    pub kind: Option<String>,
    pub instance_id: String,
    pub cwd: Option<PathBuf>,
    pub status: String,
    pub state_change_seq: Option<u64>,
}
