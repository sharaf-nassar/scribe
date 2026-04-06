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

The [[crates/scribe-pty/src/osc_interceptor.rs#OscInterceptor]] is a VTE Perform adapter that runs in parallel with alacritty_terminal's own VTE parser. This parallel execution is necessary because alacritty_terminal ignores custom OSC 1337 extensions (Claude AI state).

### Intercepted Sequences

OSC 0/2 (window title), OSC 7 (current working directory), OSC 1337 (iTerm2 / ClaudeState / CodexState), and BEL (0x07).

### Passed Through

All other VTE events (CSI sequences, ESC dispatch, printable characters, DCS hooks) are intentional no-ops. The interceptor only cares about metadata-bearing sequences.

## Metadata Parser

The [[crates/scribe-pty/src/metadata.rs#MetadataParser]] is a stateful parser that classifies OSC sequences into typed events.

### Metadata Events

Metadata events cover CWD, title, Codex task label, AI state, prompt marks, and BEL updates extracted from OSC sequences and control bytes.

### OSC 7 — Working Directory

Parses a `file://` URI from the OSC payload, percent-decodes the path, normalizes it by resolving `.` and `..` components without filesystem access, and emits `CwdChanged` if the result is an absolute path.

### OSC 0/2 — Window Title

Extracts the title string from the second parameter, truncated to 4096 characters. Empty titles are ignored.

### OSC 1337 — AI State

Two named provider formats plus a Claude-compatible legacy format are supported.

The primary formats are `ClaudeState=<state>[;key=value...]` and `CodexState=<state>[;key=value...]`, where state is one of idle_prompt, processing, waiting_for_input, permission_prompt, or error. Additional fields (tool, agent, model, context, conversation_id) arrive in subsequent semicolon-delimited parameters, each capped at 256 characters. A legacy format `AiState=state=<state>;key=val...` is also supported and is treated as Claude for compatibility. The special value `<Provider>State=inactive` emits `AiStateCleared`.

Codex hooks also use OSC 1337 for a separate task-label channel. `CodexTaskLabel=<label>` sets the short, sanitized task label shown in the tab bar, and `CodexTaskLabelCleared` clears it without disturbing the underlying shell title.

### OSC 1337 — Prompt Text

`ClaudePrompt=<text>` and `CodexPrompt=<text>` carry the user's submitted prompt, capped at 256 characters. Parsed into `MetadataEvent::PromptReceived` with provider and sanitized text.

Empty prompt payloads are silently dropped. The provider is set to `ClaudeCode` or `CodexCode` based on the prefix. Text is truncated at 256 bytes to bound IPC message size.

### OSC 133 — Prompt Marks

Shell integration prompt marks with optional exit codes. These are forwarded as `PromptMark` events for scrollbar indicators.

## Synchronized Updates

VTE synchronized updates buffer terminal bytes between `CSI ? 2026 h` and `CSI ? 2026 l` so multi-step redraws appear atomically.

Scribe uses the VTE processor's built-in sync buffer in both the client and the server-side Term pipeline. The server forwards raw `CSI ? 2026 h/l` markers unchanged, and the client feeds each PTY chunk directly into its pane-local VTE processor instead of staging committed bursts in a redraw queue. A chunk only requests redraw when some of its bytes changed visible terminal state, while both sides still flush expired sync blocks on timeout so snapshots, reconnects, and stalled TUIs see the committed content.

Normal session panes now receive the raw `CSI ? 2026 h/l` markers end to end, so in-place line edits keep the same PTY byte ordering that terminal applications emitted.

## ED 3 Filter

The [[crates/scribe-pty/src/ed3_filter.rs#Ed3Filter]] rewrites CSI ED 3 (`\x1b[3J`) to CSI ED 2 (`\x1b[2J`) for AI PTY output when `preserve_ai_scrollback` is enabled (default on).

Only runs for Claude Code / Codex Code sessions when `preserve_ai_scrollback` is `true`. Regular shell sessions are never filtered. Enabled by default so AI sessions keep earlier transcript history unless the user opts back into standard terminal scrollback clearing.

### State Machine

Four states track partial matches of the `\x1b[3J` sequence across `filter()` calls.

On a complete match the filter emits `\x1b[2J`, preserving the visible clear-screen repaint while preventing the scrollback wipe. A fast path skips allocation when no ESC byte is present. Pending bytes stay in state until the next call or `flush()`.

## Codex Hook Log Filter

The [[crates/scribe-pty/src/codex_hook_log_filter.rs#CodexHookLogFilter]] suppresses contiguous Codex hook log blocks while failing open if a block never closes.

When the setting is enabled, the filter recognizes the current documented Codex hook events (`SessionStart`, `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, and `Stop`) in both legacy `Running ... hook` / `hook (...)` blocks and the current ANSI-styled `hook: <event>` / `hook: <event> Completed` form. It buffers the full contiguous hook block until the matching trailer arrives, removes the hook boilerplate and only the first raw whitespace-only spacer line after it, and still releases buffered bytes unchanged if Codex never emits a trailer. ANSI-painted blank redraw lines are preserved so Codex prompt backgrounds and other post-hook paint operations survive filtering. Prefix matching waits for complete visible text through multibyte bullets and bounded ANSI styling prefixes, and if interactive Codex redraws a completed hook row without a trailing newline, the filter splits at the last visible hook byte so later cursor-motion and prompt repaint control sequences stay in the stream instead of disappearing with the hidden hook line. When a stripped hook prefix had already established non-default SGR colors or attributes, the kept tail replays those active styles before the remaining bytes so inherited prompt backgrounds do not drop back to the terminal default. For VTE synchronized updates, it trims hook-only rows or hook prefixes from the buffered sync block itself but keeps control-only and ANSI-painted blank rows, so prompt-background repaint tails survive even when Codex redraws the completion row and the follow-up prompt fill in one atomic commit.

## Event Listener

The [[crates/scribe-pty/src/event_listener.rs#ScribeEventListener]] bridges alacritty_terminal events into a [[crates/scribe-pty/src/event_listener.rs#SessionEvent]] channel for the server. It forwards metadata, clipboard operations, terminal query callbacks, bell notifications, and PTY write-back requests.

Metadata events still carry title, CWD, AI-state, prompt-mark, and bell signals, but clipboard store/load, color requests, text-area-size requests, and `PtyWrite` now travel through the same channel so the server can answer OSC queries and clipboard reads without logging placeholders.

### Terminal Query Replies

Server-side PTY replies reuse alacritty_terminal callbacks so standard terminal probes are answered from the live session state instead of ad-hoc parsers.

`Event::PtyWrite` covers device attributes, cursor-position reports, mode reports, and kitty keyboard protocol replies emitted by alacritty_terminal. `Event::ColorRequest` resolves dynamic color queries against the configured Scribe theme when no runtime override exists, so OSC 4/10/11/12 style palette requests return the actual ANSI, foreground, background, or cursor colours instead of black fallbacks.
