//! Pointer interpolation: one report's travel is spread over the configured
//! interval instead of arriving as a single jump.

mod common;

use common::{center, Harness};

const INTERVAL: f64 = 0.006;

#[test]
fn a_step_is_emitted_in_fractions_across_the_interval() {
    let mut harness = Harness::with_interpolation(INTERVAL);
    let (x, y) = center();
    harness.send(0.02, Some(x), Some(y), 0);
    harness.send(0.02, Some(x + 40), Some(y), 0);
    // The step is queued, not emitted: it belongs to the ticks that follow.
    assert!(harness.recorder().motions().is_empty());
    assert!(harness.pad.interpolation_pending());

    harness.tick(INTERVAL / 2.0);
    assert_eq!(harness.recorder().motions(), vec![(20, 0)]);
    assert!(harness.pad.interpolation_pending());

    harness.tick(INTERVAL / 2.0);
    assert_eq!(harness.recorder().motions(), vec![(20, 0), (20, 0)]);
    assert!(!harness.pad.interpolation_pending());
}

#[test]
fn each_step_keeps_its_own_deadline() {
    let mut harness = Harness::with_interpolation(INTERVAL);
    let (x, y) = center();
    harness.send(0.02, Some(x), Some(y), 0);
    // The second report arrives while the first step is still spreading.
    harness.send(0.02, Some(x + 40), Some(y), 0);
    harness.send(0.003, Some(x + 80), Some(y), 0);
    // Half an interval later the first step is finished and the second half
    // done; the later report did not restart the first one's clock.
    harness.tick(INTERVAL / 2.0);
    assert_eq!(harness.recorder().motions(), vec![(60, 0)]);
    // Well past the second step's deadline, everything has come due.
    harness.tick(INTERVAL);
    assert!(!harness.pad.interpolation_pending());
    let travelled: i32 = harness.recorder().motions().iter().map(|(dx, _)| *dx).sum();
    assert_eq!(travelled, 80);
}

#[test]
fn a_reversal_discards_the_travel_still_queued_against_it() {
    let mut harness = Harness::with_interpolation(INTERVAL);
    let (x, y) = center();
    harness.send(0.02, Some(x), Some(y), 0);
    harness.send(0.02, Some(x + 40), Some(y), 0);
    harness.tick(INTERVAL / 2.0);
    // Half of the rightward step is still queued when the finger turns.
    harness.send(0.02, Some(x), Some(y), 0);
    harness.tick(10.0 * INTERVAL);

    let travelled: i32 = harness.recorder().motions().iter().map(|(dx, _)| *dx).sum();
    // The emitted 20 from before the turn, then the whole leftward step; the
    // queued rightward half never arrives.
    assert_eq!(travelled, 20 - 40);
}

#[test]
fn a_click_discards_queued_travel_instead_of_emitting_it() {
    let mut harness = Harness::with_interpolation(INTERVAL);
    let (x, y) = center();
    harness.send(0.02, Some(x), Some(y), 0);
    harness.send(0.02, Some(x + 40), Some(y), 0);
    harness.send(0.001, Some(x + 40), Some(y), 0x80);
    harness.tick(10.0 * INTERVAL);
    assert!(harness.recorder().motions().is_empty());
}

#[test]
fn a_stationary_report_drops_the_step_in_flight() {
    let mut harness = Harness::with_interpolation(INTERVAL);
    let (x, y) = center();
    harness.send(0.02, Some(x), Some(y), 0);
    harness.send(0.02, Some(x + 40), Some(y), 0);
    harness.send(0.001, Some(x + 40), Some(y), 0);
    harness.tick(10.0 * INTERVAL);
    assert!(harness.recorder().motions().is_empty());
    assert!(!harness.pad.interpolation_pending());
}

#[test]
fn zero_interpolation_emits_every_step_whole() {
    let mut harness = Harness::with_interpolation(0.0);
    let (x, y) = center();
    harness.send(0.02, Some(x), Some(y), 0);
    harness.send(0.02, Some(x + 40), Some(y), 0);
    assert_eq!(harness.recorder().motions(), vec![(40, 0)]);
    // Ticks without queued travel emit nothing.
    harness.tick(0.02);
    assert_eq!(harness.recorder().motions(), vec![(40, 0)]);
}
