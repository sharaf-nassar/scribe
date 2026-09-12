---
title: Fresh Linux installation has proportional terminal text and no native window controls
date: 2026-09-11
component: scribe-client packaging and Linux backend selection
problem_type: bug
tags: [fresh-install, packaging, fonts, gpui, wayland, gnome]
---

## Reproduction

Scribe **0.1.14-1**, Ubuntu arm64 / GNOME Shell 50.1 / Mesa 26.0.3, Apple M1
Max Asahi Vulkan, 2x scale. Stock config: JetBrains Mono, 14px, padding 0,
zoom 0, opacity 1. The installed client and repository initially matched
`v0.1.14` / `9992f2f`; implementation incorporated latest main `9efe388` first.

The terminal's custom tab strip and status bar existed, but the **OS window
frame and minimize/maximize/close controls did not**. Terminal characters were
proportional and did not fit their cell grid. `fc-match 'JetBrains Mono'`
returned **Noto Sans Regular**, not a terminal font.

The original client SHA-256 was
`98e1633661307dfdb6bb428ddb75adde1ba71688b084a42f00107c8a2bdc056e`;
the downloaded Debian artifact was
`a5af04b993dd0557723dd0d69fead29c6529a21f6e877e17838fee6ee61e125e`.

## Root causes

### Default font absent from the artifact

`config.rs::default_font` selects JetBrains Mono. Neither Debian flavor supplied
it, and `fonts.rs` embedded only the Nerd Font symbols face. GPUI's missing
primary-family lookup falls back to its UI font stack, independently of Scribe's
per-glyph fallback chain. Thus specifying that chain did not protect the grid.

Both Docker images independently installed `fonts-jetbrains-mono`, hiding the
artifact's undeclared runtime dependency.

Changing only the font to installed DejaVu Sans Mono repaired text on native
Wayland immediately, without repairing the missing window frame.

### No owner for native decorations on GNOME Wayland

The terminal requests server-side decorations and leaves controls to the OS.
GNOME does not advertise `zxdg_decoration_manager_v1`; GPUI logs:

> Server-side decorations requested, but the Wayland server does not support
> them. Falling back to client-side decorations.

GPUI's fallback does not draw controls. Commit `5c1be77` had removed Scribe's
old custom controls while restoring native ownership. Existing X11/xdotool
visual tests could not expose GNOME Wayland's missing native-decoration path.

Launching the same binary on XWayland restored the **actual Ubuntu frame**.
The hardware Vulkan adapter was unchanged, isolating this from a GPU failure.

## Rejected workarounds and baseline restoration

The initial workstation workaround installed user-local fonts and a user-local
`.desktop` override. At the user's request, **both were completely removed**:
`~/.local/share/fonts/scribe-jetbrains-mono/` and
`~/.local/share/applications/scribe.desktop` no longer exist. Fontconfig again
resolves JetBrains Mono to Noto Sans. The original configuration compares
byte-for-byte equal to its backup. Original saved window geometry was restored.
The stock desktop launcher reproduced both failures again before source fixes
were tested. No installed Scribe executable or system package was replaced.

An intermediate implementation drew client controls and resize gutters.
The user rejected that appearance: the requirement is **Ubuntu's native OS
chrome**, not a Scribe imitation. That implementation and its tests were
removed. The settings window's existing designed client frame is untouched.

## Long-term fixes

1. **Self-contained fonts.** `fonts.rs::register_embedded_fonts` registers four
   unmodified JetBrains Mono 2.304 faces alongside the existing symbols before
   GPUI caches any family lookup. Font files, upstream OFL and checksums are
   committed. Stable/dev Debian and macOS packages include the license. This
   works for direct binaries and offline `dpkg -i` upgrades, not just package
   managers that can resolve a new dependency.
2. **Safe missing-family resolution.** `GridFont::resolve_family` resolves the
   configured name once at startup/font/zoom reload. Available user choices and
   canonical case are preserved; unavailable families select the bundled face
   and log a warning without rewriting config. No per-row/per-frame enumeration.
3. **Real native frame selection.** Linux `native_window_exit` runs before GPUI,
   hook repair, singleton claims or IPC. A compositor advertising the decoration
   protocol keeps Wayland. Otherwise Scribe verifies a live EWMH X11 window
   manager, then `exec`s the same binary/arguments with its new process's
   `WAYLAND_DISPLAY` removed. A private marker retains the original socket for
   systemd environment import: a future server start must not erase Wayland from
   the user manager or its shells just because this client uses X11. The desktop
   session identity, launcher and running server are unchanged. No hardcoded
   GNOME identity and no custom terminal frame.
4. **Explicit unsupported path.** With neither native-frame backend available,
   report how to enable one and exit before claiming a singleton or touching
   sessions. The combined local display probe has a two-second deadline.
   X11/headless, help/version, hardware/image probes, settings, and non-Linux
   paths do not run it.
5. **Unmasked regression coverage.** Both Docker images omit JetBrains Mono.
   The existing config-reload visual suite rejects a contaminated host font set,
   requires rendered fixture text, then compares default and missing-family
   pixels without a restart. Font unit tests pin family, weights, styles,
   coverage and equal ASCII advances; backend tests pin selection/refusal and
   argument/environment preservation.

## Verification

All workstation tests below used **no user font and no launcher override**.

- Full workspace tests and strict workspace/all-target/all-feature Clippy pass.
- `lat check`, formatting and all pre-commit hooks pass, including cargo deny,
  machete, gitleaks, Taplo and staged ratchets.
- Both Debian variants built with `cargo deb --no-build --profile dev`; the
  license payload matches source. These are **development-profile verification
  artifacts**, not a published/release-profile build.
- The client extracted from the stable `.deb` was launched directly from the
  unchanged GNOME Wayland environment. It automatically selected X11, rendered
  the bundled font and used Mutter's actual native frame. X11 reported
  `_NET_FRAME_EXTENTS = [0, 0, 74, 0]` at 2x, proving the 37-logical-pixel titlebar
  belongs to the WM, not Scribe. The same Asahi hardware Vulkan adapter remained
  selected.
- Actual native buttons maximize/minimize/restore; the close button reaches
  Scribe's existing session-safe confirmation. Settings opens/closes normally.
- Equal-length digit/`M`/`i` rows and bold/italic text render correctly. A
  deliberately nonexistent configured family warns and paints **byte-identical
  fixture pixels** to the bundled default. Clear/home the fixture before
  comparison: a previously wrapped shell command can reflow during a snapshot
  and must not be mistaken for font drift.
- Removing `DISPLAY` under GNOME gives the actionable missing-native-frame error
  and exit 1; help/version still succeed with both display paths unusable.
  A fake Wayland socket that accepts but never answers exits with the typed
  timeout error after **2.002 seconds**, before any session-side effects.
- The server PID **57423** and original shell PID **59594** retain their original
  start times. Only clients were restarted and disposable test tabs were closed.

Build/check tools were provisioned under `/tmp/scribe-build-tools`, without
installing system packages or fonts. Beads was obtained there but `bd ready`
reports **no database in this clone**; no replacement database or invented issue
ID was created. The dependency's Taplo 0.9.3 Python sdist omits `taplo-lsp` on
arm64; supplying that crate from the same upstream tag produced a local wheel,
allowing the unmodified repository hooks to run.

### Performance and remaining platform gates

No renderer hot-path algorithm changed. Font enumeration is on reload only.
Native selection adds one bounded startup probe and, on GNOME, one exec.
Five duplicate-launch samples of the extracted artifact measured 10.29–14.13 ms
with automatic selection (median 12.81 ms), versus 9.11–12.88 ms for explicit X11
(median 10.43 ms): approximately **2.38 ms median selection overhead** locally.
This is startup/singleton timing, not a frame-rate benchmark.

Native Wayland on a decoration-capable compositor, macOS and the Docker visual
suite still require their normal runtime environments; no successful runtime
result is claimed for those here. Shared workspace tests and source/package
checks do not substitute for those platform gates.

## Local evidence

`test-output/fresh-install-2026-09-11/` holds cropped before/reverted/packaged
screenshots and the native-close capture (ignored, not committed). Full captures,
client logs, display-probe timing, and original config/state backups remain in
`/tmp/scribe-investigation/`. Mutter ScreenCast/RemoteDesktop sessions were
short-lived and stopped after each operation; no capture daemon or desktop-wide
security override was installed.
