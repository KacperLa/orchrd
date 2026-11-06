//! PipeWire: the published microphone source and default-source selection.
//!
//! A dedicated thread owns the PipeWire main loop because PipeWire objects are
//! not `Send`. PipeWire drives the playout cadence: each process cycle copies
//! already-decoded PCM out of the shared ring buffer, so the daemon has no
//! audio timer of its own.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use pipewire as pw;
use pw::spa;
use spa::pod::Pod;

use crate::audio::{Link, Playout, SAMPLE_RATE};

pub const NODE_NAME: &str = "apple_remote_microphone";
pub const NODE_DESCRIPTION: &str = "Apple TV Remote Microphone";
/// WirePlumber stores the selected default input here as JSON.
pub const CONFIGURED_SOURCE_KEY: &str = "default.configured.audio.source";
pub const SOURCE_KEY: &str = "default.audio.source";
const METADATA_NAME: &str = "default";

/// State shared with the transport and dictation threads.
pub struct Shared {
    pub link: Arc<Link>,
    /// Set once the source node is streaming and can be selected.
    ready: AtomicBool,
    /// Set when the stream or the PipeWire server failed; the daemon exits so
    /// the service supervisor restarts it, like the previous pw-cat design.
    fatal: AtomicBool,
    node_id: AtomicU32,
    /// Current values of the `default` metadata object.
    properties: Mutex<HashMap<String, String>>,
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

    /// The node the session considers the default input, preferring the
    /// running selection over the stored preference.
    pub fn default_source(&self) -> Option<String> {
        let properties = self.properties.lock().ok()?;
        properties
            .get(SOURCE_KEY)
            .and_then(|value| json_name(value))
            .or_else(|| {
                properties
                    .get(CONFIGURED_SOURCE_KEY)
                    .and_then(|value| json_name(value))
            })
    }

    pub fn is_microphone_default(&self) -> bool {
        self.default_source().as_deref() == Some(NODE_NAME)
    }

    fn set_property(&self, key: &str, value: Option<&str>) {
        if let Ok(mut properties) = self.properties.lock() {
            match value {
                Some(value) => {
                    properties.insert(key.to_string(), value.to_string());
                }
                None => {
                    properties.remove(key);
                }
            }
        }
    }
}

/// Parse `{"name":"node"}` as written by WirePlumber.
pub fn json_name(value: &str) -> Option<String> {
    serde_json::from_str::<HashMap<String, String>>(value)
        .ok()
        .and_then(|value| value.get("name").cloned())
}

enum Command {
    /// Select a node by name as the configured default input.
    Select(String),
    Stop,
}

/// Handle to the PipeWire thread. Cloning shares the thread; the last handle
/// stops it.
#[derive(Clone)]
pub struct Audio {
    commands: Sender<Command>,
    thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    pub shared: Arc<Shared>,
}

impl Audio {
    pub fn spawn(playout: Playout) -> Result<Self> {
        let shared = Arc::new(Shared {
            link: playout.link().clone(),
            ready: AtomicBool::new(false),
            fatal: AtomicBool::new(false),
            node_id: AtomicU32::new(0),
            properties: Mutex::new(HashMap::new()),
        });
        let (commands, queue) = mpsc::channel();
        let state = shared.clone();
        let thread = std::thread::Builder::new()
            .name("pipewire".into())
            .spawn(move || run(queue, state, playout))
            .context("spawn PipeWire thread")?;
        Ok(Self {
            commands,
            thread: Arc::new(Mutex::new(Some(thread))),
            shared,
        })
    }

    pub fn select(&self, name: &str) -> Result<()> {
        self.commands
            .send(Command::Select(name.to_string()))
            .map_err(|_| anyhow!("PipeWire thread stopped"))
    }

    /// Handle suitable for another thread; shares this stream.
    pub fn clone_handle(&self) -> Self {
        self.clone()
    }

    /// Select the remote microphone as the configured default input.
    pub fn select_microphone(&self) -> Result<()> {
        self.select(NODE_NAME)
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

    /// Stop the PipeWire thread. Safe to call from any handle.
    pub fn shutdown(&self) {
        let _ = self.commands.send(Command::Stop);
        let thread = self.thread.lock().ok().and_then(|mut slot| slot.take());
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        // Only the final handle owns the join handle.
        if Arc::strong_count(&self.thread) == 1 {
            self.shutdown();
        }
    }
}

type DefaultMetadata = Rc<RefCell<Option<pw::metadata::Metadata>>>;

fn run(commands: mpsc::Receiver<Command>, shared: Arc<Shared>, playout: Playout) {
    pw::init();
    let metadata: DefaultMetadata = Rc::new(RefCell::new(None));
    let mainloop = match build(&shared, playout, &metadata) {
        Ok(mainloop) => mainloop,
        Err(error) => {
            tracing::error!(%error, "PipeWire microphone source unavailable");
            return;
        }
    };
    let quit = mainloop.clone();
    let state = shared.clone();
    let handle = metadata.clone();
    // A selection issued before the node or metadata is ready is kept and
    // retried, so a press during startup still selects the right input.
    let pending: RefCell<Option<String>> = RefCell::new(None);
    let timer = mainloop.loop_().add_timer(move |_| {
        loop {
            match commands.try_recv() {
                Ok(Command::Select(name)) => *pending.borrow_mut() = Some(name),
                Ok(Command::Stop) => {
                    quit.quit();
                    return;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    quit.quit();
                    return;
                }
            }
        }
        let pending_name = pending.borrow().clone();
        let Some(name) = pending_name else {
            return;
        };
        if !state.is_ready() {
            return;
        }
        match handle.borrow().as_ref() {
            Some(proxy) => match select_default(proxy, &name) {
                Ok(()) => {
                    tracing::info!(%name, "selected default input");
                    pending.borrow_mut().take();
                }
                Err(error) => tracing::warn!(%error, %name, "cannot select default input"),
            },
            None => tracing::debug!("PipeWire default metadata is not bound yet"),
        }
    });
    let _ = timer.update_timer(
        Some(Duration::from_millis(25)),
        Some(Duration::from_millis(25)),
    );
    mainloop.run();
}

fn select_default(metadata: &pw::metadata::Metadata, name: &str) -> Result<()> {
    let value = serde_json::json!({ "name": name }).to_string();
    metadata.set_property(
        0,
        CONFIGURED_SOURCE_KEY,
        Some("Spa:String:JSON"),
        Some(&value),
    );
    Ok(())
}

fn build(
    shared: &Arc<Shared>,
    playout: Playout,
    metadata: &DefaultMetadata,
) -> Result<pw::main_loop::MainLoopRc> {
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;
    // Bind WirePlumber's `default` metadata to track and change the default
    // input; this is the object behind `pw-metadata -n default`.
    let properties = shared.clone();
    let slot = metadata.clone();
    let binder = registry.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |object| {
            if object.type_ != pw::types::ObjectType::Metadata {
                return;
            }
            if object.props.and_then(|props| props.get("metadata.name")) != Some(METADATA_NAME) {
                return;
            }
            let Ok(proxy) = binder.bind::<pw::metadata::Metadata, _>(object) else {
                tracing::warn!("cannot bind the PipeWire default metadata");
                return;
            };
            let state = properties.clone();
            let listener = proxy
                .add_listener_local()
                .property(move |_, key, _, value| {
                    if let Some(key) = key {
                        state.set_property(key, value);
                    }
                    0
                })
                .register();
            // The listener must outlive the callback registration; the proxy
            // lives in `slot` for the whole process.
            std::mem::forget(listener);
            *slot.borrow_mut() = Some(proxy);
        })
        .register();
    // The listener must stay registered for the process lifetime; the
    // registry does not own it.
    std::mem::forget(_registry_listener);

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
                // is selectable and its node id known, but playout is left to
                // the consumer's cadence, as the Python daemon's pw-cat was.
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
