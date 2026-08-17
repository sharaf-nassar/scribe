---
title: Dot-prefixed terminal paths open nothing
date: 2026-08-17
component: scribe-client URL detection
tags: [terminal-links, relative-paths, dot-directories, ctrl-click]
problem_type: bug
---

## Problem

Ctrl+clicking `.impeccable/mocks/beads-board-signal-theme.html` did not open the browser, although running `open` with the same path worked. The file existed under the pane's working directory; the same path without its leading dot did not.

## Root cause

Bare relative-path detection starts only when the scanned character is ASCII alphanumeric (`crates/scribe-client/src/url_detect.rs:1048-1050`). The token predicate used before the slash also accepts `.`, `~`, `_`, and `-` (`crates/scribe-client/src/url_detect.rs:916-918`). This mismatch makes the scanner advance past a leading token character and emit the suffix. The reported path became `impeccable/mocks/beads-board-signal-theme.html`.

Ctrl+click forwards the detected string unchanged to `open_path` (`crates/scribe-client/src/main.rs:7903-7906`). `resolve_path` then joins that already-truncated string to the pane CWD (`crates/scribe-client/src/url_detect.rs:1140-1143`). The OS handler receives a valid-looking absolute path that names no file, so the click appears to do nothing. Right-click Open File uses the same path-opening route (`crates/scribe-client/src/main.rs:3514-3519`).

## What didn't work

Testing the shell's `open` command only checks the OS handler. It bypasses Scribe's detector, where the character was lost.

The earlier delimited-absolute-path fix in `scribe-ocby` and commit `00438a7` corrected leading `/` handling. It deliberately left the bare-relative branch's alphanumeric-only start unchanged, so it does not cover a leading dot, underscore, hyphen, or named-home tilde.

## Fix

Fix filed as `scribe-gv09`, unlanded as of this writing. The planned change makes the bare-relative prefix gate accept the same supported leading path-token characters as the pre-slash token while preserving the explicit `./`, `../`, and `~/` branches and the slash requirement.

The regression coverage will assert exact detection for `.impeccable/...`, `_private/...`, `-draft/...`, and `~alice/...`, then exercise a dot-directory HTML path through the visual Ctrl+click E2E and its `xdg-open` shim.

## Prevention

Keep detector tests exact-string based. A test that only checks whether a path span exists misses prefix loss.

When link opening fails but a shell opener works, inspect the detected target before changing `open_path`, CWD resolution, or desktop integration. The detector and opener are separate failure boundaries.
