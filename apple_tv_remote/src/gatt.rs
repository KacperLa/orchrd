//! BlueZ GATT setup for the remote's HID input report characteristic.
//!
//! Two things must happen before the remote streams its input reports: the
//! HID host has to subscribe to the input report characteristic, and the
//! remote is unlocked with a HID feature report sent through hidraw. Both are
//! retried because GATT objects and hidraw nodes are rebuilt whenever the
//! remote reconnects.

use std::time::Duration;

use anyhow::{Context, Result};
use zbus::zvariant::OwnedValue;
use zbus::Connection;

use crate::bluetooth::{find_hidraw, unlock_reports};

const BLUEZ: &str = "org.bluez";
const ROOT: &str = "/";
const DEVICE_INTERFACE: &str = "org.bluez.Device1";
const CHARACTERISTIC: &str = "org.bluez.GattCharacteristic1";
/// HID input report characteristic the remote multiplexes its reports over.
const REPORT_UUID: &str = "00002a4d-0000-1000-8000-00805f9b34fb";
/// How often to check the remote's connection state while subscribed.
const CONNECTION_POLL: Duration = Duration::from_millis(250);

pub struct Gatt {
    connection: Connection,
}

impl Gatt {
    pub async fn connect() -> Result<Self> {
        let connection = Connection::system()
            .await
            .context("connect to the system bus for BlueZ")?;
        Ok(Self { connection })
    }

    fn device_path(mac: &str) -> String {
        format!(
            "/org/bluez/hci0/dev_{}",
            mac.to_ascii_uppercase().replace(':', "_")
        )
    }

    /// Ask BlueZ to connect when it is not already connected, then subscribe to
    /// the input report characteristic and unlock the remote's report rate.
    pub async fn prepare(&self, mac: &str) -> Result<()> {
        let path = Self::device_path(mac);
        let device = zbus::Proxy::new(&self.connection, BLUEZ, path.as_str(), DEVICE_INTERFACE)
            .await
            .context("the remote is not known to BlueZ")?;
        // Connect is harmless when already connected and is required after
        // this remote's aggressive idle disconnects.
        if let Err(error) = device.call::<_, _, ()>("Connect", &()).await {
            tracing::debug!(%error, "BlueZ Connect request ignored");
        }

        let characteristic = self.find_report_characteristic(&path).await?;
        let uuid = characteristic
            .get_property::<String>("UUID")
            .await
            .unwrap_or_default();
        tracing::debug!(path = %characteristic.path(), %uuid, "subscribing to input reports");
        if let Err(error) = characteristic.call::<_, _, ()>("StartNotify", &()).await {
            // BlueZ's own HID host may already hold the subscription.
            tracing::debug!(%error, "StartNotify ignored");
        }

        let device_path = find_hidraw(mac)?;
        unlock_reports(&device_path)?;
        tracing::info!(path = %device_path.display(), "remote HID input reports enabled");
        Ok(())
    }

    /// Whether BlueZ still reports the remote as connected.
    pub async fn is_connected(&self, mac: &str) -> bool {
        let path = Self::device_path(mac);
        let Ok(device) =
            zbus::Proxy::new(&self.connection, BLUEZ, path.as_str(), DEVICE_INTERFACE).await
        else {
            return false;
        };
        device
            .get_property::<bool>("Connected")
            .await
            .unwrap_or(false)
    }

    async fn find_report_characteristic(&self, device: &str) -> Result<zbus::Proxy<'_>> {
        let manager = zbus::fdo::ObjectManagerProxy::new(&self.connection, BLUEZ, ROOT)
            .await
            .context("BlueZ ObjectManager is unavailable")?;
        let objects = manager
            .get_managed_objects()
            .await
            .context("list BlueZ objects")?;
        // Object paths are lowercase apart from the address, which BlueZ
        // spells in upper case; the device path passed in is no exception.
        let wanted = device.to_ascii_lowercase();
        for (path, interfaces) in objects {
            if !path.as_str().to_ascii_lowercase().starts_with(&wanted) {
                continue;
            }
            let Some(properties) = interfaces
                .iter()
                .find(|(name, _)| name.as_str() == CHARACTERISTIC)
                .map(|(_, properties)| properties)
            else {
                continue;
            };
            if !is_report_characteristic(properties) {
                continue;
            }
            return zbus::Proxy::new(&self.connection, BLUEZ, path, CHARACTERISTIC)
                .await
                .context("bind the input report characteristic");
        }
        anyhow::bail!("the A1513 input report characteristic was not found")
    }
}

/// Whether a characteristic carries the notifying HID input report.
fn is_report_characteristic(properties: &std::collections::HashMap<String, OwnedValue>) -> bool {
    let uuid = properties
        .get("UUID")
        .and_then(|value| String::try_from(value.try_clone().ok()?).ok())
        .unwrap_or_default();
    if uuid.to_ascii_lowercase() != REPORT_UUID {
        return false;
    }
    properties
        .get("Flags")
        .and_then(|value| Vec::<String>::try_from(value.try_clone().ok()?).ok())
        .is_some_and(|flags| flags.iter().any(|flag| flag == "notify"))
}

/// Background GATT setup. Runs on its own thread and runtime so the caller's
/// synchronous packet loop is never blocked.
///
/// The system-bus connection is held for as long as the remote stays
/// connected: BlueZ releases a `StartNotify` subscription as soon as the
/// connection that acquired it goes away, so it must not be dropped after
/// setup.
pub struct Preparation {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Preparation {
    pub fn stop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Preparation {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Retry GATT setup until the remote accepts it, then hold the subscription
/// and set it up again after every reconnect.
pub fn prepare_in_background(mac: String, interval: Duration) -> Preparation {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::Builder::new()
        .name("gatt".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "cannot start the GATT thread");
                    return;
                }
            };
            runtime.block_on(async {
                let mut gatt: Option<Gatt> = None;
                while !flag.load(Ordering::Acquire) {
                    let connection = match gatt.take() {
                        Some(connection) => connection,
                        None => match Gatt::connect().await {
                            Ok(connection) => connection,
                            Err(error) => {
                                tracing::warn!(%error, "Bluetooth GATT control unavailable");
                                tokio::time::sleep(interval).await;
                                continue;
                            }
                        },
                    };
                    match connection.prepare(&mac).await {
                        Ok(()) => {
                            // Keeping the connection alive keeps the input
                            // report subscription alive. Poll in short slices
                            // so shutdown does not wait a whole interval.
                            while !flag.load(Ordering::Acquire) {
                                if !connection.is_connected(&mac).await {
                                    tracing::info!(
                                        "remote disconnected; re-subscribing when it returns"
                                    );
                                    break;
                                }
                                tokio::time::sleep(interval.min(CONNECTION_POLL)).await;
                            }
                        }
                        Err(error) => {
                            tracing::debug!(%error, "input report setup not ready");
                            gatt = Some(connection);
                            tokio::time::sleep(interval).await;
                        }
                    }
                }
            });
        })
        .ok();
    Preparation { stop, thread }
}
