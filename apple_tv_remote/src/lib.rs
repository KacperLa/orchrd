//! Apple TV Siri Remote support: touchpad, buttons and Opus microphone.
//!
//! One process replaces the previous pair of Python daemons. A single HCI
//! monitor socket delivers both the HID input notifications and the Opus
//! microphone packets, so the touchpad, buttons and audio share one Bluetooth
//! capture path.

pub mod audio;
pub mod bluetooth;
pub mod gatt;
pub mod inspect;
pub mod layout;
pub mod lock;
pub mod mouse;
pub mod pipewire;
pub mod report;
pub mod touch;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::audio::{Session, Stats as AudioStats};
use crate::bluetooth::{connection_handle, parse_mac, CommandSocket, MonitorSocket};
use crate::mouse::VirtualMouse;
use crate::pipewire::Audio;
use crate::report::{
    connection_interval_ms, disconnection_handle, le_connection_handle, parse_report, AttAssembler,
    InputReport, HCI_MON_ACL_RX_PKT, HCI_MON_EVENT_PKT,
};
use crate::touch::{siri_pressed, MotionConfig, TouchPad};

/// Largest HCI monitor record we accept: ACL frames are bounded by the
/// controller's buffer size, well below this.
const MONITOR_BUFFER: usize = 65_541;
/// How long the packet loop waits for traffic before checking for a stop
/// request. Short enough to make shutdown prompt while idle.
const MONITOR_POLL: Duration = Duration::from_millis(200);
// The Siri release drain thread of the most recent release; a new release
// joins (supersedes) the previous one, and shutdown joins the last.
thread_local! {
    static DRAIN: RefCell<Option<JoinHandle<()>>> = const { RefCell::new(None) };
}

#[derive(Debug, Clone)]
pub struct Options {
    pub mac: String,
    /// HID device to send the report-rate unlock through, overriding discovery.
    pub hidraw: Option<PathBuf>,
    pub pointer: MotionConfig,
    /// How long the packet loop may wait while interpolated motion is in
    /// flight; also the pointer output rate's period.
    pub tick_interval: Duration,
    pub tune_link: bool,
    pub device: u16,
}

/// Run the daemon until SIGINT/SIGTERM.
pub fn run(options: Options) -> Result<()> {
    let mac = parse_mac(&options.mac)?;
    let _instance = lock::acquire()?;

    let (mut session, playout) = Session::new().context("create the Opus decoder")?;
    let mut audio = Audio::spawn(playout)?;
    if !audio.wait_ready(Duration::from_secs(5)) {
        bail!("the PipeWire microphone source did not start");
    }

    // BlueZ subscriptions and the HID feature report are prerequisites for the
    // touchpad; retried on their own thread because GATT objects and hidraw
    // nodes only appear once the remote is connected.
    let mut gatt = gatt::prepare_in_background(
        options.mac.clone(),
        options.hidraw.clone(),
        Duration::from_millis(500),
    );

    let mut mouse = TouchPad::new(VirtualMouse::create()?, options.pointer)
        .map_err(|error| anyhow::anyhow!(error))?;
    let stop = Arc::new(AtomicBool::new(false));
    install_signal_handlers(stop.clone())?;

    tracing::info!(
        node = pipewire::NODE_NAME,
        "Apple TV Remote Microphone created; hold Siri to speak"
    );

    // A raw HCI socket is used once per connection to shorten the link
    // interval; the monitor itself only consumes packets.
    let commands = CommandSocket::open(options.device).ok();

    let monitor = MonitorSocket::open()?;
    let mut buffer = vec![0u8; MONITOR_BUFFER];
    let mut report = Vec::with_capacity(report::MAX_REPORT);
    let mut assembler = AttAssembler::default();
    // The connection-complete event is only seen if the remote connects while
    // the daemon runs, so an already-connected remote is looked up directly.
    let mut handle: Option<u16> = match connection_handle(options.device, &mac) {
        Ok(Some(handle)) => {
            tracing::info!(handle, "remote is already connected");
            Some(handle)
        }
        Ok(None) => {
            tracing::info!("remote is asleep; waiting for a connection");
            None
        }
        Err(error) => {
            tracing::warn!(%error, "cannot read the Bluetooth connection list");
            None
        }
    };
    let mut tuned = false;
    let mut pressed = false;

    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            // A dead PipeWire stream cannot be revived in this process; exit so
            // the service supervisor restarts the daemon with a fresh stream,
            // as the previous pw-cat design did by dying with its child.
            if audio.shared.is_fatal() {
                bail!("the PipeWire microphone stream failed");
            }
            // Interpolated pointer motion is emitted between reports, so the
            // poll doubles as the interpolation tick while travel is queued
            // and relaxes to the shutdown-check interval when it is not.
            mouse.tick(Instant::now());
            let timeout = if mouse.interpolation_pending() {
                options.tick_interval
            } else {
                MONITOR_POLL
            };
            let Some(header) = monitor.recv(&mut buffer, timeout)? else {
                continue;
            };
            // The monitor socket is bound to every controller; a second adapter
            // would otherwise inject its traffic into this one's report stream.
            if header.index != options.device {
                continue;
            }
            let payload = monitor.payload(&buffer, header);
            match header.opcode {
                HCI_MON_EVENT_PKT => {
                    if let Some(disconnected) = disconnection_handle(payload) {
                        if handle == Some(disconnected) {
                            tracing::info!("remote disconnected; waiting for reconnection");
                            handle = None;
                            tuned = false;
                            assembler = AttAssembler::default();
                            mouse.release();
                            // A press that ends with the connection ends the
                            // capture session rather than leaving it open.
                            on_siri(false, &mut session, &mut pressed);
                        }
                        continue;
                    }
                    if let Some(connected) = le_connection_handle(payload, &mac) {
                        handle = Some(connected);
                        tuned = false;
                        assembler = AttAssembler::default();
                        tracing::info!(handle = connected, "remote connected");
                    }
                    if let Some(handle) = handle {
                        tune(&commands, &mut tuned, handle, options.tune_link);
                        if let Some((interval, status)) = connection_interval_ms(payload, handle) {
                            tracing::info!(
                                interval_ms = interval,
                                status,
                                "Bluetooth connection interval"
                            );
                        }
                    }
                }
                HCI_MON_ACL_RX_PKT => {
                    let Some(handle) = handle else {
                        continue;
                    };
                    // Other devices share the controller: filter theirs before
                    // reassembly, or their fragments would reset ours mid-PDU.
                    if report::acl_handle(payload) != Some(handle) {
                        continue;
                    }
                    let Some(connection) = assembler.feed(payload, &mut report) else {
                        continue;
                    };
                    if connection != handle {
                        continue;
                    }
                    match parse_report(&report) {
                        Some(InputReport::Buttons(report)) => {
                            // The click and Play/Pause/Menu bits live in the
                            // same report as the Siri bit.
                            mouse.process(report, Instant::now());
                            on_siri(siri_pressed(report), &mut session, &mut pressed);
                        }
                        Some(InputReport::Opus { sequence, payload }) => {
                            session.push_transport(sequence, payload, Instant::now());
                            // A full ring means speech older than the backlog
                            // bound is still waiting for a consumer that is
                            // gone; recording on would silently truncate.
                            if session.stats_backlog_exceeded() {
                                bail!("the remote audio backlog exceeds the two-minute limit");
                            }
                        }
                        Some(InputReport::Touch(touch)) => {
                            mouse.process(touch, Instant::now());
                        }
                        None => {}
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })();

    // Cleanup: end any open capture session, then release held buttons and
    // take down the microphone source. The final release drain is joined so
    // its summary is logged before the source disappears.
    on_siri(false, &mut session, &mut pressed);
    DRAIN.with(|slot| {
        if let Some(thread) = slot.borrow_mut().take() {
            let _ = thread.join();
        }
    });
    mouse.release();
    gatt.stop();
    let stats = session.stats;
    let touch_stats = mouse.stats;
    let link = audio.shared.link.clone();
    audio.shutdown();
    print_stats(stats, &link, pressed);
    tracing::info!(
        reports = touch_stats.reports,
        duplicates = touch_stats.duplicates,
        moves = touch_stats.moves,
        clicks = touch_stats.clicks,
        "touchpad stopped"
    );
    result
}

/// Apply a Siri button state to the capture session on the transitions.
/// `pressed` mirrors the transport state for the summary.
fn on_siri(down: bool, session: &mut Session, pressed: &mut bool) {
    let Some(down) = session.siri(down) else {
        return;
    };
    *pressed = down;
    if down {
        tracing::info!("Siri pressed: receiving audio");
    } else {
        // Let late Bluetooth packets and the final PipeWire buffers deliver
        // the tail before the session is reported over. The wait runs off the
        // capture loop, so monitor packets keep being read while it lasts.
        let queued_ms = session.link_queued_ms();
        let link = session.link();
        let handle = std::thread::Builder::new()
            .name("siri-drain".into())
            .spawn(move || log_drain(link, queued_ms));
        match handle {
            Ok(thread) => DRAIN.with(|slot| {
                if let Some(previous) = slot.borrow_mut().replace(thread) {
                    // A new release supersedes the previous drain.
                    let _ = previous.join();
                }
            }),
            Err(error) => {
                tracing::warn!(%error, "cannot start the Siri drain; ending the capture anyway")
            }
        }
    }
}

/// Log the release drain's outcome, as the Python daemon's dictation drain did.
fn log_drain(link: Arc<audio::Link>, queued_ms: u64) {
    let released = Instant::now();
    if link.wait_drained(audio::DRAIN_BOUND, audio::DRAIN_POLL) {
        tracing::info!(
            queued_ms,
            wait_s = released.elapsed().as_secs_f32(),
            "Siri audio drained"
        );
    } else {
        tracing::warn!(
            queued_ms,
            wait_s = released.elapsed().as_secs_f32(),
            "Siri audio did not drain; ending the capture anyway"
        );
    }
}

fn tune(commands: &Option<CommandSocket>, tuned: &mut bool, handle: u16, enabled: bool) {
    if *tuned || !enabled {
        return;
    }
    *tuned = true;
    match commands {
        Some(socket) => match socket.tune_link(handle) {
            Ok(()) => tracing::info!(handle, "requested a 7.5-15 ms Bluetooth interval"),
            Err(error) => tracing::warn!(%error, "Bluetooth interval update failed"),
        },
        None => tracing::debug!("no HCI command socket; interval left unchanged"),
    }
}

fn print_stats(stats: AudioStats, link: &audio::Link, pressed: bool) {
    tracing::info!(
        frames = stats.frames,
        errors = stats.errors,
        sequence_skips = stats.sequence_skips,
        concealed = stats.concealed,
        underruns = link.underruns(),
        peak = stats.peak,
        pressed,
        "microphone stopped"
    );
}

fn install_signal_handlers(stop: Arc<AtomicBool>) -> Result<()> {
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, stop.clone()).context("install signal handler")?;
    }
    Ok(())
}

/// Warn when the remote's HID node is absent; the daemon still waits for a
/// connection, so this is diagnostic only.
pub fn warn_if_unpaired(mac: &str) -> Result<()> {
    if let Err(error) = bluetooth::find_hidraw(mac) {
        tracing::warn!(%error, "remote HID endpoint not found");
    }
    Ok(())
}
