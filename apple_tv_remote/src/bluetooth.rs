//! Bluetooth plumbing: the HCI monitor socket, the remote's LE connection
//! handle, link parameter tuning and hidraw access.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::report::{le_connection_update_command, MonitorHeader, MONITOR_HEADER_LEN};

const AF_BLUETOOTH: i32 = 31;
const BTPROTO_HCI: i32 = 1;
const HCI_CHANNEL_MONITOR: u16 = 2;
const HCI_DEV_NONE: u16 = 0xFFFF;

const APPLE_VENDOR: &str = "0000004C";
const APPLE_REMOTE_PRODUCT: &str = "0000026D";

/// `_IOR('H', 212, int)` from linux/hci_sock.h.
const HCIGETCONNLIST: libc::c_ulong = 0x8004_48D4;

const SOCKADDR_HCI_LEN: libc::socklen_t = 6;
/// A remote's connection list is tiny; 64 entries mirror BlueZ's own buffer.
const MAX_CONNECTIONS: usize = 64;

#[repr(C)]
struct SockAddrHci {
    family: libc::sa_family_t,
    dev: u16,
    channel: u16,
}

/// An open HCI monitor socket delivering every controller packet.
pub struct MonitorSocket {
    fd: OwnedFd,
}

impl MonitorSocket {
    /// Requires `CAP_NET_RAW`.
    pub fn open() -> Result<Self> {
        let fd = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_RAW, BTPROTO_HCI) };
        if fd < 0 {
            return Err(io::Error::last_os_error())
                .context("open HCI monitor socket (CAP_NET_RAW)");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let address = SockAddrHci {
            family: AF_BLUETOOTH as libc::sa_family_t,
            dev: HCI_DEV_NONE,
            channel: HCI_CHANNEL_MONITOR,
        };
        let result = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&address as *const SockAddrHci).cast(),
                SOCKADDR_HCI_LEN,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error()).context("bind HCI monitor channel");
        }
        Ok(Self { fd })
    }

    /// Read one monitor record. The buffer must hold the largest ACL frame.
    ///
    /// Waits at most `timeout` for a record and returns `None` when there is
    /// none, so the caller can observe a stop request while the remote is
    /// idle or disconnected.
    pub fn recv(&self, buffer: &mut [u8], timeout: Duration) -> Result<Option<MonitorHeader>> {
        let mut poll_fd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout.as_millis() as libc::c_int) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(error.into());
        }
        if ready == 0 {
            return Ok(None);
        }
        let len = unsafe {
            libc::recv(
                self.fd.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            )
        };
        if len < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(error.into());
        }
        let len = len as usize;
        if len < MONITOR_HEADER_LEN {
            return Ok(None);
        }
        let header = crate::report::parse_monitor_header(&buffer[..MONITOR_HEADER_LEN])
            .ok_or_else(|| anyhow!("short monitor record"))?;
        if header.len as usize != len - MONITOR_HEADER_LEN {
            return Ok(None);
        }
        Ok(Some(header))
    }

    pub fn payload<'a>(&self, buffer: &'a [u8], header: MonitorHeader) -> &'a [u8] {
        &buffer[MONITOR_HEADER_LEN..MONITOR_HEADER_LEN + header.len as usize]
    }
}

/// A raw HCI socket used only to send the LE connection update command.
pub struct CommandSocket {
    fd: OwnedFd,
}

impl CommandSocket {
    pub fn open(device: u16) -> Result<Self> {
        let fd = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_RAW, BTPROTO_HCI) };
        if fd < 0 {
            return Err(io::Error::last_os_error()).context("open HCI command socket");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let address = SockAddrHci {
            family: AF_BLUETOOTH as libc::sa_family_t,
            dev: device,
            channel: 0, // HCI_CHANNEL_RAW
        };
        let result = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&address as *const SockAddrHci).cast(),
                SOCKADDR_HCI_LEN,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error()).context("bind HCI raw channel");
        }
        Ok(Self { fd })
    }

    /// Request a 7.5-15 ms connection interval with no peripheral latency.
    pub fn tune_link(&self, handle: u16) -> Result<()> {
        let command = le_connection_update_command(handle);
        let written = unsafe {
            libc::send(
                self.fd.as_raw_fd(),
                command.as_ptr().cast(),
                command.len(),
                0,
            )
        };
        if written < 0 {
            return Err(io::Error::last_os_error()).context("send LE connection update");
        }
        Ok(())
    }
}

/// Find the connection handle of `mac` on `device`, if it is connected.
pub fn connection_handle(device: u16, mac: &[u8; 6]) -> Result<Option<u16>> {
    let fd = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_RAW, BTPROTO_HCI) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("open HCI connection list socket");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // struct hci_conn_list_req { u16 dev_id; u16 conn_num; struct hci_conn_info[]; }
    let mut request = vec![0u8; 4 + MAX_CONNECTIONS * 16];
    request[0..2].copy_from_slice(&device.to_le_bytes());
    request[2..4].copy_from_slice(&(MAX_CONNECTIONS as u16).to_le_bytes());
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), HCIGETCONNLIST, request.as_mut_ptr()) };
    if result < 0 {
        return Err(io::Error::last_os_error()).context("HCIGETCONNLIST");
    }
    let count = u16::from_le_bytes([request[2], request[3]]) as usize;
    for index in 0..count.min(MAX_CONNECTIONS) {
        let offset = 4 + index * 16;
        let handle = u16::from_le_bytes([request[offset], request[offset + 1]]) & 0x0FFF;
        if request[offset + 2..offset + 8] == *mac {
            return Ok(Some(handle));
        }
    }
    Ok(None)
}

/// Encode a colon-separated Bluetooth address in HCI order.
pub fn parse_mac(value: &str) -> Result<[u8; 6]> {
    let mut octets = [0u8; 6];
    let parts: Vec<&str> = value.split(':').collect();
    if parts.len() != 6 {
        bail!("expected a Bluetooth address like AA:BB:CC:DD:EE:FF");
    }
    for (index, part) in parts.iter().enumerate() {
        octets[index] = u8::from_str_radix(part, 16)
            .with_context(|| format!("invalid Bluetooth address component {part:?}"))?;
    }
    // HCI reports addresses least significant byte first.
    octets.reverse();
    Ok(octets)
}

/// Locate the paired remote's hidraw node by HID ID and unique address.
pub fn find_hidraw(mac: &str) -> Result<PathBuf> {
    let wanted = mac.to_ascii_lowercase();
    let entries = std::fs::read_dir("/sys/class/hidraw")
        .context("read /sys/class/hidraw; is the remote paired?")?;
    for entry in entries.flatten() {
        let uevent = entry.path().join("device/uevent");
        let Ok(text) = std::fs::read_to_string(&uevent) else {
            continue;
        };
        let mut id = None;
        let mut unique = None;
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("HID_ID=") {
                id = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("HID_UNIQ=") {
                unique = Some(value.to_string());
            }
        }
        let expected = format!("0005:{APPLE_VENDOR}:{APPLE_REMOTE_PRODUCT}");
        if id.as_deref() == Some(expected.as_str())
            && unique.map(|value| value.to_ascii_lowercase()).as_deref() == Some(wanted.as_str())
        {
            let name = entry.file_name();
            return Ok(Path::new("/dev").join(name));
        }
    }
    Err(anyhow!(
        "Apple remote Bluetooth HID endpoint not found; wake the remote and reconnect it"
    ))
}

/// Send the A1513 feature report that enables full-rate touch reporting.
pub fn unlock_reports(path: &Path) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    // The report is a byte string; an integer array would send the low byte of
    // each element and the remote rejects the resulting pair.
    let mut report: [u8; 2] = [0xF0, 0xAF];
    // HIDIOCSFEATURE(len) = _IOWR('H', 0x06, len)
    let request =
        (3 << 30) | ((report.len() as libc::c_ulong) << 16) | (b'H' as libc::c_ulong) << 8 | 0x06;
    let result = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            request as libc::c_ulong,
            report.as_mut_ptr(),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("HIDIOCSFEATURE on {}", path.display()));
    }
    Ok(())
}
