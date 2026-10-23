//! Offline btsnoop decoding, which is also the only test of the HCI ACL
//! reassembly path end to end.

mod common;

use std::io::Write;

use apple_tv_remote::inspect::inspect_capture;
use common::tone_packet;

/// Build a btsnoop capture from HCI ACL fragments.
fn capture(fragments: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"btsnoop\0");
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.extend_from_slice(&2001u32.to_be_bytes());
    for fragment in fragments {
        bytes.extend_from_slice(&(fragment.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&(fragment.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&5u32.to_be_bytes()); // flags: ACL RX
        bytes.extend_from_slice(&0u32.to_be_bytes()); // drops
        bytes.extend_from_slice(&0u64.to_be_bytes()); // timestamp
        bytes.extend_from_slice(fragment);
    }
    bytes
}

fn write_capture(fragments: &[Vec<u8>]) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    file.write_all(&capture(fragments)).expect("write capture");
    file.flush().expect("flush");
    file
}

/// One HCI ACL start fragment carrying an ATT notification with `report`.
fn notification_fragment(handle: u16, report: &[u8]) -> Vec<u8> {
    let att = att_notification(report);
    let mut whole = Vec::new();
    whole.extend_from_slice(&(att.len() as u16).to_le_bytes());
    whole.extend_from_slice(&4u16.to_le_bytes()); // ATT channel
    whole.extend_from_slice(&att);

    let mut fragment = Vec::new();
    // Handle 0x40, packet boundary 0b00 (first fragment), broadcast 0.
    fragment.extend_from_slice(&handle.to_le_bytes());
    fragment.extend_from_slice(&(whole.len() as u16).to_le_bytes());
    fragment.extend_from_slice(&whole);
    fragment
}

fn att_notification(report: &[u8]) -> Vec<u8> {
    let mut att = vec![0x1b]; // ATT_HANDLE_VALUE_NTF
    att.extend_from_slice(&0x0023u16.to_le_bytes());
    att.extend_from_slice(report);
    att
}

fn opus_report(sequence: u16, payload: &[u8]) -> Vec<u8> {
    let mut report = vec![0x10, 0x00, 0x00, 0x00];
    report.extend_from_slice(&sequence.to_le_bytes());
    report.push(payload.len() as u8);
    report.extend_from_slice(payload);
    report
}

#[test]
fn a_capture_of_opus_reports_is_decoded() {
    let packet = tone_packet();
    let fragments: Vec<Vec<u8>> = (0..12)
        .map(|sequence| notification_fragment(0x0040, &opus_report(sequence, &packet)))
        .collect();
    let file = write_capture(&fragments);
    let report = inspect_capture(file.path()).expect("inspect capture");
    assert_eq!(report.records, 12);
    assert_eq!(report.stats.frames, 12);
    assert_eq!(report.invalid, 0);
    assert!(report.stats.peak > 1000);
    assert!((report.seconds() - 12.0 * 0.020).abs() < 1e-9);
}

#[test]
fn a_capture_without_opus_reports_is_not_an_error() {
    // A button report and an unrelated notification produce no audio.
    let buttons = vec![0x00, 0x10];
    let fragments = vec![
        notification_fragment(0x0040, &buttons),
        // A notification on a characteristic the daemon does not read.
        {
            let mut other = notification_fragment(0x0040, &buttons);
            other[5..7].copy_from_slice(&0x9999u16.to_le_bytes());
            other
        },
    ];
    let file = write_capture(&fragments);
    let report = inspect_capture(file.path()).expect("inspect capture");
    assert_eq!(report.stats.frames, 0);
    assert_eq!(report.stats.errors, 0);
    assert_eq!(report.invalid, 1);
}

#[test]
fn a_multi_fragment_notification_is_reassembled() {
    // A report large enough to need several fragments.
    let mut audio = tone_packet();
    audio.resize(200, 0x7f);
    let packet = opus_report(0, &audio);

    let att = att_notification(&packet);
    let mut whole = Vec::new();
    whole.extend_from_slice(&(att.len() as u16).to_le_bytes());
    whole.extend_from_slice(&4u16.to_le_bytes());
    whole.extend_from_slice(&att);

    // The start fragment carries the first bytes; the continuations carry the
    // rest with the 0b01 packet boundary.
    let mut fragments = Vec::new();
    let split = 20;
    let mut start = Vec::new();
    start.extend_from_slice(&0x0040u16.to_le_bytes());
    start.extend_from_slice(&(split as u16).to_le_bytes());
    start.extend_from_slice(&whole[..split]);
    fragments.push(start);
    for part in whole[split..].chunks(23) {
        let mut fragment = Vec::new();
        // Handle 0x40, packet boundary 0b01, broadcast 0.
        fragment.extend_from_slice(&0x1040u16.to_le_bytes());
        fragment.extend_from_slice(&(part.len() as u16).to_le_bytes());
        fragment.extend_from_slice(part);
        fragments.push(fragment);
    }
    assert!(fragments.len() > 4, "test needs several fragments");
    let file = write_capture(&fragments);
    let report = inspect_capture(file.path()).expect("inspect capture");
    assert!(
        report.stats.frames + report.stats.errors >= 1,
        "the fragmented report never reached the decoder"
    );
    assert_eq!(report.invalid, 0, "a valid fragmented PDU was rejected");
}

#[test]
fn a_capture_starting_with_a_continuation_is_rejected() {
    let fragment = {
        let mut fragment = Vec::new();
        fragment.extend_from_slice(&0x1040u16.to_le_bytes());
        fragment.extend_from_slice(&4u16.to_le_bytes());
        fragment.extend_from_slice(&[1, 2, 3, 4]);
        fragment
    };
    let file = write_capture(&[fragment]);
    let report = inspect_capture(file.path()).expect("inspect capture");
    assert_eq!(report.invalid, 1);
}

#[test]
fn a_non_btsnoop_file_is_rejected() {
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    file.write_all(b"not a capture at all").expect("write");
    file.flush().expect("flush");
    assert!(inspect_capture(file.path()).is_err());
}

#[test]
fn an_unsupported_datalink_is_rejected() {
    let mut bytes = capture(&[]);
    bytes[12..16].copy_from_slice(&1002u32.to_be_bytes());
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    file.write_all(&bytes).expect("write");
    file.flush().expect("flush");
    assert!(inspect_capture(file.path()).is_err());
}
