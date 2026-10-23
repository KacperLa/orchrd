//! Opus decoding and jitter-buffered playout.
//!
//! Decoding happens on the transport thread as packets arrive; the PipeWire
//! process callback only copies already-decoded PCM out of a wait-free ring
//! buffer. There is no helper process, no per-frame wakeup and no allocation
//! in the realtime path.
//!
//! Playout is rate-matched: every buffer the consumer's timeline advances is
//! taken from the ring or padded with silence, so the delivered audio keeps
//! the wall-clock length of the utterance whatever cadence the consumer
//! pulls at. The ring holds two minutes, the same bound the Python daemon
//! kept: a consumer that attaches late or pulls in bursts hears the whole
//! utterance, and speech older than that means the consumer is gone, which
//! is fatal rather than a silent truncation.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use audiopus::coder::{Decoder, GenericCtl};
use audiopus::{Channels, SampleRate};
use rtrb::{Consumer, Producer, RingBuffer};

/// 16 kHz mono, 20 ms frames.
pub const SAMPLE_RATE: u32 = 16_000;
pub const FRAME_SAMPLES: usize = 320;
pub const FRAME_BYTES: usize = FRAME_SAMPLES * 2;
/// Ring capacity: two minutes of decoded audio, the backlog bound the Python
/// daemon kept. It absorbs any consumer cadence, including the whole span
/// between a press and the consumer attaching; older speech means the
/// consumer is gone, which stops the daemon instead of being dropped.
pub const RING_SAMPLES: usize = 120 * SAMPLE_RATE as usize;
/// Decoder headroom for one Opus packet of up to 120 ms, the longest libopus
/// decodes at 16 kHz; the remote itself sends 20 ms frames.
const MAX_DECODE_SAMPLES: usize = 6 * FRAME_SAMPLES;
/// Frames buffered before playout starts, absorbing bursty Bluetooth delivery.
pub const PREROLL_FRAMES: usize = 6;
const MAX_CONCEALED: u32 = 10;
/// Drain quiet windows: how long the ring must have been silent on both the
/// transport and the consumer side before an utterance counts as delivered.
pub const RECEIVE_QUIET: Duration = Duration::from_millis(500);
pub const PLAY_QUIET: Duration = Duration::from_millis(200);
/// Upper bound on the release drain and its polling interval, mirroring the
/// Python daemon's ten second drain error.
pub const DRAIN_BOUND: Duration = Duration::from_secs(10);
pub const DRAIN_POLL: Duration = Duration::from_millis(20);

/// The clock the drain stamps share. Once-initialized so every stamp comes
/// from the same timeline, and read without a syscall on Linux.
fn clock() -> Instant {
    static BOOT: OnceLock<Instant> = OnceLock::new();
    *BOOT.get_or_init(Instant::now)
}

/// Nanoseconds since the shared clock origin, for lock-free drain stamps.
fn stamp() -> u64 {
    clock().elapsed().as_nanos() as u64
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub frames: u64,
    pub errors: u64,
    pub sequence_skips: u64,
    pub concealed: u64,
    pub peak: u16,
}

/// Shared state between the transport and the realtime playout.
///
/// Dropping stale audio cannot simply clear the ring, because the transport
/// only owns the write end. Instead the transport records how much it has
/// produced, and a new press marks that position as the start of the session;
/// the playout then discards exactly the samples queued before the press and
/// nothing that arrived after it.
pub struct Link {
    /// Total samples the transport has written.
    produced: AtomicUsize,
    /// Number of produced samples that precede the current session.
    drop_until: AtomicUsize,
    /// Bumped once per push-to-talk press, so the playout can tell a new
    /// session (re-prime) from a mid-session state change (keep playing).
    session: AtomicU64,
    /// Total samples the playout has taken, including dropped ones.
    consumed: AtomicUsize,
    pub receiving: AtomicBool,
    pub underruns: AtomicU64,
    pub buffered: AtomicUsize,
    /// Nanoseconds of the last decoded sample written to the ring; zero until
    /// the first frame of the process arrives.
    last_received: AtomicU64,
    /// Nanoseconds of the last sample the playout took from the ring; zero
    /// until the first take.
    last_played: AtomicU64,
    /// Nanoseconds of the last Siri release, from which the drain quiet
    /// windows are measured, as the Python drain floored them at
    /// `released_at`; zero until the first release.
    released: AtomicU64,
    /// Set when decoded audio no longer fits the ring: the consumer is gone,
    /// so recording would silently lose speech. Fatal, as the Python daemon's
    /// two-minute backlog error was.
    overflow: AtomicBool,
}

impl Link {
    fn new() -> Self {
        Self {
            produced: AtomicUsize::new(0),
            drop_until: AtomicUsize::new(0),
            session: AtomicU64::new(0),
            consumed: AtomicUsize::new(0),
            receiving: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            buffered: AtomicUsize::new(0),
            last_received: AtomicU64::new(0),
            last_played: AtomicU64::new(0),
            released: AtomicU64::new(0),
            overflow: AtomicBool::new(false),
        }
    }

    /// Discard everything written before this point and start a new session.
    fn mark_session_start(&self) {
        self.drop_until
            .store(self.produced.load(Ordering::Acquire), Ordering::Release);
        self.session.fetch_add(1, Ordering::AcqRel);
    }

    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    pub fn buffered_samples(&self) -> usize {
        self.buffered.load(Ordering::Acquire)
    }

    /// Decoded audio no longer fits the ring: the consumer is gone.
    pub fn is_overflow(&self) -> bool {
        self.overflow.load(Ordering::Acquire)
    }

    fn mark_received(&self) {
        self.last_received.store(stamp(), Ordering::Release);
    }

    fn mark_played(&self) {
        self.last_played.store(stamp(), Ordering::Release);
    }

    /// Record the Siri release the drain quiet windows are measured from.
    fn mark_released(&self) {
        self.released.store(stamp(), Ordering::Release);
    }

    /// Whether the tail of an utterance has fully reached the consumer, as
    /// the Python daemon's drain did: nothing left in the ring, no samples
    /// arriving in the last half second after the release, and none reaching
    /// the consumer in the last fifth of one. A missing stamp is floored at
    /// the release, exactly as the Python drain's `or released_at` did, so a
    /// release with nothing new still waits the quiet windows out.
    pub fn drained(&self, now: Instant) -> bool {
        if self.buffered.load(Ordering::Acquire) > 0 {
            return false;
        }
        let released = self.released.load(Ordering::Acquire);
        if released == 0 {
            return false;
        }
        let received = self.last_received.load(Ordering::Acquire).max(released);
        let played = self.last_played.load(Ordering::Acquire).max(released);
        let elapsed = now.duration_since(clock());
        elapsed - Duration::from_nanos(received) >= RECEIVE_QUIET
            && elapsed - Duration::from_nanos(played) >= PLAY_QUIET
    }

    /// Wait for the utterance tail to reach the consumer, or the drain bound
    /// to elapse. Returns false on the bound, as the Python daemon's ten
    /// second drain error did.
    pub fn wait_drained(&self, bound: Duration, sleep: Duration) -> bool {
        let deadline = Instant::now() + bound;
        loop {
            if self.drained(Instant::now()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(sleep.min(deadline.saturating_duration_since(Instant::now())));
        }
    }
}

/// Transport side: Opus packets in, PCM out.
pub struct Session {
    producer: Producer<i16>,
    link: Arc<Link>,
    decoder: Decoder,
    previous_sequence: Option<u16>,
    last_arrival: Option<Instant>,
    pressed: bool,
    pub stats: Stats,
}

impl Session {
    pub fn new() -> audiopus::Result<(Self, Playout)> {
        let (producer, consumer) = RingBuffer::new(RING_SAMPLES);
        let link = Arc::new(Link::new());
        let playout = Playout {
            consumer,
            link: link.clone(),
            playing: false,
            applied_session: 0,
            consumed: 0,
        };
        Ok((
            Self {
                producer,
                link,
                decoder: Decoder::new(SampleRate::Hz16000, Channels::Mono)?,
                previous_sequence: None,
                last_arrival: None,
                pressed: false,
                stats: Stats::default(),
            },
            playout,
        ))
    }

    /// Milliseconds of decoded audio currently waiting in the ring, as the
    /// Python daemon's release drain reported.
    pub fn link_queued_ms(&self) -> u64 {
        (self.link.buffered_samples() as u64) * 1000 / u64::from(SAMPLE_RATE)
    }

    /// Whether the decoded backlog no longer fits the ring.
    pub fn stats_backlog_exceeded(&self) -> bool {
        self.link.is_overflow()
    }

    /// Track the Siri button. Returns the press/release transition, if any.
    pub fn siri(&mut self, down: bool) -> Option<bool> {
        if down == self.pressed {
            return None;
        }
        self.pressed = down;
        if down {
            // Reset prediction between separate push-to-talk sessions and drop
            // anything queued by the previous one.
            let _ = self.decoder.reset_state();
            self.link.mark_session_start();
            self.link.receiving.store(true, Ordering::Release);
            self.previous_sequence = None;
            self.last_arrival = None;
        } else {
            self.link.receiving.store(false, Ordering::Release);
            self.link.mark_released();
        }
        Some(down)
    }

    /// The shared transport/playout state, for the release drain.
    pub fn link(&self) -> Arc<Link> {
        self.link.clone()
    }

    pub fn push_transport(&mut self, sequence: u16, payload: &[u8], now: Instant) {
        if let Some(previous) = self.previous_sequence {
            let difference = sequence.wrapping_sub(previous);
            if difference == 0 {
                return;
            }
            if difference > 0x8000 {
                let _ = self.decoder.reset_state();
            } else if difference > 1 {
                self.stats.sequence_skips += u64::from(difference - 1);
                // The remote's counter can advance without a missing audio
                // interval. Only conceal skips corroborated by elapsed
                // delivery time, otherwise speech is stretched: of the
                // intervals the gap spans, the last one carried the frame that
                // just arrived, so only the earlier ones are missing audio.
                // The half interval absorbs delivery jitter.
                let elapsed = self
                    .last_arrival
                    .map(|last| now.duration_since(last))
                    .unwrap_or(Duration::ZERO);
                let intervals = ((elapsed.as_secs_f64() + 0.010) / 0.020).floor();
                let timed = (intervals - 1.0).max(0.0) as u32;
                let conceal = timed.min(u32::from(difference - 1)).min(MAX_CONCEALED);
                for _ in 0..conceal {
                    if self.decode(None) {
                        self.stats.concealed += 1;
                    }
                }
            }
        }
        self.previous_sequence = Some(sequence);
        self.last_arrival = Some(now);
        self.decode(Some(payload));
    }

    fn decode(&mut self, payload: Option<&[u8]>) -> bool {
        // Concealment (no payload) must ask for one 20 ms frame: libopus
        // synthesizes exactly `output.len() / channels` samples, so a larger
        // buffer would stretch the audio instead of filling the gap.
        let pcm = &mut [0i16; MAX_DECODE_SAMPLES][..if payload.is_some() {
            MAX_DECODE_SAMPLES
        } else {
            FRAME_SAMPLES
        }];
        let result = match payload {
            Some(bytes) => self.decoder.decode(Some(bytes), &mut *pcm, false),
            None => self.decoder.decode(None::<&[u8]>, &mut *pcm, false),
        };
        let samples = match result {
            Ok(samples) => samples,
            Err(_) => {
                self.stats.errors += 1;
                // A failed concealment still owes its interval: pad silence so
                // the frames after the gap keep their place in the timeline.
                if payload.is_none() {
                    let mut pushed = 0usize;
                    while pushed < FRAME_SAMPLES && self.producer.push(0).is_ok() {
                        pushed += 1;
                    }
                    self.link.produced.fetch_add(pushed, Ordering::Release);
                }
                return false;
            }
        };
        if payload.is_some() {
            self.stats.frames += 1;
            let peak = pcm[..samples]
                .iter()
                .map(|s| s.unsigned_abs())
                .max()
                .unwrap_or(0);
            self.stats.peak = self.stats.peak.max(peak);
        }
        let mut pushed = 0usize;
        for value in &pcm[..samples] {
            if self.producer.push(*value).is_err() {
                // A full ring means speech older than the two-minute bound is
                // still waiting for a consumer that is gone. Dropping the new
                // audio, as an earlier version did, silently truncates the
                // recording; instead the fault is fatal so the failure is
                // visible and the daemon restarts.
                self.link.overflow.store(true, Ordering::Release);
                break;
            }
            pushed += 1;
        }
        self.link.produced.fetch_add(pushed, Ordering::Release);
        if pushed == samples {
            self.link.mark_received();
        }
        self.link
            .buffered
            .store(RING_SAMPLES - self.producer.slots(), Ordering::Release);
        pushed == samples
    }
}

/// Audio side: fills PipeWire buffers from the decoded PCM ring.
pub struct Playout {
    consumer: Consumer<i16>,
    link: Arc<Link>,
    playing: bool,
    /// Session whose drop mark has been applied, so a new press re-primes.
    applied_session: u64,
    /// Samples already taken from the ring, for the drop mark.
    consumed: usize,
}

impl Playout {
    pub fn link(&self) -> &Arc<Link> {
        &self.link
    }

    /// Write one whole PipeWire buffer, padding whatever the ring cannot
    /// supply with silence, and return the number of bytes written. Every
    /// call advances the consumer's timeline by the full buffer, so playout
    /// is rate-matched to real time at any cadence the consumer pulls at,
    /// exactly as the Python daemon's 20 ms sender did.
    pub fn fill(&mut self, data: &mut [u8]) -> usize {
        let session = self.link.session.load(Ordering::Acquire);
        if session != self.applied_session {
            // A new push-to-talk press: drop the previous session's backlog
            // and re-prime, so playout only starts once enough live audio is
            // buffered to ride out bursty Bluetooth delivery.
            self.applied_session = session;
            self.playing = false;
        }
        // A press flush is applied by the first process cycle after it.
        self.apply_drop_mark();
        let available = self.consumer.slots();
        if !self.playing {
            let receiving = self.link.receiving.load(Ordering::Acquire);
            if available >= PREROLL_FRAMES * FRAME_SAMPLES || (!receiving && available > 0) {
                self.playing = true;
            }
        }
        let slots = data.len() / 2;
        let mut taken = 0usize;
        // Before playout starts the ring is left alone: the preroll is the
        // jitter budget that keeps the playout clock from falling behind the
        // live audio, so an utterance is neither chopped up with padded
        // silence nor still draining long after the release.
        if self.playing {
            while taken < slots {
                let Ok(value) = self.consumer.pop() else {
                    break;
                };
                self.consumed += 1;
                data[taken * 2..taken * 2 + 2].copy_from_slice(&value.to_le_bytes());
                taken += 1;
            }
            if taken > 0 {
                self.link.mark_played();
            }
            // Only a take that emptied the ring is an underrun: a partial take
            // that left samples behind was served from buffered audio, and the
            // silence padding it carries is just the consumer's timeline
            // running ahead of the ring, not a missing Bluetooth interval.
            if taken < slots
                && self.consumer.slots() == 0
                && self.link.receiving.load(Ordering::Acquire)
            {
                self.link.underruns.fetch_add(1, Ordering::Relaxed);
            }
        }
        // Whatever the ring could not supply is silence; the timeline has
        // advanced either way, so the utterance keeps its spoken length.
        for byte in &mut data[taken * 2..] {
            *byte = 0;
        }
        // `Consumer::slots` is the number of items still available to read.
        self.link
            .buffered
            .store(self.consumer.slots(), Ordering::Release);
        self.link.consumed.store(self.consumed, Ordering::Release);
        data.len()
    }

    /// Pop exactly the audio condemned by the current drop mark, counting it
    /// consumed so a later cap still measures the backlog correctly.
    fn apply_drop_mark(&mut self) {
        let mark = self.link.drop_until.load(Ordering::Acquire);
        while self.consumed < mark {
            match self.consumer.pop() {
                Ok(_) => self.consumed += 1,
                Err(_) => break,
            }
        }
        self.link.consumed.store(self.consumed, Ordering::Release);
    }
}
