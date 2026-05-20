# Server

The scribe-server is a long-running daemon that owns PTY sessions, manages workspaces, and coordinates zero-downtime upgrades.

## Startup

The server initializes in [[crates/scribe-server/src/main.rs#main]] by loading config, creating a SessionManager and WorkspaceManager, then acquiring a singleton lock and binding the IPC socket.

It acquires the singleton lock via flock on `server.lock`. The main loop uses `tokio::select!` over the IPC accept loop, handoff listener, and Ctrl+C signal.

### Upgrade Path

When launched with `--upgrade`, the server restores handoff state and received file descriptors from the old instance instead of starting fresh.

It rebuilds the session and workspace managers, filtering workspace and window membership against the received live-session set so stale IDs from older servers are dropped before serving.

## Sessions

Each PTY session is represented by a [[crates/scribe-server/src/session_manager.rs#ManagedSession]] during creation and a LiveSession during active operation.

### Session Creation

The SessionManager creates sessions through alacritty_terminal's PTY spawner, wrapping the master fd in an [[crates/scribe-pty/src/async_fd.rs#AsyncPtyFd]] for epoll-driven async I/O. A maximum of 256 concurrent sessions is enforced.

Environment variables are set to TERM=xterm-256color, COLORTERM=truecolor, and TERM_PROGRAM=Scribe on top of the server process environment. On Linux, [[crates/scribe-client/src/ipc_client.rs#sync_linux_service_environment]] refreshes the user systemd manager's GUI session variables before starting the service so new PTY sessions inherit working clipboard/display access. Packaged user services are enabled under `graphical-session.target`, not `default.target`, so display-manager autostart waits until DISPLAY/XAUTHORITY are available.

New and handoff-restored terminal cores are created with kitty keyboard protocol enabled, so alacritty_terminal can answer Codex and shell keyboard-mode probes (`CSI ? u` and related mode updates) through the normal PTY write-back path.

### Session Activation

Sessions move from the SessionManager (pending) to the LiveSessionRegistry (active) via `activate_pending_sessions`. Each activated session gets a PTY reader task spawned.

### PTY Reader Task

The reader task runs three processing paths per read cycle: raw byte forwarding, ANSI processing through the alacritty_terminal state machine, and metadata extraction via the OSC interceptor.

For supported AI coding sessions, an [[crates/scribe-pty/src/ed3_filter.rs#Ed3Filter]] strips `\x1b[3J` before forwarding PTY output to the client and the server's Term. Prompt text, attention/error states, and inactive markers start a scrollback trim epoch; the first suppressed clear captures the baseline after replay, and later suppressed clears in that epoch trim both Terms back to it before replaying the redraw bytes. This keeps committed AI transcript history while preventing inline AI redraws from piling duplicate frames into scrollback. The old `/clear` bypass no longer exists.

The server-side ANSI processor also honors VTE synchronized updates (`CSI ? 2026 h/l`). If a sync block remains open past the parser timeout, the reader task flushes the buffered bytes into the server's Term before polling again so snapshots, reconnect, and search do not lag behind buffered Codex output forever.

Normal session PTY output now forwards those raw sync markers to the attached client too. The server no longer strips `CSI ? 2026 h/l` on the live path; instead the client preserves each synchronized-update commit boundary from a single PTY chunk and drains them across redraws so inline Codex and any other DECSYNCUPDATES user can animate normally without diverging from the server's authoritative `Term`.

Metadata events trigger title, provider task label, CWD, AI state, prompt text, and bell updates. CWD changes also trigger workspace auto-naming and git branch detection.

Before persisting and broadcasting `AiStateChanged`, [[crates/scribe-server/src/ipc_server.rs#merge_partial_ai_state]] folds optional metadata (`context`, `model`, `tool`, `agent`, `conversation_id`) from the previously-stored live-session state into the incoming event when those fields are `None` and the provider matches. State-only hook OSC sequences (e.g. `ClaudeState=permission_prompt`) therefore preserve the live context-window fill set by the statusLine producer instead of clobbering it.

Shell integration can also emit OSC 1337 `ScribeContext` metadata describing whether the current pane is remote, which host it is attached to, and the current tmux session name. The server stores that session context in the live session registry and rebroadcasts it on reconnect so the client can label panes before the next prompt redraw.

Terminal query callbacks share that same reader-task path. Clipboard loads, text-area-size reports, device-status replies, and dynamic colour queries are written back to the PTY from the live session state; colour queries fall back to the configured Scribe theme so foreground/background-sensitive TUIs see the real palette.

### Detach and Reattach

Client disconnection clears the client writer, while PTY EOF removes that session from live and ownership state before reconnect or handoff.

`CloseWindow` removes the whole window and its persisted tree. `CloseSession` and `CloseWindow` rely on `Pty::Drop` to send SIGHUP for fresh sessions, but handoff-restored sessions have `pty: None` so [[crates/scribe-server/src/ipc_server.rs#signal_if_handoff_session]] sends `kill(child_pid, SIGHUP)` explicitly. The PTY reader task exits naturally on EOF once the child dies.

Each live session also tracks the current client's attached-session set alongside its writer. Reattach swaps both handles together, disconnect clears both, and PTY EOF removes the session ID from that per-client set before the connection loop sees the exit. Long-lived clients therefore do not accumulate stale attachment IDs as short-lived sessions churn.

`ipc_server.rs` remains the transport and message-dispatch layer for `AttachSessions`, but [[crates/scribe-server/src/attach_flow.rs]] owns the reattach sequence itself: attach-entry preparation from live sessions, pre-snapshot Term and PTY resize, [[crates/scribe-server/src/attach_flow.rs#take_session_replay]] for the zstd-compressed ANSI replay, and the delayed client-writer install. Per-session metadata and per-workspace names travel on the preceding `SessionList` response, so the attach fan-out is just `SessionCreated` + `SessionReplay` per session.

Each session's attach work runs on its own `tokio::spawn`ed task and the per-session futures are driven via `futures::future::join_all`, so the CPU-heavy snapshot/compression steps proceed concurrently across worker threads. The shared IPC writer is a `tokio::sync::Mutex`, which serializes only the final wire writes without blocking the parallel replay builds.

When a new client attaches, the attach flow usually resizes each session's Term and PTY to the client-provided dimensions before taking the replay. This ensures the replay matches the client's pane grid and absorbs the shell's SIGWINCH response before the client writer is set. Sessions still serving a preserved v4 legacy handoff snapshot skip that pre-replay resize so a live foreground process cannot redraw over the pre-upgrade history before the first replay reaches the client.

Attach, subscribe, and snapshot requests are scoped to the caller's attached sessions and window ownership. A new connection may claim a persisted window ID only when that window is not already connected; [[crates/scribe-server/src/ipc_server.rs#claim_window]] resolves and registers that decision under one write lock so concurrent claims cannot race into a duplicate. On disconnect, [[crates/scribe-server/src/ipc_server.rs#release_window_if_owned]] removes the window's connected-client entry only when the stored writer is still this connection's (`Arc::ptr_eq` identity), so a stale disconnect from a client already superseded by a newer client for the same window cannot evict the new owner and make the window look unconnected.

When the connected-client map drops to zero, the server starts a short 250 ms grace timer before asking the singleton settings process to quit over `settings.sock`. If a client reconnects during that grace window, the settings shutdown is skipped so hot-reload or restart handoffs do not spuriously close the settings window.

### Terminal Resize

Resize updates the alacritty_terminal grid and sends `TIOCSWINSZ` via ioctl to notify the foreground process group.

### Git Branch Detection

On CWD change, the server walks up from the working directory (depth limit 50) looking for `.git/HEAD`. It extracts the branch name from `ref: refs/heads/...` or returns the first 8 characters of a detached HEAD commit.

## Workspaces

Managed by [[crates/scribe-server/src/workspace_manager.rs#WorkspaceManager]], workspaces group sessions and track per-window split layouts.

### Auto-Naming

When a session's CWD changes (via OSC 7 or /proc fallback), the server matches it against configured workspace roots and derives the workspace name and project root.

The first path component after the matching root becomes the workspace name; the full `root / name` path becomes the project root. Moving to a different project under the same root updates both. When the CWD moves outside all configured roots, the name and project root are cleared (an empty-string name is sent to the client). The project root is sent to the client so AI tabs can open at the workspace root directory instead of inheriting the current tab's CWD.

On `ConfigReloaded`, the server replaces live workspace roots and re-evaluates each session's stored CWD or `/proc` fallback so newly added roots name already-open panes without requiring a server restart or another `cd`.

### CWD Fallback Detection

If a title change is detected without an accompanying OSC 7 event, the server falls back to reading `/proc/{pid}/cwd` on Linux or calling `proc_pidinfo` on macOS to detect CWD changes.

### Accent Colors

Workspaces cycle through an 8-color palette (indigo, cyan, emerald, rose, amber, lime, pink, cyan) as they are created.

### Per-Window Trees

Each connected window can report its workspace split layout via `ReportWorkspaceTree`. The server persists these trees for handoff and reconnection. A legacy global tree is supported for backward compatibility.

### Workspace Notes

Workspace notes are durable server-owned user content keyed by `WorkspaceId`.

The authoritative store lives in [[crates/scribe-server/src/workspace_notes.rs#WorkspaceNotesStore]] and is loaded from `$XDG_STATE_HOME/<flavor>/workspace_notes.toml` during server startup. The file must carry `owner = "server"`, so the client-local note file used by the earlier implementation is ignored and the new store starts fresh.

[[crates/scribe-server/src/workspace_notes.rs#WorkspaceNotesStore#apply_mutation]] clones the current store, applies one [[crates/scribe-common/src/protocol.rs#WorkspaceNotesMutation]], writes the next TOML file atomically, and commits the in-memory state only after the write succeeds. `ipc_server` then broadcasts `WorkspaceNotesChanged` to all connected clients. If validation or persistence fails, the requester receives `Error` and no broadcast is sent.

Drafts use the same store as saved notes. Clients debounce `SaveDraft` while typing and force a final draft mutation before close, save, or shutdown transitions that can otherwise lose unsaved text.

## Handoff

Zero-downtime server upgrades are implemented in [[crates/scribe-server/src/handoff.rs#HandoffState]] using Unix file descriptor passing.

### Protocol

The new server (with `--upgrade`) connects to the old server's handoff socket, sends `SCRIBE_UPGRADE` magic bytes, and receives serialized state plus PTY master fds via SCM_RIGHTS.

On Linux and macOS, the old server also verifies that the peer PID is a permitted Scribe server executable running with `--upgrade` before sending state or PTY fds. This prevents arbitrary same-UID clients from speaking the raw handoff protocol.

An ACK confirms receipt. If the ACK is not received (version mismatch, peer crash), the old server logs the failure and loops back to accept the next connection — it keeps serving until a compatible upgrade succeeds or `postinst` cold-restarts it. The handoff version is tracked to detect incompatible format changes. The new server emits `"IPC server listening"` immediately after it binds the IPC socket (see [[crates/scribe-server/src/main.rs#run_server_loop]]), which is the Debian hot-reload watchdog's bind-ready signal — session restoration continues on the same task after that log so the watchdog never blocks on per-session work.

### State Transfer

The HandoffState contains per-session metadata, per-session replay payload, and workspace layout state for restart handoff.

Per-session payloads include title, shell basename, remote context, provider task label, CWD, AI state (including optional provider conversation IDs used for resume behavior), and a [[crates/scribe-common/src/screen_replay.rs#SessionReplay]] carrying the zstd-compressed ANSI replay for the session's visible grid plus scrollback. File descriptors are transferred one-for-one with the serialized session list.

Per-workspace payloads include name, accent color, split direction, session list, and project root path. The project root is an additive `#[serde(default)]` field so handoff from older servers defaults to `None`.

Workspace notes are not embedded in handoff state. They are write-through server state, so the replacement server reloads the persisted notes store before answering note snapshots.

### Session Replay Encoding

Both server-to-server hot-reload handoff and server-to-client reattach use the same primitive: a zstd-compressed ANSI replay that receivers feed through VTE to rebuild the `Term` durably.

The unified format replaces the legacy per-cell `ScreenSnapshot` on the reattach wire, shrinking attach payloads by 20-100x and eliminating the duplicate snapshot → ANSI round-trip the old two-format split produced.

Producers call [[crates/scribe-common/src/screen_replay.rs#build_session_replay]], which snapshots the `Term`, runs [[crates/scribe-common/src/screen_replay.rs#snapshot_to_ansi]] to emit the scrollback + visible grid + cursor as an ANSI byte stream, and zstd-compresses the result. Consumers (both `restore_from_handoff` and the client's reattach `handle_session_replay`) decompress and feed the bytes through `vte::ansi::Processor::advance` into a freshly-constructed `Term`, so the restored session's grid and scrollback are populated durably — every subsequent attach sees the same content, not just the first.

The encoder emits an ED 2 (erase display) early, which on a fresh grid scrolls the blank viewport into scrollback; [[crates/scribe-server/src/session_manager.rs#SessionManager#restore_from_handoff]] and the v4-legacy branch of [[crates/scribe-server/src/attach_flow.rs#take_session_replay]] call `Grid::update_history` after the feed to trim that pseudo-scrollback back down to the source's true `scrollback_rows`. On the client, pane.feed_output absorbs the ED 2 into its own Term and the receiving scrollback is bounded by `terminal.scrollback_lines`.

Alt-screen sessions carry only the visible grid in the replay; alt-grid history is a resize artifact rather than user content, and alt-screen applications (vim, Claude Code) redraw their own UI on reconnect.

### Defuse Strategy

Before the old server exits, Pty objects are wrapped in `ManuallyDrop` to prevent their Drop impl from sending SIGHUP to child processes.

The new server already holds the master fds via SCM_RIGHTS. Because defused sessions have `pty: None`, close handlers use [[crates/scribe-server/src/ipc_server.rs#signal_if_handoff_session]] to send SIGHUP explicitly when those sessions are later destroyed.

### Size Limits

Maximum handoff state size is 256 MiB. Maximum file descriptors transferred is 1024. Both sides verify peer UID, and Linux/macOS senders validate the peer process before sending sensitive state.

Typical v5 compressed payloads are in the low tens of megabytes even for many sessions at the default `scrollback_lines = 10_000`, since the ANSI replay + zstd combination is roughly 20-100x denser than the v4 per-cell MessagePack encoding.

### Version Bumps

Bump [[crates/scribe-server/src/handoff.rs#HANDOFF_VERSION]] when [[crates/scribe-server/src/handoff.rs#HandoffState]] changes incompatibly. Additive per-session fields that use `#[serde(default)]` stay on the current version because the wire format is named MessagePack: missing fields are filled with their defaults regardless of insertion position.

The sender uses `rmp_serde::to_vec_named` so `HandoffState` and `HandoffSession` serialize as MessagePack **maps** keyed by field name (since v6). Earlier versions used the default `rmp_serde::to_vec` which emitted MessagePack **arrays** — positional encoding where any field insertion in the middle of the struct silently mis-aligned every later field, breaking even "previous-version" hot-reloads despite `#[serde(default)]` annotations. Named encoding makes the invariant honest: as long as renames go through `#[serde(rename = "old_name")]` or `#[serde(alias = "old_name")]`, every additive struct change preserves backward compatibility. Cross-encoding handoff (v5 positional sender → v6 named receiver) is not supported; the client falls back to a cold restart of the stale old server.

Cold-restart is permitted only when hot-reload is genuinely impossible: incompatible state format (deserialization error — the underlying `rmp_serde` error is now propagated verbatim instead of being masked as "version mismatch"), version number outside the receiver's supported range, operational failure (OOM, fd/size limits, socket or zstd decode error, corrupted payload), or downgrade. A normal forward upgrade through any two consecutive releases that both use the named-map wire must hot-reload without terminating sessions.

On Linux that cold-restart path must also clean up any detached `scribe-server --upgrade` process left behind by the failed handoff before starting the user service again; otherwise the stale process can keep `server.sock` and `server.lock`, causing the restarted unit to fail with "another scribe-server is already running". On macOS the equivalent fallback runs from the client side: [[client#Client#IPC Client#Server Lifecycle]] tracks the old peer PID across the refresh request and force-restarts the stale server when the new `--upgrade` child fails to take over within `SERVER_REFRESH_TIMEOUT`.

### Binary Change Detection

All three binaries (server, client, settings) use SHA-256 hash comparison to skip unnecessary restarts during upgrades, and Linux server upgrades also track a persisted runtime-generation stamp for launcher and service behavior changes.

On Linux, `postinst` compares each running binary (`/proc/PID/exe`) against the installed copy and also checks whether the desired `server-runtime-generation` differs from the stamp recorded in `/run/user/{uid}/{app}/server-runtime-generation`. That stamp is an opaque SHA-256 signature derived from the launch-critical `postinst` functions and the installed user service unit, not a hand-maintained integer, so maintainer-script and service-launch changes automatically force hot-reload even when the server binary is unchanged. `postinst` also refreshes service enablement so older `default.target` symlinks are removed and the service is enabled for `graphical-session.target`. When `postinst` launches replacement user processes, it prefers GUI session variables from `systemctl --user show-environment` and only falls back to the invoking shell for values the user manager lacks. Client relaunches wait for the previously running client PIDs to exit, pause long enough for the server to clear old connected-window state, and skip relaunch if an old client refuses to die; this prevents a fresh client from receiving an empty `SessionList` and creating a blank window while the server still marks the old window connected. The deferred in-app cold-restart helper follows the same rule before spawning the replacement client. The Debian hot-reload watchdog now waits up to 30 seconds for the replacement server to bind its IPC socket, because large handoff snapshots can take substantially longer than 5 seconds to transfer and restore. If Linux must fall back to a cold server restart, the package script still relaunches any previously running client window even when the client binary is unchanged because that client exits on `ServerDisconnected`. On macOS, the [[server#Updater]] compares old (`.app.prev`) and new app bundle binaries before deciding which components to restart. Hash comparison failure is treated as "changed" for safety. Use `just restart-server` for manual hot-reload.

## Updater

Background update checker in [[crates/scribe-server/src/updater.rs#UpdaterHandle]] that polls GitHub releases and installs verified updates with platform-specific strategies.

### Check Cycle

After a 30-second initial delay, the updater checks on a configurable interval (from `UpdateConfig.check_interval_secs`, minimum 300 seconds) via a single `fetch_latest_release()` call to the GitHub releases API.

Stable channel filters out drafts and prereleases; Beta channel includes prereleases. The endpoint can be overridden via the `SCRIBE_UPDATE_API_URL` environment variable for testing. On failure, one retry is attempted after a 5-second backoff before giving up until the next cycle. Dismissed versions remain suppressed until a newer version appears.

### Manual Check

`UpdaterHandle::request_check` runs an immediate check off the periodic schedule and returns the outcome via a per-call oneshot reply channel.

Unlike the periodic path, it overrides the dismissed-version filter so an explicit user click always re-broadcasts a still-current update; the dismissed tracker is then refreshed so the next periodic tick stays quiet. Manual checks work even when `update.enabled = false` — the updater task always runs and only the periodic interval branch is gated by the config flag, so a user with auto-checks turned off can still drive checks from the settings window's "Check Now" button.

The reply channel has capacity 1; concurrent requests fail-fast with `Failed { reason: "already in progress" }` rather than blocking the caller's connection budget. A 20-second internal timeout caps the wait if the select loop is busy installing an update, surfacing a clean "install in progress" message instead of a generic transport timeout.

The standalone settings window can also kick off an install on the same transient first-message path. `ClientMessage::TriggerUpdate` is accepted as a transient action alongside `CheckForUpdates` and `ListReleases` (no `Hello` required, no reply frame) and routes directly to `UpdaterHandle::trigger()`. The trigger channel is single-slot so duplicate requests from the settings window and an in-client overlay collapse safely; `UpdateProgress` is broadcast only to registered clients, so the in-client overlay continues to own the live download/verify/install feedback and the restart-required prompt.

### Install Flow

Downloads the platform-specific asset via streaming (no full buffering in memory) and fetches its minisig signature in parallel, then verifies with the embedded real minisign public key.

Downloads are staged in a private per-update runtime directory with owner-only files, download byte caps, and request timeouts. Linux installs keep the verified package fd open, unlink the path, and pass `/proc/{pid}/fd/{fd}` to `pkexec dpkg` so the privileged install reads the verified inode rather than a mutable temp path.

On Linux, installation uses `pkexec dpkg -i`; the Debian maintainer scripts recover the invoking desktop UID from `SUDO_UID` or `PKEXEC_UID` so user services, runtime directories, and hook setup still target the logged-in user. Updater-triggered installs also create a runtime `update-defer-cold-restart` marker first, so `postinst` can report a handoff failure back to the UI with `update-restart-required` instead of immediately killing live sessions. On macOS, it uses `hdiutil attach` + `ditto` and replaces the currently running `.app` bundle derived from `current_exe()` instead of assuming `/Applications/Scribe.app`. Progress is broadcast to all connected clients.

### Rollback

Restores the previous installation if an update fails mid-install.

On macOS, the existing `.app` bundle is renamed to an adjacent `.app.prev` backup before `ditto` copies the new version. If `ditto` fails, that adjacent backup is renamed back to restore the previous version. On Linux, rollback relies on dpkg's own transactional behavior.

### macOS Hot-Reload

After a successful `ditto`, the updater attempts a zero-downtime handoff by running `launchctl kickstart -k` to restart the launchd service in-place.

If `kickstart` is unavailable or fails, it falls back to spawning the new binary with `--upgrade` and waits up to 30 seconds for the handoff to complete. The longer timeout avoids false restart-required fallbacks when large handoff snapshots take longer to transfer and restore. If the handoff still times out, the updater broadcasts `CompletedRestartRequired` to all connected clients and intentionally skips client/settings relaunches so the old processes stay alive until the user approves a cold restart from the UI.

### Configuration

`UpdateConfig` in [[crates/scribe-server/src/config.rs#ScribeConfig]] controls update behavior: `enabled` (bool) to globally toggle the updater, `check_interval_secs` (u64, minimum 300) for the polling period, and `channel` (Stable/Beta) to filter which releases are considered.

The GitHub API endpoint defaults to the official releases URL and can be overridden with the `SCRIBE_UPDATE_API_URL` environment variable.

## Releases

Server-side release-history fetcher and cache that backs the [[settings#Releases]] panel. Independent of the [[server#Updater]] auto-update path; reuses only the shared HTTP client in [[crates/scribe-server/src/updater.rs#http_client]] so connection pooling, DNS, and TLS sessions are shared across the updater and the catalog.

### Release Catalog

In-memory cache held in [[crates/scribe-server/src/releases.rs#ReleaseCatalog]]: an `Option<Vec<Release>>` plus `last_fetched_at`, `last_fetch_was_success`, a `ttl` (defaults to one hour via `ReleaseCatalog::DEFAULT_TTL`), and an `inflight_refresh` flag preventing thundering-herd refreshes.

A `last_refresh_error` string is carried forward into Stale responses. Entries are stale-while-revalidate: when `last_fetched_at` is older than `ttl`, the next request schedules a background refresh and returns [[crates/scribe-common/src/protocol.rs#ReleaseListResultState]]`::Stale { releases, reason }` immediately. On no-cache + fetch failure, [[crates/scribe-server/src/releases.rs#handle_list_releases]] returns `Failed { reason }` and does NOT poison the cache. Per-call branches are computed under the lock by [[crates/scribe-server/src/releases.rs#inspect_locked]] so concurrent callers see the same view of the cache.

### Fetcher

The fetcher is dependency-injected via [[crates/scribe-server/src/releases.rs#ReleaseFetcher]] (trait); the production implementation is [[crates/scribe-server/src/releases.rs#GithubReleaseFetcher]].

It hits `https://api.github.com/repos/sharaf-nassar/scribe/releases?per_page=30` (capped via `MAX_RELEASES = 30`), drops drafts, keeps pre-releases, and runs each release `body` through `pulldown-cmark` (CommonMark + GFM features) → `ammonia::clean` via [[crates/scribe-server/src/releases.rs#render_release_body]] before storing it in `Release.body_html`. Tests inject `StaticFetcher` / `PanicFetcher` implementations via the same trait so the cache state machine and render-and-sanitize pipeline can be exercised without live HTTP.

### Dispatch

[[crates/scribe-server/src/ipc_server.rs]] routes [[crates/scribe-common/src/protocol.rs#ClientMessage]]`::ListReleases` to [[crates/scribe-server/src/releases.rs#handle_list_releases]], which reads the catalog state machine and replies with [[crates/scribe-common/src/protocol.rs#ServerMessage]]`::ReleaseList { state }`.

Background refreshes scheduled by the Stale branch run on the existing tokio runtime via [[crates/scribe-server/src/releases.rs#spawn_background_refresh]] and clear `inflight_refresh` when they finish, regardless of success or failure.

## Configuration

Server config in [[crates/scribe-server/src/config.rs#ScribeConfig]] holds workspace roots and scrollback lines. Roots are validated as absolute paths with tilde expansion. Scrollback is clamped to a maximum of 100,000 lines.

Live `ConfigReloaded` handling in [[crates/scribe-server/src/ipc_server.rs#handle_config_reloaded]] reapplies workspace roots to [[crates/scribe-server/src/workspace_manager.rs#WorkspaceManager#set_roots]], then recomputes workspace names for live sessions.

## Hook Channel

Structured IPC by which AI-tool hook subprocesses report state to the server, replacing the OSC-over-`/dev/tty` path that Claude Code v2.1.139 made unusable.

CC v2.1.139 (2026-05-11) intentionally detached the controlling TTY from hook subprocesses, breaking every `printf > /dev/tty` Scribe hook. The replacement is a new `ClientMessage::HookEvent` variant carried on the existing IPC socket and consumed by [[crates/scribe-server/src/hook_ingress.rs#handle]]. Claude Code, Codex, and the Claude statusline subprocess all route through it. See `specs/003-ai-hook-channel/`.

### Discovery

Scribe injects two env vars into every spawned PTY so hook subprocesses can discover the channel.

The injection site is [[crates/scribe-server/src/session_manager.rs#build_pty_options]]: `SCRIBE_HOOK_SOCK` (absolute path to the existing server socket) and `SCRIBE_SESSION_ID` (per-PTY UUID minted by `SessionManager::create_session`). Both inherit through the user's shell and the AI tool to the hook subprocess. Absence of either signals "not under Scribe" — the helper exits 0 silently (FR-003).

### Emitter

The shared [[crates/scribe-hook-helper/src/main.rs]] binary sends one `HookEvent` per invocation, then exits 0.

CLI parsing via `clap`; both env vars read; payload built; `ClientMessage::HookEvent` length-prefix-msgpack-framed to the socket via the existing `framing::write_message`. A 100 ms `tokio::time::timeout` bounds connect + write + close (FR-012). Provider-specific adapters in `dist/ai-hook-{claude,codex,statusline}.sh` translate the AI tool's hook stdin JSON into the helper's argv.

Claude Code and Codex `UserPromptSubmit` adapters both emit `StateChanged { Processing }` followed by `PromptReceived` when the hook payload contains prompt text, so the prompt bar is driven by the same structured hook event for both providers. Codex additionally derives a `TaskLabelChanged` event from the first non-empty non-slash prompt line and maps `PermissionRequest` to `PermissionPrompt`.

### Ingress

The server dispatches `ClientMessage::HookEvent` on a transient connection (no `Hello`, no `Welcome`, no reply).

The pattern mirrors `CheckForUpdates` / `ListReleases` at `ipc_server.rs` `establish_client_window`. `hook_ingress::handle` looks up the session in `LiveSessionRegistry`, translates the `HookEventKind` to a `MetadataEvent`, and forwards into [[crates/scribe-server/src/ipc_server.rs#send_metadata_event]] — the same downstream pipeline the deleted OSC parser used, unchanged.

### Stop Classifier

[[crates/scribe-server/src/stop_classifier.rs#classify]] maps a `SessionStopped` event's last-message text to `IdlePrompt` or `WaitingForInput`.

One provider-independent Rust function (with inline `#[cfg(test)]` rule tests) replaces the per-provider shell heuristics in the deleted `detect-claude-question.sh` and `detect-codex-question.sh`. Rules: strip fenced code blocks, take the last ~20 non-empty lines, return `WaitingForInput` on trailing `?`, question phrases (`would you like`, `should i`, …), or approval/review phrases.

### Schema

`HookEvent { session_id, provider, kind }` with seven `kind` variants on the wire.

`StateChanged`, `SessionStopped` (server-classified), `StateCleared`, `PromptReceived`, `TaskLabelChanged`, `TaskLabelCleared`, `ContextChanged`. Server-side caps: prompt and task-label 256 chars, last-message 16 KiB. See [[crates/scribe-common/src/hook.rs#HookEvent]] and `specs/003-ai-hook-channel/data-model.md`.

### Adding a Provider

A new AI tool provider plugs in via one adapter script. No transport, server, or env-var changes.

Concretely: (1) add a variant to `AiProvider` in `crates/scribe-common/src/ai_state.rs` with `id`, `display_name`, and `binary_name`; (2) author `dist/ai-hook-<name>.sh` modeled on `ai-hook-claude.sh` and translate the AI tool's hook stdin JSON to `scribe-hook-helper --provider=<id> --event=…` invocations; (3) write a one-off `dist/setup-<name>-hooks.sh` that registers the adapter in that tool's settings file; (4) add the two new files to the deb-asset and DMG-build tables in `crates/scribe-server/Cargo.toml` and `dist/macos/build-dmg.sh`. The shared helper, env-var injection at `session_manager.rs:538`, server ingress at `hook_ingress.rs`, and the stop classifier require **no** changes. Events from a provider not yet recognized by the running build are dropped silently per FR-014.

### Safety Contract

Hook subprocesses must never break the AI tool — even outside Scribe.

The helper exits 0 in every code path (FR-007), writes nothing to stdout (FR-008) or stderr (FR-009), does not open `/dev/tty` (FR-010), and bounds its connect+write+close to 100 ms (FR-012). Absence of `SCRIBE_HOOK_SOCK` or `SCRIBE_SESSION_ID` is the canonical "not under Scribe" signal — the helper exits 0 silently (FR-003). The same holds for unreachable sockets, dead Scribe servers, malformed args, or any other failure. This contract is what makes the AI tool's view of "is Scribe installed?" identical to "is the channel reachable right now?", so Scribe-installed hooks run safely in cloud sessions, subagents, SSH, and CI (FR-025).

## Shell Integration

Shell integration detects the user's shell (Bash, Zsh, Fish, Nushell, PowerShell) and injects startup scripts via shell-specific mechanisms.

Bash uses `--rcfile` to load the integration script, which sources startup files itself; on macOS it mirrors Terminal's login-shell behavior by preferring `~/.bash_profile`/`~/.bash_login`/`~/.profile` before falling back to `~/.bashrc`. Zsh uses `ZDOTDIR` wrapping. Fish and Nushell extend `XDG_DATA_DIRS` so vendor autoload directories are discovered. PowerShell starts with `-NoExit -File` so the integration script is dot-sourced into the interactive session. When `SHELL` is missing, Scribe falls back to the account's login shell from the user database, and default sessions spawn that resolved shell explicitly so Finder- and launchd-started macOS installs do not inherit a stale shell choice.

Shell prompt hooks emit OSC 7 CWD updates, OSC 133 prompt marks, and OSC 1337 `ScribeContext` payloads carrying remote-host and tmux-session labels. Each shell's preexec hook also emits an OSC 1337 `ScribeAiLaunch=<provider_id>` sentinel (see [[pty#PTY#Metadata Parser#OSC 1337 — Pre-Arm Sentinel]]) when the user runs `claude` or `codex`, so the [[pty#PTY#ED 3 Filter]] re-arms before the AI tool emits its initial `\x1b[3J`. This is the counterpart to clearing `ai_provider` on `OSC 133;A` (shell-prompt return): plain shell sessions cleanly leave the filter, and `<tool> --resume` cleanly re-enters it without losing scrollback in between. [[crates/scribe-server/src/ipc_server.rs#send_metadata_event]] also synthesizes a follow-up `ServerMessage::AiStateCleared` on this same `OSC 133;A` whenever the session's live `ai_state` was active, so the client clears its [[client#Prompt Bar]], notification tracker, and [[crates/scribe-client/src/restore_state.rs#LaunchRecord]] `LaunchKind::Ai → Shell` binding in lockstep with the server's internal filter — covering the common case where Claude Code or Codex exit without an explicit `StateCleared` hook event. zsh/fish/nushell/powershell detect the AI binary inside their per-command preexec hook; bash uses a `trap … DEBUG` handler gated on `BASH_SUBSHELL == 0` so subshell expansions during `PROMPT_COMMAND`/`PS1` evaluation do not emit spurious sentinels. Because a DEBUG trap action runs as a command before every interactive command, the handler would otherwise leak its own name into the special `$_` variable; the trap captures `$_` in its action string (`trap '__scribe_emit_ai_launch "$_"' DEBUG`, where `$_` still holds the previous command's last argument at trap-fire time) and restores it as the handler's final command, so an interactive `echo $_` keeps the user's previous last argument. `$?` needs no such handling — bash preserves the exit status across DEBUG traps. zsh's `$_` is unaffected because its `preexec` hooks do not reset it the way a bash DEBUG trap does.

AI tool state and prompt/task-label/context-fill updates do **not** travel through shell integration. They use the structured hook channel — see [[server#Hook Channel]]. The installer scripts `setup-claude-hooks.sh` and `setup-codex-hooks.sh` register thin `dist/ai-hook-{claude,codex}.sh` adapters that invoke `scribe-hook-helper` for every event. `setup-claude-hooks.sh` additionally points Claude's `statusLine` at `dist/ai-hook-statusline.sh`. `setup-codex-hooks.sh` honors its `--hook-source` install prefix, enables both `[features].hooks = true` and `[features].codex_hooks = true`, and writes Scribe entries to `~/.codex/hooks.json` unless an inline `[hooks]` config already exists; in that case it preserves inline form and migrates non-Scribe `hooks.json` entries into `config.toml`. It adds matching `[hooks.state.…]` trusted-hash entries so Scribe command hooks are trusted immediately. It registers `SessionStart`, `UserPromptSubmit`, `PermissionRequest`, `PreToolUse`, `PostToolUse`, and `Stop` hooks, with context refreshes on `PostToolUse` and `Stop`.
