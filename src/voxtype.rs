use anyhow::{bail, Context, Result};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    fs,
    process::Command,
    time::{sleep, Instant},
};

#[derive(Clone)]
pub struct Voxtype {
    command: String,
    state_file: PathBuf,
    runtime_dir: PathBuf,
    transcription_timeout: Duration,
}

impl Voxtype {
    pub fn new(
        command: String,
        state_file: PathBuf,
        runtime_dir: PathBuf,
        transcription_timeout_seconds: u64,
    ) -> Self {
        Self {
            command,
            state_file,
            runtime_dir,
            transcription_timeout: Duration::from_secs(transcription_timeout_seconds),
        }
    }
    async fn state(&self) -> String {
        fs::read_to_string(&self.state_file)
            .await
            .unwrap_or_else(|_| "stopped".into())
            .trim()
            .to_ascii_lowercase()
    }
    pub async fn start(&self, id: &str) -> Result<PathBuf> {
        let state = self.state().await;
        if state != "idle" && state != "stopped" && !state.is_empty() {
            bail!("Voxtype is busy ({state})");
        }
        fs::create_dir_all(&self.runtime_dir).await?;
        let output = self.runtime_dir.join(format!("utterance-{id}.txt"));
        let _ = fs::remove_file(&output).await;
        let status = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new(&self.command)
                .kill_on_drop(true)
                .args(["record", "start"])
                .arg(format!("--file={}", output.display()))
                .stdin(Stdio::null())
                .status(),
        )
        .await
        .context("Voxtype start command timed out")?
        .context("start Voxtype recording")?;
        if !status.success() {
            bail!("voxtype record start exited with {status}");
        }
        Ok(output)
    }
    pub async fn stop_and_wait(&self, output: &Path) -> Result<String> {
        let status = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new(&self.command)
                .kill_on_drop(true)
                .args(["record", "stop"])
                .stdin(Stdio::null())
                .status(),
        )
        .await
        .context("Voxtype stop command timed out")?
        .context("stop Voxtype recording")?;
        if !status.success() {
            bail!("voxtype record stop exited with {status}");
        }
        let deadline = Instant::now() + self.transcription_timeout;
        loop {
            if output.exists() {
                let text = fs::read_to_string(output).await?;
                let _ = fs::remove_file(output).await;
                let text = text.trim().to_owned();
                if text.is_empty() {
                    bail!("Voxtype returned an empty transcript")
                };
                return Ok(text);
            }
            if Instant::now() >= deadline {
                bail!("Voxtype transcription timed out");
            }
            sleep(Duration::from_millis(100)).await;
        }
    }
    pub async fn cancel(&self) -> Result<()> {
        let status = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new(&self.command)
                .kill_on_drop(true)
                .args(["record", "cancel"])
                .status(),
        )
        .await
        .context("Voxtype cancel command timed out")??;
        if !status.success() {
            bail!("voxtype cancel exited with {status}")
        };
        Ok(())
    }
}
