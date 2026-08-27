//! Unit tests for the tooltip geometry port.

use crate::layout::Rect;
use crate::tooltip::{TooltipPosition, clamp_tooltip_x, tooltip_width, tooltip_y};

fn anchor(x: f32, width: f32) -> Rect {
    Rect { x, y: 100.0, width, height: 18.0 }
}

// @lat: [[client#GPUI Overlays#Tooltip centres on its anchor]]
#[test]
fn tooltip_centres_over_the_anchor_when_it_fits() {
    let width = tooltip_width("hi", 8.0);
    // Anchor centred at x=500; tooltip should be centred there too.
    let x = clamp_tooltip_x(anchor(480.0, 40.0), width, 1000.0);
    let center = 480.0 + 40.0 / 2.0;
    assert!((x - (center - width / 2.0)).abs() < f32::EPSILON);
}

// @lat: [[client#GPUI Overlays#Tooltip clamps to the viewport edges]]
#[test]
fn tooltip_clamps_against_both_viewport_edges() {
    let width = tooltip_width("some label", 8.0);
    // Anchor hard against the right edge: clamp pins the box inside the viewport.
    let right = clamp_tooltip_x(anchor(990.0, 20.0), width, 1000.0);
    assert!((right - (1000.0 - width)).abs() < f32::EPSILON);
    // Anchor hard against the left edge: clamp pins the box to x=0.
    let left = clamp_tooltip_x(anchor(0.0, 10.0), width, 1000.0);
    assert!(left.abs() < f32::EPSILON);
}

#[test]
fn tooltip_wider_than_viewport_collapses_to_left() {
    let width = tooltip_width("an extremely long tooltip label", 8.0);
    let x = clamp_tooltip_x(anchor(10.0, 10.0), width, 20.0);
    assert!(x.abs() < f32::EPSILON);
}

// @lat: [[client#GPUI Overlays#Tooltip picks above or below the anchor]]
#[test]
fn tooltip_y_tracks_above_and_below() {
    let a = anchor(0.0, 10.0);
    assert!((tooltip_y(a, 18.0, TooltipPosition::Above) - (100.0 - 18.0)).abs() < f32::EPSILON);
    assert!((tooltip_y(a, 18.0, TooltipPosition::Below) - (100.0 + 18.0)).abs() < f32::EPSILON);
}
