//! ATT notification reassembly and A1513 report parsing.

use apple_tv_remote::report::{
    connection_interval_ms, disconnection_handle, le_connection_handle,
    le_connection_update_command, parse_report, AttAssembler, InputReport,
};
use apple_tv_remote::touch::siri_pressed;

const ATT_CID: u16 = 4;
/// Characteristic value handle the remote notifies on.
const VALUE_HANDLE: u16 = 0x0023;
/// ACL connection handle used throughout these tests.
const HANDLE: u16 = 0x40;

/// Wrap a HID report in an ATT notification.
fn att_payload(report: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(0x1b); // ATT_HANDLE_VALUE_NTF
    payload.extend_from_slice(&VALUE_HANDLE.to_le_bytes());
    payload.extend_from_slice(report);
    payload
}

/// Wrap an ATT payload in one HCI ACL start fragment.
fn acl(handle: u16, payload: &[u8]) -> Vec<u8> {
    let mut fragment = Vec::new();
    // Handle, packet boundary 0b00 (first fragment), broadcast 0.
    fragment.extend_from_slice(&handle.to_le_bytes());
    let mut whole = Vec::new();
    whole.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    whole.extend_from_slice(&ATT_CID.to_le_bytes());
    whole.extend_from_slice(payload);
    fragment.extend_from_slice(&(whole.len() as u16).to_le_bytes());
    fragment.extend_from_slice(&whole);
    fragment
}

/// Wrap the tail of an ATT payload in one HCI ACL continuation fragment.
fn acl_continuation(handle: u16, payload: &[u8]) -> Vec<u8> {
    let mut fragment = Vec::new();
    // Handle with packet boundary 0b01 (continuation) in the upper nibble.
    let flags = handle | (0x01 << 12);
    fragment.extend_from_slice(&flags.to_le_bytes());
    fragment.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    fragment.extend_from_slice(payload);
    fragment
}

/// A complete ATT payload split into a start and continuation fragments.
fn fragmented(report: &[u8], split_at: usize) -> Vec<Vec<u8>> {
    // The ACL header is 4 bytes, so `split_at` counts L2CAP payload bytes.
    let payload = att_payload(report);
    let mut whole = Vec::new();
    whole.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    whole.extend_from_slice(&ATT_CID.to_le_bytes());
    whole.extend_from_slice(&payload);
    // The ACL header is 4 bytes; the fragments together carry the rest.
    let mut fragments = Vec::new();
    let head = &whole[..split_at];
    let mut start = Vec::new();
    start.extend_from_slice(&HANDLE.to_le_bytes());
    start.extend_from_slice(&(head.len() as u16).to_le_bytes());
    start.extend_from_slice(head);
    fragments.push(start);
    for part in whole[split_at..].chunks(5) {
        fragments.push(acl_continuation(HANDLE, part));
    }
    fragments
}

fn feed(assembler: &mut AttAssembler, payload: &[u8], out: &mut Vec<u8>) -> Option<Vec<u8>> {
    assembler.feed(payload, out).map(|_| out.clone())
}

const TOUCH_REPORT: [u8; 13] = [0x01, 0x00, 0x32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

#[test]
fn a_single_fragment_notification_decodes() {
    let mut assembler = AttAssembler::default();
    let mut out = Vec::new();
    let packet = acl(HANDLE, &att_payload(&TOUCH_REPORT));
    assert_eq!(
        feed(&mut assembler, &packet, &mut out).as_deref(),
        Some(&TOUCH_REPORT[..])
    );
}

#[test]
fn continuation_fragments_are_reassembled() {
    let mut assembler = AttAssembler::default();
    let mut out = Vec::new();
    let fragments = fragmented(&TOUCH_REPORT, 9);
    assert!(fragments.len() > 2, "test needs several fragments");
    for fragment in &fragments[..fragments.len() - 1] {
        assert!(feed(&mut assembler, fragment, &mut out).is_none());
    }
    assert_eq!(
        feed(
            &mut assembler,
            fragments.last().expect("fragments"),
            &mut out
        )
        .as_deref(),
        Some(&TOUCH_REPORT[..])
    );
}

#[test]
fn a_large_notification_survives_many_fragments() {
    let mut assembler = AttAssembler::default();
    let mut out = Vec::new();
    // An Opus report with a 300 byte payload spans several fragments.
    let mut report = vec![0x10, 0x00, 0x00, 0x00, 0x07, 0x00, 0xFF];
    report.extend((0..255u8).cycle().take(255).collect::<Vec<u8>>());
    let fragments = fragmented(&report, 9);
    assert!(fragments.len() > 5, "test needs many fragments");
    let mut assembled = None;
    for fragment in &fragments {
        if let Some(value) = feed(&mut assembler, fragment, &mut out) {
            assembled = Some(value);
        }
    }
    assert_eq!(assembled.as_deref(), Some(report.as_slice()));
}

#[test]
fn fragments_from_another_handle_are_dropped() {
    let mut assembler = AttAssembler::default();
    let mut out = Vec::new();
    let fragments = fragmented(&TOUCH_REPORT, 9);
    assert!(feed(&mut assembler, &fragments[0], &mut out).is_none());
    // A continuation for a different connection must not be appended.
    let foreign = acl_continuation(HANDLE + 1, &fragments[1][4..]);
    assert!(feed(&mut assembler, &foreign, &mut out).is_none());
    assert!(assembler.invalid >= 1);
    // Ours still completes from its own continuations.
    let mut assembled = None;
    for fragment in &fragments[1..] {
        if let Some(value) = feed(&mut assembler, fragment, &mut out) {
            assembled = Some(value);
        }
    }
    assert_eq!(assembled.as_deref(), Some(&TOUCH_REPORT[..]));
}

/// Other devices share the controller, so a PDU interleaved with theirs must
/// not clobber theirs or lose its own place; pending PDUs are per handle.
#[test]
fn interleaved_handles_reassemble_independently() {
    let mut assembler = AttAssembler::default();
    let mut out = Vec::new();
    let ours = fragmented(&TOUCH_REPORT, 9);
    // A whole notification for another device lands mid-reassembly of ours.
    let foreign_report = [0x01, 0x00, 0x32, 0, 0, 0, 9, 9, 9, 9, 9, 9, 9];
    let whole = acl(HANDLE + 1, &att_payload(&foreign_report));
    assert!(feed(&mut assembler, &ours[0], &mut out).is_none());
    assert_eq!(
        feed(&mut assembler, &whole, &mut out).as_deref(),
        Some(&foreign_report[..])
    );
    // Ours still completes from its own continuation fragments.
    let mut assembled = None;
    for fragment in &ours[1..] {
        if let Some(value) = feed(&mut assembler, fragment, &mut out) {
            assembled = Some(value);
        }
    }
    assert_eq!(assembled.as_deref(), Some(&TOUCH_REPORT[..]));
    assert_eq!(assembler.invalid, 0);
}

#[test]
fn a_broadcast_fragment_is_rejected() {
    let mut assembler = AttAssembler::default();
    let mut out = Vec::new();
    let mut packet = acl(HANDLE, &att_payload(&TOUCH_REPORT));
    // Boundary flag 3 is active broadcast; ours is never broadcast.
    packet[0..2].copy_from_slice(&(HANDLE | (0x03 << 12)).to_le_bytes());
    assert!(feed(&mut assembler, &packet, &mut out).is_none());
    assert_eq!(assembler.invalid, 1);
}

/// The flushable start flag (2) is a first fragment like the complete one.
#[test]
fn a_flushable_start_fragment_reassembles() {
    let mut assembler = AttAssembler::default();
    let mut out = Vec::new();
    let fragments = fragmented(&TOUCH_REPORT, 9);
    let mut start = fragments[0].clone();
    let flags = HANDLE | (0x02 << 12);
    start[0..2].copy_from_slice(&flags.to_le_bytes());
    assert!(feed(&mut assembler, &start, &mut out).is_none());
    let mut assembled = None;
    for fragment in &fragments[1..] {
        if let Some(value) = feed(&mut assembler, fragment, &mut out) {
            assembled = Some(value);
        }
    }
    assert_eq!(assembled.as_deref(), Some(&TOUCH_REPORT[..]));
    assert_eq!(assembler.invalid, 0);
}

/// Each rejected shape is fed to a fresh assembler so the fragment state of one
/// cannot mask the next. A truncated PDU is the one shape that is not an error:
/// it waits for the fragments that would complete it.
#[test]
fn malformed_notifications_are_rejected() {
    // A notification cut short before its L2CAP header is complete.
    let truncated = acl(HANDLE, &[0x1b]);
    // Too short to hold even the HCI ACL header.
    let short = vec![0, 0, 4];
    // A non-ATT channel must not be parsed as a notification.
    let mut wrong_channel = acl(HANDLE, &att_payload(&TOUCH_REPORT));
    wrong_channel[6..8].copy_from_slice(&0x0040u16.to_le_bytes());
    // A different ATT opcode is not a notification.
    let mut wrong_opcode = att_payload(&TOUCH_REPORT);
    wrong_opcode[0] = 0x1d;
    let wrong_opcode = acl(HANDLE, &wrong_opcode);
    // A notification for another characteristic value handle.
    let mut wrong_handle = att_payload(&TOUCH_REPORT);
    wrong_handle[1..3].copy_from_slice(&0x9999u16.to_le_bytes());
    let wrong_handle = acl(HANDLE, &wrong_handle);

    for (case, expected_invalid) in [
        (truncated, 0),
        (short, 1),
        (wrong_channel, 1),
        (wrong_opcode, 1),
        (wrong_handle, 1),
        (acl_continuation(HANDLE, &[0u8; 6]), 1),
    ] {
        let mut assembler = AttAssembler::default();
        let mut out = Vec::new();
        assert!(
            feed(&mut assembler, &case, &mut out).is_none(),
            "accepted {case:02x?}"
        );
        assert_eq!(
            assembler.invalid, expected_invalid,
            "wrong rejection count for {case:02x?}"
        );
    }
}

#[test]
fn a_declared_length_larger_than_the_l2cap_bound_is_rejected() {
    let mut assembler = AttAssembler::default();
    let mut out = Vec::new();
    let mut packet = acl(HANDLE, &att_payload(&TOUCH_REPORT));
    // Bytes 4..6 of the ACL fragment are the L2CAP payload length.
    packet[4..6].copy_from_slice(&0xFFFFu16.to_le_bytes());
    assert!(feed(&mut assembler, &packet, &mut out).is_none());
    assert_eq!(assembler.invalid, 1);
}

#[test]
fn a_declared_length_beyond_the_capture_waits_for_more() {
    let mut assembler = AttAssembler::default();
    let mut out = Vec::new();
    // One fragment with an L2CAP length that no later fragment will satisfy.
    let mut payload = att_payload(&TOUCH_REPORT);
    payload.resize(32, 0);
    let mut packet = acl(HANDLE, &payload);
    // Bytes 4..6 of the ACL fragment are the L2CAP payload length.
    packet[4..6].copy_from_slice(&64u16.to_le_bytes());
    assert!(feed(&mut assembler, &packet, &mut out).is_none());
    assert_eq!(assembler.invalid, 0, "an incomplete PDU is not an error");
}

#[test]
fn reports_are_classified() {
    let buttons = [0x00, 0x10];
    assert_eq!(parse_report(&buttons), Some(InputReport::Buttons(&buttons)));

    let mut opus = vec![0x10, 0x00, 0x00, 0x00];
    opus.extend_from_slice(&7u16.to_le_bytes());
    opus.push(3);
    opus.extend_from_slice(&[0xF8, 0xFF, 0xFE]);
    match parse_report(&opus) {
        Some(InputReport::Opus { sequence, payload }) => {
            assert_eq!(payload, &[0xF8, 0xFF, 0xFE]);
            assert_eq!(sequence, 7);
        }
        other => panic!("expected an Opus report, got {other:?}"),
    }

    assert!(matches!(
        parse_report(&TOUCH_REPORT),
        Some(InputReport::Touch(_))
    ));
}

/// The 20 byte touch report carries its HID report ID on the wire; the body is
/// what the touch state machine understands.
#[test]
fn the_touch_report_id_is_stripped_before_decoding() {
    let mut prefixed = vec![0xFC];
    prefixed.extend_from_slice(&TOUCH_REPORT);
    match parse_report(&prefixed) {
        Some(InputReport::Touch(body)) => assert_eq!(body, &TOUCH_REPORT[..]),
        other => panic!("expected a touch report, got {other:?}"),
    }
    // The Opus report has its own report ID and must not be affected.
    let opus = [0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0xF8, 0xFF];
    assert!(matches!(
        parse_report(&opus),
        Some(InputReport::Opus { .. })
    ));
}

#[test]
fn opus_reports_require_their_full_length() {
    // Declared length exceeds the report.
    let mut opus = vec![0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0x20];
    opus.extend_from_slice(&[0u8; 4]);
    assert_eq!(parse_report(&opus), None);
    // A zero length carries no audio.
    let mut opus = vec![0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00];
    opus.extend_from_slice(&[0u8; 4]);
    assert_eq!(parse_report(&opus), None);
}

#[test]
fn the_two_byte_report_carries_siri_and_the_other_buttons() {
    assert!(siri_pressed(&[0x00, 0x10]));
    assert!(!siri_pressed(&[0x00, 0x00]));
    // A click, Play/Pause or Menu report is not a Siri press.
    assert!(!siri_pressed(&[0x00, 0x80]));
    assert!(!siri_pressed(&[0x00, 0x08]));
    assert!(!siri_pressed(&[0x00, 0x20]));
}

/// The address is reported least significant byte first, and only a successful
/// connection to our remote yields a handle.
#[test]
fn le_connection_events_expose_the_remote_handle() {
    let address = [0xE5, 0xB9, 0x12, 0x95, 0xEC, 0x28];
    let mut event = vec![0x3e, 0x13, 0x01, 0x00, 0x41, 0x00, 0x00, 0x00];
    event.extend_from_slice(&address);
    event.extend_from_slice(&[0x00, 0x00]);
    assert_eq!(le_connection_handle(&event, &address), Some(0x41));

    let other = [0u8; 6];
    assert_eq!(le_connection_handle(&event, &other), None);

    let mut failed = event.clone();
    failed[3] = 0x02;
    assert_eq!(le_connection_handle(&failed, &address), None);
    // Not an LE meta event.
    assert_eq!(
        le_connection_handle(&[0x05, 0x04, 0x00, 0x41], &address),
        None
    );
}

#[test]
fn disconnection_events_expose_the_handle() {
    let event = [0x05, 0x04, 0x00, 0x41, 0x00, 0x13];
    assert_eq!(disconnection_handle(&event), Some(0x41));
    assert_eq!(disconnection_handle(&[0x05, 0x04]), None);
    assert_eq!(
        disconnection_handle(&[0x0e, 0x04, 0x00, 0x41, 0x00, 0x13]),
        None
    );
}

#[test]
fn connection_update_event_reports_the_negotiated_interval() {
    let mut event = vec![0x3e, 0x0c, 0x03, 0x00, 0x41, 0x00];
    event.extend_from_slice(&12u16.to_le_bytes());
    event.extend_from_slice(&[0, 0, 0, 0]);
    assert_eq!(connection_interval_ms(&event, 0x41), Some((15.0, 0x00)));
    assert_eq!(connection_interval_ms(&event, 0x42), None);
}

#[test]
fn connection_update_command_matches_the_hci_layout() {
    let command = le_connection_update_command(0x0041);
    assert_eq!(command[0], 0x01);
    assert_eq!(u16::from_le_bytes([command[1], command[2]]), 0x2013);
    assert_eq!(command[3], 14);
    assert_eq!(u16::from_le_bytes([command[4], command[5]]), 0x0041);
    assert_eq!(u16::from_le_bytes([command[6], command[7]]), 6);
    assert_eq!(u16::from_le_bytes([command[8], command[9]]), 12);
    assert_eq!(u16::from_le_bytes([command[10], command[11]]), 0);
    assert_eq!(u16::from_le_bytes([command[12], command[13]]), 200);
}
