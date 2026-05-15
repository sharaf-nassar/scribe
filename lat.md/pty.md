# PTY

The scribe-pty crate provides low-level terminal I/O, OSC sequence interception, and metadata extraction from PTY output streams.

## Async PTY I/O

The [[crates/scribe-pty/src/async_fd.rs#AsyncPtyFd]] wraps PTY master file descriptors for zero-copy async I/O using tokio's `AsyncFd`. No thread-per-session overhead; reads and writes are driven by epoll (Linux) or kqueue (macOS).

### Non-Blocking Setup

The fd is set to non-blocking mode via `fcntl(F_GETFL)` / `fcntl(F_SETFL, flags | O_NONBLOCK)` and registered with the current tokio reactor.

### Read Implementation

`AsyncRead` loops on `poll_read_ready`, calls `libc::read` with non-blocking semantics, and clears the ready guard on EAGAIN/EWOULDBLOCK to re-register the waker.

### Write Implementation

`AsyncWrite` follows the same pattern with `libc::write`. Flush and shutdown are no-ops; the fd is closed when dropped.

### SCM_RIGHTS Helper

The `wrap_raw_fd` function wraps a raw file descriptor received via SCM_RIGHTS into an `OwnedFd`, encapsulating the unsafe call so crates with `deny(unsafe_code)` can use it safely.

## OSC Interceptor

The [[crates/scribe-pty/src/osc_interceptor.rs#OscInterceptor]] is a VTE Perform adapter that runs in parallel with alacritty_terminal's own VTE parser.

This parallel execution is necessary because alacritty_terminal ignores custom OSC 1337 extensions (shell-integration session context and the AI-tool pre-arm sentinel).

### Intercepted Sequences

OSC 0/2 (window title), OSC 7 (current working directory), OSC 1337 (`ScribeContext` shell-integration payload and `ScribeAiLaunch` AI pre-arm sentinel only — AI tool state arrives via the hook channel; see [[server#Hook Channel]]), and BEL (0x07).

### Passed Through

All other VTE events (CSI sequences, ESC dispatch, printable characters, DCS hooks) are intentional no-ops. The interceptor only cares about metadata-bearing sequences.

## Metadata Parser

The [[crates/scribe-pty/src/metadata.rs#MetadataParser]] is a stateful parser that classifies OSC sequences into typed events.

### Metadata Events

Metadata events cover CWD, title, provider task label, AI state, prompt text, prompt marks, and BEL updates extracted from OSC sequences and control bytes.

### OSC 7 — Working Directory

Parses a `file://` URI from the OSC payload, percent-decodes the path, normalizes it by resolving `.` and `..` components without filesystem access, and emits `CwdChanged` if the result is an absolute path.

### OSC 0/2 — Window Title

Extracts the title string from the second parameter, truncated to 4096 characters. Empty titles are ignored.

### OSC 1337 — Pre-Arm Sentinel

`ScribeAiLaunch=<provider_id>` is a shell-integration sentinel that pre-arms the [[pty#ED 3 Filter]] for an AI binary the shell is about to execute.

`<provider_id>` is one of `claude_code` or `codex_code` (matched by [[crates/scribe-common/src/ai_state.rs#AiProvider#from_id]]). The shell's preexec hook emits this OSC immediately before invoking `claude` or `codex` so the PTY reader can set `ai_provider` to the correct value before the AI tool itself starts emitting bytes. Without this pre-arm, `<tool> --resume` would send its initial `\x1b[3J` before identifying via the hook channel, slipping through the filter and wiping scrollback. The parser produces an `AiProviderArmed` variant of [[crates/scribe-pty/src/metadata.rs#MetadataEvent]], which is consumed entirely inside the PTY reader and is not forwarded to the client.

This is the **only** OSC 1337 AI-related payload parsed from the PTY stream. AI tool state, prompt text, task labels, and context-window fill all arrive via the structured hook channel; see [[server#Hook Channel]].

### OSC 133 — Prompt Marks

Shell integration prompt marks with optional exit codes. These are forwarded as `PromptMark` events for scrollbar indicators.

## Synchronized Updates

VTE synchronized updates buffer terminal bytes between `CSI ? 2026 h` and `CSI ? 2026 l` so multi-step redraws appear atomically.

Scribe uses the VTE processor's built-in sync buffer in both the client and the server-side Term pipeline. The server forwards raw `CSI ? 2026 h/l` markers unchanged, and [[crates/scribe-pty/src/sync_update_filter.rs#SyncUpdateFrameSplitter]] preserves those raw commit boundaries across arbitrary PTY IPC chunking before [[crates/scribe-client/src/main.rs#App#drain_pane_output_until_frame]] stages committed bursts on a pane-local redraw queue. Light traffic can still animate one committed burst per redraw, but once a pane accumulates a larger backlog the client drains through stale bursts and presents the latest committed terminal state on the next redraw. The event loop stays in `ControlFlow::Poll` while queued bursts remain, and both sides still flush expired sync blocks on timeout so snapshots, reconnects, and stalled TUIs see the committed content. When the client-side splitter itself times out before it has emitted a committed frame, it flushes the buffered visible bytes without the leading BSU marker so the pane-local VTE sees the timed-out content once instead of starting a fresh synchronized update.

Normal session panes still receive the raw `CSI ? 2026 h/l` markers end to end. Only the client-side queueing logic uses the shared raw-frame splitter to preserve commit boundaries before those bytes reach the pane-local VTE processor.

## ED 3 Filter

The [[crates/scribe-pty/src/ed3_filter.rs#Ed3Filter]] strips CSI ED 3 (`\x1b[3J`) from AI PTY output when `preserve_ai_scrollback` is enabled (default on).

Only runs for Claude Code / Codex Code sessions when `preserve_ai_scrollback` is `true`. Regular shell sessions are never filtered. Enabled by default so AI sessions keep earlier transcript history unless the user opts back into standard terminal scrollback clearing. The filter drops ED 3 entirely rather than converting it to ED 2, because a standalone ED 3 (e.g. Codex exit cleanup) must not erase the visible screen — if the program also wants a visible clear it sends its own ED 2. The PTY reader treats prompt text, attention/error states, and inactive markers as scrollback epoch boundaries. The first suppressed clear in an epoch captures its trim baseline after replay, so a Codex ED 2 can push one copy of the prior visible transcript into scrollback; later clears in the same epoch trim back to that baseline so duplicate redraw frames do not pile up.

The PTY reader's `ai_provider` is managed by [[crates/scribe-server/src/ipc_server.rs#update_ai_provider_state]]: it is set when an AI tool announces itself via `AiStateChanged` and pre-armed via the [[pty#PTY#Metadata Parser#OSC 1337 — Pre-Arm Sentinel]] when shell integration sees the user run an AI binary, and it is cleared on `AiStateCleared` so subsequent plain-shell bytes (vim, less, etc.) bypass the filter. The pre-arm is what keeps `<tool> --resume` working across the cleared state — its initial `\x1b[3J` arrives after the shell has already armed `ai_provider` to the correct provider. Same-chunk pre-arm and ED 3 are both honored: [[crates/scribe-server/src/ipc_server.rs#chunk_mentions_ed3_provider]] inspects the chunk's OSC events for either an `AiStateChanged` or an `AiProviderArmed` carrying an ED-3-using provider before deciding whether the filter applies to the chunk.

### Scroll-Bottom on Suppression

When the filter suppresses ED 3, the PTY reader sends a [[crates/scribe-common/src/protocol.rs#ServerMessage]]`::ScrollBottom` so the client snaps the viewport to bottom.

A real ED 3 resets `display_offset` to 0 inside alacritty_terminal's `clear_history`. Stripping it without this compensating message would leave the viewport stuck at a stale scroll position while the live terminal redraws below the visible area.

### Baseline Trim on Repaint

When a later ED 3 arrives in the same scrollback epoch, the PTY reader trims the server Term back to that epoch baseline and sends [[crates/scribe-common/src/protocol.rs#ServerMessage]]`::TrimScrollback` before forwarding the redraw bytes.

The client flushes any queued PTY output for that pane, trims its own Term back to the same baseline, and then applies the new bytes. This keeps committed AI transcript history while preventing repeated inline redraws from stacking duplicate frames above it. Because [[crates/scribe-client/src/pane.rs#Pane]]`::prompt_marks` and `input_start` store positions as "lines from the very top of scrollback (0 = oldest)", the client also calls [[crates/scribe-client/src/pane.rs#shift_absolute_marks_after_trim]] with the dropped-row count so prompt jump and scrollbar markers continue to point at the right rows after each trim. Split-scroll itself no longer depends on these absolute marks — it sizes the pin from a fixed `AI_PROMPT_BLOCK_ROWS` constant and translates live cells by `cursor_line`, neither of which moves under a trim.

### State Machine

Four states track partial matches of the `\x1b[3J` sequence across `filter()` calls.

On a complete match the filter drops the sequence (emits nothing), preserving scrollback without injecting a visible screen clear. The filter sets an internal `suppressed` flag that the PTY reader consumes via `take_suppressed()`. A fast path skips allocation when no ESC byte is present. Pending bytes stay in state until the next call or `flush()`.

## Claude Picker Truncation Filter

The [[crates/scribe-pty/src/claude_picker_filter.rs#ClaudePickerTruncationFilter]] neutralises Claude Code's `AskUserQuestion` "Other" custom-text-input picker truncation 3rd-redraw so the typed prefix on the input row stays visible.

Only runs for [[crates/scribe-common/src/ai_state.rs#AiProvider]]`::ClaudeCode` sessions, gated by [[crates/scribe-server/src/ipc_server.rs#ai_provider_uses_claude_picker_filter]]. Claude Code's Ink-based picker emits a 3-stage redraw when typed text overflows the picker's 2-row input field: (1) re-emit the visible prefix on the input row, (2) overlay an `…` truncation indicator at the last column, (3) re-position to what Ink expects to be the wrap row, print `❯`, skip 3 cols, print `…`, then `\x1b[K` to erase the rest of the line. Step 3's `\x1b[K` lands on the input row instead of the wrap row in `alacritty_terminal` 0.26.0 (an off-by-one vs xterm in cursor row tracking after a print-at-last-column with DECAWM followed by `\r\n`), erasing the user's typed text. The filter detects the distinctive 18-byte signature `❯\x1b[3C\x1b[39m…\x1b[K` and replaces all 18 bytes with NULs, which `alacritty_terminal`'s parser drops. The earlier redraws' typed prefix and trailing `…` indicator on the input row then survive the redraw.

### State Machine

A single `state: u8` (0..18) tracks signature-prefix progress across PTY chunk boundaries.

On a complete match all 18 signature bytes are replaced with NULs. On mismatch any partial-match bytes are flushed unchanged before the diverging byte is reconsidered as a possible new signature start. A fast path skips allocation when the chunk contains no `\xe2` byte (signature start) and no pending state.

## LF to CRLF Filter

The [[crates/scribe-pty/src/lf_crlf_filter.rs#LfCrlfFilter]] upgrades bare `\n` (LF without a preceding `\r`) to `\r\n` in PTY output, working around an upstream `alacritty_terminal` 0.26.0 bug where its `term::Term::linefeed` does not clear `input_needs_wrap` after a print-at-last-column with DECAWM.

Always-on, no AI-provider gating. Runs as the last step in [[crates/scribe-server/src/ipc_server.rs#apply_pty_filters]] so it sees the bytes that any AI-gated filter (ED 3, Claude picker) leaves on the wire. xterm clears the deferred-wrap flag on LF, so `\n` and `\r\n` behave the same after a last-column print. `alacritty_terminal::term::linefeed` does not, so a wrap+LF pair advances the cursor by 2 visual rows instead of 1, breaking cursor-up redraws like `tools/release-me/release.sh`'s bash progress panel (`printf '\033[%dA\r'` over a panel whose border is exactly `cols` wide). `\r` (carriage_return) does clear the flag, so prepending `\r` before any bare LF restores xterm-equivalent behaviour without touching already-CRLF streams.

### State Machine

A single `prev_was_cr: bool` tracks whether the most recently emitted byte was `\r`, carried across `filter()` calls so a chunk ending in `\r` followed by a chunk starting with `\n` is correctly recognised as already-CRLF.

A fast path skips allocation when the input contains no `\n` byte; the only state update in that case is recording whether the input ended on `\r`. On a bare LF the filter pushes `\r` then `\n`; on an already-CRLF LF it pushes `\n` only. Five unit tests cover bare-LF upgrade, CRLF passthrough, mixed streams, consecutive LFs, CR-split-across-chunks, LF-split-across-chunks, and start-of-stream LF.

## Event Listener

The [[crates/scribe-pty/src/event_listener.rs#ScribeEventListener]] bridges alacritty_terminal events into a [[crates/scribe-pty/src/event_listener.rs#SessionEvent]] channel for the server. It forwards metadata, clipboard operations, terminal query callbacks, bell notifications, and PTY write-back requests.

Metadata events still carry title, CWD, AI-state, prompt-mark, and bell signals, but clipboard store/load, color requests, text-area-size requests, and `PtyWrite` now travel through the same channel so the server can answer OSC queries and clipboard reads without logging placeholders.

### Terminal Query Replies

Server-side PTY replies reuse alacritty_terminal callbacks so standard terminal probes are answered from the live session state instead of ad-hoc parsers.

`Event::PtyWrite` covers device attributes, cursor-position reports, mode reports, and kitty keyboard protocol replies emitted by alacritty_terminal. `Event::ColorRequest` resolves dynamic color queries against the configured Scribe theme when no runtime override exists, so OSC 4/10/11/12 style palette requests return the actual ANSI, foreground, background, or cursor colours instead of black fallbacks.

Three unit tests in `crates/scribe-server/src/ipc_server.rs` pin this behaviour to prevent silent regressions. `osc_10_query_emits_color_request_for_foreground_with_well_formed_response` and `osc_11_query_emits_color_request_for_background_with_well_formed_response` feed an OSC 10/11 query into a real `Term` plus alacritty `Processor`, drain the listener channel, and assert the formatter produces an `\e]N;rgb:RRRR/GGGG/BBBB\e\\` response with duplicated 16-bit hex pairs and an ST terminator. `theme_color_for_index_returns_named_slots_for_osc_10_11_12` verifies the fallback table — used when alacritty's runtime palette has no override — returns `theme.foreground` / `theme.background` / `theme.cursor` for the named OSC slots so query responses never silently degrade to opaque black.
