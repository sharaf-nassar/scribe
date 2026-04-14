//! Mouse click counting and selection mode classification.
//!
//! Tracks single/double/triple click sequences and derives the active
//! selection granularity (cell, word, or line).  Also provides an
//! edge-scroll helper for autoscrolling during drag selection.

use std::time::{Duration, Instant};

/// Maximum time between two presses to count as a multi-click.
const MULTI_CLICK_TIMEOUT: Duration = Duration::from_millis(400);

/// Maximum squared pixel distance between press positions to count as a
/// multi-click.  Corresponds to 5 px radius.
const MULTI_CLICK_MAX_DIST_SQ: f32 = 25.0;

/// Edge-scroll zone size in pixels.  When the cursor is within this distance
/// of the top or bottom of the content area, auto-scroll is triggered.
const EDGE_SCROLL_ZONE: f32 = 20.0;

/// Determines selection granularity during a drag operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// Select individual cells.
    Cell,
    /// Select whole words.
    Word,
    /// Select whole lines.
    Line,
}

/// Classifies a mouse press by its position in a click sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickKind {
    Single,
    Double,
    Triple,
}

/// Per-pane state used to classify mouse presses and derive selection mode.
pub struct MouseClickState {
    last_press_time: Option<Instant>,
    last_press_point: Option<(f32, f32)>,
    click_count: u8,
}

impl Default for MouseClickState {
    fn default() -> Self {
        Self::new()
    }
}

impl MouseClickState {
    /// Create a new state with no recorded press.
    pub fn new() -> Self {
        Self { last_press_time: None, last_press_point: None, click_count: 0 }
    }

    /// Record a mouse press at `(x, y)` and return the resulting click kind.
    ///
    /// If the press arrives within [`MULTI_CLICK_TIMEOUT`] and within
    /// [`MULTI_CLICK_MAX_DIST_SQ`] pixels squared of the last press, the
    /// click count is incremented (clamped at 3).  Otherwise it resets to 1.
    /// The selection mode is updated accordingly.
    pub fn record_press(&mut self, x: f32, y: f32) -> ClickKind {
        let now = Instant::now();

        let within_time =
            self.last_press_time.is_some_and(|t| now.duration_since(t) <= MULTI_CLICK_TIMEOUT);

        let within_dist = self.last_press_point.is_some_and(|(px, py)| {
            let dx = x - px;
            let dy = y - py;
            dx * dx + dy * dy <= MULTI_CLICK_MAX_DIST_SQ
        });

        if within_time && within_dist {
            self.click_count = self.click_count.saturating_add(1).min(3);
        } else {
            self.click_count = 1;
        }

        self.last_press_time = Some(now);
        self.last_press_point = Some((x, y));

        match self.click_count {
            2 => ClickKind::Double,
            3 => ClickKind::Triple,
            _ => ClickKind::Single,
        }
    }

    /// Reset click count to zero (e.g. on pane focus changes).
    pub fn reset(&mut self) {
        self.click_count = 0;
        self.last_press_time = None;
        self.last_press_point = None;
    }
}

/// Compute the edge-scroll delta for a cursor at `cursor_y` relative to the
/// content bounds `[content_top, content_bottom]`.
///
/// Returns `Some(-3)` when the cursor is above `content_top + EDGE_SCROLL_ZONE`
/// (scroll up into history) and `Some(3)` when below
/// `content_bottom - EDGE_SCROLL_ZONE` (scroll toward live output).
/// Returns `None` when the cursor is within the content area.
pub fn edge_scroll_delta(cursor_y: f32, content_top: f32, content_bottom: f32) -> Option<i32> {
    if cursor_y < content_top + EDGE_SCROLL_ZONE {
        Some(-3)
    } else if cursor_y > content_bottom - EDGE_SCROLL_ZONE {
        Some(3)
    } else {
        None
    }
}
