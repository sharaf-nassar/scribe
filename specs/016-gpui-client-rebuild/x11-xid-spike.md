# X11 XID Capability Spike

GPUI v1.12.0 exposes its X11 window ID through `raw-window-handle`, so the
focus guard can retain its direct EWMH comparison without title/PID lookup.

## Decision

Use the direct XID path for the GPUI client. The focus guard will obtain the
window's `RawWindowHandle` from `gpui::Window`, accept its `Xcb` variant, and
pass `handle.window.get()` to `X11FocusGuard::new`. The existing guard then
compares that XID with `_NET_ACTIVE_WINDOW` through its independent `x11rb`
connection.

Do not implement the EWMH-by-title/PID fallback. It is unnecessary at the
pinned GPUI revision and would make matching less reliable when several Scribe
windows or processes share a title.

## Evidence

The pinned Zed tag `v1.12.0` (`f96212f2c50f54d93712fa130d6226b1ce7d76b5`)
implements `raw_window_handle::HasWindowHandle` for `gpui::Window` in
`crates/gpui/src/window.rs`; the implementation delegates to the platform
window. Its Linux X11 implementation in
`crates/gpui_linux/src/linux/x11/window.rs` returns
`RawWindowHandle::Xcb(XcbWindowHandle { window: self.0.x_window, .. })`.
`XcbWindowHandle::window` is a non-zero `xcb_window_t`, i.e. the X11 XID
required by EWMH and `x11rb`.

## Demo

The following probe is the integration shape for the GPUI scaffold. It prints
the XID after `open_window` creates the native window; a non-X11 backend is
explicitly rejected rather than accidentally enabling the X11-only guard.

```rust
use gpui::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

fn x11_xid(window: &Window) -> Option<u32> {
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Xcb(handle) => Some(handle.window.get()),
        RawWindowHandle::Xlib(handle) => u32::try_from(handle.window).ok(),
        _ => None,
    }
}
```

The probe was checked against the pinned source with the temporary GPUI
example `x11_xid_demo`. Its full Cargo build could not complete because a
sibling worker held Cargo's package-cache lock after three retries; the
scaffold bead should run this same example under Xvfb as its first live-window
smoke check.

## Verification

Source inspection verified both ends of the API path at the pinned revision:
`Window::window_handle()` is public through `HasWindowHandle`, and the X11
platform returns the XCB window field as the raw handle. This preserves the
current guard's exact `_NET_ACTIVE_WINDOW == our_window` semantics and has no
runtime cost beyond the existing handle extraction during window setup.
