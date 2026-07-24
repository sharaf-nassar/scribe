//! Pure rendering logic for the GPUI Scribe client.
//!
//! This library crate holds the display-independent building blocks the GPUI
//! paint path consumes: the xterm-256 [`palette`], terminal cell [`color`]
//! semantics, and the procedural [`box_drawing`] rasterizer. Each module is
//! ported byte-for-byte from the legacy `scribe-renderer` so terminal output
//! stays identical across the cutover. The `scribe-client-gpui` binary
//! (`main.rs`) owns the live IPC attach and GPUI window.

pub mod box_drawing;
pub mod color;
pub mod palette;

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
