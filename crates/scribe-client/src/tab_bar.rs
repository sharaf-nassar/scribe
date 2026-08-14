//! Pure tab-bar logic for the GPUI titlebar, ported from the legacy
//! `scribe-client` GPU tab bar.
//!
//! The winit client emitted GPU quads into a shared glyph buffer; the
//! GPUI rebuild instead renders the tab bar as `div` elements inside the custom
//! [`crate::titlebar`]. This module keeps the display-independent pieces that
//! survive that change — the attention-flash decay envelope, the fixed-width
//! title truncation, the workspace-badge sizing, the context-% suffix banding,
//! and the drag-reorder target math — as pure functions covered by unit tests so
//! their behaviour stays identical across the cutover.

use std::time::Instant;

use gpui::Rgba;
use scribe_common::theme::ChromeColors;

use crate::opacity::scale_alpha;

/// Duration (seconds) of the transient tab attention flash, with a brief
/// ease-out so it decays smoothly rather than cutting off.
pub const TAB_FLASH_SECS: f32 = 0.45;

/// Warn-band color for the context-% suffix (`#d4a017`), matching the legacy
/// client's `AiStateTracker` bands.
pub const CONTEXT_WARN_COLOR: Rgba = Rgba { r: 0.831, g: 0.627, b: 0.090, a: 1.0 };

/// Danger-band color for the context-% suffix (`#c83030`).
pub const CONTEXT_DANGER_COLOR: Rgba = Rgba { r: 0.784, g: 0.188, b: 0.188, a: 1.0 };

/// Default context-usage warn threshold (percent).
pub const CONTEXT_WARN_THRESHOLD: u8 = 70;

/// Default context-usage danger threshold (percent).
pub const CONTEXT_DANGER_THRESHOLD: u8 = 90;

/// Map elapsed time since a tab-flash started to a 0.0–1.0 intensity.
///
/// Returns `None` once the envelope has fully elapsed so the caller can clear
/// the originating `tab_flash_start` and let the redraw loop rest — the same
/// self-decaying discipline the scrollbar fade uses. The curve is an ease-out
/// (`(1 - t)^2`) on the normalised elapsed fraction.
#[must_use]
pub fn tab_flash_intensity(elapsed_secs: f32) -> Option<f32> {
    if !elapsed_secs.is_finite() || !(0.0..TAB_FLASH_SECS).contains(&elapsed_secs) {
        return None;
    }
    let t = (elapsed_secs / TAB_FLASH_SECS).clamp(0.0, 1.0);
    let remaining = 1.0 - t;
    Some(remaining * remaining)
}

/// Advance a pane's tab-flash timer, clearing it once the envelope elapses.
///
/// The timer owns its own decay and self-clears so it cannot pin the redraw
/// loop. Returns `true` while the flash is still within its envelope (the loop
/// must keep ticking).
pub fn tick_tab_flash(flash_start: &mut Option<Instant>) -> bool {
    let Some(start) = *flash_start else { return false };
    if tab_flash_intensity(start.elapsed().as_secs_f32()).is_some() {
        true
    } else {
        *flash_start = None;
        false
    }
}

/// Maximum fraction of the accent mixed into the tab background at the peak of
/// the attention flash. Kept low so the cue stays non-disruptive.
pub const FLASH_MAX_MIX: f32 = 0.45;

/// Blend `base` toward `accent` by the flash intensity, preserving `base`'s
/// alpha. `None` (or a fully decayed intensity) returns `base` unchanged.
#[must_use]
pub fn flash_blend(base: Rgba, accent: Rgba, flash: Option<f32>) -> Rgba {
    let Some(intensity) = flash else { return base };
    let mix = intensity.clamp(0.0, 1.0) * FLASH_MAX_MIX;
    Rgba {
        r: base.r + (accent.r - base.r) * mix,
        g: base.g + (accent.g - base.g) * mix,
        b: base.b + (accent.b - base.b) * mix,
        a: base.a,
    }
}

/// Colors for the tab bar, derived from the theme's [`ChromeColors`].
///
/// Unlike the winit client these stay in sRGB (`Rgba`) because GPUI performs its
/// own sRGB→linear conversion when it paints; linearising here would double the
/// correction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TabBarColors {
    pub bg: Rgba,
    pub active_bg: Rgba,
    pub text: Rgba,
    pub active_text: Rgba,
    pub separator: Rgba,
    /// Slightly lighter background for the top of the gradient tab bar.
    pub gradient_top: Rgba,
    /// Theme accent, reused for the active underline and the attention flash.
    pub accent: Rgba,
}

/// Convert a sRGB `[f32; 4]` chrome channel to a GPUI [`Rgba`].
#[must_use]
pub const fn srgba(c: [f32; 4]) -> Rgba {
    Rgba { r: c[0], g: c[1], b: c[2], a: c[3] }
}

impl From<&ChromeColors> for TabBarColors {
    fn from(chrome: &ChromeColors) -> Self {
        Self {
            bg: srgba(chrome.tab_bar_bg),
            active_bg: srgba(chrome.tab_bar_active_bg),
            text: srgba(chrome.tab_text),
            active_text: srgba(chrome.tab_text_active),
            separator: srgba(chrome.tab_separator),
            gradient_top: srgba(chrome.tab_bar_gradient_top),
            accent: srgba(chrome.accent),
        }
    }
}

impl TabBarColors {
    /// Build the palette with `appearance.opacity` folded into the filled
    /// surfaces only.
    ///
    /// The bar background, the active-tab background and the gradient top are
    /// window backgrounds, so they scale with opacity and let the desktop show
    /// through. Text, separators and the accent underline are content and keep
    /// the theme's own alpha, matching the legacy renderer which scaled cell
    /// background alpha but never foreground glyphs.
    #[must_use]
    pub fn from_chrome(chrome: &ChromeColors, opacity: f32) -> Self {
        let base = Self::from(chrome);
        Self {
            bg: scale_alpha(base.bg, opacity),
            active_bg: scale_alpha(base.active_bg, opacity),
            gradient_top: scale_alpha(base.gradient_top, opacity),
            ..base
        }
    }
}

/// The context-% suffix appended to a tab label plus the color it renders in.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextSuffix {
    /// Formatted text, always `" NN%"` (single leading space).
    pub text: String,
    /// Band color (warn or danger).
    pub color: Rgba,
}

/// Compute the colored context-% suffix for a tab, or `None` when it should not
/// be shown.
///
/// Returns `None` below `warn`, or when `pulsing` is set (a `PermissionPrompt` /
/// `WaitingForInput` session already draws attention through its pulse and must
/// not compete with the suffix). At or above `danger` the suffix is the danger
/// band; between `warn` and `danger` it is the warn band.
#[must_use]
pub fn context_suffix(percent: u8, warn: u8, danger: u8, pulsing: bool) -> Option<ContextSuffix> {
    let text = scribe_common::ai_chrome::tab_context_suffix_text(percent, warn, pulsing)?;
    let color = if percent >= danger { CONTEXT_DANGER_COLOR } else { CONTEXT_WARN_COLOR };
    Some(ContextSuffix { text, color })
}

/// Per-tab data the titlebar renders.
#[derive(Debug, Clone, PartialEq)]
pub struct TabData {
    /// Stable identifier for the session this tab represents. It is distinct
    /// from the mutable strip index so its AccessKit node survives reorders.
    pub accessibility_id: String,
    /// Tab title (task label while active, otherwise the shell/process title).
    pub title: String,
    /// Whether this tab is the active/focused tab in its workspace.
    pub is_active: bool,
    /// AI state indicator color. `None` when no active AI state.
    pub ai_indicator: Option<Rgba>,
    /// Colored context-% suffix appended to the label. `None` when not shown.
    pub context_suffix: Option<ContextSuffix>,
    /// Transient attention-flash intensity (0.0–1.0). `None` when no flash is
    /// active. Blended additively over the tab background without overriding
    /// active-tab or AI-indicator styling.
    pub tab_flash: Option<f32>,
    /// Workspace pill rendered before a multi-workspace group's first tab
    /// when the server provides a real workspace name.
    pub badge: Option<GroupBadge>,
    /// Window-relative left edge of a workspace group's region. Set only on
    /// the group's first tab, independently of whether the workspace is named
    /// and therefore has a badge.
    pub group_region_x: Option<f32>,
    /// The owning region's accent in a multi-workspace window, tinting the
    /// active-tab underline so it meets the region border below in the same
    /// colour. `None` (single workspace) falls back to the theme accent.
    pub group_accent: Option<Rgba>,
}

/// Workspace pill prefixed to a group's first tab in a multi-workspace strip.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupBadge {
    /// Server-provided workspace name.
    pub label: String,
    /// The workspace's region accent, tinting the pill.
    pub accent: Rgba,
    /// Whether this workspace has a detected Beads project.
    pub beads: bool,
}

impl TabData {
    /// A plain inactive tab with just a title.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        let title = title.into();
        Self {
            // Production callers replace this with the server session ID.
            // Keeping test-only tabs deterministic still makes accidental
            // duplicate IDs apparent in the accessibility coverage.
            accessibility_id: format!("tab-{title}"),
            title,
            is_active: false,
            ai_indicator: None,
            context_suffix: None,
            tab_flash: None,
            badge: None,
            group_region_x: None,
            group_accent: None,
        }
    }
}

/// Truncate a tab title to fit `available` display columns, appending an ellipsis
/// when it overflows.
///
/// Returns `(display_title, is_truncated)`. `is_truncated` drives the tooltip
/// hover target: a truncated tab exposes its full title on hover. Unlike the
/// winit port this does not right-pad, because GPUI centers the flex label
/// rather than laying out a fixed cell grid.
#[must_use]
pub fn tab_display_title(title: &str, available: usize) -> (String, bool) {
    let chars: Vec<char> = title.chars().collect();
    if available == 0 {
        return (String::new(), !chars.is_empty());
    }
    if chars.len() <= available {
        return (title.to_owned(), false);
    }
    let keep = available.saturating_sub(1);
    let mut truncated: String = chars.into_iter().take(keep).collect();
    truncated.push('\u{2026}');
    (truncated, true)
}

/// Whether a workspace badge pill should be shown, and its label.
///
/// Named workspaces in a multi-workspace window show a badge; a single workspace
/// or an unnamed one shows none.
#[must_use]
pub fn badge_label(ws_name: Option<&str>, multi_workspace: bool) -> Option<&str> {
    match (multi_workspace, ws_name) {
        (true, Some(name)) if !name.trim().is_empty() => Some(name.trim()),
        _ => None,
    }
}

/// Convert a small index/column count to `f32` without a lint-tripping `as`
/// cast, saturating at `u16::MAX` (far beyond any realistic tab count).
#[must_use]
pub fn px_units(n: usize) -> f32 {
    f32::from(u16::try_from(n).unwrap_or(u16::MAX))
}

/// Resolve which tab slot a drag to `cursor_x` lands in.
///
/// Tabs are laid out left to right with uniform `tab_width` starting at
/// `origin_x`. The target is the slot the cursor sits over, clamped to
/// `0..=tab_count - 1`, so dragging past a neighbour's leading edge swaps the two
/// — the slide-reorder behaviour the legacy underline animation visualised. The
/// slot is found by walking edges rather than an `f32`→`usize` cast, which keeps
/// the strict cast lints satisfied. Returns the drag source clamped when
/// `tab_count` is zero.
#[must_use]
pub fn reorder_target_index(
    cursor_x: f32,
    origin_x: f32,
    tab_width: f32,
    tab_count: usize,
    dragging: usize,
) -> usize {
    if tab_count == 0 || tab_width <= 0.0 || !cursor_x.is_finite() {
        return dragging.min(tab_count.saturating_sub(1));
    }
    let mut idx = 0usize;
    let mut edge = origin_x + tab_width;
    while cursor_x >= edge && idx + 1 < tab_count {
        idx += 1;
        edge += tab_width;
    }
    idx
}
/// The darker "tab tone" of a workspace accent: the accent washed at 25%
/// over the chrome background, matching the workspace tag fill — so tags,
/// group hairlines, active-tab underlines, and region borders all share one
/// colour.
#[must_use]
pub fn accent_tab_tone(accent: Rgba, bg: Rgba) -> Rgba {
    Rgba {
        r: accent.r.mul_add(0.25, bg.r * 0.75),
        g: accent.g.mul_add(0.25, bg.g * 0.75),
        b: accent.b.mul_add(0.25, bg.b * 0.75),
        a: 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CONTEXT_DANGER_COLOR, CONTEXT_WARN_COLOR, TAB_FLASH_SECS, TabBarColors, badge_label,
        context_suffix, flash_blend, px_units, reorder_target_index, tab_display_title,
        tab_flash_intensity,
    };
    use gpui::Rgba;

    // @lat: [[test#GPUI Client Headless Suites#Window opacity#Chrome backgrounds scale, chrome content does not]]
    #[test]
    fn chrome_backgrounds_scale_but_content_does_not() {
        let chrome = scribe_common::theme::minimal_dark().chrome;
        let opaque = TabBarColors::from_chrome(&chrome, 1.0);
        let translucent = TabBarColors::from_chrome(&chrome, 0.85);

        // Filled surfaces let the desktop through.
        assert!((translucent.bg.a - opaque.bg.a * 0.85).abs() < 1e-6);
        assert!((translucent.active_bg.a - opaque.active_bg.a * 0.85).abs() < 1e-6);
        assert!((translucent.gradient_top.a - opaque.gradient_top.a * 0.85).abs() < 1e-6);
        // Colour itself is untouched, so only translucency changes.
        assert!((translucent.bg.r - opaque.bg.r).abs() < 1e-6);

        // Content keeps the theme's own alpha at any opacity.
        assert!((translucent.text.a - opaque.text.a).abs() < 1e-6);
        assert!((translucent.separator.a - opaque.separator.a).abs() < 1e-6);
        assert!((translucent.accent.a - opaque.accent.a).abs() < 1e-6);

        // An out-of-range config value clamps instead of overshooting.
        assert!((TabBarColors::from_chrome(&chrome, 1.5).bg.a - opaque.bg.a).abs() < 1e-6);
        assert!(TabBarColors::from_chrome(&chrome, -0.2).bg.a.abs() < 1e-6);
    }

    // @lat: [[client#GPUI Titlebar#Tab flash envelope self-decays]]
    #[test]
    fn flash_intensity_decays_to_none_at_envelope_end() {
        assert_eq!(tab_flash_intensity(0.0), Some(1.0));
        assert!(tab_flash_intensity(TAB_FLASH_SECS / 2.0).unwrap() < 1.0);
        assert_eq!(tab_flash_intensity(TAB_FLASH_SECS), None);
        assert_eq!(tab_flash_intensity(-1.0), None);
        assert_eq!(tab_flash_intensity(f32::NAN), None);
    }

    // @lat: [[client#GPUI Titlebar#Flash blends accent without touching alpha]]
    #[test]
    fn flash_blend_preserves_alpha_and_clears_when_none() {
        let base = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.5 };
        let accent = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
        assert_eq!(flash_blend(base, accent, None), base);
        let blended = flash_blend(base, accent, Some(1.0));
        assert!((blended.r - 0.45).abs() < 1e-6, "peak mix is FLASH_MAX_MIX");
        assert!((blended.a - 0.5).abs() < 1e-6, "alpha untouched");
    }

    // @lat: [[client#GPUI Titlebar#Titles truncate with an ellipsis]]
    #[test]
    fn title_truncates_with_ellipsis_and_flags_it() {
        assert_eq!(tab_display_title("short", 10), ("short".to_owned(), false));
        let (text, truncated) = tab_display_title("a-very-long-title", 6);
        assert!(truncated);
        assert_eq!(text.chars().count(), 6);
        assert!(text.ends_with('\u{2026}'));
        assert_eq!(tab_display_title("x", 0), (String::new(), true));
    }

    // @lat: [[client#GPUI Titlebar#Context suffix bands and suppression]]
    #[test]
    fn context_suffix_bands_and_suppresses() {
        assert_eq!(context_suffix(50, 70, 90, false), None);
        let warn = context_suffix(70, 70, 90, false).unwrap();
        assert_eq!(warn.text, " 70%");
        assert_eq!(warn.color, CONTEXT_WARN_COLOR);
        assert_eq!(context_suffix(92, 70, 90, false).unwrap().color, CONTEXT_DANGER_COLOR);
        assert_eq!(context_suffix(85, 70, 90, true), None, "pulsing suppresses");
    }

    // @lat: [[client#GPUI Titlebar#Badge shown only for named multi-workspace]]
    #[test]
    fn badge_label_requires_named_multi_workspace() {
        assert_eq!(badge_label(Some("work"), true), Some("work"));
        assert_eq!(badge_label(Some("work"), false), None);
        assert_eq!(badge_label(None, true), None);
        assert_eq!(badge_label(Some(""), true), None);
        assert_eq!(badge_label(Some("  "), true), None);
        assert_eq!(badge_label(Some(" work "), true), Some("work"));
    }

    // @lat: [[client#GPUI Titlebar#Drag reorder resolves the target slot]]
    #[test]
    fn reorder_target_walks_to_the_hovered_slot() {
        assert_eq!(reorder_target_index(250.0, 0.0, 100.0, 3, 1), 2);
        assert_eq!(reorder_target_index(50.0, 0.0, 100.0, 3, 1), 0);
        assert_eq!(reorder_target_index(-10.0, 0.0, 100.0, 3, 1), 0);
        assert_eq!(reorder_target_index(1.0e6, 0.0, 100.0, 3, 1), 2, "clamps to last");
        assert_eq!(reorder_target_index(50.0, 0.0, 100.0, 0, 4), 0, "empty is a no-op");
    }

    // @lat: [[client#GPUI Titlebar#Column-to-pixel conversion saturates]]
    #[test]
    fn px_units_saturates_beyond_u16() {
        assert!((px_units(3) - 3.0).abs() < 1e-6);
        assert!((px_units(1_000_000) - f32::from(u16::MAX)).abs() < 1e-6);
    }
}
