---
title: Server tests read the live user config, so a settings change turns main red
date: 2026-08-28
component: crates/scribe-server/src/ipc_server.rs, ~/.config/scribe/config.toml
tags: [tests, config, singleton-collision, pre-commit, flaky, ai-ed3, preserve-ai-scrollback]
problem_type: environment
---

## Problem

Mid-run, `cargo test --workspace` went red on `main` with no new commit:

```text
---- ipc_server::tests::suppressed_ai_ed3_forwards_filtered_output_without_scroll_bottom
assertion failed: matches!(first, ServerMessage::PtyOutput { session_id: id, data }
    if id == session_id && data.windows(4).all(|window| window != b"\x1b[3J")
    && data.ends_with(b"beforeafter"))
test result: FAILED. 371 passed; 1 failed
```

The same commit had been green forty minutes earlier and nothing in the tree
had changed. Because `.pre-commit-config.yaml:61-66` runs `cargo test
--workspace` on any staged Rust file, and `--no-verify` is refused, this
blocked every further commit in the repository.

Two plausible theories were wrong and cost real time:

- **Load-induced PTY read fragmentation.** The machine was at load ~24, so the
  first guess was that the reader returned a partial buffer. Accumulating
  `PtyOutput` frames until the payload completed did not fix it — the loop just
  timed out at 3s instead of failing instantly.
- **A newly landed change.** The failure was first reported by a worker as its
  own. It reproduces identically on a clean checkout with none of that work
  applied.

A debug dump of the frame settled it. The single frame already carried the
whole input, unfiltered, `\x1b[3J` included:

```text
DEBUG frame=[27,93,49,51,51,55,...,98,101,102,111,114,101,27,91,51,74,97,102,116,101,114]
                                        b e f o r e   ESC [  3  J  a f t e r
```

The bytes never fragmented. AI ED 3 suppression simply was not running.

## Root cause

The test builds its session through the production path, and that path reads
the developer's real config file.

`load_shared_scrollback_state` (`crates/scribe-server/src/ipc_server.rs:9169`)
seeds each session's `preserve_ai_scrollback` flag from
`load_preserve_ai_scrollback_setting` (`:11346`), which calls
`scribe_common::config::load_config()` and returns
`config.terminal.ai_session.preserve_ai_scrollback`. It falls back to `true`
only when the config cannot be read at all.

The shared test helper `live_session_with_sink` (`:16281`) constructs sessions
through that same path, so every one of its callers silently inherited whatever
`~/.config/scribe/config.toml` happened to say. The struct default is `true`
(`crates/scribe-common/src/config.rs:924`), which is why this had never been
noticed — until the file on disk said otherwise:

```toml
preserve_ai_scrollback = false
```

With the flag false, AI ED 3 suppression is correctly disabled, the raw bytes
are forwarded, and `suppressed_ai_ed3_forwards_filtered_output_without_scroll_bottom`
(`:16184`) fails. Production behaviour was right the whole time; the test was
asserting against a setting it did not control.

The config file's mtime was two minutes after the last green run. This is the
`AGENTS.md` SINGLETON COLLISION hazard — dev binaries sharing config and state
with an installed Scribe — reaching all the way into unit tests.

## Fix

Pin the ambient value in the shared helper rather than in the one failing test,
so none of its callers depend on developer configuration
(`crates/scribe-server/src/ipc_server.rs:16348`):

```rust
// Do not inherit the developer's ~/.config/scribe/config.toml.
session.preserve_ai_scrollback.store(true, std::sync::atomic::Ordering::Relaxed);
```

`true` is deliberately the same default `load_preserve_ai_scrollback_setting`
falls back to when the config is unreadable. Landed as commit `05cc12b`, bead
`scribe-boug`; verified by five consecutive passes with
`preserve_ai_scrollback = false` still set on disk.

## Rule

A unit test that constructs production objects inherits every ambient input
those objects read. Before blaming load, timing, or the last commit, check
whether the assertion depends on machine state — then reproduce on a clean
checkout, which separates "my change broke it" from "this machine changed"
in one step. Any new helper that builds a live session must pin the
config-derived state it asserts on.
