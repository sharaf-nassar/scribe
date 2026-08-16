//! Shared formatting for the AI context-window chrome.
//!
//! Two client surfaces display the AI context-window fill percentage: the
//! per-pane prompt bar renders a segmented meter (`▰▰▱ 72%`) and the tab label
//! appends a bare suffix (` 72%`) whenever usage is known. Both strings
//! are produced here so the prompt bar, the tab bar, and the E2E harness that
//! asserts on them all agree on one spelling.
//!
//! Only the text is shared. Band colours stay with each surface because they
//! resolve through different palettes — the prompt bar reads the configured
//! [`crate::config::AiContextThresholds`] hex colours while the tab bar uses its
//! own fixed band colours.

/// Number of segments in the prompt-bar context level meter.
pub const CONTEXT_BAR_SEGMENTS: usize = 3;

/// Filled segment of the context-window level meter (BLACK PARALLELOGRAM).
pub const BAR_FULL: char = '\u{25B0}';

/// Empty segment of the context-window level meter (WHITE PARALLELOGRAM).
pub const BAR_EMPTY: char = '\u{25B1}';

/// Format the segmented prompt-bar context meter label (`▰▰▱ 66%`).
///
/// `percent` is clamped to 100 so a misbehaving producer cannot overflow the
/// meter. Segments fill by `div_ceil`, so any non-zero percentage lights at
/// least one segment.
#[must_use]
pub fn context_meter_label(percent: u8) -> String {
    let percent = percent.min(100);
    let filled =
        (usize::from(percent) * CONTEXT_BAR_SEGMENTS).div_ceil(100).min(CONTEXT_BAR_SEGMENTS);
    let mut label = String::with_capacity(CONTEXT_BAR_SEGMENTS + 5);
    for _ in 0..filled {
        label.push(BAR_FULL);
    }
    for _ in filled..CONTEXT_BAR_SEGMENTS {
        label.push(BAR_EMPTY);
    }
    label.push(' ');
    label.push_str(&percent.to_string());
    label.push('%');
    label
}

/// Format the tab-inline context suffix (`" 72%"`), or `None` when the tab must
/// not show one.
///
/// The suffix is suppressed while `pulsing` is set, because a
/// `PermissionPrompt` / `WaitingForInput` session already draws attention
/// through its pulse and the suffix must not compete with it.
#[must_use]
pub fn tab_context_suffix_text(percent: u8, pulsing: bool) -> Option<String> {
    if pulsing {
        return None;
    }
    Some(format!(" {percent}%"))
}

#[cfg(test)]
mod tests {
    use super::{context_meter_label, tab_context_suffix_text};

    // @lat: [[common#AI Context Chrome#Meter label fills and clamps]]
    #[test]
    fn meter_label_fills_and_clamps() {
        assert_eq!(context_meter_label(0), "▱▱▱ 0%");
        assert_eq!(context_meter_label(1), "▰▱▱ 1%");
        assert_eq!(context_meter_label(50), "▰▰▱ 50%");
        assert_eq!(context_meter_label(67), "▰▰▰ 67%");
        assert_eq!(context_meter_label(100), "▰▰▰ 100%");
        assert_eq!(context_meter_label(255), "▰▰▰ 100%");
    }

    // @lat: [[common#AI Context Chrome#Tab suffix shows known context unless pulsing]]
    #[test]
    fn tab_suffix_shows_known_context_unless_pulsing() {
        assert_eq!(tab_context_suffix_text(50, false).as_deref(), Some(" 50%"));
        assert_eq!(tab_context_suffix_text(70, false).as_deref(), Some(" 70%"));
        assert_eq!(tab_context_suffix_text(91, false).as_deref(), Some(" 91%"));
        assert_eq!(tab_context_suffix_text(50, true), None);
    }
}
