---
title: Window chrome geometry settings save but do not change the UI
date: 2026-08-26
component: scribe-client settings and window chrome
tags:
  - settings
  - gpui
  - tab-bar
  - status-bar
  - config-reload
problem_type: bug
---

## Problem

`Tab height`, `Tab bar padding`, `Tab width`, and `Status bar height` appear in
Settings and save valid values, but changing them does not alter the running
terminal window. The controls are declared together in
`crates/scribe-client/src/settings/model.rs:263-270`, and their apply handlers
persist the values in `crates/scribe-client/src/settings/apply.rs:176-196`.

Per this investigation, Docker visual captures remained pixel-identical after
hot-reloading each setting across its full supported range. The client logged
`config hot-reloaded` for every edit.

## Root cause

The GPUI runtime replaced the old config-driven chrome geometry with constants
without removing the old settings. The reload plan only identifies theme, font,
and opacity changes (`crates/scribe-client/src/config.rs:91-127`), while the
foreground reload path handles those surfaces and then requests a repaint
(`crates/scribe-client/src/main.rs:2877-2950`). A repaint cannot help because
the renderers do not read the saved geometry values.

The top tab bar uses `TITLEBAR_HEIGHT = 34` and `TAB_WIDTH = 176`
(`crates/scribe-client/src/titlebar.rs:26-37`). Its row fixes height to that
constant and gives tabs equal flex growth from the fixed basis
(`crates/scribe-client/src/titlebar.rs:765-776,1170-1173`). Lower workspace
regions reserve the same fixed titlebar height
(`crates/scribe-client/src/pane_shell.rs:49-53`) and use the same fixed width
basis (`crates/scribe-client/src/main.rs:8280-8291`). The status renderer is
passed a fixed 24px metric (`crates/scribe-client/src/main.rs:10598-10600`,
`crates/scribe-client/src/window_chrome.rs:16-18`).

## What didn't work

Treating a successful config write or `config hot-reloaded` log as proof of a
working setting misses the last boundary: the painted layout must consume the
new value.

Restoring legacy fixed-width tabs is also the wrong repair. Current design gives
each tab an equal share of its strip (`lat.md/client.md:2294-2317`), and
`tests/e2e/visual/tab-width.sh:35-64` enforces that behavior. A configurable
fixed width now conflicts with a documented invariant rather than merely
lacking a callback.

## Fix

The fixes are filed and unlanded as of this writing:

- `scribe-7rr7.1` restores one live effective tab-bar height from
  `tab_height + tab_bar_padding` across paint, reservation, hit testing, pane
  sizing, lower-region bars, and startup geometry.
- `scribe-7rr7.2` removes the obsolete user-facing `tab_width` setting while
  preserving equal-share tabs and old-config parsing.
- `scribe-7rr7.3` makes `status_bar_height` drive the rendered band, startup
  sizing, and live pane reflow.

The cluster is tracked by `scribe-7rr7`.

## Prevention

Every user-facing setting needs a regression at its final observable boundary.
For geometry, verify both the pixels and the content rectangle that cedes those
pixels. A parser or settings-model test proves persistence only; it cannot prove
that GPUI paint, hit testing, and pane-grid publication use the value.

When a rebuild deliberately replaces configurable behavior with a new invariant,
remove the stale control in the same change or document and test the new meaning.
Keeping a writable field with no runtime consumer creates a false contract that
later config audits cannot distinguish from a partially ported feature.
