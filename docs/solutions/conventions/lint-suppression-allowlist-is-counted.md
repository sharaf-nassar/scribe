---
title: The lint-suppression allowlist counts attributes, and the line ceiling is 80
date: 2026-08-19
component: tools/check-no-new-lint-suppressions.sh, crates/scribe-client/src/main.rs
tags: [clippy, pre-commit, too_many_lines, lint-suppressions, gpui, render]
problem_type: convention
---

## Problem

A three-line addition to `render_grid` — one `on_mouse_exit` listener — failed
the commit:

```text
no new lint suppressions...........................Failed
Unexpected suppressions:
  crates/scribe-client/src/main.rs:12953 #[allow( clippy::too_many_lines, ... )]
  crates/scribe-client/src/main.rs:2238  #[allow(clippy::too_many_lines, ...)]
  crates/scribe-client/src/main.rs:6776  #[allow(clippy::too_many_lines, ...)]
  crates/scribe-client/src/main.rs:9317  #[allow(clippy::too_many_lines, ...)]
```

The hook lists four locations, three of which are long-standing and untouched.
That framing invites the wrong conclusion — that the change disturbed existing
suppressions, or that the allowlist is line-anchored and every insertion into
`main.rs` shifts it. Neither is true.

## Root cause

Two repo-specific numbers, both easy to guess wrong.

`tools/lint-suppressions-allowlist.txt` is keyed `<file>|<normalized attribute>`
with no line numbers, and each line is one permitted occurrence. It grants
`crates/scribe-client/src/main.rs` exactly three `#[allow(clippy::too_many_lines)]`.
The hook compares multisets, so adding a fourth makes the counts disagree and
it prints all four as "unexpected". Only the new one is actually new; the other
three are collateral in the message.

The threshold itself is 80 lines, set in `clippy.toml`, not clippy's default
100. `render_grid` sat at 79 and the new listener took it to 81:

```text
error: this function has too many lines (81/80)
```

Counting the function's source lines is misleading here — it is 106 lines long
and 88 non-blank non-comment, both well past 80. Clippy counts differently, so
"it looks under the limit" is not evidence. Ask clippy.

## Fix

Delete the suppression and cut a line instead. Hoisting the inline closure into
a named method removes two lines from the caller and reads better:

```rust
.on_mouse_exit(cx.listener(Self::forget_pointer_position))
```

with the handler beside `move_over_grid` as an ordinary method taking
`(&mut self, &MouseExitEvent, &mut Window, &mut Context<Self>)`. `cx.listener`
accepts a method reference wherever it accepts a closure. That landed as
`029c7a7`, folded into `b65b5c4`.

To find the real number, remove the `#[allow]` and ask clippy for one crate —
about 35 seconds warm, far quicker than reasoning about it:

```bash
cargo clippy -p scribe-client --all-targets --all-features -- -D warnings
```

## Prevention

Editing a long gpui `render_*` builder is the common way to trip this, because
those functions sit close to the ceiling by design and every added listener is
2-4 lines. Budget for it: if a render function needs a new handler, write the
handler as a named method from the start rather than inline.

Widening `tools/lint-suppressions-allowlist.txt` is a deliberate policy change,
not a way to get a commit through. The hook says so, and the three existing
entries in `main.rs` are the sanctioned exceptions, not a precedent to extend.
