---
title: Negotiated Kitty keys still arrive as legacy bytes
date: 2026-08-24
component: crates/scribe-client terminal input
tags: [kitty-keyboard, gpui, terminal-input, csi-u, parity-port]
problem_type: bug
---

## Problem

Applications negotiate Kitty keyboard reporting, but modified keys still arrive through legacy encoding. In Pi, Shift+Enter arrives as carriage return and submits the prompt instead of inserting a newline.

A throwaway regression test compared the production Shift+Enter path with the existing encoder under Kitty disambiguation. Production returned `[13]`; the encoder returned `[27, 91, 49, 51, 59, 50, 117]`, or `ESC [ 13 ; 2 u`.

## Root cause

The GPUI cutover ported the complete byte encoder without connecting it to live terminal state. `encode_key` always calls `input::encode` with `TerminalMode::legacy()` (`crates/scribe-client/src/main.rs:10240-10253`). The root registers only key-down input (`main.rs:11677-11679`), although the adapter already defines `KeyInput::from_key_up` (`crates/scribe-client/src/input.rs:161-165`).

The client already has the needed per-pane state. `DisplayOnlyTerminal` enables Kitty tracking when it creates its Alacritty `Term` (`crates/scribe-client/src/terminal.rs:289-299`) and reads other live DEC modes from `term.mode()` for bracketed paste and mouse reporting (`terminal.rs:728-765`). The missing bridge is local, not an IPC problem.

Full protocol support has a second limit. `gpui::Keystroke` carries no numeric-keypad location, so `KeyInput::from_key_down` marks every event as `KeyLocation::Standard` (`crates/scribe-client/src/input.rs:12-17,150-158`). Per this investigation, the pinned GPUI Linux backend also suppresses modifier-only key events before Scribe's root router sees them.

## What didn't work

Golden tests alone were too low-level. `scribe-38e.21` proved that the pure encoder matches the old client's Kitty, legacy, DECCKM, and DECPAM byte fixtures. It did not prove the running GPUI client supplied a negotiated mode.

`scribe-38e.84` fixed dropped PageUp/PageDown and other named keys by routing production input through the ported encoder, but deliberately passed `TerminalMode::legacy()`. That fixed the reported legacy keys while leaving every negotiated branch unreachable. Repeating either change would preserve the bug.

The documentation split also hid the gap. `lat.md/client.md` describes negotiated mode behavior as the key-translation contract, then its GPUI encoder section admits that production always passes legacy mode. Treat a stated exception inside a parity section as unfinished work, not completed architecture.

## Fix

Fix filed as epic `scribe-99uj`, unlanded as of this writing.

- `scribe-99uj.1` wires the focused pane's five Kitty flags, DECCKM/DECPAM state, config opt-out, and key-up events into the existing encoder. It reuses the old live-mode mapping instead of rewriting encoding.
- `scribe-99uj.2` restores physical key identity at the GPUI boundary so keypad, modifier-only, lock, location, repeat, and release cases can satisfy the protocol without guessing from aggregate modifiers.

## Prevention

A ported state machine needs a production-path oracle, not only pure fixtures. For terminal input parity, keep all three checks:

1. Pure byte fixtures for each protocol branch.
2. A production seam test proving the live router passes the focused pane's mode into that encoder.
3. A real GPUI-to-PTY probe that negotiates flags, switches panes, pushes and pops modes, and observes exact bytes.

When a framework event type omits physical information the old backend supplied, record the loss as a blocking parity gap. An intermediate model with richer fields does not restore data unless the platform adapter can populate them.
