//! Library surface of the GPUI Scribe client rebuild.
//!
//! Holds the display-independent building blocks the GPUI paint path consumes
//! — the xterm-256 [`palette`], terminal cell [`color`] semantics, and the
//! procedural [`box_drawing`] rasterizer — alongside the terminal [`input`]
//! byte encoder. Each of those modules is ported byte-for-byte from the legacy
//! client so terminal output stays identical across the cutover.
//!
//! It also hosts the ported pane/workspace split trees and their GPUI entity
//! wrappers. The pure split-tree logic lives in [`layout`] and
//! [`workspace_layout`]; the [`pane_tree`] and [`workspace_tree`] modules wrap
//! that logic in `gpui::Entity` models that emit change events (the workspace
//! model emits the `ReportWorkspaceTree` payload) on every mutation so the
//! chrome and IPC layers can react.
//!
//! Consumers that wire these into the live GPUI view (IPC sink, keybinding
//! dispatch, window shell) land in later beads of the `gpui-client-rebuild`
//! epic; the `scribe-client-gpui` binary (`main.rs`) remains the display-only
//! scaffold spike until then.

pub mod box_drawing;
pub mod color;
pub mod input;
pub mod layout;
pub mod palette;
pub mod pane_tree;
pub mod url_detect;
pub mod workspace_layout;
pub mod workspace_tree;

/// Assert two linear RGBA colours are bit-for-bit identical.
///
/// The rendering-parity tests require byte-exact matches against the legacy
/// renderer, so channels are compared by their raw IEEE-754 bit patterns
/// (`f32::to_bits`) rather than with `==`, which also keeps the strict
/// `clippy::float_cmp` lint satisfied without a suppression.
#[cfg(test)]
pub(crate) fn assert_rgba_eq(actual: [f32; 4], expected: [f32; 4]) {
    for (i, (a, e)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(
            a.to_bits(),
            e.to_bits(),
            "channel {i} differs: {a} (0x{:08x}) != {e} (0x{:08x})",
            a.to_bits(),
            e.to_bits(),
        );
    }
}
