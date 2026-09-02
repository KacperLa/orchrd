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
}
impl TtsClient {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }
    pub async fn speak(&self, id: &str, text: &str) -> Result<()> {
        self.request(json!({"op":"speak","id":id,"text":normalize_for_speech(text)}))
            .await
    }
    pub async fn cancel(&self) -> Result<()> {
        self.request(json!({"op":"cancel"})).await
    }
    async fn request(&self, value: serde_json::Value) -> Result<()> {
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
    let mut s = concise_speech(text).replace('_', " ");
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
pub fn concise_speech(text: &str) -> String {
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
        let input = "[project: orchard]\n# Options\n\n1. **Resume** the [agent](https://example.com).\n2. Start a new session.\n3. This sentence must not be spoken.";
        assert_eq!(
            concise_speech(input),
            "Options Resume the agent. Start a new session."
        );
    }
}
