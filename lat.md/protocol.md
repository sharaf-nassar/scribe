# Protocol

The IPC protocol defines all messages exchanged between the server and its clients over a Unix domain socket.

## Transport

Messages use length-prefixed MessagePack encoding defined in [[crates/scribe-common/src/framing.rs#read_message]].

### Frame Format

Each frame is a 4-byte big-endian u32 payload length followed by the MessagePack-serialized message body.

The maximum payload size is 64 MiB; reattach payloads ride on the far denser zstd-compressed ANSI `SessionReplay` encoding, so routine attach traffic stays well below the cap.

### Socket Path

The server socket lives at a platform-specific runtime directory selected by the active install flavor.

On Linux this is `/run/user/{uid}/scribe/server.sock` for stable installs and `/run/user/{uid}/scribe-dev/server.sock` for `scribe-dev`. On macOS it uses `~/Library/Application Support/Scribe*/run/server.sock` so GUI clients and launchd services share a stable path. All socket path logic is in [[crates/scribe-common/src/socket.rs#server_socket_path]].

### Security

Every connection is verified by checking the peer UID via `SO_PEERCRED` on Linux or `getpeereid` on macOS. Connections from a different UID are rejected. The server enforces a maximum of 32 concurrent connections per UID.

Window and session operations are further scoped after handshake: a client cannot claim an already-connected window ID, attach another window's sessions, request snapshots for unattached sessions, or close a different window.

## Client Messages

Messages sent from the UI client to the server, defined in [[crates/scribe-common/src/protocol.rs#ClientMessage]].

### Session Lifecycle

`CreateSession` spawns a new PTY with optional workspace, split direction, working directory, dimensions, and command.

`CloseSession` terminates a session. `CloseWindow` closes all sessions in a window and is acknowledged with `WindowClosed` before the client exits. The client also uses `CloseWindow` when a session exit leaves the window with no panes so the empty window is removed from persisted state before it exits. `QuitAll` broadcasts a shutdown request to every connected client, including the sender.

### Terminal I/O

`KeyInput` sends raw bytes to a session's PTY master, capped at 4 KiB per message. `Resize` updates terminal dimensions and triggers `TIOCSWINSZ`. `FocusChanged` sends CSI focus events when DECSET 1004 is active.

Keyboard-originated `KeyInput` messages also carry a dismissal bit so the server can clear persisted attention states before the next reconnect.

The client chunks large pastes into multiple `KeyInput` messages to fit the 4 KiB limit, placing bracketed-paste start/end markers on the first and last chunks only.

`SearchRequest` runs find-in-scrollback against the attached session's current snapshot and returns row/column spans. `ScrollRequest` asks the server for a snapshot rendered at a specific display offset without mutating the live session state.

### Subscription

`Subscribe` registers for output from attached session IDs (max 256). `RequestSnapshot` fetches a single attached session's full screen state.

`AttachSessions` reattaches to detached sessions with dimensions when those sessions are unowned or already belong to the caller's window. Per-session metadata (title, CWD, shell basename, session context, git branch, AI state) rides on the preceding `SessionList`/`SessionInfo` response, and per-workspace metadata (names, accent colors) rides on `SessionList::workspaces`, so the attach reply is just `SessionCreated` + [[protocol#Server Messages#Terminal Output#SessionReplay]] per session with no additional fan-out.

### Workspace Management

`CreateWorkspace` creates a new workspace with the next accent color. `ReportWorkspaceTree` sends the client's current split layout to the server for persistence.

The reported tree's [[crates/scribe-common/src/protocol.rs#WorkspaceTreeNode]] `Leaf` carries per-workspace tab ordering (`session_ids`), per-tab pane layouts (`pane_trees`), and the per-workspace active tab index (`active_tab_index`). Accent colors and names still travel separately in `WorkspaceListEntry` / `WorkspaceNamed`. The active tab index is `#[serde(default)]` so a pre-active-tab-aware handoff envelope degrades to 0 (last-active-tab is then re-asserted on the next client report).

### Workspace Notes

Workspace note messages keep note state server-owned while the client renders only cached snapshots.

`WorkspaceNotesGet` requests authoritative note collections for one or more `WorkspaceId` values. `WorkspaceNotesMutate` carries a [[crates/scribe-common/src/protocol.rs#WorkspaceNotesMutation]] value for saving drafts, creating active notes, editing notes, archiving notes, or bulk-editing archived notes.

The server validates each mutation, persists the resulting store, and only then broadcasts [[protocol#Server Messages#Workspace Notes]] to connected clients.

### Automation

Window automation messages let the CLI inspect windows and ask a connected client to execute the same actions exposed by keyboard shortcuts and the command palette.

`ListWindows` returns every connected window with its ID, session count, and connection status, plus the feature-013 workspace names and remote-controller identity per window ([[protocol#Remote Protocol#Window Context Fields]]). `DispatchAction` carries an [[crates/scribe-common/src/protocol.rs#AutomationAction]] value such as settings, find, new tab, split, close, new window, profile switch, or focus session, but the server only routes it to the caller's connected window. The server answers each dispatch with either `ActionDispatched` naming the routed window or `Error` when the target is missing or belongs to another connection.

### Connection

`Hello` is the first message sent, carrying an optional window ID and a `clipboard_gating: bool` capability flag (spec 010 C7). The server responds with [[protocol#Server Messages]] `Welcome`.

Both `Hello.clipboard_gating` and `Welcome.clipboard_gating` default to `false` for backward compatibility. When either side reports `false`, the server treats sessions in that window as headless for OSC 52 prompt and bridge purposes and the client defensively no-ops on receipt of the new clipboard variants.

If the requested window ID is already connected, the server assigns another unconnected window or a fresh ID instead of replacing the existing owner. The check and the registration are performed atomically inside [[crates/scribe-server/src/ipc_server.rs#claim_window]] while holding a single `connected_clients` write lock. A previous read-then-write split was a TOCTOU race: the concurrent-reconnect burst that a server upgrade or client relaunch triggers let two `Hello`s for the same window both observe it unconnected and both register, leaving two live clients bound to one window ID (which then fought, respawned, and churned the session).

Feature 013 adds a `takeover: bool` to `Hello` (serde-default `false`): a remote client can explicitly claim a connected window and displace its controller, and the same flag reclaims a displaced window from the other side. Local clients never set it, so this paragraph's assign-different-window behavior is exactly the `takeover = false` case — see [[protocol#Remote Protocol#Takeover]].

Transient actions are an exception: the [[server#Server#Updater#Manual Check]] path lets a non-client process (the standalone settings window) open `server.sock`, send `CheckForUpdates` as the first and only message, read back a single `UpdateCheckResult`, and disconnect — never registering as a connected client and never receiving a `Welcome`.

### Clipboard Variants

Two additive variants drive the OSC 52 gating reply path (spec 010 C3 / [[server#Sessions#Clipboard Gating]]).

`ClipboardPromptResponse { request_id, decision }` echoes the `PromptId` from the matching `ServerMessage::ClipboardPromptRequest` together with a [[crates/scribe-common/src/protocol.rs#ClipboardDecision]] (`AllowOnce` / `DenyOnce` and, in later waves, `AlwaysAllow` / `AlwaysDeny`). The server matches the id against the session's parked prompt and replays the deferred OSC 52 op against the bridge or PTY reply as appropriate.

`ClipboardBridgeReadReply { request_id, payload }` carries the host clipboard content for an allowed OSC 52 read. `payload` is a `Result<String, BridgeError>`; `Err(_)` collapses onto an empty OSC 52 reply per UX-002 so PTY-side programs never observe a distinct error state.

### Configuration

`ConfigReloaded` notifies the server that the config file has changed, triggering scrollback limit, shell integration, and workspace-root updates across live sessions.

### Update Control

`TriggerUpdate` confirms a download. `DismissUpdate` suppresses the notification for the current version. `CheckForUpdates` requests an immediate update check.

The check is answered with a single `UpdateCheckResult` on the same connection. It can be sent as the very first message on a transient connection (the settings window's "Check Now" path) or after a `Hello` on a registered client connection.

### List Releases

`ListReleases` requests a snapshot of the GitHub releases cache backing the [[settings#Releases]] panel. It carries no payload and is answered by a single [[protocol#Server Messages#Release List]] on the same connection.

The server reads the cache via [[server#Releases#Release Catalog]]. The message is sent on first Releases-tab activation when the JS-side cache is empty and again on explicit Retry / Refresh from the failure / stale sub-views, riding the same one-shot transient connection pattern as `CheckForUpdates`.

## Server Messages

Messages sent from the server to clients, defined in [[crates/scribe-common/src/protocol.rs#ServerMessage]].

### Terminal Output

`PtyOutput` carries raw PTY bytes. `SessionReplay` delivers a zstd-compressed ANSI rebuild on reattach. `ScreenSnapshot` is kept for tooling (`RequestSnapshot`, scribe-cli, scribe-test) that needs the per-cell grid.

`TrimScrollback` prunes a client's history back to the current AI redraw epoch baseline before a redraw when suppressed ED 3 clears would otherwise stack duplicate inline transcript frames into scrollback.

#### SessionReplay

Unified primitive for rebuilding a client's Term on reattach, defined in [[crates/scribe-common/src/screen_replay.rs#SessionReplay]].

Carries cols, rows, scrollback rows, cursor position/style/visibility, alt-screen flag, and a zstd-compressed ANSI byte stream. Clients decompress and feed the bytes through their VTE processor — the same primitive the server uses for hot-reload handoff, so one encoding serves both the server-to-server and server-to-client paths.

`SearchResults` pairs with `SearchRequest` and returns absolute grid spans for the current query so the client can highlight and jump between matches without replaying search locally.

### Session Events

`SessionCreated` confirms a new session with its workspace and shell basename. `SessionExited` reports a session's exit code. `Bell` forwards BEL characters, exactly one frame per BEL byte.

### Metadata

`AiStateChanged` and `AiStateCleared` report AI process state from OSC 1337. `CwdChanged` reports working directory from OSC 7.

`TitleChanged` reports window title from OSC 0/2. `SessionContextChanged` reports shell-emitted remote-host and tmux metadata from OSC 1337 `ScribeContext`. `TaskLabelChanged` and `TaskLabelCleared` report provider task-label channels used for tab naming, while legacy Codex task-label messages remain accepted for compatibility. `GitBranch` reports the detected git branch for a session's CWD. `WorkspaceNamed` reports auto-detected workspace names and the project root path. `PromptReceived` carries the session ID, AI provider, and submitted prompt text for display in the prompt bar UI.

### Connection

`Welcome` responds to Hello with the assigned window ID and a list of other unconnected windows that have sessions. `WindowClosed` and `QuitRequested` are shutdown acknowledgments.

`Welcome.clipboard_gating: bool` advertises the server's OSC 52 gating capability (spec 010 C7 — see [[protocol#Client Messages#Connection]] for the matching client-side flag and negotiation semantics).

Only the bootstrap client (launched without `--window-id`) spawns child processes for the other windows in `Welcome`; children ignore the list to prevent fan-out duplication where racing siblings each spawn redundant processes for windows not yet registered in `connected_clients`.

### Clipboard Variants

Three additive variants drive the OSC 52 gating + host clipboard bridge (spec 010 C2 / [[server#Sessions#Clipboard Gating]]).

`ClipboardPromptRequest { session_id, request_id, op, selection, preview }` asks the user to confirm an OSC 52 read or write before the server honours it. `preview` is `Some` only for `op = Write` (head-and-tail truncated payload per FR-006); reads carry no preview. The client renders a [[client#URL Detection#Clipboard Dialog]] and replies with `ClientMessage::ClipboardPromptResponse` carrying the same `request_id`.

`ClipboardBridgeWrite { session_id, selection, payload }` forwards an allowed OSC 52 write payload to the client's host clipboard bridge. No reply is expected (OSC 52 has no write-ack semantic).

`ClipboardBridgeReadRequest { session_id, request_id, selection }` asks the client to read the host clipboard for an allowed OSC 52 read. The client answers with `ClientMessage::ClipboardBridgeReadReply`; the server then runs the parked alacritty formatter against the returned payload and writes the OSC 52 reply back to the PTY.

All three variants honour the attach-time `clipboard_gating` negotiation: the server never emits them when the client failed to advertise support, and the client silently no-ops on receipt when its own cached `Welcome.clipboard_gating` is `false`.

`SessionList` returns all sessions grouped by workspace in response to `ListSessions`. Each [[crates/scribe-common/src/protocol.rs#SessionInfo]] carries the active AI state, AI provider hint, provider task label, legacy Codex task label, shell basename, session context, CWD, and detected git branch — enough for the client to restore provider-aware titles, remote labels, and status-bar branches without any post-attach metadata fan-out. A batched `workspaces: Vec<WorkspaceListEntry>` field delivers per-workspace names, accent colors, split direction, and project root paths alongside the session list. `WorkspaceInfo` messages still exist for non-attach flows (session creation, auto-naming).

When `SessionList` also includes a workspace tree, that tree is the authoritative workspace layout. The `split_direction` field is only needed for the legacy reconnect fallback where older servers omit the tree and the client must repair the linear default layout once during startup.

### Workspace Notes

Workspace notes are delivered as server-authoritative collections, not as client-owned persisted files.

`WorkspaceNotesSnapshot` answers `WorkspaceNotesGet` with `Vec<WorkspaceNotesCollection>`. `WorkspaceNotesChanged` broadcasts one updated [[crates/scribe-common/src/protocol.rs#WorkspaceNotesCollection]] after an accepted mutation has been written to the server-owned note store.

Clients replace their local note cache from these messages. The broadcast also acts as the success acknowledgement for the requester; failed mutations use `Error` and do not update client caches.

### Automation

Automation responses expose connected windows to the CLI and let the server forward actions into a specific client window.

`WindowList` returns the payload requested by `ListWindows`. `RunAction` delivers an [[crates/scribe-common/src/protocol.rs#AutomationAction]] to the target client, which executes it on the UI thread through the normal action handlers instead of a separate automation-only path. `ActionDispatched` acknowledges to the requester that the server successfully routed the action to a specific connected window.

### Update Notifications

`UpdateAvailable` announces a new version with a release URL. `UpdateProgress` reports download, verification, and installation state transitions. `UpdateCheckResult` answers a `CheckForUpdates` request.

The result variants are `NoUpdate`, `UpdateAvailable { version, release_url }`, and `Failed { reason }` (see [[crates/scribe-common/src/protocol.rs#UpdateCheckResultState]]). When the outcome is `UpdateAvailable`, the server also broadcasts the matching `UpdateAvailable` to every connected client so the regular client-side CTA stays in sync with the requester's inline status.

### Release List

`ReleaseList { state: ReleaseListResultState }` answers [[protocol#Client Messages#List Releases]]. Like `UpdateCheckResult`, it can ride a one-shot transient connection from the standalone settings window without registering as a connected client.

The `state` enum carried by [[crates/scribe-common/src/protocol.rs#ReleaseListResultState]] has three variants: `Fresh { releases }` for a cache hit within TTL or a just-completed cold fetch; `Stale { releases, reason }` for a cache hit past TTL while a background refresh is in flight (the cached vector still ships so the UI never goes blank); and `Failed { reason }` when no cache exists and the on-demand fetch failed.

Each [[crates/scribe-common/src/protocol.rs#Release]] carries `version`, `name`, `published_at`, `body_html` (pre-sanitized HTML rendered server-side from the GitHub `body` markdown), `prerelease`, and `html_url`.

### Error

`Error` carries a human-readable error message string.

## Remote Protocol

Feature 013 carries the existing framed protocol over TCP to another of the user's tailnet machines, layering a remote-only preamble, single-controller takeover, and typed refusals over the local message catalogue.

The full wire contract is `specs/013-remote-window-control/contracts/remote-protocol.md`. The local Unix-socket path is unchanged: the preamble and every remote-only message never appear locally, and the new `Hello.takeover` flag defaults `false` via serde so existing clients keep today's behavior. See [[server#Remote Control]] for the owning-side implementation and [[client#Remote Control]] for the connecting side.

Feature 015 bumps [[crates/scribe-common/src/protocol.rs#REMOTE_PROTOCOL_VERSION]] to `3` (`specs/015-multi-machine-sharing/contracts/remote-protocol-v3.md`) and layers an additive multi-machine-sharing delta over the v2 catalogue: control-handoff frames, a full-state roster broadcast, sharing fields on `WindowInfo`, an additive `Welcome.participant_id`, and a `Resize` reinterpreted as a per-participant viewport report. Every addition is `#[serde(default)]` and rides the existing framing. Because negotiation stays exact-match, a v2 peer never enters a share — it is refused `IncompatibleVersion` with both versions named, never a half-join (FR-014). See [[protocol#Remote Protocol#Sharing Messages]].

### Remote Transport

A TCP listener bound strictly to the machine's Tailscale addresses (never `0.0.0.0`) on `remote.port` (default 46061), existing only while `remote.enabled`. Frames are identical to the local socket — [[crates/scribe-common/src/framing.rs#read_message]] and the 64 MiB cap are reused unchanged.

The dialer and owner gate compatibility on [[crates/scribe-common/src/protocol.rs#REMOTE_PROTOCOL_VERSION]], a `u32` starting at 1 with an exact-match policy (bump on any change to remote-visible semantics). Up to 8 remote connections are accepted concurrently, separate from the 32 local cap; excess connections are refused `Busy` after the preamble. Everything within an accepted session — `Welcome`, `AttachSessions`, `SessionReplay`, `PtyOutput`, `KeyInput` (4 KiB cap), resize, scroll/search, clipboard, workspace, and notes messages — keeps byte-identical semantics.

### Preamble Handshake

The first frame a remote client sends MUST be `ClientMessage::RemoteHandshake` (protocol version, human Scribe version, device name); it carries no window or session data so the owner can decide before anything is revealed (FR-003).

The owner replies `ServerMessage::RemoteHandshakeReply { accepted, refusal, server versions }`. Between reading the preamble and replying, the owner resolves the peer's tailnet identity, checks the same-account authorization policy, and gates the protocol version — see [[server#Remote Control#Accept Path]]. Every refusal reaches the dialer as a typed [[crates/scribe-common/src/protocol.rs#RemoteRefusal]] so distinct UX-002 copy is possible: `Disabled` (raced a live disable), `Unauthorized` (wrong account, tagged, or unknown identity), `IdentityUnavailable` (tailscaled/WhoIs down — fail closed), `IncompatibleVersion` (both versions named), and `Busy` (connection cap reached). The same enum is the canonical audit taxonomy ([[server#Remote Control#Audit Log]]). Only a malformed or non-`RemoteHandshake` first frame closes bare with nothing to reply to, which is why transient no-`Hello` connections (update checks, hook events) stay local-only over TCP.

### Takeover

`ClientMessage::Hello` gains `takeover: bool` (serde-default `false`) to claim a currently-connected window: `false` never displaces the current controller, `true` atomically swaps the window's writer.

`true` is an explicit user action only — a first attach from the picker or a lost-control reclaim. The four outcomes turn on `takeover` and whether the claimant is local or remote:

- **Local, no takeover, window connected** — today's behavior exactly: a different or fresh window is assigned to the claimant.
- **Remote, no takeover, window connected** — the owner completes `Welcome` for the requested window then immediately sends `WindowTakenOver` naming the current controller, with no sessions attached. This is the auto-reconnect lost-control path (FR-011): a dropped remote client resumes normally when its window is free (the common case) but lands displaced — never a silent seizure — when someone took the window mid-outage.
- **Takeover, window connected** — the owner swaps the writer under the claim lock, sends `ServerMessage::WindowTakenOver { device_name, login_name }` to the displaced client, and runs the normal attach flow for the claimant. Reclaim is the same message from the other side, so local and remote claims share one path.
- `Hello { window_id: None }` over remote creates a fresh window (remote create).

All per-connection state — the `clipboard_gating` capability bit and clipboard-bridge routing — follows the NEW controller's `Hello` from the moment of the swap; no stale capability survives a takeover ([[server#Remote Control#Takeover and Control]]). On `WindowTakenOver` the displaced client stops sending input, dims and freezes its last frame under a banner naming the controller, and offers one-action reclaim; it expects no further `PtyOutput` (no fan-out in v1) — see [[client#Remote Control#Displaced and Lost Control]].

### Disconnect and Sever

`ServerMessage::RemoteDisconnect { reason }` is a best-effort final frame the owner sends before closing a remote connection for a policy reason; v1's only reason is `Disabled` (remote access turned off).

The close follows regardless of whether the frame is delivered. The delivered notice is what lets the connecting side state "remote access was turned off on `<peer>`" as fact rather than inference. If it is lost (crash, dead link) the client falls back to its reconnect path, where the vanished listener yields the combined connection-failure copy — a disabled machine is deliberately indistinguishable from an unreachable one on a cold connect because FR-001 forbids leaving anything listening. Owning-side sessions are untouched. See [[server#Remote Control#Disable and Sever]].

### Peer Discovery

`ClientMessage::ListRemotePeers` / `ServerMessage::RemotePeerList { peers }` feed the connect picker from the connecting machine's OWN local server, the only party that talks to tailscaled.

Each [[crates/scribe-common/src/protocol.rs#RemotePeerInfo]] carries a MagicDNS name, a dial address, and an online flag. These are local-socket-only, answered on the connecting client's own local connection on BOTH the pre-`Hello` transient path AND the post-`Hello` frame the live picker session sends ([[crates/scribe-server/src/ipc_server.rs#dispatch_message]] handles it only when the connection is local, `is_remote` false); a remote peer is refused on both paths (a non-`Hello` first frame closes; a post-`Hello` remote sender falls through unhandled), so it cannot enumerate a third machine's tailnet view. The GUI client never speaks LocalAPI directly — the peer list is resolved server-side in [[server#Remote Control#Tailnet Identity]].

### Local Env Query

`ClientMessage::GetRemoteEnv` / `ServerMessage::RemoteEnv { account, tailscale_detected }` report the connecting machine's OWN signed-in tailnet account name and whether Tailscale is detected at all, for the Settings → Remote section (UX-003).

Like [[protocol#Remote Protocol#Peer Discovery|ListRemotePeers]] it is a transient local-socket-only helper resolved from this machine's own LocalAPI view: over TCP it is refused as a non-`RemoteHandshake` first frame, so a remote peer can never read a third machine's identity. Any LocalAPI failure fails closed to `{ account: None, tailscale_detected: false }` (FR-015), which drives the passive "Tailscale not detected" notice. The owning-side resolution lives in [[server#Remote Control#Tailnet Identity]] and the settings-host consumer is [[settings#Config Application#Remote Keys]].

### Window Probe

An already-authorized remote connection may send `ClientMessage::ListWindows` as a read-only frame BEFORE its `Hello`, receiving a `ServerMessage::WindowList` of the peer's windows to populate the connect picker (FR-005).

The probe registers no window and claims no control: [[crates/scribe-server/src/ipc_server.rs#establish_client_window]] is a loop that answers the `WindowList` and keeps reading, so the probe then closes or a `Hello` may follow on the same link. Every OTHER non-`Hello` first frame after an accepted handshake still closes — the transient no-`Hello` helpers (update checks, hook events, `ListRemotePeers`, `GetRemoteEnv`) stay local-socket only over TCP. This is distinct from [[protocol#Remote Protocol#Peer Discovery]], which lists the connecting machine's OWN peers; the probe lists the dialed peer's windows.

### Window Context Fields

[[crates/scribe-common/src/protocol.rs#WindowInfo]] (carried by [[protocol#Server Messages#Automation]] `WindowList` and the `Welcome` unconnected-window list) gains two additive, `#[serde(default)]` fields for feature 013: `workspace_names` and `controller`.

`workspace_names` lists the window's distinct named workspaces in session order, feeding the remote connect picker's window list (FR-005); it is built by [[crates/scribe-server/src/workspace_manager.rs#WorkspaceManager#workspace_names_for_window]]. `controller` is an [[crates/scribe-common/src/protocol.rs#ControllerInfo]] (device + login name) present only while a remote peer holds the window and `None` when it is unconnected or locally controlled — this lets window-listing and status surfaces mark remotely-controlled windows, including remotely-created ones that never had a local client (FR-009b, SC-006). Both are empty or `None` from an older server.

Feature 015 adds three more `#[serde(default)]` fields for shared windows: `participants` (a `Vec<ControllerInfo>` of the attached remote participants, empty from an older server or a locally-controlled/unconnected window), `mode` (an `Option<SharingMode>`; `None` decodes from older servers), and `participant_count` (the attached-machine count). The retained `controller` still names the sole holder in `SingleController` mode and the current `holder` — or `None` when unheld — in shared modes; the connect picker reads `participants`/`participant_count` to show share occupancy ("N attached") instead of 013's binary in-use flag ([[client#Remote Control#Connect Picker]]).

### Sharing Messages

Feature 015's additive v3 layer for multi-machine sharing: control-handoff frames, a full-state roster broadcast, share-notice frames, an additive `Welcome.participant_id`, and a reinterpreted `Resize`, gated by the exact-match bump to `3`.

The owning-side handling is [[server#Remote Control#Sharing]] and the connecting side is [[client#Remote Control]]. Three new `ClientMessage` variants pass input control between attached machines without disconnecting anyone. `ControlClaim { window_id }` takes control in `SharedSingleTypist` under `control_acquisition = FreeClaim` (or the owning machine claiming regardless); `ControlRequest { window_id }` asks for it under `RequestAndGrant`; and `ControlGrant { window_id, participant_id, accept }` answers, naming the requester's server-monotonic `participant_id` (`u64`). The result reaches clients through the next `ShareRoster` (its new `holder`) or a `ControlDenied` — no dedicated ack. All are additive, never sent by a v2 client or on the local socket.

Server→participant frames announce presence and outcomes. `ShareRoster { window_id, participants, mode, holder }` is a full-state broadcast — no deltas — on every join, leave, control transfer, ejection, and mode change (FR-008, SC-005); each `participants` entry is a [[crates/scribe-common/src/protocol.rs#ParticipantInfo]] (`participant_id`, `device_name`, `login_name`, `is_local`, `is_holder`) reusing the `device_name`/`login_name` pair of [[crates/scribe-common/src/protocol.rs#ControllerInfo]] so the identity surface never drifts. `ControlRequested { window_id, from }` reaches the current holder (or the owner when unheld) under request-and-grant; `ControlDenied { window_id }` reaches a requester on a declined or cancelled request; and `ShareEnded { window_id, reason }` (a [[crates/scribe-common/src/protocol.rs#ShareEndReason]] — `OwnerClosed` / `WindowClosed` / `ModeChangedToSingleController`) is the mode-neutral notice sent to remote participants when the owner closes the window/session or flips to `SingleController` (for that flip they also receive the legacy `WindowTakenOver`).

`WindowTakenOver { device_name, login_name }` is retained UNCHANGED but used only for exclusive-takeover displacement and the `SingleController` mode flip (FR-003/017) — the frozen dimmed-frame experience ([[client#Remote Control#Displaced and Lost Control]]). It is never sent for an additive share join or a single-typist control pass, which keep the displaced machine live and use `ShareRoster`.

`Resize { session_id, size }` keeps its exact v2 wire shape but changes meaning in a share (v3): each participant's `size` is stored as its `viewport` rather than setting the session grid directly, and the server drives the authoritative grid to `min(rows)` × `min(cols)` across attached participants, debounced 250 ms (data-model `AuthoritativeGrid`, [[server#Remote Control#Sharing]]). In shared modes it is exempt from control gating — accepted from any attached participant, including a viewer, so a smaller window can drive smallest-wins; only `SingleController` keeps the legacy controller-gated direct grid-set. It is wire-identical to v2, but a v2 peer never shares, so the reinterpretation only applies among v3 peers.

`Welcome` gains an additive `participant_id: Option<u64>` (`#[serde(default)]`) carrying the connection's own server-assigned id in the window's share, so a client matches itself in a later `ShareRoster` exactly (its own `is_holder`) rather than by device name; `None` from an older server or a claim that registered no participant (a lost-control landing). `Hello` is unchanged — a joiner needs no new field because sharing is the owner's mode, not the joiner's request, and `takeover: true` keeps its exact 013 meaning (an exclusive claim that ends any share). The participant-limit refusal reuses the existing `Busy` refusal — no new variant (FR-018) — and the local Unix-socket path is completely unchanged (SC-006).

### LAN Transport

Feature 014 adds a second remote transport beside 013's tailnet path: a Tailscale-free LAN link over mutual TLS, found by mDNS and gated by explicit device approval. A separate opt-in, off by default, it reuses 013's post-approval session unchanged.

The wire contract is `specs/014-lan-remote-control/contracts/lan-protocol.md`. Every addition is serde-default-tolerant and rides the SAME [[crates/scribe-common/src/protocol.rs#REMOTE_PROTOCOL_VERSION]] — bumped to `2` for 014 (and again to `3` for feature 015, [[protocol#Remote Protocol#Sharing Messages]]) — under 013's exact-match policy, so a version mismatch is refused with both versions named. The LAN listener binds `remote.lan.port` (default 46062, distinct from the tailnet 46061) only while enabled and on a trusted network. The owning side is [[server#Remote Control#LAN Accept and Approval]] and the connecting side is [[client#Remote Control#LAN Dial]].

### LAN Discovery

LAN peers are found over mDNS/DNS-SD (service `_scribe._tcp.local.`), the control port riding the SRV record and TXT carrying `id`, `protovers`, and `host`. Discovery runs only while LAN access is enabled and the current network is trusted.

`ClientMessage::ListLanPeers` / `ServerMessage::LanPeerList { peers }` feed the connect picker from the connecting machine's OWN local server — the only party that runs mDNS — each a [[crates/scribe-common/src/protocol.rs#LanPeerInfo]] (device id, display name, resolved addresses, port, protocol version). Like 013's `ListRemotePeers` they are local-socket-only, answered on the connecting client's own local connection on BOTH the pre-`Hello` transient path AND the post-`Hello` frame the live picker session sends ([[crates/scribe-server/src/ipc_server.rs#dispatch_message]], gated to local `is_remote` false); a remote peer is refused on both (a non-`LanHello` / non-`RemoteHandshake` first frame closes; a post-`Hello` remote sender is ignored), so it cannot enumerate a third machine's LAN view. Browse filters ignore self (own `id`), dedupe by TXT `id`, drop addresses outside the current LAN subnet, and exclude tailnet/VPN interfaces. Merging a LAN peer with a tailnet peer in the picker is a **UX convenience, not a trust boundary**: the two are matched heuristically by machine name/hostname (TXT `host` vs. the tailnet MagicDNS name) because the LAN `device_id = SHA-256(SPKI)` and the tailnet identity are different namespaces (013's `RemotePeerInfo` carries no Scribe device id). A confidently name-matched dual-reachable peer is shown once with the direct LAN path preferred (FR-008); see [[client#Remote Control#Connect Picker]].

### LAN Preamble and Approval

After the mutual-TLS handshake the LAN dialer's FIRST frame is `LanHello` — carrying a display name and protocol version but no window or session data — so the owning side can gate on device approval before anything is revealed (SEC-001).

The cryptographic identity is the pinned TLS client cert (`device_id = SHA-256(SPKI)`, FR-005), NOT `LanHello.device_name`, which is a display label only. A pinned (already-trusted) device proceeds straight to the version gate; an unknown device is held pending the owning user's explicit decision. The owner first sends `ServerMessage::LanApprovalPending` — MUST precede any window data so the dialer can show "waiting for approval on `<peer>`" (FR-014, US2.5) — then raises the prompt on ITS OWN local client via `ServerMessage::LanApprovalRequest { request_id, device_name, fingerprint_words, network_label, name_collision }`; that client replies `ClientMessage::LanApprovalDecision { request_id, approve }` over its local socket (the GUI never handles the remote TLS stream, and both are refused over any remote transport). The terminal outcome is `ServerMessage::LanApprovalResult { approved, refusal }`, whose `refusal` is a typed [[crates/scribe-common/src/protocol.rs#LanRefusal]] present iff `!approved`: `Declined`, `NotTrustedNetwork`, `Disabled`, `IncompatibleVersion`, or `Busy` — the canonical UX-002 copy and audit taxonomy. On approve the server writes a trusted-device pin and proceeds to the 013 attach flow; on decline/timeout it reveals nothing. There is NO `IdentityChanged` refusal: a reinstalled peer presents a new, unpinned `device_id` and is a normal unknown device, with `name_collision` flagging a reused display name as an informational hint only (never a trust key).

### LAN Trust and Discovery Helpers

Settings and the connect flow manage LAN trust by talking to their OWN local server — the only party that owns mDNS, the device identity, and the trust stores. These helpers are local-socket-only, refused over any remote transport.

`ClientMessage::ListTrustedDevices` / `ServerMessage::TrustedDeviceList { devices }` list approved peers as [[crates/scribe-common/src/protocol.rs#TrustedDeviceInfo]] (name, fingerprint words, hex device id, approval time); `ClientMessage::RevokeTrustedDevice { device_id }` removes a pin and severs only that device's live LAN connection (FR-010). `ClientMessage::ListTrustedNetworks` / `ServerMessage::TrustedNetworkList { networks, current_trusted }` list [[crates/scribe-common/src/protocol.rs#TrustedNetworkInfo]] and whether the current network is among them (driving the active/dormant status line, UX-004); `ClientMessage::AddCurrentNetworkTrusted` marks the current network trusted (acked, or errored when it cannot be fingerprinted) and `ClientMessage::RemoveTrustedNetwork { id }` removes one (going dormant if it was the current network). `ClientMessage::GetLanEnv` / `ServerMessage::LanEnv` report THIS machine's own device fingerprint (word list + hex, for the optional out-of-band MITM compare, FR-006) and whether the current network is addable. `ClientMessage::GetLanDialIdentity` / `ServerMessage::LanDialIdentity { available, cert_der, private_key_pkcs8_der }` hand a co-located connecting `scribe-client` THIS machine's own device identity (public cert + sealed `PKCS#8` key) so the dialer builds its mutual-TLS identity without reading the OS keyring from a different binary — the server is the SOLE keychain accessor (macOS legacy `SecKeychain` per-binary ACLs deny a cross-binary key read); the reply carries private key material and, like the other helpers, never crosses a remote transport. The owning-side resolution lives in [[server#Remote Control#LAN Trust Management]] and the settings consumer is [[settings#Config Application#Local Network Keys]].

## Screen Snapshots

The per-cell terminal-state wire format, defined in [[crates/scribe-common/src/screen.rs#ScreenSnapshot]]. Used by tooling (`RequestSnapshot`, scribe-cli, scribe-test JSON/PNG capture) that needs a direct cell-level representation. The client reattach path uses the denser [[protocol#Server Messages#Terminal Output#SessionReplay]] format instead.

### ScreenSnapshot

A complete serializable screen state containing: a flat `Vec<ScreenCell>` grid (rows x cols), grid dimensions, cursor position and style, and cursor visibility.

Also includes alternate screen mode flag and scrollback history as a separate cell vector with a row count.

### ScreenCell

Each cell holds a character, foreground and background [[protocol#Screen Snapshots#ScreenColor]], and a flags struct with booleans for bold, italic, underline, strikethrough, dim, inverse, hidden, and wide.

### ScreenColor

Three representations: `Named(u16)` for semantic colors (values above 255 indicate Foreground, Background, etc.), `Indexed(u8)` for the xterm-256 palette, and `Rgb { r, g, b }` for direct 24-bit color.

## Identity Types

UUID-based newtypes defined in [[crates/scribe-common/src/ids.rs]] provide type-safe identifiers.

`SessionId`, `WorkspaceId`, and `WindowId` each wrap a UUID, generated by the `define_id!` macro, and display as an 8-character prefix for logging.

## AI Process State

Defined in [[crates/scribe-common/src/ai_state.rs#AiProcessState]]. Tracks the current AI state and optional metadata for resuming provider sessions.

Tracked fields include state (`idle_prompt`, `processing`, `waiting_for_input`, `permission_prompt`, `error`), tool name, agent identifier, model name, context usage percentage (0-100), and optional provider conversation IDs.

Optional metadata fields are sticky across same-provider state changes via [[crates/scribe-common/src/ai_state.rs#AiProcessState#merge_partial_from_previous]] — the [[server]] applies the merge before broadcasting `AiStateChanged` so partial events from state-only hooks do not erase live values like the context-window fill last set by an earlier same-provider state event.

Context-window % updates flow through a dedicated channel: status-line / usage-poll producers emit `HookEventKind::ContextChanged` (see [[server#Hook Channel]] and [[crates/scribe-common/src/hook.rs#HookEventKind]]) which [[crates/scribe-server/src/ipc_server.rs#send_ai_context_change]] applies as a partial patch on the existing live state. State transitions stay owned by the per-provider hook events so a periodic context refresh never resets a hook-set state.
