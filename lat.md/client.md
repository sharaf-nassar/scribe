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

Deferring VTE processing until after all input events are handled ensures keystrokes are never blocked behind a queue of output messages. This matters most when TUI applications like Claude Code produce large screen redraws that arrive as multiple IPC chunks. A `ScreenSnapshot` discards any buffered bytes for that session since the snapshot replaces VTE state entirely.

### Content Dirty Tracking

The `content_dirty` flag is set on PTY output or resize and cleared after instance rebuild.

Bytes buffered inside a VTE synchronized update (`CSI ? 2026 h/l`) do not mark the pane dirty until the update terminates or its timeout flushes the buffered output. [[crates/scribe-client/src/main.rs#App#handle_pty_output]] feeds each coalesced PTY chunk into [[crates/scribe-client/src/pane.rs#Pane#feed_output]], and the pane uses the VTE processor's `sync_bytes_count()` to avoid redraw requests while the current chunk is still fully buffered inside an open synchronized update. [[crates/scribe-client/src/main.rs#App#flush_expired_sync_updates]] commits expired sync blocks and marks the pane dirty when an application never sends the closing `CSI ? 2026 l`.

Visible output in the focused pane clears the active selection unless the user is actively dragging, while the shared post-output path still invalidates URL caches and shifts saved selections when scrollback grows.

The cache stores the last-built instances along with cursor visibility, focus state, selection range, and sent grid dimensions. If all match, the cached instances are reused without GPU upload.

### Synchronized Updates

Normal live sessions receive the raw synchronized-update markers from the server, and the client feeds them directly into the pane-local VTE processor in PTY delivery order.

[[crates/scribe-client/src/main.rs#App#handle_pty_output]] requests a redraw only when [[crates/scribe-client/src/pane.rs#Pane#feed_output]] reports that the processed bytes changed visible terminal state. Bytes that remain buffered inside an open synchronized update do not trigger intermediate redraws, and [[crates/scribe-client/src/main.rs#App#flush_expired_sync_updates]] still flushes stalled sync blocks on timeout so committed content cannot stay hidden forever. This keeps Claude Code, Codex, and other TUIs on the normal byte stream instead of replaying PTY output through a client-side frame queue.

### Snapshot Restore

Most panes send their dimensions in `AttachSessions` so the server resizes each session's Term and PTY before snapshotting. This eliminates the post-attach resize that would trigger SIGWINCH and corrupt restored content via shell redraw sequences.

[[crates/scribe-client/src/main.rs#App#handle_session_list]] treats Codex sessions as an exception and sends `0x0` dimensions on reconnect. A pre-snapshot SIGWINCH can make Codex redraw top-anchored before the snapshot is captured, so preserving the existing viewport restores the prompt at the bottom as expected.

Reconnect restores each pane from its actual pane-tree rect, edge padding, and final workspace tab count before `AttachSessions` is sent. That lets split panes report their real grids up front instead of restoring at full-workspace size and correcting them with a second reconnect-wide resize pass.

Codex panes still keep `last_sent_grid = None` during reconnect, but they only queue a post-restore `Resize` when the incoming snapshot dimensions differ from the restored pane grid. If dimensions still differ, snapshot ANSI is fed at the snapshot's dimensions, then the term is always resized back to `pane.grid`. Snapshot replay preserves soft-wrapped rows by carrying `WRAPLINE` through [[crates/scribe-common/src/screen.rs#CellFlags]] and avoiding an extra `CRLF` between rows that already wrap into the next line. `sync_pane_grids_if_stale` enforces that `pane.term` dimensions match `pane.grid` before every render frame as a safety net.

### Padding

Padding is computed per-pane based on edge adjacency. Internal edges (adjacent to sibling panes) have zero padding; external edges (bordering the viewport) use the configured content padding values.

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

Defined in [[crates/scribe-client/src/workspace_layout.rs#WindowLayout]], the window-level tree splits the viewport into workspace regions. Each `WorkspaceSlot` holds a workspace ID, tab list, active tab index, accent color, and name.

Splitting a workspace automatically equalizes all workspace ratios so every region gets equal space.

On reconnect, a reported workspace tree is authoritative for workspace topology. Only the legacy no-tree fallback applies `WorkspaceInfo.split_direction` patches, and each workspace is patched once during startup so later tab or session updates cannot rearrange the live split tree.

### Tab State

Each tab in a workspace owns a `LayoutTree` for its panes, a focused pane ID, and an optional text selection. Tabs are created, removed, and reordered within their workspace slot.

## Tab Bar

GPU-rendered tab bar in [[crates/scribe-client/src/tab_bar.rs]] generating [[crates/scribe-client/src/tab_bar.rs#TabBarColors]] from [[crates/scribe-client/src/tab_bar.rs#TabData]] using the same glyph atlas as the terminal grid.

[[crates/scribe-client/src/tab_bar.rs#TabBarColors]] is derived from `ChromeColors` and holds background, active background, text, separator, gradient-top, and accent color values. [[crates/scribe-client/src/tab_bar.rs#TabData]] carries per-tab title, active flag, and optional AI indicator color. The background is rendered as a two-tone vertical gradient (lighter top half, base bottom half) via `build_tab_bar_bg`. The active tab receives a uniform highlight color and a 2px accent indicator on its bottom edge. An AI state dot (from `TabData.ai_indicator`) is rendered in the tab when a session has an active AI state. For Codex sessions, the title prefers the last hook-emitted task label while that label is active, then falls back to the normal shell title. Tab titles are truncated to fit the available column width.

## Input

Keybindings are parsed from config into a `Bindings` struct in [[crates/scribe-client/src/input.rs#Bindings]] with over 50 configurable actions.

### Focus Guard

Two layers prevent stray key events from compositor overlays (e.g. GNOME Screenshot) from reaching the PTY.

#### Winit Focus

Keyboard events are only processed when the window has focus (`window_focused == true`). This catches overlays that trigger X11 `FocusOut` events.

#### X11 Active-Window Guard

[[crates/scribe-client/src/x11_focus.rs#X11FocusGuard]] polls `_NET_ACTIVE_WINDOW` via a separate `x11rb` connection to detect compositor overlays that skip X11 focus events.

Compositor overlays (e.g. GNOME Shell screenshot) clear or change this EWMH property without sending `FocusOut`. The guard polls in `about_to_wait` and on each key press. A 300ms debounce after re-activation catches stray keystrokes that arrive just after the overlay closes.

### Key Translation Priority

Key events are resolved through a four-level priority chain from layout shortcuts down to raw terminal byte encoding.

On macOS, bare `cmd+w` is handled before that chain and routed to the same close-request path as the native window close button, so it never falls through to pane bindings or terminal input.

1. Layout shortcuts (configurable keybindings) produce `LayoutAction` enum values
2. Special commands (command palette, settings, find)
3. Terminal shortcuts (word navigation, line navigation)
4. Generic terminal key translation produces PTY bytes with xterm modifier encoding

### Layout Actions

Over 50 variants in the `LayoutAction` enum covering pane, workspace, and tab management, clipboard, scrolling, zoom, and more.

Tab actions: new, new/resume the selected AI CLI, close, next, prev, select 1-9. The legacy `new_claude_*` action names remain in config and code, but at runtime they launch Claude Code or Codex based on the keybindings-page provider toggle. Those AI-tab shortcuts start the selected CLI through the user's login shell with `-lic` and `exec`, resolving the shell from `SHELL` first and then the account database so Finder-launched macOS apps still inherit the expected PATH and rc files without first rendering a normal shell prompt. Also: pane splits, pane focus/cycling, workspace splits/cycling, copy, paste, settings, find, zoom, and equalize.

### Command Palette

The command palette is a GPU-rendered action picker for common window actions and profile switching, opened from a dedicated keybinding and reusing the normal layout-action handlers.

[[crates/scribe-client/src/command_palette.rs#CommandPalette]] owns the query string, active state, and selected row. [[crates/scribe-client/src/main.rs#App#handle_open_command_palette]] populates entries for settings, find, tab and pane actions, new windows, every saved profile from [[crates/scribe-common/src/profiles.rs#list_profiles]], and (when available) an "Update Scribe to v{version}" entry. Selecting an entry routes through [[crates/scribe-client/src/main.rs#App#execute_automation_action]], so command-palette actions and server-forwarded automation stay on the same code path.

### Mouse Handling

Mouse events are processed for text selection, scrollbar interaction, divider drag, tab drag, and context menus.

Selection modes are click-drag for cell, double-click for word, triple-click for line. Scrollbar supports click-to-jump and drag-to-scroll. Divider drag resizes splits with 4px hit tolerance. Tab drag reorders with visual offset.

Click sequencing is tracked by [[crates/scribe-client/src/mouse_state.rs#MouseClickState]], which records each press time and position to classify the event as [[crates/scribe-client/src/mouse_state.rs#ClickKind]] (Single, Double, or Triple). Multi-click is recognized when a press arrives within 400 ms and 5 px of the previous one. The derived [[crates/scribe-client/src/mouse_state.rs#SelectionMode]] (Cell, Word, or Line) follows directly from the click kind. Auto-scrolling during drag is triggered by `edge_scroll_delta` when the cursor enters the 20 px edge zone at the top or bottom of the content area.

### Drag And Drop

Dropped files and directories are pasted into the focused shell using shell-aware quoting, so GUI drag-and-drop becomes a safe path insertion workflow instead of raw bytes.

[[crates/scribe-client/src/main.rs#App#handle_dropped_path]] receives `WindowEvent::DroppedFile`, looks up the focused pane's shell basename, quotes the path for POSIX shells, Fish, PowerShell, or Nushell, and sends it through the normal paste pipeline with a trailing space. Shell basenames come from reconnect metadata and `SessionCreated`, so the quoting mode follows the actual session instead of assuming the user's login shell.

### Mouse Reporting

When a terminal application enables mouse mode (SGR 1006 or X10), mouse events are encoded as escape sequences and forwarded to the PTY. Modifier keys are encoded in the xterm Cb field (Shift +1, Alt +2, Ctrl +4).

## IPC Client

The IPC connection runs in a background thread with its own Tokio runtime, defined in [[crates/scribe-client/src/ipc_client.rs#start_ipc_thread]].

### Communication Flow

The main thread sends `ClientCommand` variants through an mpsc channel to the write task for socket serialization.

The write task serializes commands to `ClientMessage` and writes to the socket. The read task deserializes `ServerMessage` responses and dispatches them as `UiEvent` variants through the winit event loop proxy. `UiEvent::PromptReceived` carries session ID, provider, and prompt text for the prompt bar feature.

Automation requests use that same path in both directions. `scribe-cli action ...` becomes [[protocol#Client Messages#Automation]] `DispatchAction`, the server forwards it as [[protocol#Server Messages#Automation]] `RunAction`, and the client executes it through the same handlers the keyboard shortcuts and command palette already use.

### Server Lifecycle

Starts and connects to the server process, with a retry loop waiting up to 5 seconds for the socket to appear.

On Linux, the client starts the server via `systemctl --user start scribe-server`. On macOS, release builds install `com.scribe.server.plist` into `~/Library/LaunchAgents/` with the current bundle's `scribe-server` path, re-bootstrap the job if that path changes, and then `kickstart` it. If a socket already exists, the client inspects the connected server's peer PID and restarts it when the running executable path differs from the current bundle or when the installed server binary is newer than the running process start time, which lets manual DMG replacements hot-reload the background server on next launch. Dev builds without a bundle fall back to spawning the server binary directly.

## Selection

Text selection in [[crates/scribe-client/src/selection.rs]] supports three modes: Cell, Word, and Line. Coordinates are absolute grid positions.

Cell selects individual characters. Word boundaries include alphanumeric, underscore, dash, dot, slash, tilde, at, plus, percent, hash, question, ampersand, and equals. Line mode follows WRAPLINE flags for logical lines.

### Scroll Adjustment

Selection coordinates are adjusted when PTY output or resize shifts grid content via `history_size` delta.

[[crates/scribe-client/src/main.rs#App#shift_active_selection]] shifts the active selection and drag anchors. [[crates/scribe-client/src/main.rs#App#shift_background_tab_selection]] handles saved selections on background tabs. Selections that move past `topmost_line` are cleared.

## Scrollbar

An overlay scrollbar in [[crates/scribe-client/src/scrollbar.rs#ScrollbarState]] that fades in on scroll and fades out after 1.5s of inactivity.

Width animates on hover via lerp expansion. The hit zone is 3x the visible width for easy targeting. Drag-to-scroll computes offset from mouse delta relative to track height. Fade-out duration is 0.3 seconds.

## Dividers

Pane split dividers in [[crates/scribe-client/src/divider.rs]] are 1px solid quads with a 4px hit tolerance for drag resize.

Focus borders are rendered as 2px accent-colored quads on the focused pane's leading edge. Workspace focus borders render as four thin quads around the entire workspace rect.

## AI Indicator

The [[crates/scribe-client/src/ai_indicator.rs#AiStateTracker]] tracks per-session AI state with pulsing border animations.

Priority order: PermissionPrompt > WaitingForInput > IdlePrompt > Error > Processing. Each state has configurable color, pulse frequency, tab indicator, and pane border settings. Error state decays over a timeout. Attention states (IdlePrompt, WaitingForInput, PermissionPrompt) clear on keystroke. Both `IdlePrompt` and `WaitingForInput` share the same `waiting_for_input` indicator config (color, pulse, timeout).

On reconnect, active AI state is populated from `SessionInfo.ai_state` during handle_session_list so indicators appear immediately without waiting for the per-session `AiStateChanged` messages from the server's `send_stored_metadata` path. `SessionInfo.ai_provider_hint` is restored separately so clipboard cleanup and other provider-aware behavior survive reconnect even when no visible indicator should be shown. When available, `SessionInfo.ai_state.conversation_id` is also used to seed per-pane AI resume bindings so restored windows attempt targeted resume of prior provider sessions.

## Prompt Bar

A per-pane bar that tracks the user's most recent AI prompts, rendered between the tab bar and terminal content.

Prompt state is stored in [[crates/scribe-client/src/pane.rs#Pane]]: `first_prompt`, `latest_prompt`, `prompt_count`, and `last_conversation_id`. [[crates/scribe-client/src/main.rs#App#handle_prompt_received]] increments `prompt_count` and stores prompt text, triggering `resize_after_layout_change` when the bar height changes (0→1 line, 1→2+ lines). [[crates/scribe-client/src/pane.rs#Pane#prompt_bar_height]] returns 0.0 when the feature is disabled or no prompts have been received; otherwise it is `lines * cell_height + 14.0` (8px top pad + 6px bottom pad). [[crates/scribe-client/src/pane.rs#compute_pane_grid]] and [[crates/scribe-client/src/pane.rs#Pane#content_offset]] both accept a `prompt_bar_height` parameter so the terminal grid is sized and positioned below the bar.

Rendering is handled by [[crates/scribe-client/src/prompt_bar.rs#render_prompt_bar]], which emits `CellInstance` quads: a background rect (`#151528`), icon glyphs (`⊙` for first, `→` for latest), and truncated prompt text. Text is clipped with an ellipsis (`…`) when it overflows the available width. [[crates/scribe-client/src/prompt_bar.rs#hit_test_prompt_bar]] maps mouse coordinates to a [[crates/scribe-client/src/prompt_bar.rs#PromptBarHover]] variant for hover highlighting. [[crates/scribe-client/src/prompt_bar.rs#hovered_prompt_text]] returns the full text of the hovered line for tooltip display. [[crates/scribe-client/src/prompt_bar.rs#is_prompt_truncated]] checks whether a given text would overflow, used to decide whether to show a tooltip.

Conversation resets are detected in [[crates/scribe-client/src/main.rs#App#maybe_reset_prompts_on_conversation_change]]: when `AiStateChanged` arrives with a different `conversation_id` than `pane.last_conversation_id`, all prompt fields are cleared and the pane is resized if the bar was visible. [[crates/scribe-client/src/main.rs#App#clear_pane_prompts]] performs the same clearing when `AiStateCleared` is received.

## Status Bar

The status bar at the bottom of the window shows connection status, workspace info, CWD, git branch, session count, host context, tmux context, time, and system stats.

Connection is indicated by a green/red dot. Workspace name appears when multi-workspace. The focused pane's remote host overrides the local hostname when shell integration emits session context, and tmux session names render as a separate accent segment. Stats include CPU sparkline, memory percentage, GPU sparkline (Linux only), and network sparklines.

## System Stats

The [[crates/scribe-client/src/sys_stats.rs#SystemStatsCollector]] refreshes every 2 seconds via sysinfo. CPU and network history are kept in rolling buffers (8 and 4 entries respectively) for sparkline rendering. GPU detection on Linux reads AMD sysfs or NVIDIA sysfs/nvidia-smi.

## Dialogs

In-app GPU-rendered overlay dialogs for confirmations, updates, and context menus.

### Close Dialog

An in-app GPU-rendered confirmation dialog with three buttons: Quit Scribe, Kill Window, and Cancel. Both destructive actions wait for a server acknowledgment before the client exits.

### Update Dialog

Shows version information and platform-specific notes with Update Now and Later buttons, opened via the command palette.

The update notification appears in the compositor window title (replacing "Scribe" with "Scribe — v{version} available") rather than in the tab bar. The command palette shows an "Update Scribe to v{version}" entry when an update is available.

### Context Menu

Right-click overlay with Copy (if selection active), Paste, Select All, Open URL (if hovering a URL), and Open File (if hovering a path). Items are rendered as GPU quads with hover highlight.

## URL Detection

The [[crates/scribe-client/src/url_detect.rs#PaneUrlCache]] scans visible terminal rows for URLs (https, http, ftp, file protocols) and file-system paths.

Trailing punctuation is stripped respecting bracket pairs. Detected spans are cached and invalidated on content change. Each span carries a `SpanKind` (`Url` or `Path`).

URL highlighting and the pointer cursor are only shown while the Ctrl modifier is held. The `ModifiersChanged` handler triggers a redraw and cursor update so visual feedback is immediate. Only the clickable span under the cursor is underlined, which keeps the rest of the viewport unchanged until the user targets a specific link or path. Ctrl+click opens the span via `xdg-open` on Linux or `open` on macOS. File paths support an optional `:N` line-number suffix; when present, `code --goto path:N` is tried first and `xdg-open` is the fallback. Relative paths are resolved against the pane's OSC 7 CWD, and `~/` is expanded using `$HOME`.

## Clipboard Cleanup

When copying from a Claude Code or Codex session, [[crates/scribe-client/src/clipboard_cleanup.rs#prepare_copy_text]] applies a two-pass transform: dedent then unwrap.

Dedent strips minimum shared leading whitespace. Unwrap joins hard-wrapped prose at auto-detected wrap width. When no dominant width is detected but at least one line exceeds 40 characters, [[crates/scribe-client/src/clipboard_cleanup.rs#join_non_break_runs]] joins consecutive non-break lines as a fallback. Structural breaks like bullets, headings, code blocks, tables, and blockquotes are preserved.

## Window State

Per-window geometry is persisted under the active install flavor's XDG state root via [[crates/scribe-client/src/window_state.rs#WindowRegistry]].

Stable installs use `$XDG_STATE_HOME/scribe/windows/{window_id}.toml`, while `scribe-dev` uses `$XDG_STATE_HOME/scribe-dev/windows/{window_id}.toml`. `Kill Window` removes the file only after the server confirms the window was destroyed.

Position is stored as Optional since Wayland does not expose window positions. Maximized state is restored after size to avoid window manager override.

### Cold Restart Restore Store

The [[crates/scribe-client/src/restore_state.rs#RestoreStore]] persists logical window state for cold restart recovery under `$XDG_STATE_HOME/{flavor}/restore/`.

A debounced save runs after every layout change via `report_workspace_tree`, snapshotting workspace splits, tabs, pane trees, and per-pane launch bindings. On startup with an empty `SessionList`, the client atomically claims the first entry from the restore index and rebuilds the layout via [[crates/scribe-client/src/restore_replay.rs#prepare_replay]], then creates sessions for each saved pane. Explicit close or quit clears the snapshot; server crash preserves it.

AI panes persist `conversation_id` via OSC 1337 hooks that include `session_id` from the hook JSON payload. [[crates/scribe-client/src/main.rs#App#update_ai_launch_binding]] preserves an existing non-None `conversation_id` when subsequent state updates omit it, ensuring hooks without `session_id` access (e.g. Notification hooks) do not erase the tracking ID. On replay, panes with a `conversation_id` launch `claude --resume <id>` directly; those without fall back to the generic resume picker.

## Config Watching

A file watcher in [[crates/scribe-client/src/config.rs#start_config_watcher]] monitors the active install flavor's XDG config root.

Stable installs watch `$XDG_CONFIG_HOME/scribe/`, while `scribe-dev` watches `$XDG_CONFIG_HOME/scribe-dev/`, for config.toml and theme file changes and forwards `ConfigChanged` through the event loop proxy.

## Search Overlay

Find-in-scrollback overlay state in [[crates/scribe-client/src/search_overlay.rs#SearchOverlay]], tracking query text, match results, and highlighted match index.

This is pure state — no rendering logic is included. The module carries `#![allow(dead_code)]` because rendering integration is pending. Methods: `open` (clears previous query and results), `close` (resets all state), `push_char`/`pop_char` (edit the query string), `set_results` (replace match list and reset highlight), `next_match`/`prev_match` (cycle through results with wrap-around). Match results are `Vec<SearchMatch>` received from the server.

## Tooltip

GPU-rendered tooltip overlay in [[crates/scribe-client/src/tooltip.rs]] that renders a small dark box with light text above or below an anchor rect.

[[crates/scribe-client/src/tooltip.rs#TooltipAnchor]] holds the tooltip text and the anchor `Rect`. [[crates/scribe-client/src/tooltip.rs#TooltipPosition]] selects `Above` or `Below` placement. [[crates/scribe-client/src/tooltip.rs#render_tooltip]] emits `CellInstance` quads into the caller's buffer: a 1 px border quad, a background quad, then per-character glyph quads. The tooltip is horizontally centered on the anchor and clamped to stay within `viewport_width`. A 1-character left/right padding is included on each side of the text.
