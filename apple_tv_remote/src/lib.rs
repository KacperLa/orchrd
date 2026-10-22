//! Apple TV Siri Remote support: touchpad and buttons.
//!
//! One process replaces the previous Python mouse daemon. A single HCI monitor
//! socket delivers the HID input notifications, so the touchpad and buttons
//! share one Bluetooth capture path, and pointer motion is emitted through
//! uinput.

pub mod bluetooth;
pub mod gatt;
pub mod layout;
pub mod lock;
pub mod mouse;
pub mod report;
pub mod touch;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::bluetooth::{connection_handle, parse_mac, CommandSocket, MonitorSocket};
use crate::mouse::VirtualMouse;
use crate::report::{
    connection_interval_ms, disconnection_handle, le_connection_handle, parse_report, AttAssembler,
    InputReport, HCI_MON_ACL_RX_PKT, HCI_MON_EVENT_PKT,
};
use crate::touch::MotionConfig;

/// Largest HCI monitor record we accept: ACL frames are bounded by the
/// controller's buffer size, well below this.
const MONITOR_BUFFER: usize = 65_541;
/// How long the packet loop waits for traffic before checking for a stop
/// request. Short enough to make shutdown prompt while idle.
const MONITOR_POLL: Duration = Duration::from_millis(200);

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

    // BlueZ subscriptions and the HID feature report are prerequisites for the
    // touchpad; retried on their own thread because GATT objects and hidraw
    // nodes only appear once the remote is connected.
    let mut gatt = gatt::prepare_in_background(
        options.mac.clone(),
        options.hidraw.clone(),
        Duration::from_millis(500),
    );

    let mut mouse = touch::TouchPad::new(VirtualMouse::create()?, options.pointer)
        .map_err(|error| anyhow::anyhow!(error))?;
    let stop = Arc::new(AtomicBool::new(false));
    install_signal_handlers(stop.clone())?;

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

    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
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
                        Some(InputReport::Buttons(buttons)) => {
                            mouse.process(buttons, Instant::now())
                        }
                        Some(InputReport::Touch(touch)) => mouse.process(touch, Instant::now()),
                        None => {}
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })();

    mouse.release();
    gatt.stop();
    tracing::info!(
        reports = mouse.stats.reports,
        duplicates = mouse.stats.duplicates,
        moves = mouse.stats.moves,
        clicks = mouse.stats.clicks,
        "touchpad stopped"
    );
    result
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
