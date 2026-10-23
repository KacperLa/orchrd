//! Touchpad behaviour end to end, with a recording sink instead of uinput.
//!
//! Most of these tests open with a one-report contact, which is what the daemon
//! sees when a finger lands: `process` treats the first report of a contact as
//! the place that decides the region and the click corner.

mod common;

use apple_tv_remote::touch::{Button, RemoteKey};
use common::{center, inside_corner, inside_right, inside_top, Harness, Recorded};

/// The contact has to be at least this far in from an edge to be pointer
/// movement; see the strip widths in `layout.rs`.
const MM_INSIDE_EDGE: f64 = 1.0;
/// Just outside the two millimetre scroll strip.
const MM_OUTSIDE_EDGE: f64 = 3.0;

#[test]
fn right_edge_drag_scrolls_vertically() {
    // The drag is diagonal: a contact latched in the right strip must scroll
    // only along that strip's axis and never fall back to pointer movement.
    let mut harness = Harness::new();
    let (x, y) = inside_right(MM_INSIDE_EDGE);
    harness.drag((x, y), 40, 40, 6);
    assert!(!harness.recorder().scrolls(false).is_empty());
    assert!(harness.recorder().scrolls(true).is_empty());
    assert!(harness.recorder().motions().is_empty());
}

#[test]
fn scroll_follows_the_finger() {
    // Pad coordinates grow toward the top of the remote.
    let mut harness = Harness::new();
    let (x, y) = inside_right(MM_INSIDE_EDGE);
    harness.drag((x, y), 0, -40, 6);
    assert!(harness
        .recorder()
        .scrolls(false)
        .iter()
        .all(|value| *value > 0));
}

#[test]
fn top_edge_drag_scrolls_horizontally() {
    let mut harness = Harness::new();
    let (x, y) = inside_top(MM_INSIDE_EDGE);
    harness.drag((x, y), -40, 0, 6);
    assert!(!harness.recorder().scrolls(true).is_empty());
    assert!(harness.recorder().scrolls(false).is_empty());
    assert!(harness.recorder().motions().is_empty());
}

#[test]
fn drag_just_inside_the_strip_moves_the_pointer() {
    let mut harness = Harness::new();
    let (x, y) = inside_right(MM_OUTSIDE_EDGE);
    harness.drag((x, y), 0, 40, 6);
    let (x, y) = inside_top(MM_OUTSIDE_EDGE);
    harness.drag((x, y), -40, 0, 6);
    let motions = harness.recorder().motions();
    assert!(motions.iter().any(|(dx, _)| *dx != 0));
    assert!(motions.iter().any(|(_, dy)| *dy != 0));
    assert!(harness.recorder().scrolls(true).is_empty());
    assert!(harness.recorder().scrolls(false).is_empty());
}

#[test]
fn center_drag_moves_the_pointer() {
    let mut harness = Harness::new();
    let (x, y) = center();
    harness.drag((x, y), 40, 40, 6);
    assert!(!harness.recorder().motions().is_empty());
    assert!(harness.recorder().scrolls(true).is_empty());
    assert!(harness.recorder().scrolls(false).is_empty());
}

#[test]
fn edge_contact_keeps_scrolling_after_leaving_the_strip() {
    let mut harness = Harness::new();
    let (x, y) = inside_right(MM_INSIDE_EDGE);
    harness.send(0.02, Some(x), Some(y), 0);
    let mut x = x;
    let mut y = y;
    for _ in 0..6 {
        x -= (2.0 * common::x_per_mm()) as i32;
        y += 40;
        harness.send(0.02, Some(x), Some(y), 0);
    }
    assert!(!harness.recorder().scrolls(false).is_empty());
    assert!(harness.recorder().motions().is_empty());
}

#[test]
fn pressed_edge_drag_still_scrolls() {
    let mut harness = Harness::new();
    let (x, y) = inside_right(MM_INSIDE_EDGE);
    harness.send(0.02, Some(x), Some(y), 0x80);
    let mut y = y;
    for _ in 0..6 {
        y += 40;
        harness.send(0.02, Some(x), Some(y), 0x80);
    }
    assert!(!harness.recorder().scrolls(false).is_empty());
    assert!(harness.recorder().motions().is_empty());
}

/// The right-click zone reaches past the scroll strips, so a click one
/// millimetre from the corner and one just outside the strips both right-click.
#[test]
fn top_right_click_is_a_right_click() {
    for inside in [MM_INSIDE_EDGE, MM_OUTSIDE_EDGE] {
        let mut harness = Harness::new();
        let (x, y) = inside_corner(inside);
        harness.send(0.02, Some(x), Some(y), 0);
        harness.send(0.02, Some(x), Some(y), 0x80);
        harness.send(0.02, Some(x), Some(y), 0);
        assert_eq!(
            harness.recorder().keys(Button::Right),
            vec![true, false],
            "{inside} mm from the corner"
        );
        assert!(harness.recorder().keys(Button::Left).is_empty());
    }
}

#[test]
fn drag_in_the_click_corner_moves_the_pointer() {
    let mut harness = Harness::new();
    let (x, y) = inside_corner(MM_OUTSIDE_EDGE);
    harness.drag((x, y), -40, -40, 6);
    let motions = harness.recorder().motions();
    assert!(motions.iter().any(|(dx, _)| *dx != 0));
    assert!(motions.iter().any(|(_, dy)| *dy != 0));
    assert!(harness.recorder().scrolls(true).is_empty());
    assert!(harness.recorder().scrolls(false).is_empty());
}

#[test]
fn center_click_stays_a_left_click() {
    let mut harness = Harness::new();
    let (x, y) = center();
    harness.send(0.02, Some(x), Some(y), 0);
    harness.send(0.02, Some(x), Some(y), 0x80);
    harness.send(0.02, Some(x), Some(y), 0);
    assert_eq!(harness.recorder().keys(Button::Left), vec![true, false]);
    assert!(harness.recorder().keys(Button::Right).is_empty());
}

/// A button-only report carries no coordinates, so the click uses the position
/// of the contact latched before it.
#[test]
fn click_without_a_touch_uses_the_latched_corner() {
    let (x, y) = inside_corner(MM_INSIDE_EDGE);
    let mut corner = Harness::new();
    corner.send(0.02, Some(x), Some(y), 0);
    corner.send(0.02, None, None, 0x80);
    corner.send(0.02, None, None, 0);
    assert_eq!(corner.recorder().keys(Button::Right), vec![true, false]);
    assert!(corner.recorder().keys(Button::Left).is_empty());

    // With no contact at all before the click, it stays a left click.
    let mut bare = Harness::new();
    bare.send(0.02, None, None, 0x80);
    bare.send(0.02, None, None, 0);
    assert_eq!(bare.recorder().keys(Button::Left), vec![true, false]);
    assert!(bare.recorder().keys(Button::Right).is_empty());
}

#[test]
fn click_matches_the_current_finger_position() {
    let mut harness = Harness::new();
    let (x, y) = center();
    harness.send(0.02, Some(x), Some(y), 0);
    let (x, y) = inside_corner(MM_INSIDE_EDGE);
    harness.send(0.02, Some(x), Some(y), 0);
    harness.send(0.02, Some(x), Some(y), 0x80);
    harness.send(0.02, Some(x), Some(y), 0);
    assert_eq!(harness.recorder().keys(Button::Right), vec![true, false]);
}

#[test]
fn click_and_touch_in_one_report_use_the_corner() {
    let mut harness = Harness::new();
    let (x, y) = inside_corner(MM_INSIDE_EDGE);
    harness.send(0.02, Some(x), Some(y), 0x80);
    harness.send(0.02, Some(x), Some(y), 0);
    assert_eq!(harness.recorder().keys(Button::Right), vec![true, false]);
    assert!(harness.recorder().keys(Button::Left).is_empty());
}

#[test]
fn corner_click_releases_even_if_region_changed() {
    let mut harness = Harness::new();
    let (x, y) = inside_corner(MM_INSIDE_EDGE);
    harness.send(0.02, Some(x), Some(y), 0);
    harness.send(0.02, Some(x), Some(y), 0x80);
    let (x, y) = center();
    harness.send(0.02, Some(x), Some(y), 0x80);
    harness.send(0.02, Some(x), Some(y), 0);
    assert_eq!(harness.recorder().keys(Button::Right), vec![true, false]);
    assert!(harness.recorder().keys(Button::Left).is_empty());
}

/// The click corner reaches the pad's own measured extreme, and the pad never
/// reports past it, so a zero-centred model would put the corner out of reach.
#[test]
fn the_click_corner_reaches_the_pad_extreme() {
    let mut harness = Harness::new();
    let (x, y) = inside_corner(0.0);
    harness.send(0.02, Some(x), Some(y), 0);
    harness.send(0.02, Some(x), Some(y), 0x80);
    harness.send(0.02, Some(x), Some(y), 0);
    assert_eq!(harness.recorder().keys(Button::Right), vec![true, false]);
}

#[test]
fn click_does_not_emit_motion() {
    let mut harness = Harness::new();
    let (x, y) = center();
    harness.drag((x, y), 40, 40, 3);
    let before = harness.recorder().motions().len();
    harness.send(0.02, Some(x + 120), Some(y + 120), 0x80);
    assert_eq!(harness.recorder().motions().len(), before);
}

#[test]
fn play_pause_and_menu_become_key_events_once() {
    let mut harness = Harness::new();
    harness.send(0.02, None, None, 0x08);
    harness.send(0.02, None, None, 0x08);
    harness.send(0.02, None, None, 0x00);
    harness.send(0.02, None, None, 0x20);
    harness.send(0.02, None, None, 0x00);
    let events: Vec<Recorded> = harness
        .recorder()
        .events
        .iter()
        .copied()
        .filter(|event| matches!(event, Recorded::RemoteKey(_, _)))
        .collect();
    assert_eq!(
        events,
        vec![
            Recorded::RemoteKey(RemoteKey::PlayPause, true),
            Recorded::RemoteKey(RemoteKey::PlayPause, false),
            Recorded::RemoteKey(RemoteKey::Menu, true),
            Recorded::RemoteKey(RemoteKey::Menu, false),
        ]
    );
}

#[test]
fn release_clears_held_buttons() {
    let mut harness = Harness::new();
    let (x, y) = center();
    harness.send(0.02, Some(x), Some(y), 0x80);
    harness.pad.release();
    harness.pad.release();
    assert_eq!(harness.recorder().keys(Button::Left), vec![true, false]);
}

#[test]
fn repeated_reports_are_ignored() {
    let mut harness = Harness::new();
    let (x, y) = center();
    harness.send(0.02, Some(x), Some(y), 0);
    let before = harness.recorder().events.len();
    for _ in 0..4 {
        harness.send(0.0, Some(x), Some(y), 0);
    }
    assert_eq!(harness.recorder().events.len(), before);
    assert!(harness.pad.stats.duplicates >= 4);
}

/// No finger can cross a third of the pad between two reports, so such a jump
/// is a decoding glitch and must not fling the pointer. Tracking resumes on the
/// next plausible sample. One raw pad unit is one pixel here.
#[test]
fn an_implausible_jump_is_dropped() {
    let mut harness = Harness::new();
    let (x, y) = center();
    harness.send(0.02, Some(x), Some(y), 0);
    harness.send(0.02, Some(x + 600), Some(y + 600), 0);
    assert!(harness.recorder().motions().is_empty());
    harness.send(0.02, Some(x + 620), Some(y + 620), 0);
    assert_eq!(harness.recorder().motions(), vec![(20, -20)]);
}

/// After a gap long enough to have been a disconnect, the pad rebases on the
/// new contact instead of counting the missing time as one huge movement.
#[test]
fn a_long_gap_rebases_on_the_new_position() {
    let mut harness = Harness::new();
    let (x, y) = center();
    harness.drag((x, y), 40, 40, 2);
    harness.send(2.0, Some(x + 80), Some(y + 80), 0);
    let before = harness.recorder().motions().len();
    harness.send(0.02, Some(x + 120), Some(y + 120), 0);
    assert_eq!(&harness.recorder().motions()[before..], &[(40, -40)]);
}

#[test]
fn scroll_notches_follow_the_configured_step() {
    let mut coarse = Harness::with_scroll_step(1.0);
    let (x, y) = inside_right(MM_INSIDE_EDGE);
    coarse.drag((x, y), 0, 40, 6);
    let coarse_notches: i32 = coarse
        .recorder()
        .scrolls(false)
        .iter()
        .map(|v| v.abs())
        .sum();
    let mut fine = Harness::with_scroll_step(4.0);
    fine.drag((x, y), 0, 40, 6);
    let fine_notches: i32 = fine.recorder().scrolls(false).iter().map(|v| v.abs()).sum();
    assert!(coarse_notches > fine_notches);
    assert!(fine_notches > 0);
}
