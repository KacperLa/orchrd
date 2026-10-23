//! Bluetooth LE framing and A1513 input report parsing.
//!
//! The remote multiplexes three kinds of input reports over its HID
//! notification characteristic: touchpad/button reports, the Siri button and
//! Opus microphone packets. The HCI monitor delivers them as HCI ACL fragments
//! of an ATT notification, so they are reassembled and unwrapped in stages.

/// Reports are a few dozen bytes; anything larger is not from this device.
pub const MAX_REPORT: usize = 512;
/// An ATT notification never approaches this; the bound stops a malformed
/// length from growing the assembly buffer without limit.
const MAX_L2CAP: usize = 4096;
/// Packet boundary flag meaning "continuation of a lower layer PDU".
const ACL_CONTINUATION: u8 = 0x01;
/// Start fragments: a complete PDU or its first fragment. Flag 2 is the
/// automatically flushable variant; 3 is broadcast and never carries ours.
const ACL_START_COMPLETE: u8 = 0x00;
const ACL_START_FLUSHABLE: u8 = 0x02;
/// L2CAP header: 2 byte length, 2 byte channel id.
const L2CAP_HEADER_LEN: usize = 4;
/// Channel id of the Attribute Protocol.
const ATT_CID: u16 = 4;
/// ATT notification: opcode 0x1b, then the 16 bit characteristic value handle,
/// then the value itself, which is the HID report.
const ATT_NOTIFICATION: u8 = 0x1b;
const ATT_HEADER_LEN: usize = 3;
/// Attribute handle of the remote's HID input report characteristic.
const ATT_REPORT_HANDLE: [u8; 2] = [0x23, 0x00];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputReport<'a> {
    /// Encoded microphone audio: sequence counter and Opus payload.
    Opus { sequence: u16, payload: &'a [u8] },
    /// Two byte button report; bit 0x10 is Siri and the click, Play/Pause and
    /// Menu bits are read by the touchpad.
    Buttons(&'a [u8]),
    /// Multiplexed touchpad/button input report.
    Touch(&'a [u8]),
}

/// ACL connection handle of one HCI ACL fragment, masked to 12 bits.
pub fn acl_handle(payload: &[u8]) -> Option<u16> {
    let bytes = payload.get(..2)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]) & 0x0FFF)
}

/// Parse one unwrapped HID input report.
pub fn parse_report(report: &[u8]) -> Option<InputReport<'_>> {
    // The 20 byte touch/button report is prefixed with its HID report ID; the
    // rest of the parsers work on the report body.
    let report = report.strip_prefix(&[0xFC]).unwrap_or(report);
    if report.len() == 2 {
        return Some(InputReport::Buttons(report));
    }
    // Gen 1 Opus: report id, buttons, two status bytes, u16 sequence, length,
    // then the payload.
    if report.first() == Some(&0x10) {
        if report.len() < 8 {
            return None;
        }
        let length = report[6] as usize;
        if length == 0 || report.len() < 7 + length {
            return None;
        }
        return Some(InputReport::Opus {
            sequence: u16::from_le_bytes([report[4], report[5]]),
            payload: &report[7..7 + length],
        });
    }
    // Touch and button/IMU reports carry a small report id.
    if report.len() >= 2 && report[0] & !0x07 == 0 {
        return Some(InputReport::Touch(report));
    }
    None
}

/// Reassembles the HCI ACL fragments of an ATT notification into a HID report.
///
/// An ATT notification can exceed the LE ACL MTU, in which case the controller
/// delivers it as several HCI ACL fragments with one L2CAP header at the front.
/// That header's length covers the whole L2CAP payload while each fragment's
/// header covers only its own bytes, so the two must not be confused.
///
/// Pending PDUs are kept per connection handle: other devices share the
/// controller, and a capture can interleave their fragments with ours.
#[derive(Debug, Default)]
pub struct AttAssembler {
    pending: std::collections::HashMap<u16, Pending>,
    /// How many PDUs or notifications were rejected.
    pub invalid: u64,
}

#[derive(Debug, Default)]
struct Pending {
    data: Vec<u8>,
    /// L2CAP payload length announced by the first fragment of the PDU.
    expected: usize,
}

impl AttAssembler {
    /// Feed one HCI ACL payload as delivered by the monitor. Returns the ACL
    /// connection handle and writes the HID report into `out` once a complete,
    /// valid notification has arrived.
    pub fn feed(&mut self, payload: &[u8], out: &mut Vec<u8>) -> Option<u16> {
        if payload.len() < L2CAP_HEADER_LEN {
            self.invalid += 1;
            return None;
        }
        // HCI ACL header: handle and packet boundary flags, then the length of
        // this fragment.
        let handle_flags = u16::from_le_bytes([payload[0], payload[1]]);
        let handle = handle_flags & 0x0FFF;
        let boundary = ((handle_flags >> 12) & 0x03) as u8;
        let body = &payload[L2CAP_HEADER_LEN..];
        if boundary == ACL_CONTINUATION {
            let Some(pending) = self.pending.get_mut(&handle) else {
                self.invalid += 1;
                return None;
            };
            pending.data.extend_from_slice(body);
        } else if boundary == ACL_START_COMPLETE || boundary == ACL_START_FLUSHABLE {
            // Only the first fragment carries the L2CAP header.
            if body.len() < L2CAP_HEADER_LEN {
                self.invalid += 1;
                return None;
            }
            let length = u16::from_le_bytes([body[0], body[1]]) as usize;
            let channel = u16::from_le_bytes([body[2], body[3]]);
            if length > MAX_L2CAP || channel != ATT_CID {
                self.invalid += 1;
                return None;
            }
            let pending = self.pending.entry(handle).or_default();
            pending.data.clear();
            pending.expected = length;
            pending.data.extend_from_slice(&body[L2CAP_HEADER_LEN..]);
        } else {
            // A broadcast fragment never carries the remote's reports.
            self.invalid += 1;
            return None;
        }
        let Some(pending) = self.pending.get(&handle) else {
            self.invalid += 1;
            return None;
        };
        if pending.data.len() < pending.expected {
            return None;
        }
        let pending = self.pending.remove(&handle).expect("just checked");
        // Fragments carry exactly one PDU; anything left over was not announced.
        if pending.data.len() != pending.expected {
            self.invalid += 1;
            return None;
        }
        // The ATT payload is the notification opcode, the characteristic value
        // handle, then the value, which is the HID report itself.
        let header = pending.data.get(..ATT_HEADER_LEN)?;
        if header[0] != ATT_NOTIFICATION || header[1..] != ATT_REPORT_HANDLE {
            self.invalid += 1;
            return None;
        }
        let value = &pending.data[ATT_HEADER_LEN..];
        if value.len() > MAX_REPORT {
            self.invalid += 1;
            return None;
        }
        out.clear();
        out.extend_from_slice(value);
        Some(handle)
    }
}

/// HCI monitor framing: `opcode`, controller index and payload length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorHeader {
    pub opcode: u16,
    pub index: u16,
    pub len: u16,
}

pub const MONITOR_HEADER_LEN: usize = 6;
pub const HCI_MON_EVENT_PKT: u16 = 3;
pub const HCI_MON_ACL_RX_PKT: u16 = 5;

pub fn parse_monitor_header(data: &[u8]) -> Option<MonitorHeader> {
    if data.len() < MONITOR_HEADER_LEN {
        return None;
    }
    Some(MonitorHeader {
        opcode: u16::from_le_bytes([data[0], data[1]]),
        index: u16::from_le_bytes([data[2], data[3]]),
        len: u16::from_le_bytes([data[4], data[5]]),
    })
}

pub const LE_META_EVENT: u8 = 0x3e;

/// Return the LE connection handle when a meta event announces our remote.
pub fn le_connection_handle(event: &[u8], address: &[u8; 6]) -> Option<u16> {
    if event.len() < 14 || event[0] != LE_META_EVENT || !matches!(event[2], 0x01 | 0x0a) {
        return None;
    }
    if event[3] != 0 || event[8..14] != *address {
        return None;
    }
    Some(u16::from_le_bytes([event[4], event[5]]) & 0x0FFF)
}

/// Return the handle of a disconnection complete event, if any.
pub fn disconnection_handle(event: &[u8]) -> Option<u16> {
    if event.len() >= 6 && event[0] == 0x05 {
        // status(1) handle(2) reason(1)
        return Some(u16::from_le_bytes([event[3], event[4]]) & 0x0FFF);
    }
    None
}

/// Return the negotiated connection interval in milliseconds when the LE
/// connection update complete event belongs to `handle`.
pub fn connection_interval_ms(event: &[u8], handle: u16) -> Option<(f64, u8)> {
    if event.len() < 12 || event[0] != LE_META_EVENT || event[2] != 0x03 {
        return None;
    }
    if u16::from_le_bytes([event[4], event[5]]) & 0x0FFF != handle {
        return None;
    }
    let interval = f64::from(u16::from_le_bytes([event[6], event[7]])) * 1.25;
    Some((interval, event[3]))
}

/// HCI command packet for `LE_CONN_UPDATE`: 7.5-15 ms interval, no latency,
/// 2 s supervision timeout.
pub fn le_connection_update_command(handle: u16) -> [u8; 18] {
    const HCI_COMMAND_PKT: u8 = 0x01;
    const OGF_LE_CTL: u16 = 0x08;
    const OCF_LE_CONN_UPDATE: u16 = 0x13;
    let mut command = [0u8; 18];
    command[0] = HCI_COMMAND_PKT;
    command[1..3].copy_from_slice(&((OGF_LE_CTL << 10) | OCF_LE_CONN_UPDATE).to_le_bytes());
    command[3] = 14; // parameter length
    command[4..6].copy_from_slice(&handle.to_le_bytes());
    command[6..8].copy_from_slice(&6u16.to_le_bytes()); // 7.5 ms minimum interval
    command[8..10].copy_from_slice(&12u16.to_le_bytes()); // 15 ms maximum interval
    command[10..12].copy_from_slice(&0u16.to_le_bytes()); // peripheral latency
    command[12..14].copy_from_slice(&200u16.to_le_bytes()); // 2 s supervision timeout
    command
}
