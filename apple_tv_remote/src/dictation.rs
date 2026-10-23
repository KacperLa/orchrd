//! Siri push-to-talk handling: Orchrd recording control and microphone
//! selection, driven by an actor so Bluetooth capture is never blocked.
//!
//! Holding Siri selects the remote microphone and starts an Orchrd recording;
//! releasing drains the audio, submits it, and restores the previous default
//! input unless the user changed it while dictating.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use zbus::Proxy;

use crate::audio::Link;
use crate::pipewire::Audio;
use std::sync::Arc;

const ORCHRD_NAME: &str = "io.github.kacper.Orchrd1";
const ORCHRD_PATH: &str = "/io/github/kacper/Orchrd1";
/// Time to let Bluetooth, the PCM ring and PipeWire deliver the tail.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Siri held: select the remote microphone and start recording.
    Begin,
    /// Siri released: submit the recording and restore the previous input.
    Submit,
    /// Daemon shutting down: cancel the recording and restore.
    Cancel,
}

/// Handle used by the transport loop; sending never blocks, so a stalled
/// Orchrd or a slow D-Bus call cannot delay Bluetooth capture.
pub struct Dictation {
    requests: Option<mpsc::Sender<Request>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Dictation {
    /// Start the actor on its own thread with its own runtime. Errors are
    /// reported through the log, never to the capture loop, so a missing
    /// Orchrd degrades to microphone-only use.
    pub fn spawn(audio: Audio) -> std::io::Result<Self> {
        let (requests, queue) = mpsc::channel(4);
        let thread = std::thread::Builder::new()
            .name("dictation".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::error!(%error, "cannot start the dictation thread");
                        return;
                    }
                };
                runtime.block_on(actor(queue, audio));
            })?;
        Ok(Self {
            requests: Some(requests),
            thread: Some(thread),
        })
    }

    pub fn begin(&self) {
        self.send(Request::Begin);
    }

    pub fn submit(&self) {
        self.send(Request::Submit);
    }

    /// Cancel any recording and wait for the actor to finish, so the
    /// microphone is only taken down after the previous input is restored.
    pub fn shutdown(&mut self) {
        self.send(Request::Cancel);
        // Dropping the sender ends the actor loop once it drains the queue.
        self.requests = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    fn send(&self, request: Request) {
        let Some(requests) = self.requests.as_ref() else {
            return;
        };
        // A full queue means a previous press is still being processed; a
        // dropped request leaves the recording in its previous state, which is
        // preferable to blocking the capture loop.
        if let Err(error) = requests.try_send(request) {
            tracing::warn!(?request, %error, "dictation request dropped");
        }
    }
}

impl Drop for Dictation {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn actor(mut queue: mpsc::Receiver<Request>, audio: Audio) {
    // The system bus is a system-wide resource; a failure here disables
    // dictation but leaves the microphone and touchpad working.
    let mut state = match DictationState::connect(&audio).await {
        Ok(state) => Some(state),
        Err(error) => {
            tracing::warn!(%error, "Orchrd dictation control unavailable");
            None
        }
    };
    while let Some(request) = queue.recv().await {
        let Some(dictation) = state.as_mut() else {
            continue;
        };
        match request {
            Request::Begin => {
                if let Err(error) = dictation.begin().await {
                    tracing::warn!(%error, "cannot start dictation");
                }
            }
            Request::Submit => dictation.finish(false).await,
            Request::Cancel => dictation.finish(true).await,
        }
    }
}

struct DictationState {
    connection: zbus::Connection,
    audio: Audio,
    link: Arc<Link>,
    /// Input selected before dictating, restored on release.
    previous_source: Option<String>,
    owned: bool,
}

impl DictationState {
    async fn connect(audio: &Audio) -> Result<Self> {
        // Failing now is not fatal: Orchrd may start later, so the connection
        // is established lazily on the first press.
        let connection = zbus::Connection::session()
            .await
            .context("connect to the session bus for Orchrd control")?;
        Ok(Self {
            connection,
            audio: audio.clone_handle(),
            link: audio.shared.link.clone(),
            previous_source: None,
            owned: false,
        })
    }

    async fn proxy(&self) -> Result<Proxy<'_>> {
        Proxy::new(&self.connection, ORCHRD_NAME, ORCHRD_PATH, ORCHRD_NAME)
            .await
            .context("Orchrd is not running")
    }

    async fn voice_state(&self) -> Result<String> {
        let snapshot: String = self.proxy().await?.call("GetSnapshot", &()).await?;
        let value: serde_json::Value = serde_json::from_str(&snapshot)?;
        Ok(value
            .get("voice_state")
            .and_then(|state| state.as_str())
            .unwrap_or("idle")
            .to_owned())
    }

    async fn call(&self, method: &str) -> Result<()> {
        self.proxy()
            .await?
            .call::<_, _, ()>(method, &())
            .await
            .with_context(|| format!("Orchrd {method} failed"))
    }

    async fn begin(&mut self) -> Result<()> {
        if self.voice_state().await? != "idle" {
            tracing::info!("Siri recording skipped: Orchrd is busy");
            return Ok(());
        }
        // Without a published source there is nothing to select, so the press
        // is ignored rather than recording the wrong input.
        if !self.audio.wait_ready(Duration::from_secs(2)) {
            anyhow::bail!("the remote microphone source is not published");
        }
        self.previous_source = self.audio.shared.default_source();
        self.audio.select_microphone()?;
        if let Err(error) = self.call("StartListening").await {
            self.restore();
            return Err(error);
        }
        self.owned = true;
        tracing::info!("Orchrd recording from Siri");
        Ok(())
    }

    async fn finish(&mut self, cancel: bool) {
        if !self.owned {
            self.restore();
            return;
        }
        if !cancel {
            // The tail must reach PipeWire before the recording is submitted,
            // or Orchrd stops reading just before the end of the utterance.
            self.drain().await;
        }
        let method = if cancel {
            "CancelListening"
        } else {
            "StopListening"
        };
        if let Err(error) = self.call(method).await {
            tracing::warn!(%error, "Orchrd recording completion failed");
        }
        self.owned = false;
        self.restore();
    }

    /// Wait for the decoded tail to reach PipeWire before submitting.
    async fn drain(&self) {
        tokio::time::sleep(DRAIN_GRACE).await;
        for _ in 0..10 {
            if self.link.buffered_samples() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn restore(&mut self) {
        let Some(previous) = self.previous_source.take() else {
            return;
        };
        // Preserve a manual input change made while dictating.
        if self.audio.shared.is_microphone_default() {
            if let Err(error) = self.audio.select(&previous) {
                tracing::warn!(%error, "cannot restore the previous input");
            }
        }
    }
}
