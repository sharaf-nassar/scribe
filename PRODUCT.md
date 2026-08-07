# Scribe

All facts below are inferred from the codebase and lat.md knowledge graph
(no user interview was possible; the user delegated with "complete freedom").
Confirmed-by-code facts are unmarked; judgment calls are marked [assumed].

## Platform

desktop (native Linux/macOS app built on GPUI; not web — no HTML/CSS surface)

## Users

Professional developers who live in a terminal all day and run AI coding
agents (Claude Code, Codex) inside it. Keyboard-first, dense-information
tolerant, dark-environment usage [assumed: primarily evening/low-light and
long sessions, given the terminal domain].

## Product Purpose

Scribe is a GPU-accelerated terminal emulator with a client-server
architecture and first-class AI process awareness: it detects and surfaces
AI assistant states (processing, waiting, permission prompts, errors)
across tabs, panes, and a prompt bar.

## Operating Context

- The settings surface is a second GPUI window inside the running client
  process (`crates/scribe-client/src/settings/`), opened from a keybinding,
  the command palette, the status-bar gear, or `scribe-client --settings`.
- Settings edits live-apply through a TOML config write plus file-watcher
  reload; there is no Save button.
- The settings palette is deliberately fixed (independent of the active
  terminal theme) so the surface stays a stable instrument while the user
  edits themes.
- Eleven pages: Appearance, Colors, Terminal, Keybindings, AI, Environment,
  Workspaces, Updates, Releases, Notifications, Remote.

## Capabilities and Constraints

- UI is Rust/GPUI elements (flex, div, no web stack); icons come from the
  embedded Symbols Nerd Font Mono; no SVG asset pipeline exists.
- Full AccessKit accessibility contract and a window-local keyboard
  traversal order must be preserved exactly.
- Validation runs only inside the Docker E2E harness; visual tests assert
  behavior and lit-pixel counts, with one layout-derived click coordinate
  (`FILTERED_WORKSPACES_*` in tests/e2e/visual/settings-entry.sh).
- Window minimum is 1040×720 with client-side decorations and a 6px resize
  gutter on Linux.

## Brand Commitments

- Amber (`#f5b83a` family) is the settings surface's incumbent accent
  [assumed: carries CRT amber-phosphor heritage worth keeping as identity].
- Warm off-white text on deep neutral ground; flat surfaces, 0–6px radii,
  one-pixel rules; no cards-with-shadows chrome [assumed from incumbent
  design and user's stated frontend defaults].

## Product Principles

- Operate surface: scanability, native expectations, and task speed
  outrank expression; brand lives in precise details.
- Monospace is reserved for real data (paths, hex colors, keybindings,
  numeric values), not decoration.
- Live-apply is the core interaction truth: controls commit immediately.
