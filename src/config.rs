use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub commands: Commands,
    pub intent: Intent,
    pub routing: Routing,
    pub conversation: Conversation,
    pub voice: Voice,
    pub stt: Stt,
    pub tts: Tts,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Commands {
    pub voxtype: String,
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

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Routing {
    #[serde(alias = "url")]
    pub endpoint: String,
    pub model: String,
    /// Empty means unauthenticated (for a local server).
    pub api_key: String,
    pub timeout_seconds: u64,
}
impl std::fmt::Debug for Routing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Routing")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("api_key", &"[redacted]")
            .field("timeout_seconds", &self.timeout_seconds)
            .finish()
    }
}
impl Default for Routing {
    fn default() -> Self {
        Self {
            endpoint: "http://192.168.1.104:8009/v1/systemone".into(),
            model: "kev-latest".into(),
            api_key: String::new(),
            timeout_seconds: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Conversation {
    // Accepted for compatibility with older configs; routing no longer replays history.
    pub max_messages: usize,
    pub max_tool_rounds: usize,
    pub max_tool_output_chars: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Voice {
    pub announcement_mode: AnnouncementMode,
    pub transcription_timeout_seconds: u64,
    pub voxtype_state_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnnouncementMode {
    Auto,
    #[serde(alias = "queued")]
    Muted,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Tts {
    pub provider: TtsProvider,
    pub api: SpeechApi,
    pub socket: Option<PathBuf>,
    pub language: String,
    pub model_config: PathBuf,
    pub voice: PathBuf,
    pub quantize: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TtsProvider {
    #[default]
    Pocket,
    OpenaiCompatible,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SttProvider {
    #[default]
    Voxtype,
    OpenaiCompatible,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Stt {
    pub provider: SttProvider,
    pub api: SpeechApi,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpeechApi {
    pub base_url: String,
    pub model: String,
    pub voice: String,
    /// Empty means unauthenticated (for a local server).
    pub api_key_env: String,
    pub timeout_seconds: u64,
}

impl Default for SpeechApi {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: String::new(),
            voice: String::new(),
            api_key_env: String::new(),
            timeout_seconds: 60,
        }
    }
}

impl SpeechApi {
    fn validate(&self, name: &str, needs_voice: bool) -> Result<()> {
        let url = reqwest::Url::parse(&self.base_url)
            .with_context(|| format!("{name}.api.base_url must be an HTTP(S) URL"))?;
        if !matches!(url.scheme(), "https" | "http")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!(
                "{name}.api.base_url must be an HTTP(S) URL without credentials, query or fragment"
            );
        }
        if self.model.trim().is_empty() || (needs_voice && self.voice.trim().is_empty()) {
            bail!("{name}.api.model (and for TTS, voice) are required");
        }
        if self.timeout_seconds == 0 {
            bail!("{name}.api.timeout_seconds must be greater than zero");
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            commands: Commands::default(),
            intent: Intent {
                base_url: String::new(),
                model: String::new(),
                api_key_env: default_api_key_env(),
                timeout_seconds: default_intent_timeout_seconds(),
            },
            routing: Routing::default(),
            conversation: Conversation::default(),
            voice: Voice::default(),
            stt: Stt::default(),
            tts: Tts::default(),
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
        }
    }
}
impl Default for Voice {
    fn default() -> Self {
        Self {
            announcement_mode: AnnouncementMode::Muted,
            transcription_timeout_seconds: 60,
            voxtype_state_file: None,
        }
    }
}
impl Default for Tts {
    fn default() -> Self {
        Self {
            socket: None,
            provider: TtsProvider::default(),
            api: SpeechApi::default(),
            language: "english_2026-04".into(),
            model_config: PathBuf::new(),
            voice: PathBuf::new(),
            quantize: true,
        }
    }
}
fn default_api_key_env() -> String {
    "ORCHRD_API_KEY".into()
}
fn default_intent_timeout_seconds() -> u64 {
    60
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
        let cfg: Self =
            toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }
    pub fn validate(&self) -> Result<()> {
        if self.intent.base_url.is_empty() || self.intent.model.is_empty() {
            bail!("intent.base_url and intent.model are required")
        }
        if self.routing.endpoint.is_empty()
            || self.routing.model.is_empty()
            || self.routing.timeout_seconds == 0
        {
            bail!("routing endpoint, model, and a positive timeout are required")
        }
        if self.voice.transcription_timeout_seconds == 0 {
            bail!("voice.transcription_timeout_seconds must be greater than zero")
        }
        if self.stt.provider == SttProvider::OpenaiCompatible {
            self.stt.api.validate("stt", false)?;
        }
        if self.tts.provider == TtsProvider::OpenaiCompatible {
            self.tts.api.validate("tts", true)?;
        }
        Ok(())
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
        assert_eq!(c.voice.announcement_mode, AnnouncementMode::Muted);
        assert_eq!(c.stt.provider, SttProvider::Voxtype);
        assert_eq!(c.tts.provider, TtsProvider::Pocket);
    }
    #[test]
    fn routing_config_accepts_api_details_and_preserves_old_configs() {
        let c: Config = toml::from_str(
            r#"
            [routing]
            url = "https://decisions.example/v1/systemone"
            model = "my-decision-model"
            api_key = "test-private-key"
        "#,
        )
        .unwrap();
        assert_eq!(c.routing.endpoint, "https://decisions.example/v1/systemone");
        assert_eq!(c.routing.model, "my-decision-model");
        assert_eq!(c.routing.api_key, "test-private-key");
        assert!(!format!("{c:?}").contains("test-private-key"));
        let old: Routing =
            toml::from_str("endpoint = 'http://localhost:8009/v1/systemone'\nmodel = 'kev'")
                .unwrap();
        assert!(old.api_key.is_empty());
        assert!(Routing::default().api_key.is_empty());
    }
    #[test]
    fn speech_provider_config_is_explicit_and_validated() {
        assert!(toml::from_str::<Stt>("provider = 'mystery'").is_err());
        let mut c = Config::default();
        c.intent.base_url = "https://example.com/v1".into();
        c.intent.model = "router".into();
        assert!(c.validate().is_ok());
        c.stt.provider = SttProvider::OpenaiCompatible;
        assert!(c.validate().is_err());
        c.stt.api.base_url = "http://localhost:8000/v1".into();
        c.stt.api.model = "transcriber".into();
        assert!(c.validate().is_ok());
        c.tts.provider = TtsProvider::OpenaiCompatible;
        c.tts.api = c.stt.api.clone();
        assert!(c.validate().is_err()); // voice required
        c.tts.api.voice = "test".into();
        assert!(c.validate().is_ok());
        c.tts.api.timeout_seconds = 0;
        assert!(c.validate().is_err());
        c.tts.api.timeout_seconds = 60;
        c.tts.api.base_url = "https://user:secret@example.com/v1".into();
        assert!(c.validate().is_err());
    }
    #[test]
    fn removed_options_are_rejected_and_transcription_timeout_is_explicit() {
        assert!(toml::from_str::<Voice>("max_recording_seconds = 10").is_err());
        let voice: Voice = toml::from_str("transcription_timeout_seconds = 10").unwrap();
        assert_eq!(voice.transcription_timeout_seconds, 10);
    }
    #[test]
    fn queued_mode_is_read_as_muted_for_compatibility() {
        let voice: Voice = toml::from_str("announcement_mode = 'queued'").unwrap();
        assert_eq!(voice.announcement_mode, AnnouncementMode::Muted);
    }
}
