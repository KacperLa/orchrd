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
    pub agents: Agents,
    pub intent: Intent,
    pub routing: Routing,
    pub conversation: Conversation,
    pub voice: Voice,
    pub stt: Stt,
    pub tts: Tts,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Agents {
    pub backend: AgentBackend,
    pub herdr_command: String,
    pub herdr_session: String,
    pub screen_lines: usize,
    pub poll_seconds: u64,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentBackend {
    #[default]
    Bridge,
    Herdr,
}

impl Default for Agents {
    fn default() -> Self {
        Self {
            backend: AgentBackend::Bridge,
            herdr_command: "herdr".into(),
            herdr_session: "default".into(),
            screen_lines: 30,
            poll_seconds: 2,
        }
    }
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
            endpoint: "http://127.0.0.1:8009/v1/systemone".into(),
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Stt {
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
        validate_url(&self.base_url, &format!("{name}.api.base_url"))?;
        validate_env_name(&self.api_key_env, &format!("{name}.api.api_key_env"))?;
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
            agents: Agents::default(),
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
impl Default for Voice {
    fn default() -> Self {
        Self {
            announcement_mode: AnnouncementMode::Muted,
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
        if self.agents.herdr_command.trim().is_empty()
            || self.agents.herdr_session.trim().is_empty()
            || !(10..=200).contains(&self.agents.screen_lines)
            || !(1..=60).contains(&self.agents.poll_seconds)
        {
            bail!("agents requires a Herdr command/session, screen_lines 10..200 and poll_seconds 1..60")
        }
        validate_url(&self.intent.base_url, "intent.base_url")?;
        validate_url(&self.routing.endpoint, "routing.endpoint")?;
        validate_env_name(&self.intent.api_key_env, "intent.api_key_env")?;
        if self.intent.model.trim().is_empty() || self.intent.timeout_seconds == 0 {
            bail!("intent.model and a positive intent.timeout_seconds are required")
        }
        if self.routing.model.trim().is_empty() || self.routing.timeout_seconds == 0 {
            bail!("routing.model and a positive routing.timeout_seconds are required")
        }
        self.stt.api.validate("stt", false)?;
        if self.tts.provider == TtsProvider::OpenaiCompatible {
            self.tts.api.validate("tts", true)?;
        }
        Ok(())
    }
    pub fn api_key(&self) -> Result<String> {
        credential(&self.intent.api_key_env)
    }
    /// Check credentials and local assets before opening sockets or the microphone.
    pub fn validate_runtime(&self) -> Result<()> {
        self.validate()?;
        credential(&self.intent.api_key_env)?;
        credential(&self.stt.api.api_key_env)?;
        if self.tts.provider == TtsProvider::OpenaiCompatible {
            credential(&self.tts.api.api_key_env)?;
        } else {
            readable_asset(&self.tts.model_config, "tts.model_config")?;
            readable_asset(&self.tts.voice, "tts.voice")?;
        }
        Ok(())
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
    pub fn tts_socket(&self) -> PathBuf {
        self.tts
            .socket
            .clone()
            .unwrap_or_else(|| Config::runtime_dir().join("tts.sock"))
    }
}

fn validate_url(value: &str, name: &str) -> Result<()> {
    // Never include the supplied URL in errors: it may contain credentials.
    let url =
        reqwest::Url::parse(value).map_err(|_| anyhow::anyhow!("{name} must be an HTTP(S) URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("{name} must be an HTTP(S) URL without credentials, query or fragment")
    }
    Ok(())
}
fn validate_env_name(value: &str, name: &str) -> Result<()> {
    if !value.is_empty()
        && (!value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || value.as_bytes()[0].is_ascii_digit())
    {
        bail!("{name} must be an environment variable name or empty")
    }
    Ok(())
}
fn credential(name: &str) -> Result<String> {
    if name.is_empty() {
        return Ok(String::new());
    }
    let value =
        env::var(name).with_context(|| format!("environment variable {name} is not set"))?;
    if value.trim().is_empty()
        || reqwest::header::HeaderValue::from_str(&format!("Bearer {value}")).is_err()
    {
        bail!("environment variable {name} must contain a nonempty valid API credential")
    }
    Ok(value)
}
fn readable_asset(path: &Path, name: &str) -> Result<()> {
    use std::io::Read;
    let mut file = fs::File::open(path)
        .with_context(|| format!("{name} must point to a readable local file"))?;
    if !file.metadata()?.is_file() || file.read(&mut [0u8; 1])? == 0 {
        bail!("{name} must point to a nonempty local file")
    }
    Ok(())
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
    fn configured() -> Config {
        let mut c: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        c.intent.api_key_env.clear();
        c
    }
    #[test]
    fn intent_and_routing_reject_invalid_urls_timeouts_and_models() {
        for url in [
            "not a URL",
            "file:///tmp/test",
            "https://user:private@example.com/v1",
            "https://example.com/?key=private",
            "https://example.com/#fragment",
        ] {
            let mut c = configured();
            c.intent.base_url = url.into();
            let error = c.validate().unwrap_err().to_string();
            assert!(error.contains("intent.base_url"));
            assert!(!error.contains("private"));
            let mut c = configured();
            c.routing.endpoint = url.into();
            assert!(c
                .validate()
                .unwrap_err()
                .to_string()
                .contains("routing.endpoint"));
        }
        let mut c = configured();
        c.intent.timeout_seconds = 0;
        assert!(c.validate().is_err());
        let mut c = configured();
        c.intent.model = "  ".into();
        assert!(c.validate().is_err());
        let mut c = configured();
        c.routing.timeout_seconds = 0;
        assert!(c.validate().is_err());
        let mut c = configured();
        c.routing.model = "  ".into();
        assert!(c.validate().is_err());
        let mut c = configured();
        c.intent.api_key_env = "bad name".into();
        assert!(c.validate().is_err());
        assert_eq!(
            Routing::default().endpoint,
            "http://127.0.0.1:8009/v1/systemone"
        );
    }
    #[test]
    fn runtime_checks_credentials_and_nonempty_local_assets() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = configured();
        c.tts.model_config = dir.path().join("model.yaml");
        c.tts.voice = dir.path().join("voice.safetensors");
        assert!(c
            .validate_runtime()
            .unwrap_err()
            .to_string()
            .contains("tts.model_config"));
        fs::write(&c.tts.model_config, "model: local").unwrap();
        fs::write(&c.tts.voice, "").unwrap();
        assert!(c
            .validate_runtime()
            .unwrap_err()
            .to_string()
            .contains("tts.voice"));
        fs::write(&c.tts.voice, "voice").unwrap();
        c.validate_runtime().unwrap();
        c.intent.api_key_env = format!("ORCHRD_TEST_MISSING_{}", uuid::Uuid::new_v4().simple());
        assert!(c
            .validate_runtime()
            .unwrap_err()
            .to_string()
            .contains(&c.intent.api_key_env));
        c.intent.api_key_env.clear();
        c.stt.api.api_key_env = format!("ORCHRD_TEST_MISSING_{}", uuid::Uuid::new_v4().simple());
        assert!(c.validate_runtime().is_err());
        c.stt.api.api_key_env.clear();
        c.tts.provider = TtsProvider::OpenaiCompatible;
        c.tts.api = SpeechApi {
            base_url: "http://localhost/v1".into(),
            model: "tts".into(),
            voice: "test".into(),
            ..SpeechApi::default()
        };
        c.tts.voice = dir.path().join("absent");
        c.validate_runtime().unwrap();
        c.tts.api.api_key_env = format!("ORCHRD_TEST_MISSING_{}", uuid::Uuid::new_v4().simple());
        assert!(c.validate_runtime().is_err());
    }
    #[test]
    fn empty_and_invalid_credentials_fail_without_echoing_values() {
        let name = format!("ORCHRD_TEST_CREDENTIAL_{}", uuid::Uuid::new_v4().simple());
        for value in [" ", "secret\ninvalid"] {
            env::set_var(&name, value);
            let error = credential(&name).unwrap_err().to_string();
            assert!(error.contains(&name));
            if !value.trim().is_empty() {
                assert!(!error.contains(value));
                assert!(!error.contains("secret"));
            }
        }
        env::set_var(&name, "valid-key");
        assert_eq!(credential(&name).unwrap(), "valid-key");
        env::remove_var(&name);
        assert_eq!(credential("").unwrap(), "");
    }
    #[test]
    fn example_configuration_is_valid() {
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        config.validate().unwrap();
    }
    #[test]
    fn herdr_is_opt_in_and_observation_limits_are_validated() {
        assert_eq!(Config::default().agents.backend, AgentBackend::Bridge);
        let agents: Agents = toml::from_str("backend = 'herdr'").unwrap();
        assert_eq!(agents.backend, AgentBackend::Herdr);
        assert_eq!(agents.screen_lines, 30);
        for (lines, seconds) in [(0, 2), (201, 2), (30, 0), (30, 61)] {
            let mut config = configured();
            config.agents.screen_lines = lines;
            config.agents.poll_seconds = seconds;
            assert!(config.validate().is_err());
        }
        assert!(toml::from_str::<Agents>("backend = 'typo'").is_err());
    }

    #[test]
    fn defaults_are_stable() {
        let c = Config::default();
        assert_eq!(c.voice.announcement_mode, AnnouncementMode::Muted);
        assert!(c.stt.api.base_url.is_empty());
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
    fn stt_api_is_required_and_validated() {
        assert!(toml::from_str::<Stt>("provider = 'voxtype'").is_err());
        let mut c = Config::default();
        c.intent.base_url = "https://example.com/v1".into();
        c.intent.model = "router".into();
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
    fn removed_voice_options_are_rejected() {
        assert!(toml::from_str::<Voice>("max_recording_seconds = 10").is_err());
        assert!(toml::from_str::<Voice>("transcription_timeout_seconds = 10").is_err());
        assert!(toml::from_str::<Voice>("voxtype_state_file = '/tmp/state'").is_err());
    }
    #[test]
    fn queued_mode_is_read_as_muted_for_compatibility() {
        let voice: Voice = toml::from_str("announcement_mode = 'queued'").unwrap();
        assert_eq!(voice.announcement_mode, AnnouncementMode::Muted);
    }
}
