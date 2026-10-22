//! Fixtures shared by the integration tests: the shipped pad geometry, an Opus
//! tone encoder, and the recording touchpad harness. Every test binary compiles
//! this module, so items a given binary does not use are allowed to be dead.
#![allow(dead_code)]

use std::time::{Duration, Instant};

use apple_tv_remote::audio::FRAME_SAMPLES;
use apple_tv_remote::layout::{
    ZoneConfig, DEFAULT_CORNER_MM, DEFAULT_EDGE_MM, DEFAULT_SCROLL_STEP_MM, PAD_SIZE_MM,
};
use apple_tv_remote::touch::{Button, InputSink, MotionConfig, Output, RemoteKey, TouchPad};

/// The pad geometry and strip widths the daemon ships with.
pub const CONFIG: ZoneConfig = ZoneConfig {
    edge_mm: DEFAULT_EDGE_MM,
    corner_mm: DEFAULT_CORNER_MM,
    scroll_step_mm: DEFAULT_SCROLL_STEP_MM,
    x_min: -1818,
    x_max: -182,
    y_min: -1074,
    y_max: 614,
};

pub fn x_per_mm() -> f64 {
    f64::from(CONFIG.x_max - CONFIG.x_min) / PAD_SIZE_MM
}

fn y_per_mm() -> f64 {
    f64::from(CONFIG.y_max - CONFIG.y_min) / PAD_SIZE_MM
}

pub fn center() -> (i32, i32) {
    (
        CONFIG.x_min + (CONFIG.x_max - CONFIG.x_min) / 2,
        CONFIG.y_min + (CONFIG.y_max - CONFIG.y_min) / 2,
    )
}

/// A point `mm` inside the pad's measured right extreme, at mid height.
pub fn inside_right(mm: f64) -> (i32, i32) {
    (CONFIG.x_max - (mm * x_per_mm()).round() as i32, center().1)
}

/// A point `mm` inside the pad's measured top extreme, at mid width.
pub fn inside_top(mm: f64) -> (i32, i32) {
    (center().0, CONFIG.y_max - (mm * y_per_mm()).round() as i32)
}

pub fn inside_corner(mm: f64) -> (i32, i32) {
    (
        CONFIG.x_max - (mm * x_per_mm()).round() as i32,
        CONFIG.y_max - (mm * y_per_mm()).round() as i32,
    )
}

/// A 440 Hz tone as 20 ms Opus frames with a continuous phase, so the frames
/// encode one signal.
pub fn tone_packets(frames: usize) -> Vec<Vec<u8>> {
    let encoder = audiopus::coder::Encoder::new(
        audiopus::SampleRate::Hz16000,
        audiopus::Channels::Mono,
        audiopus::Application::Voip,
    )
    .expect("encoder");
    let mut packets = Vec::with_capacity(frames);
    for frame in 0..frames {
        let mut pcm = [0i16; FRAME_SAMPLES];
        for (index, sample) in pcm.iter_mut().enumerate() {
            let t = (frame * FRAME_SAMPLES + index) as f32 / 16_000.0;
            *sample = (f32::sin(std::f32::consts::TAU * 440.0 * t) * 8000.0) as i16;
        }
        let mut packet = [0u8; 512];
        let len = encoder.encode(&pcm, &mut packet).expect("encode");
        packets.push(packet[..len].to_vec());
    }
    packets
}

/// One frame of the same tone.
pub fn tone_packet() -> Vec<u8> {
    tone_packets(1).pop().expect("one packet")
}

/// Builds A1513 touch reports with a hand-advanced device clock.
pub struct ReportBuilder {
    tick: u16,
}

impl ReportBuilder {
    pub fn new() -> Self {
        Self { tick: 0 }
    }

    fn advance(&mut self, seconds: f64) {
        self.tick = self.tick.wrapping_add((seconds * 2461.0).round() as u16);
    }

    fn report(&self, x: Option<i32>, y: Option<i32>, buttons: u8) -> Vec<u8> {
        let finger: [u8; 7] = match (x, y) {
            (Some(x), Some(y)) => {
                let packed = (x as u32 & 0x0FFF) | ((y as u32 & 0x0FFF) << 12);
                let bytes = packed.to_le_bytes();
                [bytes[0], bytes[1], bytes[2], 0, 0, 100, 0]
            }
            _ => [0; 7],
        };
        let mut report = vec![
            0x01,
            buttons,
            0x32,
            self.tick as u8,
            (self.tick >> 8) as u8,
            0,
        ];
        report.extend_from_slice(&finger);
        report
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    Motion(i32, i32),
    Scroll(i32, i32),
    Key(Button, bool),
    RemoteKey(RemoteKey, bool),
}

#[derive(Default)]
pub struct Recorder {
    pub events: Vec<Recorded>,
}

impl InputSink for Recorder {
    fn emit(&mut self, output: Output) {
        self.events.push(match output {
            Output::Motion { dx, dy } => Recorded::Motion(dx, dy),
            Output::Scroll {
                horizontal,
                vertical,
            } => Recorded::Scroll(horizontal, vertical),
            Output::Key { button, down } => Recorded::Key(button, down),
            Output::RemoteKey { key, down } => Recorded::RemoteKey(key, down),
        });
    }
}

impl Recorder {
    pub fn motions(&self) -> Vec<(i32, i32)> {
        self.events
            .iter()
            .filter_map(|event| match event {
                Recorded::Motion(dx, dy) => Some((*dx, *dy)),
                _ => None,
            })
            .collect()
    }

    pub fn scrolls(&self, horizontal: bool) -> Vec<i32> {
        self.events
            .iter()
            .filter_map(|event| match event {
                Recorded::Scroll(h, v) => Some(if horizontal { *h } else { *v }),
                _ => None,
            })
            .filter(|value| *value != 0)
            .collect()
    }

    pub fn keys(&self, button: Button) -> Vec<bool> {
        self.events
            .iter()
            .filter_map(|event| match event {
                Recorded::Key(found, down) if *found == button => Some(*down),
                _ => None,
            })
            .collect()
    }
}

/// A touchpad driven by synthetic reports, with the results recorded instead of
/// written to uinput.
pub struct Harness {
    pub pad: TouchPad<Recorder>,
    builder: ReportBuilder,
    now: Instant,
}

impl Harness {
    pub fn new() -> Self {
        Self::configured(DEFAULT_SCROLL_STEP_MM, 0.0)
    }

    pub fn with_scroll_step(step: f64) -> Self {
        Self::configured(step, 0.0)
    }

    pub fn with_interpolation(seconds: f64) -> Self {
        Self::configured(DEFAULT_SCROLL_STEP_MM, seconds)
    }

    fn configured(step: f64, interpolation_s: f64) -> Self {
        // One raw pad unit per pixel keeps the assertions short.
        let mut config = MotionConfig::new(
            1.0,
            1.0,
            0.0,
            80.0,
            CONFIG.edge_mm,
            CONFIG.corner_mm,
            step,
            interpolation_s,
        );
        config.x_scale = 1.0;
        config.y_scale = 1.0;
        Self {
            pad: TouchPad::new(Recorder::default(), config).expect("valid config"),
            builder: ReportBuilder::new(),
            now: Instant::now(),
        }
    }

    /// Advance the arrival clock and run one interpolation tick.
    pub fn tick(&mut self, seconds: f64) {
        self.now += Duration::from_secs_f64(seconds);
        self.pad.tick(self.now);
    }

    pub fn send(&mut self, seconds: f64, x: Option<i32>, y: Option<i32>, buttons: u8) {
        self.builder.advance(seconds);
        let report = self.builder.report(x, y, buttons);
        self.now += Duration::from_secs_f64(seconds);
        self.pad.process(&report, self.now);
    }

    pub fn drag(&mut self, start: (i32, i32), dx: i32, dy: i32, steps: i32) {
        let (mut x, mut y) = start;
        self.send(0.02, Some(x), Some(y), 0);
        for _ in 0..steps {
            x += dx;
            y += dy;
            self.send(0.02, Some(x), Some(y), 0);
        }
    }

    pub fn recorder(&self) -> &Recorder {
        self.pad.sink()
    }
}
