//! Shared keyboard activation handling for GPUI buttons.

use gpui::{App, KeyDownEvent, Window};

/// Keep unmodified button activation keys out of the terminal key path.
pub fn stop_activation_key(event: &KeyDownEvent, _window: &mut Window, app: &mut App) {
    if !event.keystroke.modifiers.modified()
        && matches!(event.keystroke.key.as_str(), "enter" | "space")
    {
        app.stop_propagation();
    }
}
