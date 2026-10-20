use crate::{config::Config, speech_api::AudioApi};
use anyhow::{bail, Context, Result};
use std::{process::Stdio, sync::Arc};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    sync::{oneshot, Mutex},
    task::JoinHandle,
};

// Two minutes of 16 kHz, mono, signed 16-bit PCM. Audio never touches disk.
const MAX_AUDIO_BYTES: usize = 120 * 16_000 * 2;

#[derive(Clone)]
pub(crate) struct SttClient {
    api: AudioApi,
    capture: Arc<Mutex<Option<Capture>>>,
}

struct Capture {
    token: String,
    stop: Option<oneshot::Sender<()>>,
    reader: JoinHandle<Result<Vec<u8>>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.reader.abort();
        // Child also has kill_on_drop enabled.
    }
}

impl SttClient {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            api: AudioApi::new(&config.stt.api)?,
            capture: Arc::new(Mutex::new(None)),
        })
    }

    pub async fn start(&self, id: &str) -> Result<()> {
        // Check credentials before opening the microphone.
        let _ = self.api.post("transcriptions")?;
        let mut active = self.capture.lock().await;
        if active.is_some() {
            bail!("previous recording is still being processed");
        }
        let mut child = Command::new("pw-record")
            .args(["--raw", "--rate=16000", "--channels=1", "--format=s16", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("start PipeWire microphone recording")?;
        let stdout = child.stdout.take().context("recording pipe unavailable")?;
        let (stop, stopped) = oneshot::channel();
        let reader = tokio::spawn(async move {
            let mut audio = Vec::new();
            let mut stdout = stdout.take((MAX_AUDIO_BYTES + 1) as u64);
            {
                let read = stdout.read_to_end(&mut audio);
                tokio::pin!(read);
                tokio::select! {
                    result = &mut read => { result?; child.kill().await?; },
                    _ = stopped => {
                        tokio::try_join!(stop_recorder(&mut child), async {
                            read.await?;
                            Ok::<_, anyhow::Error>(())
                        })?;
                    },
                    _ = tokio::time::sleep(std::time::Duration::from_secs(120)) => {
                        child.kill().await?;
                        bail!("recording exceeds the two-minute limit; please try a shorter utterance");
                    }
                }
            }
            if audio.len() > MAX_AUDIO_BYTES {
                bail!("recording exceeds the two-minute limit; please try a shorter utterance");
            }
            Ok(audio)
        });
        *active = Some(Capture {
            token: id.to_owned(),
            stop: Some(stop),
            reader,
        });
        Ok(())
    }

    /// Stop microphone capture at key release, before waiting for the turn queue.
    pub async fn stop_capture(&self) -> Result<()> {
        if let Some(capture) = self.capture.lock().await.as_mut() {
            if let Some(stop) = capture.stop.take() {
                let _ = stop.send(());
            }
        }
        Ok(())
    }

    pub async fn stop_and_wait(&self, token: &str) -> Result<String> {
        let mut active = self.capture.lock().await;
        if active.as_ref().map(|c| c.token.as_str()) != Some(token) {
            bail!("recording no longer exists");
        }
        let mut capture = active.take().unwrap();
        drop(active);
        if let Some(stop) = capture.stop.take() {
            let _ = stop.send(());
        }
        let pcm = (&mut capture.reader)
            .await
            .context("audio capture task failed")??;
        transcribe(&self.api, &pcm).await
    }

    pub async fn cancel(&self) -> Result<()> {
        // Dropping the capture aborts its task and kills its recording process.
        self.capture.lock().await.take();
        Ok(())
    }
}

async fn stop_recorder(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_none() {
        if let Some(pid) = child.id() {
            // Like Ctrl-C: let pw-record flush its last audio buffer. The owned,
            // unreaped child cannot have its PID reused while this signal is sent.
            let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
            if result != 0 {
                return Err(std::io::Error::last_os_error()).context("stop microphone recording");
            }
        }
        match tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await {
            Ok(status) => {
                status?;
            }
            Err(_) => {
                child.kill().await?;
            }
        }
    }
    Ok(())
}

async fn transcribe(api: &AudioApi, pcm: &[u8]) -> Result<String> {
    let wav = pcm_to_wav(pcm)?;
    let form = reqwest::multipart::Form::new()
        .text("model", api.settings.model.clone())
        .text("response_format", "json")
        .part(
            "file",
            reqwest::multipart::Part::bytes(wav)
                .file_name("utterance.wav")
                .mime_str("audio/wav")?,
        );
    let response = AudioApi::send(api.post("transcriptions")?.multipart(form)).await?;
    let value: serde_json::Value = response
        .json()
        .await
        .context("invalid speech API transcript response")?;
    let text = value["text"].as_str().unwrap_or_default().trim();
    if text.is_empty() {
        bail!("speech API returned an empty transcript");
    }
    Ok(text.to_owned())
}

fn pcm_to_wav(pcm: &[u8]) -> Result<Vec<u8>> {
    if pcm.is_empty() {
        bail!("microphone returned no audio; check the PipeWire input device");
    }
    if pcm.len() > MAX_AUDIO_BYTES || !pcm.len().is_multiple_of(2) {
        bail!("invalid PCM recording length");
    }
    let size = pcm.len() as u32;
    let mut wav = Vec::with_capacity(pcm.len() + 44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(size + 36).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&16_000u32.to_le_bytes());
    wav.extend_from_slice(&32_000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&size.to_le_bytes());
    wav.extend_from_slice(pcm);
    Ok(wav)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn compatible_transcription_uploads_wav_and_returns_clean_text() {
        let (settings, request) =
            crate::speech_api::tests::server("200 OK", r#"{"text":"  hello there  "}"#).await;
        let text = transcribe(&AudioApi::new(&settings).unwrap(), &[0, 1, 2, 3])
            .await
            .unwrap();
        assert_eq!(text, "hello there");
        let request = request.await.unwrap();
        let request = String::from_utf8_lossy(&request);
        assert!(request.starts_with("POST /v1/audio/transcriptions "));
        assert!(request.contains("multipart/form-data"));
        assert!(request.contains("filename=\"utterance.wav\""));
        assert!(request.contains("test-model"));
        assert!(request.contains("RIFF"));
        assert!(!request.to_lowercase().contains("authorization:"));
    }

    #[tokio::test]
    async fn invalid_transcripts_are_errors() {
        for body in [r#"{"text":" "}"#, r#"{"other":"hello"}"#, "not json"] {
            let (settings, request) = crate::speech_api::tests::server("200 OK", body).await;
            assert!(transcribe(&AudioApi::new(&settings).unwrap(), &[0, 0])
                .await
                .is_err());
            request.await.unwrap();
        }
    }
    #[test]
    fn wav_header_and_bounds() {
        let wav = pcm_to_wav(&[0, 1, 2, 3]).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(&wav[40..44], &4u32.to_le_bytes());
        assert_eq!(&wav[44..], &[0, 1, 2, 3]);
        assert!(pcm_to_wav(&[]).is_err());
        assert!(pcm_to_wav(&[0]).is_err());
        assert!(pcm_to_wav(&vec![0; MAX_AUDIO_BYTES + 2]).is_err());
    }
}
