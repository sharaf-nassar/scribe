# Client

The scribe-client is a GPU-accelerated terminal frontend built with winit for windowing and wgpu for rendering.

## App State

The master application state lives in the App struct in [[crates/scribe-client/src/main.rs]]. It holds all panes, the window layout, IPC sender, input bindings, theme, AI tracker, GPU context, and UI overlay state. The event loop is driven by winit's `ApplicationHandler` trait.

### Render Loop

Each frame collects `CellInstance` arrays from visible panes and UI chrome, uploads them to the GPU instance buffer, and executes a single render pass.

Content dirty tracking avoids rebuilding instances when nothing has changed. A splash screen renders via a separate pipeline during startup.

## Panes

Each terminal session is represented by a [[crates/scribe-client/src/pane.rs#Pane]] that owns an alacritty_terminal `Term`, VTE processor, grid dimensions, scrollbar state, and cached render instances.

### PTY Output Coalescing

`PtyOutput` IPC messages are buffered per session and drained once in `about_to_wait` by [[crates/scribe-client/src/main.rs#App#drain_pending_pty_output]].

Deferring PTY handling until after all input events are processed ensures keystrokes are never blocked behind a queue of output messages. Once drained, [[crates/scribe-client/src/main.rs#App#handle_pty_output]] still preserves pane-local synchronized-update frame boundaries before the bytes reach the terminal state, so Codex and other TUIs keep their committed redraw cadence even when multiple IPC chunks were coalesced per session. A `ScreenSnapshot` discards both the session-level byte buffer and any pane-local queued frames for that session since the snapshot replaces VTE state entirely.

### Content Dirty Tracking

The `content_dirty` flag is set on PTY output or resize and cleared after instance rebuild.

Bytes buffered inside a VTE synchronized update (`CSI ? 2026 h/l`) do not mark the pane dirty until the update terminates or its timeout flushes the buffered output. [[crates/scribe-client/src/pane.rs#Pane#queue_output_frames]] uses the streaming [[crates/scribe-pty/src/sync_update_filter.rs#SyncUpdateFrameSplitter]] so synchronized-update commits stay distinct even when the terminator is split across PTY IPC messages. [[crates/scribe-client/src/main.rs#App#drain_pane_output_until_frame]] then replays one committed burst per redraw while the pane is caught up, but it drains through older queued bursts once backlog crosses the catch-up threshold so stale frames do not pile up indefinitely. [[crates/scribe-client/src/main.rs#App#flush_expired_sync_updates]] still commits expired sync blocks and marks the pane dirty when an application never sends the closing `CSI ? 2026 l`.

Visible output in the focused pane clears the active selection unless the user is actively dragging, while the shared post-output path still invalidates URL caches and shifts saved selections when scrollback grows.

The cache stores the last-built instances along with cursor blink visibility, terminal cursor hidden state (DECTCEM), focus state, selection range, and sent grid dimensions. If all match, the cached instances are reused without GPU upload. Tracking DECTCEM separately from the blink layer ensures the cache invalidates when a program toggles cursor visibility via `CSI ? 25 h/l` without other content changes.

### Synchronized Updates

Normal live sessions receive the raw synchronized-update markers from the server, and the client decides redraw pacing from pane-local committed-frame queues instead of from raw PTY delivery order alone.

[[crates/scribe-client/src/main.rs#App#handle_pty_output]] hands incoming PTY bytes to [[crates/scribe-client/src/pane.rs#Pane#queue_output_frames]], which preserves raw `CSI ? 2026 h/l` frame boundaries across message splits before enqueuing the resulting raw frames on the pane. [[crates/scribe-client/src/main.rs#App#handle_redraw]] still lets light traffic present one committed burst per frame, while [[crates/scribe-client/src/main.rs#App#about_to_wait]] switches winit to `ControlFlow::Poll` whenever queued output remains so redraws cannot stall behind a long user-event burst. The pane-local VTE processor still handles the actual synchronized-update buffering, and [[crates/scribe-client/src/main.rs#App#flush_expired_sync_updates]] now mirrors VTE's 150 ms timeout for raw frames that are still buffered ahead of the pane-local processor, stripping the opening BSU marker before replay so a timed-out block cannot re-enter sync mode and leak bytes indefinitely.

The client does not reflow blank viewport rows after render because that heuristic could move the live prompt away from the pane bottom.

### Replay Restore

Reattach delivers each session's state as a [[crates/scribe-common/src/screen_replay.rs#SessionReplay]] — the same zstd-compressed ANSI primitive the server uses for hot-reload handoff.

[[crates/scribe-client/src/main.rs#App#handle_session_replay]] decompresses the bytes and [[crates/scribe-client/src/main.rs#App#apply_replay_ansi]] feeds them through the pane's VTE processor, rebuilding the Term durably. The same helper also backs `handle_screen_snapshot` (used by `RequestSnapshot` tooling), so there is one ANSI-feed path regardless of whether the source is a live attach or a per-cell snapshot.

Most panes send their dimensions in `AttachSessions` so the server resizes each session's Term and PTY before building the replay. This eliminates the post-attach resize that would trigger SIGWINCH and corrupt restored content via shell redraw sequences.

[[crates/scribe-client/src/main.rs#App#handle_session_list]] treats Codex sessions as an exception and sends `0x0` dimensions on reconnect. A pre-replay SIGWINCH can make Codex redraw top-anchored before the replay is captured, so preserving the existing viewport restores the prompt at the bottom as expected.

Reconnect restores each pane from its actual pane-tree rect, edge padding, and final workspace tab count before `AttachSessions` is sent. That lets split panes report their real grids up front instead of restoring at full-workspace size and correcting them with a second reconnect-wide resize pass.

Codex panes still keep `last_sent_grid = None` during reconnect, but they only queue a post-restore `Resize` when the incoming replay dimensions differ from the restored pane grid. The same mismatch safeguard covers hot-restart handoff reattach: if the replay dimensions prove the live PTY was not resized yet, the client clears `last_sent_grid`, feeds the replay ANSI at its captured size, restores the local term to `pane.grid`, and lets the normal resize debounce send one corrective `Resize` later. When the replay dimensions differ from the pane grid, Codex panes additionally clear the visible area after the resize to remove content garbled by column reflow — Codex's Ink renderer uses differential updates that may not fully overwrite the stale TUI layout. Scrollback from the replay is preserved. The ANSI encoder preserves soft-wrapped rows by carrying `WRAPLINE` through [[crates/scribe-common/src/screen.rs#CellFlags]] and avoiding an extra `CRLF` between rows that already wrap into the next line. `sync_pane_grids_if_stale` enforces that `pane.term` dimensions match `pane.grid` before every render frame as a safety net.

### Padding

Padding is computed per-pane based on edge adjacency via [[crates/scribe-client/src/pane.rs#effective_padding]]. Internal edges get zero padding; external edges use configured values.

All padding values are multiplied by the display scale factor for physical-pixel rendering (see [[rendering#Glyph Atlas#DPI Scaling]]).

## Layout

The layout system has two levels: the window layout splits into workspaces, and each workspace holds tabs that each contain a pane tree.

### Pane Tree

A binary split tree defined in [[crates/scribe-client/src/layout.rs#LayoutTree]] where each node is either a `Leaf(PaneId)` or a `Split` with direction, ratio (clamped 0.1-0.9), and two children. Pane IDs are allocated from a global atomic counter.

Splitting a pane automatically equalizes all ratios in the tree so every pane gets equal space.

### Focus Navigation

Directional focus (`FocusLeft`, `FocusRight`, `FocusUp`, `FocusDown`) uses spatial overlap scoring to find the best neighbor.

For each candidate pane in the target direction, the overlap between the source pane's perpendicular axis range and the candidate's range is computed. The closest candidate with the best overlap wins.

If no direct pane or workspace neighbor exists in that direction, focus wraps to the opposite edge while keeping the same perpendicular-axis overlap rule. When nothing overlaps on that axis, focus stays put.

### Workspace Layout

Defined in [[crates/scribe-client/src/workspace_layout.rs#WindowLayout]], the window-level tree splits the viewport into workspace regions. Each `WorkspaceSlot` holds a workspace ID, tab list, active tab index, accent color, name, and project root path.

Splitting a workspace automatically equalizes all workspace ratios so every region gets equal space.

On reconnect, a reported workspace tree is authoritative for workspace topology. Only the legacy no-tree fallback applies `WorkspaceInfo.split_direction` patches, and each workspace is patched once during startup so later tab or session updates cannot rearrange the live split tree.

### Tab State

Each tab in a workspace owns a `LayoutTree` for its panes, a focused pane ID, and an optional text selection. Tabs are created, removed, and reordered within their workspace slot.

## Tab Bar

GPU-rendered tab bar in [[crates/scribe-client/src/tab_bar.rs]] generating [[crates/scribe-client/src/tab_bar.rs#TabBarColors]] from [[crates/scribe-client/src/tab_bar.rs#TabData]] using the same glyph atlas as the terminal grid.

[[crates/scribe-client/src/tab_bar.rs#TabBarColors]] is derived from `ChromeColors` and holds background, active background, text, separator, gradient-top, and accent color values. [[crates/scribe-client/src/tab_bar.rs#TabData]] carries per-tab title, active flag, and optional AI indicator color. The background is rendered as a two-tone vertical gradient (lighter top half, base bottom half) via `build_tab_bar_bg`. The active tab receives a uniform highlight color and a 2px accent indicator on its bottom edge. An AI state dot (from `TabData.ai_indicator`) is rendered in the tab when a session has an active AI state. For provider task-label sessions, the title prefers the last hook-emitted task label while that label is active, then falls back to the normal shell title. Tab titles are truncated to fit the available column width. In multi-workspace mode, named workspaces display a badge pill with a deterministic accent color; unnamed workspaces show no badge.

Tab rows wrap only after subtracting the same rendered badge and right-edge icon reservations used by the text pass. [[crates/scribe-client/src/tab_bar.rs#compute_tab_bar_height]] and active-tab range calculation share that reservation so a narrow workspace cannot allocate a blank extra row while the tabs still fit on one row.

Because tab chrome and tab glyphs are collected into the same `CellInstance` buffer and drawn in one render pass, [[crates/scribe-client/src/main.rs#build_all_instances]] must append the tab-bar background before the tab text so the labels are composited on top of their tabs.

When context-window usage reaches the warn threshold (default 70%), a colored `" NN%"` suffix is appended to the tab label. [[crates/scribe-client/src/ai_indicator.rs#AiStateTracker#tab_context_suffix]] returns the suffix text and its `srgb_to_linear_rgba` color, or `None` when the threshold is not met or the session is in a pulsing attention state (`PermissionPrompt`, `WaitingForInput`). A `fallback_color` parameter (passed as `tab_text` color by the caller) is used when the hex color string fails to parse, matching the other context displays' invalid-hex fallback behavior. `TabData.context_suffix` carries the result; `tab_display_title` reserves the suffix columns before truncation; `render_tab` emits the suffix chars in the suffix color after the title.

### tab_context_suffix_below_warn_returns_none

Verifies that [[crates/scribe-client/src/ai_indicator.rs#AiStateTracker#tab_context_suffix]] returns `None` when context=50 is below the default warn threshold of 70.

### tab_context_suffix_at_warn_returns_warn_color

Verifies that context=70 (exactly at the default warn threshold) returns the warn-band color `#d4a017`.

### tab_context_suffix_at_danger_returns_danger_color

Verifies that context=92 (above the default danger threshold of 90) returns the danger-band color `#c83030`.

### tab_context_suffix_suppressed_when_permission_prompt

Verifies that `tab_context_suffix` returns `None` for context=85 when the session state is `PermissionPrompt`, to avoid competing with the pulse indicator.

### tab_context_suffix_suppressed_when_waiting_for_input

Verifies that `tab_context_suffix` returns `None` for context=85 when the session state is `WaitingForInput`, for the same reason as `PermissionPrompt` suppression.

### tab_context_suffix_present_when_processing

Verifies that a `Processing` session with context=85 returns a suffix in the warn-band color, confirming non-pulsing states show the suffix.

### tab_context_suffix_none_when_no_session

Verifies that an unregistered `SessionId` returns `None` from `tab_context_suffix`.

### tab_context_suffix_none_when_no_context_value

Verifies that a registered session with `context=None` returns `None` from `tab_context_suffix`.

### tab_context_suffix_falls_back_on_invalid_hex

Verifies that when `warn_color` is set to an unparseable hex string, `tab_context_suffix` still returns `Some` and the color equals the provided `fallback_color` rather than `None`, matching the other context displays' invalid-hex fallback behavior.

## Workspace Notes

Workspace notes are server-backed notes scoped to a workspace badge and rendered by client GPU overlays.

[[crates/scribe-client/src/workspace_notes.rs#WorkspaceNotesStore]] is a non-durable cache of server snapshots. It is populated from `WorkspaceNotesSnapshot` and `WorkspaceNotesChanged` messages, while durable state lives in [[server#Workspaces#Workspace Notes]]. The client no longer writes `workspace_notes.toml`.

Click routing uses [[crates/scribe-client/src/tab_bar.rs#workspace_badge_hit_rect]] to turn the rendered workspace badge/name into a modal target. [[crates/scribe-client/src/main.rs#App#open_workspace_notes_modal]] focuses that workspace, closes transient overlays, and opens [[crates/scribe-client/src/workspace_notes_modal.rs#WorkspaceNotesModal]] with any saved draft. [[crates/scribe-client/src/main.rs#App#handle_workspace_notes_keyboard]] consumes modal keys before PTY translation: Enter inserts a newline, Ctrl+Enter saves, Backspace edits the text, and Escape closes or cancels edits.

Opening a modal or receiving reconnect workspace metadata triggers [[crates/scribe-client/src/main.rs#App#request_workspace_notes_snapshot]]. Snapshot drafts hydrate the modal only while its local draft is pristine; typed text is never overwritten by a late snapshot. User edits are sent through `WorkspaceNotesMutate`; the client updates visible lists only from server broadcasts so multiple windows converge on the server's last accepted mutation.

The active view supports creating notes, editing active notes, and moving done or removed notes to archive. The archive view keeps archived notes separate, supports single archived-note edits, and has an edit-all mode that sends one bulk mutation without touching active notes.

Clicking outside the notes modal closes it through the same draft-preserving path as the explicit close action. Empty modal space remains inert so controls are the only in-modal click targets.

Draft typing is debounced by [[crates/scribe-client/src/main.rs#App#flush_workspace_notes_if_due]], then flushed immediately by [[crates/scribe-client/src/main.rs#App#flush_workspace_notes_now]] on modal close. Update, restart, quit, and window-close actions defer until pending draft or modal mutations receive the server broadcast that proves durability.

The modal renderer keeps terminal-cell geometry while spacing header tabs, note list, New-note editor, bordered input with a visible caret, retryable server-error text, and footer zones with theme-derived surfaces and title-cased actions.

Hover previews are derived from active notes only and rendered by [[crates/scribe-client/src/workspace_notes_preview.rs#build_workspace_notes_preview]]. [[crates/scribe-client/src/main.rs#App#apply_workspace_notes_preview_overlay]] draws the bounded preview above terminal content but before modal overlays, while suppressing it behind the notes modal, context menu, close dialog, and update dialog.

The hover preview stays open while the pointer is over the workspace badge or preview bounds. Visible preview notes highlight on hover, and clicking one sends `ArchiveNote { reason: Done }` so lightweight note cleanup does not require opening the modal.

## Input

Keybindings are parsed from config into a `Bindings` struct in [[crates/scribe-client/src/input.rs#Bindings]] with over 50 configurable actions.

### Focus Guard

Two layers prevent stray key events from compositor overlays (e.g. GNOME Screenshot) from reaching the PTY.

#### Winit Focus

Keyboard events are only processed when the window has focus (`window_focused == true`). This catches overlays that trigger X11 `FocusOut` events.

#### X11 Active-Window Guard

[[crates/scribe-client/src/x11_focus.rs#X11FocusGuard]] polls `_NET_ACTIVE_WINDOW` via a separate `x11rb` connection to detect compositor overlays that skip X11 focus events.

Compositor overlays (e.g. GNOME Shell screenshot) clear or change this EWMH property without sending `FocusOut`. The guard polls in `about_to_wait` and on each key press. A `was_inactive` flag tracks whether the window has been obscured; when `should_suppress_key` or `poll` first sees the window become active again, a `reactivated_at` timestamp is set and keys are suppressed for 300ms from that transition. The debounce is cleared on `Focused(true)` so it only applies to compositor overlay dismissals — not normal focus transitions — preventing the first keystroke from being swallowed when the user alt-tabs or clicks to Scribe.

### Key Translation Priority

Key events are resolved through a four-level priority chain from layout shortcuts down to raw terminal byte encoding.

On macOS, bare `cmd+w` is handled before that chain and routed to the same close-request path as the native window close button, so it never falls through to pane bindings or terminal input.

1. Layout shortcuts (configurable keybindings) produce `LayoutAction` enum values
2. Special commands (command palette, settings, find)
3. Terminal shortcuts (word navigation, line navigation)
4. Generic terminal key translation produces PTY bytes with xterm modifier encoding

Pane-local terminals enable kitty keyboard tracking so app-requested disambiguated input affects encoding. [[crates/scribe-client/src/pane.rs#Pane#new]] turns tracking on, [[crates/scribe-client/src/main.rs#App#focused_keyboard_protocol]] reads the focused pane mode, and [[crates/scribe-client/src/input.rs#KeyboardProtocol]] makes modified Enter use CSI-u. Codex panes also map Alt+Enter to Codex's newline binding.

### Layout Actions

Over 50 variants in the `LayoutAction` enum covering pane, workspace, and tab management, clipboard, scrolling, zoom, and more.

Tab actions: new, Claude Code new/resume, Codex new/resume, close, next, prev, select 1-9. The legacy `new_claude_*` action names remain in config and code and map to Claude Code, while `new_codex_*` opens Codex. Those AI-tab shortcuts start the selected CLI through the user's login shell with `-lic` and `exec`, resolving the shell from `SHELL` first and then the account database so Finder-launched macOS apps still inherit the expected PATH and rc files without first rendering a normal shell prompt. Also: pane splits, pane focus/cycling, workspace splits/cycling, copy, paste, settings, find, zoom, and equalize.

### Command Palette

The command palette is a GPU-rendered action picker for common window actions, profile switching, and explicit Claude Code and Codex tab actions, opened from a dedicated keybinding and reusing the normal layout-action handlers.

[[crates/scribe-client/src/command_palette.rs#CommandPalette]] owns the query string, active state, and selected row. [[crates/scribe-client/src/main.rs#App#handle_open_command_palette]] populates entries for settings, find, tab and pane actions, new windows, every saved profile from [[crates/scribe-common/src/profiles.rs#list_profiles]], and (when available) an "Update Scribe to v{version}" entry. Selecting an entry routes through [[crates/scribe-client/src/main.rs#App#execute_automation_action]], so command-palette actions and server-forwarded automation stay on the same code path.

### Mouse Handling

Mouse events are processed for text selection, scrollbar interaction, divider drag, tab drag, prompt bar interactions, and context menus.

Selection modes are click-drag for cell, double-click for word or configured Smart Selection, triple-click for line, and quad-click for Smart Selection when configured that way. Scrollbar supports click-to-jump and drag-to-scroll. Divider drag resizes splits with 4px hit tolerance. Tab drag reorders with visual offset.

Click sequencing is tracked by [[crates/scribe-client/src/mouse_state.rs#MouseClickState]], which records each press time and position to classify the event as [[crates/scribe-client/src/mouse_state.rs#ClickKind]] (Single, Double, Triple, or Quadruple). Multi-click is recognized when a press arrives within 400 ms and 5 px of the previous one. The derived [[crates/scribe-client/src/mouse_state.rs#SelectionMode]] (Cell, Word, or Line) follows directly from the click kind. Auto-scrolling during drag is triggered by `edge_scroll_delta` when the cursor enters the 20 px edge zone at the top or bottom of the content area.

OSC 133 `click_events=1` prompt click-to-move is evaluated on mouse release through [[crates/scribe-client/src/main.rs#prompt_click_to_move_displacement]], only when the press/release left an empty selection. Dragging the live prompt row therefore keeps normal text selection, while a plain click can still send arrow-key movement.

### Drag And Drop

Dropped files and directories are pasted into the focused shell using shell-aware quoting, so GUI drag-and-drop becomes a safe path insertion workflow instead of raw bytes.

[[crates/scribe-client/src/main.rs#App#handle_dropped_path]] receives `WindowEvent::DroppedFile`, looks up the focused pane's shell basename, quotes the path for POSIX shells, Fish, PowerShell, or Nushell, and sends it through the normal paste pipeline with a trailing space. Shell basenames come from reconnect metadata and `SessionCreated`, so the quoting mode follows the actual session instead of assuming the user's login shell.

### Mouse Reporting

When a terminal application enables mouse mode (SGR 1006 or X10), mouse events are encoded as escape sequences and forwarded to the PTY. Modifier keys are encoded in the xterm Cb field (Shift +1, Alt +2, Ctrl +4).

### Resize Coordination

Window resize coalesces per event-loop tick rather than via a wall-clock debounce, and is flushed ahead of any input bytes so the server sees `Resize` → `KeyInput` in mpsc order.

Every `WindowEvent::Resized` updates the local pane grid and sets `resize_pending`. [[crates/scribe-client/src/main.rs#App#flush_resize_if_pending]] runs in `about_to_wait` (per-tick batching) and from the input call sites — [[crates/scribe-client/src/main.rs#App#handle_terminal_key]], [[crates/scribe-client/src/main.rs#App#send_paste_data]], and [[crates/scribe-client/src/main.rs#App#perform_primary_paste]] — before any `KeyInput` is queued. The shared mpsc `Sender<ClientCommand>` preserves FIFO order, so the server processes `Resize` first; `tcsetwinsize` delivers `SIGWINCH` ahead of the bytes hitting the PTY, and bash updates `COLUMNS` before reading the next command.

This mirrors alacritty/ghostty/wezterm/kitty/vte — none use a wall-clock debounce; all coalesce implicitly per tick or by last-known-size dedup.

## IPC Client

The IPC connection runs in a background thread with its own Tokio runtime, defined in [[crates/scribe-client/src/ipc_client.rs#start_ipc_thread]].

### Communication Flow

The main thread sends `ClientCommand` variants through an mpsc channel to the write task for socket serialization.

The write task serializes commands to `ClientMessage` and writes to the socket. The read task deserializes `ServerMessage` responses and dispatches them as `UiEvent` variants through the winit event loop proxy. `UiEvent::PromptReceived` carries session ID, provider, and prompt text for the prompt bar feature.

Automation requests use that same path in both directions. `scribe-cli action ...` becomes [[protocol#Client Messages#Automation]] `DispatchAction`, the server forwards it as [[protocol#Server Messages#Automation]] `RunAction`, and the client executes it through the same handlers the keyboard shortcuts and command palette already use.

### Server Lifecycle

Starts and connects to the server process, with a retry loop waiting up to 5 seconds for the socket to appear.

On Linux, the client starts the server via `systemctl --user start scribe-server`. On macOS, release builds install `com.scribe.server.plist` into `~/Library/LaunchAgents/` with the current bundle's `scribe-server` path, re-bootstrap the job if that path changes, and then `kickstart` it. If a socket already exists, the client inspects the connected server's peer PID and restarts it when the running executable path differs from the current bundle or when the installed server binary is newer than the running process start time, which lets manual DMG replacements hot-reload the background server on next launch. When that stale-server refresh fires, the client prefers a direct `scribe-server --upgrade` spawn over `launchctl kickstart -k` so the new server performs a handoff with the still-running old one; kickstart only terminates the old server when launchd still manages it, and after a DMG drop-replace that old server is typically a launchd orphan whose flock a fresh non-upgrade child would crash-loop against. `launchctl` remains the fallback if the direct spawn fails. Dev builds without a bundle fall back to spawning the server binary directly.

## Selection

Text selection in [[crates/scribe-client/src/selection.rs]] supports three modes: Cell, Word, and Line. Coordinates are absolute grid positions.

Cell selects individual characters. Word boundaries include alphanumeric, underscore, dash, dot, slash, tilde, at, plus, percent, hash, question, ampersand, and equals, and double-click word scans cross WRAPLINE-connected rows so soft-wrapped paths or commands stay contiguous. Line mode follows WRAPLINE flags for logical lines. [[crates/scribe-client/src/selection.rs#pixel_to_grid]] converts mouse pixel coordinates to grid positions, subtracting tab bar height, prompt bar height (position-aware), and content padding before dividing by cell size. During an active drag, [[crates/scribe-client/src/selection.rs#pixel_to_grid_clamped]] clamps points that stray into prompt-bar chrome or outside the pane back to the nearest visible terminal cell so the last visible row still highlights.

### Smart Selection

Smart Selection extends click selection with configurable semantic regex matching over the visible wrapped logical line.

[[crates/scribe-client/src/smart_selection.rs]] compiles the global `terminal.smart_selection` rules and maps regex byte ranges back to terminal grid cells. A candidate must contain the clicked cell. For each rule, the longest containing match is kept; the final selected candidate comes from the highest precision class with any match, then the longest match in that class. [[crates/scribe-client/src/main.rs#App#start_selection_smart]] reuses normal `SelectionRange` highlighting and copy-on-select behavior.

The default activation is quad-click, preserving double-click word and triple-click line selection. When activation is set to double-click, Smart Selection replaces ordinary double-click word selection and falls back to word selection only when no rule matches. Shift still bypasses mouse-reporting applications before local selection starts.

Right-click context menus run Smart Selection at the pointer. Matching rules with actions add explicit menu items; selection alone never executes them. Action parameters support iTerm2-style legacy substitutions (`\0`, `\1`-`\9`, `\d`, `\u`, `\h`, `\n`, and `\\`) and interpolated strings such as `\(matches[0])`, `\(path)`, `\(user)`, and `\(host)`.

### Scroll Adjustment

Selection coordinates are adjusted when PTY output or resize shifts grid content via `history_size` delta.

[[crates/scribe-client/src/main.rs#App#shift_active_selection]] shifts the active selection and drag anchors. [[crates/scribe-client/src/main.rs#App#shift_background_tab_selection]] handles saved selections on background tabs. Selections that move past `topmost_line` are cleared.

## Scrollbar

An overlay scrollbar in [[crates/scribe-client/src/scrollbar.rs#ScrollbarState]] that fades in on scroll and fades out after 1.5s of inactivity.

Width animates on hover via lerp expansion. The hit zone is 3x the visible width for easy targeting. Drag-to-scroll computes offset from mouse delta relative to track height. Fade-out duration is 0.3 seconds.

### Prompt Mark Indicators

Each entry in [[crates/scribe-client/src/pane.rs#Pane]]`::prompt_marks` renders as a 2px horizontal tick on the scrollbar track, positioned by `mark_abs / (history_size + screen_lines)`.

Marks are stored as absolute scrollback positions (lines from the very top of scrollback, 0 = oldest). When scrollback shrinks — via [[crates/scribe-common/src/protocol.rs#ServerMessage]]`::TrimScrollback` during AI redraw epochs, or natural overflow at the configured `scrollback_lines` cap — surviving rows shift down in absolute index. `handle_trim_scrollback_event` calls [[crates/scribe-client/src/pane.rs#shift_absolute_marks_after_trim]] to keep indicators aligned with their original prompt rows; the scrollbar render path additionally clamps any residual stale abs to the track bounds so a mark from a not-yet-shifted shrink path cannot draw outside the track.

## Dividers

Pane split dividers in [[crates/scribe-client/src/divider.rs]] are 1px solid quads with a 4px hit tolerance for drag resize.

Focus borders are rendered as 2px accent-colored quads on the focused pane's leading edge. Workspace focus borders render as four thin quads around the entire workspace rect.

## AI Indicator

The [[crates/scribe-client/src/ai_indicator.rs#AiStateTracker]] tracks per-session AI state with pulsing border animations.

The shared animation loop uses a generation token per spawned thread, so fast stop/start cycles from AI pulses, scrollbar fades, or stalled-sync recovery retire older timer threads instead of letting them keep emitting `AnimationTick`. The AI-pulse contribution is additionally bounded by a [[client#AI Indicator#Pulse Envelope]] so a long-lived AI state cannot keep the loop alive — and the GPU busy — indefinitely.

Priority order: PermissionPrompt > WaitingForInput > IdlePrompt > Error > Processing. Each state has configurable color, pulse frequency, tab indicator, and pane border settings. Error state decays over a timeout. Attention states (IdlePrompt, WaitingForInput, PermissionPrompt) clear on keystroke. Both `IdlePrompt` and `WaitingForInput` share the same `waiting_for_input` indicator config (color, pulse, timeout).

Tab inline context % is gated via [[crates/scribe-client/src/ai_indicator.rs#AiStateTracker#tab_context_suffix]]; see [[client#Tab Bar]] for the gating rules and rendering details.

On reconnect, active AI state is populated from `SessionInfo.ai_state` during handle_session_list so indicators appear immediately without waiting for the per-session `AiStateChanged` messages from the server's `send_stored_metadata` path. `SessionInfo.ai_provider_hint` is restored separately so clipboard cleanup and other provider-aware behavior survive reconnect even when no visible indicator should be shown. When available, `SessionInfo.ai_state.conversation_id` is also used to seed per-pane AI resume bindings so restored windows attempt targeted resume of prior provider sessions.

### Pulse Envelope

Pulse lifetime is decoupled from AI-state lifetime so a stuck or idle session cannot pin the shared 30 fps redraw loop — and the GPU — forever.

The policy gate is [[crates/scribe-client/src/ai_indicator.rs#AiStateTracker#pulse_is_active]], consulted by both `needs_animation` (whether the shared loop may retire) and `animated_color` (pulsing vs. a steady resting colour). Attention states (`IdlePrompt`/`WaitingForInput`/`PermissionPrompt`) pulse for a bounded window after entry, then rest while still tracked and visible; they still clear instantly on keystroke. `Processing` pulses only while *alive* — within an idle window of the last liveness signal. Liveness is a state edge or fresh PTY output recorded via [[crates/scribe-client/src/ai_indicator.rs#AiStateTracker#note_activity]], fed from [[crates/scribe-client/src/main.rs#App#handle_pty_output]].

A genuinely-working session keeps re-arming the envelope across hook-silent tool calls; a hung AI on a still-open PTY goes silent, the pulse rests, and the loop retires to winit `ControlFlow::Wait` at zero GPU. When output resumes for a rested session the loop is restarted from `handle_pty_output`. Envelope durations are `ATTENTION_PULSE_SECS` and `PROCESSING_IDLE_PULSE_SECS` in [[crates/scribe-client/src/ai_indicator.rs]].

#### Stale-State Clear

A rested pulse still shows its state's *colour*. A crashed or killed AI would otherwise show a stale `Processing` border forever: it can never fire its own terminal hook, and the server supervises only the shell.

[[crates/scribe-client/src/ai_indicator.rs#AiStateTracker#clear_stale_processing]] removes any `Processing` state with no liveness (hook edge or PTY output) for `STALE_PROCESSING_CLEAR`. It uses a wall-clock map (`last_activity_instant`) rather than the f32 animation clock, which freezes once the loop retires — the very case this must still catch. The client calls it lazily from [[crates/scribe-client/src/main.rs#App#about_to_wait]]: zero cost until something is stuck, and resolved before the indicator is observed (the user returning wakes the loop). Only `Processing` is cleared — attention states legitimately persist until the human acts — and `detected_providers` is preserved so provider-aware clipboard cleanup survives, mirroring reconnect.

#### Occlusion Gating

A fully hidden window shows nothing, so keeping the pulse — and the redraw loop — alive for it is pure waste.

[[crates/scribe-client/src/main.rs#App#handle_occluded_changed]] tracks winit `WindowEvent::Occluded` in `window_occluded`; `handle_animation_tick` ANDs `!window_occluded` into `ai_animating` so the loop retires while hidden and re-arms on un-occlude.

This is deliberately gated on occlusion, **not** focus: the AI pulse exists to be noticed in a background, unfocused window, so suppressing it on unfocus would defeat its purpose. winit 0.30 only reports `Occluded` on X11/macOS (Wayland/Windows never fire it), so this is a best-effort optimisation; Layer 1's envelope still bounds the loop everywhere regardless.

### processing_pulse_rests_after_idle_window

Verifies the core GPU-bug fix: a `Processing` state pulses when fresh, but after `PROCESSING_IDLE_PULSE_SECS` of no activity `needs_animation` returns false so the shared redraw loop can retire.

### processing_activity_rearms_pulse

Verifies that `note_activity` (the PTY-output liveness signal) re-arms a rested `Processing` pulse, and that it rests again after renewed silence — the genuinely-working vs. hung distinction.

### state_edge_rearms_pulse

Verifies that a repeated `Processing` state edge via `update` re-arms a rested pulse, confirming state edges are a liveness signal alongside PTY output.

### attention_pulse_rests_after_window

Verifies that an attention state (`WaitingForInput`) pulses when fresh and rests after `ATTENTION_PULSE_SECS`, measured from entry rather than from activity.

### stale_processing_is_cleared

Verifies that `clear_stale_processing` removes a `Processing` state with no liveness for `STALE_PROCESSING_CLEAR`, reports the clear, and preserves `detected_providers` so clipboard cleanup survives.

### fresh_processing_not_cleared

Verifies that `clear_stale_processing` does not remove a just-updated `Processing` state and reports no clear.

### stale_attention_state_not_cleared

Verifies that a long-idle attention state (`WaitingForInput`) is not hard-cleared, confirming the clear is scoped to `Processing` so "waiting for you" indicators persist until the human acts.

### activity_rearms_stale_processing

Verifies that `note_activity` resets the wall-clock staleness timer so a Processing state that showed a sign of life before the prune is spared.

## Desktop Notifications

Desktop notifications fire on `Processing → attention` AI state transitions. Delivery goes through a cross-platform dispatcher so [[crates/scribe-client/src/main.rs#App]] talks to one channel regardless of OS.

[[crates/scribe-client/src/notifications.rs#NotificationTracker]] stores the previous `AiState` per session and is called from `handle_ai_state_changed` before the `AiStateTracker` update. When a `Processing → attention` transition is detected (`IdlePrompt`, `WaitingForInput`, `PermissionPrompt`), a `NotificationPayload` is returned and [[crates/scribe-client/src/main.rs#App#maybe_fire_notification]] checks focus suppression based on [[crates/scribe-common/src/config.rs#NotifyCondition]]: `WhenUnfocused` suppresses when the window is focused regardless of tab, `WhenUnfocusedOrBackgroundTab` only suppresses when both the window is focused and the session is the active tab, and `Always` never suppresses for focus reasons. The notification summary includes the workspace name or project root basename and the state label (Ready, Waiting for input, Permission required). The body carries the user's last submitted prompt text from `pane.latest_prompt`.

### Cross-Platform Dispatcher

[[crates/scribe-client/src/notification_dispatcher/mod.rs#spawn_dispatcher]] is started alongside the IPC thread in `resumed` and returns an `mpsc::UnboundedSender<NotifReq>` stored on `App.notification_tx`.

The sender always exists; main.rs has no `#[cfg(target_os = …)]` gates for notifications. Platform divergence lives entirely inside the `notification_dispatcher` directory — `linux.rs` (raw `zbus`) and `macos.rs` (`notify-rust`) — and both export the same `spawn(proxy) -> UnboundedSender<NotifReq>` shape, mirroring the `winit::platform_impl` / `wgpu::hal` pattern of OS-protocol abstraction.

The dispatcher receives [[crates/scribe-client/src/notification_dispatcher/mod.rs#NotifReq]] variants: `Show` from `maybe_fire_notification`, `Close` from [[crates/scribe-client/src/main.rs#App#close_pending_notification]] on session exit and `AiStateCleared`, and `Shutdown` from [[crates/scribe-client/src/main.rs#App#shutdown_notification_dispatcher]] on the terminal exit paths. `ShowReq::new` and `NotifReq::close` hide Linux-only payload fields from non-Linux builds so macOS only carries the data its backend uses.

### Linux Backend

[[crates/scribe-client/src/notification_dispatcher/linux.rs#spawn]] runs a single long-lived dispatcher thread that owns one D-Bus session-bus connection for every notification this client ever fires.

The thread runs its own single-threaded tokio runtime, opens a `NotificationsProxy` (generated by `#[zbus::proxy]` from [[crates/scribe-client/src/notification_dispatcher/linux.rs#Notifications]]) against `org.freedesktop.Notifications`, and subscribes once to the `ActionInvoked` and `NotificationClosed` signal streams. The main loop `tokio::select!`s between the request channel and those two streams. Repeated state changes for the same session reuse `replaces_id` from a `session → notification id` map so the daemon atomically swaps an existing toast in place — no stacked toasts under `condition = "always"` and no thread or D-Bus connection accumulation under `timeout_mode = "never"`.

`ActionInvoked` looks up the toast id in the reverse map and sends `UiEvent::RunAction { FocusSession }` through the `EventLoopProxy`. `NotificationClosed` removes the entry from both maps when the daemon retires a toast. `NotifReq::Close` calls `CloseNotification(id)` to dismiss stale toasts proactively; `NotifReq::Shutdown` closes every live notification before the loop exits.

This replaces the earlier per-notification `std::thread` + `notify-rust` `wait_for_action` pattern, which leaked one OS thread and one D-Bus connection per fired notification under the `condition = "always"` + `timeout_mode = "never"` combination. `notify-rust` is dropped from the Linux dependency set; raw `zbus` handles the `Notifications` interface directly. Linux intentionally skips `request_user_attention` because on X11 the urgency hint can become a second shell-level "`<app>` is ready" notification on top of the explicit desktop notification. The tracker also suppresses Linux bell-driven urgency for two seconds after an AI notification from the same session so BEL does not immediately cover the richer D-Bus toast with the generic shell fallback.

Linux notification expiry is configurable through [[crates/scribe-common/src/config.rs#NotifyTimeoutMode]]: `system_default` maps to `expire_timeout = -1` (server default), `custom` maps to `timeout_secs * 1000`, and `never` maps to `expire_timeout = 0` (resident until dismissed). The dispatcher passes the resolved value straight through to the `Notify` D-Bus call.

### macOS Backend

[[crates/scribe-client/src/notification_dispatcher/macos.rs#spawn]] runs the same dispatcher loop shape as Linux but services each `Show` request with a synchronous `notify_rust::Notification::show()` call against `NSUserNotification`.

`Close` and `Shutdown` are no-ops on macOS because `notify-rust` exposes no programmatic dismiss path — the system retires toasts on its own timeline. Click-to-focus uses a focus-on-activate fallback: `set_last_notified` records the session ID when a notification fires, and when macOS activates the app after a click, the `Focused(true)` handler calls `take_pending_focus` to consume the pending session and dispatch `handle_focus_session`. A 30-second expiry window prevents stale notifications from switching tabs. While an update is already announced in the window title, non-update `request_user_attention` calls are suppressed so macOS does not keep resurfacing the update-ready text for unrelated AI notifications or bells. macOS ignores the timeout-mode config because `notify-rust` cannot set banner lifetime there; the Notifications settings page instead offers a shortcut to the system Notifications pane so the user can choose the persistent style for Scribe themselves.

### FocusSession Routing

The [[crates/scribe-common/src/protocol.rs#AutomationAction]] `FocusSession` variant routes through the existing automation dispatch path on both platforms.

`execute_automation_action` calls `handle_focus_session`, which looks up the session via `session_to_pane`, switches workspace and tab, and raises the OS window with `focus_window`. Notification settings are configurable in the settings window under the Notifications page.

## Prompt Bar

A per-pane bar that tracks the user's most recent AI prompts as a flat edge-to-edge strip at the top or bottom of the terminal content.

Prompt state is stored in [[crates/scribe-client/src/pane.rs#Pane]]: `first_prompt`, `latest_prompt`, `latest_prompt_at`, `latest_prompt_finished_at`, `prompt_count`, `last_conversation_id`, and `prompt_bar_dismissed`. [[crates/scribe-client/src/main.rs#App#handle_prompt_received]] increments `prompt_count`, stores prompt text, and stamps `latest_prompt_at` with `SystemTime::now()` so the elapsed-time counter has a reference point. It then triggers [[crates/scribe-client/src/main.rs#App#resize_after_prompt_bar_height_change]] when the bar height changes; that helper resizes the pane and immediately flushes the PTY resize instead of waiting for the normal resize debounce, so Codex does not repaint old-size synchronized frames into a smaller client grid while the prompt bar is appearing or disappearing. [[crates/scribe-client/src/pane.rs#Pane#prompt_bar_height]] returns 0.0 when the feature is disabled, dismissed, or no prompts have been received; otherwise it delegates to [[crates/scribe-client/src/prompt_bar.rs#prompt_bar_height]], which derives a one-row or two-row strip from the scaled prompt-bar cell height and inserts the seam only in the two-row case. [[crates/scribe-client/src/pane.rs#compute_pane_grid]] and [[crates/scribe-client/src/pane.rs#Pane#content_offset]] both accept a `prompt_bar_height` parameter so the terminal grid is sized and positioned below the bar.

`TerminalConfig` exposes `prompt_bar_font_size` (f32, 8.0–32.0, default 14.0) and `prompt_bar_position` ([[crates/scribe-common/src/config.rs#PromptBarPosition]]: Top or Bottom, default Top). The font size is independent of the terminal font: a scale factor `prompt_bar_font_size / appearance.font_size` is applied to the terminal cell dimensions to produce the prompt bar cell size. The scaled cell size is used for bar height, text truncation, hit testing, and glyph rendering (via the per-instance `size` override in `CellInstance`). When position is Bottom, `content_offset` does not include the bar height so terminal content starts directly below the tab bar; the bar rect is placed at the pane bottom edge instead.

Rendering is handled by [[crates/scribe-client/src/prompt_bar.rs#render_prompt_bar]], which accepts a [[crates/scribe-client/src/prompt_bar.rs#PromptBarColors]] struct controlling the first-row background, second-row background, text, first icon, and latest icon colors and a `glyph_size` override for custom font scaling. Colors are derived from [[crates/scribe-common/src/theme.rs#ChromeColors]] with optional per-field overrides from `AppearanceConfig` (e.g. `prompt_bar_second_row_bg`, `prompt_bar_text`). The renderer draws a flat strip that fills the pane width with no outer inset or rounded corners, uses the configurable row backgrounds for the two prompt rows, inserts a thin seam/divider between them, and shows a hover-only left-edge `×` overlay for dismissal instead of a permanent bridged capsule. The right edge carries an elapsed-time counter, a typographic `#N` count annotation (no pill), and an optional `▰▰▰ NN%` context-window indicator built by [[crates/scribe-client/src/prompt_bar.rs#format_context_label]] — a 3-segment level meter (`▰` filled, `▱` empty) whose fill count is `(percent * 3).div_ceil(100)` so any non-zero usage shows at least one filled segment. `#N`, timer, and separator glyphs are layered as text-only glyphs at descending alpha on top of row backgrounds rather than as filled chips; the context bar uses the configured Ok/Warn/Danger threshold color.

Right-cluster glyph cells (`#N`, timer, context, separators) are rendered after the row backgrounds and use [[crates/scribe-client/src/prompt_bar.rs#effective_row_bg]] to look up the row's hover/active-aware background, so cluster cells blend with the row tint instead of punching through it. [[crates/scribe-client/src/prompt_bar.rs#render_right_cluster]] picks `row1_bg` for the timer (always row 1) and `row2_bg` for count + context in two-row mode (otherwise `row1_bg`), keyed off `prompt_row_state` for `First` and `Latest` independently.

Shared geometry lives in [[crates/scribe-client/src/prompt_bar.rs#compute_prompt_bar_layout]], which defines the strip, row, and seam rects via [[crates/scribe-client/src/prompt_bar.rs#compute_row_geometry]], then composes the right-edge cluster (timer + count + context + optional middle-dot separators) via [[crates/scribe-client/src/prompt_bar.rs#one_row_cluster]] in the 1-message state and [[crates/scribe-client/src/prompt_bar.rs#two_row_cluster]] in the 2-message state. [[crates/scribe-client/src/prompt_bar.rs#PromptContextIndicator]] carries the context percent and already-resolved linear color from [[crates/scribe-client/src/main.rs#App#prompt_context_indicator_for_session]]. The timer reserves a fixed 7-cell slot (the widest output of [[crates/scribe-client/src/prompt_bar.rs#format_elapsed]]), and the context indicator reserves a fixed 8-cell slot (`▰▰▰ 100%`) after the count so digit rollovers never shift the cluster horizontally; [[crates/scribe-client/src/prompt_bar.rs#cells_to_pixels]] resolves cell counts to pixel widths through the same bounded `usize → u16 → f32` cast as `prompt_text_width`. [[crates/scribe-client/src/prompt_bar.rs#hit_test_prompt_bar]] derives the hover-only dismiss overlay rect from that same strip geometry, and [[crates/scribe-client/src/main.rs#App#prompt_bar_target_at]] resolves the active prompt-bar target from the visible pane layout once and is reused by hover, copy, dismiss, and tooltip paths, preventing hidden tabs from leaking stale `pane.rect` geometry into prompt-bar interactions.

Layout differs by message count. With one prompt the right cluster reads `<elapsed-time> · #1 · ▰▰▰ NN%` when context is available, otherwise `<elapsed-time> · #1`; the context indicator is farthest right. With two or more prompts row 1 carries the elapsed-time counter alone (right-anchored to the cluster's right edge) and row 2 carries `#N · ▰▰▰ NN%` directly under the timer's right edge — the count + context cluster is paired with the latest prompt while the timer rides the first prompt's row, so the bar's right edges align across rows. The count is visible in both states (it was previously hidden for a single prompt). [[crates/scribe-client/src/prompt_bar.rs#format_elapsed]] picks one of three formats by elapsed seconds: `"X sec"` under one minute, `"Xm YYs"` under one hour, and `"Xh YYm"` past one hour — the trailing unit is zero-padded to keep widths stable.

The counter advances live: [[crates/scribe-client/src/main.rs#App#next_prompt_timer_wake]] computes the soonest moment any visible prompt-bar's text needs to change (the next whole-second boundary while seconds are visible, the next whole-minute boundary past one hour) and [[crates/scribe-client/src/main.rs#App#next_idle_wake_deadline]] folds that into the same `ControlFlow::WaitUntil` slot used for the cursor blink, taking the soonest of the two. When neither is active the event loop falls back to winit's default `Wait`, so an idle window with no prompt bar still consumes no CPU. The timestamp uses `SystemTime` (not `Instant`) so it can be serialized into the cold-restart snapshot.

The counter freezes when the LLM stops responding. [[crates/scribe-client/src/main.rs#App#update_prompt_timer_freeze]] is invoked from [[crates/scribe-client/src/main.rs#App#handle_ai_state_changed]] on every AI state change and stamps `pane.latest_prompt_finished_at` with `SystemTime::now()` the moment AI state leaves `Processing` (transitions to `IdlePrompt`, `WaitingForInput`, `PermissionPrompt`, or `Error`). [[crates/scribe-client/src/prompt_bar.rs#pane_elapsed_text]] then uses `latest_prompt_finished_at` as the reference instant instead of `now`, so the displayed elapsed value reflects the LLM's response duration rather than wall-clock time since the prompt; `next_prompt_timer_wake` skips frozen panes since their text is static. A return to `Processing` clears the freeze and the timer resumes ticking. `handle_prompt_received`, `clear_pane_prompts`, and `maybe_reset_prompts_on_conversation_change` all clear `latest_prompt_finished_at` so a new prompt or session reset starts a fresh live timer.

Each prompt row remains an independent copy target via [[crates/scribe-client/src/main.rs#App#try_copy_prompt_bar_text]], which copies the full (untruncated) row text to the clipboard. The hover-only dismiss overlay hides the bar for that pane via [[crates/scribe-client/src/main.rs#App#try_dismiss_prompt_bar]], setting `prompt_bar_dismissed = true` and triggering a layout resize. [[crates/scribe-client/src/main.rs#App]] tracks both `prompt_bar_hover` and `prompt_bar_pressed`, so rows and the dismiss control can render restrained hover/press feedback while preserving the existing priority order where prompt-bar interactions run before the scrollbar's 3× hit zone. The bar stays hidden until a new conversation starts.

Conversation resets are detected in [[crates/scribe-client/src/main.rs#App#maybe_reset_prompts_on_conversation_change]]: when `AiStateChanged` arrives with a different `conversation_id` than `pane.last_conversation_id`, all prompt fields including `latest_prompt_at` are cleared, `prompt_bar_dismissed` is reset to `false`, and the pane is resized if the bar was visible. [[crates/scribe-client/src/main.rs#App#clear_pane_prompts]] performs the same clearing when `AiStateCleared` is received.

During hot restart reattach, `SessionList` does not carry prompt fields. The cold-restart snapshot's [[crates/scribe-client/src/restore_state.rs#LaunchRecord]] persists `first_prompt`, `latest_prompt`, `prompt_count`, `latest_prompt_at`, and `latest_prompt_finished_at` (Unix-epoch seconds via [[crates/scribe-client/src/restore_replay.rs#system_time_to_unix_seconds]]) so a frozen timer stays frozen at its original LLM-finish instant across restart, and a still-live timer keeps counting up from the original prompt time. [[crates/scribe-client/src/main.rs#App#apply_snapshot_prompt_state]] reads the saved snapshot, converts the epoch seconds back to `SystemTime`, copies prompt state (including both timestamps) to matching panes by `conversation_id`, then triggers a layout resize if the bar becomes visible.

## Split-Scroll

Pins the live terminal bottom while scrolled up in AI panes, so users can compose prompts while reading earlier output.

When `scroll_pin` is enabled (default `false`) and the user scrolls up in a pane with a detected AI provider ([[crates/scribe-client/src/ai_indicator.rs#AiStateTracker]]), the viewport splits into a top portion (scrollback at the user's offset), a 1px divider, and a bottom portion (live terminal at `display_offset=0`). State is stored as `split_scroll: Option<SplitScrollState>` on [[crates/scribe-client/src/pane.rs#Pane]]. The [[crates/scribe-client/src/split_scroll.rs#SplitScrollState]] holds the computed `pin_height`. Alternate-screen TUIs are excluded: Scribe clears `split_scroll` whenever a pane enters `ALT_SCREEN` or otherwise stops being eligible, because stitching scrollback together with a live full-screen UI reintroduces clipped prompt backgrounds, broken animation, and row-position artifacts.

The bottom portion height is fixed-size in [[crates/scribe-client/src/split_scroll.rs#compute_pin_rows]]: `AI_PROMPT_BLOCK_ROWS` (8) rows clamped to `[3, screen_lines - 3]`, sized to fit the typical AI prompt UI block (status line, permission/help hints, input box). The pin's *contents* are then translated downward by [[crates/scribe-client/src/split_scroll.rs#live_cell_y_translation]] so the cursor row lands at the last row of the screen content area, regardless of where it sits naturally in the live grid. Without translation, an AI tool that draws the prompt in the upper half of the live screen (e.g. after a fresh launch or terminal resize) would have its cells filtered out by [[crates/scribe-client/src/split_scroll.rs#filter_instances_by_y]] and disappear while scrolled.

Translation works because AI tools generally render top-down and leave the rows below the cursor empty (or fill them with idle UI like the input row's bottom border). Shifting every live cell by `(screen_lines - 1 - cursor_line) * cell_h` puts the cursor at the bottom of the pin region; rows naturally above the cursor stack upward into the pin and rows naturally below the cursor are pushed off-screen. When the cursor is already on the last live row the shift is zero, so split-scroll falls back to its original behavior. Trim handling still calls [[crates/scribe-client/src/pane.rs#shift_absolute_marks_after_trim]] with the dropped-row count after each [[crates/scribe-common/src/protocol.rs#ServerMessage]]`::TrimScrollback` so prompt-jump and scrollbar markers stay correct.

Before converting pin rows into pixels, [[crates/scribe-client/src/split_scroll.rs#align_pin_rows_to_logical_lines]] checks the live view's `WRAPLINE` flags around the cursor-anchored boundary `cursor_line - pin_rows + 1` and expands the pinned region upward when that boundary would land inside a soft-wrapped logical line. The expansion stops once the boundary reaches the wrapped line's first row or the top portion would drop below three rows.

Rendering uses a dual-render approach in `build_all_instances`: the terminal is rendered at the current `display_offset` (scrollback) and the instances are filtered to the top portion's Y range; then `display_offset` is temporarily set to 0 (live), rendered again, the live cells are translated by `live_cell_y_translation`, filtered to the bottom portion, and the offset is restored. Selection highlighting is applied to each half before filtering, using the scrollback half's saved `display_offset` and the live half's zero offset, so selections remain visible while split-scroll is active. Chrome (divider + jump button) is rendered by [[crates/scribe-client/src/split_scroll.rs#render_chrome]].

Typing while split-scrolled sends keystrokes without snapping to bottom. Pressing Enter (`\r`) snaps to bottom and clears `split_scroll`. Paste always snaps. A clickable docked jump chip appears in the bottom-right corner of the top portion, with layered chrome, a continuous arrow-to-line icon, and a brighter hover state so it reads as part of the split divider instead of a floating glyph. [[crates/scribe-client/src/split_scroll.rs#hit_test_jump_btn]] handles click detection. Scroll activation and deactivation is managed by the free functions `update_split_scroll` and `reconcile_split_scroll`, which check `display_offset`, `scroll_pin` config, AI provider detection, and alternate-screen mode.

## Status Bar

The status bar is rendered at the bottom of the window with segments for connection status, workspace info, CWD, git branch, session count, time, and system stats.

Update availability and progress also render here, centered in the empty span between the left and right segments — see [[crates/scribe-client/src/status_bar.rs#centered_start_col]] — so the CTA stays visible on narrow windows and steps down to a shorter `↑ Update` label, then disappears entirely, only when the empty span cannot hold it. Clicking the update segment opens the in-app confirmation dialog.

Connection is indicated by a green/red dot. Workspace name appears when multi-workspace. The focused pane's remote host overrides the local hostname when shell integration emits session context, and tmux session names render as a separate accent segment. Stats include CPU sparkline, memory percentage, GPU sparkline (Linux only), and network sparklines.

## System Stats

The [[crates/scribe-client/src/sys_stats.rs#SystemStatsCollector]] refreshes every 2 seconds via sysinfo. CPU and network history are kept in rolling buffers (8 and 4 entries respectively) for sparkline rendering. GPU detection on Linux reads AMD sysfs or NVIDIA sysfs/nvidia-smi.

On Linux, network throughput prefers default-route interfaces from [[crates/scribe-client/src/sys_stats.rs#linux_default_route_interfaces]] before falling back to all non-loopback interfaces. This avoids double-counting Docker bridge and veth traffic in the status bar.

## Dialogs

In-app GPU-rendered overlay dialogs for confirmations, updates, and context menus.

### Close Dialog

An in-app GPU-rendered confirmation dialog with three buttons: Quit Scribe, Kill Window, and Cancel. Both destructive actions wait for a server acknowledgment before the client exits.

When a PTY exit removes the last remaining pane in a window, the client reuses that same permanent-close flow instead of leaving an empty workspace shell on screen.

### Update Dialog

Shows update-install and restart-required confirmations in a shared overlay, opened from the command palette or the centered status-bar CTA.

The update notification appears in the compositor window title rather than in the tab bar. Stable windows use `Scribe`, while `scribe-dev` windows use `devScribe`, yielding titles such as `devScribe - v{version} available - click below to update` when the centered bottom status-bar CTA is clickable and `devScribe - v{version} available` otherwise. If installation finishes with `CompletedRestartRequired`, the same overlay switches to a `Continue` / `Cancel` cold-restart prompt and the centered status-bar label stays clickable as `Updated! Restart required` so canceling does not strand the user.
Approving that deferred restart spawns a detached helper mode of the client binary. The helper performs the platform-specific cold restart, waits for the old client windows to disconnect and flush restore snapshots, then launches one fresh client so normal cold-restore fan-out recreates the remaining windows.

### Context Menu

Right-click overlay with Copy (if selection active), Paste, Select All, Open URL (if hovering a URL), and Open File (if hovering a path). Items are rendered as GPU quads with hover highlight.

## URL Detection

The [[crates/scribe-client/src/url_detect.rs#PaneUrlCache]] scans visible terminal content for URLs (https, http, ftp, file, mailto, ssh, and telnet schemes) and file-system paths.

Soft-wrapped rows are joined by `WRAPLINE` before scanning so a link split across terminal rows remains one clickable span. Trailing punctuation is stripped respecting bracket pairs. Detected spans are cached and invalidated on content change. Each span carries a `SpanKind` (`Url` or `Path`).

URL highlighting and the pointer cursor are only shown while the Ctrl modifier is held. The `ModifiersChanged` handler triggers a redraw and cursor update so visual feedback is immediate. Only the clickable span under the cursor is underlined; wrapped spans draw one underline segment per row. Ctrl+click opens the span via `xdg-open` on Linux or `open` on macOS. File paths support an optional `:N` line-number suffix; when present, `code --goto path:N` is tried first and `xdg-open` is the fallback. Relative paths are resolved against the pane's OSC 7 CWD, and `~/` is expanded using `$HOME`.

## Clipboard Cleanup

When copying from a supported AI coding session, [[crates/scribe-client/src/clipboard_cleanup.rs#prepare_copy_text]] applies dedent, blockquote normalization, decorative-prefix stripping, then unwrap.

Copy actions decide whether cleanup is active through [[crates/scribe-client/src/main.rs#ai_provider_for_pane]], which accepts either tracker-detected AI state or an AI launch binding on the pane. This keeps cleanup enabled for newly opened Claude Code and Codex tabs before their first hook event arrives.

Dedent strips minimum shared leading whitespace. Blockquote normalization removes markdown `>` markers and the rendered `▎` gutter used by some AI UIs so quoted prose copies as plain text. Decorative-prefix stripping removes leading AI status glyphs such as `●` when followed by whitespace. Unwrap then joins hard-wrapped prose at auto-detected wrap width. When no dominant width is detected but at least one line exceeds 40 characters, [[crates/scribe-client/src/clipboard_cleanup.rs#join_non_break_runs]] joins consecutive non-break lines as a fallback. Structural breaks like bullets, headings, code blocks, and tables are preserved after quote markers and decorative prefixes are removed.

## Window State

Per-window geometry is persisted under the active install flavor's XDG state root via [[crates/scribe-client/src/window_state.rs#WindowRegistry]].

Stable installs use `$XDG_STATE_HOME/scribe/windows/{window_id}.toml`, while `scribe-dev` uses `$XDG_STATE_HOME/scribe-dev/windows/{window_id}.toml`. `Kill Window` and a natural exit of the last remaining terminal both remove the file only after the server confirms the window was destroyed.

Additional windows are separate `scribe-client --window-id` processes spawned by [[crates/scribe-client/src/main.rs#spawn_client_process]]. The parent keeps a lightweight wait thread via [[crates/scribe-client/src/main.rs#reap_spawned_client_child]] so closed child windows do not remain as zombies. Startup timing logs from [[crates/scribe-client/src/main.rs#AppStartup#load]], [[crates/scribe-client/src/main.rs#App#init_gpu_and_terminal]], and session-list handling expose whether delays come from config, window/GPU setup, renderer/font atlas setup, IPC, splash gating, or session creation.

All geometry (position and size) is stored and restored in **logical coordinates** so windows scale correctly on HiDPI/Retina displays. `capture_window_geometry` converts physical pixels to logical using `window.scale_factor()`, and `apply_window_geometry` restores via `LogicalSize`/`LogicalPosition`. Position is stored as Optional since Wayland does not expose window positions. Size is always restored via `request_inner_size` — even for maximized windows — so the GPU surface and pane grids have reasonable pre-configure dimensions on Wayland where `inner_size()` can return a tiny default before the compositor responds. The window is created with an initial 1200×800 logical-pixel hint for the same reason. Maximized state is set after size, and restart-time restore treats size-only or monitor-only records as persisted geometry instead of requiring X11 coordinates.

`apply_window_geometry` returns whether the saved geometry was within the safe range and was actually applied; callers that need to reason about the eventual viewport (cold-restart replay) read the applied geom rather than `window.inner_size()` because both `request_inner_size` and `set_maximized(true)` are async on most compositors and may not yet be reflected when the next synchronous step runs. [[crates/scribe-client/src/window_state.rs#expected_physical_size]] converts a saved `WindowGeometry` plus the current `scale_factor` into the physical inner size the window will settle on, so PTY grids and `CreateSession` sizes match the eventual rendered viewport instead of the pre-restore startup hint.

### Cold Restart Restore Store

The [[crates/scribe-client/src/restore_state.rs#RestoreStore]] persists logical window state for cold restart recovery under `$XDG_STATE_HOME/{flavor}/restore/`.

A debounced save runs after every layout change via `report_workspace_tree`, snapshotting workspace splits, tabs, pane trees, and per-pane launch bindings. Restore directories are hardened to `0700`, and snapshot, index, lock, and temporary files are written as `0600` because launch bindings can include prompt text and provider conversation IDs. The client writes the per-window snapshot file before adding that window ID to the shared restore index, so a failed snapshot write cannot leave a dangling index entry. Empty snapshots with no replayable tabs or launches are not persisted; if an empty server starts with only those stale entries, startup falls back to a fresh session instead of replaying a blank window forever. On startup with an empty `SessionList`, the bootstrap client atomically claims the first replayable entry from the restore index and rebuilds the layout via [[crates/scribe-client/src/restore_replay.rs#prepare_replay]], then creates sessions for each saved pane. Before replay, the client reapplies geometry from the claimed snapshot's original window ID because a true cold restart connects to a fresh server that has already assigned a new window ID in `Welcome`. The geometry that was actually applied is also threaded into [[crates/scribe-client/src/main.rs#App#replay_cold_restart]] so the replay sizes pane grids and the initial `CreateSession` from [[crates/scribe-client/src/window_state.rs#expected_physical_size]] rather than `window.inner_size()`; without this, maximized windows created PTYs at the 1200×800 startup hint and stayed undersized for the lifetime of the session because the corrective resize from the eventual `WindowEvent::Resized` is dispatched while panes still hold placeholder session IDs that the server cannot match. If more saved windows remain, it spawns fresh `--restore-child` client processes; each child claims exactly one additional entry and never fans out again. The claim path scans the remaining index entries for readable per-window files and drops stale IDs before deciding how many child windows to launch, so partially missing restore files cannot fan out duplicate blank windows. Explicit close or quit clears the snapshot and sets `quit_restore_cleared` so the subsequent server-disconnect event does not re-save it; server crash preserves it. Restore is skipped when the client was launched with `--window-id` (i.e. spawned as a new window by an existing client) to prevent claiming a live window's snapshot.

AI panes persist `conversation_id` via hook events that include provider conversation IDs from hook JSON payloads. [[crates/scribe-client/src/main.rs#App#update_ai_launch_binding]] preserves an existing non-None `conversation_id` when subsequent state updates omit it, ensuring hooks without conversation access do not erase the tracking ID. When the tool later emits `AiStateCleared`, the pane's launch binding is demoted back to `shell` before the next snapshot so a normal shell tab that temporarily ran an AI CLI does not cold-restart back into `--resume`. On replay, panes with a `conversation_id` launch the provider's targeted resume command (`claude --resume <id>` or `codex resume <id>`); those without fall back to the generic resume picker. Prompt bar state (`first_prompt`, `latest_prompt`, `prompt_count`) is persisted in [[crates/scribe-client/src/restore_state.rs#LaunchRecord]] and restored during replay so the bar appears immediately after a cold restart. The `last_conversation_id` is also seeded from the launch record's `conversation_id` to ensure conversation-change detection works correctly from the first `AiStateChanged` event.

## Config Watching

A file watcher in [[crates/scribe-client/src/config.rs#start_config_watcher]] monitors the active install flavor's config root.

Stable installs watch `$XDG_CONFIG_HOME/scribe/` on Linux and `~/Library/Application Support/Scribe/` on macOS; `scribe-dev` uses the corresponding flavor-specific directory. The watcher forwards `ConfigChanged` through the event loop proxy for `config.toml`, theme changes, and on macOS the watched root directory itself, because the `notify` FSEvents backend may report only the directory that must be rescanned after a save. On reload the client reapplies the renderer theme when the preset name changes, when the inline `[theme]` values change under `custom`, and while an external theme file is selected so file edits repaint immediately.

## Search Overlay

Find-in-scrollback overlay state in [[crates/scribe-client/src/search_overlay.rs#SearchOverlay]], tracking query text, match results, and highlighted match index.

State module plus GPU-rendered overlay. Methods: `open` (clears previous query and results), `close` (resets all state), `push_char`/`pop_char` (edit the query string), `set_results` (replace match list and reset highlight), `next_match`/`prev_match` (cycle through results with wrap-around), `matches` (borrow all results). Match results are `Vec<SearchMatch>` received from the server. All visible matches on the focused pane are highlighted: the current match uses the full accent background with a contrast foreground, while other matches blend the accent into their existing cell background at 40% intensity.

## Tooltip

GPU-rendered tooltip overlay in [[crates/scribe-client/src/tooltip.rs]] that renders a small dark box with light text above or below an anchor rect.

[[crates/scribe-client/src/tooltip.rs#TooltipAnchor]] holds the tooltip text and the anchor `Rect`. [[crates/scribe-client/src/tooltip.rs#TooltipPosition]] selects `Above` or `Below` placement. [[crates/scribe-client/src/tooltip.rs#render_tooltip]] emits `CellInstance` quads into the caller's buffer: a 1 px border quad, a background quad, then per-character glyph quads. The tooltip is horizontally centered on the anchor and clamped to stay within `viewport_width`. A 1-character left/right padding is included on each side of the text.
