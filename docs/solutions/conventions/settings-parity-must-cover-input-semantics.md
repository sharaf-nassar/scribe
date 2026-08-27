---
title: GPUI settings parity kept numeric values but lost typing
date: 2026-08-27
component: GPUI settings window
tags: [settings, gpui, parity-port, numeric-input, accessibility]
problem_type: bug
---

## Problem

The GPUI settings window shows every numeric setting and lets users step it, but
users cannot type an exact value. In the visual repro, attempting to replace
Font size with `23` stores `16.0`: both Enter presses increment the default
`14`, while `Ctrl+A` and the digits reach no input.

## Root cause

The generic stepper renders its number as an inert label
(`crates/scribe-client/src/settings/window.rs:6775-6824`). Settings focus opens
the shared inline editor only for `ControlKind::Text`
(`crates/scribe-client/src/settings/window.rs:1686-1690`), while stepper
activation calls `step` (`crates/scribe-client/src/settings/window.rs:1857-1859`).

Adding the existing text editor unchanged would still fail. Its helper returns a
string (`crates/scribe-client/src/settings/window.rs:798-803`), while the config
apply path deserializes numeric settings from JSON numbers
(`crates/scribe-client/src/settings/apply.rs:80-84`).

The retired webview had separate behavior that a control inventory missed:
`git show 7f90edf^:crates/scribe-settings/src/assets/settings.js` lines
1802-1861 replaced each static numeric value with a text input. The GPUI parity
work preserved keys and stepping, but not that input semantic.

## What didn't work

Prior follow-ups repaired adjacent control classes only. `scribe-19v` restored
inline entry for text and colors, and `scribe-4hp` restored discoverable choice
menus. Neither changed `ControlKind::Stepper`, so treating this as another
free-text-field gap would repeat the incomplete scope.

## Fix

Fix filed as `scribe-2ynl`, unlanded as of this writing. Reuse the native inline
input state for every generic stepper, but parse finite numeric text into a JSON
number, enforce the stepper bounds and precision, and preserve the existing
step buttons, arrow adjustment, live apply, cancellation, gating, and AccessKit
semantics.

## Prevention

Settings parity reviews must test interaction grammar, not only key inventory
and apply-path round trips. For each control kind, record whether pointer,
keyboard, direct text entry, cancellation, validation, and accessibility value
setting still work in the replacement UI. Keep one real-window E2E for every
input class whose behavior cannot be proven by the config router alone.
