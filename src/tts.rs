use crate::{
    config::{Config, TtsProvider},
    speech_api::AudioApi,
};
use anyhow::{bail, Context, Result};
use regex::Regex;
use serde_json::json;
use std::path::PathBuf;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

#[derive(Clone)]
pub struct TtsClient {
    socket: PathBuf,
    api: Option<AudioApi>,
    cancel: tokio::sync::watch::Sender<()>,
}
impl TtsClient {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            api: None,
            cancel: tokio::sync::watch::channel(()).0,
        }
    }
    pub fn from_config(config: &Config) -> Result<Self> {
        let mut client = Self::new(config.tts_socket());
        if config.tts.provider == TtsProvider::OpenaiCompatible {
            client.api = Some(AudioApi::new(&config.tts.api)?);
        }
        Ok(client)
    }
    pub async fn speak(&self, id: &str, text: &str) -> Result<()> {
        if let Some(api) = &self.api {
            let mut cancelled = self.cancel.subscribe();
            return tokio::select! {
                _ = cancelled.changed() => bail!("speech cancelled"),
                result = tokio::time::timeout(
                    std::time::Duration::from_secs(api.settings.timeout_seconds),
                    Self::stream_speech(api, text),
                ) => result.context("speech API playback timed out")?,
            };
        }
        self.request(json!({"op":"speak","id":id,"text":normalize_for_speech(text)}))
            .await
    }
    pub async fn cancel(&self) -> Result<()> {
        if self.api.is_some() {
            self.cancel.send_replace(());
            return Ok(());
        }
        self.request(json!({"op":"cancel"})).await
    }
    async fn stream_speech(api: &AudioApi, text: &str) -> Result<()> {
        let request = api.post("speech")?.json(&json!({
            "model": api.settings.model,
            "voice": api.settings.voice,
            "input": normalize_for_speech(text),
            "response_format": "pcm",
        }));
        let response = AudioApi::send(request).await?;
        let mut player = tokio::process::Command::new("pw-play");
        player.args(["--raw", "--rate=24000", "--channels=1", "--format=s16", "-"]);
        Self::play_stream(response, player).await
    }
    async fn play_stream(
        mut response: reqwest::Response,
        mut command: tokio::process::Command,
    ) -> Result<()> {
        if response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                !matches!(
                    v.split(';').next().unwrap_or("").trim(),
                    "application/octet-stream" | "audio/pcm" | "audio/raw" | "audio/x-pcm"
                )
            })
        {
            bail!("speech API returned an unsupported content type instead of PCM audio");
        }
        // The compatible PCM contract is mono s16le, 24 kHz. Dropping this future
        // kills the player immediately, including on PTT, timeout or HTTP failure.
        let mut player = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("start PipeWire speech playback")?;
        let mut input = player
            .stdin
            .take()
            .context("speech playback pipe unavailable")?;
        let mut bytes: usize = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .context("read speech API audio stream")?
        {
            bytes += chunk.len();
            input
                .write_all(&chunk)
                .await
                .context("play speech API audio")?;
        }
        drop(input);
        if bytes == 0 || !bytes.is_multiple_of(2) {
            bail!("speech API returned empty or invalid PCM audio");
        }
        let status = player.wait().await?;
        if !status.success() {
            bail!("PipeWire speech player exited with {status}");
        }
        Ok(())
    }
    async fn request(&self, value: serde_json::Value) -> Result<()> {
        let seconds = if value["op"] == "cancel" { 5 } else { 120 };
        tokio::time::timeout(
            std::time::Duration::from_secs(seconds),
            self.exchange(value),
        )
        .await
        .context("Pocket TTS request timed out")?
    }
    async fn exchange(&self, value: serde_json::Value) -> Result<()> {
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connect Pocket TTS worker at {}", self.socket.display()))?;
        stream.write_all(format!("{value}\n").as_bytes()).await?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await?;
        let response: serde_json::Value =
            serde_json::from_str(&line).context("parse Pocket TTS worker response")?;
        if response.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            Ok(())
        } else {
            bail!(
                "Pocket TTS worker error: {}",
                response
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error")
            )
        }
    }
}
pub fn normalize_for_speech(text: &str) -> String {
    let mut s = plain_speech(text).replace('_', " ");
    for (a, b) in [
        ("✅", "completed"),
        ("❌", "failed"),
        ("⚠️", "warning"),
        ("->", "to"),
    ] {
        s = s.replace(a, b)
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
pub fn plain_speech(text: &str) -> String {
    let project_tag = Regex::new(r"(?im)^\s*\[project:\s*[^\]]+\]\s*").unwrap();
    let markdown_link = Regex::new(r"\[([^\]]+)\]\([^\)]+\)").unwrap();
    let list_prefix = Regex::new(r"^\s*(?:[-*+]\s+|\d+[.)]\s+|#{1,6}\s*)").unwrap();
    let mut lines = vec![];
    let without_tags = project_tag.replace_all(text, "");
    for line in without_tags.lines() {
        let clean = list_prefix.replace(line, "");
        let clean = clean.trim();
        if !clean.is_empty() {
            lines.push(clean.to_owned());
        }
    }
    let flattened = lines.join(" ");
    let linked = markdown_link.replace_all(&flattened, "$1");
    let plain = linked
        .replace("**", "")
        .replace("__", "")
        .replace(['`', '*', '#'], "");
    let normalized = plain.split_whitespace().collect::<Vec<_>>().join(" ");

    normalized
}
pub fn concise_speech(text: &str) -> String {
    let normalized = plain_speech(text);
    let mut sentence_ends = 0;
    let mut end = normalized.len();
    for (index, ch) in normalized.char_indices() {
        if matches!(ch, '.' | '?' | '!') {
            sentence_ends += 1;
            if sentence_ends == 2 {
                end = index + ch.len_utf8();
                break;
            }
        }
    }
    let two_sentences = &normalized[..end];
    if two_sentences.chars().count() <= 360 {
        return two_sentences.to_owned();
    }
    let shortened: String = two_sentences.chars().take(357).collect();
    let boundary = shortened
        .rfind(char::is_whitespace)
        .unwrap_or(shortened.len());
    format!("{}...", shortened[..boundary].trim_end())
}
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn audio_reaches_player_before_http_response_finishes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (finish, finish_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n").await.unwrap();
            finish_rx.await.unwrap();
            stream.write_all(b"2\r\ncd\r\n0\r\n\r\n").await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audio.pcm");
        let mut player = tokio::process::Command::new("tee");
        player.arg(&path);
        let response = reqwest::get(url).await.unwrap();
        let playback = tokio::spawn(super::TtsClient::play_stream(response, player));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if tokio::fs::read(&path).await.unwrap_or_default() == b"ab" {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("first chunk should be played before the server sends EOF");
        assert!(!playback.is_finished());
        finish.send(()).unwrap();
        playback.await.unwrap().unwrap();
        server.await.unwrap();
        assert_eq!(tokio::fs::read(path).await.unwrap(), b"abcd");
    }
    #[tokio::test]
    async fn compatible_tts_requests_pcm_and_rejects_json() {
        let (settings, request) =
            crate::speech_api::tests::server("200 OK", r#"{"error":"not audio"}"#).await;
        let api = crate::speech_api::AudioApi::new(&settings).unwrap();
        assert!(super::TtsClient::stream_speech(&api, "hello")
            .await
            .unwrap_err()
            .to_string()
            .contains("instead of PCM"));
        let request = request.await.unwrap();
        let request = String::from_utf8_lossy(&request);
        assert!(request.starts_with("POST /v1/audio/speech "));
        assert!(request.contains(r#""response_format":"pcm""#));
        assert!(request.contains(r#""voice":"test-voice""#));
    }

    #[tokio::test]
    async fn remote_cancel_interrupts_pending_request_without_pocket_worker() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = crate::config::Config::default();
        config.tts.provider = crate::config::TtsProvider::OpenaiCompatible;
        config.tts.api.base_url = format!("http://{}", listener.local_addr().unwrap());
        let client = super::TtsClient::from_config(&config).unwrap();
        let speech_client = client.clone();
        let speech = tokio::spawn(async move { speech_client.speak("id", "hello").await });
        let (_connection, _) = listener.accept().await.unwrap();
        client.cancel().await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), speech)
            .await
            .unwrap()
            .unwrap();
        assert!(result.unwrap_err().to_string().contains("cancelled"));
    }
    use super::*;
    #[test]
    fn cleans_markdown() {
        assert_eq!(
            normalize_for_speech("**Done** in `src_a.rs`"),
            "Done in src a.rs"
        );
    }
    #[test]
    fn enforces_short_plain_speech() {
        let input = "[project: orchrd]\n# Options\n\n1. **Resume** the [agent](https://example.com).\n2. Start a new session.\n3. This sentence must not be spoken.";
        assert_eq!(
            concise_speech(input),
            "Options Resume the agent. Start a new session."
        );
    }
}
