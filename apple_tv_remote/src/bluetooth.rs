//! Bluetooth plumbing: the HCI monitor socket and the remote's hidraw access.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::report::{parse_monitor_header, MonitorHeader, MONITOR_HEADER_LEN};

const AF_BLUETOOTH: i32 = 31;
const BTPROTO_HCI: i32 = 1;
const HCI_CHANNEL_MONITOR: u16 = 2;
const HCI_DEV_NONE: u16 = 0xFFFF;

const APPLE_VENDOR: &str = "0000004C";
const APPLE_REMOTE_PRODUCT: &str = "0000026D";

const SOCKADDR_HCI_LEN: libc::socklen_t = 6;

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
        let header = parse_monitor_header(&buffer[..MONITOR_HEADER_LEN])
            .ok_or_else(|| anyhow!("short monitor record"))?;
        if header.len as usize != len - MONITOR_HEADER_LEN {
            return Ok(None);
        }
        Ok(Some(header))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bluetooth_address_is_decoded_in_hci_order() {
        assert_eq!(
            parse_mac("28:EC:95:12:B9:E5").unwrap(),
            [0xE5, 0xB9, 0x12, 0x95, 0xEC, 0x28]
        );
        assert!(parse_mac("28:EC:95:12:B9").is_err());
        assert!(parse_mac("28:EC:95:12:B9:GG").is_err());
    }
}
