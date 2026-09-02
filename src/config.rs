use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub commands: Commands,
    pub herdr: Herdr,
    pub intent: Intent,
    pub conversation: Conversation,
    pub discovery: Discovery,
    pub voice: Voice,
    pub tts: Tts,
    pub summary: Summary,
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Discovery {
    pub roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Commands {
    pub voxtype: String,
    pub herdr: String,
    pub codex: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Herdr {
    pub session: String,
    pub poll_interval_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Intent {
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_intent_timeout_seconds")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Conversation {
    pub max_messages: usize,
    pub max_tool_rounds: usize,
    pub max_tool_output_chars: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Voice {
    pub announcement_mode: AnnouncementMode,
    pub max_recording_seconds: u64,
    pub voxtype_state_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnnouncementMode {
    Auto,
    Queued,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Tts {
    pub socket: Option<PathBuf>,
    pub language: String,
    pub model_config: PathBuf,
    pub voice: PathBuf,
    pub quantize: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Summary {
    pub cloud_context: String,
    pub max_terminal_lines: usize,
    pub include_diff: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub goal: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            commands: Commands::default(),
            herdr: Herdr::default(),
            intent: Intent {
                base_url: String::new(),
                model: String::new(),
                api_key_env: default_api_key_env(),
                timeout_seconds: default_intent_timeout_seconds(),
            },
            conversation: Conversation::default(),
            discovery: Discovery::default(),
            voice: Voice::default(),
            tts: Tts::default(),
            summary: Summary::default(),
            projects: vec![],
        }
    }
}
impl Default for Conversation {
    fn default() -> Self {
        Self {
            max_messages: 30,
            max_tool_rounds: 6,
            max_tool_output_chars: 6_000,
        }
    }
}
impl Default for Commands {
    fn default() -> Self {
        Self {
            voxtype: "voxtype".into(),
            herdr: "herdr".into(),
            codex: "codex".into(),
        }
    }
}
impl Default for Herdr {
    fn default() -> Self {
        Self {
            session: "orchrd".into(),
            poll_interval_ms: 2_000,
        }
    }
}
impl Default for Voice {
    fn default() -> Self {
        Self {
            announcement_mode: AnnouncementMode::Queued,
            max_recording_seconds: 60,
            voxtype_state_file: None,
        }
    }
}
impl Default for Tts {
    fn default() -> Self {
        Self {
            socket: None,
            language: "english_2026-04".into(),
            model_config: PathBuf::new(),
            voice: PathBuf::new(),
            quantize: true,
        }
    }
}
impl Default for Summary {
    fn default() -> Self {
        Self {
            cloud_context: "rich".into(),
            max_terminal_lines: 300,
            include_diff: true,
        }
    }
}
fn default_api_key_env() -> String {
    "ORCHRD_API_KEY".into()
}
fn default_intent_timeout_seconds() -> u64 {
    20
}

impl Config {
    pub fn default_path() -> Result<PathBuf> {
        Ok(ProjectDirs::from("io.github.kacper", "kacper", "orchrd")
            .context("could not resolve XDG config directory")?
            .config_dir()
            .join("config.toml"))
    }
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path.map(PathBuf::from).unwrap_or(Self::default_path()?);
        let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let mut cfg: Self =
            toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        cfg.validate()?;
        for project in &mut cfg.projects {
            project.path = project.path.canonicalize().with_context(|| {
                format!(
                    "canonicalize project {} at {}",
                    project.id,
                    project.path.display()
                )
            })?;
        }
        for root in &mut cfg.discovery.roots {
            *root = root
                .canonicalize()
                .with_context(|| format!("canonicalize discovery root {}", root.display()))?;
        }
        Ok(cfg)
    }
    pub fn validate(&self) -> Result<()> {
        if self.intent.base_url.is_empty() || self.intent.model.is_empty() {
            bail!("intent.base_url and intent.model are required")
        }
        if self.conversation.max_messages == 0 || self.conversation.max_tool_rounds == 0 {
            bail!("conversation.max_messages and max_tool_rounds must be greater than zero")
        }
        let mut ids = HashSet::new();
        for project in &self.projects {
            if project.id.is_empty()
                || !project
                    .id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
            {
                bail!(
                    "invalid project id {:?}; use lowercase letters, digits, '-' or '_'",
                    project.id
                );
            }
            if !ids.insert(&project.id) {
                bail!("duplicate project id {:?}", project.id);
            }
            if !project.path.is_dir() {
                bail!(
                    "project path is not a directory: {}",
                    project.path.display()
                );
            }
        }
        for root in &self.discovery.roots {
            if !root.is_dir() {
                bail!("discovery root is not a directory: {}", root.display());
            }
        }
        Ok(())
    }
    pub fn resolved_projects(&self) -> Result<Vec<Project>> {
        let mut projects = self.projects.clone();
        let mut ids: HashSet<String> = projects.iter().map(|p| p.id.clone()).collect();
        let explicit_paths: HashSet<PathBuf> = projects.iter().map(|p| p.path.clone()).collect();
        let mut discovered = Vec::new();

        for root in &self.discovery.roots {
            for entry in fs::read_dir(root)
                .with_context(|| format!("read discovery root {}", root.display()))?
            {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || !entry.path().is_dir() {
                    continue;
                }
                let path = entry.path().canonicalize()?;
                if explicit_paths.contains(&path) {
                    continue;
                }
                discovered.push((name, path));
            }
        }
        discovered.sort_by(|a, b| a.1.cmp(&b.1));

        for (name, path) in discovered {
            let base_id = project_id_from_name(&name);
            if base_id.is_empty() {
                continue;
            }
            let mut id = base_id.clone();
            let mut suffix = 2;
            while !ids.insert(id.clone()) {
                id = format!("{base_id}-{suffix}");
                suffix += 1;
            }
            projects.push(Project {
                id,
                goal: format!("Work on {name}"),
                name,
                path,
            });
        }
        Ok(projects)
    }
    pub fn api_key(&self) -> Result<String> {
        env::var(&self.intent.api_key_env).with_context(|| {
            format!(
                "environment variable {} is not set",
                self.intent.api_key_env
            )
        })
    }
    pub fn runtime_dir() -> PathBuf {
        env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/tmp/orchrd-{}", unsafe { libc_getuid() })))
            .join("orchrd")
    }
    pub fn data_dir() -> Result<PathBuf> {
        Ok(ProjectDirs::from("io.github.kacper", "kacper", "orchrd")
            .context("could not resolve XDG data directory")?
            .data_dir()
            .to_path_buf())
    }
    pub fn voxtype_state_file(&self) -> PathBuf {
        self.voice.voxtype_state_file.clone().unwrap_or_else(|| {
            Config::runtime_dir()
                .parent()
                .unwrap_or(Path::new("/tmp"))
                .join("voxtype/state")
        })
    }
    pub fn tts_socket(&self) -> PathBuf {
        self.tts
            .socket
            .clone()
            .unwrap_or_else(|| Config::runtime_dir().join("tts.sock"))
    }
}

fn project_id_from_name(name: &str) -> String {
    let mut id = String::new();
    let mut separator = false;
    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            id.push(ch);
            separator = false;
        } else if !id.is_empty() && !separator {
            id.push('-');
            separator = true;
        }
    }
    id.trim_end_matches('-').to_owned()
}

// Avoid adding a libc crate only to construct a last-resort temporary path.
#[cfg(unix)]
unsafe fn libc_getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}
#[cfg(not(unix))]
unsafe fn libc_getuid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_stable() {
        let c = Config::default();
        assert_eq!(c.herdr.session, "orchrd");
        assert_eq!(c.voice.announcement_mode, AnnouncementMode::Queued);
    }
    #[test]
    fn creates_safe_project_ids() {
        assert_eq!(project_id_from_name("AQEye"), "aqeye");
        assert_eq!(
            project_id_from_name("electrical_design"),
            "electrical-design"
        );
    }
    #[test]
    fn discovers_children_and_keeps_explicit_override() {
        let root = std::env::temp_dir().join(format!("orchrd-discovery-{}", uuid::Uuid::new_v4()));
        let aqeye = root.join("AQEye");
        let orchrd = root.join("orchrd");
        fs::create_dir_all(&aqeye).unwrap();
        fs::create_dir_all(&orchrd).unwrap();
        let mut config = Config::default();
        config.discovery.roots.push(root.clone());
        config.projects.push(Project {
            id: "orchrd".into(),
            name: "Custom Orchrd".into(),
            path: orchrd,
            goal: "Explicit goal".into(),
        });

        let projects = config.resolved_projects().unwrap();
        assert_eq!(projects.len(), 2);
        assert!(projects.iter().any(|p| p.id == "aqeye"));
        assert_eq!(
            projects.iter().find(|p| p.id == "orchrd").unwrap().name,
            "Custom Orchrd"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
