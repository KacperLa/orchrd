//! PipeWire: the published microphone source.
//!
//! A dedicated thread owns the PipeWire main loop because PipeWire objects are
//! not `Send`. PipeWire drives the playout cadence: each process cycle copies
//! already-decoded PCM out of the shared ring buffer, so the daemon has no
//! audio timer of its own.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use pipewire as pw;
use pw::spa;
use spa::pod::Pod;

use crate::audio::{Link, Playout, SAMPLE_RATE};

pub const NODE_NAME: &str = "apple_remote_microphone";
pub const NODE_DESCRIPTION: &str = "Apple TV Remote Microphone";

/// State shared with the transport thread.
pub struct Shared {
    pub link: Arc<Link>,
    /// Set once the source node is streaming and can be recorded from.
    ready: AtomicBool,
    /// Set when the stream or the PipeWire server failed; the daemon exits so
    /// the service supervisor restarts it, like the previous pw-cat design.
    fatal: AtomicBool,
    node_id: AtomicU32,
}

impl Shared {
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn is_fatal(&self) -> bool {
        self.fatal.load(Ordering::Acquire)
    }

    pub fn node_id(&self) -> u32 {
        self.node_id.load(Ordering::Relaxed)
    }
}

/// Handle to the PipeWire thread; dropping it stops the thread.
pub struct Audio {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub shared: Arc<Shared>,
}

impl Audio {
    pub fn spawn(playout: Playout) -> Result<Self> {
        let shared = Arc::new(Shared {
            link: playout.link().clone(),
            ready: AtomicBool::new(false),
            fatal: AtomicBool::new(false),
            node_id: AtomicU32::new(0),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let state = shared.clone();
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("pipewire".into())
            .spawn(move || run(state, playout, flag))
            .context("spawn PipeWire thread")?;
        Ok(Self {
            stop,
            thread: Some(thread),
            shared,
        })
    }

    /// Wait until the source node is published and streaming. A stream or
    /// server failure ends the wait early: the caller cannot recover in this
    /// process, so it should exit and let the supervisor restart it.
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.shared.is_ready() {
                return true;
            }
            if self.shared.is_fatal() {
                return false;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Stop the PipeWire thread.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(shared: Arc<Shared>, playout: Playout, stop: Arc<AtomicBool>) {
    pw::init();
    let mainloop = match build(&shared, playout) {
        Ok(mainloop) => mainloop,
        Err(error) => {
            tracing::error!(%error, "PipeWire microphone source unavailable");
            return;
        }
    };
    // The main loop stays on its thread, so shutdown is requested through a
    // flag polled by a short timer.
    let quit = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop.load(Ordering::Acquire) {
            quit.quit();
        }
    });
    let _ = timer.update_timer(
        Some(Duration::from_millis(25)),
        Some(Duration::from_millis(25)),
    );
    mainloop.run();
}

fn build(shared: &Arc<Shared>, playout: Playout) -> Result<pw::main_loop::MainLoopRc> {
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "apple-tv-remote-microphone",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Communication",
            // Publishing the stream as a source node makes the remote usable
            // as a normal default input, exactly like a capture device.
            *pw::keys::MEDIA_CLASS => "Audio/Source",
            *pw::keys::NODE_NAME => NODE_NAME,
            *pw::keys::NODE_DESCRIPTION => NODE_DESCRIPTION,
            *pw::keys::NODE_VIRTUAL => "true",
            *pw::keys::NODE_WANT_DRIVER => "true",
            // Without a driver the node pauses, so audio decoded before a
            // consumer attaches accumulates in the ring instead of draining
            // into a buffer nobody reads, exactly as the Python daemon's
            // pw-cat did by idling on its stdin pipe.
            *pw::keys::PRIORITY_SESSION => "1",
            *pw::keys::NODE_PAUSE_ON_IDLE => "false",
            *pw::keys::NODE_LATENCY => "640/16000",
        },
    )?;

    let playout = Mutex::new(playout);
    let state = shared.clone();
    let quit = mainloop.clone();
    let _stream_listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |stream, (), old, new| {
            match new {
                // Paused means published and driven-when-consumed: the source
                // can be recorded from and its node id known, but playout is
                // left to the consumer's cadence, as the Python daemon's
                // pw-cat was.
                pw::stream::StreamState::Paused => {
                    state.node_id.store(stream.node_id(), Ordering::Relaxed);
                    state.ready.store(true, Ordering::Release);
                    tracing::debug!(node_id = stream.node_id(), "microphone source published");
                }
                pw::stream::StreamState::Streaming => {
                    state.node_id.store(stream.node_id(), Ordering::Relaxed);
                    state.ready.store(true, Ordering::Release);
                    tracing::debug!(node_id = stream.node_id(), "microphone source streaming");
                }
                pw::stream::StreamState::Error(reason) => {
                    state.ready.store(false, Ordering::Release);
                    state.fatal.store(true, Ordering::Release);
                    tracing::error!(%reason, "PipeWire microphone stream failed");
                    quit.quit();
                }
                pw::stream::StreamState::Unconnected
                    // The server went away (logout, restart); retrying in this
                    // process cannot reach it, so let the supervisor restart us.
                    if !matches!(old, pw::stream::StreamState::Unconnected) =>
                {
                    state.ready.store(false, Ordering::Release);
                    state.fatal.store(true, Ordering::Release);
                    tracing::error!("PipeWire server connection lost");
                    quit.quit();
                }
                _ => {}
            }
        })
        .process(move |stream, ()| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let Some(target) = data.data() else {
                return;
            };
            let written = match playout.lock() {
                Ok(mut playout) => playout.fill(target),
                Err(_) => {
                    target.fill(0);
                    target.len()
                }
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = 2;
            *chunk.size_mut() = written as u32;
        })
        .register()?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(SAMPLE_RATE);
    audio_info.set_channels(1);
    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: audio_info.into(),
        }),
    )
    .map_err(|error| anyhow!("serialize audio format: {error:?}"))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).context("invalid audio format pod")?];
    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;
    // The stream and its listener must outlive this function; `StreamRc`
    // destroys the stream when dropped.
    std::mem::forget(_stream_listener);
    std::mem::forget(stream);
    Ok(mainloop)
}
