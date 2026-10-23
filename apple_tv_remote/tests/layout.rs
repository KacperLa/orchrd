//! Touchpad zone geometry.

mod common;

use apple_tv_remote::layout::{Region, TouchZones, ZoneConfig, PAD_SIZE_MM};
use apple_tv_remote::touch::{MotionConfig, X_RANGE, Y_RANGE};
use common::{center, inside_corner, inside_right, inside_top, CONFIG};

fn zones() -> TouchZones {
    TouchZones::new(PAD_SIZE_MM, CONFIG).expect("valid zone config")
}

fn with_step(step: f64) -> TouchZones {
    TouchZones::new(
        PAD_SIZE_MM,
        ZoneConfig {
            scroll_step_mm: step,
            ..CONFIG
        },
    )
    .expect("valid config")
}

#[test]
fn center_is_outside_the_strips() {
    assert_eq!(zones().region(center().0, center().1), Region::Center);
}

#[test]
fn one_millimetre_from_the_edge_is_the_strip() {
    let mut zones = zones();
    let (x, y) = inside_right(1.0);
    assert_eq!(zones.region(x, y), Region::Right);
    let (x, y) = inside_top(1.0);
    assert_eq!(zones.region(x, y), Region::Top);
    let (x, y) = inside_corner(1.0);
    assert_eq!(zones.region(x, y), Region::Corner);
    zones.reset();
}

#[test]
fn strips_leave_room_for_pointer_movement() {
    // A finger a few millimetres in from the edge is pointer movement.
    let zones = zones();
    let (x, y) = inside_right(3.0);
    assert_eq!(zones.region(x, y), Region::Center);
    let (x, y) = inside_top(3.0);
    assert_eq!(zones.region(x, y), Region::Center);
}

#[test]
fn strip_width_is_configurable() {
    let narrow = zones();
    let wide = TouchZones::new(
        PAD_SIZE_MM,
        ZoneConfig {
            edge_mm: 8.0,
            ..CONFIG
        },
    )
    .expect("valid config");
    let (x, y) = inside_right(6.0);
    assert_eq!(wide.region(x, y), Region::Right);
    assert_eq!(narrow.region(x, y), Region::Center);
}

#[test]
fn click_corner_is_wider_than_the_scroll_corner() {
    // The corner right-click stays easy to hit without widening diagonal
    // scrolling: 3 mm in from the corner is a right click but not a scroll
    // corner, and the pad's own extreme is still fully in both.
    let zones = zones();
    let (x, y) = inside_corner(3.0);
    assert_eq!(zones.region(x, y), Region::Center);
    assert!(zones.is_click_corner(x, y));
    assert!(zones.is_click_corner(CONFIG.x_max, CONFIG.y_max));
}

#[test]
fn click_corner_needs_both_edges() {
    let zones = zones();
    let (x, y) = inside_right(1.0);
    assert!(!zones.is_click_corner(x, y));
    let (x, y) = inside_top(1.0);
    assert!(!zones.is_click_corner(x, y));
    let (x, y) = center();
    assert!(!zones.is_click_corner(x, y));
}

/// The pad's own measured extreme is the strip's outer edge, and the opposite
/// extreme is plain pointer movement.
#[test]
fn the_measured_extremes_are_the_strip_edges() {
    let zones = zones();
    assert_eq!(zones.region(CONFIG.x_max, center().1), Region::Right);
    assert_eq!(zones.region(center().0, CONFIG.y_max), Region::Top);
    assert_eq!(zones.region(CONFIG.x_max, CONFIG.y_max), Region::Corner);
    assert_eq!(zones.region(CONFIG.x_min, center().1), Region::Center);
    assert_eq!(zones.region(center().0, CONFIG.y_min), Region::Center);
}

#[test]
fn pad_box_is_not_centred_on_zero() {
    // A zero-centred model would place the right edge out of reach, since the
    // pad never reports past x_max.
    assert_ne!(CONFIG.x_min + CONFIG.x_max, 0);
    assert_ne!(CONFIG.y_min + CONFIG.y_max, 0);
    let zones = zones();
    assert!((zones.right_edge_mm(CONFIG.x_max) - 0.0).abs() < f64::EPSILON);
    assert!((zones.right_edge_mm(CONFIG.x_min) - PAD_SIZE_MM).abs() < 1e-9);
    assert!((zones.top_edge_mm(CONFIG.y_max) - 0.0).abs() < f64::EPSILON);
    assert!((zones.top_edge_mm(CONFIG.y_min) - PAD_SIZE_MM).abs() < 1e-9);
}

#[test]
fn right_strip_only_scrolls_vertically() {
    let mut zones = with_step(2.0);
    assert_eq!(zones.scroll(Region::Right, 120.0, 2.0), (0, 1));
}

#[test]
fn top_strip_only_scrolls_horizontally() {
    let mut zones = with_step(2.0);
    assert_eq!(zones.scroll(Region::Top, -4.0, 120.0), (2, 0));
}

#[test]
fn corner_scrolls_both_axes() {
    let mut zones = with_step(2.0);
    assert_eq!(zones.scroll(Region::Corner, -4.0, -4.0), (2, -2));
}

#[test]
fn sub_notches_accumulate_and_reset_discards_the_fraction() {
    let mut zones = with_step(2.0);
    assert_eq!(zones.scroll(Region::Right, 0.0, 1.5), (0, 0));
    assert_eq!(zones.scroll(Region::Right, 0.0, 1.5), (0, 1));
    zones.reset();
    assert_eq!(zones.scroll(Region::Right, 0.0, 1.0), (0, 0));
}

#[test]
fn motion_config_scales_pad_ranges() {
    // One millimetre of finger travel becomes the configured pixel count.
    let config = MotionConfig::new(1.0, 3.0, 3.0, 80.0, 2.0, 4.0, 3.0, 6.0 / 1000.0);
    assert!((X_RANGE / PAD_SIZE_MM * config.x_scale - 3.0).abs() < 1e-9);
    assert!((Y_RANGE / PAD_SIZE_MM * config.y_scale - 3.0).abs() < 1e-9);
}

#[test]
fn invalid_bounds_are_rejected() {
    let config = ZoneConfig {
        x_min: 10,
        x_max: 10,
        ..CONFIG
    };
    assert!(TouchZones::new(PAD_SIZE_MM, config).is_err());
}
