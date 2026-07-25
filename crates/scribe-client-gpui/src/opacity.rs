//! Window opacity: turning `appearance.opacity` into painted surface alpha.
//!
//! The GPUI window is always created with a transparent native surface (see
//! the `window-opacity-wayland-x11` spike), so the user-visible opacity is
//! purely a property of the alpha Scribe paints into its own root, terminal
//! and chrome backgrounds. This module owns that derivation: clamping the
//! configured value and scaling background alpha by it, while leaving glyphs,
//! borders and other foreground content fully opaque so text stays readable
//! over whatever shows through.

use gpui::Rgba;

/// The opacity used when the configured value is unusable (NaN).
pub const DEFAULT_OPACITY: f32 = 1.0;

/// Clamp a configured `appearance.opacity` into the paintable range.
///
/// Out-of-range values (`1.5`, `-0.2`) saturate to `1.0` / `0.0` rather than
/// producing an invalid colour, and a NaN falls back to fully opaque so a
/// malformed config can never render an invisible window.
#[must_use]
pub fn clamp_opacity(value: f32) -> f32 {
    if value.is_nan() { DEFAULT_OPACITY } else { value.clamp(0.0, 1.0) }
}

/// Scale a GPUI colour's alpha by `opacity`, clamping the value first.
///
/// Colours that are already partially transparent (theme chrome slots derived
/// with `with_alpha`) keep their relative translucency: the two alphas
/// multiply, exactly as the legacy renderer's `apply_opacity_to_instances`
/// scaled each cell's background alpha.
#[must_use]
pub fn scale_alpha(color: Rgba, opacity: f32) -> Rgba {
    Rgba { a: color.a * clamp_opacity(opacity), ..color }
}

/// Convert an sRGB theme slot into a background colour with `opacity` folded
/// into its alpha.
///
/// This is the single entry point every alpha-aware surface uses, so the
/// terminal grid and the chrome bands can never drift apart.
#[must_use]
pub fn surface(color: [f32; 4], opacity: f32) -> Rgba {
    scale_alpha(opaque_slot(color), opacity)
}

/// Scale an sRGB theme slot's alpha by `opacity`, staying in `[f32; 4]` space.
///
/// Used where a palette struct is rebuilt from theme slots before it reaches
/// GPUI (for example [`StatusBarColors`](crate::status_bar::StatusBarColors)).
#[must_use]
pub fn scale_slot(color: [f32; 4], opacity: f32) -> [f32; 4] {
    [color[0], color[1], color[2], color[3] * clamp_opacity(opacity)]
}

/// Convert an sRGB theme slot into a GPUI colour with its own alpha intact.
///
/// Foreground content (glyphs, separators, borders) uses this: opacity must
/// not make text bleed into the desktop behind the window.
#[must_use]
pub const fn opaque_slot(color: [f32; 4]) -> Rgba {
    Rgba { r: color[0], g: color[1], b: color[2], a: color[3] }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_OPACITY, clamp_opacity, opaque_slot, scale_alpha, scale_slot, surface};
    use gpui::Rgba;

    const EPS: f32 = 1e-6;

    // @lat: [[test#GPUI Client Headless Suites#Window opacity#Clamps configured opacity]]
    #[test]
    fn clamps_configured_opacity() {
        assert!((clamp_opacity(0.85) - 0.85).abs() < EPS);
        assert!((clamp_opacity(1.5) - 1.0).abs() < EPS);
        assert!((clamp_opacity(-0.2) - 0.0).abs() < EPS);
        assert!((clamp_opacity(f32::NAN) - DEFAULT_OPACITY).abs() < EPS);
    }

    // @lat: [[test#GPUI Client Headless Suites#Window opacity#Backgrounds carry the opacity alpha]]
    #[test]
    fn backgrounds_carry_the_opacity_alpha() {
        let slot = [0.1, 0.2, 0.3, 1.0];

        let opaque = surface(slot, 1.0);
        assert!((opaque.a - 1.0).abs() < EPS);
        assert!((opaque.r - 0.1).abs() < EPS);

        let translucent = surface(slot, 0.85);
        assert!((translucent.a - 0.85).abs() < EPS);
        // Only alpha moves: the RGB stays the theme's colour so a composited
        // desktop blends toward the backdrop instead of shifting hue.
        assert!((translucent.r - 0.1).abs() < EPS);
        assert!((translucent.b - 0.3).abs() < EPS);

        // An out-of-range config value saturates rather than overshooting.
        assert!((surface(slot, 1.5).a - 1.0).abs() < EPS);
        assert!((surface(slot, -0.2).a - 0.0).abs() < EPS);
    }

    // @lat: [[test#GPUI Client Headless Suites#Window opacity#Already-translucent chrome multiplies]]
    #[test]
    fn already_translucent_chrome_multiplies() {
        let half = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.5 };
        assert!((scale_alpha(half, 0.5).a - 0.25).abs() < EPS);
        assert!((scale_slot([1.0, 1.0, 1.0, 0.5], 0.5)[3] - 0.25).abs() < EPS);
        // Foreground slots keep their own alpha untouched.
        assert!((opaque_slot([0.0, 0.0, 0.0, 0.4]).a - 0.4).abs() < EPS);
    }
}
