//! Local share-join hook: start this client as a second participant in a window
//! that another process on THIS machine already holds.
//!
//! A normal startup may name a restored window, while a fresh startup sends
//! `Hello { window_id: None }`. Neither means "join": only this hook identifies
//! a launch that should attach to a live owner rather than open its own window.
//! The server answers `SessionList` with only the *calling* window's
//! sessions (falling back to unowned ones), so a client that got its own window
//! renders an empty grid while another process's panes run untouched.
//!
//! `SCRIBE_JOIN_WINDOW` names that window and sets `Hello.join_window`. The
//! server resolves that explicit claim as an additive share join whenever the
//! sharing mode is not `single_controller`, so both processes stay attached to
//! the same panes and both receive live output. The visual E2E rig is the
//! first consumer: `scribe-test` creates and holds the session (keeping
//! `wait-output` / `snapshot` usable) and the GPUI client joins that same window
//! so the frame under the camera is the pane the assertions drive.
//!
//! Unset, the default for user launches and restore claims, leaves the bit false.

use scribe_common::ids::WindowId;

/// The local share-join env var: the full UUID of a window this process should
/// join instead of claiming one of its own.
pub const JOIN_WINDOW_ENV: &str = "SCRIBE_JOIN_WINDOW";

/// Read the optional [`JOIN_WINDOW_ENV`] hook. `None` (unset, empty, or
/// unparsable) keeps the stock `Hello { window_id: None }` handshake.
#[must_use]
pub fn join_window_from_env() -> Option<WindowId> {
    let raw = std::env::var(JOIN_WINDOW_ENV).ok()?;
    parse_join_window(&raw)
}

/// Parse a [`JOIN_WINDOW_ENV`] value into a [`WindowId`]. Split out from the env
/// read so it is testable without mutating process env (which the workspace
/// lints ban). An unparsable value is warned about and ignored rather than
/// failing the launch — a client that cannot join still starts with its own
/// window instead of refusing to run.
#[must_use]
pub fn parse_join_window(raw: &str) -> Option<WindowId> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.parse::<WindowId>() {
        Ok(window_id) => Some(window_id),
        Err(error) => {
            tracing::warn!(%error, value = %trimmed, "invalid SCRIBE_JOIN_WINDOW; ignoring");
            None
        }
    }
}

#[cfg(test)]
mod tests;
