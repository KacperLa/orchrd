//! HCI monitor framing: `opcode`, controller index and payload length.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorHeader {
    pub opcode: u16,
    pub index: u16,
    pub len: u16,
}

pub const MONITOR_HEADER_LEN: usize = 6;

/// Largest HCI monitor record we accept: ACL frames are bounded by the
/// controller's buffer size, well below this.
pub const MONITOR_BUFFER: usize = 65_541;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_monitor_record_header_is_decoded() {
        let header = parse_monitor_header(&[0x05, 0x00, 0x00, 0x00, 0x08, 0x00]);
        assert_eq!(
            header,
            Some(MonitorHeader {
                opcode: 5,
                index: 0,
                len: 8
            })
        );
        assert!(parse_monitor_header(&[0x05, 0x00]).is_none());
    }
}
