# Server

The scribe-server is a long-running daemon that owns PTY sessions, manages workspaces, and coordinates zero-downtime upgrades.

## Startup

The server initializes in [[crates/scribe-server/src/main.rs#main]] by loading config, creating a SessionManager and WorkspaceManager, then acquiring a singleton lock and binding the IPC socket.

It acquires the singleton lock via flock on `server.lock`. The main loop uses `tokio::select!` over the IPC accept loop, handoff listener, and Ctrl+C signal.

### Upgrade Path

When launched with `--upgrade`, the server restores handoff state and received file descriptors from the old instance instead of starting fresh. It rebuilds the session and workspace managers, then enters the normal event loop.

## Sessions

Each PTY session is represented by a [[crates/scribe-server/src/session_manager.rs#ManagedSession]] during creation and a LiveSession during active operation.

### Session Creation

The SessionManager creates sessions through alacritty_terminal's PTY spawner, wrapping the master fd in an [[crates/scribe-pty/src/async_fd.rs#AsyncPtyFd]] for epoll-driven async I/O. A maximum of 256 concurrent sessions is enforced.

Environment variables are set to TERM=xterm-256color, COLORTERM=truecolor, and TERM_PROGRAM=Scribe on top of the server process environment. On Linux, [[crates/scribe-client/src/ipc_client.rs#sync_linux_service_environment]] refreshes the user systemd manager's GUI session variables before starting the service so new PTY sessions inherit working clipboard/display access.

The terminal core is created with kitty keyboard protocol enabled, so alacritty_terminal can answer Codex and shell keyboard-mode probes (`CSI ? u` and related mode updates) through the normal PTY write-back path.

### Session Activation

Sessions move from the SessionManager (pending) to the LiveSessionRegistry (active) via `activate_pending_sessions`. Each activated session gets a PTY reader task spawned.

### PTY Reader Task

The reader task runs three processing paths per read cycle: raw byte forwarding, ANSI processing through the alacritty_terminal state machine, and metadata extraction via the OSC interceptor.

For Claude Code and Codex Code sessions, an [[crates/scribe-pty/src/ed3_filter.rs#Ed3Filter]] rewrites `\x1b[3J` to `\x1b[2J` before forwarding PTY output to the client and the server's Term. That clears only the visible screen while preserving scrollback in alacritty_terminal during TUI re-renders. The old `/clear` bypass no longer exists.

When `terminal.hide_codex_hook_logs` is enabled, sessions apply the [[pty#Codex Hook Log Filter]] on the same PTY read path before forwarding bytes to the client or the server-side Term. The filter recognizes Codex's current documented hook wrappers for `SessionStart`, `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, and `Stop`, including `Running ... hook: ...` status-message lines and `hook (completed|failed|blocked|stopped)` trailers, so hook command summaries and only the first raw whitespace-only spacer line after them disappear. Interactive Codex redraws some completion rows without a trailing newline, so the filter preserves the control-sequence tail after the last visible hook byte; if the hidden hook prefix had established a background or other SGR styling that later prompt bytes inherit, the kept tail now reapplies that active style state before replaying the remaining bytes. Inside synchronized updates it also trims hook-only rows from the buffered sync block without discarding ANSI-painted blank rows or other control-only repaint tails, which keeps prompt-background repaint bytes in the live stream while still hiding the hook row itself. Because it runs server-side and hot-reloads through `ConfigReloaded`, live output, scrollback, reconnect snapshots, and search all stay consistent.

The server-side ANSI processor also honors VTE synchronized updates (`CSI ? 2026 h/l`). If a sync block remains open past the parser timeout, the reader task flushes the buffered bytes into the server's Term before polling again so snapshots, reconnect, and search do not lag behind buffered Codex output forever.

Normal session PTY output now forwards those raw sync markers to the attached client too. The server no longer strips `CSI ? 2026 h/l` on the live path; instead the client preserves each synchronized-update commit boundary from a single PTY chunk and drains them across redraws so inline Codex and any other DECSYNCUPDATES user can animate normally without diverging from the server's authoritative `Term`.

Metadata events trigger title, Codex task label, CWD, AI state, and bell updates. CWD changes also trigger workspace auto-naming and git branch detection.

Shell integration can also emit OSC 1337 `ScribeContext` metadata describing whether the current pane is remote, which host it is attached to, and the current tmux session name. The server stores that session context in the live session registry and rebroadcasts it on reconnect so the client can label panes before the next prompt redraw.

Terminal query callbacks share that same reader-task path. Clipboard loads, text-area-size reports, device-status replies, and dynamic colour queries are written back to the PTY from the live session state; colour queries fall back to the configured Scribe theme so foreground/background-sensitive TUIs see the real palette.

### Detach and Reattach

Client disconnection clears the client writer but keeps the session alive in the LiveSessionRegistry. Only an explicit `CloseWindow` removes the sessions and window state from future reconnect or handoff.

`ipc_server.rs` remains the transport and message-dispatch layer for `AttachSessions`, but `attach_flow.rs` now owns the reattach sequence itself: attach-entry preparation from live sessions, pre-snapshot Term and PTY resize, stored metadata and workspace replay, screen snapshot delivery, and the delayed client-writer install.

When a new client attaches, the attach flow usually resizes each session's Term and PTY to the client-provided dimensions before taking the snapshot. This ensures the snapshot matches the client's pane grid and absorbs the shell's SIGWINCH response before the client writer is set. Sessions still serving a preserved hot-reload handoff snapshot skip that pre-snapshot resize so a live foreground process cannot redraw over the pre-upgrade history before the first replay reaches the client.

When the connected-client map drops to zero, the server starts a short 250 ms grace timer before asking the singleton settings process to quit over `settings.sock`. If a client reconnects during that grace window, the settings shutdown is skipped so hot-reload or restart handoffs do not spuriously close the settings window.

### Terminal Resize

Resize updates the alacritty_terminal grid and sends `TIOCSWINSZ` via ioctl to notify the foreground process group.

### Git Branch Detection

On CWD change, the server walks up from the working directory (depth limit 50) looking for `.git/HEAD`. It extracts the branch name from `ref: refs/heads/...` or returns the first 8 characters of a detached HEAD commit.

## Workspaces

Managed by [[crates/scribe-server/src/workspace_manager.rs#WorkspaceManager]], workspaces group sessions and track per-window split layouts.

### Auto-Naming

When a session's CWD changes (via OSC 7 or /proc fallback), the server matches it against configured workspace roots and derives the workspace name.

The first path component after the matching root becomes the workspace name. Moving to a different project under the same root updates the name.

### CWD Fallback Detection

If a title change is detected without an accompanying OSC 7 event, the server falls back to reading `/proc/{pid}/cwd` on Linux or calling `proc_pidinfo` on macOS to detect CWD changes.

### Accent Colors

Workspaces cycle through an 8-color palette (indigo, cyan, emerald, rose, amber, lime, pink, cyan) as they are created.

### Per-Window Trees

Each connected window can report its workspace split layout via `ReportWorkspaceTree`. The server persists these trees for handoff and reconnection. A legacy global tree is supported for backward compatibility.

## Handoff

Zero-downtime server upgrades are implemented in [[crates/scribe-server/src/handoff.rs#HandoffState]] using Unix file descriptor passing.

### Protocol

The new server (with `--upgrade`) connects to the old server's handoff socket, sends `SCRIBE_UPGRADE` magic bytes, and receives serialized state plus PTY master fds via SCM_RIGHTS.

An ACK confirms receipt. If the ACK is not received (version mismatch, peer crash), the old server logs the failure and loops back to accept the next connection — it keeps serving until a compatible upgrade succeeds or `postinst` cold-restarts it. The handoff version is tracked to detect incompatible format changes.

### State Transfer

The HandoffState contains per-session metadata and workspace layout state for restart handoff.

Per-session payloads include title, shell basename, remote context, Codex task label, CWD, and AI state, including optional provider conversation IDs used for resume behavior.

Per-session metadata includes ID, workspace, child PID, dimensions, screen snapshot, title, shell basename, remote/tmux context, CWD, AI state, and the launch-time AI-provider hint used for built-in Codex or Claude tabs. File descriptors are transferred one-for-one with the serialized session list.

### Defuse Strategy

Before the old server exits, Pty objects are wrapped in `ManuallyDrop` to prevent their Drop impl from sending SIGHUP to child processes. The new server already holds the master fds via SCM_RIGHTS.

### Size Limits

Maximum handoff state size is 1 GiB. Maximum file descriptors transferred is 1024. Both the sender and receiver verify peer UID for defense-in-depth.

### Version Bumps

Bump [[crates/scribe-server/src/handoff.rs#HANDOFF_VERSION]] when [[crates/scribe-server/src/handoff.rs#HandoffState]] changes incompatibly. Additive per-session fields that use `#[serde(default)]` stay on the current version so hot-reload can still accept state from the immediately previous server.

A mismatch causes the handoff to fail and `postinst` to cold-restart, killing all sessions. Do NOT bump for code-only changes or backward-compatible additive state fields — those are handled by normal hot-reload.

On Linux that cold-restart path must also clean up any detached `scribe-server --upgrade` process left behind by the failed handoff before starting the user service again; otherwise the stale process can keep `server.sock` and `server.lock`, causing the restarted unit to fail with "another scribe-server is already running".

### Binary Change Detection

All three binaries (server, client, settings) use SHA-256 hash comparison to skip unnecessary restarts during upgrades, and Linux server upgrades also track a persisted runtime-generation stamp for launcher and service behavior changes.

On Linux, `postinst` compares each running binary (`/proc/PID/exe`) against the installed copy and also checks whether the desired `server-runtime-generation` differs from the stamp recorded in `/run/user/{uid}/{app}/server-runtime-generation`. That stamp is an opaque SHA-256 signature derived from the launch-critical `postinst` functions and the installed user service unit, not a hand-maintained integer, so maintainer-script and service-launch changes automatically force hot-reload even when the server binary is unchanged. When `postinst` launches replacement user processes, it prefers GUI session variables from `systemctl --user show-environment` and only falls back to the invoking shell for values the user manager lacks. Client relaunches now wait for the previously running client PIDs to exit before spawning the replacement and skip relaunch if an old client refuses to die, which prevents a fresh client from receiving an empty `SessionList` and cold-restoring a duplicate window while the server still marks the old window connected. If Linux must fall back to a cold server restart, the package script still relaunches any previously running client window even when the client binary is unchanged because that client exits on `ServerDisconnected`. On macOS, the [[server#Updater]] compares old (`.app.prev`) and new app bundle binaries before deciding which components to restart. Hash comparison failure is treated as "changed" for safety. Use `just restart-server` for manual hot-reload.

## Updater

Background update checker in [[crates/scribe-server/src/updater.rs#UpdaterHandle]] that polls GitHub releases and installs verified updates with platform-specific strategies.

### Check Cycle

After a 30-second initial delay, the updater checks on a configurable interval (from `UpdateConfig.check_interval_secs`, minimum 300 seconds) via a single `fetch_latest_release()` call to the GitHub releases API.

Stable channel filters out drafts and prereleases; Beta channel includes prereleases. The endpoint can be overridden via the `SCRIBE_UPDATE_API_URL` environment variable for testing. On failure, one retry is attempted after a 5-second backoff before giving up until the next cycle. Dismissed versions remain suppressed until a newer version appears.

### Install Flow

Downloads the platform-specific asset via streaming (no full buffering in memory) and fetches its minisig signature in parallel, then verifies with the embedded real minisign public key.

On Linux, installation uses `pkexec dpkg -i`; on macOS, it uses `hdiutil attach` + `ditto`. Progress is broadcast to all connected clients.

### Rollback

Restores the previous installation if an update fails mid-install.

On macOS, the existing `.app` bundle is renamed to `.app.prev` before `ditto` copies the new version. If `ditto` fails, `.app.prev` is renamed back to restore the previous version. On Linux, rollback relies on dpkg's own transactional behavior.

### macOS Hot-Reload

After a successful `ditto`, the updater attempts a zero-downtime handoff by running `launchctl kickstart -k` to restart the launchd service in-place.

If `kickstart` is unavailable or fails, it falls back to spawning the new binary with `--upgrade` and waits up to 10 seconds for the handoff to complete. If the handoff times out, the updater broadcasts `CompletedRestartRequired` to all connected clients so the UI can prompt the user to restart manually.

### Configuration

`UpdateConfig` in [[crates/scribe-server/src/config.rs#ScribeConfig]] controls update behavior: `enabled` (bool) to globally toggle the updater, `check_interval_secs` (u64, minimum 300) for the polling period, and `channel` (Stable/Beta) to filter which releases are considered.

The GitHub API endpoint defaults to the official releases URL and can be overridden with the `SCRIBE_UPDATE_API_URL` environment variable.

## Configuration

Server config in [[crates/scribe-server/src/config.rs#ScribeConfig]] holds workspace roots and scrollback lines. Roots are validated as absolute paths with tilde expansion. Scrollback is clamped to a maximum of 100,000 lines.

## Shell Integration

Shell integration detects the user's shell (Bash, Zsh, Fish, Nushell, PowerShell) and injects startup scripts via shell-specific mechanisms.

Bash uses `--rcfile` to load the integration script, which sources startup files itself; on macOS it mirrors Terminal's login-shell behavior by preferring `~/.bash_profile`/`~/.bash_login`/`~/.profile` before falling back to `~/.bashrc`. Zsh uses `ZDOTDIR` wrapping. Fish and Nushell extend `XDG_DATA_DIRS` so vendor autoload directories are discovered. PowerShell starts with `-NoExit -File` so the integration script is dot-sourced into the interactive session. When `SHELL` is missing, Scribe falls back to the account's login shell from the user database, and default sessions spawn that resolved shell explicitly so Finder- and launchd-started macOS installs do not inherit a stale shell choice.

Those prompt hooks also clear any stale Codex task label as soon as control returns to the shell. They emit OSC 7 CWD updates, OSC 133 prompt marks, and OSC 1337 `ScribeContext` payloads carrying remote-host and tmux-session labels. Separately, `setup-codex-hooks.sh` installs Codex hooks that emit a new task label from the first non-command prompt in each task thread, keeping Codex tab names independent from normal OSC 0/2 titles.

Because current Codex command hooks always receive a JSON payload on stdin, Scribe's installed Codex hook helpers drain stdin before exiting even when they only emit OSC side effects. They probe `/dev/tty` directly for the controlling terminal instead of checking stdin, which keeps Bash `PreToolUse` and `PostToolUse` state hooks from failing with broken pipes on larger payloads.
