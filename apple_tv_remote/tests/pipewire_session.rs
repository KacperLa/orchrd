//! Integration against a private PipeWire session.
//!
//! The test starts its own `pipewire` and `wireplumber` with a temporary
//! runtime directory, so it never touches the developer's audio session. It is
//! skipped when the daemons are not installed.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use apple_tv_remote::audio::Session as DecoderSession;
use apple_tv_remote::pipewire::{Audio, NODE_NAME};
use common::tone_packets;

struct PrivateSession {
    dir: tempfile::TempDir,
    children: Vec<Child>,
}

impl PrivateSession {
    fn start() -> Option<Self> {
        if !which("pipewire") || !which("wireplumber") {
            eprintln!("skipping: pipewire/wireplumber not installed");
            return None;
        }
        let dir = tempfile::tempdir().expect("temp dir");
        let mut children = Vec::new();
        for program in ["pipewire", "wireplumber"] {
            let child = Command::new(program)
                .env("XDG_RUNTIME_DIR", dir.path())
                .env("PIPEWIRE_RUNTIME_DIR", dir.path())
                .env_remove("PIPEWIRE_REMOTE")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            match child {
                Ok(child) => children.push(child),
                Err(error) => {
                    eprintln!("skipping: cannot start {program}: {error}");
                    return None;
                }
            }
            if !wait_for_socket(dir.path()) {
                eprintln!("skipping: {program} did not create its socket");
                return None;
            }
        }
        // PipeWire clients locate the server through this variable.
        std::env::set_var("XDG_RUNTIME_DIR", dir.path());
        std::env::set_var("PIPEWIRE_RUNTIME_DIR", dir.path());
        Some(Self { dir, children })
    }
}

impl Drop for PrivateSession {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = &self.dir;
    }
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|path| path.join(program).is_file()))
        .unwrap_or(false)
}

fn wait_for_socket(dir: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if dir.join("pipewire-0").exists() {
            // Give the server a moment to finish announcing its objects.
            std::thread::sleep(Duration::from_millis(300));
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Feed `frames` 20 ms tone frames at real-time cadence.
fn feed_realtime(decoder: &mut DecoderSession, frames: u64) {
    let packets = tone_packets(50);
    let start = Instant::now();
    let mut pushed = 0u64;
    while pushed < frames {
        let due = (start.elapsed().as_secs_f64() / 0.020) as u64;
        while pushed < due && pushed < frames {
            decoder.push_transport(
                (pushed % 50) as u16,
                &packets[(pushed % 50) as usize],
                Instant::now(),
            );
            pushed += 1;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Record from the remote microphone like Orchrd's `pw-record` consumer does.
struct Recorder {
    child: Child,
    path: PathBuf,
}

impl Recorder {
    fn start() -> Self {
        let path = std::env::temp_dir().join(format!(
            "apple-tv-remote-record-{}-{:.0}.raw",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_secs_f64()
                * 1000.0
        ));
        let child = Command::new("pw-record")
            .args(["--raw", "--rate=16000", "--channels=1", "--format=s16"])
            .arg("--target")
            .arg(NODE_NAME)
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("pw-record");
        Self { child, path }
    }

    fn stop(&mut self) -> u64 {
        let _ = self.child.kill();
        let _ = self.child.wait();
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }
}

/// One test owns the environment mutation, so tests do not race each other.
#[test]
fn microphone_source_is_published_selected_and_consumed() {
    let Some(_session) = PrivateSession::start() else {
        return;
    };
    let (mut decoder, playout) = DecoderSession::new().expect("decoder");
    let audio = Audio::spawn(playout).expect("spawn PipeWire");

    assert!(
        audio.wait_ready(Duration::from_secs(5)),
        "the microphone source was never published"
    );
    assert!(audio.shared.node_id() > 0);

    // Selecting our own node must be remembered by the session.
    audio.select_microphone().expect("queue selection");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !audio.shared.is_microphone_default() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        audio.shared.is_microphone_default(),
        "WirePlumber did not adopt {NODE_NAME} as the default input"
    );

    // The dictation flow: speech starts before the recording consumer is
    // attached, exactly as when Siri is held while Orchrd spawns pw-record.
    decoder.siri(true);
    feed_realtime(&mut decoder, 15);
    let buffered_before = audio.shared.link.buffered_samples();
    assert!(
        buffered_before > 0,
        "speech fed before the consumer attached was dropped"
    );

    let mut recorder = Recorder::start();
    std::thread::sleep(Duration::from_millis(300));
    feed_realtime(&mut decoder, 50);

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && audio.shared.link.buffered_samples() > 0 {
        std::thread::sleep(Duration::from_millis(20));
    }
    let recorded = recorder.stop();
    decoder.siri(false);
    audio.shutdown();

    let fed_ms = 65 * 20;
    let recorded_ms = recorded / 32;
    assert!(!decoder.stats_backlog_exceeded(), "live audio was dropped");
    assert!(
        recorded_ms + 32 >= fed_ms,
        "the consumer heard only {recorded_ms} ms of a {fed_ms} ms utterance"
    );
    assert!(
        recorded_ms <= fed_ms + 400,
        "the utterance was stretched to {recorded_ms} ms"
    );
    assert!(
        decoder.stats.peak > 1000,
        "decoded tone was silent: peak {}",
        decoder.stats.peak
    );
    let _ = std::fs::remove_file(&recorder.path);
}
