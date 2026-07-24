# GPUI window opacity on Wayland and X11

GPUI at Scribe's pinned Zed revision supports a transparent native window on
both Linux backends, and the demo changes the painted background alpha live.

## Demo

Run the standalone demo in a Wayland compositor session:

```bash
cargo run --manifest-path tools/gpui-window-opacity-spike/Cargo.toml
```

Run the same command in an X11 session with a compositor. Use the **Decrease
opacity** and **Increase opacity** controls; each redraw changes the displayed
percentage and makes the desktop behind the window respectively more or less
visible. No process or window restart is involved.

The demo uses the exact `v1.12.0` pin, commit
`f96212f2c50f54d93712fa130d6226b1ce7d76b5`.

## Evidence

The pinned public API provides `WindowBackgroundAppearance::Transparent` in
`WindowOptions`, which creates an alpha-capable surface. Its Linux Wayland
backend enables a transparent surface and removes the opaque region when the
appearance is non-opaque. Its X11 backend selects a transparent visual when
available and updates WGPU surface transparency for a transparent appearance.

The demo leaves the native surface in transparent mode. It changes only the
root element's alpha and calls `Context::notify`, so GPUI repaints the existing
window live. This is numeric opacity behaviour without relying on a
backend-specific window recreation path.

## Decision

Keep the `appearance.opacity` configuration key. Clamp every live update to
`0.0..=1.0`, keep the GPUI window background transparent, and repaint Scribe's
root terminal/chrome background with that alpha. Do not model the value as a
native opaque/transparent switch: that API selects surface capability, while
per-pixel alpha supplies the user-visible opacity value.

## US3 impact

US3's live-opacity requirement remains achievable on Wayland and X11. The
GPUI client must make root terminal and chrome backgrounds alpha-aware and
notify the view after config reload. Text and controls remain opaque unless a
future setting explicitly changes them.
