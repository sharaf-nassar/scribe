# Client

The scribe-client is a GPU-accelerated terminal frontend built with winit for windowing and wgpu for rendering.

The parallel [[crates/scribe-client-gpui/src/main.rs]] spike opens one GPUI
window and renders one live display-only terminal from the unchanged local IPC
protocol. It remains separate until the GPUI rebuild launch gate.

The GPUI rebuild keeps `appearance.opacity`: [[tools/gpui-window-opacity-spike/src/main.rs]] proves that the pinned GPUI revision opens a transparent Wayland/X11 surface and repaints root alpha live. The decision is recorded in `specs/016-gpui-client-rebuild/spikes/window-opacity-wayland-x11.md`, and [[rendering#Rendering#GPUI Ported Rendering Logic#GPUI Window Opacity]] documents how the client paints it.

## GPUI Client Spike

The scaffold spike (`crates/scribe-client-gpui`) proves GPUI can render a live Scribe pane over the frozen IPC protocol. It builds against the pinned gpui/alacritty revisions and stays a separate crate until the rebuild launch gate.

The spike adopts Zed's display-only terminal model: [[crates/scribe-client-gpui/src/terminal.rs#DisplayOnlyTerminal]] owns an alacritty `Term` plus a VTE `Processor` and holds no PTY. Server bytes enter through [[crates/scribe-client-gpui/src/terminal.rs#DisplayOnlyTerminal#feed_output]], which advances the processor, reports whether the frame changed visible state, and rebuilds an immutable `Content` grid snapshot. [[crates/scribe-client-gpui/src/terminal_element.rs#TerminalElement]] paints that snapshot as fixed-width GPUI rows.

A background thread runs [[crates/scribe-client-gpui/src/main.rs#run_connection]]: it connects to the live server socket, splits it into read/write halves, and queues `Hello` + `ListSessions`. [[crates/scribe-client-gpui/src/main.rs#run_reader]] attaches the first live session and hands every message to [[crates/scribe-client-gpui/src/main.rs#dispatch_server_message]], which normalises `PtyOutput` / `SessionReplay` / `ScreenSnapshot` into raw output bytes (via the [[client#GPUI Client Spike#Session Lifecycle]] helpers, off the drain) and forwards them as [[crates/scribe-client-gpui/src/ipc_bridge.rs#InboundEvent]]. Each coalesced batch bumps a shared generation counter; [[crates/scribe-client-gpui/src/main.rs#drive_redraws]] polls it on the GPUI foreground and calls `notify()` so the window repaints.

The dispatcher implements twenty-nine of the protocol's inbound variants, six of which it names and hands to [[crates/scribe-client-gpui/src/main.rs#on_chrome_message]], four more to [[crates/scribe-client-gpui/src/main.rs#dispatch_share_message]], and three more to [[crates/scribe-client-gpui/src/main.rs#on_window_lifecycle_message]], so the table stays a list of routing decisions. The rest reach [[crates/scribe-client-gpui/src/main.rs#unhandled_server_message]], which names them from the exhaustive table in [[crates/scribe-client-gpui/src/main.rs#server_message_variant]], counts the drop, and logs it at `warn`. Session-scoped arms gate on the attached session inside the arm rather than in a match guard, so a frame for a background pane stays a deliberate no-op instead of being reported as unhandled. See [[architecture#Architecture#Build Tooling#Reachability Gate]] for the ratchet that keeps the unhandled set shrinking.

#### Terminal Chrome Metadata

Only the server knows a pane's CWD, git branch, session context and env health, or a workspace's name, so the client keeps them in [[crates/scribe-client-gpui/src/chrome_metadata.rs#ChromeMetadata]] — a pure store the reader writes and the view reads per frame.

[[crates/scribe-client-gpui/src/main.rs#on_chrome_message]] folds `CwdChanged`, `GitBranch`, `SessionContextChanged`, `EnvStatus` and `WorkspaceNamed` onto that store through [[crates/scribe-client-gpui/src/main.rs#update_chrome_metadata]], which bumps the redraw generation exactly like the AI chrome's own updater; `TitleChanged` instead retitles the pane's tab through [[crates/scribe-client-gpui/src/tab_session.rs#TabSessions#set_title]], and an empty title is ignored so a shell clearing OSC 0/2 cannot blank the label. Metadata is keyed by session rather than by the attached pane, so a background tab keeps its chrome warm and switching tabs repaints without a server round trip; [[crates/scribe-client-gpui/src/chrome_metadata.rs#ChromeMetadata#seed_from_session_list]] also adopts the CWD, branch and context the authoritative `SessionList` replays, so a reattach restores the bar instead of waiting for the next shell prompt.

### IPC Bridge

The [[crates/scribe-client-gpui/src/ipc_bridge.rs]] module carries bytes both directions over the frozen IPC protocol without adding keystroke latency or frame tearing, mirroring Zed's terminal wakeup coalescing.

Inbound: [[crates/scribe-client-gpui/src/ipc_bridge.rs#run_drain]] drains the [[crates/scribe-client-gpui/src/ipc_bridge.rs#InboundEvent]] channel with 4 ms / 100-event coalescing. [[crates/scribe-client-gpui/src/ipc_bridge.rs#coalesce]] collapses a drained run into one per-pane byte buffer in first-seen order ([[crates/scribe-client-gpui/src/ipc_bridge.rs#CoalescedBatch]]), which [[crates/scribe-client-gpui/src/main.rs#spawn_drain]] feeds through the sync-frame queue below. Because output is normalised to bytes before it enters the channel, coalescing only ever concatenates.

#### Sync Frame Queueing

Between the coalescing drain and `feed_output`, a per-session [[crates/scribe-client-gpui/src/sync_frames.rs#SyncFrameQueue]] preserves `CSI ? 2026` commit boundaries so a redraw never tears a frame across IPC splits, ported from the winit client's drain path.

[[crates/scribe-client-gpui/src/sync_frames.rs#SyncFrameQueue#queue_output_frames]] runs the ported streaming splitter, which keeps raw markers intact and emits one committed frame per commit even when the terminating `l` is split across messages. [[crates/scribe-client-gpui/src/main.rs#spawn_drain]] queues each coalesced batch, then [[crates/scribe-client-gpui/src/sync_frames.rs#drain_all_committed]] replays committed bursts into [[crates/scribe-client-gpui/src/terminal.rs#DisplayOnlyTerminal#feed_output]] one burst per redraw via [[crates/scribe-client-gpui/src/sync_frames.rs#drain_until_frame]]; once the backlog passes [[crates/scribe-client-gpui/src/sync_frames.rs#OUTPUT_FRAME_CATCH_UP_THRESHOLD]] it drains through to the latest frame so stale frames never pile up.

A companion [[crates/scribe-client-gpui/src/main.rs#run_sync_expiry]] task waits on the nearest raw-frame ([[crates/scribe-client-gpui/src/sync_frames.rs#RAW_SYNC_TIMEOUT]]) or parser deadline and commits a 150 ms-expired update whose closing `l` never arrived, via [[crates/scribe-client-gpui/src/sync_frames.rs#SyncFrameQueue#flush_raw_timeout]] and [[crates/scribe-client-gpui/src/terminal.rs#DisplayOnlyTerminal#flush_parser_sync_timeout]]. The drain task wakes it whenever a fresh sync update arms a deadline, and each committed burst bumps the shared redraw generation.

Outbound: [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink]] replaces Zed's `write_to_pty` path, enqueuing `ClientMessage::KeyInput` / `Resize` plus the session-lifecycle messages the tab shortcuts drive (`CreateSession` / `AttachSessions` / `Subscribe` / `RequestSnapshot` / `CloseSession`) and the window-lifecycle frames the close dialog, the window-list poll and the focus observer raise (`CloseWindow` / `QuitAll` / `ListWindows` / `FocusChanged`) onto the ordered IPC-writer channel drained by [[crates/scribe-client-gpui/src/main.rs#run_writer]]. The sink is independent of the inbound drain, so a keystroke is never queued behind an output firehose; because the channel is a single FIFO, a `Resize` enqueued before a `KeyInput` reaches the server first. The GPUI view feeds the sink from [[crates/scribe-client-gpui/src/main.rs#TerminalView#on_key_down]] through [[crates/scribe-client-gpui/src/main.rs#encode_key]], which is the live entry point of the ported [[client#Input#GPUI Input Encoder Port]] rather than a table of its own.

### Session Lifecycle

[[crates/scribe-client-gpui/src/session_lifecycle.rs]] ports the legacy client's reattach, reconnect, and scrollback-trim semantics onto the display-only terminal. Decode and snapshot conversion stay pure and run in [[crates/scribe-client-gpui/src/main.rs#run_reader]] ahead of the drain, so a corrupt replay degrades to pane status without crashing.

[[crates/scribe-client-gpui/src/session_lifecycle.rs#decode_replay]] zstd-decompresses a `SessionReplay` (rejecting zero-dimension or corrupt streams as a [[crates/scribe-client-gpui/src/session_lifecycle.rs#ReplayDecodeError]]); [[crates/scribe-client-gpui/src/session_lifecycle.rs#snapshot_reset_bytes]] prefixes RIS so a `ScreenSnapshot` resets the terminal before replaying `snapshot_to_ansi`. [[crates/scribe-client-gpui/src/session_lifecycle.rs#SessionRegistry]] tracks live sessions, rebuilds the reconnect topology from `SessionList`, applies `SessionCreated` / `SessionExited`, and adopts the window id from a takeover `Welcome`. [[crates/scribe-client-gpui/src/session_lifecycle.rs#shift_absolute_marks_after_trim]] shifts stored absolute [[crates/scribe-client-gpui/src/session_lifecycle.rs#CommandMark]] rows after a `TrimScrollback`.

Every attach carries a `Subscribe` for the pane it just attached, sent through [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#subscribe]] behind the `AttachSessions` from both attach paths — [[crates/scribe-client-gpui/src/main.rs#attach_session]] on the reader's `SessionList` / `SessionCreated` route and [[crates/scribe-client-gpui/src/main.rs#TerminalView#attach]] on a tab switch. Order matters because [[crates/scribe-server/src/ipc_server.rs#handle_subscribe]] rejects a subscription for a session this connection is not attached to; the shared ordered writer channel and the server's sequential per-connection dispatch make that impossible by construction. Subscribing is what makes the server run its CWD-fallback check for the newly visible pane, so a reattached tab gets its directory (and the workspace name derived from it) without waiting for the next shell prompt.

[[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#request_snapshot]] is the display-only client's resync: it owns no PTY and never replays locally, so when its pane may have drifted from the server's `Term` the only way back to a correct pane is to ask. Two live paths send it. [[crates/scribe-client-gpui/src/main.rs#TerminalView#report_cell_metrics]] follows its post-font-reload `Resize` with one, because that resize raises `SIGWINCH` on the server's PTY and the client cannot derive the resulting grid itself. [[crates/scribe-client-gpui/src/main.rs#forward_replay]] sends one when a reattach replay fails to decode, turning a permanently stale tab into a repaint. The reply lands on [[crates/scribe-client-gpui/src/main.rs#apply_screen_snapshot]], which feeds RIS plus the snapshot's own ANSI (visible grid *and* scrollback) through the drain, so everything on screen afterwards came out of that snapshot.

#### Replay reattach applies

A decoded `SessionReplay` reattach frame reproduces the session grid when written to a fresh terminal.

#### Replay decode failure

A `SessionReplay` with a corrupt zstd payload yields a `ReplayDecodeError` and leaves the terminal untouched and still usable, so the pane surfaces an error without crashing the reader.

#### Snapshot resets before replay

`snapshot_reset_bytes` clears prior pane content before replaying, so a tooling `ScreenSnapshot` replaces rather than appends onto the terminal.

#### Zero-dimension replay rejected

A replay reporting zero rows or columns is rejected up front rather than fed through the VTE pipeline.

#### Trim shifts marks

A `TrimScrollback` shifts surviving absolute marks down by the dropped-row count and drops marks anchored inside the trimmed region.

#### Trim clears input below delta

When the pending input-start row falls inside the trimmed region it is cleared, and a zero-row trim is a no-op.

#### Reconnect topology rebuild

`SessionList` rebuilds the session-to-workspace topology grouped by workspace in first-seen order, pruning workspaces without live sessions.

#### Created and exited transitions

`SessionCreated` registers a pane in arrival order without duplicating re-announcements, and `SessionExited` retires it and reports whether it was tracked.

#### Registry trims marks

`SessionRegistry::on_trim_scrollback` shifts a session's stored marks by the drop between successive server history sizes and clears that state when the session exits.

#### Takeover adoption

A takeover `Hello`'s `Welcome` records the adopted window id on the registry.

### Tab Strip And Key Dispatch

The shell's live key path runs the configured bindings before the PTY encoder, so tab shortcuts open, switch, and close real server sessions instead of leaking their chord as terminal bytes.

[[crates/scribe-client-gpui/src/main.rs#TerminalView#handle_binding]] lowers each `KeyDownEvent` through [[crates/scribe-client-gpui/src/input.rs#KeyInput#from_key_down]] and [[crates/scribe-client-gpui/src/keybindings.rs#translate_key_action]], and is consulted after the overlays own the keyboard but before [[crates/scribe-client-gpui/src/main.rs#encode_key]], which runs the ported byte encoder for everything no binding claimed — an unbound named key such as PageUp/PageDown therefore reaches the PTY as `CSI 5~` / `CSI 6~` instead of being dropped. The resolved [[crates/scribe-client-gpui/src/keybindings.rs#KeyAction]] is handed to [[crates/scribe-client-gpui/src/main.rs#TerminalView#dispatch_key_action]], the shell's single live dispatch point, which the command palette also targets so a row and its chord can never drift apart. A matched [[crates/scribe-client-gpui/src/keybindings.rs#LayoutAction]] reaches [[crates/scribe-client-gpui/src/main.rs#TerminalView#handle_layout_action]]; the four scrollback actions and the three zoom actions run ([[client#GPUI Client Spike#GPUI Terminal Viewport Wiring]]), while the pane and workspace families are still swallowed (no pane tree yet) rather than forwarded, matching the legacy client's rule that a bound shortcut never reaches the PTY. That dispatcher matches exhaustively and names each swallowed variant, so a new `LayoutAction` fails to compile instead of joining the dropped set unnoticed, and every drop goes through [[crates/scribe-client-gpui/src/main.rs#unhandled_layout_action]] to be counted and warned. [[architecture#Architecture#Build Tooling#Reachability Gate]] ratchets that set.

The tab actions drive the IPC sink: `new_tab` and the four AI-tab shortcuts send [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#create_session]] into the focused workspace (the AI variants through [[crates/scribe-client-gpui/src/main.rs#ai_tab_command]], which execs the CLI under the login shell exactly like the winit client), `next_tab` / `prev_tab` / `select_tab_N` move the selection and re-[[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#attach_sessions]], and `close_tab` sends [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#close_session]].

`new_window` opens a second top-level window in the same process through [[crates/scribe-client-gpui/src/main.rs#TerminalView#open_new_window]], rather than re-spawning the binary the way the winit client's `spawn_client_process` had to — GPUI is multi-window, as the settings window already shows. [[crates/scribe-client-gpui/src/main.rs#start_window_backend]] builds each window's own `Shared` state and its own IPC connection, and `main` calls it for the startup window too, so the two paths cannot drift. Independent state is what makes it a window rather than a mirror: the `Hello` on that second connection carries no window id, so the server registers a *new* window and gives it its own sessions, tab strip, and status line.

[[crates/scribe-client-gpui/src/tab_session.rs#TabSessions]] is the ordered strip both sides share behind a mutex. [[crates/scribe-client-gpui/src/main.rs#run_reader]] rebuilds it from `SessionList`, appends focused tabs on `SessionCreated`, and drops them on `SessionExited`; [[crates/scribe-client-gpui/src/main.rs#TerminalView#sync_tabs]] mirrors it into the titlebar on redraw.

A tab's label has two sources. `TitleChanged` sets the OSC 0/2 title through [[crates/scribe-client-gpui/src/tab_session.rs#TabSessions#set_title]], and the four provider task-label notices — `TaskLabelChanged` / `TaskLabelCleared` plus the legacy Codex spelling the server still emits for Codex sessions — reach [[crates/scribe-client-gpui/src/main.rs#on_task_label_message]], which folds them into the same strip through [[crates/scribe-client-gpui/src/tab_session.rs#TabSessions#set_task_label]]. [[crates/scribe-client-gpui/src/tab_session.rs#TabEntry#display_title]] then prefers the task label while one is active, so an AI tab is named by the work it is doing and falls back to its shell title when the tool stops — the winit `Pane::preferred_tab_title` rule, unchanged. A blank label is treated as a clear rather than a blank tab, the provider only says who set the label (one label per session wins), and `SessionList` replays a mid-task label so a reattach restores the name instead of waiting for the next provider event. Verified against the running app in [[test#Visual E2E Tests#AI task labels rename the tab]]. Because the server re-announces `SessionCreated` as its acknowledgement of every `AttachSessions`, only a genuine insert by [[crates/scribe-client-gpui/src/tab_session.rs#TabSessions#insert_active]] triggers an attach — treating the echo as a new tab would attach in an unbounded loop. [[crates/scribe-client-gpui/src/main.rs#attach_session]] then points `active_session` at the focused tab, and the reader reads that shared value on every message so output gating follows a switch made on the GPUI thread.

### GPUI Terminal Viewport Wiring

The running client reaches the ported viewport modules — scrollback navigation, vi / copy mode, split-scroll, smart selection, and font zoom — through [[crates/scribe-client-gpui/src/terminal.rs#DisplayOnlyTerminal]], the one type that owns a live `Term`.

A split window holds one of those per pane, so every surface below acts on the focused pane, reached through [[crates/scribe-client-gpui/src/main.rs#TerminalView#with_focused_grid]].

Before this, [[crates/scribe-client-gpui/src/terminal.rs#DisplayOnlyTerminal#make_content]] read the *screen* rows and ignored the grid's display offset, so the pure modules were untestable in the product: no scroll could change a pixel. It now reads the viewport through the offset, which is what makes `scroll_up` / `scroll_down` / `scroll_top` / `scroll_bottom` real. [[crates/scribe-client-gpui/src/main.rs#TerminalView#scroll_terminal]] runs each of the four [[crates/scribe-client-gpui/src/keybindings.rs#LayoutAction]] variants through [[crates/scribe-client-gpui/src/terminal.rs#DisplayOnlyTerminal#scroll]], and [[crates/scribe-client-gpui/src/main.rs#TerminalView#snap_to_bottom_for_input]] ports the winit rule that typing into a scrolled pane jumps back to the live bottom.

Split-scroll is decided across the shell/terminal boundary because each side owns half the inputs. The shell contributes the [[crates/scribe-client-gpui/src/split_scroll.rs#SplitScrollEligibility]] pair — the `scroll_pin` config key and whether the focused session runs an enabled AI provider — on every frame through [[crates/scribe-client-gpui/src/main.rs#TerminalView#sync_split_scroll]]; the terminal adds the live half (scrolled up, normal screen) in [[crates/scribe-client-gpui/src/split_scroll.rs#split_scroll_eligible]] and sizes the pin with [[crates/scribe-client-gpui/src/split_scroll.rs#compute_pin_rows]] and [[crates/scribe-client-gpui/src/split_scroll.rs#align_pin_rows_to_logical_lines]]. Rather than the winit client's dual render, the split is expressed in the snapshot itself: the trailing `Content::pin_rows` rows are read from the live screen anchored on the shell cursor while the rows above stay at the scrolled offset, which is the same cursor-anchored translation [[crates/scribe-client-gpui/src/split_scroll.rs#live_cell_y_translation]] describes in pixels, done in row space where a cell grid can express it exactly. [[crates/scribe-client-gpui/src/terminal_element.rs#TerminalElement#paint_split_scroll]] then draws only the seam — the divider and the docked jump chip from [[crates/scribe-client-gpui/src/split_scroll.rs#compute_geometry]] — and a click is resolved against the same geometry by [[crates/scribe-client-gpui/src/terminal_element.rs#hits_jump_chip]]. Typing keeps the pin up; Enter collapses it, exactly as the winit client behaves.

Vi mode is a shell-owned keyboard mode, so it enters through the [[crates/scribe-client-gpui/src/keybindings.rs#OverlayChord]] table (`ctrl+shift+space`) rather than a `KeybindingsConfig` field it has none of — which also means it yields to a user rebind that lands on the same keys. While it is active, [[crates/scribe-client-gpui/src/main.rs#TerminalView#handle_vi_key]] sits between the shell chords and the configured bindings: a bound chord still runs (paging the scrollback keeps working), a bare motion key drives [[crates/scribe-client-gpui/src/vi_mode.rs#vi_motion]] through [[crates/scribe-client-gpui/src/main.rs#vi_motion_for_key]], and every other bare key is swallowed so `j` can never leak into the shell. The cursor is published on the snapshot in viewport coordinates and outlined by [[crates/scribe-client-gpui/src/terminal_element.rs#TerminalElement#paint_vi_cursor]] as a hollow box, so the character under it stays readable.

Smart selection reaches the product through the right-click menu. [[crates/scribe-client-gpui/src/terminal_element.rs#cell_at]] lowers the pointer position onto a grid cell using the bounds the grid canvas recorded for the frame, [[crates/scribe-client-gpui/src/terminal.rs#DisplayOnlyTerminal#smart_selection_actions]] resolves that cell against the display offset before matching, and [[crates/scribe-client-gpui/src/main.rs#smart_selection_action]] lowers each resolved action onto the [[crates/scribe-client-gpui/src/context_menu.rs#ContextMenuAction]] that runs it — one-for-one with the winit `smart_selection_context_action`, so a rule authored against the legacy client behaves identically.

Zoom folds [[crates/scribe-client-gpui/src/zoom.rs#ZoomState]] into the grid font rather than into the config: [[crates/scribe-client-gpui/src/main.rs#TerminalView#rebuild_font]] is the single place both a zoom step and a config font reload rebuild [[crates/scribe-client-gpui/src/terminal_element.rs#GridFont]] from `appearance.font_size` plus the live zoom level, so a saved font-size edit rebases the zoom instead of discarding it, and both paths republish cell metrics to the server through [[crates/scribe-client-gpui/src/main.rs#TerminalView#report_cell_metrics]].

Covered headlessly by [[test#GPUI Terminal Viewport]] and against the running app by [[test#Visual E2E Tests#Terminal viewport navigation]].

### Share And Control Handoff

The feature-015 sharing surface is live in the GPUI shell: the reader mirrors the server's roster and control notices, the key path answers them, and the render pass draws the presence panel, the transient hint, and the modal grant/deny prompt.

[[crates/scribe-client-gpui/src/share.rs#ShareChrome]] is the shared aggregate — the mirrored [[crates/scribe-client-gpui/src/share.rs#ShareState]], the pending [[crates/scribe-client-gpui/src/share.rs#ControlRequestPrompt]], the transient [[crates/scribe-client-gpui/src/share.rs#ControlHint]], and this connection's own participant id from `Welcome`. [[crates/scribe-client-gpui/src/main.rs#dispatch_share_message]] folds `ShareRoster`, `ControlRequested`, `ControlDenied`, and `ShareEnded` into it through [[crates/scribe-client-gpui/src/main.rs#update_share_chrome]], which repaints on every change; a roster that drains back to a single participant tears the surfaces down, exactly like the winit [[crates/scribe-client/src/share_view.rs#ShareState]] port it replaces.

Input follows the winit dispatch order. A pending request is a full-window modal claimed at the top of [[crates/scribe-client-gpui/src/main.rs#TerminalView#handle_overlay_key]], so it is answered before any binding, overlay, or PTY byte; everything else reaches [[crates/scribe-client-gpui/src/share.rs#ShareChrome#intercept_key]] from [[crates/scribe-client-gpui/src/main.rs#TerminalView#on_key_down]], after the configured bindings, so a viewer keeps its shortcuts while its terminal keystrokes are suppressed. The decision table returns a [[crates/scribe-client-gpui/src/share.rs#ShareKeyOutcome]]: a viewer's first key raises the take-control hint and is dropped, Enter while that hint is up claims control, and the prompt's Enter/Esc grants or denies. Each emitted [[crates/scribe-client-gpui/src/share.rs#ControlIntent]] leaves through [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#control_intent]], the single place the frozen v3 `ControlClaim` / `ControlGrant` frames are built.

Rendering has two halves. [[crates/scribe-client-gpui/src/share.rs#share_overlay]] draws the roster panel, the hint strip, and the dimmed modal on the window's overlay layer, and [[crates/scribe-client-gpui/src/share.rs#ShareChrome#presence]] feeds the status bar's existing share badge ([[client#GPUI Client Spike#GPUI Status Bar Port]]). Because a hint expires on wall-clock time rather than on traffic, [[crates/scribe-client-gpui/src/main.rs#drive_redraws]] clears it on the same 16 ms idle-wake tick it already runs, so a notice cannot outlive its window on a quiet pane.

The surface is verified against the running app, not headlessly: see [[test#Visual E2E Tests#Window sharing and control handoff]].

### Cold Restart Restore

The GPUI client ports cold-restart recovery: after a server crash the bootstrap window rebuilds its windows, workspaces, tabs, and panes from persisted snapshots and re-creates each saved session at the correct geometry.

[[crates/scribe-client-gpui/src/restore_state.rs#RestoreStore]] persists one TOML snapshot per window under `$XDG_STATE_HOME/scribe/restore/windows/<window_id>.toml` plus a shared `index.toml`, all hardened to `0700`/`0600` because launch bindings can carry prompt text and provider conversation IDs. A bootstrap lock serialises multi-process index mutations and stale locks (>30 s) are reclaimed. [[crates/scribe-client-gpui/src/restore_state.rs#RestoreStore#claim_first_window]] atomically claims the first replayable [[crates/scribe-client-gpui/src/restore_state.rs#WindowRestoreState]] entry, skips non-replayable and unreadable entries, and reports how many windows remain so the caller fans out `--restore-child` processes via [[crates/scribe-client-gpui/src/restore_replay.rs#spawn_restore_children]] — each child claims exactly one more entry and, gated by [[crates/scribe-client-gpui/src/restore_replay.rs#is_restore_child]], never fans out again.

[[crates/scribe-client-gpui/src/restore_replay.rs#prepare_replay]] rebuilds a [[crates/scribe-client-gpui/src/restore_replay.rs#RebuiltWindow]] — the [[crates/scribe-client-gpui/src/workspace_layout.rs#WindowLayout]], a [[crates/scribe-client-gpui/src/restore_replay.rs#PaneRestore]] map (standing in for the legacy `Pane` struct the display-only spike lacks), and the ordered [[crates/scribe-client-gpui/src/restore_replay.rs#ReplayLaunch]] queue that re-creates each session. Before the launches dispatch, [[crates/scribe-client-gpui/src/restore_replay.rs#size_replay_pane_grids]] sizes every pane grid from the re-applied window geometry (not the pre-restore hint) so maximized windows do not create PTYs at the startup size and stay undersized. [[crates/scribe-client-gpui/src/restore_replay.rs#attach_dimensions_for_session]] preserves the Codex 0x0 exception: reattaching a Codex session sends a zero-sized `TerminalSize` so the server does not pre-size its Ink-rendered PTY. [[crates/scribe-client-gpui/src/restore_replay.rs#snapshot_window_restore]] serialises the live layout and pane metadata back into a `WindowRestoreState` for the next save.

#### Snapshot round-trips through disk

A saved [[crates/scribe-client-gpui/src/restore_state.rs#WindowRestoreState]] loads back with its window, focused workspace, workspace name, and launch records intact and reports as replayable.

#### Claim skips non-replayable and remaining count

`claim_first_window` drops a blank (non-replayable) entry, claims the first replayable window while removing its file, and reports the remaining count so the caller knows how many `--restore-child` windows to spawn.

#### Stale lock reclaimed

`RestoreStore::lock_is_stale` treats a bootstrap lock older than the 30 s window as stale (reclaimable) and a freshly stamped one as live.

#### Replay rebuilds layout and queue

`prepare_replay` reconstructs the window layout, focused workspace, and accent colour, and produces one `ReplayLaunch` per saved pane carrying the workspace, launch id, cwd, and command, with pane metadata keyed by the same pane id.

#### Snapshot survives rebuild round trip

Serialising a rebuilt window with `snapshot_window_restore` reproduces the original window, focused workspace, tabs, focused launch id, and launch records, and the result is replayable.

#### Grid sized before launch

`size_replay_pane_grids` computes each pane's terminal grid from the restored viewport and cell size and writes it back onto the pane before any launch dispatches.

#### Codex reattach sends zero size

`attach_dimensions_for_session` returns the sized grid for a normal session but a zero-sized `TerminalSize` for a Codex session, preserving the exception that leaves Codex PTY sizing to its own SIGWINCH.

#### Restore child never fans out

`is_restore_child` detects the `--restore-child` flag so a fanned-out child passes count 0 to `spawn_restore_children` and never spawns further windows.

#### AI command detection

An AI resume launch record expands to a shell argv that single-quotes the conversation id after the provider's resume args, and `detect_ai_command` recognises the provider's binary invocation.

### GPUI Layout Entities

The GPUI rebuild ports the two-level split tree into a `lib` target alongside the scaffold binary, so the pure trees and their entity wrappers are library API covered by `#[gpui::test]` headless suites.

The pane split tree is [[crates/scribe-client-gpui/src/layout.rs#LayoutTree]] (binary `Leaf`/`Split` nodes, ratios clamped 0.1-0.9, spatial-overlap directional focus with edge wrap). The workspace split tree is [[crates/scribe-client-gpui/src/workspace_layout.rs#WindowLayout]], whose `WorkspaceSlot` carries tabs, active tab index, accent color, name, and project root. `TabState` drops the winit client's `selection` field until terminal selection is ported in Phase B.

#### Pane Tree Model

[[crates/scribe-client-gpui/src/pane_tree.rs#PaneTree]] is a `gpui::Entity` wrapping a `LayoutTree`. Every structural mutation emits `PaneTreeEvent::Changed` and calls `notify()`.

[[crates/scribe-client-gpui/src/pane_tree.rs#PaneTree#split]] and [[crates/scribe-client-gpui/src/pane_tree.rs#PaneTree#close]] both auto-equalize the surviving ratios so sibling panes stay evenly sized; `close` refuses to remove the sole root leaf. [[crates/scribe-client-gpui/src/pane_tree.rs#PaneTree#set_ratio]] clamps to 0.1-0.9, and `find_pane_in_direction` resolves directional focus (with edge wrap) without mutating or emitting.

#### Workspace Tree Model

[[crates/scribe-client-gpui/src/workspace_tree.rs#WorkspaceTree]] is a `gpui::Entity` wrapping a `WindowLayout` plus the running `PaneId -> SessionId` map. Every mutation re-serializes the tree and emits `WorkspaceTreeEvent::Report`.

The event payload is the exact `WorkspaceTreeNode` the client forwards to the server as [[crates/scribe-common/src/protocol.rs#ClientMessage]] `ReportWorkspaceTree`.

Reported mutations include workspace split, tab add/remove, [[crates/scribe-client-gpui/src/workspace_tree.rs#WorkspaceTree#set_active_tab]], workspace ratio change (clamped 0.1-0.9), and in-place slot edits via `update_slot`. On reconnect the restore path pushes tabs (each auto-activating the last) and then replays `active_tab_index` through `set_active_tab` to restore the originally focused tab, matching the winit client's post-pass.

[[crates/scribe-client-gpui/src/workspace_tree.rs#WorkspaceTree#set_workspace_id]] renames a region and [[crates/scribe-client-gpui/src/workspace_tree.rs#WorkspaceTree#remove_workspace]] drops one, both reporting like every other mutation. The shell needs the first because it builds its region before the server has named a workspace, and the second when a region's last pane closes.

### GPUI Pane And Workspace Shell

[[crates/scribe-client-gpui/src/pane_shell.rs#PaneShell]] is what makes the two split trees above reachable from the running binary: it owns the window's one [[crates/scribe-client-gpui/src/workspace_tree.rs#WorkspaceTree]] plus one [[crates/scribe-client-gpui/src/pane_tree.rs#PaneTree]] per workspace region.

Two layers, matching the chrome Scribe has always had. A workspace region is a slice of the window with its own accent colour; panes are the splits inside one region. `workspace_split_*` moves the outer divider and `split_*` the inner one, and each focus family moves within its own layer. Every pane hosts at most one session, and the focused pane of the focused region is the pane keystrokes, the status bar, and the tab strip follow — [[crates/scribe-client-gpui/src/main.rs#TerminalView#focus_pane_session]] is the single place that alignment is re-established after any focus move.

The shell holds no pixel state. Callers pass a viewport rect derived from the live [[crates/scribe-client-gpui/src/terminal_element.rs#GridFont]] ([[crates/scribe-client-gpui/src/main.rs#TerminalView#pane_viewport]]), so the layout stays a pure function of the trees plus the current cell metrics, and a font-size edit re-derives it for free. [[crates/scribe-client-gpui/src/pane_shell.rs#PaneShell#placements]] resolves that viewport into one [[crates/scribe-client-gpui/src/pane_shell.rs#PanePlacement]] per leaf, which [[crates/scribe-client-gpui/src/main.rs#TerminalView#render_panes]] lowers onto absolutely positioned children whose offsets and sizes are *fractions* of the grid area — so the ratios survive any window size without the view measuring device pixels. The focus ring is drawn only once a window actually has more than one pane, and takes the owning region's accent, so an unsplit window paints exactly as before.

[[crates/scribe-client-gpui/src/pane_shell.rs#PaneShell#close_focused_pane]] closes a pane, or the whole region when it was the region's last one and other regions remain; when the window is down to a single pane in a single region it answers [[crates/scribe-client-gpui/src/pane_shell.rs#ClosedPane]] `LastPane` and the caller closes the tab instead. [[crates/scribe-client-gpui/src/pane_shell.rs#PaneShell#retire_pane]] is the shared removal path, so an exited session's pane collapses the same way a deliberate close does.

#### Pane Session Reconciliation

The two halves of the truth move on different threads — the IPC reader owns the session list and the focused session, the GPUI thread owns the split trees — so [[crates/scribe-client-gpui/src/main.rs#TerminalView#reconcile_panes]] settles them once per frame rather than letting the reader touch GPUI entities.

Three things can be out of step. The root region starts on a client-minted `WorkspaceId` because the shell exists before the first `Welcome`, and adopts the server's through [[crates/scribe-client-gpui/src/pane_shell.rs#PaneShell#adopt_server_workspace]] once a `SessionList` names one. A pane whose session exited is retired by [[crates/scribe-client-gpui/src/pane_shell.rs#PaneShell#retain_sessions]]. And a session the server has just created has no pane yet: a split queues the pane that asked for it ([[crates/scribe-client-gpui/src/pane_shell.rs#PaneShell#take_pending]]), and anything else — a new tab, a reattach, a refocus after an exit — lands in the pane the user is looking at.

Workspace regions beyond the first are client-local layout: the server still owns exactly one workspace for this window, so a region's seeded session is created in that workspace. `ClientMessage::CreateWorkspace` and the rest of the workspace IPC are a separate bead.

#### Per-Pane Grids And Sizing

A split window shows several live terminals at once, so [[crates/scribe-client-gpui/src/terminal.rs#PaneGrids]] keys one [[crates/scribe-client-gpui/src/terminal.rs#DisplayOnlyTerminal]] per session instead of folding every pane into a single grid.

The coalescing drain already carries a `SessionId` with every batch, so [[crates/scribe-client-gpui/src/main.rs#spawn_drain]] advances the grid the batch names and a background pane's burst can never land in the focused pane's scrollback. `PtyOutput` / `SessionReplay` / `ScreenSnapshot` are gated on the set of attached sessions ([[crates/scribe-client-gpui/src/main.rs#is_attached]]) rather than on the single focused one, because every pane streams.

[[crates/scribe-client-gpui/src/main.rs#TerminalView#publish_pane_sizes]] runs after any layout change: each pane's rect yields a cell count, which is reshaped locally through [[crates/scribe-client-gpui/src/terminal.rs#DisplayOnlyTerminal#resize]] and announced to the server as `Resize` followed by `RequestSnapshot` — the client owns no PTY and never reflows locally, so the authoritative grid has to come back from the server. Unchanged panes are skipped, so a redraw storm never becomes a `RequestSnapshot` storm.

Because the terminal-navigation surfaces ([[client#GPUI Client Spike#GPUI Terminal Viewport Wiring]]) all act on the live `Term`, they reach it through [[crates/scribe-client-gpui/src/main.rs#TerminalView#with_focused_grid]] rather than a window-wide terminal: scrollback, vi/copy mode, the split-scroll pin, the jump chip and smart selection all resolve against the pane the user is in. `active_session` names that pane's session by construction, since both [[crates/scribe-client-gpui/src/main.rs#TerminalView#focus_pane_session]] and the reader's attach path re-point it on every focus move. For the same reason only the focused pane publishes its painted [[crates/scribe-client-gpui/src/terminal_element.rs#GridBounds]] and its find-match spans; a background pane paints its own untouched grid.

### GPUI URL Detection Port

The GPUI rebuild ports the URL and OSC 8 scanner into the `lib` target so hover, Ctrl-highlight, and open affordances reuse the same [[client#URL Detection]] logic byte-for-byte across the cutover.

[[crates/scribe-client-gpui/src/url_detect.rs#PaneUrlCache]] is a verbatim port of the winit [[crates/scribe-client/src/url_detect.rs#PaneUrlCache]] onto Zed's Alacritty fork: the same scheme list (https/http/ftp/file/mailto/ssh/telnet), `WRAPLINE` join, trailing-punctuation stripping, per-row [[crates/scribe-client-gpui/src/url_detect.rs#RowSegment]] geometry, hard-break continuation ([[crates/scribe-client-gpui/src/url_detect.rs#hard_break_continuation_start]]), and OSC 8 precedence with `id=` reconnection and the 2048-byte URI cap ([[crates/scribe-client-gpui/src/url_detect.rs#scan_osc8_hyperlinks]]). Because the selection port lands in a separate bead, the two grid cell readers ([[crates/scribe-client-gpui/src/url_detect.rs#read_cell_char]], [[crates/scribe-client-gpui/src/url_detect.rs#read_cell_flags]]) are defined locally instead of imported from `selection`.

Activation is ported alongside detection: [[crates/scribe-client-gpui/src/url_detect.rs#open_path]] (with the `:N` line-number suffix and `code --goto` fallback), [[crates/scribe-client-gpui/src/url_detect.rs#open_url]], and the disallowed-scheme gate hook ([[crates/scribe-client-gpui/src/url_detect.rs#is_allowed_scheme]], [[crates/scribe-client-gpui/src/url_detect.rs#open_uri_unguarded]]). The view-side hover/dwell/Ctrl-highlight wiring lands in a later GPUI phase.

### GPUI IME Composition

The GPUI rebuild ports the winit [[client#Input#IME Composition]] preedit semantics onto GPUI's IME plumbing so composition anchors on an absolute scrollback row with an underline overlay. IME needs a real compositor, so it is a manual parity item, not a `#[gpui::test]`.

[[crates/scribe-client-gpui/src/preedit.rs#PreeditState]] is the verbatim data port (redacted-`Debug` composition text, optional caret hint, absolute start row + column). [[crates/scribe-client-gpui/src/preedit.rs#PreeditMachine]] is a GPUI-free state machine mirroring the winit `WindowEvent::Ime` arm: a non-empty `mark` arms/updates the composition anchored at the last `set_anchor` cell, an empty `mark` clears it, and `commit` clears and returns the committed text. The in-flight anchor stays fixed while a later `set_anchor` only affects the next composition.

[[crates/scribe-client-gpui/src/preedit.rs#Ime]] wraps the machine in a `gpui::Entity` and implements `gpui::EntityInputHandler`: GPUI routes marked (composing) text through `replace_and_mark_text_in_range` and committed text through `replace_text_in_range`, which re-emits [[crates/scribe-client-gpui/src/preedit.rs#ImeEvent]] `Commit` so the view sends it through the normal `KeyInput` path. The terminal owns no editable document, so the text-query methods report an empty buffer and candidate placement follows the composition anchor.

#### Preedit Overlay Geometry

[[crates/scribe-client-gpui/src/preedit.rs#compute_overlay]] recomputes the [[crates/scribe-client-gpui/src/preedit.rs#PreeditOverlay]] each frame from the anchor and the live [[crates/scribe-client-gpui/src/preedit.rs#PreeditGeometry]], returning `None` while scrolled into scrollback so the underline never renders at the wrong visual row.

The absolute [[crates/scribe-client-gpui/src/preedit.rs#PreeditState]] `start_row` minus `viewport_top_abs_row` resolves the on-screen line, so terminal scroll keeps the underline pinned to the originating line; a row above or below the visible window, a non-zero `display_offset`, or an anchor column past the right edge all yield `None`. [[crates/scribe-client-gpui/src/preedit.rs#preedit_cell_width]] sizes the underline via `unicode_width` advances (wide CJK glyphs reserve two cells, zero-width marks ride the base glyph, a leading combining mark is skipped), matching the renderer's styled-run accumulator.

#### IME Parity Procedure

IME is verified manually on both display servers because it depends on a live input-method engine the headless test harness cannot drive. The procedure exercises compose, update, commit, cancel, and the scrollback-stable anchor.

1. **X11 + ibus/fcitx**: start an IME engine (`ibus-daemon -drx` or `fcitx5`), select a CJK input method, run `scribe-client-gpui` under X11, focus a pane, and type a multi-key composition (e.g. Japanese `nihongo`). Verify the underlined preedit appears at the cursor cell, updates in place as keys arrive, the OS candidate window anchors under the composition, Enter commits the selected text to the PTY (preedit clears the same frame the echo lands), and Escape cancels with no bytes sent.
2. **Wayland**: repeat under a Wayland compositor with `text-input-v3` (e.g. GNOME/Mutter or Sway with `fcitx5`), confirming identical compose/commit/cancel behaviour and candidate placement.
3. **Scrollback anchor**: begin a composition, scroll the viewport up into scrollback, and confirm the preedit overlay disappears while scrolled and reappears pinned to the originating line on return to the bottom.
4. **Focus loss**: switch window focus mid-composition and confirm the preedit retires immediately with no committed bytes.

### Bracketed Paste Gate

The GPUI rebuild ports the winit [[client#Dialogs#Paste Confirmation Dialog]] spec-011 gate so a risky paste is parked behind a confirmation before any byte reaches the PTY. The pure classifier and the entity gate are covered by `#[gpui::test]`.

[[crates/scribe-client-gpui/src/paste.rs#classify_paste]] is the byte-for-byte classifier port: it returns [[crates/scribe-client-gpui/src/paste.rs#PasteRisk]] iff the content has a line break (`\n`/`\r`) or a non-tab control/escape character. [[crates/scribe-client-gpui/src/paste.rs#PasteGate]] is a `gpui::Entity` whose `request` emits [[crates/scribe-client-gpui/src/paste.rs#PasteGateEvent]] `Confirm` (and parks a [[crates/scribe-client-gpui/src/paste.rs#ParkedPaste]]) only when the `terminal.paste_confirmation` config is on, the focused pane has NOT enabled bracketed paste, and the content classifies as risky; otherwise it emits `Send`. The enabled and bracketed checks short-circuit before classification so the common path adds no work.

On the user's answer, `confirm` re-emits `Send` on the exact parked bytes — bypassing the gate, matching the winit resume path — while `cancel` drops the parked paste without sending anything.

### Bell Routing

The GPUI rebuild ports the winit `handle_bell_event` suppression gate onto an entity that routes a terminal bell to a per-tab attention badge plus the system bell, wired end to end from the live IPC reader to the window's attention request.

[[crates/scribe-client-gpui/src/bell.rs#BellController]] is a `gpui::Entity` tracking window focus, the focused session, and whether an update is in progress. `on_bell` records an attention badge and emits [[crates/scribe-client-gpui/src/bell.rs#BellEvent]] `Signal` (the view rings the OS bell / requests window attention) only when the bell targets a session other than the focused foreground pane — or the window is unfocused — and no update is in progress; a bell to the already-focused foreground pane is suppressed, exactly like the winit client. `focus_session` retires that session's badge.

The gate cannot run on the IPC reader thread: it is a GPUI entity and the action it authorises is a window-level call. [[crates/scribe-client-gpui/src/main.rs#on_bell_message]] therefore only records which session belled, onto a queue shared with the foreground, and bumps the redraw generation. [[crates/scribe-client-gpui/src/main.rs#TerminalView#poll_bells]] drains that queue on the window-lifecycle tick, refreshing the gate's three inputs from where they actually live first — the focused pane from the shared `active_session`, the in-flight update from the shared [[crates/scribe-client-gpui/src/update.rs#UpdateState]] (the winit client read `update_available.is_none()` at the same point), and window focus from [[crates/scribe-client-gpui/src/main.rs#TerminalView#on_activation]]. A queued bell is therefore judged against the focus state it is delivered under, not the one it arrived under.

[[crates/scribe-client-gpui/src/main.rs#TerminalView#start_bell_gate]] subscribes the view to the controller *in* the window, so a `Signal` arrives with the `Window` that [[crates/scribe-client-gpui/src/main.rs#TerminalView#on_bell_signal]] needs: it calls `Window::request_attention`, GPUI's equivalent of the winit client's `request_user_attention(Informational)`. On X11 that sets the `WM_HINTS` urgency flag, which is what makes the routed bell observable from outside the process — see [[test#Visual E2E Tests#Terminal bell attention routing]].

### GPUI Terminal Selection Port

The GPUI rebuild ports the winit client's terminal interaction state — mouse selection, smart selection, vi/copy mode, and regex search — into the `lib` target so the paint path and clipboard reuse the same logic across the cutover.

[[crates/scribe-client-gpui/src/selection.rs]] is a port of the winit [[crates/scribe-client/src/selection.rs]] onto Zed's Alacritty fork. [[crates/scribe-client-gpui/src/selection.rs#SelectionRange]] carries a [[crates/scribe-client-gpui/src/selection.rs#SelectionMode]] (cell/word/line) and normalizes endpoints for hit-testing ([[crates/scribe-client-gpui/src/selection.rs#SelectionRange#contains_cell]]). [[crates/scribe-client-gpui/src/selection.rs#extract_text]] walks the grid trimming trailing spaces and joins `WRAPLINE`-continued rows without a newline; [[crates/scribe-client-gpui/src/selection.rs#word_bounds_at]] and [[crates/scribe-client-gpui/src/selection.rs#line_bounds_at]] follow the same wrap flags across screen rows, and [[crates/scribe-client-gpui/src/selection.rs#pixel_to_grid]] maps pointer pixels to absolute grid lines. [[crates/scribe-client-gpui/src/selection.rs#SelectionState]] drives an interactive drag: the `start_cell`/`start_word`/`start_line` gestures set the granularity, `drag_to` extends by that granularity, and [[crates/scribe-client-gpui/src/selection.rs#SelectionState#copy_text]] yields the selected text for copy-on-select.

[[crates/scribe-client-gpui/src/smart_selection.rs#CompiledSmartSelection]] ports the winit smart-selection matcher: it compiles the configured regex rules, matches the logical line under the cursor ([[crates/scribe-client-gpui/src/smart_selection.rs#CompiledSmartSelection#candidate_at]]), ranks candidates by precision then length, and resolves iTerm2 action parameters — legacy `\0`..`\9` captures plus interpolated `\(matches[N])`/`\(path)` forms — via [[crates/scribe-client-gpui/src/smart_selection.rs#SmartSelectionCandidate#resolved_actions]].

[[crates/scribe-client-gpui/src/search.rs#TerminalSearch]] cribs Zed's regex search: it collects every match across scrollback and the viewport through the fork's `RegexIter` and cycles a highlighted match forward ([[crates/scribe-client-gpui/src/search.rs#TerminalSearch#select_next]]) and backward ([[crates/scribe-client-gpui/src/search.rs#TerminalSearch#select_prev]]) with wraparound — the matcher for a locally-owned `Term`, which the live find surface in [[client#GPUI Find Overlay]] does not use because this client's scrollback lives on the server. [[crates/scribe-client-gpui/src/vi_mode.rs]] wraps the fork's built-in vi mode — [[crates/scribe-client-gpui/src/vi_mode.rs#toggle_vi_mode]], [[crates/scribe-client-gpui/src/vi_mode.rs#vi_motion]], and [[crates/scribe-client-gpui/src/vi_mode.rs#vi_cursor]] — so keyboard copy-mode navigation shares the selection coordinate space.

### GPUI Platform Integrations Port

OS-integration surfaces the GPUI client owns beyond the terminal grid: local server lifecycle, window geometry persistence, the X11 focus guard, and drag-drop path insertion — each a faithful port of the winit helper with a GPUI-native entry point.

[[crates/scribe-client-gpui/src/server_lifecycle.rs#connect_or_start_server]] connects to the frozen local IPC socket, starting the systemd user service ([[crates/scribe-client-gpui/src/server_lifecycle.rs#platform_start_server]], which first syncs the GUI environment into the user manager) and waiting for it to accept. [[crates/scribe-client-gpui/src/server_lifecycle.rs#stale_server_reason]] is the pure decision that flags a connected server whose binary drifted (different path, or rebuilt after the process started); [[crates/scribe-client-gpui/src/server_lifecycle.rs#perform_linux_cold_restart]] is the last-ditch recovery that force-stops the unit and any surviving process, clears the stale IPC and handoff sockets, and starts a fresh server. macOS launchd support is deferred with the rest of the macOS port.

[[crates/scribe-client-gpui/src/window_state.rs#WindowRegistry]] persists one [[crates/scribe-client-gpui/src/window_state.rs#WindowGeometry]] per window under `$XDG_STATE_HOME/scribe/windows/<id>.toml`. [[crates/scribe-client-gpui/src/window_state.rs#normalize_legacy_geometry]] is the first-launch geometry-compat step: geometry saved by the OS-decorated old client would restore mis-inset under the new custom titlebar, so it clamps the size and grows a non-maximized window's height by [[crates/scribe-client-gpui/src/window_state.rs#CUSTOM_TITLEBAR_HEIGHT]] once, recording `titlebar_normalized` so it never runs twice.

[[crates/scribe-client-gpui/src/x11_focus.rs#X11FocusGuard]] polls `_NET_ACTIVE_WINDOW` and suppresses keystrokes while a compositor overlay obscures the window, reading GPUI's Xcb/Xlib window id via [[crates/scribe-client-gpui/src/x11_focus.rs#xcb_window_id]] (per the XID capability spike); non-X11 backends yield no id and leave the guard off. Its suppression timing lives in the pure [[crates/scribe-client-gpui/src/x11_focus.rs#ReactivationDebounce]] so it is testable without a display server.

[[crates/scribe-client-gpui/src/main.rs#TerminalView#new]] starts the guard from the live window-open path: [[crates/scribe-client-gpui/src/main.rs#open_window]] hands it the real `Window`, which is the first point an Xcb window id exists. Three call sites keep it honest — [[crates/scribe-client-gpui/src/main.rs#drive_x11_focus_polls]] refreshes it every [[crates/scribe-client-gpui/src/main.rs#X11_FOCUS_POLL_INTERVAL]] so an overlay opening while the user is idle is noticed before their next keystroke (the GPUI client has no winit-style event-loop tick to piggyback on), [[crates/scribe-client-gpui/src/main.rs#TerminalView#on_activation]] clears the reactivation debounce on a genuine activation because compositor overlays never send one, and [[crates/scribe-client-gpui/src/main.rs#TerminalView#compositor_overlay_active]] is the first gate on the key path — ahead of the overlay router, the bindings, and the PTY encoder — so a key aimed at the overlay lands nowhere in the client. Every dropped keystroke is logged, since silently vanishing input is precisely this guard's failure mode.

[[crates/scribe-client-gpui/src/drag_drop.rs#dropped_path_insertion]] quotes a dropped file path for the focused pane's shell via [[crates/scribe-client-gpui/src/drag_drop.rs#quote_path_for_shell]] (POSIX, fish, PowerShell, or nushell) and appends a trailing space; per FR-013 it bypasses the paste-confirmation gate because the path is already quoted.

#### GPUI Clipboard and OSC 52 Bridge

The GPUI rebuild ports the host clipboard integration — arboard handle, two-hop OSC 52 bridge, Linux primary selection, AI copy-cleanup — into the `lib` target, unit-tested off any display server.

[[crates/scribe-client-gpui/src/clipboard.rs]] speaks to a [[crates/scribe-client-gpui/src/clipboard.rs#ClipboardBackend]] trait so the pure logic runs against an in-memory fake in tests while the shipped [[crates/scribe-client-gpui/src/clipboard.rs#ArboardClipboard]] backend performs the real I/O: on Linux `ClipboardSelection::Primary` routes through arboard's `GetExtLinux`/`SetExtLinux` primary target, and everywhere else (and Wayland, per spec Assumptions) it collapses onto the system clipboard. Handle creation can fail, so a `None` arboard handle reports [[crates/scribe-common/src/protocol.rs#BridgeError]] `Unavailable` for every call.

Spec 010's bridge is verbatim from the winit client's `App` methods: [[crates/scribe-client-gpui/src/clipboard.rs#bridge_read]] services a `ClipboardBridgeReadRequest` and [[crates/scribe-client-gpui/src/clipboard.rs#read_reply]] wraps its `Result` into the outbound `ClientMessage::ClipboardBridgeReadReply`; [[crates/scribe-client-gpui/src/clipboard.rs#bridge_write]] applies the FR-019 [[crates/scribe-client-gpui/src/clipboard.rs#FocusGate]] first, silently dropping a write on an unfocused window when `focus_gate_writes` is on so a background PTY program cannot hijack the clipboard. [[crates/scribe-client-gpui/src/clipboard.rs#prompt_response]] builds the confirmation-overlay `ClipboardPromptResponse`. Middle-click paste reads through [[crates/scribe-client-gpui/src/clipboard.rs#read_primary]], and copy-on-select writes the primary selection through [[crates/scribe-client-gpui/src/clipboard.rs#set_primary]], which runs the [[crates/scribe-client-gpui/src/clipboard_cleanup.rs#prepare_copy_text]] transforms (dedent, blockquote/decoration strip, unwrap) ported byte-for-byte from the winit [[crates/scribe-client/src/clipboard_cleanup.rs]]. The confirmation-dialog view and the App-side selection extraction land with later chrome/dialog beads.

#### GPUI Notification Dispatcher

The GPUI rebuild ports the desktop notification dispatcher so one thread owns one D-Bus connection and `replaces_id` keeps a single toast per session, with click-to-focus decoupled from any concrete UI runtime.

[[crates/scribe-client-gpui/src/notification_dispatcher/mod.rs#spawn_dispatcher]] runs the Linux [[crates/scribe-client-gpui/src/notification_dispatcher/linux.rs]] backend on a dedicated thread (non-Linux platforms fall back to a drop sink; the macOS `notify-rust` path is deferred with the rest of the macOS port). It takes a [[crates/scribe-client-gpui/src/notification_dispatcher/mod.rs#NotifReq]] channel and emits [[crates/scribe-client-gpui/src/notification_dispatcher/mod.rs#NotifOutput]] `FocusSession` on a caller-supplied channel instead of a winit event-loop proxy, so the transport stays runtime-agnostic until the GPUI event bridge lands in a consumer bead.

The `replaces_id` coalescing lives in the pure [[crates/scribe-client-gpui/src/notification_dispatcher/mod.rs#NotifState]]: it tracks the daemon id both ways so [[crates/scribe-client-gpui/src/notification_dispatcher/mod.rs#NotifState#replaces_for]] reuses a session's live toast, [[crates/scribe-client-gpui/src/notification_dispatcher/mod.rs#NotifState#record_shown]] drops a stale reverse mapping when the daemon reallocates, an `ActionInvoked` click routes through [[crates/scribe-client-gpui/src/notification_dispatcher/mod.rs#NotifState#session_for_id]], and `NotificationClosed`/session-exit/shutdown clear state via `on_closed`, `take_session`, and `live_ids`. [[crates/scribe-client-gpui/src/notification_dispatcher/mod.rs#expire_timeout_millis]] maps the config [[crates/scribe-common/src/config.rs#NotifyTimeoutMode]] onto the freedesktop `expire_timeout`. The zbus proxy signature keeps the freedesktop-spec argument count, the sole approved `clippy::too_many_arguments` suppression in the crate.

Unlike the winit client, the GPUI client shares its `zbus` build with GPUI's own Linux platform layer — `accesskit_unix` (AT-SPI), `ashpd` (XDG portals), and `oo7` (secret service) all resolve to the same crate version. Cargo unifies features across the whole graph and zbus's `tokio` feature is compile-time exclusive: with it on, every internal `Task::spawn_blocking` routes through `tokio::task::spawn_blocking`, which panics with "there is no reactor running" when polled outside a Tokio runtime. GPUI drives its zbus connections on its own non-Tokio worker threads, so enabling `tokio` crashed those background threads on every client launch. The workspace `zbus` dependency therefore keeps the default async-io backend, where zbus spawns its own internal executor thread and is runtime-agnostic. The dispatcher thread still builds a single-threaded Tokio runtime for its `tokio::select!` loop and `tokio::sync::mpsc` channels, neither of which requires the zbus transport itself to be Tokio-backed.

### GPUI Animation System

The GPUI client centralises UI motion policy so every shell transition (tab, focus, overlay) and smooth scroll resolves from one place, with a sanctioned off switch for latency purists and deterministic screenshots.

[[crates/scribe-client-gpui/src/animation.rs#AnimationSettings]] resolves the policy from two inputs: the `appearance.animations` config bool (default `true`, added to the frozen [[crates/scribe-common/src/config.rs#AppearanceConfig]], doubling as the reduce-motion user setting) and the `SCRIBE_DISABLE_ANIMATIONS` environment override, which wins when truthy so E2E runs can force motion off. This is the sanctioned exception to the "no new end-user features" Non-Goal, per `specs/016-gpui-client-rebuild/plan.md`.

When motion is enabled, [[crates/scribe-client-gpui/src/animation.rs#AnimationSettings#transition]] builds a `gpui::Animation` clamped to the 150 ms `MAX_TRANSITION` budget with an ease-out curve; GPUI's `AnimationElement` re-reads the animation when a new transition starts mid-flight, so transitions stay interruptible. When motion is disabled, [[crates/scribe-client-gpui/src/animation.rs#AnimationSettings#apply_to_app]] flips GPUI's global `App::set_reduce_motion`, which makes every `with_animation` render its static end state and schedule no frames — the byte-identical-screenshot determinism path. The `scribe-client-gpui` binary resolves and applies the policy at startup; the concrete tab/focus/overlay/scroll surfaces that consume `transition` land with the shell beads.

### GPUI Status Bar Port

The GPUI rebuild ports the winit client's window-level status bar so every ambient-state segment survives the cutover, lowered from the legacy quad renderer onto a GPUI flex row.

The legacy [[crates/scribe-client/src/status_bar.rs#build_status_bar]] emitted `CellInstance` quads into the terminal grid buffer at hand-placed columns. The rebuild splits layout from paint: [[crates/scribe-client-gpui/src/status_bar.rs#build_model]] is a pure function turning [[crates/scribe-client-gpui/src/status_bar.rs#StatusBarData]] into a [[crates/scribe-client-gpui/src/status_bar.rs#StatusBarModel]] of coloured [[crates/scribe-client-gpui/src/status_bar.rs#Span]] groups — left ([[crates/scribe-client-gpui/src/status_bar.rs#build_left]]: connection dot, command/env glyphs, 013/015 remote-control and share-presence surfaces, workspace, CWD), a centred update CTA ([[crates/scribe-client-gpui/src/status_bar.rs#build_center]]), and right ([[crates/scribe-client-gpui/src/status_bar.rs#build_right]]: CPU/MEM/NET/GPU sparklines, git branch, session count, tmux, transport, host, clock). [[crates/scribe-client-gpui/src/status_bar.rs#render]] maps that model onto GPUI elements, letting flex-grown centre space keep the CTA centred instead of the legacy column arithmetic. Colours stay in sRGB via [[crates/scribe-client-gpui/src/status_bar.rs#StatusBarColors#from_theme]] because GPUI does its own linear conversion, unlike the raw-pipeline legacy renderer.

The sparklines are fed by [[crates/scribe-client-gpui/src/sys_stats.rs#SystemStatsCollector]], which ports the winit CPU/memory/network/GPU sampler's readings and rolling history buffers but samples them off the UI thread — see [[client#Client#GPUI Client Spike#GPUI Status Bar Port#Status-Bar Stats Sample Off The UI Thread|the sampling design]]. The `scribe-client-gpui` binary wires the bar into its live view from [[crates/scribe-client-gpui/src/main.rs#TerminalView#render]], driving the connection dot from a shared connected flag, the sparklines from the sampler, and the centred CTA from [[client#GPUI Update Surfaces]].

[[crates/scribe-client-gpui/src/main.rs#TerminalView#build_status_model]] fills the metadata segments from the attached pane's entry in [[client#GPUI Client Spike#Terminal Chrome Metadata|the chrome store]]: workspace name, CWD, git branch, the env-degraded `⚠` glyph, the `tmux:` label, and the host label, which a remote-flagged session context overrides and a local pane leaves at the placeholder until the hostname surface lands. The workspace id comes from the tab strip — the only place the attached pane's workspace is known — and is resolved before the metadata lock is taken so the two are never held at once. The centred update CTA comes from [[client#GPUI Update Surfaces]] instead; command status stays `None` until its own bead lands.

#### Status-Bar Stats Sample Off The UI Thread

The status-bar sampler runs on its own thread and publishes snapshots the UI copies, because the underlying probes are slow enough to dominate startup-to-first-frame if the window waits on them.

The winit client sampled synchronously: `SystemStatsCollector::new()` called `sysinfo`'s `System::new_all()`, which walks every entry in `/proc`. On a busy host (≈1650 processes) that alone cost ~1.4 s, and it ran inside the GPUI view constructor, so the first frame could not paint until it finished — measured startup-to-first-frame was 2.28–3.04 s against the then-current 500 ms budget in `specs/016-gpui-client-rebuild/spec.md`. The old client still pays that cost on every launch: its `load_host_stats` step is ~1.2 s of its 3.4–4.7 s first frame.

Two changes remove it from the critical path. [[crates/scribe-client-gpui/src/sys_stats.rs#stats_refresh_kind]] narrows `sysinfo` to exactly what the bar reads — global CPU usage and RAM totals — so the process table is never enumerated. [[crates/scribe-client-gpui/src/sys_stats.rs#spawn_sampler]] then moves all probing onto a named background thread: [[crates/scribe-client-gpui/src/sys_stats.rs#Sampler]] owns the `sysinfo` handles and the GPU probe, refreshes on the same 2 s interval, and publishes each result into a shared slot. That also gets the per-sample network and GPU reads (the AMD sysfs poll, or an `nvidia-smi` spawn where sysfs is absent) off the frame path they previously ran on.

[[crates/scribe-client-gpui/src/sys_stats.rs#SystemStatsCollector#maybe_refresh]] now only adopts the newest published snapshot under an uncontended lock, so it stays cheap enough to call every frame. Construction is non-blocking and returns zeroed stats, so the bar shows empty segments for the few milliseconds before the first background sample lands — a deliberate trade against blocking first paint. Dropping the collector clears the flag the sampler polls every [[crates/scribe-client-gpui/src/sys_stats.rs#STOP_POLL_INTERVAL]], so the thread exits promptly with its window.

The residual startup cost is no longer in client code, and the client now measures that directly: [[crates/scribe-client-gpui/src/main.rs#WINDOW_BRINGUP_MS_BITS]] times `cx.open_window` and [[crates/scribe-client-gpui/src/main.rs#log_first_frame_timing]] splits the first-frame span into `gpu_bringup_ms` and `scribe_startup_ms`. Measured on the reference host, first paint lands at 634–780 ms, of which 610–751 ms is wgpu adapter enumeration and driver bring-up inside `cx.open_window` and only 24–29 ms is Scribe's own work. That floor is why the absolute 500 ms budget was retired — see [[test#GPUI Perf A/B Gate#Startup instrumentation]].

## GPUI Update Surfaces

The terminal window learns about an available update from the server, renders it as the centred status-bar CTA, and sends the user's decision back — the `UpdateAvailable` / `UpdateProgress` / `TriggerUpdate` / `DismissUpdate` quartet, live.

[[crates/scribe-client-gpui/src/update.rs#UpdateState]] holds the latest of each broadcast behind one mutex shared between the IPC reader thread and the GPUI view, mirroring the winit client's `handle_update_available` / `handle_update_progress` pair. [[crates/scribe-client-gpui/src/main.rs#dispatch_server_message]] writes it from explicit `UpdateAvailable` and `UpdateProgress` arms through [[crates/scribe-client-gpui/src/main.rs#update_update_state]], which bumps the shared redraw generation so the bar repaints; [[crates/scribe-client-gpui/src/main.rs#TerminalView#build_status_model]] reads it straight into [[crates/scribe-client-gpui/src/status_bar.rs#StatusBarData]]'s `update_available` / `update_progress`, which is what [[crates/scribe-client-gpui/src/status_bar.rs#build_center]] lowers into the CTA label. A progress state is deliberately not cleared by a later announcement, matching the winit client.

The CTA is a real control, not a label: [[crates/scribe-client-gpui/src/status_bar.rs#render]] takes an optional [[crates/scribe-client-gpui/src/status_bar.rs#UpdateClickHandler]] and attaches it only when `center_clickable` is set, so an actionable CTA gets a pointer cursor and an accent hover tint while "Downloading..." / "Update failed" stay inert. Clicking it runs [[crates/scribe-client-gpui/src/main.rs#TerminalView#open_update_dialog]], which asks [[crates/scribe-client-gpui/src/update.rs#UpdateState#confirmation]] for the modal to raise — the restart-required flow outranks a pending version, exactly as the winit `open_update_dialog` match does.

[[crates/scribe-client-gpui/src/main.rs#TerminalView#route_update_action]] resolves the modal's choice. Confirming an install sends [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#trigger_update]] and clears the pending version so the CTA stops offering it, with the server's own `UpdateProgress` taking over the label; declining (the "Later" button, Esc, or a backdrop click, all of which resolve to the safe [[crates/scribe-client-gpui/src/dialog.rs#AnyDialog#cancel]] action) sends [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#dismiss_update]] and clears the whole state, because the server then suppresses re-notification for that version. The restart-required flow's "Continue" would spawn a platform cold-restart helper, which the GPUI shell does not host yet, so it is logged rather than silently swallowed.
## GPUI Window Lifecycle

The window's own lifecycle is live: the WM's close button raises the in-app close dialog, whose answer asks the server to kill this window or quit them all, and only the server's ack exits. The client also reports focus and polls the window list.

[[crates/scribe-client-gpui/src/window_lifecycle.rs#WindowLifecycle]] is the one piece of state the two threads share for this, behind a mutex like the AI, chrome, share and update stores. It holds the window id from `Welcome` (adopted by [[crates/scribe-client-gpui/src/main.rs#on_welcome]] alongside the reader's own registry copy), the in-flight [[crates/scribe-client-gpui/src/window_lifecycle.rs#PendingShutdown]], the acknowledged [[crates/scribe-client-gpui/src/window_lifecycle.rs#ExitReason]], the controller list projected out of the last `WindowList`, and the focus last reported. Every decision in it is pure, so the request/acknowledge rules are unit-tested without a window; see [[test#GPUI Client Headless Suites#GPUI Window Lifecycle]].

Closing starts at [[crates/scribe-client-gpui/src/main.rs#TerminalView#request_window_close]], which is registered on the platform window's close hook in [[crates/scribe-client-gpui/src/main.rs#TerminalView#new]] and always vetoes the platform close: the server owns this window's sessions and has to be told what to do with them. The shell-owned close chord ([[crates/scribe-client-gpui/src/keybindings.rs#OverlayChord]]::`CloseDialog`) raises the same dialog through the same call, so the in-app command and the WM button can never drift apart. [[crates/scribe-client-gpui/src/main.rs#TerminalView#route_close_action]] then answers it — "Quit Scribe" sends [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#quit_all]], "Kill Window" sends [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#close_window]] naming the adopted id, and Cancel (which Escape and a backdrop click also resolve to) does nothing. Neither request exits anything: [[crates/scribe-client-gpui/src/main.rs#on_window_lifecycle_message]] folds the server's `QuitRequested` or matching `WindowClosed` onto the shared state, and [[crates/scribe-client-gpui/src/main.rs#TerminalView#poll_window_lifecycle]] drains it on the GPUI thread and quits the app there. A `WindowClosed` naming a window this client never asked about is ignored, matching the winit client — an unrelated ack must not close a live window.

Focus reporting has two producers and one chokepoint. [[crates/scribe-client-gpui/src/main.rs#TerminalView#on_activation]] is the focus observer registered on the live window, and the lifecycle tick reconciles pane changes the IPC reader caused (a reattach moves the focused pane with no UI event behind it); both call [[crates/scribe-client-gpui/src/main.rs#TerminalView#report_focus]], where [[crates/scribe-client-gpui/src/window_lifecycle.rs#WindowLifecycle#focus_change]] collapses "is the window active" and "which pane is attached" into one gained/lost pair and drops the report when nothing moved. That pair leaves through [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#focus_changed]], which is how the server knows to relay CSI focus events to PTY applications that enabled DECSET 1004.

The window-list poll rounds it out: [[crates/scribe-client-gpui/src/main.rs#TerminalView#poll_window_list]] sends [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#list_windows]] on the same tick, throttled to [[crates/scribe-client-gpui/src/main.rs#WINDOW_LIST_POLL_INTERVAL]] and gated on `remote.enabled` exactly as the winit client gates it, because the reply's only rendered consumer is the status bar's owning-machine remote-control summary. The reply lands in the same shared state and [[crates/scribe-client-gpui/src/main.rs#TerminalView#build_status_model]] reads it into [[crates/scribe-client-gpui/src/status_bar.rs#RemoteStatusData]], whose `enabled` and `controllers` were hardcoded off before.

The whole surface is verified against the running app, not headlessly: see [[test#Visual E2E Tests#Window lifecycle over the wire]].

## GPUI Window Chrome Layout

The terminal window is a flex column of chrome bands around one flex-grown grid, and its startup size is derived from that stack rather than hardcoded — so the whole grid and every band land on screen at once.

[[crates/scribe-client-gpui/src/main.rs#TerminalView#render]] stacks, top to bottom: the [[client#GPUI Titlebar|titlebar]], the terminal grid, the optional [[client#Client#GPUI Prompt Bar|prompt strip]], the one-line pane status strip, and the window [[client#Client#GPUI Client Spike#GPUI Status Bar Port|status bar]]. Only the grid is flex-grown, so every pixel the chrome takes is a pixel the grid does not get.

The window used to open at a hardcoded 960x680, which was the painted height of the 36-row grid (36 x 18.9 px) and nothing else. The 84 px of titlebar, status strip and status bar therefore came out of the grid: the bottom five rows were clipped away, and because each band was flex-*shrinkable* under a flex-grown grid, a shorter window would have squeezed the bands themselves rather than the grid.

[[crates/scribe-client-gpui/src/window_chrome.rs]] is now the single place the band heights are stated. [[crates/scribe-client-gpui/src/window_chrome.rs#chrome_height]] sums the titlebar, [[crates/scribe-client-gpui/src/window_chrome.rs#STATUS_STRIP_HEIGHT]] and [[crates/scribe-client-gpui/src/window_chrome.rs#STATUS_BAR_HEIGHT]] (GPUI lays divs out border-box, so each band's hairline is inside its own number), and [[crates/scribe-client-gpui/src/window_chrome.rs#default_window_size]] adds the grid's own extent at the metrics it is *painted* with — the live `GridFont`, not the integer cell size reported to the server, because the painted metrics decide where the last row lands. [[crates/scribe-client-gpui/src/main.rs#startup_window_size]] resolves that from the live `[appearance]` config and hands it to `Bounds::centered`, so a font-size change moves the default window instead of silently clipping more rows. At the shipped defaults that is 1008x765 rather than 960x680.

Two guards keep the derivation honest. [[crates/scribe-client-gpui/src/window_chrome.rs#clamp_to_display]] shrinks the request to the primary display, because a `font_size = 72` window taller than the screen would move the status bar off the *desktop* instead of off the window — the same defect one level up. And every band now carries `flex_none` ([[crates/scribe-client-gpui/src/titlebar.rs#TitlebarView#render]], the status strip in `render`, [[crates/scribe-client-gpui/src/status_bar.rs#render]], [[crates/scribe-client-gpui/src/prompt_bar.rs#render]]), so a user-shrunk window clips the grid — the surface that can afford it — instead of squeezing the status surfaces away.

The prompt strip is deliberately excluded from the reserved height: it exists only while the attached pane has prompts, so reserving its rows up front would leave a permanent dead band under the grid. When it appears it takes its rows from the grid and the bands below it stay put. Verified on the running app by [[test#Visual E2E Tests#Window chrome bands stay on screen]].

## GPUI LAN Surface

The feature-014 LAN surface is live in the terminal window: an unknown device's approval prompt is raised and answered, the machine's own LAN environment and peers are probed, and `SCRIBE_LAN_DIAL` reaches a peer over mutual TLS.

[[crates/scribe-client-gpui/src/lan.rs#LanChrome]] is the one piece of state the IPC reader and the GPUI view share for all of it, behind a mutex like the AI, chrome, share, update and lifecycle stores. It holds the parked approval prompt, the last `LanPeerList`, the [[crates/scribe-client-gpui/src/lan.rs#LanEnvSummary]] from the last `LanEnv`, and the [[crates/scribe-client-gpui/src/lan.rs#LanDialStatus]] of this client's own dial, and derives the one-line [[crates/scribe-client-gpui/src/lan.rs#LanChrome#status_line]] the status bar shows.

### Owning side: the approval prompt

The owning server holds an unknown device — revealing nothing — and pushes `LanApprovalRequest` to its own local client, which raises the prompt and answers it.

[[crates/scribe-client-gpui/src/main.rs#on_lan_message]] builds the ported [[crates/scribe-client-gpui/src/lan_approval.rs#LanApprovalDialog]] and *parks* it rather than rendering it, because a GPUI entity may only be built on the thread that owns the window; [[crates/scribe-client-gpui/src/main.rs#TerminalView#poll_lan_approval]] takes it on the same 200 ms lifecycle tick that drains an acknowledged exit and raises it as [[crates/scribe-client-gpui/src/dialog.rs#AnyDialog]]`::LanApproval`.

Wrapping the ported model in the generic modal is what gives the prompt the backdrop, Tab/Shift+Tab cycling and click activation every other dialog already has, without a second dialog implementation. Two behaviours are deliberate: Approve carries the destructive button tone because it writes a `TrustedDevice` and admits a machine that has so far been shown nothing, and Esc or a backdrop click resolves — through [[crates/scribe-client-gpui/src/dialog.rs#AnyDialog#cancel]] — to an explicit **Decline** that is still sent, because the peer's connection is held open until the `request_id` is answered. [[crates/scribe-client-gpui/src/main.rs#TerminalView#route_lan_approval_action]] puts that answer on the wire through [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#lan_approval_decision]].

### Startup probe: environment and peers

At startup the client asks its own server what its LAN surface looks like: this device's identity fingerprint and current-network addability, plus the peers discovered on that network.

[[crates/scribe-client-gpui/src/main.rs#probe_lan_env]] runs before the session connection is opened — `GetLanEnv` is a pre-`Hello` first frame answered on its own transient socket, so it has to be a separate connection anyway, and probing first means the window has its LAN summary before the first frame paints. [[crates/scribe-client-gpui/src/main.rs#adopt_lan_surface]] then folds the reply through the same [[crates/scribe-client-gpui/src/main.rs#on_lan_message]] the live reader uses and sends [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#list_lan_peers]] on the session connection, which the server answers only for a local one. Both are gated on `remote.lan.enabled`, exactly as the window-list poll is gated on `remote.enabled`.

### Connecting side: the mutual-TLS dial

With `SCRIBE_LAN_DIAL` set, [[crates/scribe-client-gpui/src/main.rs#run_connection]] reaches a peer over TCP and pinned mutual TLS instead of the local Unix socket, gated by the owning side's device approval.

[[crates/scribe-client-gpui/src/lan_dial.rs#LanDialer#build]] fetches this machine's device identity from its co-located server over the local-socket-only `GetLanDialIdentity` — the sealed key is granted only to the binary that created it, so the client must never read the keyring itself — and builds the dialer from the server-owned `LanTls`, which keeps the SPKI-pinning verifier in one place rather than duplicating it client-side.

[[crates/scribe-client-gpui/src/lan_dial.rs#handshake]] then sends `LanHello` and reads the owning side's approval gate: an unknown device is answered `LanApprovalPending` and held with no timeout of our own (the owning user's decision legitimately takes as long as it takes, and the peer already bounds the hold), which surfaces as "Waiting for approval on the peer…"; a trusted device is admitted straight to `LanApprovalResult { approved: true }`. Anything short of acceptance ends the process rather than falling back to the local server, because silently attaching the user to the wrong machine is worse than not connecting. Past the gate the encrypted stream is interchangeable with the Unix socket, so both transports converge on [[crates/scribe-client-gpui/src/main.rs#serve_connection]].

`LanDialIdentity` carries private key material, so it is never stored in shared state and never logged: [[crates/scribe-client-gpui/src/main.rs#on_lan_message]]'s arm for one arriving out of band on the session connection logs only the presence flag.

The whole surface is verified against the running app, not headlessly: see [[test#Visual E2E Tests#LAN approval and mutual-TLS dial]].

## GPUI Titlebar

The GPUI rebuild replaces native window decorations with a custom titlebar that also hosts the integrated tab bar. The pure layout/decay math is ported into a testable module; the interactive chrome is a `gpui::Entity`.

[[crates/scribe-client-gpui/src/tab_bar.rs]] holds the display-independent logic ported from the winit [[crates/scribe-client/src/tab_bar.rs]] — the self-decaying attention-flash envelope ([[crates/scribe-client-gpui/src/tab_bar.rs#tab_flash_intensity]], additively blended by [[crates/scribe-client-gpui/src/tab_bar.rs#flash_blend]] without touching alpha), fixed-width title truncation ([[crates/scribe-client-gpui/src/tab_bar.rs#tab_display_title]]), the colored context-% suffix banding with pulse suppression ([[crates/scribe-client-gpui/src/tab_bar.rs#context_suffix]]), the workspace-badge gate ([[crates/scribe-client-gpui/src/tab_bar.rs#badge_label]]), and the drag-reorder slot math ([[crates/scribe-client-gpui/src/tab_bar.rs#reorder_target_index]], walking tab edges rather than an `f32`→`usize` cast). Colors stay sRGB in [[crates/scribe-client-gpui/src/tab_bar.rs#TabBarColors]] because GPUI performs its own sRGB→linear conversion at paint time.

[[crates/scribe-client-gpui/src/titlebar.rs#TitlebarView]] is the `gpui::Entity` that assembles the chrome as `div` elements: a `WindowControlArea::Drag` move region, the workspace-badge pill, the tab strip (active accent underline, per-tab close button revealed on hover, AI activity dot, context-% suffix, and drag-reorder slide), the equalize and gear icons, and the min/maximize/close window controls. Each interaction mutates state and emits a [[crates/scribe-client-gpui/src/titlebar.rs#TitlebarEvent]] the shell acts on; [[crates/scribe-client-gpui/src/titlebar.rs#TitlebarView#update_drag]] reorders tabs live as the cursor crosses a neighbour, and the window controls drive the platform window through GPUI's `WindowControlArea` hit regions. [[crates/scribe-client-gpui/src/titlebar.rs#pane_title_pill]] builds the semi-transparent per-pane title pill the shell overlays on a split pane; [[crates/scribe-client-gpui/src/titlebar.rs#WindowControlKind]] names the three window-control buttons. The spike wires the titlebar above the terminal grid in [[crates/scribe-client-gpui/src/main.rs#TerminalView]] so the visual E2E harness (`tests/e2e/visual/titlebar.sh`) can screenshot the assembled bar and its interaction checklist.

### Tab flash envelope self-decays

Verifies [[crates/scribe-client-gpui/src/tab_bar.rs#tab_flash_intensity]] peaks at 1.0, eases down mid-envelope, and returns `None` at or past `TAB_FLASH_SECS` (and for negative/NaN inputs) so the flash self-clears and cannot pin the redraw loop.

### Flash blends accent without touching alpha

Verifies [[crates/scribe-client-gpui/src/tab_bar.rs#flash_blend]] returns the base color unchanged for `None`, mixes toward the accent by `FLASH_MAX_MIX` at peak intensity, and preserves the base alpha channel.

### Titles truncate with an ellipsis

Verifies [[crates/scribe-client-gpui/src/tab_bar.rs#tab_display_title]] leaves short titles intact, truncates an overflowing title to exactly the available columns ending in an ellipsis, and flags truncation (driving the tooltip hover target).

### Context suffix bands and suppression

Verifies [[crates/scribe-client-gpui/src/tab_bar.rs#context_suffix]] returns `None` below the warn threshold, the warn color at the threshold, the danger color above the danger threshold, and `None` while the session is pulsing so it never competes with the attention pulse.

### Badge shown only for named multi-workspace

Verifies [[crates/scribe-client-gpui/src/tab_bar.rs#badge_label]] shows a badge only for a named workspace in multi-workspace mode, and hides it for a single workspace or an empty name.

### Drag reorder resolves the target slot

Verifies [[crates/scribe-client-gpui/src/tab_bar.rs#reorder_target_index]] walks tab edges to the hovered slot, clamps below the first and past the last tab, and treats an empty tab list as a no-op.

### Column-to-pixel conversion saturates

Verifies [[crates/scribe-client-gpui/src/tab_bar.rs#px_units]] converts small counts exactly and saturates at `u16::MAX` for pathological inputs, keeping the strict cast lints satisfied without an `as` cast.

### Selecting a tab activates it and emits

Verifies [[crates/scribe-client-gpui/src/titlebar.rs#TitlebarView#select]] marks the clicked tab active (clearing the others) and emits `TitlebarEvent::SelectTab`.

### Closing a tab removes it and reactivates

Verifies [[crates/scribe-client-gpui/src/titlebar.rs#TitlebarView#close]] removes the tab, keeps exactly one tab active when the active tab is closed, and emits `TitlebarEvent::CloseTab`.

### Drag reorder moves the tab and emits

Verifies a `begin_drag`/`update_drag`/`end_drag` sequence on [[crates/scribe-client-gpui/src/titlebar.rs#TitlebarView]] moves the dragged tab to the hovered slot and emits `TitlebarEvent::ReorderTab`.

### Out-of-range interactions are no-ops

Verifies that select, close, and begin-drag on out-of-range indices leave the tab list unchanged and emit no events, so stray hit targets cannot corrupt state.

## GPUI AI Indicator

The GPUI rebuild ports the winit client's per-session AI state machine so pulsing pane borders, tab indicators, and the context store behave identically across the cutover. The state machine is pure and covered by `#[gpui::test]`.

[[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker]] is a byte-for-byte port of the winit [[crates/scribe-client/src/ai_indicator.rs]] tracker. It keeps the Layer-1 pulse envelope (attention states pulse for a bounded window from entry; `Processing` pulses only while alive, re-armed by state edges and PTY output via [[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#note_activity]]) so a hung AI stops pinning the redraw loop ([[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#needs_animation]]), the Layer-2 wall-clock [[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#clear_stale_processing]] that removes a dead `Processing` state entirely, the keystroke-driven [[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#clear_attention_states]], and the workspace-level priority aggregation ([[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#workspace_border_color]], `PermissionPrompt > WaitingForInput > IdlePrompt > Error > Processing`).

The context-window percent is stored independently of the visible state ([[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#context_for]]) so it survives every state-pruning path; the pulse-suppression predicate ([[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#context_suffix_suppressed]]) is the `pulsing` argument for the tab suffix banding that now lives in [[crates/scribe-client-gpui/src/tab_bar.rs#context_suffix]]. The pulsing border geometry is [[crates/scribe-client-gpui/src/ai_indicator.rs#pane_border_edges]], which excludes the tab bar and reuses the shared [[crates/scribe-client-gpui/src/focus_border.rs#border_edges]] strip math; the GPUI paint path fills those rects with the aggregated colour. `AiStateChanged`/`AiStateCleared` are verified by the visual-E2E harness.

### Provider toggle gates the indicator

Verifies [[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#tab_indicator_color]] returns `None` for a provider disabled in `TerminalConfig`, so a toggled-off tool shows no indicator.

### Provider memory survives clears

Verifies a Codex session is remembered as a Codex provider (not Claude) so provider-aware clipboard cleanup never mistakes it for Claude Code.

### Processing pulse rests after idle window

Verifies a fresh `Processing` state pulses, then after `PROCESSING_IDLE_PULSE_SECS` of silence [[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#needs_animation]] reports idle so the shared redraw loop retires — the GPU-drain fix.

### Activity re-arms the processing pulse

Verifies fresh PTY output via [[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#note_activity]] re-arms a rested `Processing` pulse, which rests again after renewed silence.

### A state edge re-arms the pulse

Verifies a repeated `Processing` state edge (an `update`) is treated as a sign of life and re-arms a rested pulse.

### Attention pulse rests after its window

Verifies an attention state (`WaitingForInput`) pulses for a bounded window measured from entry, then rests without being extended by later activity.

### Stale processing is cleared

Verifies [[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#clear_stale_processing]] removes a `Processing` state with no liveness for `STALE_PROCESSING_CLEAR`, while preserving provider memory for clipboard cleanup.

### Fresh processing is not cleared

Verifies a just-updated `Processing` state is not treated as stale and stays tracked.

### Only processing is hard-cleared

Verifies an idle attention state is never hard-cleared by [[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#clear_stale_processing]] — it must persist until the human acts.

### Activity re-arms the stale-clear timer

Verifies [[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#note_activity]] resets the wall-clock staleness timer so a sign of life before the prune spares the state.

### Workspace border takes the highest-priority state

Verifies [[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#workspace_border_color]] aggregates several sessions to the highest-priority state's colour (`PermissionPrompt` over `WaitingForInput` and `Processing`).

### Border colour drops decayed sessions

Verifies [[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#workspace_border_color]] returns `None` when no tracked session drives a border.

### Context survives the stale-processing clear

Verifies [[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#context_for]] still returns the percent after a stale-`Processing` clear removes the visible state.

### Context suffix suppressed during attention pulse

Verifies [[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#context_suffix_suppressed]] is true for `PermissionPrompt`/`WaitingForInput` and false for `Processing`, so the tab suffix yields to a pulsing attention state.

### Conversation change wipes the context

Verifies [[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#clear_context]] drops the stored percent so a new conversation does not show the prior window's usage.

### Session removal drops the context

Verifies [[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker#remove]] clears the stored context percent for a closed session.

### Pane border edges exclude the tab bar

Verifies [[crates/scribe-client-gpui/src/ai_indicator.rs#pane_border_edges]] offsets the border below the tab bar and produces corner-safe top/bottom/left/right strips.

## GPUI Prompt Bar

The GPUI rebuild ports the winit prompt bar's display-independent logic — elapsed-timer formatting with freeze-on-AI-stop, the segmented context meter, the `#N` count, strip height, and truncation — and lowers the visuals onto a GPUI flex strip.

[[crates/scribe-client-gpui/src/prompt_bar.rs#build_model]] turns a [[crates/scribe-client-gpui/src/prompt_bar.rs#PromptBarData]] snapshot into a pure [[crates/scribe-client-gpui/src/prompt_bar.rs#PromptBarModel]] (first/latest rows, count, elapsed, optional meter); [[crates/scribe-client-gpui/src/prompt_bar.rs#render]] lowers it onto div rows (timer on row 1 with count/context on row 2 in the two-prompt state, everything on row 1 otherwise, plus the hover dismiss overlay). The elapsed timer is computed by [[crates/scribe-client-gpui/src/prompt_bar.rs#elapsed_text]], which freezes at `latest_prompt_finished_at` when the AI stops and clamps a backwards wall clock; the reference clock is threaded in so the freeze is `#[gpui::test]`-verifiable without a live window. [[crates/scribe-client-gpui/src/prompt_bar.rs#is_prompt_truncated]] gates the hover tooltip and [[crates/scribe-client-gpui/src/prompt_bar.rs#prompt_bar_height]] sizes the strip. The rendered strip is a visual-E2E surface.

The meter text itself comes from [[common#AI Context Chrome]] so the prompt bar, the tab suffix, and the E2E assertions share one spelling; [[crates/scribe-client-gpui/src/prompt_bar.rs#PromptContextIndicator#from_thresholds]] colors it by the configured band and falls back to the bar's text color when a band hex fails to parse, degrading the color rather than hiding the percentage.

### Live AI wiring

The GPUI client feeds the bar from the IPC reader: `AiStateChanged`, `AiStateCleared`, and `PromptReceived` land in a shared AI chrome record that the view reads on every frame.

`AiStateChanged` updates an [[crates/scribe-client-gpui/src/ai_indicator.rs#AiStateTracker]] (whose decoupled context store keeps the percentage alive across pulse pruning), `PromptReceived` appends to the pane's [[crates/scribe-client-gpui/src/prompt_bar.rs#PromptBarData]], and `AiStateCleared` plus `SessionExited` drop both so a closed pane leaves no stale percentage behind. Each mutation bumps the redraw generation, so the strip repaints without polling. On render the view builds the model with a context indicator whenever the tracker holds a percentage — the prompt bar is the surface that always shows the Ok band — and separately pushes the warn-and-above tab suffix from [[crates/scribe-client-gpui/src/tab_bar.rs#context_suffix]] onto the active tab. A poisoned chrome mutex is dropped with a warning rather than propagated, because losing an indicator update must never tear down the reader and with it the pane's terminal output.

### Elapsed formats span sec, minute, and hour bands

Verifies [[crates/scribe-client-gpui/src/prompt_bar.rs#format_elapsed]] renders `"X sec"` under a minute, `"Xm YYs"` under an hour, and `"Xh YYm"` beyond, with zero-padded trailing units.

### Elapsed timer tracks now until the AI stops

Verifies [[crates/scribe-client-gpui/src/prompt_bar.rs#elapsed_text]] advances with `now` while `latest_prompt_finished_at` is unset.

### Elapsed timer freezes when the AI stops

Verifies [[crates/scribe-client-gpui/src/prompt_bar.rs#elapsed_text]] holds at the prompt-to-finish duration once `latest_prompt_finished_at` is set, regardless of how far `now` advances.

### Elapsed clamps a backwards wall clock

Verifies [[crates/scribe-client-gpui/src/prompt_bar.rs#elapsed_text]] clamps to `0 sec` when `now` precedes the prompt timestamp (DST/NTP skew) rather than underflowing.

### No timer without a prompt timestamp

Verifies [[crates/scribe-client-gpui/src/prompt_bar.rs#elapsed_text]] returns `None` when no prompt timestamp is recorded, so nothing is drawn.

### Context meter fills and clamps

Verifies [[crates/scribe-client-gpui/src/prompt_bar.rs#format_context_label]] fills the three-segment meter proportionally and clamps above 100%.

### Strip height tracks the prompt count

Verifies [[crates/scribe-client-gpui/src/prompt_bar.rs#prompt_bar_height]] is zero for no prompts or a non-positive cell height, one row for one prompt, and two rows plus a seam for two or more.

### Model shows one row for one prompt, two for many

Verifies [[crates/scribe-client-gpui/src/prompt_bar.rs#build_model]] emits only the first row for a single prompt and both rows (with the `#N` count) for multiple, and `None` for zero prompts.

### Truncation predicate gates the hover tooltip

Verifies [[crates/scribe-client-gpui/src/prompt_bar.rs#is_prompt_truncated]] reports a short prompt as fitting a wide bar and a long prompt as overflowing a narrow one.

## GPUI Find Overlay

Find-in-scrollback is the client surface that is nothing but a server round trip: the client is display-only, so the scrollback being searched lives on the server and every query travels as `SearchRequest` and returns as `SearchResults`.

[[crates/scribe-client-gpui/src/search.rs#FindOverlayView]] is the overlay itself, a port of the winit [[crates/scribe-client/src/search_overlay.rs#SearchOverlay]] with its quad painter replaced by GPUI elements: a top-right box carrying a `Find  n/m` header and a `/query` field. It owns no session and no sink, so an edit — [[crates/scribe-client-gpui/src/search.rs#FindOverlayView#push_str]], [[crates/scribe-client-gpui/src/search.rs#FindOverlayView#pop_char]], [[crates/scribe-client-gpui/src/search.rs#FindOverlayView#clear_query]] — clears the stale matches and emits [[crates/scribe-client-gpui/src/search.rs#FindOverlayEvent]]`::QueryChanged` rather than searching anything. Clearing before the reply lands is deliberate: the grid must never contradict the query field, even for one frame.

The shell closes the loop. [[crates/scribe-client-gpui/src/keybindings.rs#KeyAction]]`::OpenFind` reaches [[crates/scribe-client-gpui/src/main.rs#TerminalView#open_find_overlay]] through the shared [[crates/scribe-client-gpui/src/main.rs#TerminalView#dispatch_key_action]], so the find chord and the palette's "Find in Scrollback" row open the same surface; a query edit lands in [[crates/scribe-client-gpui/src/main.rs#TerminalView#send_search_request]], which lowers it onto [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#search_request]] for the attached pane at [[crates/scribe-client-gpui/src/search.rs#SEARCH_RESULT_LIMIT]]. The reply is folded in by [[crates/scribe-client-gpui/src/main.rs#on_search_results]] on the IPC reader thread, which stores it in [[crates/scribe-client-gpui/src/search.rs#FindResults]] behind the same kind of mutex the chrome and share stores use and bumps the repaint generation. [[crates/scribe-client-gpui/src/main.rs#TerminalView#sync_find_results]] carries it across into the entity on the next redraw, and [[crates/scribe-client-gpui/src/search.rs#FindOverlayView#adopt_results]] drops any reply whose query the user has already typed past — with per-keystroke requests in flight, a late answer would otherwise highlight the wrong thing.

While the overlay is up it owns the keyboard: [[crates/scribe-client-gpui/src/main.rs#TerminalView#handle_find_overlay_key]] consumes every keystroke (Escape closes, Enter / Shift+Enter and the arrows cycle, Backspace and Delete edit, printable characters extend), so nothing leaks to the PTY and the find chord cannot reopen the overlay underneath itself. It also notifies the *shell* view rather than only the overlay, because the match highlights are painted by the grid and not by the box.

Highlighting runs through the cell-accurate paint path. [[crates/scribe-client-gpui/src/search.rs#visible_highlights]] projects the server's absolute grid rows onto the painted viewport, dropping matches in scrollback instead of clamping them onto rows they do not occupy, and [[crates/scribe-client-gpui/src/terminal_element.rs#TerminalElement#with_highlights]] folds the resulting [[crates/scribe-client-gpui/src/search.rs#MatchHighlight]] spans into the per-cell colours the paint resolve step already computes. [[crates/scribe-client-gpui/src/search.rs#MatchHighlightColors]] reproduces the winit rule: the current match takes the opaque accent with a luminance-chosen contrast foreground, and every other match blends the accent into its own background at 40% so it stays legible over coloured output.

## GPUI Overlays

The GPUI rebuild ports the three interactive overlays — command palette, right-click context menu, and hover tooltip — as `gpui::Entity` views with rounded corners, drop shadows, and hover/pressed states, replacing the winit quad painters.

[[crates/scribe-client-gpui/src/command_palette.rs#CommandPaletteView]] folds the winit palette state and the `main.rs` entry machinery into one entity. The pure assembly stays testable: [[crates/scribe-client-gpui/src/command_palette.rs#base_entries]] holds the fixed action rows (including the feature-013 client-local "Connect to remote machine…" row), [[crates/scribe-client-gpui/src/command_palette.rs#profile_entries]] builds the "Switch Profile" rows tagging the active one, [[crates/scribe-client-gpui/src/command_palette.rs#build_entries]] appends the conditional update row, and [[crates/scribe-client-gpui/src/command_palette.rs#filter_entries]] applies the case-insensitive substring filter. Typing and [[crates/scribe-client-gpui/src/command_palette.rs#CommandPaletteView#push_str]] paste (control characters stripped) drive the filter; the wrapping selection and [[crates/scribe-client-gpui/src/command_palette.rs#CommandPaletteView#confirm]] emit a [[crates/scribe-client-gpui/src/command_palette.rs#PaletteAction]] via [[crates/scribe-client-gpui/src/command_palette.rs#CommandPaletteEvent]] for the shell to route (the winit `execute_automation_action` seam).

[[crates/scribe-client-gpui/src/context_menu.rs#ContextMenuView]] ports the right-click menu. [[crates/scribe-client-gpui/src/context_menu.rs#build_menu_items]] assembles the ordered rows verbatim: the Copy/Paste/Select-All head (Copy gated on a selection), the OSC 8 "Open URL" precedence and appended "Copy hyperlink address" entry (spec 009 FR-003 / FR-007), the file row, and the smart-selection actions resolved through [[crates/scribe-client-gpui/src/context_menu.rs#smart_selection_menu_item]]. Clicking an enabled row runs [[crates/scribe-client-gpui/src/context_menu.rs#ContextMenuView#activate]] (emitting a [[crates/scribe-client-gpui/src/context_menu.rs#ContextMenuAction]] on [[crates/scribe-client-gpui/src/context_menu.rs#ContextMenuEvent]]); Escape or a backdrop click runs [[crates/scribe-client-gpui/src/context_menu.rs#ContextMenuView#dismiss]].

[[crates/scribe-client-gpui/src/tooltip.rs#tooltip_element]] draws the hover tooltip, sizing and positioning it from the pure geometry ports: [[crates/scribe-client-gpui/src/tooltip.rs#clamp_tooltip_x]] centres the box on the anchor and clamps it inside the viewport, [[crates/scribe-client-gpui/src/tooltip.rs#tooltip_y]] picks above/below, and [[crates/scribe-client-gpui/src/tooltip.rs#truncate_url]] head+tail-elides a long URI (spec 009 FR-006). The spike wires all three into [[crates/scribe-client-gpui/src/main.rs#TerminalView]] — Ctrl+Shift+P opens the palette, a right-click opens the menu, Ctrl+Shift+U toggles the tooltip demo — so the visual E2E harness (`tests/e2e/visual/overlays.sh`) can screenshot each overlay and its interaction checklist.

#### Overlay Chords Yield To Bindings

Surfaces with no `KeybindingsConfig` field of their own are opened from a fixed chord the shell hard-codes, and a hard-coded chord must never shadow a configured action.

[[crates/scribe-client-gpui/src/keybindings.rs#OVERLAY_CHORDS]] is that table — the tooltip demo, the close dialog, the clipboard dialog, and the workspace-notes modal — and [[crates/scribe-client-gpui/src/keybindings.rs#translate_overlay_chord]] resolves a keystroke against it *after* [[crates/scribe-client-gpui/src/keybindings.rs#translate_key_action]] has had first refusal, returning `None` whenever a binding claims the key. The precedence is load-bearing because [[crates/scribe-client-gpui/src/main.rs#TerminalView#claim_shell_chord]] runs ahead of [[crates/scribe-client-gpui/src/main.rs#TerminalView#handle_binding]]: a chord claimed there never reaches the binding dispatcher at all. For the whole of the rebuild the close dialog sat on `ctrl+shift+q` and the notes modal on `ctrl+shift+n`, which are the Linux defaults for `close_tab` and `new_window`, so both actions were unreachable without a rebind. The overlays moved to `ctrl+shift+d` and `ctrl+shift+m` (chords no default binding uses) and the precedence rule keeps any future collision — including one a user creates by rebinding onto an overlay chord — resolved in the user's favour.

### Overlay Action Routing

Both overlays emit their choice for the shell to run; the shell routes those events into the same dispatchers the keyboard uses, so a palette row and its bound chord always do the same thing.

[[crates/scribe-client-gpui/src/main.rs#TerminalView#execute_palette_action]] is the confirm seam. The client-local remote-connect row is still unported; every other row carries a shared [[crates/scribe-common/src/protocol.rs#AutomationAction]] into [[crates/scribe-client-gpui/src/main.rs#TerminalView#execute_automation_action]]. Most of those lower onto a [[crates/scribe-client-gpui/src/keybindings.rs#KeyAction]] through [[crates/scribe-client-gpui/src/main.rs#key_action_for_automation]] and are handed to [[crates/scribe-client-gpui/src/main.rs#TerminalView#dispatch_key_action]] — literally the call the key path makes — so wiring a surface for one wires it for both. The three actions with no bindable chord are handled directly: [[crates/scribe-client-gpui/src/main.rs#TerminalView#switch_profile]] activates a stored profile and applies the config it returned through the normal reload path (rather than racing the file watcher against that write), [[crates/scribe-client-gpui/src/main.rs#TerminalView#focus_session]] moves the tab selection onto a session, and the update dialog waits on the update surfaces. The lowering match is exhaustive, so a new automation action fails to compile instead of quietly becoming unroutable.

[[crates/scribe-client-gpui/src/main.rs#TerminalView#dispatch_context_menu_action]] ports the winit menu routing for the open/run group: heuristic URLs keep the silent scheme-allowlist drop of [[crates/scribe-client-gpui/src/url_detect.rs#open_url]], an OSC 8 URI goes through [[crates/scribe-client-gpui/src/main.rs#TerminalView#route_osc8_activation]] so an allowlisted scheme opens immediately and any other scheme first raises the disallowed-scheme confirmation (spec 009 FR-015), file rows open with the OS handler, and the smart-selection rows type into the attached pane, spawn a detached command, or open a login shell in a new tab. The pending URI is parked on the view while that modal is up so [[crates/scribe-client-gpui/src/main.rs#TerminalView#route_dialog_outcome]] can activate it verbatim on "Open Anyway".

Rows whose destination surface is not ported yet — the settings window, the remote picker, the update dialog, and the clipboard/selection trio — are still *routed*: they reach a dispatcher and are named and counted by [[crates/scribe-client-gpui/src/main.rs#unroutable_action]] instead of being discarded at the subscription. That warning is the difference between "not built yet" and "silently dead", and it is what the scripted E2E asserts on for an unported row.

### Palette base entries and update row

Verifies [[crates/scribe-client-gpui/src/command_palette.rs#base_entries]] leads with "Open Settings" and ends with the remote-connect row, and that [[crates/scribe-client-gpui/src/command_palette.rs#build_entries]] appends the "Update Scribe to v{version}" row only when an update is available.

### Palette profile rows tag the active profile

Verifies [[crates/scribe-client-gpui/src/command_palette.rs#profile_entries]] emits one "Switch Profile: {name}" row per profile and suffixes " (active)" onto the currently active profile, wiring each row to a `SwitchProfile` action.

### Palette query filters case-insensitively

Verifies [[crates/scribe-client-gpui/src/command_palette.rs#filter_entries]] keeps every entry for a blank/whitespace query and otherwise retains only rows whose label contains the trimmed, lowercased needle.

### Palette typing and paste drive the filter

Verifies typing characters and [[crates/scribe-client-gpui/src/command_palette.rs#CommandPaletteView#push_str]] paste both extend the query (paste dropping control characters so a multi-line payload collapses) and narrow the filtered list.

### Palette selection wraps and confirms an action

Verifies the palette selection wraps with `next_item`/`prev_item`, [[crates/scribe-client-gpui/src/command_palette.rs#CommandPaletteView#confirm]] emits the highlighted row's [[crates/scribe-client-gpui/src/command_palette.rs#PaletteAction]], and confirming an empty filter is a no-op.

### Context menu head reflects selection state

Verifies [[crates/scribe-client-gpui/src/context_menu.rs#build_menu_items]] always leads with Copy / Paste / Select All and enables Copy only when a selection exists.

### Context menu OSC 8 precedence and copy entry

Verifies an OSC 8 URI takes "Open URL" precedence over a heuristic URL (via `OpenOsc8Url`) and appends a "Copy hyperlink address" row, while a heuristic-only right-click keeps the plain `OpenUrl` row and no copy entry (spec 009 FR-003 / FR-007).

### Context menu appends smart-selection actions

Verifies [[crates/scribe-client-gpui/src/context_menu.rs#smart_selection_menu_item]] drops actions with an empty expanded parameter and that surviving smart actions append after the file entry.

### Context menu click dispatches or dismisses

Verifies [[crates/scribe-client-gpui/src/context_menu.rs#ContextMenuView#activate]] emits an enabled row's action, that a disabled row is a no-op, and that [[crates/scribe-client-gpui/src/context_menu.rs#ContextMenuView#dismiss]] emits `Dismissed`.

### Tooltip centres on its anchor

Verifies [[crates/scribe-client-gpui/src/tooltip.rs#clamp_tooltip_x]] centres a box that fits horizontally on the middle of its anchor rect.

### Tooltip clamps to the viewport edges

Verifies [[crates/scribe-client-gpui/src/tooltip.rs#clamp_tooltip_x]] pins a box against the right edge when the anchor is near it and to `x=0` at the left edge, so an edge-anchored tooltip slides inward instead of clipping.

### Tooltip picks above or below the anchor

Verifies [[crates/scribe-client-gpui/src/tooltip.rs#tooltip_y]] returns the anchor-top-minus-height for `Above` and the anchor-bottom for `Below`.

### Tooltip truncates a long URL head and tail

Verifies [[crates/scribe-client-gpui/src/tooltip.rs#truncate_url]] returns short URIs unchanged, head+tail-elides an overflowing URI to exactly the budget with a middle `...`, falls back to a plain head cut at tiny budgets, and never splits a multibyte codepoint.

## GPUI Dialogs

The GPUI rebuild ports the winit client's five GPU-painted modals into display-independent state models plus one generic [[crates/scribe-client-gpui/src/dialog.rs#DialogView]] entity, replacing the winit `CellInstance` quad painters and pixel hit-testing with GPUI flex layout and `on_click` listeners.

Each modal is one variant of [[crates/scribe-client-gpui/src/dialog.rs#AnyDialog]] — [[crates/scribe-client-gpui/src/dialog.rs#CloseDialog]] (quit / kill / cancel, warning about active sessions), [[crates/scribe-client-gpui/src/dialog.rs#UpdateDialog]] (install-available live-reload plus the restart-required helper cold-restart, built by [[crates/scribe-client-gpui/src/dialog.rs#UpdateDialog#new_install]] / [[crates/scribe-client-gpui/src/dialog.rs#UpdateDialog#new_restart_required]]), [[crates/scribe-client-gpui/src/dialog.rs#PasteConfirmationDialog]] (the spec-011 risky-paste gate), [[crates/scribe-client-gpui/src/dialog.rs#ClipboardDialog]] (the OSC 52 four-button policy prompt), and [[crates/scribe-client-gpui/src/dialog.rs#DisallowedSchemeDialog]] (the spec-009 OSC 8 out-of-allowlist prompt). Every model lowers to a [[crates/scribe-client-gpui/src/dialog.rs#DialogSpec]] (title, body lines, tone-tagged buttons, focused index) so parity is asserted without a live window, and keeps the winit **safe default focus** — Cancel / Later / Deny once — so [[crates/scribe-client-gpui/src/dialog.rs#AnyDialog#confirm]] on an unexpected prompt never performs the risky action.

[[crates/scribe-client-gpui/src/dialog.rs#DialogView]] renders any spec onto a dimmed backdrop and a rounded, drop-shadowed box with a centred title, body, separator rule, and a button row whose accent / warm-red-destructive / subtle tones come from [[crates/scribe-client-gpui/src/dialog.rs#ButtonTone]] resolved against a theme-derived [[crates/scribe-client-gpui/src/dialog.rs#DialogColors]]. [[crates/scribe-client-gpui/src/dialog.rs#DialogView#focus_next]] / [[crates/scribe-client-gpui/src/dialog.rs#DialogView#focus_prev]] cycle focus, [[crates/scribe-client-gpui/src/dialog.rs#DialogView#confirm]] activates the focused button, [[crates/scribe-client-gpui/src/dialog.rs#DialogView#activate]] activates a clicked button, and [[crates/scribe-client-gpui/src/dialog.rs#DialogView#dismiss]] (Esc / backdrop) resolves to the safe [[crates/scribe-client-gpui/src/dialog.rs#AnyDialog#cancel]] action — each emitting a tagged [[crates/scribe-client-gpui/src/dialog.rs#DialogOutcome]] on [[crates/scribe-client-gpui/src/dialog.rs#DialogEvent]] for the shell to route. The paste gate reuses [[crates/scribe-client-gpui/src/paste.rs#classify_paste]] and renders the parked [[crates/scribe-client-gpui/src/paste.rs#ParkedPaste]] in caret notation so a control sequence in the preview can never drive the terminal (FR-005), returning it verbatim via [[crates/scribe-client-gpui/src/dialog.rs#PasteConfirmationDialog#into_parked_paste]] for byte-identical delivery.

The spike wires two representative modals into [[crates/scribe-client-gpui/src/main.rs#TerminalView]] — Ctrl+Shift+Q opens the close dialog and Ctrl+Shift+K opens the clipboard dialog — so the visual E2E harness (`tests/e2e/visual/dialogs.sh`) can screenshot the modal chrome, the focus ring, and the tone-tagged buttons across the three- and four-button layouts.

The update confirmation is the first of the five that is a live surface rather than a demo: [[crates/scribe-client-gpui/src/main.rs#TerminalView#open_dialog]] routes the resolved [[crates/scribe-client-gpui/src/dialog.rs#DialogOutcome]] before dropping the overlay, so `DialogOutcome::Update` reaches [[crates/scribe-client-gpui/src/main.rs#TerminalView#route_update_action]] and turns into IPC (see [[client#GPUI Update Surfaces]]). The other four still only close.

### Close dialog buttons and safe default

Verifies [[crates/scribe-client-gpui/src/dialog.rs#CloseDialog]] renders Quit Scribe / Kill Window / Cancel with accent / danger / normal tones, focuses the safe Cancel by default, and shows the active-session-loss warning only when sessions are open.

### Close dialog focus cycling maps to actions

Verifies [[crates/scribe-client-gpui/src/dialog.rs#AnyDialog#focus_next]] / [[crates/scribe-client-gpui/src/dialog.rs#AnyDialog#focus_prev]] wrap across the three close buttons, that [[crates/scribe-client-gpui/src/dialog.rs#AnyDialog#action_at]] maps each index to its `CloseAction`, and that dismissal always resolves to Cancel.

### Update dialog install and restart flows

Verifies the install-available dialog titles "Update Available", defaults focus to the accent Update Now, and keeps live-reload copy, while the restart-required helper cold-restart titles "Restart Required" and offers Continue / Cancel.

### Paste gate reason line distinguishes risk

Verifies [[crates/scribe-client-gpui/src/dialog.rs#PasteConfirmationDialog]] derives the reason line for the multiline-only, control-only, and combined cases, defaults focus to Cancel, and offers Cancel / Paste.

### Paste preview is caret-escaped

Verifies the paste dialog body never contains a raw control byte (ESC renders as `^[`) and that [[crates/scribe-client-gpui/src/dialog.rs#PasteConfirmationDialog#into_parked_paste]] returns the parked text verbatim for byte-identical delivery.

### Clipboard dialog four-button policy

Verifies [[crates/scribe-client-gpui/src/dialog.rs#ClipboardDialog]] renders Deny once / Always deny / Allow once / Always allow with the two Allow variants tinted destructive, focuses the safe Deny once, maps each index to its policy action, and shows the write payload preview.

### Disallowed scheme dialog truncation

Verifies [[crates/scribe-client-gpui/src/dialog.rs#DisallowedSchemeDialog]] names the blocked scheme, head-and-tail-truncates a long URI while keeping both ends visible, defaults focus to Cancel, and preserves the full URI verbatim via [[crates/scribe-client-gpui/src/dialog.rs#DisallowedSchemeDialog#into_pending_uri]].

### Dialog view confirms the focused button

Verifies [[crates/scribe-client-gpui/src/dialog.rs#DialogView#focus_next]] followed by [[crates/scribe-client-gpui/src/dialog.rs#DialogView#confirm]] emits the newly focused button's [[crates/scribe-client-gpui/src/dialog.rs#DialogOutcome]] on [[crates/scribe-client-gpui/src/dialog.rs#DialogEvent]].

### Dialog view click and dismissal resolve

Verifies [[crates/scribe-client-gpui/src/dialog.rs#DialogView#activate]] emits the clicked button's outcome regardless of focus, and that [[crates/scribe-client-gpui/src/dialog.rs#DialogView#dismiss]] emits the safe cancel outcome for an Esc or backdrop click.

## GPUI Workspace Notes

The GPUI rebuild ports the per-workspace notes modal and its hover preview as `gpui::Entity` views, replacing the winit `CellInstance` painters while keeping the state machines and geometry verbatim.

[[crates/scribe-client-gpui/src/workspace_notes.rs#WorkspaceNotesStore]] caches the server-owned [[crates/scribe-common/src/protocol.rs#WorkspaceNotesCollection]] per workspace and projects [[crates/scribe-client-gpui/src/workspace_notes.rs#WorkspaceNoteSummary]] rows for the preview; [[crates/scribe-client-gpui/src/workspace_notes.rs#AddingNoteState]] is the shared inline-editor buffer (FR-020) whose caret-motion and scroll helpers are byte-for-byte ports.

[[crates/scribe-client-gpui/src/workspace_notes_modal.rs#WorkspaceNotesModalView]] folds the active/archive/editor state machine (the [[crates/scribe-client-gpui/src/workspace_notes_modal.rs#WorkspaceNotesView]] toggle, the [[crates/scribe-client-gpui/src/workspace_notes_modal.rs#WorkspaceNotesEditMode]] target, draft dirty flag, and `\n---\n` bulk splitter) into one entity, painting the panel, nav, note rows, and editor with GPUI elements. Clicking a control emits a [[crates/scribe-client-gpui/src/workspace_notes_modal.rs#WorkspaceNotesModalAction]]; [[crates/scribe-client-gpui/src/workspace_notes_modal.rs#WorkspaceNotesModalView#save_mutation]] and [[crates/scribe-client-gpui/src/workspace_notes_modal.rs#WorkspaceNotesModalView#archive_mutation]] turn the current state into a frozen [[crates/scribe-common/src/protocol.rs#WorkspaceNotesMutation]] for the shell to send.

[[crates/scribe-client-gpui/src/workspace_notes_modal.rs#DraftDebounce]] ports the winit `WORKSPACE_NOTES_DEBOUNCE` timer: each edit restarts a 250 ms window via a cancel-on-drop `gpui::Task`, so a typing burst collapses to one [[crates/scribe-client-gpui/src/workspace_notes_modal.rs#DraftDebounceEvent]] flush, and workspace-switch / close paths force one with [[crates/scribe-client-gpui/src/workspace_notes_modal.rs#DraftDebounce#flush_now]].

[[crates/scribe-client-gpui/src/workspace_notes_preview.rs#WorkspaceNotesPreviewView]] paints the hover preview in two modes — a read-only list with a "+N more" overflow row plus a "+" affordance (FR-001), or the inline editor (FR-002) with a caret, error row, and scroll clamp. The pure sizing/wrap helpers ([[crates/scribe-client-gpui/src/workspace_notes_preview.rs#preview_cols]], [[crates/scribe-client-gpui/src/workspace_notes_preview.rs#wrap_text_for_editor]], [[crates/scribe-client-gpui/src/workspace_notes_preview.rs#caret_line_index]]) stay testable; clicks emit a [[crates/scribe-client-gpui/src/workspace_notes_preview.rs#WorkspaceNotesPreviewAction]].

The shell wires both over the frozen protocol through [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#workspace_notes_get]] and [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#workspace_notes_mutate]]; the `main.rs` spike opens the modal on Ctrl+Shift+N and routes its actions so the visual E2E can exercise every surface.

### Inline editor caret motion

Verifies [[crates/scribe-client-gpui/src/workspace_notes.rs#AddingNoteState]] inserts and backspaces on char boundaries and that horizontal, line-edge, and vertical caret motion preserve the character column across multi-byte text.

### Store projects summaries

Verifies [[crates/scribe-client-gpui/src/workspace_notes.rs#WorkspaceNotesStore#hover_summaries]] flattens whitespace, caps each summary length, and reports the total active count so the preview can show overflow.

### Modal view and edit-mode state machine

Verifies [[crates/scribe-client-gpui/src/workspace_notes_modal.rs#WorkspaceNotesModalView]] open seeds the draft, close resets the editor, switching views cancels a non-draft edit, and draft typing marks the buffer dirty while editing an existing note does not.

### Modal save maps to a mutation

Verifies [[crates/scribe-client-gpui/src/workspace_notes_modal.rs#WorkspaceNotesModalView#save_mutation]] maps each edit mode to the right [[crates/scribe-common/src/protocol.rs#WorkspaceNotesMutation]] (blank draft yields none), and that archive controls carry the correct reason.

### Draft debounce coalesces and fires once

Verifies [[crates/scribe-client-gpui/src/workspace_notes_modal.rs#DraftDebounce]] emits no flush before the 250 ms window, exactly one flush after it, coalesces a typing burst into a single flush, and that [[crates/scribe-client-gpui/src/workspace_notes_modal.rs#DraftDebounce#flush_now]] forces one immediately while cancelling the pending timer.

### Preview sizing and wrap geometry

Verifies the preview's pure geometry: [[crates/scribe-client-gpui/src/workspace_notes_preview.rs#preview_cols]] sizes to the longest note or editor line and clamps, [[crates/scribe-client-gpui/src/workspace_notes_preview.rs#wrap_text_for_editor]] splits on hard and soft breaks, and the caret line/column helpers track the wrapped position.

### Preview inline and editor modes

Verifies [[crates/scribe-client-gpui/src/workspace_notes_preview.rs#WorkspaceNotesPreviewView]] toggles between the read-only affordance and the inline editor as its [[crates/scribe-client-gpui/src/workspace_notes.rs#AddingNoteState]] buffer is set and cleared.

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

The cache stores the last-built instances along with cursor blink visibility, terminal cursor hidden state (DECTCEM), focus state, selection range, and display offset. If all match, the cached instances are reused without GPU upload. Tracking the viewport offset prevents scrollback changes from reusing stale cells, while DECTCEM tracking invalidates when a program toggles cursor visibility via `CSI ? 25 h/l` without other content changes.

### Synchronized Updates

Normal live sessions receive the raw synchronized-update markers from the server, and the client decides redraw pacing from pane-local committed-frame queues instead of from raw PTY delivery order alone.

[[crates/scribe-client/src/main.rs#App#handle_pty_output]] hands incoming PTY bytes to [[crates/scribe-client/src/pane.rs#Pane#queue_output_frames]], which preserves raw `CSI ? 2026 h/l` frame boundaries across message splits before enqueuing the resulting raw frames on the pane. [[crates/scribe-client/src/main.rs#App#handle_redraw]] still lets light traffic present one committed burst per frame, while [[crates/scribe-client/src/main.rs#App#about_to_wait]] switches winit to `ControlFlow::Poll` whenever queued output remains so redraws cannot stall behind a long user-event burst. The pane-local VTE processor still handles the actual synchronized-update buffering, and [[crates/scribe-client/src/main.rs#App#flush_expired_sync_updates]] mirrors VTE's 150 ms timeout for raw frames buffered ahead of the pane-local processor. Each raw timeout starts when its own block opens, and its BSU-stripped bytes join the queued frames in FIFO order so a timeout cannot overtake an earlier commit or re-enter sync mode.

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

`handle_session_list` must apply per-workspace `WorkspaceListEntry` metadata (name, accent color, project root) *after* `reconstruct_workspaces_for_sessions` runs — the reconstruction path does `self.window_layout = WindowLayout::from_tree(tree)` and replaces the layout wholesale, so any names applied before reconstruction are silently dropped. Post-handoff the preserved shells emit no fresh OSC 7, so SessionList is the only source of workspace names at reconnect.

The per-workspace `active_tab_index` reported in each tree leaf is also applied *after* `restore_reconnect_tabs` populates tabs, not as part of `from_tree`. Each `add_tab_with_pane_tree` call inside the restore loop auto-sets `active_tab` to the last-pushed tab (correct UX for user-initiated tab creation, wrong for restore), so the post-pass calls [[crates/scribe-client/src/workspace_layout.rs#WindowLayout#set_active_tab]] per leaf to restore the originally focused tab.

### Tab State

Each tab in a workspace owns a `LayoutTree` for its panes, a focused pane ID, and an optional text selection. Tabs are created, removed, and reordered within their workspace slot.

The workspace's `active_tab` index lives on [[crates/scribe-client/src/workspace_layout.rs#WorkspaceSlot]] and rides the [[crates/scribe-common/src/protocol.rs#WorkspaceTreeNode]] `Leaf` variant as `active_tab_index`. Client reports it via `ReportWorkspaceTree` on every tab switch ([[crates/scribe-client/src/main.rs#App#switch_active_tab]]) alongside the existing report-on-split/close triggers, so the server's per-window tree (used for handoff and reconnect) always reflects the latest focused tab.

## Tab Bar

GPU-rendered tab bar in [[crates/scribe-client/src/tab_bar.rs]] generating [[crates/scribe-client/src/tab_bar.rs#TabBarColors]] from [[crates/scribe-client/src/tab_bar.rs#TabData]] using the same glyph atlas as the terminal grid.

[[crates/scribe-client/src/tab_bar.rs#TabBarColors]] is derived from `ChromeColors` and holds background, active background, text, separator, gradient-top, and accent color values. [[crates/scribe-client/src/tab_bar.rs#TabData]] carries per-tab title, active flag, optional AI indicator color, and an optional transient `tab_flash` intensity (a short theme-accent blend over the tab background that self-decays over `TAB_FLASH_SECS` ≈0.45s via the same animation/redraw envelope as the scrollbar fade, additive over active/hover styling and the AI indicator). The background is rendered as a two-tone vertical gradient (lighter top half, base bottom half) via `build_tab_bar_bg`. The active tab receives a uniform highlight color and a 2px accent indicator on its bottom edge. An AI state dot (from `TabData.ai_indicator`) is rendered in the tab when a session has an active AI state. For provider task-label sessions, the title prefers the last hook-emitted task label while that label is active, then falls back to the normal shell title. Tab titles are truncated to fit the available column width. In multi-workspace mode, named workspaces display a badge pill with a deterministic accent color; unnamed workspaces show no badge. The pill quad and per-cell backgrounds both span `space + name + space + trailing-gap` so the accent fills the full `badge_columns` allocation up to the next tab boundary — the gap cells are emitted with `pill_bg` rather than the chrome bg, otherwise the cell-bg pass would punch through the underlying quad and leave a visible strip between the badge and the first tab.

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

Click routing uses [[crates/scribe-client/src/tab_bar.rs#workspace_badge_hit_rect]] to turn the rendered workspace badge/name into a modal target. [[crates/scribe-client/src/main.rs#App#open_workspace_notes_modal]] focuses that workspace, closes transient overlays, and opens [[crates/scribe-client/src/workspace_notes_modal.rs#WorkspaceNotesModal]] with any saved draft. [[crates/scribe-client/src/main.rs#App#handle_workspace_notes_keyboard]] consumes modal keys before PTY translation: **Enter saves, Ctrl+Enter inserts a newline**, Backspace edits the text, the spacebar inserts a literal space, and Escape closes or cancels edits.

Opening a modal or receiving reconnect workspace metadata triggers [[crates/scribe-client/src/main.rs#App#request_workspace_notes_snapshot]]. Snapshot drafts hydrate the modal only while its local draft is pristine; typed text is never overwritten by a late snapshot. User edits are sent through `WorkspaceNotesMutate`; the client updates visible lists only from server broadcasts so multiple windows converge on the server's last accepted mutation.

The active view supports creating notes, editing active notes, and moving done or removed notes to archive. The archive view keeps archived notes separate, supports single archived-note edits, and has an edit-all mode that sends one bulk mutation without touching active notes.

Clicking outside the notes modal closes it through the same draft-preserving path as the explicit close action. Empty modal space remains inert so controls are the only in-modal click targets.

Draft typing is debounced by [[crates/scribe-client/src/main.rs#App#flush_workspace_notes_if_due]], then flushed immediately by [[crates/scribe-client/src/main.rs#App#flush_workspace_notes_now]] on modal close. Update, restart, quit, and window-close actions defer until pending draft or modal mutations receive the server broadcast that proves durability.

The modal renderer keeps terminal-cell geometry while spacing header tabs, note list, New-note editor, bordered input with a visible caret, retryable server-error text, and footer zones with theme-derived surfaces and title-cased actions.

Hover previews are derived from active notes only and rendered by [[crates/scribe-client/src/workspace_notes_preview.rs#build_workspace_notes_preview]]. [[crates/scribe-client/src/main.rs#App#apply_workspace_notes_preview_overlay]] draws the bounded preview above terminal content but before modal overlays, while suppressing it behind the notes modal, context menu, close dialog, and update dialog.

The hover preview stays open while the pointer is over the workspace badge or preview bounds. Visible preview notes highlight on hover with a left accent bar drawn by `draw_row_hover` (no row-wide tint — the bar plus brighter `hover_text` carry the selection signal), and clicking one sends `ArchiveNote { reason: Done }` so lightweight note cleanup does not require opening the modal.

### Inline Note Editor

The hover preview exposes an inline note-capture affordance — a `~2 col × 1 row` bordered "+" cell — that lets a user create a new active note without opening the modal.

Visual treatment: when the editor is open the row uses an underline-only style (no box fill, no top/side borders) with a leading accent "›" prompt indicator rendered by `draw_editor_text`. An `EDITOR_PREFIX_COLS`-wide indent is reserved across wrapped continuation rows so the caret column stays aligned, and `editor_content_cols` subtracts the prefix from the wrap budget so layout sizing and `clamp_scroll_to_caret` agree with the renderer.

Affordance routing: the read-only preview reserves a bottom row for a bordered "+" cell positioned with one cell of inset from the preview's right and bottom inner borders. Its hit-rect is published on [[crates/scribe-client/src/workspace_notes_preview.rs#WorkspaceNotesPreviewInteraction]]`.affordance_rect`. Hover state is tracked by the new `App` field `affordance_hovered_workspace`. Click routing in [[crates/scribe-client/src/main.rs#App#handle_workspace_notes_preview_mouse_press]] hit-tests the affordance before existing note-row archival, and on hit calls [[crates/scribe-client/src/main.rs#App#open_inline_note_editor]].

Per-workspace state: the editor's lifetime state lives in a per-workspace map field `adding_note_states: HashMap<WorkspaceId, AddingNoteState>` on `App`. Multiple workspaces can hold "adding note" state simultaneously (relevant under split panes and cross-workspace pointer movement); editor state survives pointer-leave / pointer-return cycles because the map is independent of which preview is currently rendered. A single workspace is also tracked in `focused_inline_editor: Option<WorkspaceId>` so the keyboard router knows which editor owns key focus when keys arrive between the modal handler and PTY translation.

[[crates/scribe-client/src/workspace_notes.rs#AddingNoteState]] holds `draft_text: String`, `draft_dirty: bool`, `caret_byte: usize`, `scroll_offset_rows: usize`, `last_server_error: Option<String>`, and `committed_pending: bool`. The struct is constructed via [[crates/scribe-client/src/workspace_notes.rs#AddingNoteState#new_from_saved_draft]] which seeds `draft_text` from the workspace's existing saved draft (shared buffer with the modal's New-note editor) and places the caret at the end of that text. Text mutation goes through [[crates/scribe-client/src/workspace_notes.rs#AddingNoteState#insert_char]] and [[crates/scribe-client/src/workspace_notes.rs#AddingNoteState#backspace]]; caret navigation goes through `move_caret_left`, `move_caret_right`, `move_caret_up`, `move_caret_down`, `move_caret_line_start`, `move_caret_line_end`. The helper [[crates/scribe-client/src/workspace_notes.rs#AddingNoteState#is_blank_trimmed]] gates empty-draft Enter as a no-op.

Caret math is UTF-8-safe end-to-end: vertical movement computes column position as a CHAR count (not a byte difference) and converts back to a byte offset via the file-private `byte_offset_of_nth_char` helper, so multi-byte glyphs (CJK, emoji, combining marks) never leave `caret_byte` mid-codepoint. `String::insert` / `String::replace_range` panics on bad boundaries are therefore unreachable from any keymap path.

Keyboard capture: [[crates/scribe-client/src/main.rs#App#try_handle_inline_editor_keyboard]] runs at level 2 of the [[client#Input#Key Translation Priority]] chain (Special commands) when `focused_inline_editor` is set, before PTY translation. The keymap matches the modal post-FR-017: Enter commits via [[crates/scribe-client/src/main.rs#App#commit_inline_editor]] sending `WorkspaceNotesMutation::CreateActiveNote`; Ctrl+Enter calls `insert_char('\n')` for a literal newline; Escape calls [[crates/scribe-client/src/main.rs#App#close_inline_note_editor]] which discards the in-progress draft text without flushing; Space inserts a literal space; Backspace deletes the previous char; arrow keys, Home, and End drive caret navigation. Commit-coalescing (FR-016) short-circuits successive Enter presses while `committed_pending == true`.

Draft pipeline: typing piggybacks on the existing `workspace_notes_save_pending: Option<Instant>` debounce timer. When [[crates/scribe-client/src/main.rs#App#flush_workspace_notes_if_due]] elapses or any higher-priority overlay fires, [[crates/scribe-client/src/main.rs#App#flush_inline_editor_drafts]] (driven by [[crates/scribe-client/src/main.rs#App#dirty_inline_editor_drafts]]) emits one `SaveDraft` per workspace whose `draft_dirty` flag is set. The shared-buffer guarantee (FR-020) means committing via either the modal or the inline editor consumes the same draft.

Layout & growth: [[crates/scribe-client/src/workspace_notes_preview.rs#WorkspaceNotesPreviewBuildContext]] carries `inline_editor: Option<&mut AddingNoteState>`, `affordance_hovered: bool`, and `max_editor_rows: Option<usize>`. The `&mut` is needed because the build pass calls [[crates/scribe-client/src/workspace_notes.rs#AddingNoteState#clamp_scroll_to_caret]] internally — after `PreviewLayout::new` computes the real `cols` and `editor_rows` — so the scroll-to-caret math always uses the renderer's actual content width (not an external estimate). The build call site in [[crates/scribe-client/src/main.rs#App#apply_workspace_notes_preview_overlay]] computes `max_editor_rows` via [[crates/scribe-client/src/main.rs#App#inline_editor_max_rows]] — 3/4 of the focused pane's vertical extent per FR-019 — and passes a `&mut` borrow of the workspace's `AddingNoteState`. The layout struct `PreviewLayout` (in `workspace_notes_preview.rs`) tracks `bottom_zone_row`, `editor_rows`, and `has_editor_error` to drive rendering of either the "+" affordance row (`draw_affordance`) or the inline editor row (`draw_editor_row`). Wrapping helpers (`wrap_text_for_editor`, `wrapped_row_count`, `caret_line_index`, `caret_visible_col`, `longest_visible_line_chars`) are pure functions that compute visual wrap geometry; [[crates/scribe-client/src/workspace_notes.rs#AddingNoteState#clamp_scroll_to_caret]] uses a parallel `visual_line_of` helper so editor state and renderer agree on caret position.

Scroll inputs: caret-tracking auto-scroll fires inside `clamp_scroll_to_caret`; [[crates/scribe-client/src/main.rs#App#try_inline_editor_mouse_wheel]] consumes mouse-wheel events landing inside `WorkspaceNotesPreviewInteraction.editor_rect` and updates `scroll_offset_rows` without moving the caret (wheel events outside the editor row fall through to the terminal). A static overlay scrollbar inside the editor row visually indicates scroll position when wrapped content exceeds `editor_rows`; the full `ScrollbarState` fade animation pattern (`[[client#Scrollbar]]`) is reserved for a future polish pass.

Dismissal & errors: higher-priority overlays (notes modal, context menu, command palette, search overlay, close dialog, update dialog) close all inline editors and flush their drafts through `SaveDraft` via [[crates/scribe-client/src/main.rs#App#dismiss_all_inline_editors]]. The dismiss is wired at the **overlay-open entry points themselves** (`open_workspace_notes_modal`, `handle_open_command_palette`, `handle_open_find`, context-menu-open, `handle_close_requested`, update-dialog-open) rather than from the render path, so the flush IPC fires synchronously at the moment the user takes the action — no in-render side effects. [[crates/scribe-client/src/main.rs#App#workspace_notes_preview_allowed]] gates the read-only preview against all six overlay sources so the affordance disappears the same frame any of them opens. The modal-open path captures the inline editor's local `draft_text` for the target workspace BEFORE the dismiss runs, then passes that text directly to `WorkspaceNotesModal::open` — otherwise the asynchronous `SaveDraft` ack would leave the modal showing stale text.

The pointer-leave-while-editing exemption in [[crates/scribe-client/src/main.rs#App#update_workspace_notes_hover]] falls back to `focused_inline_editor` as the hover anchor so the editor stays visible across hover gaps. Window-close and update-relaunch flush through [[crates/scribe-client/src/main.rs#App#defer_workspace_notes_action_until_flush]] which iterates inline-editor drafts before allowing the action to proceed. Late server snapshots are reconciled by [[crates/scribe-client/src/main.rs#App#hydrate_inline_editor_pristine_draft]] which adopts the broadcast draft only when the local `draft_dirty == false` (FR-015 pristine-draft policy) and clamps the caret to the nearest char boundary in the new text. Server rejections of an in-flight `CreateActiveNote` surface into `AddingNoteState.last_server_error` (set in the `ServerError` arm of [[crates/scribe-client/src/main.rs#App#handle_workspace_user_event]]) while preserving the typed text so the user can retry without retyping.

## Input

Keybindings are parsed from config into a `Bindings` struct in [[crates/scribe-client/src/input.rs#Bindings]] with over 50 configurable actions.

### Focus Guard

Two layers prevent stray key events from compositor overlays (e.g. GNOME Screenshot) from reaching the PTY.

Both layers also drive [[client#Input#IME Composition#Activation Gate]]: window-focus transitions flow through `notify_focus_change` (a single chokepoint reused by every pane / session focus path) which calls `refresh_ime_allowed`, and the per-tick X11 guard poll re-evaluates the gate so a compositor overlay's reactivation debounce also blocks IME until the window is truly active again.

#### Winit Focus

Keyboard events are only processed when the window has focus (`window_focused == true`). This catches overlays that trigger X11 `FocusOut` events.

#### X11 Active-Window Guard

[[crates/scribe-client/src/x11_focus.rs#X11FocusGuard]] polls `_NET_ACTIVE_WINDOW` via a separate `x11rb` connection to detect compositor overlays that skip X11 focus events.

Compositor overlays (e.g. GNOME Shell screenshot) clear or change this EWMH property without sending `FocusOut`. The guard polls in `about_to_wait` and on each key press. A `was_inactive` flag tracks whether the window has been obscured; when `should_suppress_key` or `poll` first sees the window become active again, a `reactivated_at` timestamp is set and keys are suppressed for 300ms from that transition. The debounce is cleared on `Focused(true)` so it only applies to compositor overlay dismissals — not normal focus transitions — preventing the first keystroke from being swallowed when the user alt-tabs or clicks to Scribe.

#### GPUI X11 Handle

Pinned GPUI exposes the X11 XID as `RawWindowHandle::Xcb`, so the rebuild keeps
the guard's direct EWMH comparison without title/PID lookup.

The non-zero `window` field initializes `X11FocusGuard`; the decision and
integration probe are recorded in `specs/016-gpui-client-rebuild/x11-xid-spike.md`.

#### GPUI Terminal Ligatures

Pinned GPUI keeps multi-cell terminal ligatures grid-aligned when its terminal
run is shaped with `shape_line(..., Some(cell_width))`, so the rebuild retains
the `appearance.ligatures` key.

The port batches equal-style cells and uses the forced cell width while
painting. GPUI preserves a zero-advance glyph's offset from its base glyph,
then assigns each advancing glyph to the next grid cell; disabling `calt`
through `FontFeatures::disable_ligatures()` provides the false-setting path.
The demo and source evidence are recorded in
`specs/016-gpui-client-rebuild/ligatures-spike.md`.

### Key Translation Priority

Key events are resolved through a four-level priority chain from layout shortcuts down to raw terminal byte encoding.

On macOS, bare `cmd+w` is handled before that chain and routed to the same close-request path as the native window close button, so it never falls through to pane bindings or terminal input.

The hover-preview inline editor (see [[client#Workspace Notes]]) inserts itself at level 2: when `focused_inline_editor` is set, [[crates/scribe-client/src/main.rs#App#try_handle_inline_editor_keyboard]] consumes the key before the rest of the chain runs. This is in addition to the existing modal handler, which still receives keys through `handle_workspace_notes_window_event` when the modal is open.

Above the level-4 encoder, [[crates/scribe-client/src/main.rs#App#key_consumed_by_ime]] short-circuits the dispatch at the entry of `handle_keyboard` whenever an OS IME composition is in flight, so synthesized winit key events that the IME is mid-composing never reach the legacy or Kitty encoder. See [[client#Input#IME Composition]] for the state machine that drives that predicate.

1. Layout shortcuts (configurable keybindings) produce `LayoutAction` enum values
2. Special commands (command palette, settings, find, hover-preview inline editor)
3. Terminal shortcuts (word navigation, line navigation)
4. Generic terminal key translation produces PTY bytes — legacy xterm modifier encoding, or full Kitty CSI-u when the focused application negotiated the keyboard protocol

Pane-local terminals enable kitty keyboard tracking so an application's negotiated progressive-enhancement flags shape encoding. [[crates/scribe-client/src/pane.rs#Pane#new]] turns tracking on; [[crates/scribe-client/src/main.rs#App#focused_terminal_mode]] bundles the five negotiated flags from the focused pane's `Term` mode together with the two DEC private modes (`APP_CURSOR` for DECCKM, `APP_KEYPAD` for DECPAM) into [[crates/scribe-client/src/input.rs#TerminalMode]] — Kitty flags are forced all-off when the `[terminal]` `keyboard_protocol_enhanced` opt-out is disabled, but the DEC modes always reflect the pane so terminfo `smkx` / `rmkx` keeps working. The level-4 encoder emits CSI-u for Kitty functional keys whose protocol entries are true codepoints, but arrows, Insert/Delete, Page keys, Home/End, and F1-F12 stay on their Kitty legacy-shaped CSI letter/tilde forms; repeat/release markers are carried in the modifier parameter's event-type subfield. With no Kitty flag negotiated the legacy byte encoding is reproduced byte-identically. Codex panes still map Alt+Enter to Codex's newline binding before the generic path.

#### DEC Application Modes

The level-4 encoder consults DECCKM and DECPAM so unmodified arrows and numpad keys switch escape forms in app-cursor / app-keypad mode.

Apps such as `less`, `vim`, `top`, and `htop` enable DECCKM via terminfo's `smkx` capability when they start. If the encoder kept emitting CSI form (`\x1b[A`) instead of SS3 form (`\x1bOA`), those pagers silently swallow cursor keys because their terminfo `kcuu1` describes the SS3 byte sequence.

| Key                       | DECCKM off (default) | DECCKM on    |
|---------------------------|----------------------|--------------|
| Bare ArrowUp/Down/Left/Right | `\x1b[A..D`         | `\x1bOA..D`  |
| Bare Home / End           | `\x1b[H`, `\x1b[F`   | `\x1bOH`, `\x1bOF` |
| Modified arrow / Home / End | `\x1b[1;<mod><L>` (same regardless of DECCKM) | same |

[[crates/scribe-client/src/input.rs#translate_named_csi_letter]] picks the SS3 form when `app_cursor` is set and no modifier is held; otherwise it emits the modifier-aware CSI form. Modified chords always use CSI because SS3 has no slot for the xterm modifier parameter — this matches alacritty's default bindings and xterm's `modifyCursorKeys=2`.

Numeric-keypad keys (`KeyLocation::Numpad`) emit SS3 sequences when DECPAM is active and no modifier is held: digits `0..9` map to `\x1bOp..\x1bOy`, `.,-+*/=` map to `\x1bOn`/`\x1bOl`/`\x1bOm`/`\x1bOk`/`\x1bOj`/`\x1bOo`/`\x1bOX`, and numpad Enter maps to `\x1bOM`. [[crates/scribe-client/src/input.rs#translate_numpad_app_keypad]] runs ahead of the legacy / Kitty dispatch so the numpad table wins over the generic encoder for those events.

### GPUI Input Encoder Port

The GPUI rebuild reproduces the level-4 terminal byte encoder in [[crates/scribe-client-gpui/src/input.rs#encode]], byte-identical to the winit client's [[crates/scribe-client/src/input.rs#translate_key]] across legacy xterm, Kitty CSI-u, DECCKM, and DECPAM output.

Because GPUI's `Keystroke` drops numeric-keypad location and a distinct unshifted base vs shifted glyph, the encoder consumes an intermediate [[crates/scribe-client-gpui/src/input.rs#KeyInput]] carrying the key token, base character, associated text, modifiers, [[crates/scribe-client-gpui/src/input.rs#KeyLocation]], and press/repeat/release state. [[crates/scribe-client-gpui/src/input.rs#KeyInput#from_key_down]] lowers a GPUI `KeyDownEvent` into that shape — numpad location is unavailable on that path, so callers with richer platform data set it directly. Negotiated Kitty flags travel through [[crates/scribe-client-gpui/src/input.rs#KittyFlags]] and the two DEC modes through [[crates/scribe-client-gpui/src/input.rs#TerminalMode]], mirroring the winit encoder's [[crates/scribe-client/src/input.rs#TerminalMode]].

The keybinding dispatch above this encoder is wired into the shell (see [[client#GPUI Client Spike#Tab Strip And Key Dispatch]]), and the level-4 byte encoder is now the binary's own key path: [[crates/scribe-client-gpui/src/main.rs#encode_key]] lowers the GPUI event and calls [[crates/scribe-client-gpui/src/input.rs#encode]] directly. The port is verified against the committed oracle (see [[client#Input#Mouse Reporting#GPUI Rebuild Golden Oracle]]) by a golden byte-capture test that replays every case in `tests/fixtures/gpui-client/keyboard-byte-golden.json`.

The one piece still missing is the per-pane mode: the binary always passes [[crates/scribe-client-gpui/src/input.rs#TerminalMode]]`::legacy` because it tracks no negotiated Kitty flags and no DECCKM/DECPAM state yet, so the Kitty and application-mode branches of the encoder stay unreachable from the running client even though the golden test covers them. Wiring the focused pane's mode (the winit client's [[crates/scribe-client/src/main.rs#App#focused_terminal_mode]]) is the remaining step to full parity.

### GPUI Keybindings Port

The GPUI rebuild ports the keybinding parser and layout-action dispatch from the winit client, retargeted at GPUI's `Keystroke`/`Modifiers` via the intermediate [[crates/scribe-client-gpui/src/input.rs#KeyInput]], so no configured shortcut regresses at cutover.

GPUI's Linux backends name a key by the keysym the *current* modifier level resolves to and then drop the shift flag for single-character non-letter keys, so `ctrl+shift+\` arrives as control plus the key `|` with shift clear — nothing like the combo the config spells. Every shifted-symbol default (`split_vertical`, `split_horizontal`, `zoom_in`) was therefore unreachable in the running client, so [[crates/scribe-client-gpui/src/keybindings.rs#Keybinding#character_matches]] also accepts a binding's US-layout shifted glyph ([[crates/scribe-client-gpui/src/keybindings.rs#shifted_ascii]]) arriving with shift already folded in. Letters are absent from that table on purpose: the backends already report them by their own lowercase key and keep the shift flag.

[[crates/scribe-client-gpui/src/keybindings.rs#Bindings]] parses every configurable action from [[crates/scribe-common/src/config.rs#KeybindingsConfig]] (invalid combos skipped with a warning). [[crates/scribe-client-gpui/src/keybindings.rs#Keybinding#parse]] reads the same combo vocabulary as [[crates/scribe-client/src/input.rs#Keybinding#parse]], mapping `cmd`/`super` onto GPUI's platform modifier, and [[crates/scribe-client-gpui/src/keybindings.rs#Keybinding#matches]] requires an exact modifier match (ignoring the GPUI function flag) on a key-down event, comparing characters against the unshifted base case-insensitively. [[crates/scribe-client-gpui/src/keybindings.rs#translate_key_action]] runs the legacy three-level intercept order — layout shortcuts, then command-palette/settings/find, then the seven fixed terminal-shortcut escape sequences — returning a [[crates/scribe-client-gpui/src/keybindings.rs#KeyAction]]; the generic byte encoder handles level 4 when it returns `None`. All 50+ [[crates/scribe-client-gpui/src/keybindings.rs#LayoutAction]] variants are enumerated one-for-one against the legacy tables. The module also owns the shell's fixed overlay chords, so that the "a configured binding always wins" rule lives next to the table it outranks — see [[client#GPUI Overlays#Overlay Chords Yield To Bindings]].

### Layout Actions

Over 50 variants in the `LayoutAction` enum covering pane, workspace, and tab management, clipboard, scrolling, zoom, and more.

Tab actions: new, Claude Code new/resume, Codex new/resume, close, next, prev, select 1-9. The legacy `new_claude_*` action names remain in config and code and map to Claude Code, while `new_codex_*` opens Codex. Those AI-tab shortcuts start the selected CLI through the user's login shell with `-lic` and `exec`, resolving the shell from `SHELL` first and then the account database so Finder-launched macOS apps still inherit the expected PATH and rc files without first rendering a normal shell prompt. Their working directory follows the `terminal.ai_tab_cwd` setting: the default `pane` inherits the focused pane's CWD like a plain new tab, while `project_root` anchors to the workspace project root when the pane is inside a configured workspace root. Also: pane splits, pane focus/cycling, workspace splits/cycling, copy, paste, settings, find, zoom, equalize, prompt-jump up/down, and jump-to-failure (scroll to the most recent failed command; when no failed command exists, `signal_no_failed_command` instead fires a non-disruptive scrollbar pulse plus a brief focused-pane tab flash and leaves the viewport unchanged).

### Command Palette

The command palette is a GPU-rendered action picker for common window actions, profile switching, and explicit Claude Code and Codex tab actions, opened from a dedicated keybinding and reusing the normal layout-action handlers.

[[crates/scribe-client/src/command_palette.rs#CommandPalette]] owns the query string, active state, and selected row. [[crates/scribe-client/src/main.rs#App#handle_open_command_palette]] populates entries for settings, find, tab and pane actions, new windows, every saved profile from [[crates/scribe-common/src/profiles.rs#list_profiles]], and (when available) an "Update Scribe to v{version}" entry. Selecting an entry routes through [[crates/scribe-client/src/main.rs#App#execute_automation_action]], so command-palette actions and server-forwarded automation stay on the same code path.

The query field accepts clipboard paste (`Ctrl+V` / `Cmd+V`), reading the host clipboard through [[crates/scribe-client/src/main.rs#App#read_clipboard_text]] and inserting via [[crates/scribe-client/src/command_palette.rs#CommandPalette#push_str]] (control characters stripped). The [[client#Client#Remote Control#Connect Picker]] manual `host:port` field shares that read path via `RemoteConnectAction::PasteManual` → [[crates/scribe-client/src/remote_connect.rs#RemoteConnect#append_manual]].

### Mouse Handling

Mouse events are processed for text selection, scrollbar interaction, divider drag, tab drag, prompt bar interactions, and context menus.

Workspace-notes hover-preview hit testing extends this with three rects on `WorkspaceNotesPreviewInteraction`: the "+" affordance (`affordance_rect`, click opens the inline editor), the editor row (`editor_rect`, absorbs clicks so they don't archive notes behind it), and the in-editor wheel routing through [[crates/scribe-client/src/main.rs#App#try_inline_editor_mouse_wheel]] which consumes the wheel inside the editor row.

Selection modes are click-drag for cell, double-click for word or configured Smart Selection, triple-click for line, and quad-click for Smart Selection when configured that way. Scrollbar supports click-to-jump and drag-to-scroll. Divider drag resizes splits with 4px hit tolerance. Tab drag reorders with visual offset.

Click sequencing is tracked by [[crates/scribe-client/src/mouse_state.rs#MouseClickState]], which records each press time and position to classify the event as [[crates/scribe-client/src/mouse_state.rs#ClickKind]] (Single, Double, Triple, or Quadruple). Multi-click is recognized when a press arrives within 400 ms and 5 px of the previous one. The derived [[crates/scribe-client/src/mouse_state.rs#SelectionMode]] (Cell, Word, or Line) follows directly from the click kind. Auto-scrolling during drag is triggered by `edge_scroll_delta` when the cursor enters the 20 px edge zone at the top or bottom of the content area.

OSC 133 `click_events=1` prompt click-to-move is evaluated on mouse release through [[crates/scribe-client/src/main.rs#prompt_click_to_move_displacement]], only when the press/release left an empty selection. Dragging the live prompt row therefore keeps normal text selection, while a plain click can still send arrow-key movement.

### Drag And Drop

Dropped files and directories are pasted into the focused shell using shell-aware quoting, so GUI drag-and-drop becomes a safe path insertion workflow instead of raw bytes.

[[crates/scribe-client/src/main.rs#App#handle_dropped_path]] receives `WindowEvent::DroppedFile`, looks up the focused pane's shell basename, quotes the path for POSIX shells, Fish, PowerShell, or Nushell, and sends it through the normal paste pipeline with a trailing space. Shell basenames come from reconnect metadata and `SessionCreated`, so the quoting mode follows the actual session instead of assuming the user's login shell.

### Mouse Reporting

When a terminal application enables mouse mode, button, motion, and scroll events are encoded as xterm escape sequences and forwarded to the PTY.

#### GPUI Rebuild Golden Oracle

The future GPUI client ports input byte-for-byte from committed old-client captures before this implementation is deleted.

`tests/fixtures/gpui-client/keyboard-byte-golden.json` captures legacy xterm, Kitty CSI-u, DECCKM, and DECPAM bytes. `mouse-byte-golden.json` captures X10, SGR-1006, and the 1000/1002/1003 motion gate. The root test-fixture location survives old-client deletion and is copied into the new crate when that scaffold exists; porting beads load the captures rather than recreate expected strings by hand.

The GPUI reporter lives in [[crates/scribe-client-gpui/src/mouse_reporting.rs#encode_mouse_press]] and its siblings ([[crates/scribe-client-gpui/src/mouse_reporting.rs#encode_mouse_release]], [[crates/scribe-client-gpui/src/mouse_reporting.rs#encode_mouse_scroll]], [[crates/scribe-client-gpui/src/mouse_reporting.rs#encode_mouse_motion]]), retargeted from winit's `MouseButton`/`ModifiersState` to GPUI's `MouseButton`/`Modifiers` but byte-identical on the wire. The motion gate is the pure [[crates/scribe-client-gpui/src/mouse_reporting.rs#should_report_mouse_motion]]; the click-count / selection-mode classifier and edge-scroll helper port verbatim into [[crates/scribe-client-gpui/src/mouse_state.rs#MouseClickState]] and [[crates/scribe-client-gpui/src/mouse_state.rs#edge_scroll_delta]]. A golden byte-capture test replays every `mouse-byte-golden.json` case and the motion-gate truth table against the port.

Encoding lives in [[crates/scribe-client/src/mouse_reporting.rs]]; modifiers go in the Cb field (Shift +4, Alt +8, Ctrl +16) and SGR 1006 vs X10 is chosen per the terminal's `SGR_MOUSE` mode.

Stale mouse modes left behind by a dead foreground program (force-closed SSH, killed TUI) are cleared server-side when the shell prompt returns — the client needs no special handling because the injected DECRST arrives through the normal `PtyOutput` stream; see [[server#Sessions#PTY Reader Task]].

#### Mode Gate

The mouse-mode gate uses `intersects(MOUSE_MODE)`, not `contains`, because `contains` is always false for enabled mouse modes.

`MOUSE_MODE` is a union of three bits (`MOUSE_REPORT_CLICK | MOUSE_MOTION | MOUSE_DRAG`) that alacritty stores mutually exclusively — each DECSET (1000/1002/1003) clears the union then sets exactly one bit, so requiring all three (`contains`) never matches.

[[crates/scribe-client/src/pane.rs#Pane#has_mouse_mode]] and the wheel-handler mode read both use `intersects`. The other mode reads alongside it (`ALT_SCREEN`, `ALTERNATE_SCROLL`, `SGR_MOUSE`) are single-bit and keep `contains`.

#### Held-Button Tracking And Motion De-Duplication

App-forwarding is tracked separately from native selection so mouse-off behavior is unchanged: `mouse_selecting` still drives native click-drag selection, while `mouse_report_button` records the button currently forwarded to the app.

Drag motion (mode 1002) gates on `mouse_report_button.is_some()`, and the reported Cb carries that exact button rather than a hardcoded Left.

`mouse_report_button` is set when a press is forwarded. [[crates/scribe-client/src/main.rs#App#clear_mouse_report_state]] clears it (and `last_mouse_report_cell`) on **every physical button release** — left, middle, and right — even when the release itself could not be forwarded (mode disabled mid-drag, Shift held), so a button-up always ends a forwarded press.

It is also cleared on **focus and pane transitions** via [[crates/scribe-client/src/main.rs#App#notify_focus_change]], the single chokepoint every pane/session/window focus path routes through. A press forwarded to the previously focused pane never sees its matching release, so dropping the tracked button here prevents phantom drag motion being reported to the newly focused pane or after the window regains focus.

The motion gating and per-cell de-dup themselves live in the pure [[crates/scribe-client/src/mouse_reporting.rs#should_report_mouse_motion]]: it reports motion only when mode 1003 (`any_motion`) is set, or mode 1002 (`drag`) is set and a button is held, and only when the pointer has entered a different cell than `last_reported`. [[crates/scribe-client/src/main.rs#App#maybe_forward_mouse_motion]] supplies the live mode bits, the held-button flag, and `last_mouse_report_cell` to it.

`last_mouse_report_cell` is intentionally seeded to the press cell when a press is forwarded (see [[crates/scribe-client/src/main.rs#App#finish_selection_mouse_press]] and the middle/right press arms). This deliberately suppresses the **first** same-cell motion event so the PTY is not flooded while the pointer sits on the press cell — matching alacritty's `cell_changed` semantics. It is not a bug: motion is reported only once the pointer crosses into a new cell.

### Resize Coordination

Window resize coalesces per event-loop tick rather than via a wall-clock debounce, and is flushed ahead of any input bytes so the server sees `Resize` → `KeyInput` in mpsc order.

Every `WindowEvent::Resized` updates the local pane grid and sets `resize_pending`. [[crates/scribe-client/src/main.rs#App#flush_resize_if_pending]] runs in `about_to_wait` (per-tick batching) and from the input call sites — [[crates/scribe-client/src/main.rs#App#handle_terminal_key]], [[crates/scribe-client/src/main.rs#App#send_paste_data]], and [[crates/scribe-client/src/main.rs#App#perform_primary_paste]] — before any `KeyInput` is queued. The shared mpsc `Sender<ClientCommand>` preserves FIFO order, so the server processes `Resize` first; `tcsetwinsize` delivers `SIGWINCH` ahead of the bytes hitting the PTY, and bash updates `COLUMNS` before reading the next command.

This mirrors alacritty/ghostty/wezterm/kitty/vte — none use a wall-clock debounce; all coalesce implicitly per tick or by last-known-size dedup.

### IME Composition

IME is wired via `WindowEvent::Ime` with a per-window `Option<PreeditState>`, gated on focus and surface, suppressing keys only during composition, routing committed text through the existing PTY write path.

The state lives in [[crates/scribe-client/src/preedit.rs#PreeditState]] (composition text, optional caret byte range, and the absolute scrollback row + column where composition started). The state machine: `Enabled` arms the gate; the first non-empty `Ime::Preedit` creates a `PreeditState` anchored at the focused pane's current cursor cell; subsequent `Ime::Preedit` events update the text and caret on the same anchor; `Ime::Commit` clears `PreeditState` before sending the committed bytes through the normal `ClientMessage::KeyInput` path so the preedit overlay disappears in the same frame the PTY echo arrives; an empty-text `Ime::Preedit` or `Ime::Disabled` cancels and clears the state.

#### Activation Gate

The gate predicate is `window_focused && current_surface == TerminalPane`; the result is ANDed with the X11 active-window guard and pushed to `window.set_ime_allowed(...)` whenever it changes.

[[crates/scribe-client/src/main.rs#App#ime_should_be_allowed]] handles the immutable surface check: it returns false when the search overlay, command palette, workspace-notes modal, close / update / context-menu dialogs, or the workspace-notes hover-preview inline editor (FR-005) is active. [[crates/scribe-client/src/main.rs#App#refresh_ime_allowed]] then ANDs that with `!compositor_overlay_active` (the X11 active-window guard, which mutates the post-reactivation debounce) and pushes the result to winit. The pushed value is memoised in `last_ime_allowed` so steady-state ticks (the per-frame `about_to_wait` call on Linux) short-circuit without touching winit IPC; the gate only flips when the computed value differs from the last push. Whenever the gate flips to disallowed, any in-flight `PreeditState` is dropped so focus loss or surface change immediately retires the visual overlay (FR-008) and clears the [[client#Input#Key Translation Priority]] short-circuit.

#### Cursor-Rect Strategy

[[crates/scribe-client/src/main.rs#App#push_ime_cursor_area]] is re-pushed from the redraw path whenever the focused pane's cursor cell coordinates change or the gate state flips, with a memoized last-pushed rect to dedup redundant winit calls.

The cache lives in `last_ime_cursor_area`, so identical frames skip the winit IPC; pushes are suppressed entirely while the window is occluded (mirroring [[client#AI Indicator]]'s occlusion-gate pattern). `WindowEvent::Resized` and `WindowEvent::ScaleFactorChanged` force an immediate fresh push after the resize handler runs, and focused-pane changes invalidate the cache so the popup re-anchors on the new pane.

#### Preedit Rendering

The overlay layers above the terminal grid and below search / dialog overlays — a theme-foreground underline (Alacritty-minimal) anchored at the composition-start cell on a scrollback-stable absolute row, clipped at the pane right edge.

[[crates/scribe-client/src/main.rs#App#preedit_overlay]] computes a `PreeditOverlay { origin_px, cell_px, text, max_cells }` per frame from the saved anchor and the focused pane's current layout, returning `None` while the viewport is scrolled into scrollback (`grid.display_offset() > 0`) so the underline can't render at the wrong visual row. Per-char advances come from `unicode_width::UnicodeWidthChar::width` (matching the renderer's styled-run accumulator), so CJK wide glyphs reserve two cells and zero-width combining marks ride the prior base glyph (a leading combining mark with no base is skipped). [[crates/scribe-client/src/main.rs#App#apply_preedit_overlay]] then emits a background fill behind the preedit cells, one glyph per advance via the existing cosmic-text + atlas path, and a 1px theme-foreground underline (hi-DPI scaled to ≥1 physical pixel) — per `research.md#R4`. Because the anchor uses an absolute scrollback row, terminal scroll keeps the preedit pinned to the originating line.

## IPC Client

The IPC connection runs in a background thread with its own Tokio runtime, defined in [[crates/scribe-client/src/ipc_client.rs#start_ipc_thread]].

### Communication Flow

The main thread sends `ClientCommand` variants through an mpsc channel to the write task for socket serialization.

The write task serializes commands to `ClientMessage` and writes to the socket. The read task deserializes `ServerMessage` responses and dispatches them as `UiEvent` variants through the winit event loop proxy. `UiEvent::PromptReceived` carries session ID, provider, and prompt text for the prompt bar feature.

Automation requests use that same path in both directions. `scribe-cli action ...` becomes [[protocol#Client Messages#Automation]] `DispatchAction`, the server forwards it as [[protocol#Server Messages#Automation]] `RunAction`, and the client executes it through the same handlers the keyboard shortcuts and command palette already use.

### Server Lifecycle

Starts and connects to the server process, with a retry loop waiting up to 5 seconds for the socket to appear.

On Linux, the client starts the server via `systemctl --user start scribe-server`. On macOS, release builds install `com.scribe.server.plist` into `~/Library/LaunchAgents/` with the current bundle's `scribe-server` path, re-bootstrap the job if that path changes, and then `kickstart` it. If a socket already exists, the client inspects the connected server's peer PID and restarts it when the running executable path differs from the current bundle or when the installed server binary is newer than the running process start time, which lets manual DMG replacements hot-reload the background server on next launch. When that stale-server refresh fires, the client prefers a direct `scribe-server --upgrade` spawn over `launchctl kickstart -k` so the new server performs a handoff with the still-running old one; kickstart only terminates the old server when launchd still manages it, and after a DMG drop-replace that old server is typically a launchd orphan whose flock a fresh non-upgrade child would crash-loop against. `launchctl` remains the fallback if the direct spawn fails. Dev builds without a bundle fall back to spawning the server binary directly.

[[crates/scribe-client/src/ipc_client.rs#wait_for_refreshed_server]] polls the socket's peer PID after the `--upgrade` spawn and only returns once the connected server differs from the captured old PID. If the new server fails to take over within `SERVER_REFRESH_TIMEOUT` — most often because the in-process handoff aborted (incompatible state format from a long-deferred update, peer-validation rejection, or any other `--upgrade` exit) — the client falls back to [[crates/scribe-client/src/ipc_client.rs#perform_macos_update_restart]], which force-terminates every scribe-server process other than ourselves (via `pgrep -x scribe-server`), removes stale `server.sock` / `handoff.sock` files, and starts a fresh server through the normal path. Live sessions in the stuck old server are lost on that fallback path, but Scribe launches instead of looping until the wait deadline expires and then crashing.

The same `perform_macos_update_restart` is invoked from [[crates/scribe-client/src/ipc_client.rs#try_cold_restart_recovery]] when [[crates/scribe-client/src/ipc_client.rs#wait_for_server_connection]] times out and `pgrep` still reports a stale scribe-server. This covers the case where the old server is alive but its IPC accept loop has wedged: `tokio::net::UnixStream::connect` returns `ECONNREFUSED`, so the refresh path never runs; the fresh server spawned by `start_server` then can't acquire `flock` because the stuck old process still holds it. The pgrep-based recovery kills the orphan and restarts cleanly. Recovery only runs when pgrep finds a stale process so legitimate slow-startup timeouts still surface as errors.

## Remote Control

Feature 013's connecting side: a remote-dialed client process attaches to another tailnet machine's window over TCP, auto-reconnects after drops, and renders the displaced state when another controller takes the window.

The owning side is [[server#Remote Control]] and the wire contract is [[protocol#Remote Protocol]]. A remote-control window runs as its own process launched with `SCRIBE_REMOTE_DIAL` (and optional `SCRIBE_REMOTE_WINDOW` / `SCRIBE_REMOTE_TAKEOVER`) in its env, spawned by [[crates/scribe-client/src/main.rs#spawn_remote_client_process]] from the connect picker or a reclaim. The owning machine's own status indicators are separate — see [[client#Status Bar#Remote Control Surfaces]].

Feature 014 adds a direct-LAN connecting path (`SCRIBE_LAN_DIAL`, mutual TLS + device approval) beside the tailnet dial — see [[client#Remote Control#LAN Dial]] — plus the owning-side approval prompt this machine raises for an unknown LAN device — see [[client#Remote Control#LAN Approval Prompt]]. The picker merges both peer sources ([[client#Remote Control#Connect Picker]]) and the status bar shows which transport a controlled window uses ([[client#Status Bar#Remote Control Surfaces]]).

Feature 015 turns the connecting side into a live participant in a shared window rather than a sole controller: a shared client keeps receiving live output but, as a viewer, suppresses its own input and offers a take-control affordance, tracks the roster, and shows a status-bar presence badge. The frozen [[crates/scribe-client/src/lost_control.rs#LostControlState]] banner now appears only for `SingleController` displacement, never for a shared join. The default picker attach also becomes a non-takeover additive join. See [[client#Remote Control#Shared Viewer and Control]].

### Remote Dial

When `SCRIBE_REMOTE_DIAL` is set, [[crates/scribe-client/src/ipc_client.rs#start_ipc_thread]] dials the peer's `host:port` over TCP via [[crates/scribe-client/src/ipc_client.rs#start_remote_ipc_thread]] instead of the local Unix socket, running the [[protocol#Remote Protocol#Preamble Handshake]] as the first frame before any `Hello`. The dial parameters travel as one [[crates/scribe-client/src/ipc_client.rs#RemoteDial]].

Unlike the local path this is strictly connect-only — it never starts or upgrades the peer's server. [[crates/scribe-client/src/ipc_client.rs#start_ipc_thread]] returns an [[crates/scribe-client/src/ipc_client.rs#IpcHandle]] carrying the command sender plus, for a remote process only, a [[crates/scribe-client/src/ipc_client.rs#RemoteReconnectCancel]] switch. [[crates/scribe-client/src/ipc_client.rs#remote_dial_target]] lets other code (the reclaim path, status suppression) tell whether this process is a controlling-side window and re-dial the same peer.

### Reconnect State Machine

A dropped remote link auto-reconnects with capped exponential backoff (research D6): [[crates/scribe-client/src/ipc_client.rs#reconnect_with_backoff]] retries up to [[crates/scribe-client/src/ipc_client.rs#RECONNECT_MAX_ATTEMPTS]] times, delay from [[crates/scribe-client/src/ipc_client.rs#backoff_delay]] (500 ms base, 15 s cap), re-running the preamble and re-claiming the SAME window with `Hello { takeover: false }` on each attempt.

Each attempt emits `UiEvent::RemoteReconnecting { attempt }`, driving a cancelable [[crates/scribe-client/src/remote_connect.rs#ReconnectOverlay]] ("Reconnecting to `<peer>`… (attempt n)"); success emits `RemoteReconnected` and exhaustion emits `RemoteReconnectFailed` (one-action reconnect). Because reconnect always uses `takeover = false`, a window reclaimed by someone else mid-outage lands the client in lost control rather than silently re-seizing it (FR-011). Cancel — via the overlay ([[crates/scribe-client/src/main.rs#App#handle_reconnect_overlay_key]]) or the `RemoteReconnectCancel` switch — settles into a disconnected state, and a delivered `RemoteDisconnect` sever notice ends the loop and reports the disable as fact.

Each attempt is cancel-aware end to end (FR-011): [[crates/scribe-client/src/ipc_client.rs#reconnect_with_backoff]] races the whole connect → handshake → `Hello` sequence — run as [[crates/scribe-client/src/ipc_client.rs#try_reconnect_attempt]] returning a [[crates/scribe-client/src/ipc_client.rs#ReconnectAttempt]] — against [[crates/scribe-client/src/ipc_client.rs#wait_for_cancel]], so a Cancel or delivered sever fired mid-attempt drops the in-flight (half-open) stream instead of completing and emitting `RemoteReconnected` over a settled overlay. Because `try_reconnect_attempt` emits no UI events the caller alone decides whether the attempt goes live, and [[crates/scribe-client/src/main.rs#App#handle_remote_reconnected]] now carries the same `is_settled()` overlay guard as [[crates/scribe-client/src/main.rs#App#handle_remote_reconnecting]], so a late success never revives a window the user already settled.

The status-bar connection dot reflects the down link (see [[client#Status Bar]]): `server_connected` flips false in `handle_remote_reconnecting` and [[crates/scribe-client/src/main.rs#App#settle_reconnect_terminal]] — the remote read task deliberately emits no `ServerDisconnected`, so nothing else clears it — and is restored true only by a successful [[crates/scribe-client/src/main.rs#App#reattach_sessions_after_reconnect]], so it stays red through every retry, failure, and severed state.

### Connect Picker

The command palette's "Connect to remote machine…" action ([[crates/scribe-client/src/main.rs#App#open_remote_connect]]) opens the GPU-overlay [[crates/scribe-client/src/remote_connect.rs#RemoteConnect]] picker.

It lists same-account online peers — fetched by a local `ListRemotePeers` request ([[crates/scribe-client/src/main.rs#App#request_remote_peers]] → [[crates/scribe-client/src/remote_connect.rs#RemoteConnect#set_peers]]) — with a manual-entry field, then the chosen peer's window list ([[crates/scribe-client/src/remote_connect.rs#RemoteConnect#set_windows]], workspace names and session counts) plus a "New window" option.

Key handling ([[crates/scribe-client/src/remote_connect.rs#RemoteConnect#handle_key]]) returns a [[crates/scribe-client/src/remote_connect.rs#RemoteConnectAction]] that [[crates/scribe-client/src/main.rs#App#apply_remote_connect_action]] turns into a [[crates/scribe-client/src/main.rs#spawn_remote_client_process]] call: attaching an existing window uses `takeover = true`, creating a new one uses `takeover = false`. Each failure class maps to distinct copy per UX-002 (unreachable / disabled / unauthorized / version / busy / taken-over); [[crates/scribe-client/src/remote_connect.rs#RemoteConnect#on_severed]] surfaces a delivered sever notice. The spawn also clears the inheritable takeover markers on the paths that must not use them — [[crates/scribe-client/src/main.rs#spawn_remote_client_process]] `env_remove`s `SCRIBE_REMOTE_WINDOW` / `SCRIBE_REMOTE_TAKEOVER` on the new-window and non-takeover branches — so a child launched from an already-takeover process never inherits a stale marker and silently re-seizes a window (FR-011).

Feature 015 changes the default attach from a takeover to an additive share join and shows live occupancy, superseding the 013 default above: [[crates/scribe-client/src/main.rs#App#apply_remote_connect_action]] now dials an existing window with `takeover = false` (the owning machine decides additive-join vs lost-control by its mode), reserving `takeover = true` for an explicit reclaim of a window this user lost. Each existing-window [[crates/scribe-client/src/remote_connect.rs#WindowRow]] carries a `participant_count` and `mode` from the enriched [[protocol#Remote Protocol#Window Context Fields|WindowInfo]], rendering "shared · N attached" occupancy when two or more machines are attached instead of 013's binary in-use marker.

Feature 014 merges a "Local network" source into the same picker. [[crates/scribe-client/src/main.rs#App]] requests both peer lists (`ListRemotePeers` + `ListLanPeers`) and feeds them via [[crates/scribe-client/src/remote_connect.rs#RemoteConnect#set_peers]] / [[crates/scribe-client/src/remote_connect.rs#RemoteConnect#set_lan_peers]]; the two are merged into [[crates/scribe-client/src/remote_connect.rs#PeerRow]]s by machine name, each tagged with the [[crates/scribe-client/src/remote_connect.rs#PeerTransport]] it dials over ("Local network" vs. "Tailscale" from [[crates/scribe-client/src/remote_connect.rs#PeerTransport#label]]). A confidently name-matched dual-reachable machine appears once with the direct LAN path preferred (FR-008), while unmatched peers may appear once per transport, each labeled. [[crates/scribe-client/src/remote_connect.rs#RemoteConnect#handle_key]] returns a [[crates/scribe-client/src/remote_connect.rs#RemoteConnectAction]] whose `ProbeWindows` / `Attach` / `NewWindow` variants now carry the chosen transport; [[crates/scribe-client/src/main.rs#App#apply_remote_connect_action]] routes a LAN choice through [[crates/scribe-client/src/main.rs#App#probe_windows]] and [[crates/scribe-client/src/main.rs#spawn_client_for_transport]] → [[crates/scribe-client/src/main.rs#spawn_lan_client_process]] (setting `SCRIBE_LAN_DIAL`), which runs the full TLS + device-approval gate. Manual `host:port` entry is retained for both transports.

### Displaced and Lost Control

When the server sends `WindowTakenOver`, [[crates/scribe-client/src/main.rs#App#handle_window_taken_over]] builds a [[crates/scribe-client/src/lost_control.rs#LostControlState]]: the last frame is dimmed and frozen under a banner naming the new controller's device and account, with one-action reclaim.

Under feature 015 this frozen path is scoped to `SingleController` displacement — an exclusive takeover or a mode flip to `SingleController` (FR-003/017) — because a shared join or single-typist control pass keeps the displaced machine live and drives the roster instead ([[client#Remote Control#Shared Viewer and Control]]).

Reclaim ([[crates/scribe-client/src/main.rs#App#reclaim_window]]) re-claims the same window with `takeover = true`, but the two sides differ. The LOCAL owning-machine reclaim ([[crates/scribe-client/src/main.rs#App#reclaim_window_local_in_place]]) is IN PLACE: it drops the displaced command sender — closing that connection cleanly, with no spurious `ServerDisconnected` because the local [[crates/scribe-client/src/ipc_client.rs#ipc_main]] select aborts the read task once the write task ends — then starts a fresh local IPC thread via [[crates/scribe-client/src/ipc_client.rs#start_local_takeover_ipc_thread]] whose first `Hello` swaps the writer back, clears the banner, and KEEPS the window (no close+reopen, geometry already correct). The REMOTE controlling-side reclaim still re-dials the peer in a fresh process ([[crates/scribe-client/src/main.rs#spawn_client_for_transport]]) and closes this window — a documented follow-up to make it in-place too. Either way a short grace timer defers the pane reattach ([[crates/scribe-client/src/main.rs#App#flush_reconnect_reattach_if_due]]) so a displacing `WindowTakenOver` can land first: a reclaim that lost the race resolves cleanly to lost control (the reattach is skipped) instead of clobbering the new controller.

### Shared Viewer and Control

Feature 015's connecting-side sharing: a shared client stays live but, as a viewer, suppresses its own input and offers a take-control affordance. The roster arrives as `ShareRoster`, driving both the affordance and the presence badge (FR-006).

[[crates/scribe-client/src/main.rs#App#handle_share_roster]] stores the incoming roster as a [[crates/scribe-client/src/share_view.rs#ShareState]], and [[crates/scribe-client/src/main.rs#App#resolve_share_self_id]] matches this connection to its own entry by `Welcome.participant_id` (falling back to the `is_local` / device-name entry) so [[crates/scribe-client/src/main.rs#App#share_holder_is_me]] is exact. [[crates/scribe-client/src/main.rs#App#is_share_viewer]] is true only in `SharedSingleTypist` while this client is NOT the holder; when it holds, or in free-for-all, input flows normally. A viewer's input is suppressed at three choke points so nothing the server would drop is ever sent: keystrokes ([[crates/scribe-client/src/main.rs#App#intercept_viewer_key]]), paste, and the raw/synthetic byte path ([[crates/scribe-client/src/main.rs#App#send_bytes_to_focused_pane]], which also covers mouse-reporting sequences).

Instead of typing, a viewer's key raises a non-intrusive [[crates/scribe-client/src/share_view.rs#ControlHint]] naming the current holder and how to take control ([[crates/scribe-client/src/main.rs#App#show_control_hint]]); pressing Enter while it shows calls [[crates/scribe-client/src/main.rs#App#claim_control]], which sends `ControlClaim`. The client sends the same frame whether the owner's policy is free-claim or request-and-grant and learns the result from the next `ShareRoster` (granted) or a `ControlDenied`. When this client is the approver under request-and-grant, an incoming `ControlRequested` becomes a modal [[crates/scribe-client/src/share_view.rs#ControlRequestPrompt]] resolved by [[crates/scribe-client/src/main.rs#App#resolve_control_request]] (Enter grants, Esc denies) into a `ControlGrant` naming the requester. [[crates/scribe-client/src/main.rs#App#handle_share_ended]] surfaces a `ShareEnded` notice when the owner ends the share.

### LAN Dial

Feature 014's LAN connecting path: when `SCRIBE_LAN_DIAL` names a peer, the client dials `host:port` over mutual TLS instead of the tailnet TCP path, running the [[protocol#Remote Protocol#LAN Preamble and Approval]] handshake before any `Hello`.

[[crates/scribe-client/src/ipc_client.rs#lan_dial_target_from_env]] reads the env; [[crates/scribe-client/src/ipc_client.rs#build_lan_tls]] obtains this machine's own [[crates/scribe-server/src/lan/identity.rs#DeviceIdentity]] — the client links the `scribe-server` crate's LAN identity/TLS layer rather than a separate stack — and builds the [[crates/scribe-client/src/ipc_client.rs#LanDialer]] that presents it. Crucially the identity is FETCHED from this machine's own local server over the local-socket-only [[protocol#Remote Protocol#LAN Trust and Discovery Helpers|GetLanDialIdentity]] exchange ([[crates/scribe-client/src/ipc_client.rs#fetch_lan_dial_identity]]) and rebuilt via [[crates/scribe-server/src/lan/identity.rs#DeviceIdentity#from_der]], never reading the OS keyring from the client binary: on macOS the keyring's legacy `SecKeychain` per-item ACL trusts only the creating binary (`scribe-server`), so a cross-binary key read is denied and the server stays the sole keychain accessor. Any fetch failure (server down, identity unavailable, malformed reply) fails closed to a `ConnectionFailure` outcome, so the dial never proceeds without a valid identity. [[crates/scribe-client/src/ipc_client.rs#lan_ipc_main]] drives the connection: [[crates/scribe-client/src/ipc_client.rs#lan_handshake]] runs the TLS connect + `LanHello` (bundled as a [[crates/scribe-client/src/ipc_client.rs#LanDial]]), [[crates/scribe-client/src/ipc_client.rs#connect_and_attach_lan_initial]] completes the first attach, and [[crates/scribe-client/src/ipc_client.rs#run_lan_session]] hands the split TLS stream to the transport-agnostic session loop shared with the tailnet path.

While the owner holds an unknown device pending approval the dialer receives `LanApprovalPending` and emits [[crates/scribe-client/src/ipc_client.rs#UiEvent]]`::LanAwaitingApproval` — a cancelable "waiting for approval on `<peer>`" state (FR-014, US2.5); the terminal result surfaces exactly once as `UiEvent::LanDialOutcome` carrying a [[crates/scribe-client/src/ipc_client.rs#LanConnectOutcome]] whose refusal copy mirrors the [[crates/scribe-common/src/protocol.rs#LanRefusal]] taxonomy. A dropped LAN link auto-reconnects like the tailnet path via [[crates/scribe-client/src/ipc_client.rs#lan_reconnect_with_backoff]] ([[crates/scribe-client/src/ipc_client.rs#try_lan_reconnect_attempt]] returning a [[crates/scribe-client/src/ipc_client.rs#LanReconnectAttempt]]), re-claiming the same window with `takeover = false` so a window reclaimed mid-outage lands in lost control (FR-011).

### LAN Approval Prompt

The owning side of feature 014: when this machine's server holds an unknown LAN device pending approval, this client renders a GPU-overlay dialog showing the device's fingerprint before any data flows (SEC-001, UX-002).

[[crates/scribe-client/src/main.rs#App]] receives [[crates/scribe-client/src/ipc_client.rs#UiEvent]]`::LanApprovalRequest` and builds a [[crates/scribe-client/src/lan_approval.rs#LanApprovalDialog]] ([[crates/scribe-client/src/lan_approval.rs#LanApprovalDialog#new]]) naming the requesting device, its fingerprint words, and the trusted network it arrived on, with equally prominent Approve / Decline. When the advertised name collides with an already-trusted device the dialog adds the `name_collision` informational hint ("approve only if you recognize this one"). [[crates/scribe-client/src/main.rs#App#handle_lan_approval_action]] turns the [[crates/scribe-client/src/lan_approval.rs#LanApprovalAction]] into a [[crates/scribe-client/src/ipc_client.rs#ClientCommand]]`::LanApprovalDecision` echoing the request id, routed to the local server by [[crates/scribe-client/src/ipc_client.rs#dispatch_lan_approval_message]]. [[crates/scribe-client/src/main.rs#App#apply_lan_approval_overlay]] and the keyboard / click / hover handlers drive the dialog; at most one is in flight at a time.

## Selection

Text selection in [[crates/scribe-client/src/selection.rs]] supports three modes: Cell, Word, and Line. Coordinates are absolute grid positions.

Cell selects individual characters. Word boundaries include alphanumeric, underscore, dash, dot, slash, tilde, at, plus, percent, hash, question, ampersand, and equals, and double-click word scans cross WRAPLINE-connected rows so soft-wrapped paths or commands stay contiguous. Line mode follows WRAPLINE flags for logical lines. [[crates/scribe-client/src/selection.rs#pixel_to_grid]] converts mouse pixel coordinates to grid positions, subtracting tab bar height, prompt bar height (position-aware), and content padding before dividing by cell size. During an active drag, [[crates/scribe-client/src/selection.rs#pixel_to_grid_clamped]] clamps points that stray into prompt-bar chrome or outside the pane back to the nearest visible terminal cell so the last visible row still highlights.

### Smart Selection

Smart Selection extends click selection with configurable semantic regex matching over the visible wrapped logical line.

[[crates/scribe-client/src/smart_selection.rs]] compiles the global `terminal.smart_selection` rules and maps regex byte ranges back to terminal grid cells. A candidate must contain the clicked cell. For each rule, the longest containing match is kept; the final selected candidate comes from the highest precision class with any match, then the longest match in that class. [[crates/scribe-client/src/main.rs#App#start_selection_smart]] reuses normal `SelectionRange` highlighting and copy-on-select behavior.

Built-in recognizers come from [[crates/scribe-common/src/config.rs#default_smart_selection_rules]] and are restored via `terminal.smart_selection.reset`. The defaults include a `whitespace_word` fallback at VeryLow precision, while higher-precision recognizers (paths, URIs, emails, quoted strings, namespace identifiers) still win when they match.

The default activation is quad-click, preserving double-click word and triple-click line selection. When activation is set to double-click, Smart Selection replaces ordinary double-click word selection and falls back to word selection only when no rule matches. Shift still bypasses mouse-reporting applications before local selection starts.

Right-click context menus run Smart Selection at the pointer. Matching rules with actions add explicit menu items; selection alone never executes them. Action parameters support iTerm2-style legacy substitutions (`\0`, `\1`-`\9`, `\d`, `\u`, `\h`, `\n`, and `\\`) and interpolated strings such as `\(matches[0])`, `\(path)`, `\(user)`, and `\(host)`.

### Scroll Adjustment

Selection coordinates are adjusted when PTY output or resize shifts grid content via `history_size` delta.

[[crates/scribe-client/src/main.rs#App#shift_active_selection]] shifts the active selection and drag anchors. [[crates/scribe-client/src/main.rs#App#shift_background_tab_selection]] handles saved selections on background tabs. Selections that move past `topmost_line` are cleared.

## Scrollbar

An overlay scrollbar in [[crates/scribe-client/src/scrollbar.rs#ScrollbarState]] that fades in on scroll and fades out after 1.5s of inactivity.

Width animates on hover via lerp expansion. The hit zone is 3x the visible width for easy targeting. Drag-to-scroll computes offset from mouse delta relative to track height. Fade-out duration is 0.3 seconds.

### Prompt Mark Indicators

Each [[crates/scribe-client/src/pane.rs#Pane]]`::command_records` entry renders as a 2px scrollbar tick at `abs_pos / (history_size + screen_lines)`, coloured by command status: neutral for `Unknown`, theme-derived hues for `Success`/`Failure`.

`command_records` (a `Vec<CommandRecord { abs_pos, status }>`) supersedes the old flat prompt-mark list. OSC 133 `D` exit codes now reach the client: [[crates/scribe-client/src/ipc_client.rs#UiEvent]]`::PromptMark` carries `exit_code`, and `handle_prompt_mark` runs the A→D state machine (`A` opens an `Unknown` record; `D` resolves the most-recent open one — exit 0 `Success`, non-zero `Failure`, absent stays `Unknown`, never falsely `Failure`). The authoritative, accessible cue is a non-colour glyph (`✓`/`✗`/`?`) in the [[client#Client#Status Bar]] (`Pane::last_command_status`); the scrollbar colour is a redundant secondary hint.

Records are stored as absolute scrollback positions (lines from the very top of scrollback, 0 = oldest). When scrollback shrinks — via [[crates/scribe-common/src/protocol.rs#ServerMessage]]`::TrimScrollback` during AI redraw epochs, or natural overflow at the configured `scrollback_lines` cap — surviving rows shift down in absolute index. `handle_trim_scrollback_event` calls [[crates/scribe-client/src/pane.rs#shift_absolute_marks_after_trim]] to keep indicators aligned with their original command rows (each record's `abs_pos` shifts; per-record status preserved); the scrollbar render path additionally clamps any residual stale abs to the track bounds so a mark from a not-yet-shifted shrink path cannot draw outside the track. On reattach/cold-restart `command_records` starts empty (replay reproduces cells, not OSC 133 callbacks) — historical rows show no status rather than a fabricated one.

## Dividers

Pane split dividers in [[crates/scribe-client/src/divider.rs]] are 1px solid quads with a 4px hit tolerance for drag resize.

Focus borders are rendered as 2px accent-colored quads on the focused pane's leading edge. Workspace focus borders render as four thin quads around the entire workspace rect.

## AI Indicator

The [[crates/scribe-client/src/ai_indicator.rs#AiStateTracker]] tracks per-session AI state with pulsing border animations.

The shared animation loop uses a generation token per spawned thread, so fast stop/start cycles from AI pulses, scrollbar fades, or stalled-sync recovery retire older timer threads instead of letting them keep emitting `AnimationTick`. The AI-pulse contribution is additionally bounded by a [[client#AI Indicator#Pulse Envelope]] so a long-lived AI state cannot keep the loop alive — and the GPU busy — indefinitely.

Priority order: PermissionPrompt > WaitingForInput > IdlePrompt > Error > Processing. Each state has configurable color, pulse frequency, tab indicator, and pane border settings. Error state decays over a timeout. Attention states (IdlePrompt, WaitingForInput, PermissionPrompt) clear on keystroke. Both `IdlePrompt` and `WaitingForInput` share the same `waiting_for_input` indicator config (color, pulse, timeout).

Tab inline context % is gated via [[crates/scribe-client/src/ai_indicator.rs#AiStateTracker#tab_context_suffix]]; see [[client#Tab Bar]] for the gating rules and rendering details. The percent itself lives in a parallel `last_contexts` map alongside `detected_providers`, so a border-clear (stale-Processing prune, attention-state keystroke clear, Error decay) does not drop it — see [[client#AI Indicator#Context Survives State Clears]]. The map is cleared explicitly on session removal and on conversation change via [[crates/scribe-client/src/ai_indicator.rs#AiStateTracker#clear_context]], called from [[crates/scribe-client/src/main.rs#App#maybe_reset_prompts_on_conversation_change]].

On reconnect, active AI state is populated from `SessionInfo.ai_state` during handle_session_list so indicators appear immediately without waiting for the per-session `AiStateChanged` messages from the server's `send_stored_metadata` path. `SessionInfo.ai_provider_hint` is restored separately so clipboard cleanup and other provider-aware behavior survive reconnect even when no visible indicator should be shown. When available, `SessionInfo.ai_state.conversation_id` is also used to seed per-pane AI resume bindings so restored windows attempt targeted resume of prior provider sessions.

### Pulse Envelope

Pulse lifetime is decoupled from AI-state lifetime so a stuck or idle session cannot pin the shared 30 fps redraw loop — and the GPU — forever.

The policy gate is [[crates/scribe-client/src/ai_indicator.rs#AiStateTracker#pulse_is_active]], consulted by both `needs_animation` (whether the shared loop may retire) and `animated_color` (pulsing vs. a steady resting colour). Attention states (`IdlePrompt`/`WaitingForInput`/`PermissionPrompt`) pulse for a bounded window after entry, then rest while still tracked and visible; they still clear instantly on keystroke. `Processing` pulses only while *alive* — within an idle window of the last liveness signal. Liveness is a state edge or fresh PTY output recorded via [[crates/scribe-client/src/ai_indicator.rs#AiStateTracker#note_activity]], fed from [[crates/scribe-client/src/main.rs#App#handle_pty_output]].

A genuinely-working session keeps re-arming the envelope across hook-silent tool calls; a hung AI on a still-open PTY goes silent, the pulse rests, and the loop retires to winit `ControlFlow::Wait` at zero GPU. When output resumes for a rested session the loop is restarted from `handle_pty_output`. Envelope durations are `ATTENTION_PULSE_SECS` and `PROCESSING_IDLE_PULSE_SECS` in [[crates/scribe-client/src/ai_indicator.rs]].

#### Stale-State Clear

A rested pulse still shows its state's *colour*. A crashed or killed AI would otherwise show a stale `Processing` border forever: it can never fire its own terminal hook, and the server supervises only the shell.

[[crates/scribe-client/src/ai_indicator.rs#AiStateTracker#clear_stale_processing]] removes any `Processing` state with no liveness (hook edge or PTY output) for `STALE_PROCESSING_CLEAR`. It uses a wall-clock map (`last_activity_instant`) rather than the f32 animation clock, which freezes once the loop retires — the very case this must still catch. The client calls it lazily from [[crates/scribe-client/src/main.rs#App#about_to_wait]]: zero cost until something is stuck, and resolved before the indicator is observed (the user returning wakes the loop). Only `Processing` is cleared — attention states legitimately persist until the human acts — and `detected_providers` plus `last_contexts` are preserved so provider-aware clipboard cleanup survives and the context % stays visible in tabs and prompt bars, mirroring reconnect.

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

### Context Survives State Clears

Context-window percentage is tracked separately from transient AI state so UI clears do not hide still-current context pressure.

#### context_survives_stale_processing_clear

Verifies that a stale `Processing` clear removes the transient state while preserving the last context percent for the tab suffix.

#### context_survives_attention_keystroke_clear

Verifies that keystroke-driven attention-state clearing preserves the last context percent and allows the tab suffix to reappear.

#### clear_context_wipes_for_conversation_change

Verifies that an explicit context clear removes the stored percent when the pane moves to a different conversation.

#### context_remove_clears_last_context

Verifies that removing a session also removes its stored context percent so no stale tab suffix survives session teardown.

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

The status bar is rendered at the bottom of the window with segments for connection status, command status, workspace info, CWD, git branch, session count, time, and system stats.

Update availability and progress also render here, centered in the empty span between the left and right segments — see [[crates/scribe-client/src/status_bar.rs#centered_start_col]] — so the CTA stays visible on narrow windows and steps down to a shorter `↑ Update` label, then disappears entirely, only when the empty span cannot hold it. Clicking the update segment opens the in-app confirmation dialog.

Connection is indicated by a green/red dot. A command-status glyph (`✓`/`✗`/`?`) sits next to it, reflecting the focused pane's most recent command outcome from shell integration (Success/Failure/Unknown); it is the authoritative non-colour cue and stays hidden until the first command resolves. When env-persistence is enabled and the focused pane's [[crates/scribe-client/src/pane.rs#Pane]]`::env_status` is `Some(Degraded { .. })`, a `⚠` (U+26A0) warning glyph in the palette's warning slot renders immediately to the right of the command-status indicator (see [[crates/scribe-client/src/status_bar.rs#render_env_status_warning]]); `None` and `Some(Active)` render nothing. The glyph's hover tooltip directs the user to retry from Settings → Terminal → General. Workspace name appears when multi-workspace. The focused pane's remote host overrides the local hostname when shell integration emits session context, and tmux session names render as a separate accent segment. Stats include CPU sparkline, memory percentage, GPU sparkline (Linux only), and network sparklines.

### Remote Control Surfaces

Feature 013 (T022) adds two owning-machine remote indicators to the left side, between the env-warning glyph and the workspace name — see [[crates/scribe-client/src/status_bar.rs#render_remote_status]].

A persistent subtle `⇅` (U+21C5, dimmed) shows while this machine allows remote control (`remote.enabled`, FR-009a). While a remote peer controls any window, a prominent accent segment names the controller(s) and window counts (e.g. `laptop-2 controls 1 window`, FR-009b) built by [[crates/scribe-client/src/status_bar.rs#build_remote_control_summary]], with an account-naming tooltip from [[crates/scribe-client/src/status_bar.rs#remote_control_tooltip]]. Both fields live in [[crates/scribe-client/src/status_bar.rs#RemoteStatusData]] on [[crates/scribe-client/src/status_bar.rs#StatusBarData]].

The controller list is fed by polling the local server's window list every [[crates/scribe-client/src/main.rs#WINDOW_LIST_POLL_INTERVAL]] (2s) while `remote.enabled`: [[crates/scribe-client/src/main.rs#App#poll_window_list_if_due]] issues [[crates/scribe-client/src/ipc_client.rs#ClientCommand]]`::ListWindows`, and [[crates/scribe-client/src/main.rs#App#handle_local_window_list]] caches the windows with `controller = Some` from the [[crates/scribe-client/src/ipc_client.rs#UiEvent]]`::LocalWindowList` reply. Because the list comes from the server, it covers remotely-created windows that never had a local client (SC-006). Controlling-side (remote-dialed) windows suppress both surfaces, and disabling remote clears the cache via [[crates/scribe-client/src/main.rs#App#apply_remote_status_config]].

Feature 014 (T025) adds a transport indicator on the CONTROLLING side: a controlling-side window carries the [[crates/scribe-client/src/remote_connect.rs#PeerTransport]] it dials over on [[crates/scribe-client/src/main.rs#App]]`::remote_transport`, and [[crates/scribe-client/src/status_bar.rs#StatusBarData]]`::remote_transport` renders a persistent right-side `⇅ Local network` / `⇅ Tailscale` segment via [[crates/scribe-client/src/status_bar.rs#render_right_segments_with_tooltips]], so the user always sees which path a controlled window uses (FR-009). An owning or ordinary local window leaves it `None` and renders nothing.

Feature 015 adds a shared-window presence badge on the connecting side, fed by the roster: while a [[crates/scribe-client/src/share_view.rs#ShareState]] has two or more participants, a [[crates/scribe-client/src/status_bar.rs#SharePresenceData]] on [[crates/scribe-client/src/status_bar.rs#StatusBarData]] renders a badge naming the attached count and current holder ([[crates/scribe-client/src/status_bar.rs#share_presence_badge]]) with a device/account tooltip ([[crates/scribe-client/src/status_bar.rs#share_presence_tooltip]]), so every participant sees who is attached and who is driving (FR-008). It clears when the share drops to a single machine.

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

### Paste Confirmation Dialog

An opt-in confirmation that gates a risky paste before any byte reaches the PTY (spec 011). Off by default via `terminal.paste_confirmation`; fires only when the focused pane has not enabled bracketed paste.

[[crates/scribe-client/src/paste_confirmation_dialog.rs#classify_paste]] is a pure classifier returning [[crates/scribe-client/src/paste_confirmation_dialog.rs#PasteRisk]] — `Some` when the text has a line break (`\n`/`\r`) or a non-tab control/escape byte (C0 except tab/LF/CR, DEL, C1), else `None`. The gate in [[crates/scribe-client/src/main.rs#App#send_paste_data]] consults it after `prepare_paste_target`, and only when `terminal.paste_confirmation` is set and the target is unbracketed; the disabled flag short-circuits first so an off configuration takes the exact prior path (zero added cost). Keybinding and context-menu paste both flow through `send_paste_data` (gated); middle-click `perform_primary_paste` now routes through it too, gaining the gate and >4 KiB chunking. Drag-and-drop file insertion and the context-menu "Run command" action use [[crates/scribe-client/src/main.rs#App#send_paste_data_ungated]] so they stay ungated (FR-013).

[[crates/scribe-client/src/paste_confirmation_dialog.rs#PasteConfirmationDialog]] is a sibling of the close/update/clipboard dialogs sharing the same `DialogLayout` / `DialogRenderer` / `CellInstance` pipeline. It parks the raw paste text plus the resolved [[crates/scribe-client/src/main.rs#PasteTarget]] and shows a reason line (line count / control-character count) above a caret-escaped preview — control bytes render as `^[`, `^M`, or `\u{NN}` so the preview can never drive the terminal (FR-005 / SC-008). Two buttons: **Cancel** (index 0, default focus, also Esc) and **Paste** (Enter on focus; Tab cycles). On Paste, [[crates/scribe-client/src/main.rs#App#handle_paste_confirmation_action]] delivers the parked bytes verbatim through the shared [[crates/scribe-client/src/main.rs#App#send_paste_resolved]] tail (byte-identical to the disabled path), or drops safely if the parked session has closed; the parked decision is honored even if the setting is toggled off while the dialog is open. Cancel sends nothing. No protocol/IPC change — the content and the bracketed-paste signal are both client-side.

## URL Detection

The [[crates/scribe-client/src/url_detect.rs#PaneUrlCache]] scans visible terminal content for URLs (https, http, ftp, file, mailto, ssh, and telnet schemes) and file-system paths.

Soft-wrapped rows are joined by `WRAPLINE` before scanning so a link split across terminal rows remains one clickable span. Trailing punctuation is stripped respecting bracket pairs. Detected spans are cached and invalidated on content change. Each span carries a `SpanKind` (`Osc8Hyperlink`, `Url`, or `Path`). OSC 8 hyperlinks take precedence over heuristic URL/path detection on overlapping cells (see [[client#URL Detection#Explicit Hyperlinks]] below).

Every span also carries exact per-row geometry as [[crates/scribe-client/src/url_detect.rs#RowSegment]]s — spans are not rectangles, because a hard-break continuation row starts at its indent rather than column 0 and merged OSC 8 runs can have partial middle rows. Hit-testing (`contains_cell`), OSC 8 masking, and the hover underline all consume segments; the bounding `row`/`col_start`/`row_end`/`col_end` fields remain for identity comparison and ordering.

URL highlighting and the pointer cursor are only shown while the Ctrl modifier is held. The `ModifiersChanged` handler triggers a redraw and cursor update so visual feedback is immediate. Only the clickable span under the cursor is underlined; wrapped spans draw one underline segment per row (one quad per `RowSegment`). Ctrl+click opens the span via `xdg-open` on Linux or `open` on macOS. File paths support an optional `:N` line-number suffix; when present, `code --goto path:N` is tried first and `xdg-open` is the fallback. Relative paths are resolved against the pane's OSC 7 CWD, and `~/` is expanded using `$HOME`.

### Hard-Break Continuation

Joining a URL split by a program-side line break, where no `WRAPLINE` flag connects the rows and the logical-line join cannot fire.

Programs that lay out their own text (Claude Code's Ink renderer, pagers, shell line editors) hard-wrap long URLs with an explicit newline instead of letting the terminal soft-wrap, so without this pass the URL is detected truncated at the break.

The URL scanner joins across hard breaks: whenever a heuristic URL match is the last content on its row (only blank filler follows it to the end of the logical line), [[crates/scribe-client/src/url_detect.rs#extend_url_across_hard_breaks]] fetches the logical line below and consults [[crates/scribe-client/src/url_detect.rs#hard_break_continuation_start]] — the join policy — with a `HardBreakContext` (the break column vs the grid's last column, the broken row's cells, and the full next line). The policy returns the column where the URL body resumes (cells before it, e.g. a program-drawn gutter indent, stay outside the span) or `None` to refuse. Joins are capped at `MAX_HARD_JOIN_ROWS` explicit breaks; soft-wrapped rows inside a continuation line do not count against the cap. A continuation never absorbs cells covered by an OSC 8 span (FR-004) — the appended run is cut at the first such cell.

The policy is modelled on kitty, the only major terminal that bridges hard breaks by default (its `url_excluded_characters` docs allow newlines "to accommodate programs such as mutt"; mid-row breaks were declined in kitty#2927): a break is bridged only when the URL ran **exactly to the terminal edge** and the next row resumes with URL characters at column 0. iTerm2 offers the same behaviour opt-in (`ignoreHardNewlinesInURLs`, default off); Alacritty, WezTerm, VTE/GNOME Terminal, Windows Terminal, and Konsole all treat a hard end-of-line as an absolute barrier (their search haystacks insert `\n` exactly at non-wrapped row ends), with OSC 8 as the ecosystem's sanctioned producer-side fix. Scribe extends the kitty rule in three guarded directions: a continuation behind a program-drawn gutter (e.g. Claude Code's banner bar `▏ `) is accepted when the broken row carries the identical gutter run ([[crates/scribe-client/src/url_detect.rs#matching_gutter_len]]); a pure-space table-alignment indent is accepted up to a separate 32-column cap even when the broken row has content at that prefix ([[crates/scribe-client/src/url_detect.rs#whitespace_alignment_indent]]); and a next row that starts its own scheme prefix is a new link, never a continuation. The admitted false-positive class — a flush-to-edge URL followed by a column-0 word joins — is exactly kitty's default behaviour; kitty's opt-out precedent (`url_excluded_characters "\n"`) is the model if a config toggle is ever wanted.

Cells consumed as continuation tails are masked from later line scans ([[crates/scribe-client/src/url_detect.rs#cell_in_existing_span]]) so a joined tail such as `articles/15424964` is not re-matched as a fresh bare path on its own row.

### Explicit Hyperlinks

OSC 8 explicit hyperlinks are surfaced as a distinct `SpanKind::Osc8Hyperlink` so the displayed text can be inspected separately from the destination URI. Parsing lives upstream in `alacritty_terminal`'s VTE Perform impl.

[[crates/scribe-client/src/url_detect.rs#scan_osc8_hyperlinks]] is the client-side discovery pass: it iterates the visible grid and emits `UrlSpan { kind: Osc8Hyperlink, url, .. }` values from contiguous cells sharing the same hyperlink. Same-id, same-URI runs that restart within one row merge while retaining exact per-run `RowSegment` geometry, so unlinked gutter or blank filler does not break hover coverage. The pass runs **before** the heuristic pass; the heuristic pass then skips cells already covered by an OSC 8 span (FR-004 precedence) but continues to function unchanged for cells outside one (FR-014). [[crates/scribe-client/src/url_detect.rs#PaneUrlCache#url_at]] returns `Osc8Hyperlink` before `Url` and `Path` on overlap.

The `id=` parameter reconnects same-URI multi-segment hyperlinks separated by unlinked cells when their runs are adjacent or on consecutive rows, including wrapped lines and program-side hard breaks. A later or different-URI run remains separate; anonymous same-URI links also remain separate because upstream gives each open its own id.

URI length is capped at 2048 bytes (FR-010). Upstream VTE (in the `std` build Scribe uses) does not cap OSC sequence length, so Scribe applies the cap in `scan_osc8_hyperlinks` itself; URIs longer than the cap are treated as absent and the affected cells fall back to the heuristic detector.

### Hyperlink Hover Tooltip

When the cursor settles on a cell carrying an OSC 8 URI for ≥300 ms with no movement, the verbatim URI is rendered through the existing [[crates/scribe-client/src/tooltip.rs#render_tooltip]] above or below the cell.

[[crates/scribe-client/src/main.rs#App#apply_osc8_hover_tooltip_overlay]] is the render-loop hook. `Position::Below` is preferred and flipped to `Above` when the cell is on the bottom row. The URI is cached on `App.hover_tooltip_uri` at dwell-threshold time so subsequent frames render without re-reading `cell.hyperlink()`. Truncation only affects what is *displayed* — long URIs render with a **head + tail** view (`prefix...suffix`) via `osc8_tooltip_truncate` so domain-confusion suffixes stay visible to the user; the full URI is preserved on the span for activation. The dwell state lives on `App.hover_cell` / `hover_started_at` / `hover_tooltip_visible` / `hover_tooltip_uri` and resets whenever the cursor moves to a different cell or leaves the terminal area.

Cells without an OSC 8 hyperlink never trigger the dwell path, so the tooltip does not surface heuristic URLs (those continue to use the established Ctrl+highlight affordance only).

### Disallowed-Scheme Confirmation

Activation of an OSC 8 hyperlink whose scheme is outside the existing outbound allowlist routes through a confirmation dialog instead of opening unprompted or being silently blocked.

[[crates/scribe-client/src/disallowed_scheme_dialog.rs#DisallowedSchemeDialog]] is a sibling of the close and update dialogs that shows the URI (head+tail truncated for long URIs so the tail stays visible) plus a "scheme normally blocked" warning. **Cancel** (default focus, also bound to Esc) dismisses without opening; **Open Anyway** routes the URI through `url_detect::open_uri_unguarded`. Allowed-scheme hyperlinks bypass the dialog entirely so the common-case activation latency is unchanged.

[[crates/scribe-client/src/main.rs#App#route_osc8_activation]] is the single helper that performs the allowlist branch and emits a `tracing::debug!`/`tracing::info!` line per decision. The OSC-8 activation paths funnel through it via the dedicated `ContextMenuAction::OpenOsc8Url` variant (right-click "Open URL" item and smart-selection `OpenUrl` rewriting) plus the Ctrl+click handler. Heuristic-URL `ContextMenuAction::OpenUrl` actions continue to flow directly through `url_detect::open_url` so the pre-009 silent-drop behaviour for non-allowlisted heuristic schemes is preserved.

### Clipboard Dialog

OSC 52 read and write requests issued by PTY-side programs surface through an in-app confirmation dialog whose chrome mirrors the disallowed-scheme dialog verbatim (spec 010 / [[protocol#Server Messages#Clipboard Variants]]).

[[crates/scribe-client/src/clipboard_dialog.rs#ClipboardDialog]] is a sibling of the disallowed-scheme dialog rendered through the same `DialogLayout` / `DialogRenderer` / `CellInstance` quad pipeline. It is opened by the `ServerMessage::ClipboardPromptRequest` handler on `App` and dismissed when the user chooses an action; the choice is forwarded to the server as `ClientMessage::ClipboardPromptResponse` carrying the matching `PromptId`. Wave 4 surfaces all four buttons in a single row, left to right: **Deny once** (default focus, also bound to Esc), **Always deny**, **Allow once**, **Always allow**. Tab cycles forward across all four; Shift+Tab cycles back. The two `Always*` variants both resolve the single in-flight prompt AND persist the corresponding policy axis to disk: [[crates/scribe-client/src/main.rs#persist_clipboard_policy_axis]] loads the current `ScribeConfig`, writes `terminal.clipboard.read_mode` or `terminal.clipboard.write_mode` (per the dialog's `op`) to the matching `"allow"` / `"deny"` value, and saves the config back through `scribe_common::config::save_config`. The on-disk change is observed by the server's existing config-file watcher and rebroadcast as `ConfigReloaded`, which lands as a `ClipboardCommand::RefreshPolicy` in every live PTY-reader task; the server-side prompt-response handler also mutates its in-memory policy snapshot immediately so the next OSC 52 op outside the burst window sees the persisted mode without waiting for the file-watcher round-trip.

The body copy varies by op: reads show "A program in this terminal wants to read the clipboard"; writes show the same lead together with a head-and-tail truncated payload preview built server-side per FR-006. Selection target (`clipboard` vs `primary selection`) is mentioned inline so the user can tell `c` from `p` requests apart.

The host clipboard bridge backing the dialog reuses the same `arboard::Clipboard` handle already wired into `App` for user-driven copy / paste (research decision 3 / [[crates/scribe-client/src/main.rs#App#bridge_read]] and `#App#bridge_write`). The bridge is policy-agnostic — the read / write modes live server-side — and silently fails on `arboard` error per UX-002, collapsing onto a `BridgeError` value the server maps to an empty OSC 52 reply.

Wave 5 wires the selection-target branch and the FR-019 opt-in focus gate. When the incoming `ClipboardSelection` is `Primary` on Linux, the bridge routes through arboard's `GetExtLinux::primary_clipboard` / `SetExtLinux::primary_clipboard` extension traits so X11 reads and writes hit the primary selection rather than the system clipboard; on Wayland the same extension call falls back through arboard's internal mapping, and on macOS / Windows the `#[cfg(target_os = "linux")]`-gated arm is removed so primary collapses onto the regular `get_text` / `set_text` system-clipboard call (spec Assumptions). `bridge_write` additionally consults `self.config.terminal.clipboard_policy.focus_gate_writes` — when the toggle is on and `self.focus.window_focused` is false, the bridge returns `Ok(())` without touching the host clipboard so a background PTY-side program cannot hijack the clipboard while another application holds focus. The toggle defaults off and lives on the client because window-focus state has no synchronous server-side view (research decision 6). The flag is read straight off `App::config`, which the existing config-file watcher refreshes on every save via `handle_config_changed`, so the next OSC 52 write after a settings change already sees the new value without a dedicated IPC variant.

### Copy Hyperlink Address

A right-click on an OSC 8 cell adds a "Copy hyperlink address" entry to the context menu — distinct from "Copy" which copies the displayed text selection unchanged.

The new action variant is `ContextMenuAction::CopyHyperlinkAddress(String)` and the dispatch writes the verbatim URI to the system clipboard via the same path `ContextMenuAction::Copy` uses for text selections. The regular "Copy" path on a selection spanning a hyperlink is unchanged — selecting text inside a hyperlink and copying still yields the displayed text, never the URI.

[[crates/scribe-client/src/context_menu.rs#ContextMenuRequest]] gains an `osc8_uri: Option<String>` field that the menu builder uses to decide whether to append the new item and whether the "Open URL" item emits `OpenOsc8Url(uri)` (OSC 8 origin, routes through the allowlist gate) or `OpenUrl(uri)` (heuristic origin, preserves the pre-009 direct-open behaviour).

### Replay-Scrollback Limitation

Hyperlinks reconstructed from `SessionReplay` (zero-downtime hot reattach and cold-restart restore) do **not** carry OSC 8 URIs. *Live* hyperlinks emitted by the PTY after the reattach completes work without regression.

[[crates/scribe-common/src/screen_replay.rs#SessionReplay]]'s `snapshot_to_ansi` emits cell characters and SGR style only, with no OSC 8 open/close around hyperlinked runs. Live (post-reattach) hyperlinks ride the normal PTY-output byte path and reach the client-side VTE which populates cells as usual.

Extending `snapshot_to_ansi` to re-emit OSC 8 open/close around hyperlinked runs is the documented follow-up improvement path; it would require a `SessionReplay` byte-format / version bump and is out of scope for the 009-osc8-hyperlinks spec.

## Clipboard Cleanup

When copying from a supported AI coding session, [[crates/scribe-client/src/clipboard_cleanup.rs#prepare_copy_text]] applies dedent, blockquote normalization, decorative-prefix stripping, then unwrap.

Copy actions decide whether cleanup is active through [[crates/scribe-client/src/main.rs#copy_cleanup_active]], which requires both an AI provider — via [[crates/scribe-client/src/main.rs#ai_provider_for_pane]] (tracker-detected AI state or an AI launch binding on the pane) — and that the pane is **not** on the alternate screen. The provider check keeps cleanup enabled for newly opened Claude Code and Codex tabs before their first hook event arrives; the alternate-screen exclusion disables it for fullscreen TUIs (e.g. Claude Code's fullscreen renderer), whose grid is arbitrary content rather than AI-chat markdown, so a `Shift`-selected copy is taken raw instead of being mangled by the cleanup transforms.

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

### GPUI Config Port

The GPUI rebuild reproduces the config watcher and runtime-reload semantics against the frozen `scribe-common` config surface, keeping TOML format, flavor config dirs, inline `[theme]`, and removed-key tolerance identical to the winit client.

[[crates/scribe-client-gpui/src/config.rs#start_config_watcher]] watches the active flavor's [[crates/scribe-common/src/app.rs#current_config_dir]] and invokes a caller-supplied closure (instead of a winit `EventLoopProxy`) on each relevant modify/create event; relevance is decided by [[crates/scribe-client-gpui/src/config.rs#is_relevant_config_event_path]], a byte-for-byte port of the legacy filter (`config.toml`, `themes/`, and the macOS FSEvents directory rescan). [[crates/scribe-client-gpui/src/config.rs#ClientConfig]] bundles the parsed [[crates/scribe-common/src/config.rs#ScribeConfig]], the resolved [[crates/scribe-common/src/theme.rs#Theme]] and its derived [[crates/scribe-common/src/theme.rs#ChromeColors]] (via [[crates/scribe-common/src/config.rs#resolve_theme]]), and the parsed [[crates/scribe-client-gpui/src/keybindings.rs#Bindings]].

[[crates/scribe-client-gpui/src/config.rs#ClientConfig#reload]] swaps in a freshly parsed config and returns a [[crates/scribe-client-gpui/src/config.rs#ConfigReloadPlan]] naming which live surfaces changed — theme, font metrics, or opacity — mirroring the legacy `ConfigReloadPlan` heuristics ([[crates/scribe-client/src/main.rs#theme_reload_needed]], `font_params_changed`). Theme, chrome colors, and keybindings are always recomputed so a saved edit reapplies without a restart; the plan lets the caller skip redundant reapply work. Removed appearance keys deserialize inertly because `ScribeConfig` uses serde defaults and models no `deny_unknown_fields`, so the GPUI paint path never observes them.

#### Terminal Window Reload Wiring

The terminal window owns a [[crates/scribe-client-gpui/src/config.rs#ConfigRuntime]] for its whole lifetime, which is what actually turns a saved config edit into a repainted window with no restart.

`notify` delivers its callback on its own thread, which must never touch GPUI entities, so the watcher only bumps a [[crates/scribe-client-gpui/src/config.rs#ConfigChangeSignal]] — an atomic generation counter. The GPUI foreground hops back onto the owning thread through a `cx.spawn` task (`drive_config_reloads` in the client binary) that polls the signal every 120 ms and calls `TerminalView::reload_config`. Polling rather than waking per event collapses the delete/create/modify burst a single editor save produces into one idempotent reload, and [[crates/scribe-client-gpui/src/config.rs#ConfigChangeSignal#take_change]] guarantees no save is missed between polls because the counter is monotonic.

[[crates/scribe-client-gpui/src/config.rs#ConfigRuntime#poll_reload]] returns `None` when nothing is pending and otherwise reloads from disk, handing back the plan. `TerminalView::apply_config_reload` then reapplies each surface the plan flags: a theme change rebuilds the status-bar palette, the grid's terminal colours and the chrome colors, pushes the new [[crates/scribe-client-gpui/src/tab_bar.rs#TabBarColors]] into the titlebar via [[crates/scribe-client-gpui/src/titlebar.rs#TitlebarView#set_colors]], and drops any open palette/context-menu overlay so none keeps painting the old colours; a font change rebuilds the [[crates/scribe-client-gpui/src/terminal_element.rs#GridFont]] the grid paints with and republishes the derived cell metrics to the server as a `Resize`. Keybindings need no flag — they are re-parsed on every reload and `handle_overlay_key` matches each keystroke against [[crates/scribe-client-gpui/src/config.rs#ConfigRuntime#bindings]] through [[crates/scribe-client-gpui/src/keybindings.rs#translate_key_action]], so a saved shortcut edit is live on the next key press.

Every accepted reload ends with [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#config_reloaded]], matching the legacy client's unconditional [[crates/scribe-client/src/main.rs#App#finish_config_reload]] send: the client does not try to guess which server-side surfaces changed, it just tells the server to re-read the same file so clipboard policy, the env store, and the remote/share listeners follow in the same round trip. The plan's `opacity_changed()` signal is delivered to `TerminalView::apply_opacity_change`, which clamps the new value from [[crates/scribe-client-gpui/src/config.rs#ConfigRuntime#opacity]], caches it for the render pass, and pushes a fresh [[crates/scribe-client-gpui/src/tab_bar.rs#TabBarColors]] into the titlebar because that view owns its own palette. Nothing is recreated: the window's surface is already transparent, so the `cx.notify()` ending the reload repaints every alpha-aware background in place. See [[rendering#Rendering#GPUI Ported Rendering Logic#GPUI Window Opacity]].

A watcher that fails to start (missing config dir, exhausted inotify watches) is logged and left absent: the window still runs, it just does not live-reload, exactly as the legacy client degrades. The client binary also installs a `tracing_subscriber` at startup — without it every `tracing` call in the GPUI client, including the hot-reload confirmation, was silently discarded.

## GPUI Remote And Sharing Port

The GPUI rebuild ports the feature 013/014/015 multi-machine surfaces into the `lib` target as rendering-independent state machines: the connect picker, dial handshake, displaced-client banner, LAN approval dialog, and sharing overlays.

Each is the transport-free core of a winit [[client#Remote Control]] module with the `CellInstance` painting dropped in favour of flattened views the GPUI chrome will consume; the frozen IPC protocol is unchanged.

[[crates/scribe-client-gpui/src/remote.rs#RemoteConnect]] is a verbatim port of the winit [[crates/scribe-client/src/remote_connect.rs#RemoteConnect]] picker: the tailnet/LAN merge and dedup (LAN-preferred, incompatible peers dropped, online-first sort), the peer → windows → failed step transitions, and the typed [[crates/scribe-client-gpui/src/remote.rs#RemoteConnectAction]] intents. It consumes a framework-neutral [[crates/scribe-client-gpui/src/remote.rs#PickerKey]] (the GPUI view lowers a `KeyDownEvent` at the call site) and exposes a flattened [[crates/scribe-client-gpui/src/remote.rs#PickerView]] instead of quads; the auto-reconnect [[crates/scribe-client-gpui/src/remote.rs#ReconnectOverlay]] ports alongside it.

[[crates/scribe-client-gpui/src/remote_handshake.rs#perform_remote_handshake]] ports the winit dial preamble ([[crates/scribe-client/src/ipc_client.rs#remote_handshake]]): it sends [[crates/scribe-common/src/protocol.rs#ClientMessage]] `RemoteHandshake` as the first frame over any framed async stream and maps the mandatory `RemoteHandshakeReply` to a [[crates/scribe-client-gpui/src/remote.rs#RemoteConnectOutcome]]. The `SCRIBE_REMOTE_DIAL` / `SCRIBE_LAN_DIAL` / `SCRIBE_REMOTE_WINDOW` dial-env spawn hooks port as [[crates/scribe-client-gpui/src/remote_handshake.rs#parse_dial_target]] and its env wrappers, split from the env read so the grammar stays testable without mutating process env.

The connect picker and the dial-env grammar are still library-only; the LAN half of that surface is live and documented in [[client#Client#GPUI LAN Surface]].

The displaced-client [[crates/scribe-client-gpui/src/lost_control.rs#LostControlState]] (from [[crates/scribe-client/src/lost_control.rs#LostControlState]]) keeps the `Controlled by <device> (<account>)` headline and Enter-only reclaim; [[crates/scribe-client-gpui/src/lan_approval.rs#LanApprovalDialog]] (from [[crates/scribe-client/src/lan_approval.rs#LanApprovalDialog]]) keeps the Decline-default focus, fingerprint-word body, and name-collision hint. The feature 015 sharing surfaces port into [[crates/scribe-client-gpui/src/share.rs#ShareState]] (roster roles, holder derivation), the transient [[crates/scribe-client-gpui/src/share.rs#ControlHint]], and the [[crates/scribe-client-gpui/src/share.rs#ControlRequestPrompt]] — with control passing expressed as a [[crates/scribe-client-gpui/src/share.rs#ControlIntent]] that lowers to the frozen v3 `ControlClaim` / `ControlRequest` / `ControlGrant` messages the winit [[crates/scribe-client/src/share_view.rs#ShareState]] emits.

## Search Overlay

Find-in-scrollback overlay state in [[crates/scribe-client/src/search_overlay.rs#SearchOverlay]], tracking query text, match results, and highlighted match index.

State module plus GPU-rendered overlay. Methods: `open` (clears previous query and results), `close` (resets all state), `push_char`/`pop_char` (edit the query string), `set_results` (replace match list and reset highlight), `next_match`/`prev_match` (cycle through results with wrap-around), `matches` (borrow all results). Match results are `Vec<SearchMatch>` received from the server. All visible matches on the focused pane are highlighted: the current match uses the full accent background with a contrast foreground, while other matches blend the accent into their existing cell background at 40% intensity.

## Tooltip

GPU-rendered tooltip overlay in [[crates/scribe-client/src/tooltip.rs]] that renders a small dark box with light text above or below an anchor rect.

[[crates/scribe-client/src/tooltip.rs#TooltipAnchor]] holds the tooltip text and the anchor `Rect`. [[crates/scribe-client/src/tooltip.rs#TooltipPosition]] selects `Above` or `Below` placement. [[crates/scribe-client/src/tooltip.rs#render_tooltip]] emits `CellInstance` quads into the caller's buffer: a 1 px border quad, a background quad, then per-character glyph quads. The tooltip is horizontally centered on the anchor and clamped to stay within `viewport_width`. A 1-character left/right padding is included on each side of the text.
