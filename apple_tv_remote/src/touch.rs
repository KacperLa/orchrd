//! A1513 input report decoding and the touchpad-to-pointer state machine.
//!
//! The state machine is generic over its sink so it can be exercised without
//! uinput: [`InputSink`] is implemented by the virtual mouse device and by the
//! recorder used in tests.

use std::time::Instant;

use crate::layout::{Interpolator, Region, TouchZones, ZoneConfig, PAD_SIZE_MM};

/// A1513 button mask: Play/Pause, Menu and a mechanical click.
const BUTTON_PLAY: u8 = 0x08;
const BUTTON_MENU: u8 = 0x20;
const BUTTON_CLICK: u8 = 0x80;
/// Siri is reported in the two byte button report.
const SIRI_BUTTON: u8 = 0x10;

/// Pad X spans 26180/16 raw units, Y spans 106*16 raw units.
pub const X_RANGE: f64 = 26180.0 / 16.0;
pub const Y_RANGE: f64 = 106.0 * 16.0;
/// Pad reports carry a 16 bit timestamp ticking at 2461 Hz.
const TICKS_PER_SECOND: f64 = 2461.0;
const REPORT_TOUCH: u8 = 0x32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
}

/// Keys other than the touchpad click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKey {
    PlayPause,
    Menu,
}

/// Outputs of the touch state machine, expressed without evdev codes so the
/// pad logic stays device agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Motion { dx: i32, dy: i32 },
    Scroll { horizontal: i32, vertical: i32 },
    Key { button: Button, down: bool },
    RemoteKey { key: RemoteKey, down: bool },
}

pub trait InputSink {
    fn emit(&mut self, output: Output);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Touch {
    pub identifier: u8,
    pub x: i32,
    pub y: i32,
    pub pressure: u8,
}

/// Decode one seven-byte A1513 touch record.
pub fn decode_finger(data: &[u8]) -> Option<Touch> {
    if data.len() != 7 {
        return None;
    }
    // Two signed 12-bit coordinates share three bytes. The upper nibble of
    // byte 1 belongs to Y, not X; dropping it quantizes Y by 16x.
    let packed = u32::from(data[0]) | u32::from(data[1]) << 8 | u32::from(data[2]) << 16;
    let x = (((packed & 0x0FFF) as i32) ^ 0x800) - 0x800;
    let y = ((((packed >> 12) & 0x0FFF) as i32) ^ 0x800) - 0x800;
    Some(Touch {
        identifier: 1 - ((data[6] & 0x08) >> 3),
        x,
        y,
        pressure: data[5],
    })
}

/// Decode the touch records in a multiplexed A1513 input report.
fn decode_touches(report: &[u8]) -> ([Option<Touch>; 2], usize) {
    let mut touches = [None, None];
    if report.len() < 13 || report[0] & 0x03 == 0 {
        return (touches, 0);
    }
    let payload = &report[2..];
    let count = if report[0] & 0x02 != 0 && payload.len() >= 18 {
        2
    } else {
        1
    };
    let mut stored = 0;
    for index in 0..count {
        let start = 4 + index * 7;
        let Some(record) = payload.get(start..start + 7) else {
            break;
        };
        if let Some(touch) = decode_finger(record) {
            if touch.pressure > 0 {
                touches[stored] = Some(touch);
                stored += 1;
            }
        }
    }
    (touches, stored)
}

/// Pointer and scroll behaviour derived from touchpad millimetres.
#[derive(Debug, Clone, Copy)]
pub struct MotionConfig {
    pub x_scale: f64,
    pub y_scale: f64,
    pub acceleration: f64,
    pub acceleration_threshold: f64,
    pub edge_mm: f64,
    pub corner_mm: f64,
    pub scroll_step_mm: f64,
    /// Seconds of interpolation per pointer step; zero emits each step whole.
    pub interpolation_s: f64,
}

impl MotionConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sensitivity: f64,
        pointer_speed: f64,
        acceleration: f64,
        acceleration_threshold: f64,
        edge_mm: f64,
        corner_mm: f64,
        scroll_step_mm: f64,
        interpolation_s: f64,
    ) -> Self {
        Self {
            x_scale: sensitivity * pointer_speed * PAD_SIZE_MM / X_RANGE,
            y_scale: sensitivity * pointer_speed * PAD_SIZE_MM / Y_RANGE,
            acceleration,
            acceleration_threshold,
            edge_mm,
            corner_mm,
            scroll_step_mm,
            interpolation_s,
        }
    }

    /// Interpolation time in whole milliseconds, for logging.
    pub fn interpolation_ms(&self) -> u64 {
        (self.interpolation_s * 1000.0).round() as u64
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub reports: u64,
    pub duplicates: u64,
    pub moves: u64,
    pub clicks: u64,
}

pub struct TouchPad<S: InputSink> {
    sink: S,
    zones: TouchZones,
    config: MotionConfig,
    /// Latched when a contact starts so a wobbling edge swipe keeps scrolling
    /// and a button-only click keeps the corner of the contact that made it.
    region: Region,
    click_corner: bool,
    button_code: Button,
    last_sample: [u8; 18],
    last_sample_len: usize,
    device_tick: Option<u16>,
    device_time: f64,
    last_arrival: Option<Instant>,
    previous_touch: Option<Touch>,
    previous_timestamp: Option<f64>,
    output_x_remainder: f64,
    output_y_remainder: f64,
    /// Pointer travel queued for later ticks, on the arrival clock.
    interpolator: Interpolator,
    /// Arrival-clock origin the interpolator measures deadlines from.
    clock_origin: Option<Instant>,
    button_down: bool,
    play_down: bool,
    menu_down: bool,
    pub stats: Stats,
}

impl<S: InputSink> TouchPad<S> {
    pub fn new(sink: S, config: MotionConfig) -> Result<Self, &'static str> {
        let zones = TouchZones::new(
            PAD_SIZE_MM,
            ZoneConfig {
                edge_mm: config.edge_mm,
                corner_mm: config.corner_mm,
                scroll_step_mm: config.scroll_step_mm,
                ..Default::default()
            },
        )?;
        Ok(Self {
            sink,
            zones,
            config,
            region: Region::Center,
            click_corner: false,
            button_code: Button::Left,
            last_sample: [0; 18],
            last_sample_len: 0,
            device_tick: None,
            device_time: 0.0,
            last_arrival: None,
            previous_touch: None,
            previous_timestamp: None,
            output_x_remainder: 0.0,
            output_y_remainder: 0.0,
            interpolator: Interpolator::new(config.interpolation_s),
            clock_origin: None,
            button_down: false,
            play_down: false,
            menu_down: false,
            stats: Stats::default(),
        })
    }

    pub fn sink(&self) -> &S {
        &self.sink
    }

    pub fn into_sink(self) -> S {
        self.sink
    }

    pub fn process(&mut self, report: &[u8], arrival: Instant) {
        if report.len() < 2 || (report.len() != 2 && report[0] & !0x07 != 0) {
            return;
        }
        self.update_remote_keys(report[1]);
        if let Some(last) = self.last_arrival {
            if arrival.duration_since(last).as_secs_f64() > 1.0 {
                // Rebase after sleep/disconnection, including ambiguous timer wraps.
                self.previous_touch = None;
                self.device_tick = None;
                self.last_sample_len = 0;
                self.region = Region::Center;
                self.click_corner = false;
                self.zones.reset();
                self.cancel_motion();
            }
        }
        self.last_arrival = Some(arrival);
        self.stats.reports += 1;

        // Decode before button handling so a corner click uses the current finger.
        let touch = self.report_touch(report);
        let continuing = match (touch, self.previous_touch) {
            (Some(current), Some(previous)) => current.identifier == previous.identifier,
            _ => false,
        };
        if let Some(touch) = touch {
            if !continuing {
                // Latch the strip for the whole contact: the strip is too narrow
                // to track through a swipe, and drifting out must not start a drag.
                self.region = self.zones.region(touch.x, touch.y);
                self.click_corner = self.zones.is_click_corner(touch.x, touch.y);
                self.zones.reset();
            }
        }

        let clicked = report[1] & BUTTON_CLICK != 0;
        if clicked != self.button_down {
            if clicked {
                // Button-only reports carry no coordinates; then the latched
                // contact position decides, otherwise the current finger does.
                let corner = match touch {
                    Some(touch) => self.zones.is_click_corner(touch.x, touch.y),
                    None => self.click_corner,
                };
                self.button_code = if corner { Button::Right } else { Button::Left };
                self.stats.clicks += 1;
            }
            self.cancel_motion();
            self.sink.emit(Output::Key {
                button: self.button_code,
                down: clicked,
            });
            self.button_down = clicked;
        }

        // Button/IMU-only reports do not mean the finger lifted.
        if report[0] & 3 == 0 {
            return;
        }
        let expected = if report[0] & 0x02 != 0 { 20 } else { 13 };
        if report.len() < expected || report[2] != REPORT_TOUCH {
            return;
        }
        let sample = &report[2..expected];
        if sample.len() == self.last_sample_len && self.last_sample[..sample.len()] == *sample {
            self.stats.duplicates += 1;
            return;
        }
        self.last_sample[..sample.len()].copy_from_slice(sample);
        self.last_sample_len = sample.len();

        let tick = u16::from_le_bytes([report[3], report[4]]);
        if let Some(previous) = self.device_tick {
            let ticks = tick.wrapping_sub(previous);
            if ticks == 0 || ticks > 0x8000 {
                return;
            }
            self.device_time += f64::from(ticks) / TICKS_PER_SECOND;
        }
        self.device_tick = Some(tick);
        let timestamp = self.device_time;

        match (touch, continuing) {
            (Some(touch), true) => {
                if let Some(previous) = self.previous_touch {
                    self.handle_motion(touch, previous, timestamp, arrival);
                }
            }
            _ => self.cancel_motion(),
        }

        if touch.is_some() {
            self.previous_timestamp = Some(timestamp);
        } else {
            self.previous_timestamp = None;
            self.region = Region::Center;
            self.click_corner = false;
            self.zones.reset();
        }
        self.previous_touch = touch;
    }

    fn handle_motion(&mut self, touch: Touch, previous: Touch, timestamp: f64, arrival: Instant) {
        let dx_raw = touch.x - previous.x;
        let dy_raw = touch.y - previous.y;
        // Finger order changes and a newly placed finger can otherwise jump.
        if f64::from(dx_raw).abs() >= X_RANGE / 3.0 || f64::from(dy_raw).abs() >= Y_RANGE / 3.0 {
            self.cancel_motion();
            return;
        }
        let dx_mm = f64::from(dx_raw) * PAD_SIZE_MM / X_RANGE;
        let dy_mm = f64::from(dy_raw) * PAD_SIZE_MM / Y_RANGE;
        if self.region != Region::Center {
            // Pad Y grows toward the top; scroll coordinates grow down.
            let (horizontal, vertical) = self.zones.scroll(self.region, dx_mm, -dy_mm);
            if horizontal != 0 || vertical != 0 {
                self.sink.emit(Output::Scroll {
                    horizontal,
                    vertical,
                });
            }
            return;
        }
        if dx_raw == 0 && dy_raw == 0 {
            self.interpolator.clear();
            return;
        }
        let elapsed = (timestamp - self.previous_timestamp.unwrap_or(timestamp)).max(1e-3);
        let speed = dx_mm.hypot(dy_mm) / elapsed;
        // A bounded velocity curve gives quick swipes more reach while
        // retaining one-to-one precision at low speed.
        let speed_squared = speed * speed;
        let threshold_squared = self.config.acceleration_threshold.powi(2);
        let gain =
            1.0 + self.config.acceleration * (speed_squared / (speed_squared + threshold_squared));
        let dx = f64::from(dx_raw) * self.config.x_scale * gain;
        // The remote's Y coordinate increases toward its top; REL_Y increases down.
        let dy = -f64::from(dy_raw) * self.config.y_scale * gain;
        self.queue_motion(arrival, dx, dy);
    }

    /// Hand one step of pointer travel to the interpolator, or emit it whole.
    /// Interpolation runs on the arrival clock, not the device timestamps the
    /// speed gain uses: it paces output against real time, and the device clock
    /// stalls whenever the finger pauses.
    fn queue_motion(&mut self, now: Instant, dx: f64, dy: f64) {
        if !self.interpolator.enabled() {
            self.emit_motion(dx, dy);
            return;
        }
        let seconds = self.clock_seconds(now);
        // Finishing travel queued against a new direction would keep pushing
        // the pointer the old way while the finger has already turned.
        let (pending_x, pending_y) = self.interpolator.pending();
        if dx * pending_x < 0.0 || dy * pending_y < 0.0 {
            self.cancel_motion();
        }
        self.interpolator.push(seconds, dx, dy);
    }

    /// Advance interpolation to `now` and emit the travel that came due. The
    /// caller owns the tick cadence, so the pad stays runnable without a timer.
    pub fn tick(&mut self, now: Instant) {
        if !self.interpolator.enabled() {
            return;
        }
        let seconds = self.clock_seconds(now);
        let (dx, dy) = self.interpolator.take(seconds);
        self.emit_motion(dx, dy);
    }

    /// Whether later ticks still owe pointer travel.
    pub fn interpolation_pending(&self) -> bool {
        self.interpolator.is_pending()
    }

    fn clock_seconds(&mut self, now: Instant) -> f64 {
        match self.clock_origin {
            Some(origin) => now.duration_since(origin).as_secs_f64(),
            None => {
                self.clock_origin = Some(now);
                0.0
            }
        }
    }

    /// Drop queued pointer travel and the sub-pixel fraction. Travel left over
    /// from a finished gesture must not move the pointer into the next one.
    fn cancel_motion(&mut self) {
        self.interpolator.clear();
        self.output_x_remainder = 0.0;
        self.output_y_remainder = 0.0;
    }

    fn emit_motion(&mut self, dx: f64, dy: f64) {
        self.output_x_remainder += dx;
        self.output_y_remainder += dy;
        let dx = self.output_x_remainder as i32;
        let dy = self.output_y_remainder as i32;
        self.output_x_remainder -= f64::from(dx);
        self.output_y_remainder -= f64::from(dy);
        if dx != 0 || dy != 0 {
            self.sink.emit(Output::Motion { dx, dy });
            self.stats.moves += 1;
        }
    }

    fn update_remote_keys(&mut self, buttons: u8) {
        for (mask, key, down) in [
            (BUTTON_PLAY, RemoteKey::PlayPause, &mut self.play_down),
            (BUTTON_MENU, RemoteKey::Menu, &mut self.menu_down),
        ] {
            let pressed = buttons & mask != 0;
            if pressed != *down {
                self.sink.emit(Output::RemoteKey { key, down: pressed });
                *down = pressed;
            }
        }
    }

    /// Return the tracked finger in a touch report without changing state.
    fn report_touch(&self, report: &[u8]) -> Option<Touch> {
        if report[0] & 3 == 0 {
            return None;
        }
        let expected = if report[0] & 0x02 != 0 { 20 } else { 13 };
        if report.len() < expected || report[2] != REPORT_TOUCH {
            return None;
        }
        let (touches, count) = decode_touches(report);
        let touches = touches[..count].iter().flatten();
        match self.previous_touch {
            Some(previous) => touches
                .clone()
                .find(|touch| touch.identifier == previous.identifier)
                .or_else(|| touches.clone().next())
                .copied(),
            None => touches.copied().next(),
        }
    }

    /// Release every held button; called when the remote disconnects.
    pub fn release(&mut self) {
        self.cancel_motion();
        self.zones.reset();
        self.region = Region::Center;
        self.click_corner = false;
        self.previous_touch = None;
        self.last_arrival = None;
        for (key, down) in [
            (RemoteKey::PlayPause, &mut self.play_down),
            (RemoteKey::Menu, &mut self.menu_down),
        ] {
            if *down {
                self.sink.emit(Output::RemoteKey { key, down: false });
                *down = false;
            }
        }
        if self.button_down {
            self.sink.emit(Output::Key {
                button: self.button_code,
                down: false,
            });
            self.button_down = false;
        }
    }
}

/// Whether a two byte button report has the Siri button held.
pub fn siri_pressed(report: &[u8]) -> bool {
    report.len() == 2 && report[1] & SIRI_BUTTON != 0
}
