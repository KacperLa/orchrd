//! Opus decode, jitter buffering and playout.

mod common;

use std::time::{Duration, Instant};

use apple_tv_remote::audio::{
    Playout, Session, FRAME_BYTES, FRAME_SAMPLES, PREROLL_FRAMES, RING_SAMPLES,
};
use common::tone_packets;

fn new_session() -> (Session, Playout) {
    Session::new().expect("decoder")
}

fn push(session: &mut Session, packets: &[Vec<u8>], first_sequence: u16, now: Instant) {
    for (index, packet) in packets.iter().enumerate() {
        session.push_transport(first_sequence + index as u16, packet, now);
    }
}

#[test]
fn decoded_audio_reaches_the_playout_ring() {
    let (mut session, mut playout) = new_session();
    session.siri(true);
    // The press flush is applied by the first process cycle; audio arriving
    // afterwards survives.
    let mut discard = vec![0u8; FRAME_BYTES];
    playout.fill(&mut discard);

    let packets = tone_packets(8);
    push(&mut session, &packets, 0, Instant::now());
    assert_eq!(session.stats.frames, 8);
    assert!(session.stats.peak > 1000);
    assert!(playout.link().buffered_samples() >= FRAME_SAMPLES * 6);

    let mut buffer = vec![0u8; FRAME_BYTES * 4];
    assert_eq!(playout.fill(&mut buffer), buffer.len());
    assert!(buffer.chunks_exact(2).any(|sample| sample != [0, 0]));
}

#[test]
fn silence_is_emitted_when_nothing_was_received() {
    let (mut session, mut playout) = new_session();
    session.siri(true);
    let mut buffer = vec![0xffu8; FRAME_BYTES * 2];
    assert_eq!(playout.fill(&mut buffer), buffer.len());
    assert!(buffer.iter().all(|byte| *byte == 0));
    assert_eq!(playout.link().underruns(), 0);
}

/// A new press must discard what the previous session queued without touching
/// what arrives afterwards.
#[test]
fn a_press_discards_only_the_previous_session_audio() {
    let (mut session, mut playout) = new_session();
    session.siri(true);
    let packets = tone_packets(10);
    let now = Instant::now();
    push(&mut session, &packets, 0, now);
    assert!(playout.link().buffered_samples() > 0);

    session.siri(false);
    session.siri(true);
    // The first process cycle applies the drop mark and reports nothing left.
    let mut discard = vec![0xffu8; FRAME_BYTES];
    playout.fill(&mut discard);
    assert!(discard.iter().all(|byte| *byte == 0));
    assert_eq!(playout.link().buffered_samples(), 0);

    // Fresh audio still reaches the playout.
    push(&mut session, &packets, 100, now);
    let mut buffer = vec![0u8; FRAME_BYTES * 4];
    playout.fill(&mut buffer);
    assert!(playout.link().buffered_samples() > 0);
    assert!(buffer.chunks_exact(2).any(|sample| sample != [0, 0]));
}

#[test]
fn duplicate_sequences_are_skipped() {
    let (mut session, _playout) = new_session();
    session.siri(true);
    let packets = tone_packets(2);
    let now = Instant::now();
    session.push_transport(5, &packets[0], now);
    session.push_transport(5, &packets[1], now);
    assert_eq!(session.stats.frames, 1);
}

#[test]
fn a_large_jump_resets_the_decoder() {
    let (mut session, _playout) = new_session();
    session.siri(true);
    let packets = tone_packets(2);
    let now = Instant::now();
    session.push_transport(0, &packets[0], now);
    session.push_transport(u16::MAX, &packets[1], now);
    assert_eq!(session.stats.frames, 2);
    assert_eq!(session.stats.sequence_skips, 0);
}

#[test]
fn gaps_conceal_only_when_the_clock_agrees() {
    let (mut session, _playout) = new_session();
    session.siri(true);
    let packets = tone_packets(3);
    let start = Instant::now();
    session.push_transport(0, &packets[0], start);
    // Sequence jumps by 3 but no time passed: no concealment frames.
    session.push_transport(3, &packets[1], start);
    assert_eq!(session.stats.sequence_skips, 2);
    assert_eq!(session.stats.concealed, 0);
    assert_eq!(session.stats.frames, 2);

    // Two frames worth of time passed: two concealment frames.
    let (mut slow, slow_playout) = new_session();
    slow.siri(true);
    slow.push_transport(0, &packets[0], start);
    slow.push_transport(3, &packets[1], start + Duration::from_millis(60));
    assert_eq!(slow.stats.concealed, 2);
    assert!(slow_playout.link().buffered_samples() > 0);
}

/// Each concealed interval must become exactly one 20 ms frame: libopus
/// synthesizes `frame_size` samples, so a larger decode buffer stretches the
/// gap's audio instead of filling it.
#[test]
fn concealment_fills_exactly_the_missing_intervals() {
    let (mut session, playout) = new_session();
    session.siri(true);
    let packets = tone_packets(2);
    let start = Instant::now();
    session.push_transport(0, &packets[0], start);
    session.push_transport(3, &packets[1], start + Duration::from_millis(60));
    assert_eq!(session.stats.concealed, 2);
    // Two arrived frames plus two concealed ones, each exactly 20 ms; a
    // stretched concealment would leave 320 extra samples in the ring.
    assert_eq!(playout.link().buffered_samples(), 4 * FRAME_SAMPLES);
}

/// The counter can jump without audio going missing, so a jump larger than the
/// elapsed delivery time explains must not be concealed whole: only the frames
/// the elapsed intervals have room for, less the frame that just arrived.
#[test]
fn a_counter_jump_beyond_the_elapsed_time_is_partially_concealed() {
    let (mut session, _playout) = new_session();
    session.siri(true);
    let packets = tone_packets(2);
    let start = Instant::now();
    session.push_transport(0, &packets[0], start);
    // Five missing counter values, but only 40 ms passed: one lost frame.
    session.push_transport(6, &packets[1], start + Duration::from_millis(40));
    assert_eq!(session.stats.sequence_skips, 5);
    assert_eq!(session.stats.concealed, 1);
}

#[test]
fn invalid_packets_count_as_errors_without_stopping() {
    let (mut session, _playout) = new_session();
    session.siri(true);
    let now = Instant::now();
    session.push_transport(0, &[0xFF, 0xFF, 0xFF, 0xFF], now);
    assert_eq!(session.stats.errors, 1);
    let packets = tone_packets(1);
    session.push_transport(1, &packets[0], now);
    assert_eq!(session.stats.frames, 1);
}

/// A consumer that never pulls at all keeps the whole utterance, exactly as
/// the Python daemon's bounded backlog did: speech is never dropped to make
/// room, and speech older than the two-minute bound is a fault the daemon
/// reports rather than a silent truncation of the recording.
#[test]
fn an_absent_consumer_keeps_the_utterance_until_the_ring_fills() {
    let (mut session, mut playout) = new_session();
    session.siri(true);
    // Apply the session drop mark, then never consume again.
    let mut buffer = vec![0u8; FRAME_BYTES];
    playout.fill(&mut buffer);

    let packets = tone_packets(1);
    let now = Instant::now();
    for index in 0..(RING_SAMPLES / FRAME_SAMPLES) {
        session.push_transport(index as u16, &packets[0], now);
    }
    // Two minutes fill the ring with nothing dropped: the whole utterance is
    // retained for a consumer that attaches late.
    assert!(!session.stats_backlog_exceeded());
    assert!(playout.link().buffered_samples() <= RING_SAMPLES);
    // Past fullness the overflow flag is raised: keeping the recording going
    // would silently lose speech.
    session.push_transport(0x7FFF, &packets[0], now);
    assert!(session.stats_backlog_exceeded());
    let mut drain = vec![0u8; FRAME_BYTES * 4];
    playout.fill(&mut drain);
    assert!(
        playout.link().buffered_samples() > 0,
        "the buffered utterance was dropped"
    );
}

#[test]
fn siri_transitions_are_reported_once() {
    let (mut session, _playout) = new_session();
    assert_eq!(session.siri(true), Some(true));
    assert_eq!(session.siri(true), None);
    assert_eq!(session.siri(false), Some(false));
    assert_eq!(session.siri(false), None);
}

#[test]
fn playout_pads_partial_buffers_with_silence() {
    let (mut session, mut playout) = new_session();
    session.siri(true);
    let packets = tone_packets(1);
    session.push_transport(0, &packets[0], Instant::now());
    // The consumer's timeline advances by the whole buffer: one and a half
    // frames advance the playout by one and a half frames, the half frame is
    // silence, and everything is written.
    let mut buffer = vec![0xffu8; FRAME_BYTES + FRAME_BYTES / 2];
    let written = playout.fill(&mut buffer);
    assert_eq!(written, FRAME_BYTES + FRAME_BYTES / 2);
    assert_eq!(&buffer[FRAME_BYTES..], &vec![0u8; FRAME_BYTES / 2]);
}

/// Playout must not follow the first arriving frame: without the six frame
/// preroll as a jitter budget, the playout clock falls behind every Bluetooth
/// burst, chopping the utterance with padded silence and stretching it past
/// the release.
#[test]
fn playout_holds_off_until_the_preroll_is_buffered() {
    let (mut session, mut playout) = new_session();
    session.siri(true);
    // Apply the press flush.
    let mut discard = vec![0u8; FRAME_BYTES];
    playout.fill(&mut discard);

    let packets = tone_packets(PREROLL_FRAMES - 1);
    push(&mut session, &packets, 0, Instant::now());
    let mut buffer = vec![0u8; FRAME_BYTES * 2];
    assert_eq!(playout.fill(&mut buffer), buffer.len());
    assert!(
        buffer.iter().all(|byte| *byte == 0),
        "audio played before the preroll was buffered"
    );
    assert_eq!(
        playout.link().buffered_samples(),
        (PREROLL_FRAMES - 1) * FRAME_SAMPLES,
        "the preroll was consumed before playout started"
    );

    // The sixth frame starts playout, from the buffered preroll.
    session.push_transport(PREROLL_FRAMES as u16 - 1, &packets[0], Instant::now());
    let mut buffer = vec![0u8; FRAME_BYTES * 2];
    assert_eq!(playout.fill(&mut buffer), buffer.len());
    assert!(buffer.chunks_exact(2).any(|sample| sample != [0, 0]));
}

/// A consumer pulling slower than real time keeps the whole backlog within the
/// two-minute ring: playback continues with the oldest audio rather than
/// dropping live speech, exactly as the Python daemon's bounded queue did.
#[test]
fn a_slow_consumer_keeps_the_whole_backlog() {
    let (mut session, mut playout) = new_session();
    session.siri(true);
    let mut discard = vec![0u8; FRAME_BYTES];
    playout.fill(&mut discard);

    let packets = tone_packets(PREROLL_FRAMES);
    let now = Instant::now();
    push(&mut session, &packets, 0, now);
    let mut buffer = vec![0u8; FRAME_BYTES];
    playout.fill(&mut buffer);
    assert!(buffer.chunks_exact(2).any(|sample| sample != [0, 0]));

    // The consumer stalls while speech keeps arriving: nothing is dropped
    // while the backlog fits within the ring's two minutes.
    let packet = &packets[0];
    for index in packets.len() as u16..(packets.len() as u16 + 40) {
        session.push_transport(index, packet, now);
    }
    assert!(!session.stats_backlog_exceeded(), "live audio was dropped");
    assert!(playout.link().buffered_samples() > 40 * FRAME_SAMPLES);

    let mut buffer = vec![0u8; FRAME_BYTES * 2];
    assert_eq!(playout.fill(&mut buffer), buffer.len());
    assert!(
        buffer.chunks_exact(2).any(|sample| sample != [0, 0]),
        "playout re-primed after a backlog cap instead of continuing"
    );
}

/// A press shorter than the preroll still plays its tail once released, so
/// nothing is left sitting in the ring after the utterance.
#[test]
fn a_tail_shorter_than_the_preroll_plays_after_release() {
    let (mut session, mut playout) = new_session();
    session.siri(true);
    let mut discard = vec![0u8; FRAME_BYTES];
    playout.fill(&mut discard);

    let packets = tone_packets(2);
    push(&mut session, &packets, 0, Instant::now());
    session.siri(false);
    let mut buffer = vec![0u8; FRAME_BYTES * 2];
    assert_eq!(playout.fill(&mut buffer), buffer.len());
    assert!(buffer.chunks_exact(2).any(|sample| sample != [0, 0]));
    let mut drain = vec![0u8; FRAME_BYTES * 4];
    playout.fill(&mut drain);
    assert_eq!(playout.link().buffered_samples(), 0);
}

/// The release drain, as the Python daemon's AudioDrain: it waits for late
/// Bluetooth packets and the final PipeWire buffers, is satisfied once both
/// sides have been quiet past their windows, and gives up at the bound.
#[test]
fn the_release_drain_waits_for_late_packets_and_buffers() {
    let (mut session, mut playout) = new_session();
    session.siri(true);
    let mut discard = vec![0u8; FRAME_BYTES];
    playout.fill(&mut discard);

    let packets = tone_packets(PREROLL_FRAMES + 4);
    push(&mut session, &packets, 0, Instant::now());
    session.siri(false);
    // The playout has not consumed the tail yet: still queued.
    let link = playout.link().clone();
    assert!(!link.drained(Instant::now()));
    assert!(!link.wait_drained(Duration::from_millis(100), Duration::from_millis(5)));

    // Consume the tail: once both sides have been quiet, the drain passes.
    let mut buffer = vec![0u8; FRAME_BYTES * 16];
    playout.fill(&mut buffer);
    assert!(buffer.chunks_exact(2).any(|sample| sample != [0, 0]));
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !link.drained(Instant::now()) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        link.drained(Instant::now()),
        "the tail never reached the consumer"
    );
    assert!(link.wait_drained(Duration::from_secs(1), Duration::from_millis(5)));
}

/// Timestamps from before the release must not satisfy the drain, so a
/// release that arrived nothing new still waits for the late tail.
#[test]
fn stale_quiet_windows_do_not_satisfy_the_drain() {
    let (mut session, mut playout) = new_session();
    session.siri(true);
    let packets = tone_packets(4);
    push(&mut session, &packets, 0, Instant::now());
    let mut buffer = vec![0u8; FRAME_BYTES * 8];
    playout.fill(&mut buffer);
    let link = playout.link().clone();
    // Long before the release windows:
    assert!(!link.drained(Instant::now() + Duration::from_secs(60)));
    session.siri(false);
    assert!(!link.drained(Instant::now()));
}

/// A partial take that leaves samples in the ring is not an underrun: the
/// silence it carries is the consumer's timeline running ahead of the ring,
/// not a missing Bluetooth interval, as the Python send loop's has_audio rule.
#[test]
fn a_partial_take_that_leaves_audio_is_not_an_underrun() {
    let (mut session, mut playout) = new_session();
    session.siri(true);
    let mut discard = vec![0u8; FRAME_BYTES];
    playout.fill(&mut discard);

    // Enough for the preroll plus more than one buffer.
    let packets = tone_packets(PREROLL_FRAMES + 3);
    push(&mut session, &packets, 0, Instant::now());
    let mut buffer = vec![0u8; FRAME_BYTES * 2];
    playout.fill(&mut buffer);
    assert!(buffer.chunks_exact(2).any(|sample| sample != [0, 0]));
    // The next take is partial but leaves audio behind.
    let mut partial = vec![0xffu8; FRAME_BYTES * 3];
    playout.fill(&mut partial);
    assert!(playout.link().buffered_samples() > 0);
    assert_eq!(playout.link().underruns(), 0);
    // Only a take that empties the ring while receiving is an underrun.
    let mut empty = vec![0u8; FRAME_BYTES * 8];
    playout.fill(&mut empty);
    assert!(playout.link().underruns() > 0);
}
