//! Apple TV Siri Remote connection setup.
//!
//! The remote's HID input reports (touchpad, buttons, microphone) all arrive
//! over one GATT characteristic that BlueZ's HID host only delivers once the
//! link is subscribed and unlocked. This daemon performs that setup and keeps
//! it alive across the remote's idle disconnects; it interprets no reports.

pub mod bluetooth;
pub mod gatt;
pub mod report;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::bluetooth::{parse_mac, MonitorSocket};

/// How long the packet loop waits for traffic before checking for a stop
/// request, so shutdown stays prompt while the remote is idle.
const MONITOR_POLL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
pub struct Options {
    pub mac: String,
}

/// Establish the connection setup and hold it until SIGINT/SIGTERM.
pub fn run(options: Options) -> Result<()> {
    parse_mac(&options.mac).context("--mac")?;
    let mut gatt = gatt::prepare_in_background(options.mac.clone(), Duration::from_millis(500));
    let monitor = MonitorSocket::open()?;
    let mut buffer = vec![0u8; report::MONITOR_BUFFER];
    let stop = install_signal_handlers()?;
    tracing::info!(mac = %options.mac, "waiting for the Apple TV remote");
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Some(_record) = monitor.recv(&mut buffer, MONITOR_POLL)? else {
            continue;
        };
    }
    gatt.stop();
    tracing::info!("stopped");
    Ok(())
}

fn install_signal_handlers() -> Result<Arc<AtomicBool>> {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, stop.clone()).context("install signal handler")?;
    }
    Ok(stop)
}
