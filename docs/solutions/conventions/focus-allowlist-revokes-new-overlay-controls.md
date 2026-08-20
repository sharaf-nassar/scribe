---
title: A new focusable control loses focus one frame after it gains it
date: 2026-08-20
component: crates/scribe-client (GPUI shell, ensure_focus, find overlay)
tags: [gpui, focus, accessibility, overlays, ensure_focus, zed-vendored-reference]
problem_type: convention
---

## Problem

A control added to a GPUI overlay carries every focus attribute the repo's
existing button idiom uses — `track_focus`, `focus_visible`, an Enter/Space
`on_key_down` — and none of it ever fires. The focus ring never appears, the
keyboard activation path is dead, and nothing in the build looks wrong: it
compiles, clippy is clean, the unit tests pass, and the visual E2E passes,
because every one of those oracles exercises the *pointer* path.

Observed on the find overlay's previous/next/close controls (scribe-1mpq),
which shipped with an acceptance criterion demanding "keyboard activation on
Enter/Space, and a visible `focus_visible` ring" that the shipped build does
not satisfy.

## Root cause

Two independent causes, and the second is the one that generalises.

**1. The focus never arrives.** `find_control`
(`crates/scribe-client/src/search.rs:702-756`) was documented as copying
`ci_bar::action_button` verbatim but omits the single line that makes the idiom
work: `ci_bar.rs:983-987` calls `window.focus(&click_focus, cx)` inside
`on_click`; `search.rs:748-751` does not. The three handles are also built with
a bare `cx.focus_handle()` (`search.rs:430-434`) rather than the CI bar's
`.tab_index(0).tab_stop(true)` (`main.rs:9922-9927`), and
`handle_find_overlay_key` (`main.rs:8059-8082`) consumes every key including
Tab before `focus_next_titlebar_control` (`main.rs:10418-10428`) can run. So no
click, no Tab, and no traversal reaches the control.

**2. The focus would not survive anyway.** `TerminalView::ensure_focus`
(`main.rs:10275-10300`) runs at the top of every render and decides whether the
current focus is legitimate by consulting a hard-coded *allowlist* of six
claimants (`focus_is_unclaimed([bool; 6])`, `main.rs:1573-1575`). Any focus
matching no slot is declared unclaimed and `window.focus(&self.focus.root, cx)`
takes it back on the next frame.

An allowlist fails closed against anything nobody remembered to add, and this
one has already been forgotten three times: the CI bar action buttons (slot 4),
the beads inline editor (slot 5), and now the find overlay's four handles
(`search.rs:405-413`), which have no slot at all. The first two each left
behind a test that asserts only that a slot exists —
`ci_control_claim_prevents_terminal_focus_restore` (`main.rs:15379-15382`) and
`beads_editor_claim_prevents_terminal_focus_restore` (`main.rs:15385-15388`).
They document the trap rather than removing it. `command_palette.rs:378` and
`context_menu.rs:318` are unclaimed for the same reason and are harmless only
because both are key-routed at the root.

A headless probe written into `search::overlay_tests`, run, and reverted
isolated the two halves:

```
PROBE: focused_after_click=false tab_stop=false focused_after_draw=true
       enter_index 1 -> 0
```

`focused_after_draw=true` is the tell: with `ensure_focus` out of the picture
the control focuses and Enter activates it exactly as designed, so the
revocation is the shell's, not GPUI's.

## What didn't work

**Reading the diff for a missing feature.** The change looks complete —
`Role::Button`, `aria_label`, `aria_description`, `track_focus`,
`focus_visible`, Enter/Space handling are all present and all cite the idiom
they copy. Nothing is missing on the page; what is missing is one line in
another file plus a reachability property that no single file expresses.

**Trusting the test suite as evidence of a11y.** All eight headless tests and
all eight visual E2E phases pass against this code. The visual phases click the
controls, so they prove the pointer path and say nothing about focus. A passing
suite was the reason this shipped.

**Assuming the ci_bar idiom was the right one to copy.** It is the correct
idiom for a *toolbar* button that keeps focus after activation. It is the wrong
one for a find bar, where focus belongs in the query field — but that only
becomes visible once you compare against a find bar rather than against another
toolbar.

## Fix

Filed as **scribe-hqi5** (the allowlist) and **scribe-2yw1** (the find
controls, blocked on it); both unlanded as of this writing.

**Invert the predicate and delete the allowlist.** GPUI answers the real
question structurally — `FocusHandle::contains_focused`
(vendored gpui `window.rs:360-364`) asks the *rendered frame's* dispatch tree
whether the focused element is inside a subtree:

```rust
if !self.focus.root.contains_focused(window, cx) {
    window.focus(&self.focus.root, cx);
}
```

`self.focus.root` is the outermost div of `TerminalView::render`
(`main.rs:10385`), so every surface in the window is a descendant. Focus on any
live rendered control is respected; focus that is dangling or on an unmounted
handle is repaired, which is the only thing `ensure_focus` was ever for. The
`self.dialog` early-return stays — that is a containment rule, not a repair
rule. `focus_is_unclaimed` and both slot-reminder tests then delete.

**For the find controls, stop adding focus and remove what is there.** Per this
investigation the correct shape is the pointer affordance, not the focusable
button: drop `track_focus`, `focus_visible`, and the Enter/Space `on_key_down`,
focus the overlay's own query handle on click, and put the keystroke in a
tooltip.

## Prevention

**The authoritative GPUI reference is vendored in this repo's own dependency
tree.** `Cargo.toml:113` pins gpui to a Zed revision, and
`~/.cargo/git/checkouts/zed-*/<rev>/crates/` is the *entire* Zed source at that
exact revision, not just gpui. Any "what is the framework-native way to do X"
question has a first-party answer sitting on disk. For this incident the
analogue was `crates/search/src/search_bar.rs:40-67`, Zed's own find-bar
button, which focuses the query editor on click (`:56-58`), dispatches a named
action so pointer and keymap cannot diverge (`:59`), surfaces the binding in a
tooltip (`:62`), and deliberately leaves `tab_index` unset
(`crates/ui/src/components/button/button_like.rs:492,543`) while
`BufferSearchBar::cycle_field` (`crates/search/src/buffer_search.rs:1627-1648`)
excludes every button from Tab traversal. Check the vendored tree before
copying a local idiom sideways.

**An unfocusable `role=button` is defensible; an undiscoverable keystroke is
not.** WCAG 2.1.1's Understanding document states normatively that the
criterion "does not require that every visible control that can be activated
using a mouse or touchscreen must also be focusable and actionable using the
keyboard", only that a keyboard path to the same action exists — but adds that
"authors are advised to consider how users will discover any keyboard
equivalents which are available." GPUI's `.on_click()` auto-registers
`AccessibleAction::Click` (vendored gpui `_accessibility.rs:219-221`, serviced
at `window.rs:5662-5680` by synthesising a real click), so assistive tech can
already activate a control that no keyboard can focus. Spend the effort on the
tooltip, not the focus ring. If focusable controls really are wanted, the APG
answer is `role=toolbar` with roving tabindex — one tab stop, arrows within,
and only for groups of three or more — not a tab stop per button.

**A focus attribute is a claim that needs a test, not a decoration.** Any change
adding `track_focus` or `focus_visible` should leave behind one headless
assertion that the handle is *still focused after a draw*. That single
assertion catches both halves of this bug: a control nothing can focus, and a
control the shell un-focuses. Copying an accessibility idiom "verbatim" is not
evidence it works in the new context.
