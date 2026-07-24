//! Unit tests for the tooltip geometry and URL truncation ports.

use crate::layout::Rect;
use crate::tooltip::{TooltipPosition, clamp_tooltip_x, tooltip_width, tooltip_y, truncate_url};

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

#[test]
fn short_url_is_returned_unchanged() {
    assert_eq!(truncate_url("https://x.dev", 40), "https://x.dev");
}

// @lat: [[client#GPUI Overlays#Tooltip truncates a long URL head and tail]]
#[test]
fn long_url_keeps_head_and_tail_with_ellipsis() {
    let uri = "https://example.com/very/long/path/segment/that/overflows";
    let out = truncate_url(uri, 20);
    assert_eq!(out.chars().count(), 20);
    assert!(out.contains("..."));
    assert!(out.starts_with("https"));
    assert!(out.ends_with("flows"));
    // Head-heavy split: budget 17 -> head 9, tail 8.
    assert!(out.starts_with("https://e"));
}

#[test]
fn tiny_budget_falls_back_to_head_cut() {
    assert_eq!(truncate_url("https://example.com", 3), "htt");
    assert_eq!(truncate_url("https://example.com", 0), "");
}

#[test]
fn truncation_never_splits_a_multibyte_codepoint() {
    let uri = "https://例え.example.com/セグメント/長いパスの終わり";
    let out = truncate_url(uri, 15);
    // Round-trips as valid UTF-8 with exactly the budgeted char count.
    assert_eq!(out.chars().count(), 15);
    assert!(out.contains("..."));
}
