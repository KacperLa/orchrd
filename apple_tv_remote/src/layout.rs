//! Touch pad geometry: scroll strips and the corner right-click.
//!
//! The pad does not report coordinates centred on zero, so distances are
//! measured inward from the measured extremes of reachable finger positions.

pub const DEFAULT_EDGE_MM: f64 = 2.0;
pub const DEFAULT_CORNER_MM: f64 = 4.0;
pub const DEFAULT_SCROLL_STEP_MM: f64 = 3.0;
pub const PAD_SIZE_MM: f64 = 37.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Center,
    Right,
    Top,
    Corner,
}

#[derive(Debug, Clone, Copy)]
pub struct ZoneConfig {
    pub edge_mm: f64,
    pub corner_mm: f64,
    pub scroll_step_mm: f64,
    /// Measured by sweeping an A1513 pad to all four extremes: x and y grow
    /// toward the right and top, and the pad's box is far from the origin.
    pub x_min: i32,
    pub x_max: i32,
    pub y_min: i32,
    pub y_max: i32,
}

impl Default for ZoneConfig {
    fn default() -> Self {
        Self {
            edge_mm: DEFAULT_EDGE_MM,
            corner_mm: DEFAULT_CORNER_MM,
            scroll_step_mm: DEFAULT_SCROLL_STEP_MM,
            x_min: -1818,
            x_max: -182,
            y_min: -1074,
            y_max: 614,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TouchZones {
    config: ZoneConfig,
    mm_per_x: f64,
    mm_per_y: f64,
    horizontal_mm: f64,
    vertical_mm: f64,
}

impl TouchZones {
    pub fn new(pad_size_mm: f64, config: ZoneConfig) -> Result<Self, &'static str> {
        if config.x_max <= config.x_min || config.y_max <= config.y_min {
            return Err("pad bounds must have max greater than min");
        }
        Ok(Self {
            mm_per_x: pad_size_mm / f64::from(config.x_max - config.x_min),
            mm_per_y: pad_size_mm / f64::from(config.y_max - config.y_min),
            config,
            horizontal_mm: 0.0,
            vertical_mm: 0.0,
        })
    }

    pub fn config(&self) -> &ZoneConfig {
        &self.config
    }

    /// Classify a raw pad coordinate; the remote's Y grows toward the top.
    pub fn region(&self, x: i32, y: i32) -> Region {
        let right = self.right_edge_mm(x) <= self.config.edge_mm;
        let top = self.top_edge_mm(y) <= self.config.edge_mm;
        match (right, top) {
            (true, true) => Region::Corner,
            (true, false) => Region::Right,
            (false, true) => Region::Top,
            (false, false) => Region::Center,
        }
    }

    /// Whether a mechanical click here is a right click.
    pub fn is_click_corner(&self, x: i32, y: i32) -> bool {
        let corner = self.config.corner_mm;
        self.right_edge_mm(x) <= corner && self.top_edge_mm(y) <= corner
    }

    /// Distance inside the pad from the right edge; clamps outside the pad.
    pub fn right_edge_mm(&self, x: i32) -> f64 {
        (f64::from(self.config.x_max - x) * self.mm_per_x).max(0.0)
    }

    pub fn top_edge_mm(&self, y: i32) -> f64 {
        (f64::from(self.config.y_max - y) * self.mm_per_y).max(0.0)
    }

    /// Consume screen-space pad motion and return whole (horizontal, vertical) notches.
    pub fn scroll(&mut self, region: Region, dx_mm: f64, dy_mm: f64) -> (i32, i32) {
        // Natural scrolling: the content follows the finger.
        if matches!(region, Region::Right | Region::Corner) {
            self.vertical_mm += dy_mm;
        }
        if matches!(region, Region::Top | Region::Corner) {
            self.horizontal_mm -= dx_mm;
        }
        let step = self.config.scroll_step_mm;
        let horizontal = (self.horizontal_mm / step) as i32;
        let vertical = (self.vertical_mm / step) as i32;
        self.horizontal_mm -= f64::from(horizontal) * step;
        self.vertical_mm -= f64::from(vertical) * step;
        (horizontal, vertical)
    }

    pub fn reset(&mut self) {
        self.horizontal_mm = 0.0;
        self.vertical_mm = 0.0;
    }
}

/// Timed linear interpolation of pointer motion.
///
/// A report delivers a whole step of finger travel at once, which reads as a
/// jump. Each step is emitted instead as fractions of itself over the following
/// `seconds`, so the pointer follows a smooth path at the caller's tick cadence.
/// Every step keeps its own deadline: motion arriving later never defers the
/// interpolation already in flight. Time is seconds on one monotonic clock.
///
/// A fixed capacity bounds the queue, so a tick costs no allocation.
pub struct Interpolator {
    seconds: f64,
    steps: [Step; CAPACITY],
    length: usize,
}

/// Well over half a second of reports; a full queue means ticks stopped.
const CAPACITY: usize = 64;

#[derive(Debug, Clone, Copy, Default)]
struct Step {
    /// Clock time the step was queued at.
    start: f64,
    /// Fraction of the step already emitted.
    previous: f64,
    dx: f64,
    dy: f64,
}

impl Interpolator {
    pub fn new(seconds: f64) -> Self {
        Self {
            seconds,
            steps: [Step::default(); CAPACITY],
            length: 0,
        }
    }

    /// Whether interpolation is long enough to be observable.
    pub fn enabled(&self) -> bool {
        self.seconds > 0.0
    }

    /// Whether queued travel is still waiting for later ticks.
    pub fn is_pending(&self) -> bool {
        self.length > 0
    }

    /// Travel not yet emitted, as (horizontal, vertical) motion.
    pub fn pending(&self) -> (f64, f64) {
        self.steps[..self.length]
            .iter()
            .fold((0.0, 0.0), |(x, y), step| {
                (
                    x + step.dx * (1.0 - step.previous),
                    y + step.dy * (1.0 - step.previous),
                )
            })
    }

    /// Queue one step of pointer travel, starting at `now`.
    pub fn push(&mut self, now: f64, dx: f64, dy: f64) {
        let step = Step {
            start: now,
            previous: 0.0,
            dx,
            dy,
        };
        if self.length < CAPACITY {
            self.steps[self.length] = step;
            self.length += 1;
            return;
        }
        // The queued travel is stale, so it is dropped rather than emitted as
        // one jump; the new step starts the queue over.
        self.steps[0] = step;
        self.length = 1;
    }

    /// Take the travel due at `now`, as fractional (horizontal, vertical)
    /// motion. Each step finishes at its own deadline, measured from when it
    /// was queued; later steps never defer an earlier one.
    pub fn take(&mut self, now: f64) -> (f64, f64) {
        let mut horizontal = 0.0;
        let mut vertical = 0.0;
        let mut kept = 0;
        for index in 0..self.length {
            let mut step = self.steps[index];
            let fraction = ((now - step.start) / self.seconds).clamp(0.0, 1.0);
            horizontal += step.dx * (fraction - step.previous);
            vertical += step.dy * (fraction - step.previous);
            if fraction < 1.0 {
                step.previous = fraction;
                self.steps[kept] = step;
                kept += 1;
            }
        }
        self.length = kept;
        (horizontal, vertical)
    }

    /// Drop all queued travel.
    pub fn clear(&mut self) {
        self.length = 0;
    }
}
