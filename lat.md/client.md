# Client

The scribe-client is the GPUI terminal frontend, preserving the established
IPC, binary, and session-continuity contracts after the client cutover.

 opens GPUI windows over the unchanged
local IPC protocol. `--settings` opens the integrated settings window and
`--vulkan-probe` verifies hardware or lavapipe before package relaunches.

The GPUI rebuild keeps `appearance.opacity`:  proves that the pinned GPUI revision opens a transparent Wayland/X11 surface and repaints root alpha live. The decision is recorded in `specs/016-gpui-client-rebuild/spikes/window-opacity-wayland-x11.md`, and  documents how the client paints it.

The follow-on image-protocol decision is recorded in `specs/016-gpui-client-rebuild/spikes/terminal-image-protocols.md`: Sixel uses `icy_sixel`, Kitty control data uses a narrowed WezTerm-derived parser, and both feed one bounded placement renderer.

## GPUI Client Spike

The cutover crate (`crates/scribe-client`) renders a live Scribe pane over the
frozen IPC protocol and builds against pinned gpui/alacritty revisions.

### gpui-component adoption

Scribe retains bespoke chrome and revisits `gpui-component` only for isolated
new surfaces after compatibility and integration validation. The source decision
is `specs/016-gpui-client-rebuild/gpui-component-evaluation.md`.

The spike adopts Zed's display-only terminal model:  owns an alacritty `Term` plus a VTE `Processor` and holds no PTY. Server bytes enter through , which advances the processor, reports whether the frame changed visible state, and rebuilds an immutable `Content` grid snapshot.  paints that snapshot as fixed-width GPUI rows.

A background thread runs : it connects to the live server socket, splits it into read/write halves, and queues `Hello` + `ListSessions`.  attaches the first live session and hands every message to , which normalises `PtyOutput` / `SessionReplay` / `ScreenSnapshot` into raw output bytes (via the  helpers, off the drain) and forwards them as . Each coalesced batch bumps a shared generation counter;  polls it on the GPUI foreground and calls `notify()` so the window repaints.

The dispatcher implements twenty-nine of the protocol's inbound variants, six of which it names and hands to , four more to , and three more to , so the table stays a list of routing decisions. The rest reach , which names them from the exhaustive table in , counts the drop, and logs it at `warn`. Session-scoped arms gate on the attached session inside the arm rather than in a match guard, so a frame for a background pane stays a deliberate no-op instead of being reported as unhandled. See  for the ratchet that keeps the unhandled set shrinking.

### Terminal Chrome Metadata

Only the server knows a pane's CWD, git branch, session context and env health, or a workspace's name, so the client keeps them in  — a pure store the reader writes and the view reads per frame.

 folds `CwdChanged`, `GitBranch`, `SessionContextChanged`, `EnvStatus` and `WorkspaceNamed` onto that store through , which bumps the redraw generation exactly like the AI chrome's own updater; `TitleChanged` and `IconTitleChanged` independently update OSC 0/2 window and OSC 0/1 icon titles through . Metadata is keyed by session rather than by the attached pane, so a background tab keeps its chrome warm and switching tabs repaints without a server round trip;  also adopts the CWD, branch and context the authoritative `SessionList` replays, so a reattach restores the bar instead of waiting for the next shell prompt.

### IPC Bridge

The  module carries bytes both directions over the frozen IPC protocol without adding keystroke latency or frame tearing, mirroring Zed's terminal wakeup coalescing.

Inbound:  drains the  channel with 4 ms / 100-event coalescing.  collapses a drained run into one per-pane byte buffer in first-seen order (), which  feeds through the sync-frame queue below. Because output is normalised to bytes before it enters the channel, coalescing only ever concatenates.

#### Bounded inbound queue

The channel the reader feeds is [[crates/scribe-client/src/ipc_bridge.rs#inbound_channel]], bounded at [[crates/scribe-client/src/ipc_bridge.rs#INBOUND_QUEUE_EVENTS]] events and [[crates/scribe-client/src/ipc_bridge.rs#INBOUND_QUEUE_BYTES]] of buffered output.

Both bounds are load-bearing. The event bound is the one the audit named, but events carry variable-size payloads and coalescing trades event count for bytes, so the byte ceiling is what actually flattens RSS under a `yes` firehose; one frame larger than the whole ceiling is still admitted onto an emptied queue, because refusing the newest frame would stall the pane rather than bound it.

[[crates/scribe-client/src/ipc_bridge.rs#InboundState#admit]] never blocks the reader — a stalled socket read would only push the backlog onto the server's sink — so a queue at either bound coalesces first, through the very same [[crates/scribe-client/src/ipc_bridge.rs#coalesce]] the drain applies to a batch, and drops from the front only for what still does not fit. Reusing the drain's own rule is what guarantees the queue can never invent an order the drain would not have produced: a pane's events keep their order and every byte survives, only the event count falls.

Each dropped event records its pane, and [[crates/scribe-client/src/ipc_bridge.rs#PendingResync#settle]] turns that debt into one `RequestSnapshot` per affected pane. The request waits for the queue to reach empty, so the repaint lands on a calm queue instead of being dropped in turn; a firehose that never lets the queue drain gets the request anyway after [[crates/scribe-client/src/ipc_bridge.rs#RESYNC_MAX_DELAY]]. Nothing new goes on the wire — the client detects its own overflow and repairs only the panes it lost bytes for, which is what makes the policy per-participant in a shared session.

#### Batch byte cap

[[crates/scribe-client/src/ipc_bridge.rs#collect_batch]] bounds each drain batch by payload as well as by count and time, at [[crates/scribe-client/src/ipc_bridge.rs#MAX_BATCH_BYTES]].

The count and time bounds do not bound the work a batch costs. A batch is parsed and applied in one uninterruptible pass, and 100 server frames is anywhere from a few kilobytes of shell output to 6.4 MiB of `cat` — the queue's own byte ceiling is four times the cap and a decompressed replay event is larger still — so bytes, not events, are what set how long a single drain stalls.

The cap never rejects anything. An event whose own payload exceeds it is handed to the drain alone: splitting a pane's bytes would tear its VTE stream, and dropping them would lose output the queue already accepted. [[crates/scribe-client/src/ipc_bridge.rs#InboundReceiver#recv_within]] decides the fit under the same lock that pops, so an oversize event that raced in behind a momentarily empty queue is deferred to a batch of its own rather than absorbed into one that is already near the cap.

#### Bounded outbound queue

The writer channel is [[crates/scribe-client/src/ipc_bridge.rs#outbound_channel]], bounded at [[crates/scribe-client/src/ipc_bridge.rs#OUTBOUND_QUEUE_FRAMES]] frames under a deliberately different policy from the inbound one: all-or-nothing, never a per-frame drop.

The asymmetry is the whole point. Inbound frames are output, so losing some and resyncing the pane is recoverable. Outbound frames are user *input* — a keystroke, a resize, a session the user asked for — and evicting one queued `KeyInput` does not lose a byte, it hands the server the truncated remainder of a command line that then executes. So [[crates/scribe-client/src/ipc_bridge.rs#OutboundState#admit]] refuses the *incoming* frame at the cap, keeps everything already queued, and reports the refusal to its caller.

A refusal does two things. It raises a tear request, which [[crates/scribe-client/src/ipc_bridge.rs#OutboundTear#wait]] delivers to [[crates/scribe-client/src/main.rs#run_writer]] even mid-write, through the [[crates/scribe-client/src/ipc_bridge.rs#write_or_tear]] race — the cap is only reachable behind a writer wedged on a socket the server has stopped reading, so the refusal has to interrupt that write rather than wait it out — and the redial then drains the same queue onto a fresh stream. And it becomes visible: [[crates/scribe-client/src/main.rs#TerminalView#report_refused_input]] puts the refused keystroke in the window status bar's live region, while [[crates/scribe-client/src/main.rs#supervise_connection]] keeps naming the refusal on every backoff for as long as the queue stays at its cap. Input that never reached the server is never silently swallowed.

Tearing costs no input either: the frame the writer already took is returned by [[crates/scribe-client/src/ipc_bridge.rs#OutboundReceiver#requeue]] and goes out first on the next stream, so the real high-water mark is the cap plus that one frame. The tear request itself is scoped to the connection it was refused on and cleared when a new one opens, because a refusal taken while no stream was alive has nothing to tear and carrying it forward would tear each redial before it wrote a frame. The backlog is never pruned across a redial — a `CreateSession` queued before the stream died is still a session the user asked for, and dropping it would be the client acting on its own behalf rather than the user's.

### Sync Frame Queueing

Between the coalescing drain and `feed_output`, a per-session [[crates/scribe-client/src/sync_frames.rs#SyncFrameQueue]] preserves `CSI ? 2026` commit boundaries so a redraw never tears a frame across IPC splits, ported from the winit client's drain path.

[[crates/scribe-client/src/sync_frames.rs#SyncFrameQueue#queue_output_frames]] runs the ported streaming splitter, which keeps raw markers intact and emits one committed frame per commit even when the terminating `l` is split across messages. [[crates/scribe-client/src/main.rs#apply_pane_op]] queues each coalesced batch and then presents exactly one committed burst through [[crates/scribe-client/src/sync_frames.rs#present_next_burst]]; once the backlog passes [[crates/scribe-client/src/sync_frames.rs#OUTPUT_FRAME_CATCH_UP_THRESHOLD]] that single call drains through to the latest frame so stale frames never pile up.

Outbound:  replaces Zed's `write_to_pty` path, enqueuing `ClientMessage::KeyInput` / `Resize` plus the session-lifecycle messages the tab shortcuts drive (`CreateSession` / `AttachSessions` / `Subscribe` / `RequestSnapshot` / `CloseSession`), the window-lifecycle frames the close dialog, the window-list poll and the focus observer raise (`CloseWindow` / `QuitAll` / `ListWindows` / `FocusChanged`), and the workspace frames the split shell raises (`CreateWorkspace` / `CloseWorkspace` / `MoveSession` / `ReportWorkspaceTree`, see ) onto the ordered IPC-writer channel drained by . The sink is independent of the inbound drain, so a keystroke is never queued behind an output firehose; because the channel is a single FIFO, a `Resize` enqueued before a `KeyInput` reaches the server first. That channel is bounded as well — see [[client#GPUI Client Spike#IPC Bridge#Bounded outbound queue]] for the all-or-nothing policy that bound runs under. The GPUI view feeds the sink from  through , which is the live entry point of the ported  rather than a table of its own.

#### Paced presentation

Pacing is what makes a burst a burst: the drain hands the grid one committed frame per redraw instead of emptying the pane's queue every batch.

Emptying it was the de-facto behaviour, and it collapsed a whole run of committed frames into one repaint — every frame was parsed, the redraw generation was bumped once, and a `CSI ? 2026` animation was shown as its last frame only. [[crates/scribe-client/src/main.rs#run_frame_pacer]] presents what pacing holds back, one burst per pane per [[crates/scribe-client/src/main.rs#REDRAW_INTERVAL]] — the same clock [[crates/scribe-client/src/main.rs#drive_redraws]] repaints on, so "one burst per redraw" is one number rather than two that can drift apart. It parks while every pane is caught up and the drain wakes it whenever a batch leaves a burst behind, so a pane that queued more than one frame never waits on the next batch to show the rest.

The pacer needs no bound of its own because the catch-up threshold already is one: a pane past it is drained through in a single pass, so a firehose is presented at the batch's own rate and only a caught-up pane is actually paced. That is also why the pacing fix is safe to make: the bursts it skips are never load-bearing for correctness, because a client that falls behind is repaired by the bounded inbound queue's `RequestSnapshot` resync rather than by replaying every intermediate frame.

A skipped burst also builds no screen. [[crates/scribe-client/src/sync_frames.rs#drain_until_frame]] advances every frame of a pass through the target's `advance_output` and publishes the snapshot once, through its `publish_content`, for the state the pass leaves behind. The frames it drains through still reach the parser — they are what the grid is made of — but the viewport projection they would each have rebuilt under the pane lock is work no redraw could ever have shown. [[crates/scribe-client/src/sync_frames.rs#present_rebuild]] publishes on the same rule: the queue it clears ahead of a rebuild is about to be replaced, so the whole boundary costs one snapshot.

#### Rebuild burst boundary

A whole-pane rebuild is not output and is not paced: a decoded [[protocol#Server Messages#Terminal Output#SessionReplay]] — or the same self-resetting ANSI built from a `ScreenSnapshot` — replaces the pane rather than advancing it.

It rides the inbound FIFO as its own [[crates/scribe-client/src/ipc_bridge.rs#InboundEvent]] variant, because its position in the byte stream is exactly what makes it correct: everything the server sent ahead of it is already folded into the state it carries. [[crates/scribe-client/src/ipc_bridge.rs#coalesce]] refuses to fold that variant into the output runs on either side of it, so no committed frame can span a rebuild.

[[crates/scribe-client/src/sync_frames.rs#present_rebuild]] then applies it as a burst boundary. [[crates/scribe-client/src/sync_frames.rs#SyncFrameQueue#seal_frame_boundary]] commits whatever the splitter still holds — an unterminated `CSI ? 2026 h` opened by earlier output would otherwise swallow the rebuild into the frame it is buffering — everything already queued lands first in arrival order, and the rebuild itself reaches `feed_output` whole instead of being re-split into frames the server never emitted.

#### Synchronized-update expiry

A companion [[crates/scribe-client/src/main.rs#run_sync_expiry]] task waits on the nearest raw-frame ([[crates/scribe-client/src/sync_frames.rs#RAW_SYNC_TIMEOUT]]) or parser deadline and commits a 150 ms-expired update whose closing `l` never arrived.

[[crates/scribe-client/src/terminal.rs#PaneGrid#flush_expired_sync]] runs the parser side first and then [[crates/scribe-client/src/sync_frames.rs#SyncFrameQueue#flush_raw_timeout]], and presents the flushed frame under the same pacing every other burst is, so an expiry cannot jump the queue ahead of the frames already waiting on it. The drain task wakes the expiry task whenever a fresh sync update arms a deadline, and each presented burst bumps the shared redraw generation.

### Session Lifecycle

 ports the legacy client's reattach, reconnect, and scrollback-trim semantics onto the display-only terminal. Decode and snapshot conversion stay pure and run ahead of the drain, so a corrupt replay degrades to pane status without crashing.

 zstd-decompresses a `SessionReplay` (rejecting zero-dimension or corrupt streams as a ). The reader never runs that inflate itself: [[crates/scribe-client/src/session_lifecycle.rs#decode_replay_off_thread]] hands it to Tokio's blocking pool and awaits the result, so message order is preserved while the IPC thread's current-thread runtime stays free to drive the writer and the drain — a multi-megabyte reattach inflated inline froze keystrokes and every pane's repaint for as long as zstd ran. A blocking task that panics or is cancelled surfaces as the same `ReplayDecodeError` a corrupt payload does, so the snapshot-request fallback covers both. [[crates/scribe-common/src/screen_replay.rs#snapshot_to_ansi]] starts every new replay with RIS; [[crates/scribe-client/src/session_lifecycle.rs#ensure_replay_reset]] conditionally supplies it only for an old server's pre-RIS payload.  tracks live sessions, rebuilds the reconnect topology from `SessionList`, applies `SessionCreated` / `SessionExited`, and adopts the window id from a takeover `Welcome`.  shifts stored absolute  rows after a `TrimScrollback`; the marks themselves live in the shared  store, because the drain writes them and the key path reads them.

Every attach carries a `Subscribe` for the pane it just attached, sent through  behind the `AttachSessions` from both attach paths —  on the reader's `SessionList` / `SessionCreated` route and  on a tab switch. Order matters because  rejects a subscription for a session this connection is not attached to; the shared ordered writer channel and the server's sequential per-connection dispatch make that impossible by construction. The same authorisation covers `Resize` and `RequestSnapshot`: a tab switch that adopts a background tab's session into the focused pane attaches *before* publishing pane sizes, since a publish enqueued ahead of the attach put a denied `RequestSnapshot` ("denied for unattached session") into the window status bar. Subscribing is what makes the server run its CWD-fallback check for the newly visible pane, so a reattached tab gets its directory (and the workspace name derived from it) without waiting for the next shell prompt.

A fresh create sends that `Subscribe` and nothing else. A `CreateSession` is itself an attach — the server installs this connection's sink while it starts the session, and spawns the PTY at the geometry the request named — so [[crates/scribe-client/src/main.rs#adopt_created_session|adopt_created_session]] takes the answer up instead of re-attaching it, and only a `SessionCreated` with no create outstanding goes through [[crates/scribe-client/src/main.rs#attach_session|attach_session]]. The two are told apart by [[crates/scribe-client/src/ipc_bridge.rs#IpcSink#claim_pending_create|claim_pending_create]], a counter the sink raises on every accepted `CreateSession` and the reader claims on every genuine tab insert — the answer carries no echo of the request, so the FIFO order the single ordered writer channel guarantees is what matches them, the same rule the launch bindings follow. Re-attaching instead bought a redundant full-state replay that could overwrite startup bytes already delivered, plus a re-point of a sink that was already this connection's.

The grid an attach announces is the focused pane's live one. The reader thread can measure nothing, so it used to carry the nominal 120x36 startup box for the whole life of the window: every attach drove the PTY onto that box and the next redraw drove it back, a shrink and a regrow the foreground process sees as two `SIGWINCH`es. [[crates/scribe-client/src/main.rs#TerminalView#adopt_focused_pane_size|adopt_focused_pane_size]] mirrors the measured focused-pane size into `Shared` on every republish and [[crates/scribe-client/src/main.rs#reader_attach_size|reader_attach_size]] reads it back, so an attach — which always means "show this session in the focused pane" — names the grid that pane actually has.

 is the display-only client's resync: it owns no PTY and never replays locally, so when its pane may have drifted from the server's `Term` the only way back to a correct pane is to ask. Three live paths send it.  follows its post-font-reload `Resize` with one, because that resize raises `SIGWINCH` on the server's PTY and the client cannot derive the resulting grid itself.  sends one when a reattach replay fails to decode, turning a permanently stale tab into a repaint. [[crates/scribe-client/src/ipc_bridge.rs#PendingResync#settle]] sends one per pane the bounded inbound queue had to drop, which is what lets that queue bound memory without silently losing screen state. The reply lands on , which feeds the common self-resetting ANSI (visible grid *and* scrollback) through the drain, so everything on screen afterwards came out of that snapshot.

#### Replay reattach applies

A decoded `SessionReplay` reattach frame reproduces the session grid when written to a fresh terminal.

#### Replay decode failure

A `SessionReplay` with a corrupt zstd payload yields a `ReplayDecodeError` and leaves the terminal untouched and still usable, so the pane surfaces an error without crashing the reader.

#### Snapshot replay replaces prior state

The common replay encoder clears prior pane state before painting, so a tooling `ScreenSnapshot` replaces rather than appends onto the terminal without client-only reset wrapping.

#### Zero-dimension replay rejected

A replay reporting zero rows or columns is rejected up front rather than fed through the VTE pipeline.

#### Trim shifts marks

A `TrimScrollback` shifts surviving absolute marks down by the dropped-row count and drops marks anchored inside the trimmed region.

#### Trim clears input below delta

When the pending input-start row falls inside the trimmed region it is cleared, and a zero-row trim is a no-op.

#### Reconnect topology rebuild

`SessionList` rebuilds the session-to-workspace topology grouped by workspace in first-seen order, pruning workspaces without live sessions.

The registry's flat grouping is bookkeeping only; the window's actual regions and splits are no longer flattened on reconnect. The first list of a (re)connect carries the server-persisted `workspace_tree` (shipped back unchanged from `ReportWorkspaceTree`), which the reader parks — before it rebuilds the tab strip, so no frame sees the sessions without the tree — and [[crates/scribe-client/src/main.rs#TerminalView#adopt_server_topology]] adopts on the GPUI thread. A live layout wins only while this view is the parked tree's author: the view keeps a bounded history of the trees it recently reported, and a parked tree found in it (a mid-session redial, possibly a few queued reports behind) is ignored. A parked tree the view never reported means another client owned and reshaped the window since — a stale claim, the case where a leftover pre-update client once re-imposed its collapsed layout and closed every rebuilt workspace of the reconnecting window — and the view adopts the server's tree over its own layout. A fresh client start ([[crates/scribe-client/src/pane_shell.rs#PaneShell#is_unused]]) adopts unconditionally, and a cold-restart replay in flight still defers to the replay. On the frame an adoption rebuilt the regions, [[crates/scribe-client/src/main.rs#TerminalView#reconcile_panes]] skips its retain/close pass: the reader parks the tree before it rebuilds the tab strip, so that frame's strip can predate the adopted layout, and judging fresh regions against a stale strip would close on the server the very workspaces it just restored. [[crates/scribe-client/src/pane_shell.rs#PaneShell#adopt_server_tree]] prunes sessions missing from the list (collapsing half-dead splits, the same rule the cold path applies to panes without sessions), rebuilds regions, splits, and pane→session placement, and the view then attaches every visible pane, focuses the pane holding the tab strip's active session, and reports the pruned tree back so the server's copy matches what the window shows. Sessions in the list but absent from the tree stay ordinary tabs, exactly as before.

#### Server upgrade reconnect

The local IPC supervisor redials an upgraded server without restarting the GPUI process.

 retains one window's IPC queues across a local server handoff, redials with bounded backoff, and sends `Hello` plus `ListSessions` before queued UI traffic. A successful connection resets the backoff to its 100 ms initial delay, so each later outage gets a fresh fast retry instead of inheriting an earlier outage's 2 s ceiling. The reply follows , rebuilding the registry, workspace metadata, and tab strip, then  replays every visible pane's connection-local attachment at its retained grid dimensions. LAN and tailnet dials keep their one-shot refusal behavior so a rejected peer never becomes an endless local-looking retry.

#### Created and exited transitions

`SessionCreated` registers a pane in arrival order without duplicating re-announcements, and `SessionExited` retires it and reports whether it was tracked.

#### Takeover adoption

A takeover `Hello`'s `Welcome` records the adopted window id on the registry.

### Prompt Marks And Jumps

The running client ingests the server's OSC 133 `PromptMark` stream into per-session command records and navigates them with `prompt_jump_up`, `prompt_jump_down`, and `jump_to_failure`.

Before this all five surfaces were dead: the marks were dropped by the reader's catch-all and the three chords were swallowed by the key path.

 holds the records — the port of the legacy client's per-`Pane` `command_records` — as absolute scrollback rows plus a . The record type itself is , owned by the scrollbar module because the store and the overlay's ticks are two ends of one record rather than two types that have to be converted between. `A` opens a record at the prompt row and prunes anchors the grid no longer holds, `D` resolves the most-recent still-open record from its exit code (`None` stays `Unknown`, so an unreported exit is never a failure), and `B` / `C` only move the pending input start.

The marks are anchored by the drain, not the reader, because an absolute row only means anything once the output that moved the cursor has been applied. `PromptMark`, `ScrollBottom` and `TrimScrollback` therefore travel the pane's own ordered channel as  variants and come back out of  as  entries interleaved with that pane's output runs;  then applies each in turn against the same grid. The server helps: it sends the `PtyOutput` chunk carrying the OSC before the `PromptMark` it parsed out of it, so the mark lands on the row the shell actually drew.

A `TrimScrollback` is applied there too, and it is two operations, not one.  first replicates the trim on the display grid — the server suppressed the AI session's ED 3, so the sequence never reached the client and the grid would otherwise keep rows the server has already forgotten — and reports how many of *its own* oldest rows that removed.  then shifts every surviving anchor by exactly that count through , retiring the marks whose rows are gone. The drop is measured on the client's ring rather than taken from the server's reported history, because the server names only the size it kept.

All three jumps share , which resolves the focused session, compares the marks against , dissolves the split-scroll pin a landing would otherwise outlive, and moves the viewport with . `jump_to_failure` with no failed command is a deliberate no-op rather than a jump to the nearest thing (FR-011). A `ScrollBottom` replays the viewport reset a real ED 3 would have caused, which the server stripped from the stream.

Verified against the running app in .

#### Mark state machine resolves exits

`A` opens a record, `D` resolves the most-recent open one from its exit code (`Some(0)` success, `Some(≠0)` failure, `None` unchanged), and a `D` arriving once every record is resolved rewrites nothing.

#### Jump picks the neighbouring mark

`jump_target` takes the newest mark strictly above the viewport top going up and the oldest strictly below it going down, so repeated presses walk the list one command at a time and an unknown session has nothing to jump to.

#### Failure jump picks the newest failure

`failure_target` reverse-scans for the most recent `Failure`, skipping successes and unresolved records; a trim re-anchors the survivor and an exited session forgets it.

#### Evicted anchors are pruned

A new `PromptStart` drops records anchored past `history + screen_lines`, so marks whose rows the grid no longer holds cannot be jumped to.

### Tab Strip And Key Dispatch

The shell's live key path runs the configured bindings before the PTY encoder, so tab shortcuts open, switch, and close real server sessions instead of leaking their chord as terminal bytes.

 lowers each `KeyDownEvent` through  and , and is consulted after the overlays own the keyboard but before , which runs the ported byte encoder for everything no binding claimed — an unbound named key such as PageUp/PageDown therefore reaches the PTY as `CSI 5~` / `CSI 6~` instead of being dropped. The resolved  is handed to , the shell's single live dispatch point, which the command palette also targets so a row and its chord can never drift apart. A matched  reaches ; the four scrollback actions and the three zoom actions run () and the three mark-relative jumps run (), and the clipboard pair copies the pane selection and pastes the host clipboard back through the spec-011 gate () — so no bound shortcut is swallowed any more. That dispatcher still matches exhaustively rather than with a `_` arm, so a new `LayoutAction` fails to compile instead of joining a dropped set unnoticed.  ratchets that set.

The tab actions drive the IPC sink: `new_tab` and the four AI-tab shortcuts send  into the focused workspace (the AI variants through , which the server turns into a plain-tab shell that execs the CLI over itself), `next_tab` / `prev_tab` / `select_tab_N` move the selection and re-, and `close_tab` sends . Every tab shortcut names the region the user is in — the shell's, not the strip's — and [[crates/scribe-client/src/tab_session.rs#TabSessions#select]] counts only that region's tabs, so a digit cannot reach a neighbouring region's. The same switch/close paths are what the titlebar's tab buttons emit, resolved from a row position to a tab through [[crates/scribe-client/src/main.rs#TerminalView#titlebar_slot]], so pointer activation and keyboard shortcuts stay in lockstep instead of maintaining a second tab-only code path.

`new_window` opens a second top-level window in the same process through , rather than re-spawning the binary the way the winit client's `spawn_client_process` had to — GPUI is multi-window, as the settings window already shows.  builds each window's own `Shared` state and its own IPC connection, and `main` calls it for the startup window too, so the two paths cannot drift. Independent state is what makes it a window rather than a mirror: the `Hello` on that second connection carries no window id, so the server registers a *new* window and gives it its own sessions, tab strip, and status line.

 is the ordered strip both sides share behind a mutex.  rebuilds it from `SessionList`, appends focused tabs on `SessionCreated`, and drops them on `SessionExited`;  mirrors it into the titlebar on redraw.

A tab preserves four independent sources: shell basename, OSC 0/2 window title, OSC 0/1 icon title, and AI task label. `TitleChanged` and `IconTitleChanged` update only their own native source; provider notices update only AI metadata.  resolves icon title, window title, AI label, then shell. Blank OSC 1 reveals the latest window title; blank OSC 0 clears both native sources because that sequence emits both reset events. `SessionList` and handoff preserve every source unchanged, so reconnect order cannot change ownership. Verified against the running app in . Because the server re-announces `SessionCreated` as its acknowledgement of every `AttachSessions`, only a genuine insert by  triggers an attach — treating the echo as a new tab would attach in an unbounded loop.  then points `active_session` at the focused tab, and the reader reads that shared value on every message so output gating follows a switch made on the GPUI thread. A genuine insert that answers this window's own `CreateSession` is adopted rather than attached; see [[client#Client#GPUI Client Spike#Session Lifecycle]].

### GPUI Terminal Viewport Wiring

The running client reaches the ported viewport modules — scrollback navigation, vi / copy mode, split-scroll, smart selection, and font zoom — through , the one type that owns a live `Term`.

A split window holds one of those per pane, so every surface below acts on the focused pane, reached through .

Before this,  read the *screen* rows and ignored the grid's display offset, so the pure modules were untestable in the product: no scroll could change a pixel. It now reads the viewport through the offset, which is what makes `scroll_up` / `scroll_down` / `scroll_top` / `scroll_bottom` real.  runs each of the four  variants through , and  ports the winit rule that typing into a scrolled pane jumps back to the live bottom.

Split-scroll is decided across the shell/terminal boundary because each side owns half the inputs. The shell contributes the  pair — the `scroll_pin` config key and whether the focused session runs an enabled AI provider — on every frame through ; the terminal adds the live half (scrolled up, normal screen) in  and sizes the pin with  and . Rather than the winit client's dual render, the split is expressed in the snapshot itself: the trailing `Content::pin_rows` rows are read from the live screen anchored on the shell cursor while the rows above stay at the scrolled offset, which is the same cursor-anchored translation  describes in pixels, done in row space where a cell grid can express it exactly.  then draws only the seam — the divider and the docked jump chip from  — and a click is resolved against the same geometry by . Typing keeps the pin up; Enter collapses it, exactly as the winit client behaves.

Vi mode is a shell-owned keyboard mode, so it enters through the  table (`ctrl+shift+space`) rather than a `KeybindingsConfig` field it has none of — which also means it yields to a user rebind that lands on the same keys. While it is active,  sits between the shell chords and the configured bindings: a bound chord still runs (paging the scrollback keeps working), a bare motion key drives  through , and every other bare key is swallowed so `j` can never leak into the shell. The cursor is published on the snapshot in viewport coordinates and outlined by  as a hollow box, so the character under it stays readable.

Smart selection reaches the product through the right-click menu.  lowers the pointer position onto a grid cell using the bounds the grid canvas recorded for the frame,  resolves that cell against the display offset before matching, and  lowers each resolved action onto the  that runs it — one-for-one with the winit `smart_selection_context_action`, so a rule authored against the legacy client behaves identically.

Zoom folds  into the grid font rather than into the config:  is the single place both a zoom step and a config font reload rebuild  from `appearance.font_size` plus the live zoom level, so a saved font-size edit rebases the zoom instead of discarding it, and both paths republish cell metrics to the server through .

A zoom step therefore re-lays the grid, not just the glyphs: the pane geometry is divided out of the window's measured grid area (), so a smaller font buys real columns and rows and the `Resize` that follows carries them to the server, which is what makes the freed pixels part of the terminal instead of dead space.

The level is not runtime-only state: it is persisted with the window's geometry record and restored with it — see [[client#Client#Window State]] for what is stored and why it is the level rather than a font size. A step needs no write path of its own, because the rebuild's `cx.notify()` brings the new level through the per-frame [[crates/scribe-client/src/main.rs#TerminalView#capture_geometry]], where it arms the same debounce a move or a resize does.

Covered headlessly by  and against the running app by  and .

### GPUI Mouse Wheel And Reporting

The grid band carries the pointer surfaces the running client had none of: a scroll-wheel listener, and the xterm mouse reporter every button gesture now asks before claiming the click for itself.

The crate previously contained no wheel handling of any kind, and `mouse_reporting.rs` shipped with a green golden-byte suite and zero live callers — the two losses  now closes.  is the wheel's single entry point: it converts the event to rows, asks  who claims them, and then either encodes a button 64 / 65 report, sends the mode-1007 cursor keys, or moves the viewport through the same  the paging chords use.

The button paths are a chain of first-refusal. ,  and  each return whether the application claimed the event; only when they decline does the click mean selection, primary-selection paste, or the context menu. Middle and right releases exist purely for the reporter — they carry no client-side gesture, but a mode-1002 drag needs its button-up.

The held button and the last reported cell live on `PointerState` beside the selection classifier rather than inside it, mirroring the winit split between `mouse_selecting` and `mouse_report_button`. A physical button-up always drops the tracked button, even when the release itself cannot be forwarded (tracking turned off mid-drag, or Shift held now), so the next pointer move cannot report a phantom drag.

Reports go out through , deliberately *not* the keystroke path: a mouse report must neither snap a scrolled viewport back to the live bottom nor dismiss an AI attention state, and a live share viewer sends nothing at all. Each forwarded payload is logged with its control bytes escaped, which is what lets a scripted run assert the exact sequence.

Covered headlessly by  and against the running app by .

### GPUI Command-Mark Scrollbar

Each pane paints a non-reserving overlay scrollbar on its right edge, ticked with the command boundaries the OSC 133 stream reported: theme green for a success, red for a failure, neutral for an unresolved exit.

`scrollbar.rs` was ported byte-for-byte from the winit client and then referenced by nothing at all — the running client painted no scrollbar. What was missing was every connection to live state, and each one lands somewhere different.

State lives on the view, not on the element.  keys a  per *session* rather than per pane, because the fade belongs to the scrollback being scrolled: moving a session between panes carries its thumb with it, and a pane with no session has nothing to scroll.  drops a closed tab's state alongside its grid.

Geometry is resolved during paint, because only the grid canvas knows the pane's pixel rect and the whole point of the overlay is that it hugs the right edge of the cells instead of reserving a gutter the layout would subtract.  therefore supplies the *data* —  read live off the grid by , the pane's marks cloned out of the shared prompt-mark store, and the theme palette — and  calls  against the real bounds and lowers the result onto rounded GPUI quads. Whole-pane rebuilds trim the ANSI replay's synthetic history back to the server's authoritative `scrollback_rows`, so a fresh pane stays at zero history and wheel input cannot expose a blank scrollbar. The track spans the full canvas with no tab-bar inset, because this client's tab strip lives in the window titlebar.

The palette comes from the theme, never from config.  takes the derived `chrome.scrollbar` slot (whose 40 % alpha doubles as the resting fade ceiling) and the ANSI green/red for the ticks; `appearance.scrollbar_width` and `appearance.scrollbar_color` stay deliberately unread, because both are declared removed keys for the GPUI client and the width is fixed at .

The fade is wall-clock, so it is driven from the idle tick rather than from output:  runs on the same 16 ms wake  already uses for the share hint, and repaints while any scrollbar is animating *or* while this tick changed an opacity — the tick that finally reaches zero reports "not visible" while still owing the frame that clears it.  reveals the overlay from every path that moves a viewport, including a paging chord that hit the end of the scrollback, because that is exactly when the user wants to see where they are.

The pointer surfaces reuse the module's own hit-testing.  offers a left press to the scrollbar before the mouse reporter, mirroring the winit client's chrome-first ordering: a click on the thumb was never meant for the application below it. A press on the thumb starts a drag, a press elsewhere in the 3x hit zone jumps the viewport to that point on the track, and both land on  so they share the split-scroll bookkeeping the keyboard path does. Hover is tracked even while an application owns the pointer, because the hover widen is what makes the 6 px thumb grabbable at all.

Covered headlessly by  and against the running app by .

### Share And Control Handoff

The feature-015 sharing surface is live in the GPUI shell: the reader mirrors the server's roster and control notices, the key path answers them, and the render pass draws the presence panel, the transient hint, and the modal grant/deny prompt.

 is the shared aggregate — the mirrored , the pending , the transient , and this connection's own participant id from `Welcome`.  folds `ShareRoster`, `ControlRequested`, `ControlDenied`, and `ShareEnded` into it through , which repaints on every change; a roster that drains back to a single participant tears the surfaces down, exactly like the winit  port it replaces.

Input follows the winit dispatch order. A pending request is a full-window modal claimed at the top of , so it is answered before any binding, overlay, or PTY byte; everything else reaches  from , after the configured bindings, so a viewer keeps its shortcuts while its terminal keystrokes are suppressed. The decision table returns a : a viewer's first key raises the take-control hint and is dropped, Enter while that hint is up claims control, and the prompt's Enter/Esc grants or denies. Each emitted  leaves through , the single place the frozen v3 `ControlClaim` / `ControlGrant` frames are built.

Rendering has two halves.  draws the roster panel, the hint strip, and the dimmed modal on the window's overlay layer, and  feeds the status bar's existing share badge (). Because a hint expires on wall-clock time rather than on traffic,  clears it on the same 16 ms idle-wake tick it already runs, so a notice cannot outlive its window on a quiet pane.

The surface is verified against the running app, not headlessly: see .

### Tab Order And The Reported Tree

A window's tab order, which tab is active, and where the window sits on the desktop are all *reported* state: the client owns them, and anything it leaves out of its report is state the user loses on the next restart.

[[crates/scribe-common/src/protocol.rs#WorkspaceTreeNode]]'s `Leaf` has carried the whole shape from the start — `session_ids` ("Ordered session IDs for tabs in this workspace"), `pane_trees` parallel to it, and `active_tab_index` — and the server persists the per-window tree and re-applies the order through `apply_tab_order_from_tree`. The GPUI shell filled in only the session its visible pane was showing and hardcoded `active_tab_index: 0`, so the server was told a one-element order and never learned which tab was active. [[crates/scribe-client/src/pane_shell.rs#region_tab_payload]] now builds the leaf from the window's strip: every tab of the region in strip order, the live split at the active tab's index (a lone pane needs no tree — [[crates/scribe-client/src/pane_shell.rs#wire_tab_pane_tree]] rebuilds it from the tab's own id), and the index of the tab in the region's focused pane. A pane whose session the strip has not caught up with is appended rather than dropped.

The strip stops being derived from the server. [[crates/scribe-client/src/tab_session.rs#TabSessions#reconcile]] folds a `SessionList` in without reordering: the list says which sessions exist, never what order the user put them in. Replacing the strip wholesale undid every drag-reorder on the next list, and because the server grouped its answer by a `HashMap` of workspaces it also reshuffled a multi-region window on every reconnect. Order is restored from the tree instead — [[crates/scribe-client/src/pane_shell.rs#wire_tree_tab_order]] flattens every region's tabs left to right and the adoption orders the strip by it, then activates the tab the shell's focused pane is showing.

Because the order and the active tab are only durable once reported, a tab switch and a drag-reorder now report too, alongside the existing split/close/adopt triggers. The report is deduplicated against the last one, so a switch that changes nothing costs one tree build and no traffic.

[[crates/scribe-server/src/workspace_manager.rs#WorkspaceManager#sessions_for_window]] answers in the same order for the no-tree fallback: the window's own reported tree leads, and anything it does not name follows in workspace-stored order with the workspaces in a stable id order.

### Restored Window Placement

A restored window is moved back onto its monitor after it is mapped, because the bounds `open_window` is given are only a hint.

GPUI's X11 backend passes the requested origin to `create_window` but sets no `USPosition`/`PPosition` size hint, and under ICCCM 4.1.2.3 a window without one is placed entirely at the window manager's discretion — Mutter takes it and puts every window on the active monitor, which is why restored windows all came back on one screen however good their geometry record was. This is a known, unfixed upstream gap rather than a stale pin or a missed API: `PlatformWindow` exposes no position setter at all, and the gap is the root cause behind open Zed issues 8345, 41246, 12521, 47231, and 40666. No upstream PR proposes setting `size_hints.position`, so Scribe works around it entirely on its own side instead of forking gpui: GPUI exposes `Window::resize` and no way to move a window, so [[crates/scribe-client/src/monitor.rs#apply_saved_position]] issues the move over the same X11 connection this module already keeps for `RandR`.

The move goes out as an EWMH `_NET_MOVERESIZE_WINDOW` client message with `StaticGravity`, sent to the root window with `SubstructureRedirect|SubstructureNotify` so the window manager rather than the X server answers it. A plain `ConfigureRequest` carries the window's default `NorthWestGravity`, under which the coordinates name the *frame's* outer corner while the record holds the content origin — so a restore landed one border and one titlebar down and right of the record, and repeated it on every restart until the window walked off the screen. EWMH 4.2 exists for exactly this: `StaticGravity` makes the coordinates the content origin whatever decoration the window manager drew, so the round trip is exact and there is no residual to measure. Window managers that do not advertise `_NET_MOVERESIZE_WINDOW` in `_NET_SUPPORTED` fall back to the `ConfigureRequest`.

The message restates the window's current size even though only the position is changing, and [[crates/scribe-client/src/monitor.rs#apply_saved_position]] reads it off the same `get_geometry` reply it already needs for the root window. EWMH 4.2 marks each of x, y, width and height present through its own bit in `data[0]`, and a move that sets only the position bits leaves the window manager to reconstruct the size — under `StaticGravity` that reconstruction runs through `WM_NORMAL_HINTS`, where GPUI publishes a maximum and no base, minimum, or increment. Mutter resolved the gap by collapsing the window to 1×1: mapped, listed in the taskbar, and invisible, on every start of every window that had a saved position, which is as complete an outage as the client has. Naming the size costs one word each and removes the reconstruction entirely.

A maximized or fullscreen window is placed the same way, because the window manager owns its size but not the monitor it fills — and the monitor follows the origin. [[crates/scribe-client/src/monitor.rs#apply_saved_position]] lifts the state around the move: `_NET_WM_STATE_REMOVE` of the atoms that own the placement, the `_NET_MOVERESIZE_WINDOW`, then `_NET_WM_STATE_ADD` of the same atoms. EWMH 5.7 lets one message carry both maximize properties, so the window never passes through a half-maximized state; the atoms are resolved all-or-nothing out of `_NET_SUPPORTED`, and a window manager that advertises neither pair leaves the plain move. The origin aimed at is [[crates/scribe-client/src/window_state.rs#WindowGeometry#restore_origin]] — the pre-maximize rect's, which is both on the saved monitor and where the window sits mid-sequence, falling back to the record's own origin for a legacy record that never captured one.

The state is asserted in its own right by [[crates/scribe-client/src/monitor.rs#assert_window_state]], because GPUI's X11 operation is a *toggle*. `WindowBounds::Maximized` reaches `zoom()` before GPUI maps the window, but its `set_wm_hints` call sends `_NET_WM_STATE_TOGGLE` on GPUI's X connection; Scribe's idempotent `_NET_WM_STATE_ADD` uses another connection after the map. X11 orders requests per connection, not across connections, so either can land last and a late GPUI toggle can undo the correct add.

[[crates/scribe-client/src/main.rs#x11_creation_bounds]] removes that second owner: on X11, maximized and fullscreen records create a window at their saved restore rect, so GPUI emits no state toggle, and Scribe sends the only transition as an ADD. The earliest public hook is the root-view builder after GPUI maps the platform window; Scribe asserts there before a root view exists or an application frame can paint, then [[crates/scribe-client/src/main.rs#TerminalView#apply_saved_window_state]] repeats the idempotent ADD after the placement move. True pre-map EWMH state needs an upstream GPUI creation API and remains unavailable. Wayland keeps `WindowBounds::Maximized`/`Fullscreen`, whose native request is state-setting rather than toggling through a second X connection.

What the window manager cannot be talked out of is a strut, a snap, an off-screen clamp, or a decision to use the active monitor. [[crates/scribe-client/src/main.rs#TerminalView#verify_restored_position]] therefore checks the landing rather than correcting it: once the request has had `RESTORE_DEBOUNCE` to be answered — it is asynchronous, so a reading taken beside it reports a position the window has not reached — it compares [[crates/scribe-client/src/monitor.rs#window_monitor_name]] against the monitor the record names. Same monitor is logged as a success; a different one is logged as an explicit give-up, because re-asserting against a window manager that is enforcing something real is how a placement loop starts. Nothing is checked when the record names no monitor, or when the platform cannot resolve the live one.

#### A restore never saves over the record it aims at

[[crates/scribe-client/src/main.rs#RestorePlacement]] gates geometry persistence on the restore having converged, so a misplaced restore stays a one-restart annoyance instead of corrupting the record permanently.

[[crates/scribe-client/src/main.rs#TerminalView#capture_geometry]] runs on every frame and every bounds change, and it used to arm the debounced flush unconditionally. Wherever the window manager actually dropped a restored window — right monitor or wrong — that placement was written back over the saved record half a second later, so one bad start destroyed the only position the next start had to aim at, and every placement defect became self-reinforcing.

The gate is a three-state advance: `Restoring` while [[crates/scribe-client/src/main.rs#RestoreRuntime]]'s `pending_position` or `position_target` still hold work, `Verifying` for one more `RESTORE_DEBOUNCE` after the landing has been checked (a window manager still nudging the window with a strut or a snap must not have that reading adopted as the user's layout), then `Settled`. A window opened without a saved position starts `Settled`, so nothing about a fresh window's first-frame capture changes.

While unsettled the reading is still tracked as the live geometry — the change detection needs a baseline — but it is also recorded as `saved_geometry`, which is what "already on disk" means to both [[crates/scribe-client/src/main.rs#TerminalView#flush_geometry_if_due]] and the unconditional quit-time [[crates/scribe-client/src/main.rs#TerminalView#flush_geometry_now]]. That keeps both flushes off the record without a second suppression flag, and the user's next move or resize differs from the baseline and arms the flush exactly as before.

#### A restore never saves over the record it aims at

[[crates/scribe-client/src/main.rs#RestorePlacement]] gates geometry persistence on the restore having converged, so a misplaced restore stays a one-restart annoyance instead of corrupting the record permanently.

[[crates/scribe-client/src/main.rs#TerminalView#capture_geometry]] runs on every frame and every bounds change, and it used to arm the debounced flush unconditionally. Wherever the window manager actually dropped a restored window — right monitor or wrong — that placement was written back over the saved record half a second later, so one bad start destroyed the only position the next start had to aim at, and every placement defect became self-reinforcing.

The gate is a three-state advance: `Restoring` while [[crates/scribe-client/src/main.rs#RestoreRuntime]]'s `pending_position` or `position_target` still hold work, `Correcting` for one `RESTORE_DEBOUNCE` after the single correction goes out (that move is another asynchronous `ConfigureRequest`, so bounds read beside it still report the placement it is undoing), then `Settled`. A window opened without a saved position starts `Settled`, so nothing about a fresh window's first-frame capture changes.

While unsettled the reading is still tracked as the live geometry — the change detection needs a baseline — but it is also recorded as `saved_geometry`, which is what "already on disk" means to both [[crates/scribe-client/src/main.rs#TerminalView#flush_geometry_if_due]] and the unconditional quit-time [[crates/scribe-client/src/main.rs#TerminalView#flush_geometry_now]]. That keeps both flushes off the record without a second suppression flag, and the user's next move or resize differs from the baseline and arms the flush exactly as before.

### Window Identity And Warm Restart

Every connection names the window it wants. The server keeps a window's sessions when its client goes away, so which window a `Hello` claims is what decides whether a restart resumes the user's windows or scatters them.

An unnamed `Hello` is a *request to be given* one of the windows whose sessions outlived their client: [[crates/scribe-server/src/ipc_server.rs#resolve_window_assignment]] hands back one of them and reports the rest as `Welcome`'s `other_windows`. That makes it exactly the wrong frame for a deliberate new window — a `None` claim from [[crates/scribe-client/src/main.rs#TerminalView#open_new_window]] opened a window the user already had, while the real one stayed unopenable — so a new window mints its own `WindowId` instead. A freshly minted id is by construction not a window the server knows, so it is assigned verbatim and the window is genuinely empty.

The claim is carried on [[crates/scribe-client/src/main.rs#WindowBackend]], one per window backend, and resolved per handshake by [[crates/scribe-client/src/main.rs#IpcThread#window_claim]]: the id a previous `Welcome` already assigned this connection wins over the seed, so a redial claims its own window back. Without that, a hot upgrade — which drops every stream at once and lets all windows redial together — could hand two windows each other's session sets, because the server's "any unconnected window" pick walks a `HashSet`.

`other_windows` is what makes a restart bring *all* the windows back rather than one. [[crates/scribe-client/src/main.rs#on_welcome]] parks the ids and [[crates/scribe-client/src/main.rs#TerminalView#poll_sibling_windows]] reopens one window per id on the foreground, each claiming its own id and creating no first shell, so each adopts its own sessions instead of racing the others for them. Only the process bootstrap's first handshake may fan out: the flag is consumed there, so a redial cannot reopen a window whose own process is mid-reconnect, and a reopened window cannot fan out again.

The two fan-outs are mutually exclusive by construction. `other_windows` restores windows the server still holds sessions for; the `--restore-child` processes restore snapshots after the server itself died. Whichever one fires zeroes the other's count, so no window is ever opened twice.

### Hot Restart Reattach

When only the client restarts and the server survives, the whole window is rebuilt from the server's `SessionList` — and the AI half of the chrome is rebuilt there too, not left blank until the next hook event.

[[crates/scribe-client/src/main.rs#on_session_list|on_session_list]] already seeded [[crates/scribe-client/src/chrome_metadata.rs#ChromeMetadata|ChromeMetadata]] (cwd, git branch, context, shell name) and the tab strip from the list; nothing read its AI fields, so `SessionInfo.ai_state` and `ai_provider_hint` had zero consumers and a reattached client came up with no prompt bar and no indicator. [[crates/scribe-client/src/main.rs#AiChrome#seed_from_session_list|AiChrome::seed_from_session_list]] closes that: prompt history goes through [[crates/scribe-client/src/main.rs#AiChrome#restore_prompts|restore_prompts]] (so a `PromptReceived` that beat the list still wins), the retained state goes to the tracker, and the provider hint is applied last as the fallback for a session whose visible state is gone but whose provider-aware behaviour must survive the reattach. The history itself is retained server-side; see [[server#Server#Sessions#Retained Prompt History]].

Only the *first* list of a connection seeds. A later list is a topology refresh, and re-applying `ai_state` there would resurrect an attention state the user has already dismissed with a keystroke through [[crates/scribe-client/src/ai_indicator.rs#AiStateTracker#clear_attention_states|clear_attention_states]] — a purely local clear the server never hears about.

The conversation id comes from retained `ai_state` even when the pane has no prompt rows, so [[crates/scribe-client/src/main.rs#TerminalView#sync_launch_bindings|sync_launch_bindings]] can rebuild a structured AI resume binding from the shared tracker, conversation map, and retained CWD. The running provider's first live `AiStateChanged` re-announces an id the chrome already knows and does not read as a switch. `dismissed` has no wire counterpart on purpose: dismissal is a local gesture against a pane, so a reattaching client starts with the bar shown, exactly as a fresh window would.

The same reconciliation runs for existing bindings on every restore poll. A live AI state edge promotes a shell fallback or updates its provider and conversation id; explicit `AiStateCleared` demotes it to a shell; a CWD edge updates its fallback directory. Each change dirties the snapshot without replacing its launch id. A local `SessionInfo` also returns the launch id when its environment envelope belongs to this window, so a new client process retains that identity. Older servers and the first handoff from a version that did not transfer envelope coordinates return none and mint once; remote/shared clients never receive the selector.

Provider absence in `SessionList` is not treated as a clear because it is also the valid pre-hook state of a fresh AI process. A clear emitted only while this client is disconnected therefore waits for the next explicit AI edge instead of risking a false shell demotion.

This path is independent of the cold-restart snapshot below and never touches it. A surviving server means the snapshot is not replayed at all, and burning a claim to read prompt text out of it would cost a later cold restart the layout it needs.

The same list drives the reattach itself, and a Codex pane is the exception there. [[crates/scribe-client/src/main.rs#reattach_visible_sessions|reattach_visible_sessions]] replays the window's attached set onto the replacement connection with each pane's live display grid — except for a session the list calls `CodexCode`, which [[crates/scribe-client/src/main.rs#reattach_panes|reattach_panes]] gives a zero-sized `TerminalSize` through [[crates/scribe-client/src/restore_replay.rs#attach_dimensions_for_session|attach_dimensions_for_session]]. The server's `attach_flow::send_attach_replay` runs its pre-replay `resize_term` + `TIOCSWINSZ` only for a size with a grid, so a zero leaves the PTY alone while the replay restores the pane's history; Codex renders through Ink, which repaints on `SIGWINCH` and would paint over that history the moment it arrived.

Announcing nothing is only two thirds of the port. The retired client also skipped the follow-up `Resize` (the GPUI client sends one per reattached pane, and a resize landing right behind the attach would size the PTY anyway) and cleared the pane's last-sent grid so the real size arrived later through the ordinary publish. The GPUI equivalent of that clear is `Shared::deferred_grids`: the reader parks the Codex session ids there because `pane_sizes` lives on the view, and [[crates/scribe-client/src/main.rs#TerminalView#publish_pane_sizes|publish_pane_sizes]] drops each parked entry so its next pass re-sends the real grid instead of skipping a pane whose cached size still matches.

Only the reconnect path takes the exception. [[crates/scribe-client/src/main.rs#attach_session|attach_session]] and [[crates/scribe-client/src/main.rs#TerminalView#stream_session|stream_session]] are the tab-switch and pane-adopt attaches, where the pane is already correctly sized and a zero would leave it unsized until the next publish — worse than announcing the grid. Ink's actual repaint behaviour is unverified: there is no Codex binary in the harness, so [[test#Visual E2E Tests#Codex reattach announces no grid]] pins the announced wire shape and nothing beyond it.

#### Session list seeds the AI chrome

A `SessionInfo` carrying retained prompt history, an AI state, and a provider hint paints its prompt bar and restores the tracker's provider on seeding, and the seeded bar survives the resumed provider re-announcing its own conversation id.

#### Retained AI bindings stay structured

A retained provider, conversation id, and CWD build an AI resume binding, so the next cold restart sends structured targeted resume intent instead of launching the pane as a shell.

#### Live AI metadata updates restore

A live AI state edge promotes or updates the persisted binding without replacing its launch id, a partial edge that omits the conversation id preserves the last targeted resume id, and only an explicit provider-clear edge demotes it to a shell.

#### Codex reattach defers its grid

A reconnect gives a `CodexCode` session zero attach dimensions and no follow-up resize, while every other retained pane announces its display grid and takes the resize.

That leaves the Ink-rendered PTY unsized until the replay has restored its history, and the real grid arrives afterwards through the ordinary publish.

### Cold Restart Restore

The GPUI client ports cold-restart recovery: after a server crash the bootstrap window rebuilds its windows, workspaces, tabs, and panes from persisted snapshots and re-creates each saved session at the correct geometry.

 persists one TOML snapshot per window under `$XDG_STATE_HOME/scribe/restore/windows/<window_id>.toml` plus a shared `index.toml`, all hardened to `0700`/`0600` because launch bindings can carry prompt text and provider conversation IDs. A bootstrap lock serialises multi-process index mutations and stale locks (>30 s) are reclaimed.  atomically claims the first replayable  entry, skips non-replayable and unreadable entries, and reports how many windows remain. Those are fanned out as `--restore-child` processes via  only once the server's first `SessionList` proves it lost its sessions, because a server that kept them restores its own windows through `Welcome`'s `other_windows` and fanning out both would open every window twice; each child claims exactly one more entry and, gated by , never fans out again. A claim is non-destructive: the snapshot file stays on disk as the window's last good layout, its id parked in the index's `claimed` list so it can never be claimed twice, and it is retired only when the claiming window durably writes a fresh snapshot of its own, or the user explicitly closes the window. A claim the window never consumed — the server kept its sessions, so nothing was replayed — is released rather than retired, because a claim the server refused names another live window's layout and deleting it is how a layout gets lost.

 rebuilds a  — the , a  map (standing in for the legacy `Pane` struct the display-only spike lacks), and the ordered  queue that re-creates each session. Before the launches dispatch, the replay sizes every pane grid from the re-applied window geometry (not the pre-restore hint) so maximized windows do not create PTYs at the startup size and stay undersized, through the same [[crates/scribe-client/src/restore_replay.rs#grid_for_rect|grid_for_rect]] the live republish uses. It also owns the Codex 0x0 exception the reconnect path applies — see [[client#Client#GPUI Client Spike#Hot Restart Reattach|Hot Restart Reattach]] — so reattaching a Codex session sends a zero-sized `TerminalSize` and the server does not pre-size its Ink-rendered PTY.  serialises the live layout and pane metadata back into a `WindowRestoreState` for the next save.

#### Live wiring in the GPUI shell

The two persisted files are driven from the running window: `ColdStart` claims and geometry before the window opens, and `RestoreRuntime` writes, replays, and clears them for its lifetime.

 runs in `main` *before* the backend connects, because the claim must happen once per process rather than once per reconnect, and because the claimed snapshot's window id is both the only key the geometry record can be found under and the window this process claims in its `Hello`. A true cold restart reaches a fresh server that has not named this window yet, and by the time `Welcome` does the window is already on screen. The loaded record goes through  and , then  turns it into the `WindowBounds` handed to `open_window` — so unlike the winit port there is no post-creation move/resize/maximize sequence to race the compositor, and no flash at the default size.  distinguishes "nothing persisted" from the default geometry, because the default is a size *hint*: opening at it would override the grid-derived startup size on every launch that never saved anything.

A launch that finds nothing claimable has no such id: it opens at the default size on the active monitor and sends an unnamed `Hello`, so the window it adopts is only named by `Welcome` — read at [[crates/scribe-client/src/main.rs#TerminalView#poll_restore|poll_restore]], well after the window is on screen. [[crates/scribe-client/src/main.rs#TerminalView#adopt_assigned_geometry|adopt_assigned_geometry]] reads that window's record once at that point and hands it to the same [[crates/scribe-client/src/main.rs#RestoreRuntime#adopt_geometry_record|adopt_geometry_record]] the seeded path uses, so the adopted window's position is re-asserted and its `RestorePlacement` is held open — which is what keeps the opening default from being flushed over the record. Without it the first flush destroyed the bounds of whichever window the server happened to hand over.

Geometry is read back off the live window by  through  — GPUI bounds are already logical pixels, so no scale-factor division is needed. It runs from an `observe_window_bounds` subscription (the GPUI equivalent of winit's `Moved`/`Resized`) and again on every paint, so a window that is opened and never touched still persists the size it came up at. Both files are written on a 500 ms debounce (`RESTORE_DEBOUNCE`), since a drag-resize emits a bounds change per frame and a split re-reports the tree several times while sessions arrive.

The snapshot is marked stale from , the single funnel every layout change already passes through.  serialises the live shell into the ported format: the shell keeps panes in  entities while `snapshot_window_restore` is written against , so a scratch layout is filled from the live trees — one region becomes one tab whose pane tree is the region's whole split, the same shape reported to the server — and panes still awaiting a session are pruned exactly as they are on the wire. A window with nothing replayable in it is *removed* from the store rather than saved blank, so the next cold start cannot claim it and replay an empty window forever.

Live prompt-bar history is threaded into that walk rather than read off the pane. The GPUI client keeps it in the IPC-written `AiChrome.prompts` map keyed by `SessionId`, so [[crates/scribe-client/src/pane_shell.rs#PaneShell#restore_snapshot|restore_snapshot]] takes the map as an argument and [[crates/scribe-client/src/restore_replay.rs#PaneRestore|PaneRestore]] carries the same [[crates/scribe-client/src/prompt_bar.rs#PromptBarData|PromptBarData]] the bar renders from, instead of mirroring three of its fields. The caller clones the map under the mutex rather than holding the lock across the walk, because the reader thread writes it on every `PromptReceived`. The five prompt fields themselves are declared once, on [[crates/scribe-common/src/protocol.rs#SessionPromptState|SessionPromptState]]: `PromptBarData` wraps it beside the local `dismissed` flag and [[crates/scribe-client/src/restore_state.rs#LaunchRecord|LaunchRecord]] embeds it `#[serde(flatten)]`, keeping the on-disk field names older snapshots were written with while the snapshot and replay conversions collapse to a field copy. Both instants are already Unix-epoch seconds there, so a frozen timer stays frozen at its original finish instant; [[crates/scribe-common/src/protocol.rs#from_epoch_secs|from_epoch_secs]] is the one place they are lifted back to a `SystemTime` for the elapsed-timer comparison. Without this the snapshot persisted `prompt_count: 0` for every pane, which is not a degraded bar but no bar at all — the render path gates the whole strip on a non-zero count.

Reading that history back is a pane-to-session hop, because the snapshot files prompts under a pane while the bar reads them out of `AiChrome` under a session, and the session does not exist until the server answers the replayed launch. [[crates/scribe-client/src/restore_replay.rs#queue_from_launch_record|queue_from_launch_record]] fills `PaneRestore.prompts` and `last_conversation_id`; the replay parks both on `RestoreRuntime.restored_prompts` keyed by pane; and [[crates/scribe-client/src/main.rs#TerminalView#seed_restored_prompts|seed_restored_prompts]] drains that map from [[crates/scribe-client/src/main.rs#TerminalView#adopt_session|adopt_session]], the one funnel every adoption passes through, into [[crates/scribe-client/src/main.rs#AiChrome#restore_prompts|AiChrome::restore_prompts]]. A `PromptReceived` that beat the adoption is newer than the snapshot and is left alone. Seeding the conversation id alongside is what keeps the resumed provider's first `AiStateChanged` from reading as a switch in [[crates/scribe-client/src/main.rs#AiChrome#note_conversation|note_conversation]], which retires a session's prompt history and its context percent whenever a state edge names a *different* conversation than the last one seen.

What a pane relaunches as is decided by its . AI shortcuts construct `LaunchKind::Ai` directly from structured provider/resume intent; commands without that intent always persist as `CustomCommand`, even when their argv invokes a supported provider. Fresh AI creates and cold-restart replays send `ai_launch: Some(...)` with `command: None`; custom launches send `command: Some(...)` with `ai_launch: None`. The binding is queued *before* the request goes out; the answering `SessionCreated` carries no id, so it is matched by FIFO order on the single writer channel. A session this window did not create falls back to a plain shell binding. The binding's `launch_id` leaves as `env_envelope_id`, and replay forwards the persisted id unchanged, so restore-delta staging keeps the same envelope identity across fresh and replay paths.

 gates the replay on three conditions in order: the server must have *answered* the startup `ListSessions` (latched by `session_list_seen` — an unanswered list is not an empty one), that answer must be empty (a server that kept these sessions is restored by the ordinary reattach plus the workspace-tree adoption above, and replaying on top would double every pane — the skipped snapshot's file survives on disk until the fresh layout is persisted over it), and the window must have painted once, because restored panes are sized from the measured grid area via  — the same helper the per-frame republish uses, so a restored PTY is created at exactly the size its pane will report one frame later instead of the fallback 80x24.  then replaces the whole shell with the rebuilt window and queues every restored pane as pending, and each launch leaves through , which fills a [[crates/scribe-client/src/ipc_bridge.rs#SessionLaunch|SessionLaunch]] with the pane's persisted `launch_id` so the server can hand the shell back its saved environment envelope. A new tab or pane goes out through the same single [[crates/scribe-client/src/ipc_bridge.rs#IpcSink#create_session|create_session]]; the two cases differ only in whether an envelope already exists under the id being sent.

 drains the answers. The ordinary reconcile pass adopts one arriving session per tick, which is enough for a split but not for a replay: five `CreateSession` frames come back faster than five ticks, so four panes would stay empty while their sessions lived on as tabs with nowhere to render. It is gated on a replay being in flight, because outside one the pairing would be wrong — a split's pending pane must get the session that split asked for, not whichever older tab happens to have no pane.

Clearing follows intent, and only a window close is destruction. A quit ends the client while every session keeps running on the server, so it flushes the geometry *and* the snapshot: they are the window's layout and the id the next launch reclaims it by. A `CloseWindow` drops both, since the window and its sessions are gone. The whole path is asserted against the running app by .

`LaunchKind::Ai` persists the shared [[crates/scribe-common/src/protocol.rs#AiResumeMode]] directly. Moving that type out of the client does not migrate snapshots: serialized mode names remain exactly `New` and `Resume`.

#### Snapshot round-trips through disk

A saved  loads back with its window, focused workspace, workspace name, and launch records intact and reports as replayable.

#### AI resume variant names stay stable

Both AI resume modes serialize to their historical `New`/`Resume` TOML names and deserialize back to the same variant.

#### Claim skips non-replayable and remaining count

`claim_first_window` drops a blank (non-replayable) entry, claims the first replayable window while keeping its file on disk, and reports the remaining count for the `--restore-child` fan-out.

The claimed id moves to the index's `claimed` list, so a repeat claim cannot double-replay it. A later `upsert_index` for the same window supersedes the claim, making the fresh snapshot claimable again.

#### Stale lock reclaimed

`RestoreStore::lock_is_stale` treats a bootstrap lock older than the 30 s window as stale (reclaimable) and a freshly stamped one as live.

#### Replay rebuilds layout and queue

`prepare_replay` reconstructs the window layout, focused workspace, and accent colour, and produces one `ReplayLaunch` per saved pane carrying the workspace, launch id, cwd, and command, with pane metadata keyed by the same pane id.

#### Snapshot survives rebuild round trip

Serialising a rebuilt window with `snapshot_window_restore` reproduces the original window, focused workspace, tabs, focused launch id, and launch records, and the result is replayable.

#### Prompt state survives the snapshot round trip

A saved `LaunchRecord` carrying prompt text, count, and both epoch timestamps replays into `PaneRestore.prompts` as `SystemTime` values and re-serialises back to the same epoch seconds, so a restored prompt bar keeps its rows and its elapsed timer.

#### An adopted window keeps its saved geometry

Verifies a window opened without a geometry record still owes the window the server assigns it a read, and that reading the record arms the same restore a seeded window starts with instead of leaving the opening default to be persisted.

A window opened at its own record named that window in `Hello`, so it has nothing left to adopt and starts out placing itself; one opened without a record starts settled — it should persist the bounds it came up at — and only stops being settled once the assigned window's record is adopted.

#### A maximized record is asserted, not toggled

Verifies a maximized or fullscreen record queues its state for an explicit `_NET_WM_STATE_ADD`, from both halves of the restore: the window opened at the record and the window that only learned which record it has afterwards.

The second is the one with nothing to fall back on — it was never opened with `WindowBounds::Maximized` at all. A windowed record queues nothing, since it owns no atoms. A minimized record queues the state it *unminimizes into* rather than minimization itself, so a window that was maximized before it was minimized comes back as both.

#### Replayed prompts reach the live AI chrome

A session seeded with a replayed pane's prompt history keeps its rows when the resumed provider re-announces the conversation id it was resumed with, and loses them only once a state edge names a different conversation.

#### Grid sized before launch

[[crates/scribe-client/src/restore_replay.rs#grid_for_rect|grid_for_rect]] computes a pane's terminal grid from the restored viewport and cell size, so a replayed pane launches at the grid it will paint at rather than at the startup fallback.

#### Codex reattach sends zero size

`attach_dimensions_for_session` returns the sized grid for a normal session but a zero-sized `TerminalSize` for a Codex session, encoding the exception that leaves Codex PTY sizing to its own SIGWINCH.

Its one caller is the reconnect path described under [[client#Client#GPUI Client Spike#Hot Restart Reattach|Hot Restart Reattach]], which pairs the zero with a deferred republish so the pane's real grid still reaches the server — after the replay, as an ordinary resize.

#### Restore child never fans out

`is_restore_child` detects the `--restore-child` flag so a fanned-out child passes count 0 to `spawn_restore_children` and never spawns further windows.

#### Structured AI replay

An AI resume launch record replays as structured provider, resume-mode, and conversation-id intent with no client-built command argv.

#### Replayed workspace regains its project root

A region rebuilt from a snapshot starts with no project root — [[crates/scribe-client/src/restore_state.rs#WorkspaceSnapshot|WorkspaceSnapshot]] deliberately omits the one [[crates/scribe-client/src/workspace_layout.rs#WorkspaceSlot|WorkspaceSlot]] field the server owns — and takes it back from the server's answer.

The root is derived, not authored: [[crates/scribe-server/src/workspace_manager.rs#WorkspaceManager#on_cwd_changed|on_cwd_changed]] sets it whenever a session's CWD falls under a configured `workspaces.roots` entry and clears it on the way out. Persisting it would freeze a value the server recomputes on the next CWD report, so a `workspaces.roots` edit between the snapshot and the restart would replay a root the server immediately contradicts.

Recovery needs no OSC 7. The `Subscribe` [[crates/scribe-client/src/main.rs#adopt_created_session|adopt_created_session]] sends for every replayed pane reaches `handle_subscribe`, which runs `WorkspaceManager::check_cwd_fallback` against the new child's `/proc/<pid>/cwd` and answers with `WorkspaceNamed { project_root }`; the reader parks it as a [[crates/scribe-client/src/pane_shell.rs#WorkspaceInfo|WorkspaceInfo]] and [[crates/scribe-client/src/pane_shell.rs#PaneShell#apply_workspace_info|apply_workspace_info]] writes it onto the replayed slot. Only [[crates/scribe-client/src/main.rs#TerminalView#create_ai_tab|create_ai_tab]] reads the root, and it falls back to the focused pane's CWD, so the window is never wrong in the interval — just less specific.

### Pane Grid Sizing

The client owns no PTY, so the `cols`x`rows` it publishes *is* the terminal's width, and it must equal the grid the pane paints down to the column.

An application right-pads to what the ioctl reports, so one column of overspend wraps every full-width line.

[[crates/scribe-client/src/restore_replay.rs#grid_for_rect|grid_for_rect]] is the one grid formula in the client. The live republish ([[crates/scribe-client/src/main.rs#TerminalView#grid_size_for|grid_size_for]]) and the cold-restart replay both resolve a pane through it, so the size a PTY is created at and the size it is later resized to cannot come from different arithmetic. It takes the largest whole cell count that *fits* — `cols * cell_width <= width` — rather than dividing and flooring, because at fractional cell metrics the `f32` quotient can round up over the boundary: at `cell_width = 6.3` and `width = 428.4` the quotient floors to 68 while only 67 columns fit.

What it is handed is the rect the pane *paints* into, produced by [[crates/scribe-client/src/main.rs#TerminalView#painted_pane_rect|painted_pane_rect]]. Three bands live inside a placement rect and none of them belong to the PTY: a lower region's tab bar (already taken off by [[crates/scribe-client/src/pane_shell.rs#PaneShell#content_rect|content_rect]] before placements are returned), the one-pixel border a split window draws around every pane (GPUI hands taffy the border as a box inset, so the child's content box is two pixels smaller on each axis), and the pane-internal prompt strip. Reserving neither the border nor the strip is how the client came to report more columns than it renders.

Nothing derived from the [[crates/scribe-client/src/main.rs#TerminalView#pane_viewport|pane_viewport]] fallback may be published. Before the measuring canvas has reported a rect, the only viewport on offer is the nominal `COLUMNS`x`ROWS` box, which differs from the real band by whatever the chrome takes; publishing it announced a grid the window never had and corrected it a frame later, and every application that had already padded a line to the first width wrapped it against the second. [[crates/scribe-client/src/main.rs#TerminalView#publish_pane_sizes|publish_pane_sizes]] reads [[crates/scribe-client/src/main.rs#TerminalView#measured_pane_viewport|measured_pane_viewport]] and says nothing until the probe has measured one. The fallback stays for the paint path alone, which has to draw something on the first frame.

#### Published grid fits the painted pane

Across fractional cell metrics and a sweep of pane widths, the computed grid always satisfies `cols * cell_width <= width` and is the largest one that does.

So a line of exactly `cols` characters occupies one painted row, with no dead column left at the right edge.

#### Divide and floor overruns the pane

At `cell_width = 6.3` and a pane 428.4 pixels wide the `f32` quotient floors to 68 columns, one more than fits; the exact-fit search returns 67. This is the regression that made right-padded status output wrap.

### GPUI Layout Entities

The GPUI rebuild ports the two-level split tree into a `lib` target alongside the scaffold binary, so the pure trees and their entity wrappers are library API covered by `#[gpui::test]` headless suites.

The pane split tree is  (binary `Leaf`/`Split` nodes, ratios clamped 0.1-0.9, spatial-overlap directional focus with edge wrap). The workspace split tree is , whose `WorkspaceSlot` carries tabs, active tab index, accent color, name, and project root. `TabState` drops the winit client's `selection` field until terminal selection is ported in Phase B.

#### Pane Tree Model

 is a `gpui::Entity` wrapping a `LayoutTree`. Every structural mutation emits `PaneTreeEvent::Changed` and calls `notify()`.

 and  both auto-equalize the surviving ratios so sibling panes stay evenly sized; `close` refuses to remove the sole root leaf.  clamps to 0.1-0.9, and `find_pane_in_direction` resolves directional focus (with edge wrap) without mutating or emitting.

#### Workspace Tree Model

 is a `gpui::Entity` wrapping a `WindowLayout` plus the running `PaneId -> SessionId` map. Every mutation re-serializes the tree and emits `WorkspaceTreeEvent::Report`.

The event payload is the exact `WorkspaceTreeNode` the client forwards to the server as  `ReportWorkspaceTree`.

Reported mutations include workspace split, tab add/remove, , workspace ratio change (clamped 0.1-0.9), and in-place slot edits via `update_slot`. On reconnect the restore path pushes tabs (each auto-activating the last) and then replays `active_tab_index` through `set_active_tab` to restore the originally focused tab, matching the winit client's post-pass.

 renames a region and  drops one, both reporting like every other mutation. The shell needs the first because it builds its region before the server has named a workspace, and the second when a region's last pane closes.

### GPUI Pane And Workspace Shell

 is what makes the two split trees above reachable from the running binary: it owns the window's one  plus one  per workspace region.

Two layers, matching the chrome Scribe has always had. A workspace region is a slice of the window with its own accent colour; panes are the splits inside one region. `workspace_split_*` moves the outer divider and `split_*` the inner one, and each focus family moves within its own layer. Every pane hosts at most one session, and the focused pane of the focused region is the pane keystrokes, the status bar, and the tab strip follow —  is the single place that alignment is re-established after any focus move.

The shell holds no pixel state. Callers pass the grid area's measured pixel rect (), so the layout stays a pure function of the trees plus the window the grid actually got. The rect is reported by a measuring canvas in  because the grid is the one flex-grown band and its height is whatever the chrome bands leave over — the prompt strip comes and goes with the pane's prompts, so no arithmetic on the window size can predict it. Before the first frame reports anything, the fallback is the nominal grid at the current  metrics, which is exactly the size the window opens at.  resolves that viewport into one  per leaf, which  lowers onto absolutely positioned children whose offsets and sizes are *fractions* of the grid area — so the ratios survive any window size without the view measuring device pixels. The focus ring is drawn only once a window actually has more than one pane, and takes the owning region's accent, so an unsplit window paints exactly as before.

 resolves pane and workspace-region divider geometry over that same viewport, and  paints the resulting 1px quads above the grids. A workspace divider's full 4px-tolerance hit band carries GPUI's native left-right or up-down resize cursor. The grid pointer path claims either divider before terminal selection, maps motion through the matching ratio helper, then writes the ratio through  and republishes both adjacent pane sizes.

 closes a pane, or the whole region when it was the region's last one and other regions remain; when the window is down to a single pane in a single region it answers  `LastPane` and the caller closes the tab instead.  is the shared removal path, so an exited session's pane collapses the same way a deliberate close does.

### Pane Session Reconciliation

The two halves of the truth move on different threads — the IPC reader owns the session list and the focused session, the GPUI thread owns the split trees — so  settles them once per frame rather than letting the reader touch GPUI entities.

Three things can be out of step. The root region starts on a client-minted `WorkspaceId` because the shell exists before the first `Welcome`, and adopts the server's through  once a `SessionList` names one. A pane whose session exited is retired by . And a session the server has just created has no pane yet: a split queues the pane that asked for it (), and anything else — a new tab, a reattach, a refocus after an exit — goes to [[crates/scribe-client/src/main.rs#TerminalView#tab_adoption_pane]], which answers with the focused pane of *the tab's own region*. That is the single adoption rule, shared with [[crates/scribe-client/src/main.rs#TerminalView#switch_tab]] so a pointer click and a reconcile pass cannot place the same session differently. Only a tab under a workspace this window shows no region for — a reattach after reconnect, a strip that outlived its region — has nowhere of its own to go and falls back to the window's focused pane.

The exit path is region-scoped end to end. [[crates/scribe-client/src/tab_session.rs#TabSessions#remove]] refocuses onto the nearest surviving tab of the exited tab's own region and stops there; a region that runs out of tabs is dropped rather than handing its selection to a strip neighbour. [[crates/scribe-client/src/pane_shell.rs#PaneShell#retain_sessions]] keeps a region's last pane alive (empty) when its workspace still holds tabs, so a hidden tab is never orphaned by its shown sibling's exit; only a tabless region collapses and is closed on the server. [[crates/scribe-client/src/main.rs#TerminalView#fill_empty_region_panes]] then refills each surviving empty pane with an unshown tab of its own workspace, streaming it via attach/subscribe without touching the active session ([[crates/scribe-client/src/main.rs#TerminalView#stream_session]]) so an unfocused region repopulates without stealing the keyboard.

Root-ID adoption settles before the `WorkspaceInfo` answers the reader parked,
so initial metadata matches a server-owned region instead of being discarded as
unclaimed. Metadata still precedes pane/session adoption, allowing a workspace
split's answer to re-key its region before that region claims its new session.
`SessionList` workspace entries join the same ordered queue, so reconnects
restore each slot's name, accent, and project root without waiting for another
CWD change. See .

### GPUI Workspace IPC

A workspace region is a *server* concept: it owns the id, the accent colour and the name. The shell's regions are therefore reconciled with the server over four client frames and one server answer, rather than staying client-local layout.

`workspace_split_*` opens the region client-local (only the server may allocate a `WorkspaceId`, and `CreateWorkspace` carries none) and immediately sends .  queues the region as awaiting a server workspace, so the answering `WorkspaceInfo` can be matched to the region that asked by the FIFO order of the one ordered writer channel — which is the only correlation available, since the request names nothing.

That answer arrives on the reader thread, where  splits it in two. The display name goes straight into the shared  the status bar renders its workspace segment from — the same store `WorkspaceNamed` writes, so the two channels cannot disagree — and an absent name clears it. The id, accent and project root are parked for the GPUI thread, because a region is an entity the reader must never touch. Live `WorkspaceNamed` updates park their project root through the same path while retaining the region's existing accent.  drains them at the top of the reconcile pass and  folds each onto its region: a known id is a metadata refresh, an unknown one re-keys the oldest waiting region, and anything else is reported as unclaimed rather than silently renaming an unrelated region.

Draining before the session adoption below is what makes  work. The session a workspace split seeds is necessarily created through the workspace the tab strip was pointing at, because the new one does not exist yet; once the pane that adopts it turns out to sit in another region,  tells the server so and re-files the strip entry, so a later split seeds its session in the region the user is actually looking at.

Collapsing a region sends . Both removal paths report it — a deliberate `close_pane` through `::Removed` and an exited session through  — and only regions the server itself minted are ever named, because the server has never heard of a client-minted id.

Every one of those mutations ends in , which sends  with the tree  serializes. The topology comes from the  that owns it and the per-region payload from the shell's own pane trees, because the GPUI shell keeps panes in  entities rather than in the workspace model's tab list; a region maps to one tab whose pane tree is the region's whole split, which is the wire shape the winit client restores from. Panes still waiting for a session are pruned rather than serialized under a synthetic id, so a reconnect cannot restore a pane pointing at a session that never existed. Because the report is called from paths that fire far more often than the topology moves, the throttle is an exact equality check against the last reported value — which is sound precisely because that value *is* the wire payload.

### Per-Pane Grids And Sizing

A split window shows several live terminals at once, so  keys one  per session instead of folding every pane into a single grid.

The coalescing drain already carries a `SessionId` with every batch, so  advances the grid the batch names and a background pane's burst can never land in the focused pane's scrollback. `PtyOutput` / `SessionReplay` / `ScreenSnapshot` are gated on the set of attached sessions () rather than on the single focused one, because every pane streams.

#### Parsing off the registry lock

A VTE parse is the longest thing the client does with a lock held, so [[crates/scribe-client/src/terminal.rs#PaneGrids]] guards only its own map and every parse runs with that lock released.

Each entry is a [[crates/scribe-client/src/terminal.rs#PaneGrid]] behind an `Arc`: [[crates/scribe-client/src/main.rs#resolve_batch_panes]] resolves a whole batch's panes under one short registry lock and releases it before a single byte is parsed, and [[crates/scribe-client/src/main.rs#live_panes]] does the same for the expiry sweep. Forgetting a session therefore never waits on the batch being parsed into it — the removed handle simply outlives the map entry.

Each pane is split into two independently locked halves. [[crates/scribe-client/src/terminal.rs#PaneStream]] holds the sync-frame queue and the grid it feeds, because a committed frame leaves the queue and enters the parser in the same step; that lock is held for as long as a batch takes to apply. [[crates/scribe-client/src/terminal.rs#PaneFrame]] is the projection the paint pass reads — snapshot, scroll metrics, selection spans, cursor placement, geometry — republished out of the stream by [[crates/scribe-client/src/terminal.rs#PaneGrid#with_stream]] after every mutation. Every grid mutation goes through that one method, which is what lets the projection be republished unconditionally rather than guessing which edits changed something paintable.

The snapshot itself is shared rather than copied: [[crates/scribe-client/src/terminal.rs#DisplayOnlyTerminal#make_content]] publishes an `Arc<Content>` that the projection, the pane element and every reader hold by reference, so republishing after a parse is a refcount bump instead of a copy of every painted row.

Building that snapshot is the expensive half, and it is charged per presented burst rather than per parsed frame. [[crates/scribe-client/src/terminal.rs#DisplayOnlyTerminal#advance_output]] only marks the snapshot stale; [[crates/scribe-client/src/terminal.rs#DisplayOnlyTerminal#publish_content]] rebuilds it once, at the end of the drain pass, which is the only state a redraw can show.

Two orderings hold the design together. Nothing on the paint path takes a pane's stream lock: a frame that had to queue behind a parse is a dropped frame. And the prompt-mark store is always taken *inside* a pane lock, the order the drain takes them in when it anchors a mark against the grid it just advanced — [[crates/scribe-client/src/main.rs#TerminalView#jump_to_mark]] follows the same order for exactly that reason, because the inversion is the one that could deadlock a jump against a firehosed pane.

 runs after any layout change: each pane's rect yields a cell count, which is reshaped locally through  and announced to the server as `Resize` followed by `RequestSnapshot` — the client owns no PTY and never reflows locally, so the authoritative grid has to come back from the server. Unchanged panes are skipped, so a redraw storm never becomes a `RequestSnapshot` storm.

The cell count divides real pixels by the live cell box, which is why the viewport above is measured rather than derived from the font: a rect stated in the font's own cells moves its numerator and denominator together, so every font size yields the same `cols`x`rows` and a zoom step would leave the freed pixels dead while telling the server nothing new.  closes the other half of that loop — it compares the measured area against the last published size and republishes exactly once when a window resize or a chrome band moved the grid's boundaries.

What arms that comparison is , the invisible canvas that measures the band. The rect is written during *prepaint*, after the render that built the canvas has already run, so the single repaint a window resize buys still reads the pre-resize area and — with nothing scheduling another frame — the panes would be re-laid on screen while every PTY kept its old size. The measuring write therefore reports whether the rect moved () and defers a call back into `sync_grid_geometry` when it did, which keeps the publish on the view rather than mid-paint while guaranteeing it sees a measured area. Gating on a *moved* rect is what keeps it a single republish instead of a per-frame `Resize` storm.

Because the terminal-navigation surfaces () all act on the live `Term`, they reach it through  rather than a window-wide terminal: scrollback, vi/copy mode, the split-scroll pin, the jump chip and smart selection all resolve against the pane the user is in. `active_session` names that pane's session by construction, since both  and the reader's attach path re-point it on every focus move. For the same reason only the focused pane publishes its painted  and its find-match spans; a background pane paints its own untouched grid.

### GPUI URL Detection Port

The GPUI rebuild ports the URL and OSC 8 scanner into the `lib` target so hover, Ctrl-highlight, and open affordances reuse the same  logic byte-for-byte across the cutover.

 is a verbatim port of the winit  onto Zed's Alacritty fork: the same scheme list (https/http/ftp/file/mailto/ssh/telnet), `WRAPLINE` join, trailing-punctuation stripping, per-row  geometry, hard-break continuation (), and OSC 8 precedence with `id=` reconnection and the 2048-byte URI cap (). Because the selection port lands in a separate bead, the two grid cell readers (, ) are defined locally instead of imported from `selection`.

Activation is ported alongside detection:  (with the `:N` line-number suffix and `code --goto` fallback), , and the disallowed-scheme gate hook (, ). The view-side hover/dwell/Ctrl-highlight wiring lands in a later GPUI phase.

### GPUI IME Composition

The GPUI rebuild ports the winit  preedit semantics onto GPUI's IME plumbing so composition anchors on an absolute scrollback row with an underline overlay.

IME needs a real compositor, so it is covered by an ibus-driven E2E and a manual parity procedure rather than a `#[gpui::test]`.

 is the verbatim data port (redacted-`Debug` composition text, optional caret hint, absolute start row + column).  is a GPUI-free state machine mirroring the winit `WindowEvent::Ime` arm: a non-empty `mark` arms/updates the composition anchored at the last `set_anchor` cell, an empty `mark` clears it, and `commit` clears and returns the committed text. The in-flight anchor stays fixed while a later `set_anchor` only affects the next composition.

 wraps the machine in a `gpui::Entity` and implements `gpui::EntityInputHandler`: GPUI routes marked (composing) text through `replace_and_mark_text_in_range` and committed text through `replace_text_in_range`, which re-emits  `Commit` so the view sends it through the normal `KeyInput` path. The terminal owns no editable document, so `selected_text_range` reports an empty selection at the composition point — X11 candidate placement asks for a selection before it will honour a rect, and `None` parks the popup at the window origin.

#### Registering the Handler

The platform only accepts an input handler *during paint* and only for the frame it is registered on, so the window has to re-offer it every frame or the OS has nowhere to deliver marked and committed text.

 creates the one `Ime` entity per window and subscribes to its commits;  refreshes the composition anchor from the focused pane's  and packs the frame's . Only the focused pane receives it, because a window holds exactly one input handler.  then calls `Window::handle_input` from inside the grid's paint pass with the *cursor cell's* rect rather than the whole grid's, which is what makes the OS candidate list hang under the composition point.

Registering a handler changes what GPUI does with ordinary keys: an un-stopped `KeyDown` is followed by `replace_text_in_range(key_char)`, the "insert the typed character into the focused text field" behaviour an editor wants. A terminal has already encoded that keystroke itself, so the root key listener calls `stop_propagation` on every `KeyDown` — without it every printable character is typed twice and keys consumed by vi mode or a binding leak to the PTY as well. A real input method is unaffected: composed text arrives through the platform's own commit callback, which propagation does not gate.

A composition is retired by  on focus loss and on any keystroke that reached the byte encoder. The latter is load-bearing for GPUI's xkb-compose path, which marks a dead key as preedit and then delivers the composed character as an ordinary `KeyDown` without ever retracting the mark.

#### Preedit Overlay Geometry

 recomputes the  each frame from the anchor and the live , returning `None` while scrolled into scrollback so the underline never renders at the wrong visual row.

The absolute  `start_row` minus `viewport_top_abs_row` resolves the on-screen line, so terminal scroll keeps the underline pinned to the originating line; a row above or below the visible window, a non-zero `display_offset`, or an anchor column past the right edge all yield `None`.  sizes the underline via `unicode_width` advances (wide CJK glyphs reserve two cells, zero-width marks ride the base glyph, a leading combining mark is skipped), matching the renderer's styled-run accumulator.

 draws the resolved overlay in the legacy renderer's three layers — an opaque backdrop that hides the cells underneath (opaque even under a translucent window, or they would read through the glyphs), the composition glyphs at their natural advances, and a foreground rule marking the text unconfirmed. The grid itself is never mutated, so cancelling a composition leaves the pane exactly as it was.  trims the text to the cells left on its row against the same `unicode_width` budget, so backdrop and underline are exactly as wide as the glyphs the shaper lays down.

#### IME Parity Procedure

IME depends on a live input-method engine. `tests/e2e/visual/ime-preedit.sh` () drives a real one in the container; the manual procedure below covers the display-server and interaction axes the rig cannot reach.

1. **X11 + ibus/fcitx**: start an IME engine (`ibus-daemon -drx` or `fcitx5`), select a CJK input method, run `scribe-client` under X11, focus a pane, and type a multi-key composition (e.g. Japanese `nihongo`). Verify the underlined preedit appears at the cursor cell, updates in place as keys arrive, the OS candidate window anchors under the composition, Enter commits the selected text to the PTY (preedit clears the same frame the echo lands), and Escape cancels with no bytes sent.
2. **Wayland**: repeat under a Wayland compositor with `text-input-v3` (e.g. GNOME/Mutter or Sway with `fcitx5`), confirming identical compose/commit/cancel behaviour and candidate placement.
3. **Scrollback anchor**: begin a composition, scroll the viewport up into scrollback, and confirm the preedit overlay disappears while scrolled and reappears pinned to the originating line on return to the bottom.
4. **Focus loss**: switch window focus mid-composition and confirm the preedit retires immediately with no committed bytes.

### Bracketed Paste Gate

The GPUI rebuild ports the winit  spec-011 gate so a risky paste is parked behind a confirmation before any byte reaches the PTY. The pure classifier and the entity gate are covered by `#[gpui::test]`.

 is the byte-for-byte classifier port: it returns  iff the content has a line break (`\n`/`\r`) or a non-tab control/escape character.  is a `gpui::Entity` whose `request` emits  `Confirm` (and parks a ) only when the `terminal.paste_confirmation` config is on, the focused pane has NOT enabled bracketed paste, and the content classifies as risky; otherwise it emits `Send`. The enabled and bracketed checks short-circuit before classification so the common path adds no work.

On the user's answer, `confirm` re-emits `Send` on the exact parked bytes — bypassing the gate, matching the winit resume path — while `cancel` drops the parked paste without sending anything.

### Bell Routing

The GPUI rebuild ports the winit `handle_bell_event` suppression gate onto an entity that routes a terminal bell to a per-tab attention badge plus the system bell, wired end to end from the live IPC reader to the window's attention request.

 is a `gpui::Entity` tracking window focus, the focused session, and whether an update is in progress. `on_bell` records an attention badge and emits  `Signal` (the view rings the OS bell / requests window attention) only when the bell targets a session other than the focused foreground pane — or the window is unfocused — and no update is in progress; a bell to the already-focused foreground pane is suppressed, exactly like the winit client. `focus_session` retires that session's badge.

The gate cannot run on the IPC reader thread: it is a GPUI entity and the action it authorises is a window-level call.  therefore only records which session belled, onto a queue shared with the foreground, and bumps the redraw generation.  drains that queue on the window-lifecycle tick, refreshing the gate's three inputs from where they actually live first — the focused pane from the shared `active_session`, the in-flight update from the shared  (the winit client read `update_available.is_none()` at the same point), and window focus from . A queued bell is therefore judged against the focus state it is delivered under, not the one it arrived under.

 subscribes the view to the controller *in* the window, so a `Signal` arrives with the `Window` that  needs: it calls `Window::request_attention`, GPUI's equivalent of the winit client's `request_user_attention(Informational)`. On X11 that sets the `WM_HINTS` urgency flag, which is what makes the routed bell observable from outside the process — see .

### GPUI Terminal Selection Port

The GPUI rebuild ports the winit client's terminal interaction state — mouse selection, smart selection, vi/copy mode, and regex search — into the `lib` target so the paint path and clipboard reuse the same logic across the cutover.

 is a port of the winit  onto Zed's Alacritty fork.  carries a  (cell/word/line) and normalizes endpoints for hit-testing ().  walks the grid trimming trailing spaces and joins `WRAPLINE`-continued rows without a newline;  and  follow the same wrap flags across screen rows, and  maps pointer pixels to absolute grid lines.  drives an interactive drag: the `start_cell`/`start_word`/`start_line` gestures set the granularity, `drag_to` extends by that granularity, and  yields the selected text for copy-on-select.

 ports the winit smart-selection matcher: it compiles the configured regex rules, matches the logical line under the cursor (), ranks candidates by precision then length, and resolves iTerm2 action parameters — legacy `\0`..`\9` captures plus interpolated `\(matches[N])`/`\(path)` forms — via .

 cribs Zed's regex search: it collects every match across scrollback and the viewport through the fork's `RegexIter` and cycles a highlighted match forward () and backward () with wraparound — the matcher for a locally-owned `Term`, which the live find surface in  does not use because this client's scrollback lives on the server.  wraps the fork's built-in vi mode — , , and  — so keyboard copy-mode navigation shares the selection coordinate space.

### GPUI Platform Integrations Port

OS-integration surfaces the GPUI client owns beyond the terminal grid: local server lifecycle, window geometry persistence, the X11 focus guard, and drag-drop path insertion — each a faithful port of the winit helper with a GPUI-native entry point.

 connects to the frozen local IPC socket, starting the systemd user service (, which first syncs the GUI environment into the user manager) and waiting for it to accept.  is the pure decision that flags a connected server whose binary drifted (different path, or rebuilt after the process started);  is the last-ditch recovery that force-stops the unit and any surviving process, clears the stale IPC and handoff sockets, and starts a fresh server. macOS launchd support is deferred with the rest of the macOS port.

 persists one  per window under `$XDG_STATE_HOME/scribe/windows/<id>.toml`.  is the first-launch geometry-compat step: geometry saved by the OS-decorated old client would restore mis-inset under the new custom titlebar, so it clamps the size and grows a non-maximized window's height by  once, recording `titlebar_normalized` so it never runs twice.

 polls `_NET_ACTIVE_WINDOW` and suppresses keystrokes while a compositor overlay obscures the window, reading GPUI's Xcb/Xlib window id via  (per the XID capability spike); non-X11 backends yield no id and leave the guard off. Its suppression timing lives in the pure  so it is testable without a display server.

The module remains available on non-Linux targets with an inert guard API, so the platform-agnostic window lifecycle compiles unchanged while X11 dependencies and behavior remain Linux-only.

 starts the guard from the live window-open path:  hands it the real `Window`, which is the first point an Xcb window id exists. Three call sites keep it honest —  refreshes it every  so an overlay opening while the user is idle is noticed before their next keystroke (the GPUI client has no winit-style event-loop tick to piggyback on),  clears the reactivation debounce on a genuine activation because compositor overlays never send one, and  is the first gate on the key path — ahead of the overlay router, the bindings, and the PTY encoder — so a key aimed at the overlay lands nowhere in the client. Every dropped keystroke is logged, since silently vanishing input is precisely this guard's failure mode.

The poll has one active-only lifecycle repair: when `_NET_ACTIVE_WINDOW` positively names this XID while `WindowLifecycle` still says blurred, it sends `true` through the same activation update and deduplicated `FocusChanged` path as GPUI's callback. Inactive or failed EWMH reads never synthesize blur; they remain input-suppression signals, preserving compositor-overlay behavior while repairing callbacks GPUI misses during immediate alternate-screen restore.

 quotes a dropped file path for the focused pane's shell via  (POSIX, fish, PowerShell, or nushell) and appends a trailing space; per FR-013 it bypasses the paste-confirmation gate because the path is already quoted.

### Server Lifecycle Wiring

The running client reaches the lifecycle port:  opens its one local connection through  rather than a bare `UnixStream::connect`, so a client launched with no server running starts one instead of failing the window.

After the first authoritative `SessionList`, a genuinely fresh window with no sessions requests one login shell through `CreateSession`. The one-shot is disabled for cold-restart replay and existing-window claims, and a non-empty first list consumes it, so reconnects cannot add surprise tabs. The reader stages the shell's launch binding before enqueueing the request, allowing the foreground restore state to retain the same environment-envelope launch id after `SessionCreated` arrives.

A refused connect is diagnosed before the autostart is attempted.  separates the two cases that look identical at the syscall: no socket file at all means no server has ever run for this user, while a socket file that refuses connections is the residue of a server that died without unlinking it. That sentence is carried into the error the autostart failure returns, so the status line names the stale socket instead of showing a bare `systemctl` exit code, and  logs it as well — the window is one line wide and outlives nothing.

Once connected,  holds the server on the far end up against the binary installed beside this client. The peer PID comes from the kernel (`SO_PEERCRED`) through , so it names the process actually serving this connection;  resolves the sibling binary, and the pure  decides. A drift publishes a status line rather than forcing a restart: a package upgrade or a local rebuild under a live process is worth naming, not worth killing the user's panes over.

Verified against the running app rather than headlessly: see .

### Drag-Drop Wiring

A file dropped on the window reaches the pane.  registers `on_drop` for GPUI's `ExternalPaths` on the root element, so the whole window is a drop target.

GPUI lowers a compositor file drop onto an ordinary drag whose payload is that type, which is why the listener goes on the root rather than on the grid — exactly the coverage winit's `DroppedFile` had.

 quotes each path for the focused pane's shell and delivers it. The shell it quotes for is the server's own `shell_name`, recorded per session on  by `SessionCreated` and by the authoritative `SessionList` — deliberately not the tab label, which the server overwrites with the OSC 0/2 terminal title as soon as one arrives. Delivery reuses  so the bytes are chunked to the server's `KeyInput` limit and wrapped in DEC 2004 markers when the application asked for them, but bypasses the spec-011 paste gate per FR-013: the path is machine-generated and already quoted, so there is nothing for a confirmation to protect against.

Verified against the running app rather than headlessly: see .

### GPUI Clipboard and OSC 52 Bridge

The GPUI rebuild ports the host clipboard integration — arboard handle, two-hop OSC 52 bridge, Linux primary selection, AI copy-cleanup — into the `lib` target, unit-tested off any display server.

 speaks to a  trait so the pure logic runs against an in-memory fake in tests while the shipped  backend performs the real I/O: on Linux `ClipboardSelection::Primary` routes through arboard's `GetExtLinux`/`SetExtLinux` primary target, and everywhere else (and Wayland, per spec Assumptions) it collapses onto the system clipboard. Handle creation can fail, so a `None` arboard handle reports  `Unavailable` for every call.

Spec 010's bridge is verbatim from the winit client's `App` methods:  services a `ClipboardBridgeReadRequest` and  wraps its `Result` into the outbound `ClientMessage::ClipboardBridgeReadReply`;  applies the FR-019  first, silently dropping a write on an unfocused window when `focus_gate_writes` is on so a background PTY program cannot hijack the clipboard.  builds the confirmation-overlay `ClipboardPromptResponse`. Middle-click paste reads through , and copy-on-select writes the primary selection through , which runs the  transforms (dedent, blockquote/decoration strip, unwrap) ported byte-for-byte from the winit .

### Clipboard Wiring

The running client reaches all of the above: it announces `clipboard_gating: true` in `Hello`, so the server routes OSC 52 through it instead of taking the headless-deny path, and it answers every frame that comes back.

Nothing is performed on the IPC reader thread. arboard is a window-thread resource and the FR-019 focus gate can only be judged against a live window, so  folds `ClipboardPromptRequest`, `ClipboardBridgeWrite`, and `ClipboardBridgeReadRequest` into the shared  — a parked prompt plus a bounded job queue — and the window-lifecycle tick drains it through . A frame arriving before the capability was negotiated is dropped, exactly as the winit client drops one: without that bit the server should not have sent it. The queue is bounded at  because a PTY program can emit OSC 52 far faster than a 200 ms tick drains.

The confirmation modal resolves through the same  path every other dialog uses:  correlates the choice back to the parked `request_id` and sends it through , the one seam the two spec-010 client answers leave by. The reply is not optional — the server holds the PTY-side program until the id resolves — so Esc and a backdrop click send a deliberate `DenyOnce` rather than nothing, and an `Always*` choice additionally persists the axis through .

Copy and paste are the user-facing half. A drag over the grid drives  /  at the granularity  classifies (click / double / triple), and the range is projected onto the painted viewport by  and painted by  in the theme's selection colours, under the find-match accent so a match inside a selection still reads as a match. Settling a drag publishes the text to the system clipboard and Linux primary selection when `copy_on_select` is on; the `copy` chord writes the same cleaned text to the system clipboard; typing clears the selection, because the highlighted region describes content the shell is about to overwrite.

Every paste — the chord, a middle click, and the context menu's Paste row — is requested through the spec-011 , so risky content parks behind  and resumes on the exact original bytes when confirmed. Delivery goes through , which splits the payload into frames the server's `KeyInput` limit accepts while keeping a single DEC 2004 marker pair across the whole paste.

Verified against the running app rather than headlessly: see .

### GPUI Notification Dispatcher

The GPUI rebuild ports the desktop notification dispatcher so one thread owns one D-Bus connection and `replaces_id` keeps a single toast per session, with click-to-focus decoupled from any concrete UI runtime.

 runs the Linux  backend on a dedicated thread (non-Linux platforms fall back to a drop sink; the macOS `notify-rust` path is deferred with the rest of the macOS port). It takes a  channel and emits  `FocusSession` on a caller-supplied channel instead of a winit event-loop proxy, so the transport stays runtime-agnostic until the GPUI event bridge lands in a consumer bead.

The `replaces_id` coalescing lives in the pure : it tracks the daemon id both ways so  reuses a session's live toast,  drops a stale reverse mapping when the daemon reallocates, an `ActionInvoked` click routes through , and `NotificationClosed`/session-exit/shutdown clear state via `on_closed`, `take_session`, and `live_ids`.  maps the config  onto the freedesktop `expire_timeout`. The zbus proxy signature keeps the freedesktop-spec argument count, the sole approved `clippy::too_many_arguments` suppression in the crate.

Unlike the winit client, the GPUI client shares its `zbus` build with GPUI's own Linux platform layer — `accesskit_unix` (AT-SPI), `ashpd` (XDG portals), and `oo7` (secret service) all resolve to the same crate version. Cargo unifies features across the whole graph and zbus's `tokio` feature is compile-time exclusive: with it on, every internal `Task::spawn_blocking` routes through `tokio::task::spawn_blocking`, which panics with "there is no reactor running" when polled outside a Tokio runtime. GPUI drives its zbus connections on its own non-Tokio worker threads, so enabling `tokio` crashed those background threads on every client launch. The workspace `zbus` dependency therefore keeps the default async-io backend, where zbus spawns its own internal executor thread and is runtime-agnostic. The dispatcher thread still builds a single-threaded Tokio runtime for its `tokio::select!` loop and `tokio::sync::mpsc` channels, neither of which requires the zbus transport itself to be Tokio-backed.

### Notification Wiring

The running client fires desktop notifications, split between the IPC reader and the foreground exactly as the terminal bell is.

The reader can judge neither focus nor config and owns no dispatcher handle, so  records each AI transition verbatim as an  and the foreground decides on its lifecycle tick.

 is the decision gate, ported from the winit .  fires only on the `Processing → attention` transition, so a session already sitting in an attention state does not re-notify and a replayed `SessionList` does not notify on first sight;  then applies the configured  against a  resolved per drain, so a transition is judged against the focus state it is actually delivered under rather than the one it arrived in.

 drains the queue and  builds the payload: the summary names the workspace and the state (), the body is the pane's most recent prompt, and both come from the shared chrome the status bar already renders so a toast can never describe a pane differently from the window. `AiStateCleared` and session exit both queue a `Cleared` notice, which retires the toast — one that outlives its pane is a click that can only land nowhere — and the exit paths send `NotifReq::Shutdown` through  so a fresh process does not inherit ids it cannot manage.

Click-to-focus closes the loop.  hands the dispatcher an output channel drained by a relay thread that parks the clicked session for the foreground;  routes it through the entity so  consumes the focus-on-activate fallback in the same breath, and the window-bound subscription runs  — selecting the session's tab and raising the window. The fallback exists for services that activate the app without naming the toast:  consumes it, and because both paths take the same token a dispatcher that does report the click never double-switches.

Verified against the running app rather than headlessly: see .

### GPUI Animation System

The GPUI client centralises UI motion policy so every shell transition (tab, focus, overlay) and smooth scroll resolves from one place, with a sanctioned off switch for latency purists and deterministic screenshots.

 resolves the policy from two inputs: the `appearance.animations` config bool (default `true`, added to the frozen , doubling as the reduce-motion user setting) and the `SCRIBE_DISABLE_ANIMATIONS` environment override, which wins when truthy so E2E runs can force motion off. This is the sanctioned exception to the "no new end-user features" Non-Goal, per `specs/016-gpui-client-rebuild/plan.md`.

When motion is enabled,  builds a `gpui::Animation` clamped to the 150 ms `MAX_TRANSITION` budget with an ease-out curve; GPUI's `AnimationElement` re-reads the animation when a new transition starts mid-flight, so transitions stay interruptible. When motion is disabled,  flips GPUI's global `App::set_reduce_motion`, which makes every `with_animation` render its static end state and schedule no frames — the byte-identical-screenshot determinism path. The `scribe-client` binary resolves and applies the policy at startup; the concrete tab/focus/overlay/scroll surfaces that consume `transition` land with the shell beads.

### GPUI Status Bar Port

The GPUI rebuild ports the winit client's window-level status bar so every ambient-state segment survives the cutover, lowered from the legacy quad renderer onto a GPUI flex row.

The legacy  emitted `CellInstance` quads into the terminal grid buffer at hand-placed columns. The rebuild splits layout from paint:  is a pure function turning  into a  of coloured  groups — left (: connection dot, command/env glyphs, 013/015 remote-control and share-presence surfaces, workspace, CWD), a centred update CTA (), and right (: CPU/MEM/NET/GPU sparklines, git branch, session count, tmux, transport, host, clock).  maps that model onto GPUI elements, letting flex-grown centre space keep the CTA centred instead of the legacy column arithmetic. Colours stay in sRGB via  because GPUI does its own linear conversion, unlike the raw-pipeline legacy renderer.

The sparklines are fed by , which ports the winit CPU/memory/network/GPU sampler's readings and rolling history buffers but samples them off the UI thread — see . The `scribe-client` binary wires the bar into its live view from , driving the connection dot from a shared connected flag, the sparklines from the sampler, and the centred CTA from .

 fills the metadata segments from the attached pane's entry in : workspace name, CWD, git branch, the env-degraded `⚠` glyph, the `tmux:` label, and the host label, which a remote-flagged session context overrides and a local pane leaves at the placeholder until the hostname surface lands. The workspace id and the session count both come from the tab strip — the only place the attached pane's workspace and the window's live sessions are known — and are resolved before the metadata lock is taken so the two are never held at once. The count is the number of open tabs in this window, matching the legacy client's pane count, not a boolean of whether a pane is attached. The centred update CTA comes from  instead; command status stays `None` until its own bead lands.

### Status-Bar Stats Sample Off The UI Thread

The status-bar sampler runs on its own thread and publishes snapshots the UI copies, because the underlying probes are slow enough to dominate startup-to-first-frame if the window waits on them.

The winit client sampled synchronously: `SystemStatsCollector::new()` called `sysinfo`'s `System::new_all()`, which walks every entry in `/proc`. On a busy host (≈1650 processes) that alone cost ~1.4 s, and it ran inside the GPUI view constructor, so the first frame could not paint until it finished — measured startup-to-first-frame was 2.28–3.04 s against the then-current 500 ms budget in `specs/016-gpui-client-rebuild/spec.md`. The old client still pays that cost on every launch: its `load_host_stats` step is ~1.2 s of its 3.4–4.7 s first frame.

Two changes remove it from the critical path.  narrows `sysinfo` to exactly what the bar reads — global CPU usage and RAM totals — so the process table is never enumerated.  then moves all probing onto a named background thread:  owns the `sysinfo` handles and the GPU probe, refreshes on the same 2 s interval, and publishes each result into a shared slot. That also gets the per-sample network and GPU reads (the AMD sysfs poll, or an `nvidia-smi` spawn where sysfs is absent) off the frame path they previously ran on.

 now only adopts the newest published snapshot under an uncontended lock, so it stays cheap enough to call every frame. Construction is non-blocking and returns zeroed stats, so the bar shows empty segments for the few milliseconds before the first background sample lands — a deliberate trade against blocking first paint. Dropping the collector clears the flag the sampler polls every , so the thread exits promptly with its window.

The residual startup cost is no longer in client code, and the client now measures that directly:  times `cx.open_window` and  splits the first-frame span into `gpu_bringup_ms` and `scribe_startup_ms`. Measured on the reference host, first paint lands at 634–780 ms, of which 610–751 ms is wgpu adapter enumeration and driver bring-up inside `cx.open_window` and only 24–29 ms is Scribe's own work. That floor is why the absolute 500 ms budget was retired — see .

## GPUI Update Surfaces

The terminal window learns about an available update from the server, renders it as the centred status-bar CTA, and sends the user's decision back — the `UpdateAvailable` / `UpdateProgress` / `TriggerUpdate` / `DismissUpdate` quartet, live.

 holds the latest of each broadcast behind one mutex shared between the IPC reader thread and the GPUI view, mirroring the winit client's `handle_update_available` / `handle_update_progress` pair.  writes it from explicit `UpdateAvailable` and `UpdateProgress` arms through , which bumps the shared redraw generation so the bar repaints;  reads it straight into 's `update_available` / `update_progress`, which is what  lowers into the CTA label. A progress state is deliberately not cleared by a later announcement, matching the winit client.

The CTA is a real control, not a label:  takes an optional  and attaches it only when `center_clickable` is set, so an actionable CTA gets a pointer cursor and an accent hover tint while "Downloading..." / "Update failed" stay inert. Clicking it runs , which asks  for the modal to raise — the restart-required flow outranks a pending version, exactly as the winit `open_update_dialog` match does.

 resolves the modal's choice. Confirming an install sends  and clears the pending version so the CTA stops offering it, with the server's own `UpdateProgress` taking over the label; declining (the "Later" button, Esc, or a backdrop click, all of which resolve to the safe  action) sends  and clears the whole state, because the server then suppresses re-notification for that version. The restart-required flow's "Continue" spawns [[crates/scribe-client/src/server_lifecycle.rs#spawn_update_restart_helper]], then sends `QuitAll` so every client window flushes its restore snapshot and exits before the helper cold-restarts the server and launches one replacement client.

## GPUI Window Lifecycle

The window's own lifecycle is live: the WM's close button raises the in-app close dialog, whose answer asks the server to kill this window or quit them all, and only the server's ack exits. The client also reports focus and polls the window list.

 is the one piece of state the two threads share for this, behind a mutex like the AI, chrome, share and update stores. It holds the window id from `Welcome` (adopted by  alongside the reader's own registry copy), the in-flight , the acknowledged , the controller list projected out of the last `WindowList`, and the focus last reported. Every decision in it is pure, so the request/acknowledge rules are unit-tested without a window; see .

Closing starts at , which  registers on the platform window's close hook and which always vetoes the platform close: the server owns this window's sessions and has to be told what to do with them. The shell-owned close chord (::`CloseDialog`) raises the same dialog through the same call, so the in-app command and the WM button can never drift apart.  then answers it — "Quit Scribe" sends , "Kill Window" sends  naming the adopted id, and Cancel (which Escape and a backdrop click also resolve to) does nothing. Neither request exits anything: [[crates/scribe-client/src/main.rs#on_window_lifecycle_message]] folds the server's `QuitRequested` or matching `WindowClosed` onto the shared state, and [[crates/scribe-client/src/main.rs#TerminalView#poll_window_lifecycle]] drains it on the GPUI thread. A `WindowClosed` naming a window this client never asked about is ignored, matching the winit client — an unrelated ack must not close a live window.

The two answers are not the same teardown, because one process hosts every window the user opened (see [[crates/scribe-client/src/main.rs#TerminalView#open_new_window]] and [[crates/scribe-client/src/main.rs#TerminalView#open_restored_window]], each of which starts a backend of its own and opens a window on the same app). `QuitRequested` ends the process — the server told every window to go. An acknowledged `CloseWindow` removes only the window that asked, and GPUI ends the process itself once the last one is gone. Quitting on both was the bug: killing one window closed all of them, and merely *closed* the siblings, since nobody had asked the server to destroy their sessions.

The killed window's IPC backend goes with it. The server answers `WindowClosed` and keeps the socket open, so nothing else would ever end that reader: [[crates/scribe-client/src/window_lifecycle.rs#WindowLifecycle#window_closed]] latches the ack — [[crates/scribe-client/src/window_lifecycle.rs#WindowLifecycle#take_exit]] is one-shot and drained by the other thread, so it cannot answer this — and both [[crates/scribe-client/src/main.rs#run_reader]] and [[crates/scribe-client/src/main.rs#supervise_connection]] stop on it. The supervisor is the one that matters: its redial would claim ([[crates/scribe-client/src/main.rs#IpcThread#window_claim]]) a window id the server has already dropped, resurrecting it as a ghost.

Focus reporting has two producers and one chokepoint.  is the focus observer registered on the live window, and the lifecycle tick reconciles pane changes the IPC reader caused (a reattach moves the focused pane with no UI event behind it); both call , where  collapses "is the window active" and "which pane is attached" into one gained/lost pair and drops the report when nothing moved. That pair leaves through , which is how the server knows to relay CSI focus events to PTY applications that enabled DECSET 1004.

The window-list poll rounds it out:  sends  on the same tick, throttled to  and gated on `remote.enabled` exactly as the winit client gates it, because the reply's only rendered consumer is the status bar's owning-machine remote-control summary. The reply lands in the same shared state and  reads it into , whose `enabled` and `controllers` were hardcoded off before.

The whole surface is verified against the running app, not headlessly: see .

## GPUI Window Chrome Layout

The terminal window is a flex column of chrome bands around one flex-grown grid, and its startup size is derived from that stack rather than hardcoded — so the whole grid and every band land on screen at once.

 stacks, top to bottom: the titlebar, terminal grid, optional prompt strip, and window status bar. Only the grid is flex-grown, so every pixel the chrome takes is a pixel the grid does not get. Connection and pane feedback share the window status bar; routine tab and attach success copy is omitted because the tab strip and connection dot already show it.

The window used to open at a hardcoded 960x680, which was the painted height of the 36-row grid (36 x 18.9 px) and nothing else. The always-present titlebar and status bar therefore came out of the grid: bottom rows were clipped away, and because each band was flex-*shrinkable* under a flex-grown grid, a shorter window would have squeezed the bands themselves rather than the grid. A redundant 26 px band was later removed rather than reserving permanent space for duplicate status copy.

 is now the single place the band heights are stated.  sums the titlebar and status bar (GPUI lays divs out border-box, so the bar's hairline is inside that number), and  adds the grid's own extent at the metrics it is *painted* with — the live `GridFont`, not the integer cell size reported to the server, because the painted metrics decide where the last row lands.  resolves that from the live `[appearance]` config and hands it to `Bounds::centered`, so a font-size change moves the default window instead of silently clipping more rows. At the shipped defaults that is 1008x739 rather than 960x680.

Two guards keep the derivation honest.  shrinks the request to the primary display, because a `font_size = 72` window taller than the screen would move the status bar off the *desktop* instead of off the window — the same defect one level up. And every chrome band carries `flex_none`, so a user-shrunk window clips the grid — the surface that can afford it — instead of squeezing status surfaces away.

The prompt strip is deliberately excluded from the reserved height: it exists only while the attached pane has prompts, so reserving its rows up front would leave a permanent dead band under the grid. When it appears it takes its rows from the grid and the bands below it stay put. Verified on the running app by .

## GPUI LAN Surface

The feature-014 LAN surface is live in the terminal window: an unknown device's approval prompt is raised and answered, the machine's own LAN environment and peers are probed, and `SCRIBE_LAN_DIAL` reaches a peer over mutual TLS.

 is the one piece of state the IPC reader and the GPUI view share for all of it, behind a mutex like the AI, chrome, share, update and lifecycle stores. It holds the parked approval prompt, the last `LanPeerList`, the  from the last `LanEnv`, and the  of this client's own dial. Its one-line  reports only actionable warnings and errors in the status bar; healthy peer counts stay in the picker.

### Owning side: the approval prompt

The owning server holds an unknown device — revealing nothing — and pushes `LanApprovalRequest` to its own local client, which raises the prompt and answers it.

 builds the ported  and *parks* it rather than rendering it, because a GPUI entity may only be built on the thread that owns the window;  takes it on the same 200 ms lifecycle tick that drains an acknowledged exit and raises it as `::LanApproval`.

Wrapping the ported model in the generic modal is what gives the prompt the backdrop, Tab/Shift+Tab cycling and click activation every other dialog already has, without a second dialog implementation. Two behaviours are deliberate: Approve carries the destructive button tone because it writes a `TrustedDevice` and admits a machine that has so far been shown nothing, and Esc or a backdrop click resolves — through  — to an explicit **Decline** that is still sent, because the peer's connection is held open until the `request_id` is answered.  puts that answer on the wire through .

### Startup probe: environment and peers

At startup the client asks its own server what its LAN surface looks like: this device's identity fingerprint and current-network addability, plus the peers discovered on that network.

 runs before the session connection is opened — `GetLanEnv` is a pre-`Hello` first frame answered on its own transient socket, so it has to be a separate connection anyway, and probing first means the window has its LAN summary before the first frame paints.  then folds the reply through the same  the live reader uses and sends  on the session connection, which the server answers only for a local one. Both are gated on `remote.lan.enabled`, exactly as the window-list poll is gated on `remote.enabled`.

### Connecting side: the mutual-TLS dial

With `SCRIBE_LAN_DIAL` set,  reaches a peer over TCP and pinned mutual TLS instead of the local Unix socket, gated by the owning side's device approval.

 fetches this machine's device identity from its co-located server over the local-socket-only `GetLanDialIdentity` — the sealed key is granted only to the binary that created it, so the client must never read the keyring itself — and builds the dialer from the server-owned `LanTls`, which keeps the SPKI-pinning verifier in one place rather than duplicating it client-side.

 then sends `LanHello` and reads the owning side's approval gate: an unknown device is answered `LanApprovalPending` and held with no timeout of our own (the owning user's decision legitimately takes as long as it takes, and the peer already bounds the hold), which surfaces as "Waiting for approval on the peer…"; a trusted device is admitted straight to `LanApprovalResult { approved: true }`. Anything short of acceptance ends the process rather than falling back to the local server, because silently attaching the user to the wrong machine is worse than not connecting. Past the gate the encrypted stream is interchangeable with the Unix socket, so both transports converge on .

`LanDialIdentity` carries private key material, so it is never stored in shared state and never logged: 's arm for one arriving out of band on the session connection logs only the presence flag.

The whole surface is verified against the running app, not headlessly: see .

## GPUI Remote Tailnet Surface

The feature-013 tailnet surface is live in the terminal window: the account and peers are probed, a displaced window freezes under a reclaim banner, `SCRIBE_REMOTE_DIAL` reaches a peer, and automation actions round trip.

 is the one piece of state the IPC reader and the GPUI view share for all of it, the tailnet twin of . It holds the last `RemotePeerList`, the  from the last `RemoteEnv`, this client's own , the displaced , the typed severance reason, and the queue of inbound automation actions. Its one-line  reports only actionable warnings and errors in the window status bar; healthy peer counts stay in the picker.

### Startup probe: account and peers

At startup the client asks its own server which tailnet account it is signed in to and which same-account peers are online.

 runs before the session connection is opened, for the same reason the LAN probe does: `GetRemoteEnv` is a pre-`Hello` first frame answered on its own transient socket.  folds the reply through the same  the live reader uses and sends  on the session connection, which the server answers only for a local one. Both are gated on `remote.enabled`, exactly as the window-list poll is. The same pair is re-requested by the command palette's Remote Connect row through , which ports the winit `request_remote_peers` seam so the picker overlay bead only has to add rendering. The Settings → Remote page reaches `GetRemoteEnv` independently, on its own transient socket, from .

### Displacement: the frozen reclaim banner

`WindowTakenOver` means another controller now drives this window. The client stops expecting output, suppresses every keystroke, and offers one action.

 stores the ported ; the render pass hangs  as the LAST child of the root, so the dimmed backdrop and its centred banner cover every other overlay — while the window is frozen there is nothing else to interact with. The key path checks  before the share prompt, before the shell chords, and before any binding: everything is swallowed except Enter, which  lowers onto the ported Enter-only gate. A click anywhere on the backdrop is the mouse half of the same affordance.

 clears the banner optimistically — matching the winit client, which drops the displaced connection and clears the state before its reclaiming `Hello` is answered — and puts the frozen v3 `::Claim` on the wire. Claiming rather than re-dialing is what makes the reclaim in-place: the GPUI client's connection is never torn down, so there is no visible close-and-reopen. A server that refuses the claim simply displaces this client again, which re-raises the banner.

`RemoteDisconnect` is the peer's best-effort final frame before it closes a link for a policy reason. Recording its typed reason is the only chance the window has to say *why* the connection went away instead of just dying, so it outranks the dial status in the window status bar.

### Connecting side: the tailnet dial

With `SCRIBE_REMOTE_DIAL` set,  reaches a peer over plain TCP instead of the local Unix socket. LAN wins a double-set, because the encrypted device-approved link is the preferred transport for the same machine.

### GPUI remote connect picker

The command palette's `Connect to remote machine…` row opens the GPUI picker over the live window, requests fresh tailnet and LAN peer snapshots, and gives the picker all keyboard input until it closes.

 resets the transport-free  state, requests both local discovery lists, and  folds the reader-owned snapshots into its peer step.  renders the picker above ordinary overlays. A LAN advertisement with another protocol version remains visible as a disabled row that names both protocol versions and tells the user which machine needs a Scribe update; it never emits a probe, and it does not hide a usable same-name Tailscale route. Selecting a compatible tailnet peer runs `RemoteHandshake` then `ListWindows`; selecting a compatible LAN peer builds its local identity, completes mutual TLS plus `LanHello` approval, then runs the same probe. Selecting a listed window launches a new client with the chosen dial environment, leaving the local window untouched. The visual E2E proves the overlay paints a discovered peer and sends the tailnet probe on the TCP wire ().

The picker starts its probes through  on the app-global Tokio bridge, rather than GPUI's foreground executor: TCP timers and sockets therefore always have a Tokio reactor, while every result and LAN approval notice returns through a GPUI task before changing the view. This keeps both transport handshakes asynchronous without letting a network task touch GPUI state.

 opens the TCP connection with `TCP_NODELAY` (Nagle would coalesce keystrokes into the previous packet) and  runs the mandatory preamble. Unlike the LAN dial there is no identity to build: identity is `tailscaled`'s `WhoIs` on the owning side, never a certificate this process holds. Anything short of acceptance ends the process rather than falling back to the local server. Past the gate the stream is interchangeable with the Unix socket, so all three transports converge on  — which is also where the picker's window claim and its explicit-attach `takeover` ride the ordinary `Hello`. An accepted tailnet dial lights the status bar's controlling-side transport indicator through .

### Automation round trip

`scribe action …` reaches a window as `RunAction`, and a client that cannot run a window mutation itself asks the server to route it.

An inbound `RunAction` is *queued* rather than executed by the reader, because the action it names opens tabs, splits panes and moves focus — all GPUI entities only the window's own thread may touch.  drains the whole queue on the same 200 ms lifecycle tick that raises a LAN approval, marking each action `::Server`.

The outbound half exists for one concrete case: a feature-015 **viewer**. Its keystrokes are already suppressed locally, and the window mutations a layout row would make are refused by the server for a non-controller, so running them locally would fail silently.  routes exactly those rows out as  with `window_id: None` (the server refuses any other id from a registered connection) and the server answers `ActionDispatched` naming the window it reached. Client-local rows — the find overlay, the settings window, a profile switch, tab focus, the update dialog — are never routed: they change this process, not the shared window. `ActionOrigin` closes the loop that would otherwise open, because an action the server delivered is already at the controller.

The whole surface is verified against the running app, not headlessly: see .

## Beads Board Reader Spike

This bounded prototype measures a direct embedded-Dolt snapshot and defines the optional one-shot reader boundary; it is not shipped product code.

[[tools/scribe-beads-board/main.go#readSnapshot]] resolves one embedded Beads
workspace, takes its shared workspace and physical-root maintenance gates, and
uses one Dolt connector and one read transaction. Beads' own list, Ready, and
Blocked functions produce a coherent source set; Scribe partitions it as Done,
Blocked, In Progress, Ready, then Backlog and bounds only the returned items.

The reader requires exact main and ignored/wisp schema cursor matches and
rejects missing storage, server/proxied modes, and gate contention. It has no
mutation calls and requests a read-only transaction, but raw `OpenSQL` remains
write-capable and embedded Dolt excludes every other `bd` process while open.

Redirect/source database identity is unsupported, standard status filters omit
custom or hooked statuses, and failures have no versioned JSON error contract.
This means the successful snapshot shape is versioned, but the prototype does
not provide a complete or mechanically read-only integration boundary.

Ten-process evidence in `tools/scribe-beads-board/README.md` compares the current
three-command installed-`bd` fallback and records an 89.73% median latency
reduction. A 116.5 MB stripped helper, 23.1 MB compressed artifact, reproduced
read/write lock collision, and unavailable local ARM64/macOS toolchains rule out
bundling the prototype unchanged.

The selected direction is a separately signed optional component behind a
server-owned memory/disk stale-while-revalidate cache. Cached state renders
immediately; missing, busy, timed-out, or schema-skewed helpers fall back to
installed `bd`. Each helper refresh is serialized per physical database,
bounded to 500–750 ms, and exits after one snapshot.

A disposable-fixture driver patch enabled Dolt's engine-wide `IsReadOnly` mode
and removed telemetry. Snapshot reads remained 0.15 s while INSERT, DDL, and
`DOLT_COMMIT` were rejected; logical database state and existing file hashes
did not change. Gate creation, NBS timestamp touches, and the roughly 150 ms
exclusive lock remain. Production still requires canonical redirect identity,
custom statuses, versioned errors, native packaging, and license inventory.

## GPUI Titlebar

The GPUI rebuild replaces native window decorations with a custom titlebar that also hosts the integrated tab bar. The pure layout/decay math is ported into a testable module; the interactive chrome is a `gpui::Entity`.

 holds the display-independent logic ported from the winit  — the self-decaying attention-flash envelope (, additively blended by  without touching alpha), fixed-width title truncation (), the colored context-% suffix banding with pulse suppression (), the workspace-badge gate (), and the drag-reorder slot math (, walking tab edges rather than an `f32`→`usize` cast). Colors stay sRGB in  because GPUI performs its own sRGB→linear conversion at paint time.

[[crates/scribe-client/src/titlebar.rs#TitlebarView]] assembles the custom chrome: move region, workspace group bars, integrated terminal tabs, and equalize control. Native decorations own window controls, and Settings opens from the status bar; equalize appears only when the focused tab has multiple panes. Activating it runs [[crates/scribe-client/src/main.rs#TerminalView#equalize_layout]], resetting every workspace-region and pane split to equal space — the same handler behind the status bar's balance button and the `equalize` keybinding (`ctrl+shift+e` by default), so the layout can be rebalanced without a pointer.

In a multi-workspace window the titlebar hosts tabs only for regions on the window's top row. [[crates/scribe-client/src/main.rs#TerminalView#partition_tab_strip]] splits the decorated strip: tabs of a region stacked below go to that region's own in-region bar (below), everything else stays in the titlebar, and each titlebar position keeps the [[crates/scribe-client/src/tab_session.rs#TabAddress]] of the tab it rendered so Select/Close/Reorder events resolve back to a region and a position inside it ([[crates/scribe-client/src/main.rs#TerminalView#titlebar_slot]]). [[crates/scribe-client/src/main.rs#TerminalView#apply_group_badges]] then marks every titlebar run with `TabData::group_region_x` and `TabData::group_accent`. A region's tabs are contiguous because a region owns its tab list, so no regrouping pass runs before a paint and opening a tab in an earlier region cannot produce a repeated region edge. Top-row regions' left edges are distinct by construction, so titlebar groups always absolutely align over their regions — the old duplicate-edge flowed fallback is gone with the stacked tabs that caused it. The region accent styles the group hairline and active-tab underline. A clickable [[crates/scribe-client/src/tab_bar.rs#GroupBadge]] is added only when shared server/chrome metadata provides a nonempty workspace name. Its deterministic colour comes from the configured badge palette; missing or cleared names render terminal tabs only, with no CWD-basename or literal fallback. The rightmost group extends to the window edge, crowded tabs shrink to `TAB_MIN_WIDTH`, and selecting a badge focuses its group's first tab.

An in-region bar carries the same drag-reorder its titlebar counterpart does, held in [[crates/scribe-client/src/main.rs#RegionChrome]] beside the bars it indexes into rather than inside `TitlebarView`, because the view renders those bars itself. Leaving it out made every tab below the first region the only unreorderable tab in the window. The target slot comes from pointer travel divided by `TAB_WIDTH` rather than the titlebar's absolute edge walk — a region bar's tab run starts after a workspace pill whose width follows its badge text, which a listener cannot measure — and the shared [[crates/scribe-client/src/tab_bar.rs#reorder_target_index]] is anchored half a tab left of the press so both bars put the swap boundary at the same half-tab overlap. A bar's slots and its region's tab positions are the same index now that a region owns its tab list, so the swap applies directly through [[crates/scribe-client/src/tab_session.rs#TabSessions#reorder]] and a drag cannot leave its region by construction rather than by an offset staying in range. Every swap reports the tree for the same reason the titlebar does — that report is the only place tab order is durable.

A region below the top row carries the legacy client's per-workspace tab bar instead: [[crates/scribe-client/src/pane_shell.rs#PaneShell#region_bar_rects]] reserves a `REGION_TAB_BAR_HEIGHT` strip at the region's top (every pane-geometry consumer shrinks through the same content-rect rule, so panes, dividers and hit-tests agree), and [[crates/scribe-client/src/main.rs#TerminalView#render_region_tab_bars]] paints the bar from the partition's [[crates/scribe-client/src/main.rs#RegionBarData]] — badge pill, then tabs with the titlebar's look (flash blend, AI dot, context suffix, active underline) minus drag-reorder and keyboard tab stops. A bar tab is "active" when its session is the one the region shows ([[crates/scribe-client/src/pane_shell.rs#PaneShell#region_shown_session]]), independent of window focus; clicking selects by session id, and the bar-set shape is logged on change as the scripted oracle (`tests/e2e/visual/workspace-split.sh`).

After reconnect adoption, [[crates/scribe-client/src/tab_session.rs#TabSessions#order_by]] restores the strip's region order. Titlebar tabs expose stable AccessKit roles, selection, Click actions, close controls, AI state, context usage, and attention flashes. [[crates/scribe-client/src/titlebar.rs#TitlebarView#update_drag]] reorders as the dragged tab crosses a neighbour, while [[crates/scribe-client/src/titlebar.rs#TitlebarView#end_drag]] preserves click-to-select when GPUI arms a drag but no reorder occurs. [[crates/scribe-client/src/titlebar.rs#pane_title_pill]] builds split-pane titles; `tests/e2e/visual/titlebar.sh` exercises the assembled bar.

### Window move region

Dragging empty titlebar space moves the window through an imperative
`Window::start_window_move()` call, not through a declared hit-test region.
`WindowControlArea::Drag` is kept for Windows but is a no-op on Linux.

Both the X11 and Wayland backends of the pinned GPUI revision implement
`on_hit_test_window_control` as an empty body, so the painted `Drag` hitboxes
are never consulted and the platform never starts a move on its own.
`start_window_move()` *is* implemented on both (`_NET_WM_MOVERESIZE` on X11,
`xdg_toplevel::_move` on Wayland), so [[crates/scribe-client/src/titlebar.rs#TitlebarView]]
drives it directly: a left press on the root titlebar records the press origin
in `move_arm`, and `advance_move_arm` starts the move once the pointer has
travelled `WINDOW_MOVE_THRESHOLD` px from that origin with the button still
held. A press that wobbles by a pixel stays an ordinary click. Mouse-up
disarms, as does a press outside the titlebar or any motion with no button
held — so a mouse-up lost outside the window cannot strand an arm that a later
press-and-drag would redeem.

The arm step is gated on `drag.is_none()`, so `None` means unarmed. GPUI
dispatches its bubble phase front-to-back, so a tab's own `on_mouse_down`
has already recorded the drag-reorder state by the time the root handler
runs — pressing a tab therefore reorders tabs and never moves the window. The gear, equalize,
and min/maximize/close buttons each stop propagation on left press for the same
reason, so a click with a pixel of pointer jitter cannot arm a move and swallow
the click. The same pattern is used by the settings window titlebar.

### Keyboard operation

Every interactive titlebar control is a GPUI tab stop in painted order: tabs,
their close targets, equalize when visible, settings, then
minimize, maximize, and close.

Their tracked focus handles are explicit tab stops with one shared tab index,
so GPUI insertion order supplies that sequence while equalize appears or
disappears.

Every tab's close node stays mounted in that order at zero opacity until its
tab is active or hovered, its tab has focus, or the close node itself has
focus. This keeps forward and reverse traversal stable because pinned GPUI
registers only mounted focus handles; focus reveals the glyph on the next frame.

Tab traversal is scoped to the chrome: once a chrome control has focus, Tab
advances that order and Shift+Tab reverses it, and focused controls paint the
accent focus treatment so keyboard position remains visible without a pointer.
A focused terminal consumes plain Tab and Shift+Tab as PTY input (`\t` /
`ESC [ Z`) — tab completion is core terminal behavior — with
[[crates/scribe-client/src/main.rs#TerminalView#traversal_claims_tab]] as the
gate. Titlebar controls keep their own key handlers, so traversal continues to
work from any focused chrome stop.

Enter or Space activates the focused control. On a tab, Left/Right move focus
between tabs, while Ctrl+Shift+Left/Right reorders the focused tab and retains
focus on it; neither route reaches the PTY. Closing a focused tab preserves
the active-tab invariant; the nearest remaining tab receives focus, while GPUI
continues its deterministic tab-stop order. Settings opens its singleton window,
equalize emits its normal titlebar event, and window controls invoke their
native operations.

Pointer activation is deliberately different from keyboard activation. A click
may focus the tab control for the duration of the event, but once the shell has
switched the live session it defers focus back to the terminal root so typing
resumes immediately without a second click.

### Tab flash envelope self-decays

Verifies  peaks at 1.0, eases down mid-envelope, and returns `None` at or past `TAB_FLASH_SECS` (and for negative/NaN inputs) so the flash self-clears and cannot pin the redraw loop.

### Flash blends accent without touching alpha

Verifies  returns the base color unchanged for `None`, mixes toward the accent by `FLASH_MAX_MIX` at peak intensity, and preserves the base alpha channel.

### Titles truncate with an ellipsis

Verifies  leaves short titles intact, truncates an overflowing title to exactly the available columns ending in an ellipsis, and flags truncation (driving the tooltip hover target).

### Title budget reserves AI dot columns

Verifies [[crates/scribe-client/src/titlebar.rs#title_columns]] reserves two extra columns while a tab shows the AI dot, on top of the padding/close and context-suffix reservations, clamping to zero on degenerate budgets.

Under-reserving let a full-width tab's title outgrow its slot when the dot appeared and wrap onto a hidden second line, showing as raised text. The title element also wears `truncate` so any residual overflow clips on one line instead of wrapping.

The shell's in-region bars render their own tabs at the same width with the same chrome, and carried a private column budget that skipped the dot and a title element without `truncate` — so the fix held in the titlebar while an alerting AI tab below the top row still rode up. Both bars now call `title_columns` and truncate, so the reservation cannot drift apart again.

### Context suffix bands and suppression

Verifies  returns `None` below the warn threshold, the warn color at the threshold, the danger color above the danger threshold, and `None` while the session is pulsing so it never competes with the attention pulse.

### Badge shown only for named multi-workspace

Verifies  shows a badge only for a nonempty server-provided name in multi-workspace mode, and hides it for a single workspace or an empty name.

### Drag reorder resolves the target slot

Verifies  walks tab edges to the hovered slot, clamps below the first and past the last tab, and treats an empty tab list as a no-op.

### Column-to-pixel conversion saturates

Verifies  converts small counts exactly and saturates at `u16::MAX` for pathological inputs, keeping the strict cast lints satisfied without an `as` cast.

### Selecting a tab activates it and emits

Verifies  marks the chosen tab active, clears the others, and emits `TitlebarEvent::SelectTab` with the activation source.

### Closing a tab removes it and reactivates

Verifies  removes the tab, keeps exactly one tab active when the active tab is closed, and emits `TitlebarEvent::CloseTab`.

### Drag reorder moves the tab and emits

Verifies a `begin_drag`/`update_drag`/`end_drag` sequence on  moves the dragged tab to the hovered slot and emits `TitlebarEvent::ReorderTab`.

### A drag arm without reordering still selects

Verifies `end_drag` with `click_swallowed` set selects the pressed tab when the drag never reordered: GPUI's drag engages after ~2 px of pressed travel and cancels the element click, so a jittered real-mouse click must still activate on release.

### Slot swaps track the dragged tab's centre

Verifies a swap requires the dragged tab's centre to cross into the neighbour's slot: a grab near the tab's edge does not swap the moment the cursor enters the neighbour, so slots cannot thrash near a boundary.

### A drag survives leaving the tab strip

Verifies drag updates far past either end of the strip keep the drag live, clamp the target to the first or last slot, and clamp the slide offset so the dragged tab never renders outside the strip.

### Release outside the strip commits the reorder

Verifies ending a drag after the pointer wanders out of the titlebar band clears the drag state and keeps the reorders committed while dragging — the `on_mouse_up_out` path in the rendered titlebar.

### Out-of-range interactions are no-ops

Verifies that select, close, and begin-drag on out-of-range indices leave the tab list unchanged and emit no events, so stray hit targets cannot corrupt state.

### Accessibility IDs survive tab reordering

Verifies titlebar tabs keep unique session-backed accessibility IDs when drag reordering changes their visual positions.

## GPUI AI Indicator

The GPUI rebuild ports the winit client's per-session AI state machine so pulsing pane borders, tab indicators, and the context store behave identically across the cutover. The state machine is pure and covered by `#[gpui::test]`.

 is a byte-for-byte port of the winit  tracker. It keeps the Layer-1 pulse envelope (attention states pulse for a bounded window from entry; `Processing` pulses only while alive, re-armed by state edges and PTY output via ) so a hung AI stops pinning the redraw loop (), the Layer-2 wall-clock  that removes a dead `Processing` state entirely, the keystroke-driven , and the workspace-level priority aggregation (, `PermissionPrompt > WaitingForInput > IdlePrompt > Error > Processing`).

The context-window percent is stored independently of the visible state () so it survives every state-pruning path; the pulse-suppression predicate () is the `pulsing` argument for the tab suffix banding that now lives in . The pulsing border geometry is , which excludes the tab bar and reuses the shared  strip math; the GPUI paint path fills those rects with the aggregated colour. `AiStateChanged`/`AiStateCleared` are verified by the visual-E2E harness.

 supplies each tab's indicator and context suffix, while  aggregates and paints each workspace border. Config reloads reconfigure the shared tracker, so active tab dots and pane borders immediately use the latest per-state signal colors. The redraw and lifecycle ticks advance pulses and clear stale processing; PTY output re-arms liveness and encoded keystrokes clear attention states.

### Provider toggle gates the indicator

Verifies  returns `None` for a provider disabled in `TerminalConfig`, so a toggled-off tool shows no indicator.

### Config reload updates active indicator colors

Verifies a live config reload updates both `tab_indicator_color` and `workspace_border_color` for an already-active state, so saved signal-color changes repaint both surfaces without a restart or new hook edge.

### Provider memory survives clears

Verifies a Codex session is remembered as a Codex provider (not Claude) so provider-aware clipboard cleanup never mistakes it for Claude Code.

### Processing pulse rests after idle window

Verifies a fresh `Processing` state pulses, then after `PROCESSING_IDLE_PULSE_SECS` of silence  reports idle so the shared redraw loop retires — the GPU-drain fix.

### Activity re-arms the processing pulse

Verifies fresh PTY output via  re-arms a rested `Processing` pulse, which rests again after renewed silence.

### A state edge re-arms the pulse

Verifies a repeated `Processing` state edge (an `update`) is treated as a sign of life and re-arms a rested pulse.

### Attention pulse rests after its window

Verifies an attention state (`WaitingForInput`) pulses for a bounded window measured from entry, then rests without being extended by later activity.

### Stale processing is cleared

Verifies  removes a `Processing` state with no liveness for `STALE_PROCESSING_CLEAR`, while preserving provider memory for clipboard cleanup.

### Fresh processing is not cleared

Verifies a just-updated `Processing` state is not treated as stale and stays tracked.

### Only processing is hard-cleared

Verifies an idle attention state is never hard-cleared by  — it must persist until the human acts.

### Activity re-arms the stale-clear timer

Verifies  resets the wall-clock staleness timer so a sign of life before the prune spares the state.

### Workspace border takes the highest-priority state

Verifies  aggregates several sessions to the highest-priority state's colour (`PermissionPrompt` over `WaitingForInput` and `Processing`).

### Border colour drops decayed sessions

Verifies  returns `None` when no tracked session drives a border.

### Context survives the stale-processing clear

Verifies  still returns the percent after a stale-`Processing` clear removes the visible state.

### Context suffix suppressed during attention pulse

Verifies  is true for `PermissionPrompt`/`WaitingForInput` and false for `Processing`, so the tab suffix yields to a pulsing attention state.

### Conversation change wipes the context

Verifies  drops the stored percent so a new conversation does not show the prior window's usage.

### Session removal drops the context

Verifies  clears the stored context percent for a closed session.

### Pane border edges exclude the tab bar

Verifies  offsets the border below the tab bar and produces corner-safe top/bottom/left/right strips.

## GPUI Prompt Bar

The GPUI rebuild ports the winit prompt bar's display-independent logic — elapsed-timer formatting with freeze-on-AI-stop, the segmented context meter, the `#N` count, strip height, and truncation — and lowers the visuals onto a GPUI flex strip.

 turns a  snapshot into a pure  (first/latest rows, count, elapsed, optional meter);  lowers it onto div rows (timer on row 1 with count/context on row 2 in the two-prompt state, everything on row 1 otherwise, plus the hover dismiss overlay). Its prompt-text child fills and centers inside the fixed row before truncation, so a large `terminal.prompt_bar_font_size` cannot paint the text above a bottom-positioned strip while the icon and timer stay inside it. The elapsed timer is computed by , which freezes at `latest_prompt_finished_at` when the AI stops and clamps a backwards wall clock; the reference clock is threaded in so the freeze is `#[gpui::test]`-verifiable without a live window.  gates the hover tooltip and  sizes the strip. The rendered strip is a visual-E2E surface.

The strip is per-pane chrome, matching the winit client: [[crates/scribe-client/src/main.rs#TerminalView#compose_pane_content]] stacks it inside the pane that runs the AI session — above or below that pane's grid per `terminal.prompt_bar_position` (default Top) — so it never spans neighbouring panes or regions. It is also per-tab: the model is keyed by the pane placement's *shown* session, so switching tabs inside a region swaps the bar with the tab, and a hidden AI tab keeps its prompt history for when it is shown again. [[crates/scribe-client/src/main.rs#TerminalView#publish_pane_sizes]] subtracts [[crates/scribe-client/src/main.rs#TerminalView#pane_prompt_bar_height]] from that pane's rect before deriving the PTY grid, and the render pass republishes pane sizes every frame (idempotent per session) because a prompt arriving or clearing changes one pane's strip height without moving the grid band the probe watches. A tab switch adopts the incoming session before [[crates/scribe-client/src/main.rs#TerminalView#stream_session]] resolves [[crates/scribe-client/src/main.rs#TerminalView#placed_pane_size]], so its first attach replay already reserves that tab's strip rather than briefly using the outgoing tab's row count.

The meter text itself comes from  so the prompt bar, the tab suffix, and the E2E assertions share one spelling;  colors it by the configured band and falls back to the bar's text color when a band hex fails to parse, degrading the color rather than hiding the percentage.

Both the reserved and the painted height come from one [[crates/scribe-client/src/prompt_bar.rs#PromptBarMetrics]], resolved per frame by [[crates/scribe-client/src/main.rs#TerminalView#prompt_bar_metrics]] from the live `GridFont` plus the optional `terminal.prompt_bar_font_size` override. Unset, the strip's glyph size *is* the grid's, so an `appearance.font_size` edit and a zoom step both carry the strip along instead of leaving it at a fixed 12px beside larger terminal text; set, the override scales the grid's row height and advance width by the same ratio so the row padding and the truncation measure stay proportional. Handing the one value to both [[crates/scribe-client/src/prompt_bar.rs#prompt_bar_height]] and [[crates/scribe-client/src/prompt_bar.rs#render]] is what keeps the band the pane reserves identical to the band it paints — a drift there sizes the PTY grid against rows that are not on screen.

### Hover, reveal, and dismissal

Hovering a prompt row instantly reveals its full text, tints that row, and puts the dismiss control in the strip's left lane; clicking it takes the bar down for that pane.

The renderer owns the visuals and the view owns the state, the split [[crates/scribe-client/src/status_bar.rs#StatusBarActions]] already uses: [[crates/scribe-client/src/main.rs#TerminalView#prompt_bar_actions]] builds a [[crates/scribe-client/src/prompt_bar.rs#PromptBarActions]] carrying the pointed-at target in and the listeners back out. Hover lives in [[crates/scribe-client/src/main.rs#PointerState]] as a `(session, target)` pair, so a split window tints exactly one pane's strip, and a leave only clears the hover it names — GPUI does not order the old row's leave before the new row's enter, and clearing blind would drop the fresh hover as the pointer slides between rows. Each strip takes a session-derived element id so two panes never share GPUI's hover or tooltip state.

The reveal is a GPUI tooltip with [[crates/scribe-client/src/prompt_bar.rs#PromptTooltip]] as its view and its show delay set to zero: the default half-second delay reads as a hint arriving late rather than as the row expanding. Only a clipped row gets one — [[crates/scribe-client/src/prompt_bar.rs#is_prompt_truncated]] measures the row's text against the painted strip width and the strip's own advance width (`PromptBarMetrics::cell_width`, scaled by the same override ratio as the row height), so a prompt that already fits raises no popup.

[[crates/scribe-client/src/prompt_bar.rs#dismiss_overlay]] is built only while the strip is hovered, and it sits inside row 1's own hitbox, so pointing at the `×` keeps that row hovered and the overlay stays up long enough to click. The click routes to [[crates/scribe-client/src/main.rs#TerminalView#dismiss_prompt_bar]], which sets `PromptBarData::dismissed`; [[crates/scribe-client/src/main.rs#AiChrome#visible_prompts]] is the single gate both [[crates/scribe-client/src/main.rs#TerminalView#prompt_model_for]] and [[crates/scribe-client/src/main.rs#TerminalView#pane_prompt_bar_height]] read, so the strip stops painting and stops reserving rows in the same frame and the redraw hands them back to the PTY grid. The flag rides on the prompt record, so it lifts exactly where the record does — [[crates/scribe-client/src/main.rs#AiChrome#note_conversation]] retiring the history on a conversation switch, or [[crates/scribe-client/src/main.rs#AiChrome#forget]] dropping it when the provider or the session exits. That is the legacy boundary: dismissed for the rest of the conversation, back for the next one. A restored pane starts undismissed, because the gesture is not carried in the launch record.

### Live AI wiring

The GPUI client feeds the bar from the IPC reader: `AiStateChanged`, `AiStateCleared`, and `PromptReceived` land in a shared AI chrome record that the view reads on every frame.

`AiStateChanged` updates an  (whose decoupled context store keeps the percentage alive across pulse pruning), `PromptReceived` appends to the pane's , and `AiStateCleared` plus `SessionExited` both route through [[crates/scribe-client/src/main.rs#AiChrome#forget]], dropping the tracker state, remembered provider, context percentage, and prompt history in one call. A Claude Code or Codex exit therefore takes down both the pane's prompt bar and the provider gate that permits split-scroll; neither survives as AI chrome in a plain shell tab. Each mutation bumps the redraw generation, so the strip repaints without polling. On render the view builds the model with a context indicator whenever the tracker holds a percentage — the prompt bar is the surface that always shows the Ok band — and separately pushes the warn-and-above tab suffix from  onto the active tab. A poisoned chrome mutex is dropped with a warning rather than propagated, because losing an indicator update must never tear down the reader and with it the pane's terminal output.

A state edge is also what stops the elapsed timer. [[crates/scribe-client/src/main.rs#AiChrome#note_prompt_progress]] stamps `latest_prompt_finished_at` with the edge's arrival instant the moment the state leaves `Processing` — to `IdlePrompt`, `WaitingForInput`, `PermissionPrompt`, or `Error` — and clears it on the way back in, so the strip shows the LLM's response duration instead of wall-clock time since the prompt; [[crates/scribe-client/src/main.rs#AiChrome#record_prompt]] clears it too, and the next turn starts live. The stamp is taken once per run rather than on every non-`Processing` edge, because an idle provider keeps emitting them and each one would push a frozen figure forward. The whole edge — conversation bookkeeping, freeze, tracker update — is one [[crates/scribe-client/src/main.rs#AiChrome#apply_state_change]] rather than a closure inside the reader, so it is exercisable without a live `ReaderCtx`: the freeze shipped broken because only the pure formatter was under test and nothing asserted that anything ever set the field.

### Elapsed formats span sec, minute, and hour bands

Verifies  renders `"X sec"` under a minute, `"Xm YYs"` under an hour, and `"Xh YYm"` beyond, with zero-padded trailing units.

### Elapsed timer tracks now until the AI stops

Verifies  advances with `now` while `latest_prompt_finished_at` is unset.

### Elapsed timer freezes when the AI stops

Verifies  holds at the prompt-to-finish duration once `latest_prompt_finished_at` is set, regardless of how far `now` advances.

### AI state edges freeze and resume the elapsed timer

Verifies the wiring that produces the frozen value, not just the formatter that renders it.

Driving [[crates/scribe-client/src/main.rs#AiChrome#apply_state_change]] with the edges one real turn emits leaves the label live while `Processing`, frozen at the prompt-to-finish figure once the AI stops, unmoved by the further idle edges an idle provider keeps sending, and ticking again on the return to `Processing`. The assertions read the label off [[crates/scribe-client/src/prompt_bar.rs#build_model]] through [[crates/scribe-client/src/main.rs#AiChrome#visible_prompts]] — the pair the render pass itself uses — so a stamp that never reaches the strip fails the test.

### A new conversation retires the bar and the meter

Verifies that a state edge naming a different conversation drops the pane's prompt rows and the previous conversation's context percentage, while the new conversation's own first reading is kept.

The ordering inside [[crates/scribe-client/src/main.rs#AiChrome#apply_state_change]] is the whole point: the retirement runs before the tracker takes the edge, so a switching edge that already carries the new conversation's fill lands that fill rather than losing it. The server half of the same boundary is [[common#Common#AI State#A conversation switch breaks the metadata merge]] — without it the edge would arrive carrying the retired conversation's `context` and the clear would be undone in the same call.

### Elapsed clamps a backwards wall clock

Verifies  clamps to `0 sec` when `now` precedes the prompt timestamp (DST/NTP skew) rather than underflowing.

### No timer without a prompt timestamp

Verifies  returns `None` when no prompt timestamp is recorded, so nothing is drawn.

### Context meter fills and clamps

Verifies  fills the three-segment meter proportionally and clamps above 100%.

### Strip height tracks the prompt count

Verifies  is zero for no prompts or a non-positive cell height, one row for one prompt, and two rows plus a seam for two or more.

### Strip metrics follow the grid font

Verifies [[crates/scribe-client/src/prompt_bar.rs#PromptBarMetrics#resolve]] paints at the grid's own glyph size, row height, and advance width when `terminal.prompt_bar_font_size` is unset, and that an explicit size both wins and scales the row height and advance width by the same ratio.

### Model shows one row for one prompt, two for many

Verifies  emits only the first row for a single prompt and both rows (with the `#N` count) for multiple, and `None` for zero prompts.

### Truncation predicate gates the hover tooltip

Verifies  reports a short prompt as fitting a wide bar and a long prompt as overflowing a narrow one.

### Dismissal hides the strip without losing the history

Verifies [[crates/scribe-client/src/main.rs#AiChrome#dismiss]] takes the pane out of [[crates/scribe-client/src/main.rs#AiChrome#visible_prompts]] — so nothing is painted and no rows are reserved — while the prompt record itself survives until [[crates/scribe-client/src/main.rs#AiChrome#forget]] clears it.

## GPUI Find Overlay

Find-in-scrollback is the client surface that is nothing but a server round trip: the client is display-only, so the scrollback being searched lives on the server and every settled query travels as `SearchRequest` and returns as `SearchResults`.

 is the overlay itself, a port of the winit  with its quad painter replaced by GPUI elements: a top-right box carrying a `Find  n/m` header and a `/query` field. It owns no session and no sink, so an edit — , ,  — clears the stale matches and schedules a `::QueryChanged` rather than searching anything. Clearing before the reply lands is deliberate: the grid must never contradict the query field, even for one frame.

The shell closes the loop. `::OpenFind` reaches  through the shared , so the find chord and the palette's "Find in Scrollback" row open the same surface; a query edit lands in , which lowers it onto  for the attached pane at . The reply is folded in by  on the IPC reader thread, which stores it in  behind the same kind of mutex the chrome and share stores use and bumps the repaint generation.  carries it across into the entity on the next redraw, and  drops any reply whose query the user has already typed past — a pause mid-word puts a second request in flight, and its predecessor's answer would otherwise highlight the wrong thing.

The edit itself is debounced by [[crates/scribe-client/src/search.rs#FIND_QUERY_DEBOUNCE|150 ms]] (spec 017 US8-2). Each keystroke restarts a timer held on the overlay entity, and only the surviving one emits, so a typed word costs one round trip instead of one per character — the server answers each from a full-scrollback snapshot that was tens of megabytes and a `Term`-lock hold per keystroke. The window is short enough to read as instant and long enough that no fluent typist ever sees an intermediate query leave. Dismissing retires the pending timer and, through [[crates/scribe-client/src/main.rs#TerminalView#close_find_overlay]], sends `SearchClosed` so the server drops the snapshot it was reusing; the theme-reload path that rebuilds open overlays goes through the same release rather than dropping the entity on the floor.

While the overlay is up it owns the keyboard:  consumes every keystroke (Escape closes, Enter / Shift+Enter and the arrows cycle, Backspace and Delete edit, printable characters extend), so nothing leaks to the PTY and the find chord cannot reopen the overlay underneath itself. It also notifies the *shell* view rather than only the overlay, because the match highlights are painted by the grid and not by the box.

Highlighting runs through the cell-accurate paint path.  projects the server's absolute grid rows onto the painted viewport, dropping matches in scrollback instead of clamping them onto rows they do not occupy, and  folds the resulting  spans into the per-cell colours the paint resolve step already computes.  reproduces the winit rule: the current match takes the opaque accent with a luminance-chosen contrast foreground, and every other match blends the accent into its own background at 40% so it stays legible over coloured output.

## GPUI Overlays

The GPUI rebuild ports the three interactive overlays — command palette, right-click context menu, and hover tooltip — as `gpui::Entity` views with rounded corners, drop shadows, and hover/pressed states, replacing the winit quad painters.

 folds the winit palette state and the `main.rs` entry machinery into one entity. The pure assembly stays testable:  holds the fixed action rows (including the feature-013 client-local "Connect to remote machine…" row),  builds the "Switch Profile" rows tagging the active one,  appends the conditional update row, and  applies the case-insensitive substring filter. Typing and  paste (control characters stripped) drive the filter; the wrapping selection and  emit a  via  for the shell to route (the winit `execute_automation_action` seam).

 ports the right-click menu.  assembles the ordered rows verbatim: the Copy/Paste/Select-All head (Copy gated on a selection), the OSC 8 "Open URL" precedence and appended "Copy hyperlink address" entry (spec 009 FR-003 / FR-007), the file row, and the smart-selection actions resolved through . Clicking an enabled row runs  (emitting a  on ); Escape or a backdrop click runs .

 draws the hover tooltip, sizing and positioning it from the pure geometry ports:  centres the box on the anchor and clamps it inside the viewport,  picks above/below, and  head+tail-elides a long URI (spec 009 FR-006). The spike wires all three into  — Ctrl+Shift+P opens the palette, a right-click opens the menu, Ctrl+Shift+U toggles the tooltip demo — so the visual E2E harness (`tests/e2e/visual/overlays.sh`) can screenshot each overlay and its interaction checklist.

#### Overlay Chords Yield To Bindings

Surfaces with no `KeybindingsConfig` field of their own are opened from a fixed chord the shell hard-codes, and a hard-coded chord must never shadow a configured action.

 is that table — the tooltip demo, the close dialog, the clipboard dialog, and vi mode — and  resolves a keystroke against it *after*  has had first refusal, returning `None` whenever a binding claims the key. The precedence is load-bearing because  runs ahead of : a chord claimed there never reaches the binding dispatcher at all. During the rebuild the close dialog sat on `ctrl+shift+q`, the Linux default for `close_tab`, so the action was unreachable without a rebind. The dialog moved to `ctrl+shift+d`, and the precedence rule keeps any future collision — including one a user creates by rebinding onto an overlay chord — resolved in the user's favour.

### Overlay Action Routing

Both overlays emit their choice for the shell to run; the shell routes those events into the same dispatchers the keyboard uses, so a palette row and its bound chord always do the same thing.

 is the confirm seam. The client-local remote-connect row is still unported; every other row carries a shared  into . Most of those lower onto a  through  and are handed to  — literally the call the key path makes — so wiring a surface for one wires it for both. The three actions with no bindable chord are handled directly:  activates a stored profile and applies the config it returned through the normal reload path (rather than racing the file watcher against that write),  moves the tab selection onto a session, and the update dialog waits on the update surfaces. The lowering match is exhaustive, so a new automation action fails to compile instead of quietly becoming unroutable.

 ports the winit menu routing for the open/run group: heuristic URLs keep the silent scheme-allowlist drop of , an OSC 8 URI goes through  so an allowlisted scheme opens immediately and any other scheme first raises the disallowed-scheme confirmation (spec 009 FR-015), file rows open with the OS handler, and the smart-selection rows type into the attached pane, spawn a detached command, or open a login shell in a new tab. The pending URI is parked on the view while that modal is up so  can activate it verbatim on "Open Anyway".

The settings row's destination is another top-level window: it lowers onto `KeyAction::OpenSettings` and reaches  (). Rows whose destination surface is not ported yet — the remote picker, the update dialog, and the clipboard/selection trio — are still *routed*: they reach a dispatcher and are named and counted by  instead of being discarded at the subscription. That warning is the difference between "not built yet" and "silently dead".

### Palette base entries and update row

Verifies  leads with "Open Settings" and ends with the remote-connect row, and that  appends the "Update Scribe to v{version}" row only when an update is available.

### Palette profile rows tag the active profile

Verifies  emits one "Switch Profile: {name}" row per profile and suffixes " (active)" onto the currently active profile, wiring each row to a `SwitchProfile` action.

### Palette query filters case-insensitively

Verifies  keeps every entry for a blank/whitespace query and otherwise retains only rows whose label contains the trimmed, lowercased needle.

### Palette typing and paste drive the filter

Verifies typing characters and  paste both extend the query (paste dropping control characters so a multi-line payload collapses) and narrow the filtered list.

### Palette selection wraps and confirms an action

Verifies the palette selection wraps with `next_item`/`prev_item`,  emits the highlighted row's , and confirming an empty filter is a no-op.

### Context menu head reflects selection state

Verifies  always leads with Copy / Paste / Select All and enables Copy only when a selection exists.

### Context menu OSC 8 precedence and copy entry

Verifies an OSC 8 URI takes "Open URL" precedence over a heuristic URL (via `OpenOsc8Url`) and appends a "Copy hyperlink address" row, while a heuristic-only right-click keeps the plain `OpenUrl` row and no copy entry (spec 009 FR-003 / FR-007).

### Context menu appends smart-selection actions

Verifies  drops actions with an empty expanded parameter and that surviving smart actions append after the file entry.

### Context menu click dispatches or dismisses

Verifies  emits an enabled row's action, that a disabled row is a no-op, and that  emits `Dismissed`.

### Tooltip centres on its anchor

Verifies  centres a box that fits horizontally on the middle of its anchor rect.

### Tooltip clamps to the viewport edges

Verifies  pins a box against the right edge when the anchor is near it and to `x=0` at the left edge, so an edge-anchored tooltip slides inward instead of clipping.

### Tooltip picks above or below the anchor

Verifies  returns the anchor-top-minus-height for `Above` and the anchor-bottom for `Below`.

### Tooltip truncates a long URL head and tail

Verifies  returns short URIs unchanged, head+tail-elides an overflowing URI to exactly the budget with a middle `...`, falls back to a plain head cut at tiny budgets, and never splits a multibyte codepoint.

## GPUI Dialogs

The GPUI rebuild ports the winit client's five GPU-painted modals into display-independent state models plus one generic  entity, replacing the winit `CellInstance` quad painters and pixel hit-testing with GPUI flex layout and `on_click` listeners.

Each modal is one variant of  —  (quit / kill / cancel, warning about active sessions),  (install-available live-reload plus the restart-required helper cold-restart, built by  / ),  (the spec-011 risky-paste gate),  (the OSC 52 four-button policy prompt), and  (the spec-009 OSC 8 out-of-allowlist prompt). Every model lowers to a  (title, body lines, tone-tagged buttons, focused index) so parity is asserted without a live window, and keeps the winit **safe default focus** — Cancel / Later / Deny once — so  on an unexpected prompt never performs the risky action.

 renders any spec onto a dimmed backdrop and a rounded, drop-shadowed box with a centred title, body, separator rule, and a button row whose accent / warm-red-destructive / subtle tones come from  resolved against a theme-derived .  /  cycle focus,  activates the focused button,  activates a clicked button, and  (Esc / backdrop) resolves to the safe  action — each emitting a tagged  on  for the shell to route. The paste gate reuses  and renders the parked  in caret notation so a control sequence in the preview can never drive the terminal (FR-005), returning it verbatim via  for byte-identical delivery.

While a modal exists, [[crates/scribe-client/src/main.rs#TerminalView#ensure_focus]] moves GPUI keyboard focus onto its [[crates/scribe-client/src/dialog.rs#DialogView]] handle. Root dispatch applies the compositor guard, then [[crates/scribe-client/src/main.rs#TerminalView#handle_overlay_key]], before plain Tab may enter [[crates/scribe-client/src/main.rs#TerminalView#focus_next_titlebar_control]]. Tab and Shift+Tab therefore cycle only modal buttons; ordinary titlebar traversal resumes once the modal closes.

The spike wires two representative modals into  — Ctrl+Shift+Q opens the close dialog and Ctrl+Shift+K opens the clipboard dialog — so the visual E2E harness (`tests/e2e/visual/dialogs.sh`) can screenshot the modal chrome, the focus ring, and the tone-tagged buttons across the three- and four-button layouts.

The update confirmation is the first of the five that is a live surface rather than a demo:  routes the resolved  before dropping the overlay, so `DialogOutcome::Update` reaches  and turns into IPC (see ). The other four still only close.

### Close dialog buttons and safe default

Verifies  renders Quit Scribe / Kill Window / Cancel with accent / danger / normal tones, focuses the safe Cancel by default, and shows the active-session-loss warning only when sessions are open.

### Close dialog focus cycling maps to actions

Verifies  /  wrap across the three close buttons, that  maps each index to its `CloseAction`, and that dismissal always resolves to Cancel.

### Update dialog install and restart flows

Verifies the install-available dialog titles "Update Available", defaults focus to the accent Update Now, and keeps live-reload copy, while the restart-required helper cold-restart titles "Restart Required" and offers Continue / Cancel.

### Paste gate reason line distinguishes risk

Verifies  derives the reason line for the multiline-only, control-only, and combined cases, defaults focus to Cancel, and offers Cancel / Paste.

### Paste preview is caret-escaped

Verifies the paste dialog body never contains a raw control byte (ESC renders as `^[`) and that  returns the parked text verbatim for byte-identical delivery.

### Clipboard dialog four-button policy

Verifies  renders Deny once / Always deny / Allow once / Always allow with the two Allow variants tinted destructive, focuses the safe Deny once, maps each index to its policy action, and shows the write payload preview.

### Disallowed scheme dialog truncation

Verifies  names the blocked scheme, head-and-tail-truncates a long URI while keeping both ends visible, defaults focus to Cancel, and preserves the full URI verbatim via .

### Dialog view confirms the focused button

Verifies  followed by  emits the newly focused button's  on .

### Dialog view click and dismissal resolve

Verifies  emits the clicked button's outcome regardless of focus, and that  emits the safe cancel outcome for an Esc or backdrop click.

## App State

The master application state lives in the App struct in . It holds all panes, the window layout, IPC sender, input bindings, theme, AI tracker, GPU context, and UI overlay state. The event loop is driven by winit's `ApplicationHandler` trait.

### Render Loop

Each frame collects `CellInstance` arrays from visible panes and UI chrome, uploads them to the GPU instance buffer, and executes a single render pass.

Content dirty tracking avoids rebuilding instances when nothing has changed. A splash screen renders via a separate pipeline during startup.

## Panes

Each terminal session is represented by a  that owns an alacritty_terminal `Term`, VTE processor, grid dimensions, scrollbar state, and cached render instances.

### PTY Output Coalescing

`PtyOutput` IPC messages are buffered per session and drained once in `about_to_wait` by .

Deferring PTY handling until after all input events are processed ensures keystrokes are never blocked behind a queue of output messages. Once drained,  still preserves pane-local synchronized-update frame boundaries before the bytes reach the terminal state, so Codex and other TUIs keep their committed redraw cadence even when multiple IPC chunks were coalesced per session. A `ScreenSnapshot` discards both the session-level byte buffer and any pane-local queued frames for that session since the snapshot replaces VTE state entirely.

### Content Dirty Tracking

The `content_dirty` flag is set on PTY output or resize and cleared after instance rebuild.

Bytes buffered inside a VTE synchronized update (`CSI ? 2026 h/l`) do not mark the pane dirty until the update terminates or its timeout flushes the buffered output.  uses the streaming  so synchronized-update commits stay distinct even when the terminator is split across PTY IPC messages.  then replays one committed burst per redraw while the pane is caught up, but it drains through older queued bursts once backlog crosses the catch-up threshold so stale frames do not pile up indefinitely.  still commits expired sync blocks and marks the pane dirty when an application never sends the closing `CSI ? 2026 l`.

Visible output in the focused pane clears the active selection unless the user is actively dragging, while the shared post-output path still invalidates URL caches and shifts saved selections when scrollback grows.

The cache stores the last-built instances along with cursor blink visibility, terminal cursor hidden state (DECTCEM), focus state, selection range, and display offset. If all match, the cached instances are reused without GPU upload. Tracking the viewport offset prevents scrollback changes from reusing stale cells, while DECTCEM tracking invalidates when a program toggles cursor visibility via `CSI ? 25 h/l` without other content changes.

### Synchronized Updates

Normal live sessions receive the raw synchronized-update markers from the server, and the client decides redraw pacing from pane-local committed-frame queues instead of from raw PTY delivery order alone.

 hands incoming PTY bytes to , which preserves raw `CSI ? 2026 h/l` frame boundaries across message splits before enqueuing the resulting raw frames on the pane.  still lets light traffic present one committed burst per frame, while  switches winit to `ControlFlow::Poll` whenever queued output remains so redraws cannot stall behind a long user-event burst. The pane-local VTE processor still handles the actual synchronized-update buffering, and  mirrors VTE's 150 ms timeout for raw frames buffered ahead of the pane-local processor. Each raw timeout starts when its own block opens, and its BSU-stripped bytes join the queued frames in FIFO order so a timeout cannot overtake an earlier commit or re-enter sync mode.

The client does not reflow blank viewport rows after render because that heuristic could move the live prompt away from the pane bottom.

### Replay Restore

Reattach delivers each session's state as a  — the same zstd-compressed ANSI primitive the server uses for hot-reload handoff.

 decompresses the bytes and  feeds them through the pane's VTE processor, rebuilding the Term durably. The same helper also backs `handle_screen_snapshot` (used by `RequestSnapshot` tooling), so there is one ANSI-feed path regardless of whether the source is a live attach or a per-cell snapshot.

Most panes send their dimensions in `AttachSessions` so the server resizes each session's Term and PTY before building the replay. This eliminates the post-attach resize that would trigger SIGWINCH and corrupt restored content via shell redraw sequences.

 treats Codex sessions as an exception and sends `0x0` dimensions on reconnect. A pre-replay SIGWINCH can make Codex redraw top-anchored before the replay is captured, so preserving the existing viewport restores the prompt at the bottom as expected.

Reconnect restores each pane from its actual pane-tree rect, edge padding, and final workspace tab count before `AttachSessions` is sent. That lets split panes report their real grids up front instead of restoring at full-workspace size and correcting them with a second reconnect-wide resize pass.

Codex panes still keep `last_sent_grid = None` during reconnect, but they only queue a post-restore `Resize` when the incoming replay dimensions differ from the restored pane grid. The same mismatch safeguard covers hot-restart handoff reattach: if the replay dimensions prove the live PTY was not resized yet, the client clears `last_sent_grid`, feeds the replay ANSI at its captured size, restores the local term to `pane.grid`, and lets the normal resize debounce send one corrective `Resize` later. When the replay dimensions differ from the pane grid, Codex panes additionally clear the visible area after the resize to remove content garbled by column reflow — Codex's Ink renderer uses differential updates that may not fully overwrite the stale TUI layout. Scrollback from the replay is preserved. The ANSI encoder preserves soft-wrapped rows by carrying `WRAPLINE` through  and avoiding an extra `CRLF` between rows that already wrap into the next line. `sync_pane_grids_if_stale` enforces that `pane.term` dimensions match `pane.grid` before every render frame as a safety net.

### Padding

There is no content padding. A pane paints its grid edge to edge, and the only
insets the sizing path reserves are ones the paint path actually applies.

The legacy client had an `appearance.content_padding` setting and computed it
per pane from edge adjacency. The GPUI client never inset anything by it, so
the knob only ever showed up as a grid the client reported wider than it
rendered — the setting and its geometry helpers were removed rather than left
as a control that silently did nothing. See
[[client#GPUI Client Spike#Pane Grid Sizing]].

## Layout

The layout system has two levels: the window layout splits into workspaces, and each workspace holds tabs that each contain a pane tree.

### Pane Tree

A binary split tree defined in  where each node is either a `Leaf(PaneId)` or a `Split` with direction, ratio (clamped 0.1-0.9), and two children. Pane IDs are allocated from a global atomic counter.

Splitting a pane automatically equalizes all ratios in the tree so every pane gets equal space.

### Focus Navigation

Directional focus (`FocusLeft`, `FocusRight`, `FocusUp`, `FocusDown`) uses spatial overlap scoring to find the best neighbor.

A pointer press also moves focus: [[crates/scribe-client/src/main.rs#TerminalView#press_focuses_pane]] hit-tests the press against the frame's pane placements and, when it lands in an unfocused pane, routes through the tab-switch path (strip selection, attach, focus report) and consumes the gesture — so clicking into any visible terminal focuses it directly, no tab click required. It runs after the divider hit test so a boundary press stays a resize.

For each candidate pane in the target direction, the overlap between the source pane's perpendicular axis range and the candidate's range is computed. The closest candidate with the best overlap wins.

If no direct pane or workspace neighbor exists in that direction, focus wraps to the opposite edge while keeping the same perpendicular-axis overlap rule. When nothing overlaps on that axis, focus stays put.

### Workspace Layout

Defined in , the window-level tree splits the viewport into workspace regions. Each `WorkspaceSlot` holds a workspace ID, tab list, active tab index, accent color, name, and project root path.

Splitting a workspace automatically equalizes all workspace ratios so every region gets equal space, and removing a workspace re-equalizes the survivors — the same rule pane close applies. Both route through [[crates/scribe-client/src/workspace_layout.rs#WindowLayout#equalize_all_workspace_ratios]], which weights each split by the leaf count on either side. Restoration (`split_workspace_with_id`, `from_tree`) preserves reported ratios and never equalizes.

On reconnect, a reported workspace tree is authoritative for workspace topology. Only the legacy no-tree fallback applies `WorkspaceInfo.split_direction` patches, and each workspace is patched once during startup so later tab or session updates cannot rearrange the live split tree.

`handle_session_list` must apply per-workspace `WorkspaceListEntry` metadata (name, accent color, project root) *after* `reconstruct_workspaces_for_sessions` runs — the reconstruction path does `self.window_layout = WindowLayout::from_tree(tree)` and replaces the layout wholesale, so any names applied before reconstruction are silently dropped. Post-handoff the preserved shells emit no fresh OSC 7, so SessionList is the only source of workspace names at reconnect.

The per-workspace `active_tab_index` reported in each tree leaf is also applied *after* `restore_reconnect_tabs` populates tabs, not as part of `from_tree`. Each `add_tab_with_pane_tree` call inside the restore loop auto-sets `active_tab` to the last-pushed tab (correct UX for user-initiated tab creation, wrong for restore), so the post-pass calls  per leaf to restore the originally focused tab.

### Tab State

Each tab in a workspace owns a `LayoutTree` for its panes, a focused pane ID, and an optional text selection. Tabs are created, removed, and reordered within their workspace slot.

The workspace's `active_tab` index lives on  and rides the  `Leaf` variant as `active_tab_index`. Client reports it via `ReportWorkspaceTree` on every tab switch () alongside the existing report-on-split/close triggers, so the server's per-window tree (used for handoff and reconnect) always reflects the latest focused tab.

## Tab Bar

GPU-rendered tab bar in  generating  from  using the same glyph atlas as the terminal grid.

 is derived from `ChromeColors` and holds background, active background, text, separator, gradient-top, and accent color values.  carries per-tab title, active flag, optional AI indicator color, and an optional transient `tab_flash` intensity (a short theme-accent blend over the tab background that self-decays over `TAB_FLASH_SECS` ≈0.45s via the same animation/redraw envelope as the scrollbar fade, additive over active/hover styling and the AI indicator). The background is rendered as a two-tone vertical gradient (lighter top half, base bottom half) via `build_tab_bar_bg`. The active tab receives a uniform highlight color and a 2px accent indicator on its bottom edge. An AI state dot (from `TabData.ai_indicator`) is rendered in the tab when a session has an active AI state. For provider task-label sessions, the title prefers the last hook-emitted task label while that label is active, then falls back to the normal shell title. Tab titles are truncated to fit the available column width. In multi-workspace mode, a nonempty server/chrome metadata name is the sole badge gate: named workspaces display a badge using the configured palette, while missing or empty names display only terminal tabs. `TabData::group_region_x` independently marks each group's first tab, preserving adjacent unnamed group boundaries and absolute alignment with the regions below; `TabData::group_accent` still styles region hairlines and active-tab underlines. The pill quad and per-cell backgrounds both span `space + name + space + trailing-gap` so the accent fills the full `badge_columns` allocation up to the next tab boundary — the gap cells are emitted with `pill_bg` rather than the chrome bg, otherwise the cell-bg pass would punch through the underlying quad and leave a visible strip between the badge and the first tab.

Tab rows wrap only after subtracting the same rendered badge and right-edge icon reservations used by the text pass.  and active-tab range calculation share that reservation so a narrow workspace cannot allocate a blank extra row while the tabs still fit on one row.

Because tab chrome and tab glyphs are collected into the same `CellInstance` buffer and drawn in one render pass,  must append the tab-bar background before the tab text so the labels are composited on top of their tabs.

When context-window usage reaches the warn threshold (default 70%), a colored `" NN%"` suffix is appended to the tab label.  returns the suffix text and its `srgb_to_linear_rgba` color, or `None` when the threshold is not met or the session is in a pulsing attention state (`PermissionPrompt`, `WaitingForInput`). A `fallback_color` parameter (passed as `tab_text` color by the caller) is used when the hex color string fails to parse, matching the other context displays' invalid-hex fallback behavior. `TabData.context_suffix` carries the result; `tab_display_title` reserves the suffix columns before truncation; `render_tab` emits the suffix chars in the suffix color after the title.

### tab_context_suffix_below_warn_returns_none

Verifies that  returns `None` when context=50 is below the default warn threshold of 70.

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

## Input

Keybindings are parsed from config into a `Bindings` struct in  with over 50 configurable actions.

### Focus Guard

Two layers prevent stray key events from compositor overlays (e.g. GNOME Screenshot) from reaching the PTY.

Both layers also drive : window-focus transitions flow through `notify_focus_change` (a single chokepoint reused by every pane / session focus path) which calls `refresh_ime_allowed`, and the per-tick X11 guard poll re-evaluates the gate so a compositor overlay's reactivation debounce also blocks IME until the window is truly active again.

#### Winit Focus

Keyboard events are only processed when the window has focus (`window_focused == true`). This catches overlays that trigger X11 `FocusOut` events.

#### X11 Active-Window Guard

 polls `_NET_ACTIVE_WINDOW` via a separate `x11rb` connection to detect compositor overlays that skip X11 focus events.

Compositor overlays (e.g. GNOME Shell screenshot) clear or change this EWMH property without sending `FocusOut`. The guard polls in `about_to_wait` and on each key press. A `was_inactive` flag tracks whether the window has been obscured; when `should_suppress_key` or `poll` first sees the window become active again, a `reactivated_at` timestamp is set and keys are suppressed for 300ms from that transition. The debounce is cleared on `Focused(true)` so it only applies to compositor overlay dismissals — not normal focus transitions — preventing the first keystroke from being swallowed when the user alt-tabs or clicks to Scribe.

Off Linux the guard carries an uninhabited field, so [[crates/scribe-client/src/x11_focus.rs#X11FocusGuard]] cannot be constructed at all rather than merely being empty — `from_window_handle` already returns `None` there. That makes the platform-neutral `poll`, `clear_reactivation_debounce`, and `should_suppress_key` discharge `self` by matching a value that cannot exist, instead of silently ignoring it.

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

On macOS, GPUI application actions reserve bare `cmd+w` and `cmd+q` before that chain and expose them through the native File and Scribe menus. `cmd+w` follows the active window's normal close path, while `cmd+q` defers its terminal-window update until action dispatch returns the active window to GPUI's table, then raises the server-owned close dialog before any process exits; neither chord can fall through to pane bindings or terminal input.

Above the level-4 encoder,  short-circuits the dispatch at the entry of `handle_keyboard` whenever an OS IME composition is in flight, so synthesized winit key events that the IME is mid-composing never reach the legacy or Kitty encoder. See  for the state machine that drives that predicate.

1. Layout shortcuts (configurable keybindings) produce `LayoutAction` enum values
2. Special commands (command palette, settings, find, hover-preview inline editor)
3. Terminal shortcuts (word navigation, line navigation)
4. Generic terminal key translation produces PTY bytes — legacy xterm modifier encoding, or full Kitty CSI-u when the focused application negotiated the keyboard protocol

Pane-local terminals enable kitty keyboard tracking so an application's negotiated progressive-enhancement flags shape encoding.  turns tracking on;  bundles the five negotiated flags from the focused pane's `Term` mode together with the two DEC private modes (`APP_CURSOR` for DECCKM, `APP_KEYPAD` for DECPAM) into  — Kitty flags are forced all-off when the `[terminal]` `keyboard_protocol_enhanced` opt-out is disabled, but the DEC modes always reflect the pane so terminfo `smkx` / `rmkx` keeps working. The level-4 encoder emits CSI-u for Kitty functional keys whose protocol entries are true codepoints, but arrows, Insert/Delete, Page keys, Home/End, and F1-F12 stay on their Kitty legacy-shaped CSI letter/tilde forms; repeat/release markers are carried in the modifier parameter's event-type subfield. With no Kitty flag negotiated the legacy byte encoding is reproduced byte-identically. Codex panes still map Alt+Enter to Codex's newline binding before the generic path.

#### DEC Application Modes

The level-4 encoder consults DECCKM and DECPAM so unmodified arrows and numpad keys switch escape forms in app-cursor / app-keypad mode.

Apps such as `less`, `vim`, `top`, and `htop` enable DECCKM via terminfo's `smkx` capability when they start. If the encoder kept emitting CSI form (`\x1b[A`) instead of SS3 form (`\x1bOA`), those pagers silently swallow cursor keys because their terminfo `kcuu1` describes the SS3 byte sequence.

| Key                       | DECCKM off (default) | DECCKM on    |
|---------------------------|----------------------|--------------|
| Bare ArrowUp/Down/Left/Right | `\x1b[A..D`         | `\x1bOA..D`  |
| Bare Home / End           | `\x1b[H`, `\x1b[F`   | `\x1bOH`, `\x1bOF` |
| Modified arrow / Home / End | `\x1b[1;<mod><L>` (same regardless of DECCKM) | same |

 picks the SS3 form when `app_cursor` is set and no modifier is held; otherwise it emits the modifier-aware CSI form. Modified chords always use CSI because SS3 has no slot for the xterm modifier parameter — this matches alacritty's default bindings and xterm's `modifyCursorKeys=2`.

Numeric-keypad keys (`KeyLocation::Numpad`) emit SS3 sequences when DECPAM is active and no modifier is held: digits `0..9` map to `\x1bOp..\x1bOy`, `.,-+*/=` map to `\x1bOn`/`\x1bOl`/`\x1bOm`/`\x1bOk`/`\x1bOj`/`\x1bOo`/`\x1bOX`, and numpad Enter maps to `\x1bOM`.  runs ahead of the legacy / Kitty dispatch so the numpad table wins over the generic encoder for those events.

### Terminal focus keeps Tab for the PTY

Verifies [[crates/scribe-client/src/main.rs#TerminalView#traversal_claims_tab]] never claims plain Tab or Shift+Tab while the terminal root owns focus, and that [[crates/scribe-client/src/main.rs#encode_key]] emits `\t` and `ESC [ Z` so tab completion reaches the PTY.

### Chrome focus keeps Tab traversal

Verifies traversal claims plain Tab and Shift+Tab once chrome owns focus, while modified Tab chords and Ctrl+I — a different keystroke that encodes to the same `\t` byte — are never claimed, keeping titlebar tab stops keyboard-reachable.

### GPUI Input Encoder Port

The GPUI rebuild reproduces the level-4 terminal byte encoder in , byte-identical to the winit client's  across legacy xterm, Kitty CSI-u, DECCKM, and DECPAM output.

Because GPUI's `Keystroke` drops numeric-keypad location and a distinct unshifted base vs shifted glyph, the encoder consumes an intermediate  carrying the key token, base character, associated text, modifiers, , and press/repeat/release state.  lowers a GPUI `KeyDownEvent` into that shape — numpad location is unavailable on that path, so callers with richer platform data set it directly. Negotiated Kitty flags travel through  and the two DEC modes through , mirroring the winit encoder's .

The keybinding dispatch above this encoder is wired into the shell (see ), and the level-4 byte encoder is now the binary's own key path:  lowers the GPUI event and calls  directly. The port is verified against the committed oracle (see ) by a golden byte-capture test that replays every case in `tests/fixtures/gpui-client/keyboard-byte-golden.json`.

The one piece still missing is the per-pane mode: the binary always passes `::legacy` because it tracks no negotiated Kitty flags and no DECCKM/DECPAM state yet, so the Kitty and application-mode branches of the encoder stay unreachable from the running client even though the golden test covers them. Wiring the focused pane's mode (the winit client's ) is the remaining step to full parity.

### GPUI Keybindings Port

The GPUI rebuild ports the keybinding parser and layout-action dispatch from the winit client, retargeted at GPUI's `Keystroke`/`Modifiers` via the intermediate , so no configured shortcut regresses at cutover.

GPUI's backends do not spell shifted symbols uniformly. Linux resolves the keysym at the active modifier level and may drop the shift flag for single-character non-letter keys, so `ctrl+shift+\` arrives as control plus the key `|` with shift clear; macOS can keep the shift flag while still reporting the shifted glyph (`}` for `cmd+shift+]`). Every shifted-symbol default (`split_vertical`, `split_horizontal`, `zoom_in`, `next_tab`, `prev_tab`) was therefore unreachable on at least one live backend until  accepted a binding's US-layout shifted glyph () in both forms. Letters are absent from that table on purpose: the backends already report them by their own lowercase key and keep the shift flag.

 parses every configurable action from  (invalid combos skipped with a warning).  reads the same combo vocabulary as , mapping `cmd`/`super` onto GPUI's platform modifier, and  requires an exact modifier match (ignoring the GPUI function flag) on a key-down event, comparing characters against the unshifted base case-insensitively.  runs the legacy three-level intercept order — layout shortcuts, then command-palette/settings/find, then the seven fixed terminal-shortcut escape sequences — returning a ; the generic byte encoder handles level 4 when it returns `None`. All 50+  variants are enumerated one-for-one against the legacy tables. The module also owns the shell's fixed overlay chords, so that the "a configured binding always wins" rule lives next to the table it outranks — see .

### Layout Actions

Over 50 variants in the `LayoutAction` enum covering pane, workspace, and tab management, clipboard, scrolling, zoom, and more.

Tab actions: new, Claude Code new/resume, Codex new/resume, close, next, prev, select 1-9. The legacy `new_claude_*` action names remain in config and code and map to Claude Code, while `new_codex_*` opens Codex. Those AI-tab shortcuts send provider/resume intent only through `CreateSession.ai_launch`; `command` remains empty. The server builds the same argv a plain tab gets and appends a shell-specific interactive `-c` command that execs the provider, so an AI tab's shell, integration, and startup files are identical to every other tab's and the tab ends when the provider does. An AI tab always starts at the focused region's project root when the server reports one, falling back to the focused pane's CWD otherwise: an AI session is scoped to a project rather than to wherever a shell has wandered, so a `cd` deep into a subtree must not move where the assistant is rooted. Also: pane splits, pane focus/cycling, workspace splits/cycling, copy, paste, settings, find, zoom, prompt-jump up/down, and jump-to-failure (scroll to the most recent failed command; when no failed command exists, `signal_no_failed_command` instead fires a non-disruptive scrollbar pulse plus a brief focused-pane tab flash and leaves the viewport unchanged).

Fresh AI creates resolve that directory client-side from the focused `WorkspaceSlot.project_root` and then the focused session `ChromeMetadata.cwd`, and send it beside structured intent in `CreateSession.cwd`. The server reports a project root only while the region's CWD is under a configured `workspaces.roots` entry and clears it on the way out, so no separate in-a-workspace test is needed. Missing focus or OSC 7 metadata and server-forwarded automation without visible focus send `None`, leaving the server's directory check and home fallback authoritative. The resolved value is also captured in the launch binding for later persistence; cold-restart replay bypasses fresh resolution and sends the persisted `LaunchRecord.cwd` unchanged.

A plain new tab and a pane split instead always capture the focused pane's last server-reported CWD, so the new shell continues where the current one is; the split captures it before focus moves to the new pending pane. Both send and persist that directory, and neither consults the project root, which anchors AI tabs only; missing CWD metadata still leaves the server's home fallback authoritative. A workspace split is the deliberate exception: a new region is a fresh context rather than a continuation, so it sends no CWD and always opens at the server's home directory. Smart-selection commands keep their existing semantics: a command opened in a new tab sends no CWD, while a detached coprocess inherits the client process CWD.

### Command Palette

The command palette is a GPU-rendered action picker for common window actions, profile switching, and explicit Claude Code and Codex tab actions, opened from a dedicated keybinding and reusing the normal layout-action handlers.

 owns the query string, active state, and selected row.  populates entries for settings, find, tab and pane actions, new windows, every saved profile from , and (when available) an "Update Scribe to v{version}" entry. Selecting an entry routes through , so command-palette actions and server-forwarded automation stay on the same code path.

The query field accepts clipboard paste (`Ctrl+V` / `Cmd+V`), reading the host clipboard through  and inserting via  (control characters stripped). The  manual `host:port` field shares that read path via `RemoteConnectAction::PasteManual` → .

### Mouse Handling

Mouse events are processed for text selection, scrollbar interaction, divider drag, tab drag, prompt bar interactions, and context menus.

Selection modes are click-drag for cell, double-click for word or configured Smart Selection, triple-click for line, and quad-click for Smart Selection when configured that way. Scrollbar supports click-to-jump and drag-to-scroll. Divider drag resizes splits with 4px hit tolerance. Tab drag reorders with visual offset.

Click sequencing is tracked by , which records each press time and position to classify the event as  (Single, Double, Triple, or Quadruple). Multi-click is recognized when a press arrives within 400 ms and 5 px of the previous one. The derived  (Cell, Word, or Line) follows directly from the click kind. Auto-scrolling during drag is triggered by `edge_scroll_delta` when the cursor enters the 20 px edge zone at the top or bottom of the content area.

OSC 133 `click_events=1` prompt click-to-move is evaluated on mouse release through , only when the press/release left an empty selection. Dragging the live prompt row therefore keeps normal text selection, while a plain click can still send arrow-key movement.

### Drag And Drop

Dropped files and directories are pasted into the focused shell using shell-aware quoting, so GUI drag-and-drop becomes a safe path insertion workflow instead of raw bytes.

 receives `WindowEvent::DroppedFile`, looks up the focused pane's shell basename, quotes the path for POSIX shells, Fish, PowerShell, or Nushell, and sends it through the normal paste pipeline with a trailing space. Shell basenames come from reconnect metadata and `SessionCreated`, so the quoting mode follows the actual session instead of assuming the user's login shell.

### Mouse Reporting

When a terminal application enables mouse mode, button, motion, and scroll events are encoded as xterm escape sequences and forwarded to the PTY.

#### The Wheel Follows The Pointer

[[crates/scribe-client/src/main.rs#TerminalView#attach_wheel]] hangs the wheel off each pane element rather than off the shared grid band, so GPUI's own hit test decides which terminal a scroll belongs to.

The wheel is a pointer gesture: hovering a pane scrolls it whether or not it holds focus, and whether or not the window is the active one — the behaviour every other tiling application has, and the reason a wheel must never move the focus to make itself work. [[crates/scribe-client/src/main.rs#TerminalView#scroll_pane]] therefore takes the hovered session rather than reading `active_session`, and all three wheel consumers are addressed by it: [[crates/scribe-client/src/main.rs#TerminalView#session_mouse_modes]] reads that pane's DEC modes, [[crates/scribe-client/src/main.rs#TerminalView#send_pty_bytes_to]] addresses its PTY, and [[crates/scribe-client/src/main.rs#TerminalView#scroll_session]] moves its viewport and pulses its scrollbar. The focused-pane wrappers ([[crates/scribe-client/src/main.rs#TerminalView#send_pty_bytes]], [[crates/scribe-client/src/main.rs#TerminalView#scroll_terminal]]) remain for the scroll chords, which are keyboard gestures and do belong to the focused pane.

A pane still waiting on `SessionCreated` gets no listener at all — there is nothing to scroll yet.

#### Per-Pane Painted Bounds

Every pane records where it painted its grid into its own `GridBounds` sink, keyed by session in `TerminalView::pane_bounds`, so a hit test can never disagree with what paint drew.

The sinks are minted in [[crates/scribe-client/src/main.rs#TerminalView#prepare_pane_surfaces]] beside the scrollbar state and retired with it in [[crates/scribe-client/src/main.rs#TerminalView#retire_scrollbars]], because a closed pane's last rect must not keep answering for a region some other pane now occupies. [[crates/scribe-client/src/main.rs#TerminalView#pane_at]] resolves the pointer to a session against them; [[crates/scribe-client/src/main.rs#TerminalView#focused_grid_bounds]] is the focused pane's entry.

That split is what divides the pointer surface in two. Gestures a *click* precedes — selection, Ctrl+click links, smart selection, the split-scroll chip — stay focus-relative, because [[crates/scribe-client/src/main.rs#TerminalView#press_focuses_pane]] has already made the pane under the pointer the focused one by the time they run. Gestures with no click in front of them resolve by position: the wheel, and the overlay scrollbar.

#### The Scrollbar Is A Scroll Gesture

[[crates/scribe-client/src/main.rs#TerminalView#press_scrollbar]] resolves its pane from the pointer and is ordered *ahead* of [[crates/scribe-client/src/main.rs#TerminalView#press_focuses_pane]] in [[crates/scribe-client/src/main.rs#TerminalView#press_grid]].

Dragging a thumb is scrolling, so it obeys the same rule the wheel does: a pane you can see is a pane you can scroll, without a focus-stealing click first. Behind the focusing click it would have cost two clicks on every unfocused pane, and the priority comment on `press_grid` had already claimed the scrollbar went first — the focusing click was inserted ahead of it without that being revisited. [[crates/scribe-client/src/main.rs#TerminalView#update_scrollbar_hover]] sweeps every pane rather than the focused one for the same reason, which is also what clears the hover on the pane the pointer just left.

[[crates/scribe-client/src/main.rs#TerminalView#scrollbar_layout]] takes the named pane's own rect. It had always accepted a `session_id` and then measured the focused pane, which was invisible while every caller passed the focused session and wrong the moment one did not.

#### GPUI Rebuild Golden Oracle

The future GPUI client ports input byte-for-byte from committed old-client captures before this implementation is deleted.

`tests/fixtures/gpui-client/keyboard-byte-golden.json` captures legacy xterm, Kitty CSI-u, DECCKM, and DECPAM bytes. `mouse-byte-golden.json` captures X10, SGR-1006, and the 1000/1002/1003 motion gate. The root test-fixture location survives old-client deletion and is copied into the new crate when that scaffold exists; porting beads load the captures rather than recreate expected strings by hand.

The GPUI reporter lives in  and its siblings (, , ), retargeted from winit's `MouseButton`/`ModifiersState` to GPUI's `MouseButton`/`Modifiers` but byte-identical on the wire. The motion gate is the pure ; the click-count / selection-mode classifier and edge-scroll helper port verbatim into  and . A golden byte-capture test replays every `mouse-byte-golden.json` case and the motion-gate truth table against the port.

Encoding lives in ; modifiers go in the Cb field (Shift +4, Alt +8, Ctrl +16) and SGR 1006 vs X10 is chosen per the terminal's `SGR_MOUSE` mode.

Stale mouse modes left behind by a dead foreground program (force-closed SSH, killed TUI) are cleared server-side when the shell prompt returns — the client needs no special handling because the injected DECRST arrives through the normal `PtyOutput` stream; see .

#### Mode Gate

The mouse-mode gate uses `intersects(MOUSE_MODE)`, not `contains`, because `contains` is always false for enabled mouse modes.

`MOUSE_MODE` is a union of three bits (`MOUSE_REPORT_CLICK | MOUSE_MOTION | MOUSE_DRAG`) that alacritty stores mutually exclusively — each DECSET (1000/1002/1003) clears the union then sets exactly one bit, so requiring all three (`contains`) never matches.

 and the wheel-handler mode read both use `intersects`. The other mode reads alongside it (`ALT_SCREEN`, `ALTERNATE_SCROLL`, `SGR_MOUSE`) are single-bit and keep `contains`.

#### Held-Button Tracking And Motion De-Duplication

App-forwarding is tracked separately from native selection so mouse-off behavior is unchanged: `mouse_selecting` still drives native click-drag selection, while `mouse_report_button` records the button currently forwarded to the app.

Drag motion (mode 1002) gates on `mouse_report_button.is_some()`, and the reported Cb carries that exact button rather than a hardcoded Left.

`mouse_report_button` is set when a press is forwarded.  clears it (and `last_mouse_report_cell`) on **every physical button release** — left, middle, and right — even when the release itself could not be forwarded (mode disabled mid-drag, Shift held), so a button-up always ends a forwarded press.

It is also cleared on **focus and pane transitions** via , the single chokepoint every pane/session/window focus path routes through. A press forwarded to the previously focused pane never sees its matching release, so dropping the tracked button here prevents phantom drag motion being reported to the newly focused pane or after the window regains focus.

The motion gating and per-cell de-dup themselves live in the pure : it reports motion only when mode 1003 (`any_motion`) is set, or mode 1002 (`drag`) is set and a button is held, and only when the pointer has entered a different cell than `last_reported`.  supplies the live mode bits, the held-button flag, and `last_mouse_report_cell` to it.

`last_mouse_report_cell` is intentionally seeded to the press cell when a press is forwarded (see  and the middle/right press arms). This deliberately suppresses the **first** same-cell motion event so the PTY is not flooded while the pointer sits on the press cell — matching alacritty's `cell_changed` semantics. It is not a bug: motion is reported only once the pointer crosses into a new cell.

#### Live-Path Decisions

The GPUI reporter owns the *decisions* around the encoders as pure functions too, so the shell's event handlers hold no policy of their own and the whole gate is unit-testable.

 is the pane's mouse-related DEC private modes, read off the live `Term` once per event by  and then passed by value. Tracking and its motion level are one `Option<MotionReporting>` field rather than two, because a motion level means nothing while the application tracks the mouse not at all; the encoding is a  rather than an `sgr` flag, which also keeps the struct under the workspace's bool-count lint.

 is the one gate every button path asks: an application that enabled tracking owns the pointer unless Shift is held, which is the universal override that lets a user still select text inside vim or tmux.  orders the three wheel consumers — a tracking application first, then the alternate screen's mode-1007 cursor-key fallback (), then this client's own scrollback — matching the winit client's priority exactly.

 converts one GPUI wheel event into signed terminal rows, positive meaning "backwards into the scrollback". GPUI already scales a notched wheel to three rows in its `ScrollDelta::Lines` form (the same factor the winit client multiplied in by hand), so that form passes through and only a trackpad's `ScrollDelta::Pixels` is divided by the row height. The platform sign is "positive `y` reveals the content above", which is traditional terminal behaviour, so `terminal.scroll.natural_scroll` — off by default — is the branch that inverts.

### Resize Coordination

Pane geometry is published from the GPUI layout pass, once per frame, and every whole-pane rebuild carries the geometry it was rendered at so it can be replayed at that shape rather than at whatever size the client has meanwhile moved to.

[[crates/scribe-client/src/main.rs#TerminalView#publish_pane_sizes]] measures each placement against the live cell metrics, skips the panes whose `TerminalSize` is unchanged, and for the rest does three things in order: reshapes the local display grid, sends `Resize`, and sends `RequestSnapshot`. The per-frame layout pass is the coalescing — a drag that crosses a cell boundary many times a second publishes only on the frames where the cell count actually changed, so a redraw storm never becomes a `RequestSnapshot` storm. The outbound channel is a single ordered FIFO, so the server still processes `Resize` before any `KeyInput` queued behind it and `SIGWINCH` reaches the PTY ahead of the bytes. This client owns no PTY and never reflows on its own, which is why the local reshape is paired with a request for the authoritative screen instead of standing on its own.

The two sides are therefore routinely out of step for the length of one round trip: the client narrows its grid immediately, while the server debounces its own `Term` resize and answers `RequestSnapshot` from the size it still has. That gap is what makes rebuild geometry load-bearing.

#### Attach announces the adopting pane's grid

A tab switch locally places its selected session before attaching, so the attach replay can use the exact grid that session will paint on its first frame.

An unshown tab has no `pane_sizes` cache entry, and `focused_pane_size` describes the outgoing tab. The two are not interchangeable: a per-tab prompt strip removes rows from only the tab that owns it. [[crates/scribe-client/src/main.rs#TerminalView#switch_tab]] therefore assigns and focuses the selected session locally first, then [[crates/scribe-client/src/main.rs#TerminalView#stream_session]] resolves [[crates/scribe-client/src/main.rs#TerminalView#placed_pane_size]] from that session's measured placement through the same [[crates/scribe-client/src/main.rs#TerminalView#painted_pane_rect]] and [[crates/scribe-client/src/main.rs#TerminalView#grid_size_for]] used by normal publication. It reshapes and caches that grid before sending `AttachSessions`; the server's first `SessionReplay` is already authoritative, and the following publish sees the cached size rather than issuing a corrective `Resize` / `RequestSnapshot` round trip.

Local adoption performs no wire operation, so the attach still precedes any publish that could send `Resize` or `RequestSnapshot`; the server never sees a resize for an unattached session. The seed `terminal_size` remains only for a brand-new window or cold-restart launch where no measured placement exists yet.

#### Rebuild Geometry

A rebuild is state, not a delta, so it is only correct at the geometry and scrollback size it was rendered with — carried on [[crates/scribe-client/src/ipc_bridge.rs#InboundEvent]]`::PaneRebuild` and [[crates/scribe-client/src/ipc_bridge.rs#PaneOp]]`::Rebuild` alongside the bytes.

[[crates/scribe-common/src/screen_replay.rs#snapshot_to_ansi]] emits every row as exactly `cols` printable characters — trailing blanks are literal spaces, with no EL or ED to absorb the difference — and ends on an absolute CUP in snapshot coordinates. Fed into a grid one column narrower, every row autowraps, the whole screen scrolls into scrollback, and the viewport is left blank with the cursor parked mid-screen; the shell's own `SIGWINCH` redraw then paints a duplicate prompt onto the wreckage.

[[crates/scribe-client/src/main.rs#reshape_for_rebuild]] closes that by reshaping the pane grid to the rebuild's own `cols`/`rows` before [[crates/scribe-client/src/sync_frames.rs#present_rebuild]] hands the bytes to the parser, and no-ops when the dimensions already agree or the wire reported zero. [[crates/scribe-common/src/screen_replay.rs#snapshot_to_ansi]] begins with RIS, which resets both buffers, history, margins, modes, attributes, and cursor before restoring the snapshot; it emits no ED 2, because Alacritty turns even a fresh-grid ED 2 into one synthetic history row. After replay, the drain still normalizes history to the rebuild's authoritative `scrollback_rows` solely for old-server pre-RIS payloads. The reshape is driven by the rebuild rather than by the layout, and only has to hold until the size the client actually asked for comes back as a rebuild of its own — which the `RequestSnapshot` already in flight guarantees. Both producers supply the geometry and history count: the `ScreenSnapshot` path through [[crates/scribe-client/src/main.rs#apply_screen_snapshot]] and the reattach `SessionReplay` path through [[crates/scribe-client/src/main.rs#forward_replay]]. That also covers the bounded-queue overflow resync, which repairs a dropped pane through the same `RequestSnapshot`.

#### Rebuild applies at its own geometry

A 120-column `ScreenSnapshot` rebuild driven through the real [[crates/scribe-client/src/ipc_bridge.rs#PaneOp]]`::Rebuild` path into a pane grid still sized 119x36 leaves the pane at 120x36 with each snapshot row on its own line.

Without the reshape the grid stays 119 wide and the viewport opens on row 18 — half the snapshot has autowrapped into scrollback — which is exactly the blank-pane corruption a window drag produced roughly 25 times a second.

#### Rebuild retains authoritative scrollback

A modern raw replay produces exactly zero or N history rows, while compatibility normalization leaves an old pre-RIS replay at the same authoritative count.

This distinguishes the producer root fix from the N-1 receiver guard and proves genuine scrollback remains usable.

### IME Composition

IME is wired via `WindowEvent::Ime` with a per-window `Option<PreeditState>`, gated on focus and surface, suppressing keys only during composition, routing committed text through the existing PTY write path.

The state lives in  (composition text, optional caret byte range, and the absolute scrollback row + column where composition started). The state machine: `Enabled` arms the gate; the first non-empty `Ime::Preedit` creates a `PreeditState` anchored at the focused pane's current cursor cell; subsequent `Ime::Preedit` events update the text and caret on the same anchor; `Ime::Commit` clears `PreeditState` before sending the committed bytes through the normal `ClientMessage::KeyInput` path so the preedit overlay disappears in the same frame the PTY echo arrives; an empty-text `Ime::Preedit` or `Ime::Disabled` cancels and clears the state.

#### Activation Gate

The gate predicate is `window_focused && current_surface == TerminalPane`; the result is ANDed with the X11 active-window guard and pushed to `window.set_ime_allowed(...)` whenever it changes.

 handles the immutable surface check: it returns false when the search overlay, command palette, or close / update / context-menu dialogs are active.  then ANDs that with `!compositor_overlay_active` (the X11 active-window guard, which mutates the post-reactivation debounce) and pushes the result to winit. The pushed value is memoised in `last_ime_allowed` so steady-state ticks (the per-frame `about_to_wait` call on Linux) short-circuit without touching winit IPC; the gate only flips when the computed value differs from the last push. Whenever the gate flips to disallowed, any in-flight `PreeditState` is dropped so focus loss or surface change immediately retires the visual overlay (FR-008) and clears the  short-circuit.

#### Cursor-Rect Strategy

 is re-pushed from the redraw path whenever the focused pane's cursor cell coordinates change or the gate state flips, with a memoized last-pushed rect to dedup redundant winit calls.

The cache lives in `last_ime_cursor_area`, so identical frames skip the winit IPC; pushes are suppressed entirely while the window is occluded (mirroring 's occlusion-gate pattern). `WindowEvent::Resized` and `WindowEvent::ScaleFactorChanged` force an immediate fresh push after the resize handler runs, and focused-pane changes invalidate the cache so the popup re-anchors on the new pane.

#### Preedit Rendering

The overlay layers above the terminal grid and below search / dialog overlays — a theme-foreground underline (Alacritty-minimal) anchored at the composition-start cell on a scrollback-stable absolute row, clipped at the pane right edge.

 computes a `PreeditOverlay { origin_px, cell_px, text, max_cells }` per frame from the saved anchor and the focused pane's current layout, returning `None` while the viewport is scrolled into scrollback (`grid.display_offset() > 0`) so the underline can't render at the wrong visual row. Per-char advances come from `unicode_width::UnicodeWidthChar::width` (matching the renderer's styled-run accumulator), so CJK wide glyphs reserve two cells and zero-width combining marks ride the prior base glyph (a leading combining mark with no base is skipped).  then emits a background fill behind the preedit cells, one glyph per advance via the existing cosmic-text + atlas path, and a 1px theme-foreground underline (hi-DPI scaled to ≥1 physical pixel) — per `research.md#R4`. Because the anchor uses an absolute scrollback row, terminal scroll keeps the preedit pinned to the originating line.

## IPC Client

The IPC connection runs in a background thread with its own Tokio runtime, defined in .

### Communication Flow

The main thread sends `ClientCommand` variants through an mpsc channel to the write task for socket serialization.

The write task serializes commands to `ClientMessage` and writes to the socket. The read task deserializes `ServerMessage` responses and dispatches them as `UiEvent` variants through the winit event loop proxy. `UiEvent::PromptReceived` carries session ID, provider, and prompt text for the prompt bar feature.

Automation requests use that same path in both directions. `scribe-cli action ...` becomes  `DispatchAction`, the server forwards it as  `RunAction`, and the client executes it through the same handlers the keyboard shortcuts and command palette already use.

### Terminal Image Live Scene

Each GPUI pane owns an immutable, bounded CPU image scene that advances atomically beside its text grid.

`TerminalImageLive` records enter the same ordered inbound queue as `PtyOutput`.
[[crates/scribe-client/src/terminal.rs#DisplayOnlyTerminal#apply_image_live]]
hands each record to
[[crates/scribe-client/src/terminal_image_scene.rs#LiveImageScene#apply]], which
clones the committed scene on `Begin`, stages definitions, contiguous bounded
chunks, placements, deletes, and grid effects, then swaps one `Arc` only on a
matching `Commit`. Invalid, stale, interrupted, or incomplete generations drop
their staging state without exposing a partial operation burst.

Definitions and placements retain operation order. Replacement removes the old
image's placements only after replacement bytes complete; delete, screen,
scroll, erase, resize, and reset effects clean active scene state under the
frozen per-session quotas. Classic and Sixel scroll/resize clipping persists as
exclusive logical cell bounds intersected with a conservative placement
envelope, leaving source, destination, and pixel offsets unchanged. Nonzero
offsets extend that envelope by one cell; later scrolls move only placements
whose effective old clip intersects their margin. Resize and Sixel erase also
use the effective envelope. Physical delete selectors use the same clipped
extent, while placeholder cells remain the virtual placement authority and
only an explicit image-id delete removes their placement. Hard deletion derives
definition candidates from placements actually removed; only explicit Image
scope may add an unplaced definition, preventing unrelated data sweeps.
`PaneFrame` captures
the committed CPU scene with the text snapshot but no GPUI image resource or
cache.

[[crates/scribe-client/src/terminal_image_scene.rs#filter_terminal_image_placeholders]]
removes `U+10EEEE` and at most its three zero-width coordinate marks from
selection/copy and outbound search queries while retaining ordinary surrounding
and combining text. A typed capability mismatch reaches the visible pane status
strip through
[[crates/scribe-client/src/terminal_image_scene.rs#capability_mismatch_message]];
the client continues offering false image capabilities until painting exists.

### Server Lifecycle

Starts and connects to the server process, with a retry loop waiting up to 5 seconds for the socket to appear.

On Linux, the client starts the server via `systemctl --user start scribe-server`. On macOS, release builds install `com.scribe.server.plist` into `~/Library/LaunchAgents/` with the current bundle's `scribe-server` path and an `EnvironmentVariables` PATH of `/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin` (launchd's default omits both Homebrew prefixes; the server-side floor in [[crates/scribe-server/src/session_manager.rs#path_with_macos_baseline]] backstops non-launchd starts), re-bootstrap the job if that path changes, and then `kickstart` it. If a socket already exists, the client inspects the connected server's peer PID and restarts it when the running executable path differs from the current bundle or when the installed server binary is newer than the running process start time, which lets manual DMG replacements hot-reload the background server on next launch. When that stale-server refresh fires, the client prefers a direct `scribe-server --upgrade` spawn over `launchctl kickstart -k` so the new server performs a handoff with the still-running old one; kickstart only terminates the old server when launchd still manages it, and after a DMG drop-replace that old server is typically a launchd orphan whose flock a fresh non-upgrade child would crash-loop against. `launchctl` remains the fallback if the direct spawn fails. Dev builds without a bundle fall back to spawning the server binary directly.

Stable macOS launches also repair bundled AI hooks before any window mode is chosen. Because a DMG install has no `postinst` equivalent, `repair_ai_hooks_on_startup` probes `~/.claude/settings.json` and `~/.codex/{config.toml,hooks.json}` for the current app bundle's `Contents/Resources/ai-hook-{claude,codex}.sh` paths and reruns the bundled setup scripts when they are missing or stale after a first install or app move. Dev builds skip the repair, and Claude's `statusLine` is only rewritten when it is absent or still points at an older Scribe bundle, so custom non-Scribe status lines are preserved.

 polls the socket's peer PID after the `--upgrade` spawn and only returns once the connected server differs from the captured old PID. If the new server fails to take over within `SERVER_REFRESH_TIMEOUT` — most often because the in-process handoff aborted (incompatible state format from a long-deferred update, peer-validation rejection, or any other `--upgrade` exit) — the client falls back to , which force-terminates every scribe-server process other than ourselves (via `pgrep -x scribe-server`), removes stale `server.sock` / `handoff.sock` files, and starts a fresh server through the normal path. Live sessions in the stuck old server are lost on that fallback path, but Scribe launches instead of looping until the wait deadline expires and then crashing.

The same `perform_macos_update_restart` is invoked from  when  times out and `pgrep` still reports a stale scribe-server. This covers the case where the old server is alive but its IPC accept loop has wedged: `tokio::net::UnixStream::connect` returns `ECONNREFUSED`, so the refresh path never runs; the fresh server spawned by `start_server` then can't acquire `flock` because the stuck old process still holds it. The pgrep-based recovery kills the orphan and restarts cleanly. Recovery only runs when pgrep finds a stale process so legitimate slow-startup timeouts still surface as errors.

## Remote Control

Feature 013's connecting side: a remote-dialed client process attaches to another tailnet machine's window over TCP, auto-reconnects after drops, and renders the displaced state when another controller takes the window.

The owning side is  and the wire contract is . A remote-control window runs as its own process launched with `SCRIBE_REMOTE_DIAL` (and optional `SCRIBE_REMOTE_WINDOW` / `SCRIBE_REMOTE_TAKEOVER`) in its env, spawned by  from the connect picker or a reclaim. The owning machine's own status indicators are separate — see .

Feature 014 adds a direct-LAN connecting path (`SCRIBE_LAN_DIAL`, mutual TLS + device approval) beside the tailnet dial — see  — plus the owning-side approval prompt this machine raises for an unknown LAN device — see . The picker merges both peer sources () and the status bar shows which transport a controlled window uses ().

Feature 015 turns the connecting side into a live participant in a shared window rather than a sole controller: a shared client keeps receiving live output but, as a viewer, suppresses its own input and offers a take-control affordance, tracks the roster, and shows a status-bar presence badge. The frozen  banner now appears only for `SingleController` displacement, never for a shared join. The default picker attach also becomes a non-takeover additive join. See .

### Remote Dial

When `SCRIBE_REMOTE_DIAL` is set,  dials the peer's `host:port` over TCP via  instead of the local Unix socket, running the  as the first frame before any `Hello`. The dial parameters travel as one .

Unlike the local path this is strictly connect-only — it never starts or upgrades the peer's server.  returns an  carrying the command sender plus, for a remote process only, a  switch.  lets other code (the reclaim path, status suppression) tell whether this process is a controlling-side window and re-dial the same peer.

### Reconnect State Machine

A dropped remote link auto-reconnects with capped exponential backoff (research D6):  retries up to  times, delay from  (500 ms base, 15 s cap), re-running the preamble and re-claiming the SAME window with `Hello { takeover: false }` on each attempt.

Each attempt emits `UiEvent::RemoteReconnecting { attempt }`, driving a cancelable  ("Reconnecting to `<peer>`… (attempt n)"); success emits `RemoteReconnected` and exhaustion emits `RemoteReconnectFailed` (one-action reconnect). Because reconnect always uses `takeover = false`, a window reclaimed by someone else mid-outage lands the client in lost control rather than silently re-seizing it (FR-011). Cancel — via the overlay () or the `RemoteReconnectCancel` switch — settles into a disconnected state, and a delivered `RemoteDisconnect` sever notice ends the loop and reports the disable as fact.

Each attempt is cancel-aware end to end (FR-011):  races the whole connect → handshake → `Hello` sequence — run as  returning a  — against , so a Cancel or delivered sever fired mid-attempt drops the in-flight (half-open) stream instead of completing and emitting `RemoteReconnected` over a settled overlay. Because `try_reconnect_attempt` emits no UI events the caller alone decides whether the attempt goes live, and  now carries the same `is_settled()` overlay guard as , so a late success never revives a window the user already settled.

The status-bar connection dot reflects the down link (see ): `server_connected` flips false in `handle_remote_reconnecting` and  — the remote read task deliberately emits no `ServerDisconnected`, so nothing else clears it — and is restored true only by a successful , so it stays red through every retry, failure, and severed state.

### Connect Picker

The command palette's "Connect to remote machine…" action () opens the GPU-overlay  picker.

It lists same-account online peers — fetched by a local `ListRemotePeers` request ( → ) — with a manual-entry field, then the chosen peer's window list (, workspace names and session counts) plus a "New window" option.

Key handling () returns a  that  turns into a  call: attaching an existing window uses `takeover = true`, creating a new one uses `takeover = false`. Each failure class maps to distinct copy per UX-002 (unreachable / disabled / unauthorized / version / busy / taken-over);  surfaces a delivered sever notice. The spawn also clears the inheritable takeover markers on the paths that must not use them —  `env_remove`s `SCRIBE_REMOTE_WINDOW` / `SCRIBE_REMOTE_TAKEOVER` on the new-window and non-takeover branches — so a child launched from an already-takeover process never inherits a stale marker and silently re-seizes a window (FR-011).

Feature 015 changes the default attach from a takeover to an additive share join and shows live occupancy, superseding the 013 default above:  now dials an existing window with `takeover = false` (the owning machine decides additive-join vs lost-control by its mode), reserving `takeover = true` for an explicit reclaim of a window this user lost. Each existing-window  carries a `participant_count` and `mode` from the enriched , rendering "shared · N attached" occupancy when two or more machines are attached instead of 013's binary in-use marker.

Feature 014 merges a "Local network" source into the same picker.  requests both peer lists (`ListRemotePeers` + `ListLanPeers`) and feeds them via  / ; the two are merged into s by machine name, each tagged with the  it dials over ("Local network" vs. "Tailscale" from ). A confidently name-matched dual-reachable machine appears once with the direct LAN path preferred (FR-008), while unmatched peers may appear once per transport, each labeled. An incompatible LAN advertisement stays visible as a disabled row with both protocol versions and machine-specific update guidance; it cannot produce a dial intent and does not consume a compatible same-name Tailscale row (FR-014).  returns a  whose `ProbeWindows` / `Attach` / `NewWindow` variants now carry the chosen transport;  routes a LAN choice through  and  →  (setting `SCRIBE_LAN_DIAL`), which runs the full TLS + device-approval gate. Manual `host:port` entry is retained for both transports.

### Displaced and Lost Control

When the server sends `WindowTakenOver`,  builds a : the last frame is dimmed and frozen under a banner naming the new controller's device and account, with one-action reclaim.

Under feature 015 this frozen path is scoped to `SingleController` displacement — an exclusive takeover or a mode flip to `SingleController` (FR-003/017) — because a shared join or single-typist control pass keeps the displaced machine live and drives the roster instead ().

Reclaim () re-claims the same window with `takeover = true`, but the two sides differ. The LOCAL owning-machine reclaim () is IN PLACE: it drops the displaced command sender — closing that connection cleanly, with no spurious `ServerDisconnected` because the local  select aborts the read task once the write task ends — then starts a fresh local IPC thread via  whose first `Hello` swaps the writer back, clears the banner, and KEEPS the window (no close+reopen, geometry already correct). The REMOTE controlling-side reclaim still re-dials the peer in a fresh process () and closes this window — a documented follow-up to make it in-place too. Either way a short grace timer defers the pane reattach () so a displacing `WindowTakenOver` can land first: a reclaim that lost the race resolves cleanly to lost control (the reattach is skipped) instead of clobbering the new controller.

### Shared Viewer and Control

Feature 015's connecting-side sharing: a shared client stays live but, as a viewer, suppresses its own input and offers a take-control affordance. The roster arrives as `ShareRoster`, driving both the affordance and the presence badge (FR-006).

 stores the incoming roster as a , and  matches this connection to its own entry by `Welcome.participant_id` (falling back to the `is_local` / device-name entry) so  is exact.  is true only in `SharedSingleTypist` while this client is NOT the holder; when it holds, or in free-for-all, input flows normally. A viewer's input is suppressed at three choke points so nothing the server would drop is ever sent: keystrokes (), paste, and the raw/synthetic byte path (, which also covers mouse-reporting sequences).

Instead of typing, a viewer's key raises a non-intrusive  naming the current holder and how to take control (); pressing Enter while it shows calls , which sends `ControlClaim`. The client sends the same frame whether the owner's policy is free-claim or request-and-grant and learns the result from the next `ShareRoster` (granted) or a `ControlDenied`. When this client is the approver under request-and-grant, an incoming `ControlRequested` becomes a modal  resolved by  (Enter grants, Esc denies) into a `ControlGrant` naming the requester.  surfaces a `ShareEnded` notice when the owner ends the share.

### LAN Dial

Feature 014's LAN connecting path: when `SCRIBE_LAN_DIAL` names a peer, the client dials `host:port` over mutual TLS instead of the tailnet TCP path, running the  handshake before any `Hello`.

 reads the env;  obtains this machine's own  — the client links the `scribe-server` crate's LAN identity/TLS layer rather than a separate stack — and builds the  that presents it. Crucially the identity is FETCHED from this machine's own local server over the local-socket-only  exchange () and rebuilt via , never reading the OS keyring from the client binary: on macOS the keyring's legacy `SecKeychain` per-item ACL trusts only the creating binary (`scribe-server`), so a cross-binary key read is denied and the server stays the sole keychain accessor. Any fetch failure (server down, identity unavailable, malformed reply) fails closed to a `ConnectionFailure` outcome, so the dial never proceeds without a valid identity.  drives the connection:  runs the TLS connect + `LanHello` (bundled as a ),  completes the first attach, and  hands the split TLS stream to the transport-agnostic session loop shared with the tailnet path.

While the owner holds an unknown device pending approval the dialer receives `LanApprovalPending` and emits `::LanAwaitingApproval` — a cancelable "waiting for approval on `<peer>`" state (FR-014, US2.5); the terminal result surfaces exactly once as `UiEvent::LanDialOutcome` carrying a  whose refusal copy mirrors the  taxonomy. A dropped LAN link auto-reconnects like the tailnet path via  ( returning a ), re-claiming the same window with `takeover = false` so a window reclaimed mid-outage lands in lost control (FR-011).

### LAN Approval Prompt

The owning side of feature 014: when this machine's server holds an unknown LAN device pending approval, this client renders a GPU-overlay dialog showing the device's fingerprint before any data flows (SEC-001, UX-002).

 receives `::LanApprovalRequest` and builds a  () naming the requesting device, its fingerprint words, and the trusted network it arrived on, with equally prominent Approve / Decline. When the advertised name collides with an already-trusted device the dialog adds the `name_collision` informational hint ("approve only if you recognize this one").  turns the  into a `::LanApprovalDecision` echoing the request id, routed to the local server by .  and the keyboard / click / hover handlers drive the dialog; at most one is in flight at a time.

## Selection

Text selection in  supports three modes: Cell, Word, and Line. Coordinates are absolute grid positions.

Cell selects individual characters. Word boundaries include alphanumeric, underscore, dash, dot, slash, tilde, at, plus, percent, hash, question, ampersand, and equals, and double-click word scans cross WRAPLINE-connected rows so soft-wrapped paths or commands stay contiguous. Line mode follows WRAPLINE flags for logical lines.  converts mouse pixel coordinates to grid positions, subtracting tab bar height and prompt bar height (position-aware) before dividing by cell size. During an active drag,  clamps points that stray into prompt-bar chrome or outside the pane back to the nearest visible terminal cell so the last visible row still highlights.

### Smart Selection

Smart Selection extends click selection with configurable semantic regex matching over the visible wrapped logical line.

 compiles the global `terminal.smart_selection` rules and maps regex byte ranges back to terminal grid cells. A candidate must contain the clicked cell. For each rule, the longest containing match is kept; the final selected candidate comes from the highest precision class with any match, then the longest match in that class.  reuses normal `SelectionRange` highlighting and copy-on-select behavior.

Built-in recognizers come from  and are restored via `terminal.smart_selection.reset`. The defaults include a `whitespace_word` fallback at VeryLow precision, while higher-precision recognizers (paths, URIs, emails, quoted strings, namespace identifiers) still win when they match.

The default activation is quad-click, preserving double-click word and triple-click line selection. When activation is set to double-click, Smart Selection replaces ordinary double-click word selection and falls back to word selection only when no rule matches. Shift still bypasses mouse-reporting applications before local selection starts.

Right-click context menus run Smart Selection at the pointer. Matching rules with actions add explicit menu items; selection alone never executes them. Action parameters support iTerm2-style legacy substitutions (`\0`, `\1`-`\9`, `\d`, `\u`, `\h`, `\n`, and `\\`) and interpolated strings such as `\(matches[0])`, `\(path)`, `\(user)`, and `\(host)`.

### Scroll Adjustment

Selection coordinates are adjusted when PTY output or resize shifts grid content via `history_size` delta.

 shifts the active selection and drag anchors.  handles saved selections on background tabs. Selections that move past `topmost_line` are cleared.

## Scrollbar

An overlay scrollbar in  that fades in on scroll and fades out after 1.5s of inactivity.

Width animates on hover via lerp expansion. The hit zone is 3x the visible width for easy targeting. Drag-to-scroll computes offset from mouse delta relative to track height. Fade-out duration is 0.3 seconds.

The pure module has a second consumer: the settings window's content pane reuses the same fade state, thumb geometry, and pointer gestures — hover widen, click-to-jump, and thumb drag — as its page-length affordance, counting whole pixels as its scroll unit. Its render pass goes through `build_scrollbar_render` with no command marks, because that is where the module drives the hover width target. See [[lat.md/settings#Settings#GPUI Settings Window#Typeset Ink presentation]].

### Prompt Mark Indicators

Each `::command_records` entry renders as a 2px scrollbar tick at `abs_pos / (history_size + screen_lines)`, coloured by command status: neutral for `Unknown`, theme-derived hues for `Success`/`Failure`.

`command_records` (a `Vec<CommandRecord { abs_pos, status }>`) supersedes the old flat prompt-mark list. OSC 133 `D` exit codes now reach the client: `::PromptMark` carries `exit_code`, and `handle_prompt_mark` runs the A→D state machine (`A` opens an `Unknown` record; `D` resolves the most-recent open one — exit 0 `Success`, non-zero `Failure`, absent stays `Unknown`, never falsely `Failure`). The authoritative, accessible cue is a non-colour glyph (`✓`/`✗`/`?`) in the  (`Pane::last_command_status`); the scrollbar colour is a redundant secondary hint.

Records are stored as absolute scrollback positions (lines from the very top of scrollback, 0 = oldest). When scrollback shrinks — via `::TrimScrollback` during AI redraw epochs, or natural overflow at the configured `scrollback_lines` cap — surviving rows shift down in absolute index. `handle_trim_scrollback_event` calls  to keep indicators aligned with their original command rows (each record's `abs_pos` shifts; per-record status preserved); the scrollbar render path additionally clamps any residual stale abs to the track bounds so a mark from a not-yet-shifted shrink path cannot draw outside the track. On reattach/cold-restart `command_records` starts empty (replay reproduces cells, not OSC 133 callbacks) — historical rows show no status rather than a fabricated one.

## Dividers

Pane split dividers in  are 1px solid quads with a 4px hit tolerance for drag resize.

Focus borders are rendered as 2px accent-colored quads on the focused pane's leading edge. Workspace focus borders render as four thin quads around the entire workspace rect.

## AI Indicator

The  tracks per-session AI state with pulsing border animations.

The shared animation loop uses a generation token per spawned thread, so fast stop/start cycles from AI pulses, scrollbar fades, or stalled-sync recovery retire older timer threads instead of letting them keep emitting `AnimationTick`. The AI-pulse contribution is additionally bounded by a  so a long-lived AI state cannot keep the loop alive — and the GPU busy — indefinitely.

Priority order: PermissionPrompt > WaitingForInput > IdlePrompt > Error > Processing. Each state has configurable color, pulse frequency, tab indicator, and pane border settings. Error state decays over a timeout. Attention states (IdlePrompt, WaitingForInput, PermissionPrompt) clear on keystroke. Both `IdlePrompt` and `WaitingForInput` share the same `waiting_for_input` indicator config (color, pulse, timeout).

Tab inline context % is gated via ; see  for the gating rules and rendering details. The percent itself lives in a parallel `last_contexts` map alongside `detected_providers`, so a border-clear (stale-Processing prune, attention-state keystroke clear, Error decay) does not drop it — see . The map is cleared explicitly on session removal and on conversation change via , called from .

On reconnect, active AI state is populated from `SessionInfo.ai_state` during handle_session_list so indicators appear immediately without waiting for the per-session `AiStateChanged` messages from the server's `send_stored_metadata` path. `SessionInfo.ai_provider_hint` is restored separately so clipboard cleanup and other provider-aware behavior survive reconnect even when no visible indicator should be shown. When available, `SessionInfo.ai_state.conversation_id` is also used to seed per-pane AI resume bindings so restored windows attempt targeted resume of prior provider sessions.

### Pulse Envelope

Pulse lifetime is decoupled from AI-state lifetime so a stuck or idle session cannot pin the shared 30 fps redraw loop — and the GPU — forever.

The policy gate is , consulted by both `needs_animation` (whether the shared loop may retire) and `animated_color` (pulsing vs. a steady resting colour). Attention states (`IdlePrompt`/`WaitingForInput`/`PermissionPrompt`) pulse for a bounded window after entry, then rest while still tracked and visible; they still clear instantly on keystroke. `Processing` pulses only while *alive* — within an idle window of the last liveness signal. Liveness is a state edge or fresh PTY output recorded via , fed from .

A genuinely-working session keeps re-arming the envelope across hook-silent tool calls; a hung AI on a still-open PTY goes silent, the pulse rests, and the loop retires to winit `ControlFlow::Wait` at zero GPU. When output resumes for a rested session the loop is restarted from `handle_pty_output`. Envelope durations are `ATTENTION_PULSE_SECS` and `PROCESSING_IDLE_PULSE_SECS` in .

#### Stale-State Clear

A rested pulse still shows its state's *colour*. A crashed or killed AI would otherwise show a stale `Processing` border forever: it can never fire its own terminal hook, and the server supervises only the shell.

 removes any `Processing` state with no liveness (hook edge or PTY output) for `STALE_PROCESSING_CLEAR`. It uses a wall-clock map (`last_activity_instant`) rather than the f32 animation clock, which freezes once the loop retires — the very case this must still catch. The client calls it lazily from : zero cost until something is stuck, and resolved before the indicator is observed (the user returning wakes the loop). Only `Processing` is cleared — attention states legitimately persist until the human acts — and `detected_providers` plus `last_contexts` are preserved so provider-aware clipboard cleanup survives and the context % stays visible in tabs and prompt bars, mirroring reconnect.

#### Occlusion Gating

A fully hidden window shows nothing, so keeping the pulse — and the redraw loop — alive for it is pure waste.

 tracks winit `WindowEvent::Occluded` in `window_occluded`; `handle_animation_tick` ANDs `!window_occluded` into `ai_animating` so the loop retires while hidden and re-arms on un-occlude.

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

Desktop notifications fire on `Processing → attention` AI state transitions. Delivery goes through a cross-platform dispatcher so  talks to one channel regardless of OS.

 stores the previous `AiState` per session and is called from `handle_ai_state_changed` before the `AiStateTracker` update. When a `Processing → attention` transition is detected (`IdlePrompt`, `WaitingForInput`, `PermissionPrompt`), a `NotificationPayload` is returned and  checks focus suppression based on : `WhenUnfocused` suppresses when the window is focused regardless of tab, `WhenUnfocusedOrBackgroundTab` only suppresses when both the window is focused and the session is the active tab, and `Always` never suppresses for focus reasons. The notification summary includes the workspace name or project root basename and the state label (Ready, Waiting for input, Permission required). The body carries the user's last submitted prompt text from `pane.latest_prompt`.

### Cross-Platform Dispatcher

 is started alongside the IPC thread in `resumed` and returns an `mpsc::UnboundedSender<NotifReq>` stored on `App.notification_tx`.

The sender always exists; main.rs has no `#[cfg(target_os = …)]` gates for notifications. Platform divergence lives entirely inside the `notification_dispatcher` directory — `linux.rs` (raw `zbus`) and `macos.rs` (`notify-rust`) — and both export the same `spawn(proxy) -> UnboundedSender<NotifReq>` shape, mirroring the `winit::platform_impl` / `wgpu::hal` pattern of OS-protocol abstraction.

The dispatcher receives  variants: `Show` from `maybe_fire_notification`, `Close` from  on session exit and `AiStateCleared`, and `Shutdown` from  on the terminal exit paths. `ShowReq::new` and `NotifReq::close` hide Linux-only payload fields from non-Linux builds so macOS only carries the data its backend uses.

### Linux Backend

 runs a single long-lived dispatcher thread that owns one D-Bus session-bus connection for every notification this client ever fires.

The thread runs its own single-threaded tokio runtime, opens a `NotificationsProxy` (generated by `#[zbus::proxy]` from ) against `org.freedesktop.Notifications`, and subscribes once to the `ActionInvoked` and `NotificationClosed` signal streams. The main loop `tokio::select!`s between the request channel and those two streams. Repeated state changes for the same session reuse `replaces_id` from a `session → notification id` map so the daemon atomically swaps an existing toast in place — no stacked toasts under `condition = "always"` and no thread or D-Bus connection accumulation under `timeout_mode = "never"`.

`ActionInvoked` looks up the toast id in the reverse map and sends `UiEvent::RunAction { FocusSession }` through the `EventLoopProxy`. `NotificationClosed` removes the entry from both maps when the daemon retires a toast. `NotifReq::Close` calls `CloseNotification(id)` to dismiss stale toasts proactively; `NotifReq::Shutdown` closes every live notification before the loop exits.

This replaces the earlier per-notification `std::thread` + `notify-rust` `wait_for_action` pattern, which leaked one OS thread and one D-Bus connection per fired notification under the `condition = "always"` + `timeout_mode = "never"` combination. `notify-rust` is dropped from the Linux dependency set; raw `zbus` handles the `Notifications` interface directly. Linux intentionally skips `request_user_attention` because on X11 the urgency hint can become a second shell-level "`<app>` is ready" notification on top of the explicit desktop notification. The tracker also suppresses Linux bell-driven urgency for two seconds after an AI notification from the same session so BEL does not immediately cover the richer D-Bus toast with the generic shell fallback.

Linux notification expiry is configurable through : `system_default` maps to `expire_timeout = -1` (server default), `custom` maps to `timeout_secs * 1000`, and `never` maps to `expire_timeout = 0` (resident until dismissed). The dispatcher passes the resolved value straight through to the `Notify` D-Bus call.

### macOS Backend

 runs the same dispatcher loop shape as Linux but services each `Show` request with a synchronous `notify_rust::Notification::show()` call against `NSUserNotification`.

`Close` and `Shutdown` are no-ops on macOS because `notify-rust` exposes no programmatic dismiss path — the system retires toasts on its own timeline. Click-to-focus uses a focus-on-activate fallback: `set_last_notified` records the session ID when a notification fires, and when macOS activates the app after a click, the `Focused(true)` handler calls `take_pending_focus` to consume the pending session and dispatch `handle_focus_session`. A 30-second expiry window prevents stale notifications from switching tabs. While an update is already announced in the window title, non-update `request_user_attention` calls are suppressed so macOS does not keep resurfacing the update-ready text for unrelated AI notifications or bells. macOS ignores the timeout-mode config because `notify-rust` cannot set banner lifetime there; the Notifications settings page instead offers a shortcut to the system Notifications pane so the user can choose the persistent style for Scribe themselves.

### FocusSession Routing

The  `FocusSession` variant routes through the existing automation dispatch path on both platforms.

`execute_automation_action` calls `handle_focus_session`, which looks up the session via `session_to_pane`, switches workspace and tab, and raises the OS window with `focus_window`. Notification settings are configurable in the settings window under the Notifications page.

## Prompt Bar

A per-pane bar that tracks the user's most recent AI prompts as a flat edge-to-edge strip at the top or bottom of the terminal content. This heading described the deleted winit renderer.

See [[client#GPUI Prompt Bar]] for the live mechanism — the pane's [[crates/scribe-client/src/prompt_bar.rs#PromptBarData]] wrapping [[crates/scribe-common/src/protocol.rs#SessionPromptState]], keyed per session in [[crates/scribe-client/src/main.rs#AiChrome]], and persisted across a cold restart via the launch record described in [[client#Window State#Cold Restart Restore Store]].

## Split-Scroll

Pins the live terminal bottom while scrolled up in AI panes, so users can compose prompts while reading earlier output.

When `scroll_pin` is enabled (default `false`) and the user scrolls up in a pane with a detected AI provider (), the viewport splits into a top portion (scrollback at the user's offset), a 1px divider, and a bottom portion (live terminal at `display_offset=0`). State is stored as `split_scroll: Option<SplitScrollState>` on . The  holds the computed `pin_height`. Alternate-screen TUIs are excluded: Scribe clears `split_scroll` whenever a pane enters `ALT_SCREEN` or otherwise stops being eligible, because stitching scrollback together with a live full-screen UI reintroduces clipped prompt backgrounds, broken animation, and row-position artifacts.

The bottom portion height is fixed-size in : `AI_PROMPT_BLOCK_ROWS` (8) rows clamped to `[3, screen_lines - 3]`, sized to fit the typical AI prompt UI block (status line, permission/help hints, input box). The pin's *contents* are then translated downward by  so the cursor row lands at the last row of the screen content area, regardless of where it sits naturally in the live grid. Without translation, an AI tool that draws the prompt in the upper half of the live screen (e.g. after a fresh launch or terminal resize) would have its cells filtered out by  and disappear while scrolled.

Translation works because AI tools generally render top-down and leave the rows below the cursor empty (or fill them with idle UI like the input row's bottom border). Shifting every live cell by `(screen_lines - 1 - cursor_line) * cell_h` puts the cursor at the bottom of the pin region; rows naturally above the cursor stack upward into the pin and rows naturally below the cursor are pushed off-screen. When the cursor is already on the last live row the shift is zero, so split-scroll falls back to its original behavior. Trim handling still calls  with the dropped-row count after each `::TrimScrollback` so prompt-jump and scrollbar markers stay correct.

Before converting pin rows into pixels,  checks the live view's `WRAPLINE` flags around the cursor-anchored boundary `cursor_line - pin_rows + 1` and expands the pinned region upward when that boundary would land inside a soft-wrapped logical line. The expansion stops once the boundary reaches the wrapped line's first row or the top portion would drop below three rows.

Rendering uses a dual-render approach in `build_all_instances`: the terminal is rendered at the current `display_offset` (scrollback) and the instances are filtered to the top portion's Y range; then `display_offset` is temporarily set to 0 (live), rendered again, the live cells are translated by `live_cell_y_translation`, filtered to the bottom portion, and the offset is restored. Selection highlighting is applied to each half before filtering, using the scrollback half's saved `display_offset` and the live half's zero offset, so selections remain visible while split-scroll is active. Chrome (divider + jump button) is rendered by .

Typing while split-scrolled sends keystrokes without snapping to bottom. Pressing Enter (`\r`) snaps to bottom and clears `split_scroll`. Paste always snaps. A clickable docked jump chip appears in the bottom-right corner of the top portion, with layered chrome, a continuous arrow-to-line icon, and a brighter hover state so it reads as part of the split divider instead of a floating glyph.  handles click detection. Scroll activation and deactivation is managed by the free functions `update_split_scroll` and `reconcile_split_scroll`, which check `display_offset`, `scroll_pin` config, AI provider detection, and alternate-screen mode.

## Status Bar

The status bar is rendered at the bottom of the window with segments for connection status, command status, workspace info, CWD, git branch, session count, time, and system stats.

The settings gear renders at the band's far right and opens the settings window on click — the pointer-based settings entry point, moved out of the titlebar. Beside it, a balance button (`⊞`) renders whenever the window holds two or more panes and resets every workspace-region and pane split to equal space via [[crates/scribe-client/src/main.rs#TerminalView#equalize_layout]]. Update availability and progress also render here, centered in the empty span between the left and right segments — see  — so the CTA stays visible on narrow windows and steps down to a shorter `↑ Update` label, then disappears entirely, only when the empty span cannot hold it. Clicking the update segment opens the in-app confirmation dialog.

Connection is indicated by a green/red dot. Transient connection and pane status messages (input refused, reconnect reasons, denied-frame errors) are not rendered at all — internal plumbing noise is kept off the user surface; `set_status` logs them at WARN and the string is otherwise unread. A command-status glyph (`✓`/`✗`/`?`) sits next to the dot, reflecting the focused pane's most recent command outcome from shell integration (Success/Failure/Unknown); it is the authoritative non-colour cue and stays hidden until the first command resolves. When env-persistence is enabled and the focused pane's `::env_status` is `Some(Degraded { .. })`, a `⚠` (U+26A0) warning glyph in the palette's warning slot renders immediately to the right of the command-status indicator (see ); `None` and `Some(Active)` render nothing. The glyph's hover tooltip directs the user to retry from Settings → Terminal → General. Workspace name appears when multi-workspace. The focused pane's remote host overrides the local hostname when shell integration emits session context, and tmux session names render as a separate accent segment. Stats include CPU sparkline, memory percentage, GPU sparkline (Linux only), and network sparklines.

### Remote Control Surfaces

Feature 013 (T022) adds two owning-machine remote indicators to the left side, between the env-warning glyph and the workspace name — see .

A persistent subtle `⇅` (U+21C5, dimmed) shows while this machine allows remote control (`remote.enabled`, FR-009a). While a remote peer controls any window, a prominent accent segment names the controller(s) and window counts (e.g. `laptop-2 controls 1 window`, FR-009b) built by , with an account-naming tooltip from . Both fields live in  on .

The controller list is fed by polling the local server's window list every  (2s) while `remote.enabled`:  issues `::ListWindows`, and  caches the windows with `controller = Some` from the `::LocalWindowList` reply. Because the list comes from the server, it covers remotely-created windows that never had a local client (SC-006). Controlling-side (remote-dialed) windows suppress both surfaces, and disabling remote clears the cache via .

Feature 014 (T025) adds a transport indicator on the CONTROLLING side: a controlling-side window carries the  it dials over on `::remote_transport`, and `::remote_transport` renders a persistent right-side `⇅ Local network` / `⇅ Tailscale` segment via , so the user always sees which path a controlled window uses (FR-009). An owning or ordinary local window leaves it `None` and renders nothing.

Feature 015 adds a shared-window presence badge on the connecting side, fed by the roster: while a  has two or more participants, a  on  renders a badge naming the attached count and current holder () with a device/account tooltip (), so every participant sees who is attached and who is driving (FR-008). It clears when the share drops to a single machine.

## System Stats

The  refreshes every 2 seconds via sysinfo. CPU and network history are kept in rolling buffers (8 and 4 entries respectively) for sparkline rendering. GPU detection on Linux reads AMD sysfs or NVIDIA sysfs/nvidia-smi.

On Linux, network throughput prefers default-route interfaces from  before falling back to all non-loopback interfaces. This avoids double-counting Docker bridge and veth traffic in the status bar.

## Dialogs

In-app GPU-rendered overlay dialogs for confirmations, updates, and context menus.

### Close Dialog

An in-app GPU-rendered confirmation dialog with three buttons: Quit Scribe, Kill Window, and Cancel. Both destructive actions wait for a server acknowledgment before anything is torn down.

The two are scoped differently once the ack lands: Quit Scribe ends the whole client, Kill Window destroys the one window it was raised on and leaves its siblings running. See [[client#Client#GPUI Window Lifecycle]].

When a PTY exit removes the last remaining pane in a window, the client reuses that same permanent-close flow instead of leaving an empty workspace shell on screen.

### Update Dialog

Shows update-install and restart-required confirmations in a shared overlay, opened from the command palette or the centered status-bar CTA.

The update notification appears in the compositor window title rather than in the tab bar. Stable windows use `Scribe`, while `scribe-dev` windows use `devScribe`, yielding titles such as `devScribe - v{version} available - click below to update` when the centered bottom status-bar CTA is clickable and `devScribe - v{version} available` otherwise. If installation finishes with `CompletedRestartRequired`, the same overlay switches to a `Continue` / `Cancel` cold-restart prompt and the centered status-bar label stays clickable as `Updated! Restart required` so canceling does not strand the user.
Approving that deferred restart spawns a detached helper mode of the client binary, then sends `QuitAll` so all client windows flush their restore snapshots and exit. The helper waits for those processes, performs the platform cold restart, and launches one fresh client so normal cold-restore fan-out recreates the remaining windows.

### Context Menu

Right-click overlay with Copy (if selection active), Paste, Select All, Open URL (if hovering a URL), and Open File (if hovering a path). Items are rendered as GPU quads with hover highlight.

### Paste Confirmation Dialog

An opt-in confirmation that gates a risky paste before any byte reaches the PTY (spec 011). Off by default via `terminal.paste_confirmation`; fires only when the focused pane has not enabled bracketed paste.

 is a pure classifier returning  — `Some` when the text has a line break (`\n`/`\r`) or a non-tab control/escape byte (C0 except tab/LF/CR, DEL, C1), else `None`. The gate in  consults it after `prepare_paste_target`, and only when `terminal.paste_confirmation` is set and the target is unbracketed; the disabled flag short-circuits first so an off configuration takes the exact prior path (zero added cost). Keybinding and context-menu paste both flow through `send_paste_data` (gated); middle-click `perform_primary_paste` now routes through it too, gaining the gate and >4 KiB chunking. Drag-and-drop file insertion and the context-menu "Run command" action use  so they stay ungated (FR-013).

 is a sibling of the close/update/clipboard dialogs sharing the same `DialogLayout` / `DialogRenderer` / `CellInstance` pipeline. It parks the raw paste text plus the resolved  and shows a reason line (line count / control-character count) above a caret-escaped preview — control bytes render as `^[`, `^M`, or `\u{NN}` so the preview can never drive the terminal (FR-005 / SC-008). Two buttons: **Cancel** (index 0, default focus, also Esc) and **Paste** (Enter on focus; Tab cycles). On Paste,  delivers the parked bytes verbatim through the shared  tail (byte-identical to the disabled path), or drops safely if the parked session has closed; the parked decision is honored even if the setting is toggled off while the dialog is open. Cancel sends nothing. No protocol/IPC change — the content and the bracketed-paste signal are both client-side.

## URL Detection

The  scans visible terminal content for URLs (https, http, ftp, file, mailto, ssh, and telnet schemes) and file-system paths.

Soft-wrapped rows are joined by `WRAPLINE` before scanning so a link split across terminal rows remains one clickable span. Trailing punctuation is stripped respecting bracket pairs. Detected spans are cached and invalidated on content change. Each span carries a `SpanKind` (`Osc8Hyperlink`, `Url`, or `Path`). OSC 8 hyperlinks take precedence over heuristic URL/path detection on overlapping cells (see  below).

Every span also carries exact per-row geometry as s — spans are not rectangles, because a hard-break continuation row starts at its indent rather than column 0 and merged OSC 8 runs can have partial middle rows. Hit-testing (`contains_cell`), OSC 8 masking, and the hover underline all consume segments; the bounding `row`/`col_start`/`row_end`/`col_end` fields remain for identity comparison and ordering.

URL highlighting and the pointer cursor are only shown while the Ctrl modifier is held. The `ModifiersChanged` handler triggers a redraw and cursor update so visual feedback is immediate. Only the clickable span under the cursor is underlined; wrapped spans draw one underline segment per row (one quad per `RowSegment`). Ctrl+click opens the span via `xdg-open` on Linux or `open` on macOS. File paths support an optional `:N` line-number suffix; when present, `code --goto path:N` is tried first and `xdg-open` is the fallback. Relative paths are resolved against the pane's OSC 7 CWD, and `~/` is expanded using `$HOME`.

### Ctrl+Click in the GPUI Client

The GPUI shell's whole side of the surface: the hover rule, the pointing-hand cursor, and the click that opens. The detector above was ported with the client, but nothing called it — the feature was dead from the rebuild until this wiring.

[[crates/scribe-client/src/terminal.rs#DisplayOnlyTerminal#link_at]] is the seam, a sibling of the smart-selection lookup for the same reason: [[crates/scribe-client/src/url_detect.rs#PaneUrlCache]] needs the live `Term` this type owns. The scan is lazy and the cache is invalidated from [[crates/scribe-client/src/terminal.rs#DisplayOnlyTerminal#make_content]] — the one place every path that can move a visible cell already funnels through — so an idle pane pays nothing and a pointer resting on a cell pays one scan. Rows are mapped by the plain `row - display_offset` the scanner itself uses rather than by the split-scroll-aware [[crates/scribe-client/src/terminal.rs#DisplayOnlyTerminal#selection_point]], and a pinned viewport offers no links at all: the scanner has no notion of the pin either, so hit-testing against it would only rule the wrong cells.

One [[crates/scribe-client/src/terminal.rs#HoveredLink]] serves both consumers, so the rule can never cover a span a click would not follow. The render pass reads the pointer and the modifier straight off the `Window` instead of tracking hover state — both are already live there, so a hover that survives a repaint needs no state of its own to survive with it — and the result rides to the focused pane in [[crates/scribe-client/src/main.rs#FocusedPanePaint]] alongside the other three things only that pane gets. [[crates/scribe-client/src/terminal_element.rs#TerminalElement#paint_link_underline]] rules it one quad per row segment in each cell's own resolved foreground, sharing the resolved-cell bundle with the selection overlay so neither can draw against a different view of the cells; the pane wears `cursor_pointer()` for as long as the rule is drawn.

Inside `render_panes` the rule joins the three things that were already focused-pane-only — the recorded bounds, the find matches, the mouse selection — in one branch rather than a fourth repetition of `if placement.focused`, and it is `take`n out of an `Option` exactly as the pinned snapshot and the window's single IME slot are. The `with_cursor` / `with_scrollbar` / `with_ime` builders take the `Option` their fields already hold, so a pane that gets none of them says so in the chain instead of guarding each call.

Two repaints have to be asked for, because an idle pane bumps no generation and the redraw pump would never come round on its own. Ctrl going down or up is caught by an `on_modifiers_changed` listener on the *focus root* — gpui dispatches modifier changes down the focus path like key events, so a listener on the hovered-but-unfocusable grid band is never reached — and pointer motion notifies from [[crates/scribe-client/src/main.rs#TerminalView#move_over_grid]], gated on Ctrl so an ordinary move over the grid still costs nothing.

[[crates/scribe-client/src/main.rs#TerminalView#press_opens_link]] sits between the scrollbar and mouse reporting in [[crates/scribe-client/src/main.rs#TerminalView#press_grid]]: a modifier chord on a detected link is aimed at the terminal, not at the program in it. The gate is deliberately narrow — with no link under the pointer the press falls straight through, so Ctrl+click keeps working for programs that use it. A consumed press parks `GridDrag::Link` so the matching release is swallowed rather than handed to a tracking application that never saw the press, and so a drag afterwards paints no selection.

[[crates/scribe-client/src/main.rs#TerminalView#press_opens_link]] keeps the three kinds apart: an OSC 8 URI is program-supplied and goes through the scheme-allowlist gate that can raise the confirmation dialog, a heuristic URL keeps the silent non-allowlisted drop, and a path is not a URI at all. The same detector fills the right-click menu's open rows through [[crates/scribe-client/src/main.rs#TerminalView#open_context_menu]], which used to hand `ContextMenuRequest` a hardcoded `https://example.com/spec` for every cell — so every right-click offered to open it. Those rows now appear only over a cell that carries a link, which is why `overlay-actions.sh` addresses its target row two rows higher than it used to.

### Resolving a Path Link

What a relative link in a pane is relative *to*: that pane's shell, never the client process. [[crates/scribe-client/src/url_detect.rs#resolve_path]] is the pure half of [[crates/scribe-client/src/url_detect.rs#open_path]], split out so the rule is unit-tested without spawning anything.

The CWD comes from [[crates/scribe-client/src/main.rs#TerminalView#focused_cwd]] — the server-reported OSC 7 directory on the focused pane, which the server also synthesises from `/proc/<pid>/cwd` when a shell sets only its title. Without one the path is handed over unresolved rather than resolved against a guess: the OS handler failing on a relative path is a visible, correct failure, where silently opening the wrong file is not.

The `./` the detector matched on comes back off, because the string reaches `xdg-open`, `code --goto`, and the log line. `std::path::absolute` does that removal — it drops `.` components and deliberately leaves `..` alone, which is the behaviour wanted here: collapsing `..` lexically names a different file whenever a symlink sits on the path, and the OS resolves it correctly anyway. It is only ever called on an already-absolute path, since on a relative one it would resolve against this process's directory — the one answer the function exists to avoid.

### Hard-Break Continuation

Joining a URL split by a program-side line break, where no `WRAPLINE` flag connects the rows and the logical-line join cannot fire.

Programs that lay out their own text (Claude Code's Ink renderer, pagers, shell line editors) hard-wrap long URLs with an explicit newline instead of letting the terminal soft-wrap, so without this pass the URL is detected truncated at the break.

The URL scanner joins across hard breaks: whenever a heuristic URL match is the last content on its row (only blank filler follows it to the end of the logical line),  fetches the logical line below and consults  — the join policy — with a `HardBreakContext` (the break column vs the grid's last column, the broken row's cells, and the full next line). The policy returns the column where the URL body resumes (cells before it, e.g. a program-drawn gutter indent, stay outside the span) or `None` to refuse. Joins are capped at `MAX_HARD_JOIN_ROWS` explicit breaks; soft-wrapped rows inside a continuation line do not count against the cap. A continuation never absorbs cells covered by an OSC 8 span (FR-004) — the appended run is cut at the first such cell.

The policy is modelled on kitty, the only major terminal that bridges hard breaks by default (its `url_excluded_characters` docs allow newlines "to accommodate programs such as mutt"; mid-row breaks were declined in kitty#2927): a break is bridged only when the URL ran **exactly to the terminal edge** and the next row resumes with URL characters at column 0. iTerm2 offers the same behaviour opt-in (`ignoreHardNewlinesInURLs`, default off); Alacritty, WezTerm, VTE/GNOME Terminal, Windows Terminal, and Konsole all treat a hard end-of-line as an absolute barrier (their search haystacks insert `\n` exactly at non-wrapped row ends), with OSC 8 as the ecosystem's sanctioned producer-side fix. Scribe extends the kitty rule in three guarded directions: a continuation behind a program-drawn gutter (e.g. Claude Code's banner bar `▏ `) is accepted when the broken row carries the identical gutter run (); a pure-space table-alignment indent is accepted up to a separate 32-column cap even when the broken row has content at that prefix (); and a next row that starts its own scheme prefix is a new link, never a continuation. The admitted false-positive class — a flush-to-edge URL followed by a column-0 word joins — is exactly kitty's default behaviour; kitty's opt-out precedent (`url_excluded_characters "\n"`) is the model if a config toggle is ever wanted.

Cells consumed as continuation tails are masked from later line scans () so a joined tail such as `articles/15424964` is not re-matched as a fresh bare path on its own row.

### Explicit Hyperlinks

OSC 8 explicit hyperlinks are surfaced as a distinct `SpanKind::Osc8Hyperlink` so the displayed text can be inspected separately from the destination URI. Parsing lives upstream in `alacritty_terminal`'s VTE Perform impl.

 is the client-side discovery pass: it iterates the visible grid and emits `UrlSpan { kind: Osc8Hyperlink, url, .. }` values from contiguous cells sharing the same hyperlink. Same-id, same-URI runs that restart within one row merge while retaining exact per-run `RowSegment` geometry, so unlinked gutter or blank filler does not break hover coverage. The pass runs **before** the heuristic pass; the heuristic pass then skips cells already covered by an OSC 8 span (FR-004 precedence) but continues to function unchanged for cells outside one (FR-014).  returns `Osc8Hyperlink` before `Url` and `Path` on overlap.

The `id=` parameter reconnects same-URI multi-segment hyperlinks separated by unlinked cells when their runs are adjacent or on consecutive rows, including wrapped lines and program-side hard breaks. A later or different-URI run remains separate; anonymous same-URI links also remain separate because upstream gives each open its own id.

URI length is capped at 2048 bytes (FR-010). Upstream VTE (in the `std` build Scribe uses) does not cap OSC sequence length, so Scribe applies the cap in `scan_osc8_hyperlinks` itself; URIs longer than the cap are treated as absent and the affected cells fall back to the heuristic detector.

### Hyperlink Hover Tooltip

When the cursor settles on a cell carrying an OSC 8 URI for ≥300 ms with no movement, the verbatim URI is rendered through the existing  above or below the cell.

 is the render-loop hook. `Position::Below` is preferred and flipped to `Above` when the cell is on the bottom row. The URI is cached on `App.hover_tooltip_uri` at dwell-threshold time so subsequent frames render without re-reading `cell.hyperlink()`. Truncation only affects what is *displayed* — long URIs render with a **head + tail** view (`prefix...suffix`) via `osc8_tooltip_truncate` so domain-confusion suffixes stay visible to the user; the full URI is preserved on the span for activation. The dwell state lives on `App.hover_cell` / `hover_started_at` / `hover_tooltip_visible` / `hover_tooltip_uri` and resets whenever the cursor moves to a different cell or leaves the terminal area.

Cells without an OSC 8 hyperlink never trigger the dwell path, so the tooltip does not surface heuristic URLs (those continue to use the established Ctrl+highlight affordance only).

### Disallowed-Scheme Confirmation

Activation of an OSC 8 hyperlink whose scheme is outside the existing outbound allowlist routes through a confirmation dialog instead of opening unprompted or being silently blocked.

 is a sibling of the close and update dialogs that shows the URI (head+tail truncated for long URIs so the tail stays visible) plus a "scheme normally blocked" warning. **Cancel** (default focus, also bound to Esc) dismisses without opening; **Open Anyway** routes the URI through `url_detect::open_uri_unguarded`. Allowed-scheme hyperlinks bypass the dialog entirely so the common-case activation latency is unchanged.

 is the single helper that performs the allowlist branch and emits a `tracing::debug!`/`tracing::info!` line per decision. The OSC-8 activation paths funnel through it via the dedicated `ContextMenuAction::OpenOsc8Url` variant (right-click "Open URL" item and smart-selection `OpenUrl` rewriting) plus the Ctrl+click handler. Heuristic-URL `ContextMenuAction::OpenUrl` actions continue to flow directly through `url_detect::open_url` so the pre-009 silent-drop behaviour for non-allowlisted heuristic schemes is preserved.

### Clipboard Dialog

OSC 52 read and write requests issued by PTY-side programs surface through an in-app confirmation dialog whose chrome mirrors the disallowed-scheme dialog verbatim (spec 010 / ).

 is a sibling of the disallowed-scheme dialog rendered through the same `DialogLayout` / `DialogRenderer` / `CellInstance` quad pipeline. It is opened by the `ServerMessage::ClipboardPromptRequest` handler on `App` and dismissed when the user chooses an action; the choice is forwarded to the server as `ClientMessage::ClipboardPromptResponse` carrying the matching `PromptId`. Wave 4 surfaces all four buttons in a single row, left to right: **Deny once** (default focus, also bound to Esc), **Always deny**, **Allow once**, **Always allow**. Tab cycles forward across all four; Shift+Tab cycles back. The two `Always*` variants both resolve the single in-flight prompt AND persist the corresponding policy axis to disk:  loads the current `ScribeConfig`, writes `terminal.clipboard.read_mode` or `terminal.clipboard.write_mode` (per the dialog's `op`) to the matching `"allow"` / `"deny"` value, and saves the config back through `scribe_common::config::save_config`. The on-disk change is observed by the server's existing config-file watcher and rebroadcast as `ConfigReloaded`, which lands as a `ClipboardCommand::RefreshPolicy` in every live PTY-reader task; the server-side prompt-response handler also mutates its in-memory policy snapshot immediately so the next OSC 52 op outside the burst window sees the persisted mode without waiting for the file-watcher round-trip.

The body copy varies by op: reads show "A program in this terminal wants to read the clipboard"; writes show the same lead together with a head-and-tail truncated payload preview built server-side per FR-006. Selection target (`clipboard` vs `primary selection`) is mentioned inline so the user can tell `c` from `p` requests apart.

The host clipboard bridge backing the dialog reuses the same `arboard::Clipboard` handle already wired into `App` for user-driven copy / paste (research decision 3 /  and `#App#bridge_write`). The bridge is policy-agnostic — the read / write modes live server-side — and silently fails on `arboard` error per UX-002, collapsing onto a `BridgeError` value the server maps to an empty OSC 52 reply.

Wave 5 wires the selection-target branch and the FR-019 opt-in focus gate. When the incoming `ClipboardSelection` is `Primary` on Linux, the bridge routes through arboard's `GetExtLinux::primary_clipboard` / `SetExtLinux::primary_clipboard` extension traits so X11 reads and writes hit the primary selection rather than the system clipboard; on Wayland the same extension call falls back through arboard's internal mapping, and on macOS / Windows the `#[cfg(target_os = "linux")]`-gated arm is removed so primary collapses onto the regular `get_text` / `set_text` system-clipboard call (spec Assumptions). `bridge_write` additionally consults `self.config.terminal.clipboard_policy.focus_gate_writes` — when the toggle is on and `self.focus.window_focused` is false, the bridge returns `Ok(())` without touching the host clipboard so a background PTY-side program cannot hijack the clipboard while another application holds focus. The toggle defaults off and lives on the client because window-focus state has no synchronous server-side view (research decision 6). The flag is read straight off `App::config`, which the existing config-file watcher refreshes on every save via `handle_config_changed`, so the next OSC 52 write after a settings change already sees the new value without a dedicated IPC variant.

### Copy Hyperlink Address

A right-click on an OSC 8 cell adds a "Copy hyperlink address" entry to the context menu — distinct from "Copy" which copies the displayed text selection unchanged.

The new action variant is `ContextMenuAction::CopyHyperlinkAddress(String)` and the dispatch writes the verbatim URI to the system clipboard via the same path `ContextMenuAction::Copy` uses for text selections. The regular "Copy" path on a selection spanning a hyperlink is unchanged — selecting text inside a hyperlink and copying still yields the displayed text, never the URI.

 gains an `osc8_uri: Option<String>` field that the menu builder uses to decide whether to append the new item and whether the "Open URL" item emits `OpenOsc8Url(uri)` (OSC 8 origin, routes through the allowlist gate) or `OpenUrl(uri)` (heuristic origin, preserves the pre-009 direct-open behaviour).

### Replay-Scrollback Limitation

Hyperlinks reconstructed from `SessionReplay` (zero-downtime hot reattach and cold-restart restore) do **not** carry OSC 8 URIs. *Live* hyperlinks emitted by the PTY after the reattach completes work without regression.

's `snapshot_to_ansi` emits cell characters and SGR style only, with no OSC 8 open/close around hyperlinked runs. Live (post-reattach) hyperlinks ride the normal PTY-output byte path and reach the client-side VTE which populates cells as usual.

Extending `snapshot_to_ansi` to re-emit OSC 8 open/close around hyperlinked runs is the documented follow-up improvement path; it would require a `SessionReplay` byte-format / version bump and is out of scope for the 009-osc8-hyperlinks spec.

## Clipboard Cleanup

When copying from a supported AI coding session,  applies dedent, blockquote normalization, decorative-prefix stripping, then unwrap.

Copy actions decide whether cleanup is active through , which requires both an AI provider — via  (tracker-detected AI state or an AI launch binding on the pane) — and that the pane is **not** on the alternate screen. The provider check keeps cleanup enabled for newly opened Claude Code and Codex tabs before their first hook event arrives; the alternate-screen exclusion disables it for fullscreen TUIs (e.g. Claude Code's fullscreen renderer), whose grid is arbitrary content rather than AI-chat markdown, so a `Shift`-selected copy is taken raw instead of being mangled by the cleanup transforms.

Dedent strips minimum shared leading whitespace. Blockquote normalization removes markdown `>` markers and the rendered `▎` gutter used by some AI UIs so quoted prose copies as plain text. Decorative-prefix stripping removes leading AI status glyphs such as `●` when followed by whitespace. Unwrap then joins hard-wrapped prose at auto-detected wrap width. When no dominant width is detected but at least one line exceeds 40 characters,  joins consecutive non-break lines as a fallback. Structural breaks like bullets, headings, code blocks, and tables are preserved after quote markers and decorative prefixes are removed.

## Window State

Per-window geometry is persisted under the active install flavor's XDG state root via .

Stable installs use `$XDG_STATE_HOME/scribe/windows/{window_id}.toml`, while `scribe-dev` uses `$XDG_STATE_HOME/scribe-dev/windows/{window_id}.toml`. `Kill Window` and a natural exit of the last remaining terminal both remove the file only after the server confirms the window was destroyed.

Additional windows are separate `scribe-client --window-id` processes spawned by . The parent keeps a lightweight wait thread via  so closed child windows do not remain as zombies. Startup timing logs from , , and session-list handling expose whether delays come from config, window/GPU setup, renderer/font atlas setup, IPC, splash gating, or session creation.

All geometry (position and size) is stored and restored in **logical coordinates** so windows scale correctly on HiDPI/Retina displays. `capture_window_geometry` converts physical pixels to logical using `window.scale_factor()`, and `apply_window_geometry` restores via `LogicalSize`/`LogicalPosition`. Position is stored as Optional since Wayland does not expose window positions. Size is always restored via `request_inner_size` — even for maximized windows — so the GPU surface and pane grids have reasonable pre-configure dimensions on Wayland where `inner_size()` can return a tiny default before the compositor responds. The window is created with an initial 1200×800 logical-pixel hint for the same reason. Maximized state is set after size, and restart-time restore treats size-only or monitor-only records as persisted geometry instead of requiring X11 coordinates.

`apply_window_geometry` returns whether the saved geometry was within the safe range and was actually applied; callers that need to reason about the eventual viewport (cold-restart replay) read the applied geom rather than `window.inner_size()` because both `request_inner_size` and `set_maximized(true)` are async on most compositors and may not yet be reflected when the next synchronous step runs.  converts a saved `WindowGeometry` plus the current `scale_factor` into the physical inner size the window will settle on, so PTY grids and `CreateSession` sizes match the eventual rendered viewport instead of the pre-restore startup hint.

`state` records how the window was displayed as a [[crates/scribe-client/src/window_state.rs#WindowState]] — windowed, maximized, minimized, or fullscreen — replacing the `maximized: bool` that could express neither of the last two. A minimized record also carries `restore_state` (what unminimizing returns to) and every non-windowed record carries `restore_rect`, the pre-maximize/pre-fullscreen rect: the window's own bounds are the work area once it is no longer windowed, and GPUI documents `WindowBounds::Maximized`/`Fullscreen` as taking the *restore* size. EWMH 5.7 also makes restoring the pre-fullscreen geometry the window manager's job, which it can only do if handed that rect. Records written before the enum existed carry `maximized = true|false`; [[crates/scribe-client/src/window_state.rs#WindowRegistry#load_saved]] folds the bool into the state on load and never writes it back. The fold cannot live inside [[crates/scribe-client/src/window_state.rs#normalize_legacy_geometry]] because that short-circuits on `titlebar_normalized`, which records from the intervening client already set.

Reading the live state needs the window manager, not GPUI: GPUI exposes no `is_minimized()`, and its X11 `is_maximized()` is `!hidden && maximized_vertical && maximized_horizontal`, so minimizing a maximized window and quitting persisted "windowed" and brought it back unmaximized. [[crates/scribe-client/src/monitor.rs#observed_window_state]] reads EWMH `_NET_WM_STATE` over the same X11 connection the module keeps for RandR, falling back to GPUI's own answer off X11. `_NET_WM_STATE_HIDDEN` is WM-owned — EWMH 5.7 says it "is a function of some other aspect of the window such as minimization" — so it is only ever read, never written.

`_NET_WM_STATE_HIDDEN` is the *only* signal read for minimization; ICCCM `WM_STATE == IconicState` used to be OR'd in beside it as a pre-EWMH fallback and no longer is. ICCCM 4.1.3.1 makes `IconicState` mean only "not currently mapped", which is equally true of a window on another virtual desktop — something `apply_saved_desktop` now arranges on every restore that names one. Reading that as minimization latched: the capture persisted `minimized`, the next restore obediently re-minimized the window, and its capture read `IconicState` again, so a window hidden once for any reason never came back and no restart cleared it. EWMH is specific where ICCCM is not — HIDDEN means the window "would not be visible on the screen if its desktop/viewport were active" — so it is the one of the two that answers the question actually being asked. The cost is that a pre-EWMH window manager publishing no `_NET_WM_STATE` at all loses minimize detection and keeps GPUI's answer, which is the same thing that already happens off X11. Restoring minimization goes the other way: ICCCM's "map me iconified" is the pre-map `WM_HINTS.initial_state`, which GPUI's in-`Window::new` map puts out of reach, so `TerminalView::apply_pending_minimize` issues `Window::minimize_window` (the ICCCM `WM_CHANGE_STATE` message) from the first frame instead, after the saved position. The window therefore flashes visible for one frame on the way down.

`monitor_name` records which monitor the window was on, so a restore that lands elsewhere can be recognised as such. GPUI's X11 display `uuid()` is a hardcoded nil placeholder, so [[crates/scribe-client/src/monitor.rs#persisted_monitor_name]] resolves the identity itself: the RandR connector name (`DP-4`) of the monitor containing the window, falling back to GPUI's display UUID where it is real (a stable v5 hash of the output name on Wayland, the CGDisplay UUID on macOS), then `None`. The nil-UUID placeholder ([[crates/scribe-client/src/window_state.rs#NIL_MONITOR_ID]]) that v0.1.0 cutover clients wrote is not a connector name and can never match one, so the post-move landing check skips it rather than warning on every such start; those records self-heal to `RandR` names on the next capture.

`desktop` records the virtual desktop the window was on, as EWMH `_NET_WM_DESKTOP`, with `0xFFFFFFFF` meaning "all desktops" — without it a window that lived on desktop 3 came back on whichever desktop happened to be current. [[crates/scribe-client/src/monitor.rs#window_desktop]] reads it off the window over the same X11 connection as the rest, and it is `None` off X11 and wherever the window manager publishes no desktop, which is also what a single-desktop window manager looks like. Restoring it is the same shape as the minimize restore: EWMH 5.5 says the window manager "should honor `_NET_WM_DESKTOP` whenever a withdrawn window requests to be mapped", a pre-map property that GPUI's map inside `Window::new` puts out of reach, so [[crates/scribe-client/src/monitor.rs#apply_saved_desktop]] sends the post-map `_NET_WM_DESKTOP` client message to the root instead — the same client-message path the placement move uses, gated on the atom appearing in `_NET_SUPPORTED`. The window therefore appears on the current desktop for one frame before it is sent away. `TerminalView::apply_saved_desktop` issues it after the position move because a window on another desktop is unmapped and stops painting: the move has to be out before the window leaves, and the landing check the restore is waiting on only completes when the user returns to that desktop.

`zoom` records the font zoom level the window was last at, and it is the one restored property that is not geometry: every other window state came back across a quit while a deliberately zoomed grid dropped to the configured size. What is stored is the LEVEL — the signed point delta [[crates/scribe-client/src/zoom.rs#ZoomState]] holds — never the resulting size, so a later `appearance.font_size` edit still rebases the delta through [[crates/scribe-client/src/main.rs#zoomed_font]] instead of being overridden by a size captured against the old base. [[crates/scribe-client/src/main.rs#TerminalView#opening_font]] applies the record's level before the first frame so the window never paints at the configured size on its way to the restored one, and [[crates/scribe-client/src/main.rs#TerminalView#adopt_assigned_geometry]] applies it for a process that only learns which window it is holding from `Welcome` — without that second call the next capture would write level 0 over the adopted window's record. [[crates/scribe-client/src/zoom.rs#ZoomState#at_level]] re-clamps whatever is read back into the `-7..=7` range the live steps saturate at, because the record is a TOML file a user can put any `i8` in. This is a deliberate divergence from the legacy client, which reset zoom on every start; the zoom-reset action remains the escape hatch for a level the user cannot read.

The saved rect itself is reconciled with the live layout by [[crates/scribe-client/src/window_state.rs#clamp_geometry_to_layout]], against the work areas [[crates/scribe-client/src/monitor.rs#connected_monitors]] enumerates. This used to be a gate that dropped `x`/`y` when the record named a monitor that was verifiably gone, which both handed the window back to the window manager's default placement and kept a size the remaining screen could not hold — a 3840-wide window reopening on a 1920-wide screen, with whatever the window manager settled on persisted afterwards. Clamping is what toolkits do instead, and what Zed is missing in zed-industries/zed#12521 and #47231: the window comes back as close to where the user left it as the layout allows, and reachable.

A rect that still touches a work area is clamped into the union of the areas it touches, so a window deliberately spanning two monitors keeps spanning them and only an oversized one is shrunk — that union is also what keeps the v0.1.x upgrade regression (every nil-UUID record losing its position and side-by-side windows re-opening stacked) from returning, since a window already on screen is left exactly where it is. A rect that touches nothing is moved onto the monitor nearest its centre and adopts that monitor's name, so `verify_restored_position` does not report the deliberate move as the window manager picking the wrong screen. An empty monitor list (macOS, pure Wayland, no RandR) means nothing is verifiable and the record is returned untouched.

The clamp target is the **work area**, not the whole monitor: `connected_monitors` intersects each RandR rect with EWMH `_NET_WORKAREA`, so a clamped window clears panels and docks instead of landing under them. `_NET_WORKAREA` is screen-wide rather than per-monitor, which makes the intersection exact for a panel spanning a screen edge (the usual case) and merely conservative for one covering a single monitor; a window manager that publishes no usable rect leaves the full monitor rect in place.

The origin recorded beside it is only stored when the platform reports one. Wayland hides window positions and GPUI answers `(0, 0)` for every window there, so a capture that read the origin straight out of the bounds persisted that fake corner as if the user had put the window in it — and the layout clamp cannot catch it, because off X11 the connected-monitor list is empty and an empty list means "unverifiable, keep the record". [[crates/scribe-client/src/window_state.rs#geometry_from_bounds]] therefore takes an `Option` origin instead of reading it off the bounds, so the decision cannot be silently dropped, and [[crates/scribe-client/src/monitor.rs#window_origin_is_exposed]] makes it: on Linux, holding an X11 window id (X11 is the only Linux backend with an origin, and the handle check is free next to the `RandR` round trips); true everywhere else, since macOS and Windows report a real one. A record captured without an origin restores at the default placement rather than in the screen corner.

### Cold Restart Restore Store

The  persists logical window state for cold restart recovery under `$XDG_STATE_HOME/{flavor}/restore/`.

A debounced save runs after every layout change via `report_workspace_tree`, snapshotting workspace splits, tabs, pane trees, and per-pane launch bindings. Restore directories are hardened to `0700`, and snapshot, index, lock, and temporary files are written as `0600` because launch bindings can include prompt text and provider conversation IDs. The client writes the per-window snapshot file before adding that window ID to the shared restore index, so a failed snapshot write cannot leave a dangling index entry. Empty snapshots with no replayable tabs or launches are not persisted; if an empty server starts with only those stale entries, startup falls back to a fresh session instead of replaying a blank window forever. On startup with an empty `SessionList`, the bootstrap client atomically claims the first replayable entry from the restore index and rebuilds the layout via , then creates sessions for each saved pane. Before replay, the client reapplies geometry from the claimed snapshot's original window ID because a true cold restart connects to a fresh server that has already assigned a new window ID in `Welcome`. The geometry that was actually applied is also threaded into  so the replay sizes pane grids and the initial `CreateSession` from  rather than `window.inner_size()`; without this, maximized windows created PTYs at the 1200×800 startup hint and stayed undersized for the lifetime of the session because the corrective resize from the eventual `WindowEvent::Resized` is dispatched while panes still hold placeholder session IDs that the server cannot match. If more saved windows remain, it spawns fresh `--restore-child` client processes; each child claims exactly one additional entry and never fans out again. The claim path scans the remaining index entries for readable per-window files and drops stale IDs before deciding how many child windows to launch, so partially missing restore files cannot fan out duplicate blank windows. Explicit close or quit clears the snapshot and sets `quit_restore_cleared` so the subsequent server-disconnect event does not re-save it; server crash preserves it. Restore is skipped when the client was launched with `--window-id` (i.e. spawned as a new window by an existing client) to prevent claiming a live window's snapshot.

Claiming is non-destructive ([[crates/scribe-client/src/restore_state.rs#RestoreStore#claim_first_window]]): the claimed id moves to the index's `claimed` list — never claimable again, so a crash loop cannot double-replay the same snapshot — while the file itself survives as the window's last good layout. It disappears only when a replacement is durably on disk: a fresh snapshot save clears the claim through [[crates/scribe-client/src/restore_state.rs#RestoreStore#upsert_index]], and the claiming window retires the old file after its first successful post-claim flush (or on an explicit quit or close). Non-replayable and unreadable entries are still pruned at claim time. A reconnect that finds the server kept its sessions therefore no longer destroys the last good snapshot — the failure mode that lost layouts in the v0.0.23→v0.1.0 update, where one reconnect both flattened the window and deleted the only snapshot of the real layout.

AI panes persist `conversation_id` via hook events that include provider conversation IDs from hook JSON payloads.  preserves an existing non-None `conversation_id` when subsequent state updates omit it, ensuring hooks without conversation access do not erase the tracking ID. When the tool later emits `AiStateCleared`, the pane's launch binding is demoted back to `shell` before the next snapshot so a normal shell tab that temporarily ran an AI CLI does not cold-restart back into `--resume`. On replay, panes with a `conversation_id` launch the provider's targeted resume command (`claude --resume <id>` or `codex resume <id>`); those without fall back to the generic resume picker.

Prompt bar state rides the same [[crates/scribe-client/src/restore_state.rs#LaunchRecord]] rather than a separate store: `LaunchRecord` flattens the pane's [[crates/scribe-common/src/protocol.rs#SessionPromptState]] (`first_prompt`, `latest_prompt`, `latest_prompt_at`, `latest_prompt_finished_at`, `prompt_count`) so each field defaults individually for snapshots written before it existed. [[crates/scribe-client/src/restore_replay.rs#queue_from_launch_record]] carries that state into the replayed [[crates/scribe-client/src/restore_replay.rs#PaneRestore]] via [[crates/scribe-client/src/prompt_bar.rs#PromptBarData]] so the bar appears immediately after a cold restart, with the timer read back as a frozen or still-ticking value depending on whether `latest_prompt_finished_at` was set. The replayed pane's conversation tracking is seeded from the same record: `queue_from_launch_record` reads the launch's `conversation_id` (present only for an `Ai` launch kind) into `PaneRestore::last_conversation_id`, so [[crates/scribe-client/src/main.rs#AiChrome#note_conversation]] sees the resumed provider re-announce the id it was already given rather than reading it as a conversation switch that would retire the just-restored prompt history. Hot-restart reattach against a surviving server does not go through this path — it seeds prompts and the conversation id straight from `SessionList` via [[crates/scribe-client/src/main.rs#AiChrome#seed_from_session_list]] instead.

## Config Watching

A file watcher in  monitors the active install flavor's config root.

Stable installs watch `$XDG_CONFIG_HOME/scribe/` on Linux and `~/Library/Application Support/Scribe/` on macOS; `scribe-dev` uses the corresponding flavor-specific directory. The watcher forwards `ConfigChanged` through the event loop proxy for `config.toml`, theme changes, and on macOS the watched root directory itself, because the `notify` FSEvents backend may report only the directory that must be rescanned after a save. On reload the client reapplies the renderer theme when the preset name changes, when the inline `[theme]` values change under `custom`, and while an external theme file is selected so file edits repaint immediately.

### GPUI Config Port

The GPUI rebuild reproduces the config watcher and runtime-reload semantics against the frozen `scribe-common` config surface, keeping TOML format, flavor config dirs, inline `[theme]`, and removed-key tolerance identical to the winit client.

 watches the active flavor's  and invokes a caller-supplied closure (instead of a winit `EventLoopProxy`) on each relevant modify/create event; relevance is decided by , a byte-for-byte port of the legacy filter (`config.toml`, `themes/`, and the macOS FSEvents directory rescan).  bundles the parsed , the resolved  and its derived  (via ), and the parsed .

 swaps in a freshly parsed config and returns a  naming which live surfaces changed — theme, font metrics, or opacity — mirroring the legacy `ConfigReloadPlan` heuristics (, `font_params_changed`). Theme, chrome colors, and keybindings are always recomputed so a saved edit reapplies without a restart; the plan lets the caller skip redundant reapply work. Removed appearance keys deserialize inertly because `ScribeConfig` uses serde defaults and models no `deny_unknown_fields`, so the GPUI paint path never observes them.

#### Terminal Window Reload Wiring

The terminal window owns a  for its whole lifetime, which is what actually turns a saved config edit into a repainted window with no restart.

`notify` delivers its callback on its own thread, which must never touch GPUI entities, so the watcher only bumps a  — an atomic generation counter. The GPUI foreground hops back onto the owning thread through a `cx.spawn` task (`drive_config_reloads` in the client binary) that polls the signal every 120 ms and calls `TerminalView::reload_config`. Polling rather than waking per event collapses the delete/create/modify burst a single editor save produces into one idempotent reload, and  guarantees no save is missed between polls because the counter is monotonic.

 returns `None` when nothing is pending and otherwise reloads from disk, handing back the plan. `TerminalView::apply_config_reload` then reapplies each surface the plan flags: a theme change rebuilds the status-bar palette, the grid's terminal colours and the chrome colors, pushes the new  into the titlebar via , and drops any open palette/context-menu overlay so none keeps painting the old colours; a font change rebuilds the  the grid paints with and republishes the derived cell metrics to the server as a `Resize`. Keybindings need no flag — they are re-parsed on every reload and `handle_overlay_key` matches each keystroke against  through , so a saved shortcut edit is live on the next key press.

Every accepted reload ends with , matching the legacy client's unconditional  send: the client does not try to guess which server-side surfaces changed, it just tells the server to re-read the same file so clipboard policy, the env store, and the remote/share listeners follow in the same round trip. The plan's `opacity_changed()` signal is delivered to `TerminalView::apply_opacity_change`, which clamps the new value from , caches it for the render pass, and pushes a fresh  into the titlebar because that view owns its own palette. Nothing is recreated: the window's surface is already transparent, so the `cx.notify()` ending the reload repaints every alpha-aware background in place. See .

A watcher that fails to start (missing config dir, exhausted inotify watches) is logged and left absent: the window still runs, it just does not live-reload, exactly as the legacy client degrades. The client binary also installs a `tracing_subscriber` at startup — without it every `tracing` call in the GPUI client, including the hot-reload confirmation, was silently discarded.

## GPUI Remote And Sharing Port

The GPUI rebuild ports the feature 013/014/015 multi-machine surfaces into the `lib` target as rendering-independent state machines: the connect picker, dial handshake, displaced-client banner, LAN approval dialog, and sharing overlays.

Each is the transport-free core of a winit  module with the `CellInstance` painting dropped in favour of flattened views the GPUI chrome will consume; the frozen IPC protocol is unchanged.

 ports the winit  picker: the tailnet/LAN merge and dedup (compatible LAN preferred, incompatible LAN rows retained with update guidance, online-first sort), the peer → windows → failed step transitions, and the typed  intents. It consumes a framework-neutral  (the GPUI view lowers a `KeyDownEvent` at the call site) and exposes a flattened  instead of quads; the auto-reconnect  ports alongside it.

 ports the winit dial preamble (): it sends  `RemoteHandshake` as the first frame over any framed async stream and maps the mandatory `RemoteHandshakeReply` to a . The `SCRIBE_REMOTE_DIAL` / `SCRIBE_LAN_DIAL` / `SCRIBE_REMOTE_WINDOW` dial-env spawn hooks port as  and its env wrappers, split from the env read so the grammar stays testable without mutating process env.

The connect picker overlay, dial-env grammar, handshake preambles, and displaced banner are live, documented in , alongside the LAN half in .

The displaced-client  (from ) keeps the `Controlled by <device> (<account>)` headline and Enter-only reclaim;  (from ) keeps the Decline-default focus, fingerprint-word body, and name-collision hint. The feature 015 sharing surfaces port into  (roster roles, holder derivation), the transient , and the  — with control passing expressed as a  that lowers to the frozen v3 `ControlClaim` / `ControlRequest` / `ControlGrant` messages the winit  emits.

## Search Overlay

Find-in-scrollback overlay state in , tracking query text, match results, and highlighted match index.

State module plus GPU-rendered overlay. Methods: `open` (clears previous query and results), `close` (resets all state), `push_char`/`pop_char` (edit the query string), `set_results` (replace match list and reset highlight), `next_match`/`prev_match` (cycle through results with wrap-around), `matches` (borrow all results). Match results are `Vec<SearchMatch>` received from the server. All visible matches on the focused pane are highlighted: the current match uses the full accent background with a contrast foreground, while other matches blend the accent into their existing cell background at 40% intensity.

## Tooltip

GPU-rendered tooltip overlay in  that renders a small dark box with light text above or below an anchor rect.

 holds the tooltip text and the anchor `Rect`.  selects `Above` or `Below` placement.  emits `CellInstance` quads into the caller's buffer: a 1 px border quad, a background quad, then per-character glyph quads. The tooltip is horizontally centered on the anchor and clamped to stay within `viewport_width`. A 1-character left/right padding is included on each side of the text.
