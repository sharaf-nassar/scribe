# Server

The scribe-server is a long-running daemon that owns PTY sessions, manages workspaces, and coordinates zero-downtime upgrades.

## Startup

[[crates/scribe-server/src/main.rs#parse_args]] handles `--help`, `--version`, and invalid arguments before server setup.

Those paths return before environment, logging, runtime, lock, socket, or PTY setup. Valid `--upgrade` and `--launchd-slot=primary|alternate` arguments keep their existing handoff and launchd startup modes.

The server then initializes by loading config, creating a SessionManager and WorkspaceManager, then acquiring a singleton lock and binding the IPC socket. It acquires the singleton lock via flock on `server.lock`. The main loop uses `tokio::select!` over the IPC accept loop, handoff listener, and Ctrl+C signal.

### Local Admission

Local-socket admission is charged in two stages against three independent semaphores, so no class of connection can starve another (spec 017 US5-5).

[[crates/scribe-server/src/ipc_server.rs#start_ipc_server]] can only reserve a *pending* permit (`LOCAL_PENDING_CAP`, 64) when it accepts a stream, because a connection's class is unknowable until its first frame arrives; that permit is what bounds accepted-but-unclassified connections and the tasks they spawn. [[crates/scribe-server/src/ipc_server.rs#establish_client_window]] then exchanges it exactly once through [[crates/scribe-server/src/ipc_server.rs#LocalSlot#claim]]: a `Hello` — or a legacy no-`Hello` claim, still how `scribe-cli` connects — takes one of the 32 long-lived client slots (`MAX_CONNECTIONS`), and a one-shot transient action takes one of the 16 `MAX_TRANSIENT_CONNECTIONS` slots. The replacement permit is acquired before the pending one is released, so an exchange can never over-admit either pool. A full pool closes the connection, and the client pool logs the same "connection limit reached" refusal it always did.

Splitting the pools is what makes a hook burst survivable. Hook events arrive as transient connections ([[server#Hook Channel#Ingress]]), so while they shared one 32-slot semaphore a burst could hold every client slot and lock new windows out of the server; the client pool is also the only local class whose output queue can grow large, so it still bounds per-connection memory. Which first frames count as transient is decided by [[crates/scribe-server/src/ipc_server.rs#is_transient_first_frame]] — exactly the arms that answer at most one frame and register nothing.

A local first frame is read under `LOCAL_PRE_HELLO_TIMEOUT` (5 s). Every local caller writes immediately after connecting, so a still-silent connection is an abandoned or half-open dialer sitting on a pending slot; reads after the first frame stay untimed, since an idle window is legitimate. Remote transports keep their own caps and their own idle timeout — see [[server#Remote Control#Accept Path]].

### Frame Error Policy

The server skips an undecodable MessagePack payload only after the framing layer has consumed that payload completely and preserved alignment.

Both [[crates/scribe-server/src/ipc_server.rs#establish_client_window]] and [[crates/scribe-server/src/ipc_server.rs#run_client_message_loop]] then read the next frame, so one incompatible or corrupt message does not discard an otherwise usable connection. Framing I/O failures, oversized lengths, and timeouts still close because the stream may be incomplete or its next boundary unknown. Remote transport preambles are a deliberate fail-closed exception before authorization. See [[protocol#Protocol#Transport#Frame Format]].

### Upgrade Path

When launched with `--upgrade`, the server restores handoff state and received file descriptors from the old instance instead of starting fresh.

The IPC socket is already bound by the time restoration starts — the handoff claims it before acknowledging (see [[server#Server#Handoff#Socket Takeover]]) — so clients that dial during the restore queue in the kernel backlog rather than meeting a refusal. Both startup paths hand an already-acquired listener to [[crates/scribe-server/src/main.rs#run_server_loop]], which no longer binds anything itself.

It rebuilds the session and workspace managers, filtering workspace and window membership against the received live-session set so stale IDs from older servers are dropped before serving. An empty-workspace payload or any dropped sessions are logged at WARN with counts by [[crates/scribe-server/src/workspace_manager.rs#WorkspaceManager#restore_from_handoff]].

An upgrade server has no durable stdio of its own — Debian `postinst` redirects it to a state-dir `upgrade.log`, while the macOS LaunchAgents use `/dev/null` — so `--upgrade` startup mirrors tracing into `server.log` under the app state dir via [[crates/scribe-server/src/main.rs#open_server_log_file]], rotated once to `server.log.1` when it exceeds 8 MiB.

`upgrade.log` is the postinst watchdog's readiness channel and stays the successor's stdio for the rest of its life, so `postinst` truncates it at spawn instead of deleting it after the ready check. Deleting it left the new server appending to an unlinked tmpfs inode.

## Agent API

The server adapts its authoritative PTY, workspace, window-share, and automation state into one-shot local agent calls without adding a listener or registering an agent window.

[[crates/scribe-server/src/ipc_server.rs#is_transient_first_frame]] classifies `AgentRequest` with the bounded transient pool. [[crates/scribe-server/src/ipc_server.rs#handle_transient_agent_request]] supplies registry and client seams to [[crates/scribe-server/src/agent_api/mod.rs#dispatch]], then sends exactly one `AgentResponse` before the socket closes. Agent traffic never attaches a session, claims a `WindowId`, resizes a PTY, or enters a remote transport.

### Admission and capability policy

Every request is admitted; every operation except `Capabilities` is policy-authorized before protected target or session lookup.

Prompt delivery may resolve caller orientation to choose a window, but capture, target-session, and action execution seams remain after authorization.

[[crates/scribe-server/src/agent_api/mod.rs#AgentApiState]] admits four concurrent calls. `WriteInput` byte length is checked before admission can raise a prompt; excess requests return typed `Busy`, and oversized input returns `TooLarge` without touching a session or client.

[[crates/scribe-server/src/agent_api/policy.rs#AgentPolicyEngine]] resolves `Deny`, `Allow`, or `Prompt` per capability. A live `origin_session_id` routes a prompt only to its owning window's capable local client; if that window cannot render it, prompt mode denies rather than using another window. Originless or stale callers use the deterministic capable-window fallback. Prompt mode otherwise denies when no local `agent_api` client exists, issues one correlated prompt, and parks up to 64 same-key requests behind it. The key is caller-supplied agent label, capability, and target. Decisions are reused for the configured burst window; timeout denies; `AlwaysAllow` and `AlwaysDeny` update only the matching in-memory axis.

`ConfigReloaded` projects the fresh `[agent_api]` table through [[crates/scribe-server/src/config.rs#project_config]] and [[crates/scribe-server/src/agent_api/mod.rs#AgentApiState#refresh_policy]]. Refresh cancels pending prompts and takes effect on the next call without restarting the server. An all-`Deny` policy also releases held activity leases; it is the off state, not a separate master switch.

### World and siblings

Metadata reads copy one coherent, allowlisted view from the live-session, window-share, and workspace registries.

[[crates/scribe-server/src/agent_api/world.rs#capture]] takes read guards in the order live sessions, window shares, workspace manager, stamps one `snapshot_id` and capture time, then releases the guards before DTO formatting. Windows reuse `ListWindows` workspace names, session counts, connection state, sharing mode, and participant count. Session copies include only identity, title, CWD, provider/state, task label, and context fill; retained prompts, conversation ids, model/tool/agent metadata, environment envelopes, and participant identities never enter the capture type.

`World` returns every window, workspace, and live session server-wide. A matching `origin_session_id` marks exactly one session as `is_caller`; a missing or stale origin marks none. `Siblings` filters the same captured snapshot to the origin session's window and returns typed `NotFound` when the origin is absent or stale.

### Screen reads

Screen reads copy only the requested viewport and bounded trailing scrollback while holding the terminal lock, then normalize text after releasing it.

[[crates/scribe-server/src/agent_api/text.rs#copy_rows]] excludes alternate-screen history and copies characters, spacer state, and soft-wrap state only. [[crates/scribe-server/src/agent_api/text.rs#format_rows]] joins soft wraps, preserves hard breaks, trims blank tails, skips wide-character spacers, keeps OSC 8 labels without URIs, replaces each image-placeholder run with `[image omitted]`, and cuts at a UTF-8 boundary under `max_response_bytes`, setting `truncated` when content is dropped.

The reply includes session id, title, CWD, logical line count, capture time, and snapshot id. Policy is evaluated before the live-session lookup, so `Deny` returns no content bytes; an approved missing session returns `NotFound`.

The configured `max_response_bytes` caps only the text field, so [[crates/scribe-server/src/agent_api/mod.rs#enforce_serialized_response_ceiling]] re-measures the complete serialized `ServerMessage::AgentResponse` and re-truncates screen text at UTF-8 boundaries until the whole reply fits the hard 256 KiB `AGENT_MAX_RESPONSE_BYTES_CEILING`, marking `truncated`. A screen reply whose metadata alone exceeds the cap becomes typed `TooLarge`.

### Actions and input

Mutation calls reuse existing window action handlers and live PTY writers, adding policy and completion semantics rather than a second implementation.

`DispatchAction` maps every `AutomationAction` exhaustively to either `DispatchAction` or `DispatchDestructiveAction`; close-pane, close-tab, and update-dialog actions use the destructive gate. [[crates/scribe-server/src/ipc_server.rs#run_agent_action]] accepts an explicit connected window or the sole connected window, returns `AmbiguousTarget` when omission cannot select exactly one, and waits for `RunActionCorrelated` completion. Queue refusal, timeout, or client disconnect becomes `ActionFailed`; successful session-creating actions return the created session id.

Agent writes are bounded before prompting, authorized once, and acknowledged only after the complete PTY write. [[crates/scribe-server/src/agent_api/mod.rs#write_agent_input]] appends exactly one carriage return only when `submit` is true and awaits `write_all`; an absent live session returns `NotFound` and write failure returns `ActionFailed`.

### Activity leases

Per-session activity uses reference-counted leases so overlapping calls cannot clear another call's visible state.

[[crates/scribe-server/src/agent_api/activity.rs#AgentActivityTracker]] emits one active edge on the first lease and one inactive edge after the last release plus the configured dwell. Reacquisition during dwell cancels the pending clear; caller teardown can release only that caller's leases, while an all-`Deny` refresh releases all leases.

`ReadScreen` and `WriteInput` hold leases for their named session, and authorized actions hold one under the origin rules in [[server#Server#Agent API#Action activity leases]]. [[crates/scribe-server/src/ipc_server.rs#spawn_agent_activity_forwarder]] resolves each transition to the session's window and sends `AgentActivity` only to participants that advertised the `agent_api` capability bit.

### Metadata-only call audit

Every completed call emits one named `agent_call` event on target `scribe::agent_api`.

Prompted calls remain inside the same dispatcher invocation, so prompt resolution cannot create a second record. The event contains only `agent_label`, capability, target kind and id, decision, and `response_bytes`.

Target kind is constrained to `server`, `window`, or `session`; no request body, reply body, error text, or terminal text is recorded. `response_bytes` is the named-MessagePack byte length of the `ServerMessage::AgentResponse` body that the socket writer serializes, excluding the 4-byte frame-length prefix described by [[protocol#Protocol#Transport#Frame Format]].

### Action activity leases

Authorized agent actions derive visibility only from explicit request semantics.

After policy authorization and unambiguous target-window resolution, `FocusSession` uses its explicit session id; every other action uses `origin_session_id` only while that session still belongs to the target window. An absent, stale, or cross-window origin leaves the action sessionless rather than guessing from a multi-region window tree.

[[crates/scribe-server/src/ipc_server.rs#run_agent_action]] acquires the shared per-session activity lease before queuing `RunActionCorrelated` and retains it through correlated completion, failure, disconnect, or timeout. Dropping it then follows the same configured dwell and overlapping refcount rules as screen reads and input writes. Policy-denied and ambiguous actions acquire no lease and emit no activity transition.

## Sessions

Each PTY session is represented by a  during creation and a LiveSession during active operation.

### Session Creation

The SessionManager creates sessions through alacritty_terminal's PTY spawner, wrapping the master fd in an  for epoll-driven async I/O. A maximum of 256 concurrent sessions is enforced.

[[crates/scribe-server/src/ipc_server.rs#handle_create_session]] carries optional structured AI launch intent into [[crates/scribe-server/src/session_manager.rs#SessionLaunchRequest]]. The server owns argv construction and takes the provider hint from the structured field; the client never builds a command string.

#### AI tabs are plain tabs that run through their shell

An AI tab is not its own session class. [[crates/scribe-server/src/session_manager.rs#build_launch_shell]] owns the whole decision: the ordinary [[crates/scribe-server/src/session_manager.rs#build_shell]] argv with [[crates/scribe-server/src/session_manager.rs#tool_exec_args]] appended, so the provider inherits exactly what a plain tab inherits.

That means the same [[crates/scribe-common/src/shell.rs#default_shell_program|default shell resolver]], the same integration attachment (bash `--rcfile`, zsh `ZDOTDIR`, fish `XDG_DATA_DIRS`), and the same startup files every other tab reads. Only an interactive `-c` command is added. Every provider runs in normal shell command position and then exits the shell with its status. This is necessary because Bash treats a provider name in `exec <provider>` as an `exec` argument, bypassing same-named functions and aliases from the rc file. The status exit still ends the session and closes the tab when the provider exits rather than dropping the user at a stray prompt. An AI tab also cannot resolve a different `PATH` than the tab beside it — the earlier design launched a login shell, which on bash reads only the first of `.bash_profile` / `.bash_login` / `.profile` and therefore missed `~/.bashrc`, where most users keep `PATH`; a provider that was on `PATH` in every plain tab was then a command-not-found command that killed the AI tab on sight.

Zsh and fish are the one exception carried in the appended command. Both schedule their restore-delta apply for the first `precmd` so it lands after user rc, and an AI tab runs its provider before any prompt, so the `-c` command consumes and deletes the staged delta itself — still after rc, still before the provider, satisfying spec-006 FR-008. Bash needs no equivalent because `scribe.bash` applies the delta inline while being sourced, which is already post-rc. Nushell takes `-i -c` because it rejects the grouped short form, and gets no integration under any `-c` variant (its vendor autoload is REPL-only). PowerShell speaks neither `-i` nor `exec`, so it runs the provider through `-NoLogo -Command` and drops the `-File` integration attachment, whose argument has to come last; pwsh exits when that command returns, so the tab still ends with the CLI even without an exec to replace the process. Prompt OSC marks, per-prompt env-delta emission, and baseline emission still install on an AI tab exactly as on a plain one — they simply never fire, because the shell exits before its first prompt. The server-injected absolute `SCRIBE_HOOK_HELPER` is independent of `PATH` ordering, so hook delivery does not depend on which flavor a profile puts first. If the requested AI binary is absent, the shell prints its normal command-not-found diagnostic and the tab exits; no separate client error protocol is added.

#### Tool tabs are plain tabs that run through their shell

A launch-only [[protocol#Protocol#Client Messages#Session Lifecycle#Launch-only tool intent|ShellTool]] tab reuses that same path rather than adding a third session class.

[[crates/scribe-server/src/session_manager.rs#launch_exec_command]] is the only fork: structured AI intent yields the provider command with its resume arguments, while `ShellTool::Pi` yields the same shell-specific `pi; exit <status>` form as structured Pi. Neither yields `None` for the untouched plain-tab argv. Whatever it returns is a string, so [[crates/scribe-server/src/session_manager.rs#build_launch_shell]] and [[crates/scribe-server/src/session_manager.rs#tool_exec_args]] cannot tell a structured Pi tab from a legacy tool tab: both preserve the integration attachment, zsh/fish restore-delta consumption, Nushell `-i -c` form, and PowerShell `-NoLogo -Command` fallback.

[[crates/scribe-server/src/session_manager.rs#SessionLaunchRequest#normalize]] makes structured intent authoritative before shell detection: AI clears both `shell_tool` and legacy `command`, while `shell_tool` clears `command`. The resulting precedence is `ai_launch > shell_tool > command`, so an arbitrary simultaneous command cannot replace the host shell executable or receive structured provider arguments. The binary name comes from the [[crates/scribe-common/src/protocol.rs#ShellTool]] variant rather than the wire, so no client-supplied text reaches the shell command string.

The legacy shell-tool request itself still carries no conversation or resume mode, but Pi is now a user-visible [[crates/scribe-common/src/ai_state.rs#AiProvider]]. Ambient command detection recognizes the `pi` binary, structured Pi hook events can promote the live session into the shared AI-state pipeline, and Pi uses the same preserved-scrollback gate as the other visible providers. Its resume capability is explicitly false and its resume argument list empty, so server argv construction never adds a Pi resume command.

After normalization, [[crates/scribe-server/src/session_manager.rs#ResolvedShell#for_request]] preserves legacy explicit commands and otherwise uses the host's env-first [[crates/scribe-common/src/shell.rs#default_shell_program|default shell resolver]], while [[crates/scribe-server/src/session_manager.rs#build_shell]] keeps bash `--rcfile`, zsh `ZDOTDIR`, fish/nushell vendor autoload, and PowerShell `-File` integration. Feature 018 briefly made AI tabs a second session class with their own login-shell resolution and script gates; unifying them back onto this one path deleted that resolver, the `SCRIBE_AI_TAB` mode in all three integration scripts, and the per-shell pre-exec preambles. Resident shells keep the prompt and env-capture hooks a provider tab never reaches, so nothing was lost by sharing the path — the AI or tool tab exits before running them.

Typed tool identity follows the same retention spine as AI hints: `PreparedSessionLaunch` → `ManagedSession` → `LiveSession`, then `SessionInfo` for warm client reconstruction and `HandoffSession` for server replacement. The handoff field is additive with `#[serde(default)]`; restore chooses AI metadata first, then `ShellTool`, then shell, matching client reconstruction.

That cap is a semaphore, not a map length. [[crates/scribe-server/src/session_manager.rs#SessionManager#create_session]] takes a [[crates/scribe-server/src/session_manager.rs#SessionSlot]] permit before it spawns the PTY, and the permit rides on the session into its LiveSession registry entry, so a slot is returned exactly when the session is dropped on a close path — the SessionManager's own map is only a staging area and its length never counted anything. Because the take is a single non-blocking `try_acquire`, a burst of concurrent creates admits exactly the number of free slots and every loser gets the typed `SessionLimitReached` variant of [[crates/scribe-common/src/error.rs#ScribeError]] immediately instead of queueing behind an unrelated session's close. Handoff-restored sessions take slots the same way, so a hot reload cannot restart the budget from zero.

Environment variables are set to TERM=xterm-256color, COLORTERM=truecolor, and TERM_PROGRAM=Scribe on top of the server process environment. On macOS, launchd starts the server with only `/usr/bin:/bin:/usr/sbin:/sbin`, so [[crates/scribe-server/src/session_manager.rs#build_pty_options]] floors every session's PATH through [[crates/scribe-server/src/session_manager.rs#path_with_macos_baseline]]: both Homebrew prefixes (`/opt/homebrew/bin` for Apple Silicon, `/usr/local/bin` for Intel) are prepended when absent — matching `brew shellenv` ordering — and the system directories appended; non-empty inherited entries keep their order, while empty entries (POSIX implicit-cwd, e.g. `::` or a leading/trailing `:`) are deliberately dropped as a safety measure. This single funnel covers plain, AI, and SSH-local sessions, so shells without login-profile emulation (fish, nushell, PowerShell) still get a Homebrew-capable baseline; it is one of three PATH layers, alongside the launchd plist `EnvironmentVariables` and the bash/zsh in-shell login-profile emulation. Scribe deliberately spawns non-login shells (bash integration rides `--rcfile`, which login bash ignores) and emulates login sourcing inside the integration scripts instead of capturing a login-shell environment up front. On Linux,  refreshes the user systemd manager's GUI session variables before starting the service so new PTY sessions inherit working clipboard/display access. Packaged user services are enabled under `graphical-session.target`, not `default.target`, so display-manager autostart waits until DISPLAY/XAUTHORITY are available.

New and handoff-restored terminal cores are created with kitty keyboard protocol enabled, so alacritty_terminal can answer Codex and shell keyboard-mode probes (`CSI ? u` and related mode updates) through the normal PTY write-back path.

### Session Activation

Sessions move from the SessionManager (pending) to the LiveSessionRegistry (active) via `activate_pending_sessions`. Each activated session gets a PTY reader task spawned.

### PTY Reader Task

The reader task runs three processing paths per read cycle: raw byte forwarding, ANSI processing through the alacritty_terminal state machine, and metadata extraction via the OSC interceptor.

Terminal-image chronology is observed inside the ANSI path, not by replaying
PTY bytes through a second parser. A delegating handler forwards every callback
to the same real `Term` and records only cursor, saved cursor, deferred wrap,
screen, margin, mode, dimension, scroll, and erase facts. Completed graphics
boundaries may split one processor feed, but every byte is still consumed once.
Image-state rejection uses one full-span delegating feed because no trustworthy
graphics cuts committed. Synchronized-update timeout flushes also use the
delegating handler, but publish state/effects without claiming a new byte span.

The session's payload-free observer is also carried by attach and live resize
handles. Every production resize observes the real post-resize `Term`, whose
Alacritty path resizes active and inactive grids together, before publishing an
internal both-grid resize effect. Inactive cursor facts become unavailable
until that grid is activated and read from the real `Term`; dimensions remain
exact for both grids. No image update is fanned out at this stage.

The read is raced against a per-session cancellation signal, and the task's `JoinHandle` is retained instead of discarded. Both live on the session's [[crates/scribe-server/src/session_exit.rs#SessionExitGate|exit gate]], shared by the `LiveSession` and the reader. The gate exists because the PTY master fd is duplicated three ways — the reader's `AsyncPtyFd`, the resize fd, and the `Pty` parked on the `LiveSession` — so dropping any one of them never delivers EOF. A child that ignores SIGHUP (`trap '' HUP; sleep inf`) would otherwise park the reader on a `read()` that can never complete, with no handle left to stop or bound it. Every select arm is cancel-safe, so losing the race never drops a wakeup or a byte.

The gate also holds the exactly-once exit funnel. Reader EOF, a reader read error, reader cancellation, an explicit `CloseSession`/`CloseWindow`, and the child-exit watcher all call [[crates/scribe-server/src/ipc_server.rs#finalize_session_exit|one idempotent finalizer]]; a compare-and-swap on the gate elects exactly one of them to emit `SessionExited` and unwire the session from the live registry, the attachment set, and the workspace manager. A close racing the child's own death therefore neither double-emits nor drops the notification. The end of the master stream only proves that every slave fd closed, which a live child can do on its own, so when a child-exit watcher is armed it is the authoritative emitter and the reader yields to it; handoff-inherited sessions arm no watcher and keep that path with `exit_code: None`. Both stream endings count: Linux answers a read on a master whose last slave closed with `EIO` rather than a zero-length read, so the ordinary "the shell exited" case reaches the reader as an error, not an EOF.

Winning that CAS also cancels the reader, because the winner is not always a path that already did. Both close handlers cancel their gates themselves, but the child-exit watcher finalizes a session whose reader can still be parked on a `read()` that will never complete: a descendant that inherited the slave fd keeps the master alive long after the child it belongs to is gone, which is exactly why exit detection cannot ride on the stream ending. Left running, that reader outlives its registry entry, feeding a `Term` and sinks nobody can reach — and it spins, because the only sender on its OSC 52 command channel lived on the `LiveSession` the finalizer just dropped and a closed receiver completes instantly and forever. The clipboard arm [[crates/scribe-server/src/ipc_server.rs#next_clipboard_command|parks]] once that channel closes rather than completing, which also quiets the stretch between a `CloseWindow` removing its sessions and its post-reply cancel.

That watcher is a per-session [[crates/scribe-server/src/child_watch.rs#ChildExitWatcher|pidfd wait]], opened at spawn and armed before the reader task starts so a child that dies immediately still finds the funnel pointed at it. When the pidfd becomes readable the watcher *peeks* the wait status with `waitid(..., WNOWAIT)` instead of reaping it: the child stays a zombie, its PID cannot be recycled underneath a later `kill`, and `Pty::Drop`'s blocking `waitpid` still owns the reap. A normal exit and a signal death travel in separate `SessionExited` fields, so a `SIGKILL` is never read back as a status. The watcher waits for the reader to finish draining before it emits, bounded at two seconds, because the child's death and its last write are independent wakeups and a client that retired the pane first would lose the tail output; the bound only bites when a descendant inherited the slave fd, where the reader may never see EOF at all. Sessions with no pidfd — handoff-inherited children, whose child predates this server process, and any platform without `pidfd` — arm nothing and stay on the EOF path.

The `Pty` itself is never dropped inline. `LiveSession` parks it in a [[crates/scribe-server/src/pty_guard.rs#PtyGuard|PtyGuard]], and every path that ends a session — both close handlers and the finalizer's registry removal — hands it to [[crates/scribe-server/src/pty_guard.rs#PtyGuard#teardown|teardown]], which moves the `Pty` onto the blocking pool. `Pty::Drop` still does the SIGHUP and the `waitpid`, but now off every Tokio worker and after every lock is released; `CloseWindow`, which used to run it under the global live-session write guard, collects the guards inside that guard and tears them down outside it. A child that ignores SIGHUP therefore parks one blocking-pool thread instead of a worker and the registry. Dropping a guard nobody tore down takes the same off-worker route, so a missed call site degrades to a late reap rather than a stall.

That parked thread is bounded as well, because moving an unbounded `waitpid` off the workers does not shorten it. Every teardown arms a watchdog that escalates to SIGKILL once [[crates/scribe-server/src/pty_guard.rs#TEARDOWN_KILL_GRACE|two seconds]] have passed with no reap, and on Linux it signals through a pidfd opened before the drop, so a kill that races the reap goes stale rather than landing on a recycled PID. The watchdog is a plain OS thread rather than a blocking-pool task because the moment it matters most is runtime shutdown, where a queued task is dropped instead of run and the `Pty` is dropped inline on the very thread doing the shutting down.

Both close handlers follow one take-then-release-then-join protocol. Under the live-session write guard — which covers the removals and nothing else, and never spans an `.await` — the close takes the registry entry with its `PtyGuard` and exit gate; it releases that guard before acquiring the workspace-manager one for the workspace-side removal, so the two are never held at once. Only then does it cancel the gate, hand the `Pty` to the blocking pool, and [[crates/scribe-server/src/session_exit.rs#SessionExitGate#join_reader_by|wait for the reader]] under a two-second bound, detaching the task with a `warn!` if the bound expires. That order is what keeps the two write locks off the wait: a reader finalizing itself takes both of them, so joining under either would stall for the full bound. It also means a wedged reader delays nothing but the exit notification — the session is already gone from the registry, the workspace, and the client's attached set — while in the ordinary case the reader wins the funnel CAS during the join and its last bytes reach the client ahead of `SessionExited`. `CloseWindow` cancels every pane before joining any and shares one deadline across them, so a whole window of wedged readers costs one bound rather than one per pane.

Those two guards are the server-wide lock order: live sessions before workspace manager, never the reverse, and neither held across an `.await`. `CloseWindow` reads the workspace first to resolve the window's session ids, but that read guard is a statement temporary and is gone before the registry guard is taken.

Process shutdown runs the same cancel-and-bounded-join from [[crates/scribe-server/src/ipc_server.rs#shutdown_pty_readers|shutdown_pty_readers]], with one difference: it claims every exit CAS before cancelling, so the funnel each reader reaches is already taken and nothing is published — the IPC socket dies with the process, and the children are left to the `PtyGuard`s still parked on their sessions. A handoff exit skips it entirely and only defuses, because cancellation drives the funnel and the funnel would report `SessionExited` for panes the incoming server is about to keep serving. What follows the shutdown is bounded too: `main` ends the runtime with an explicit [[crates/scribe-server/src/main.rs#RUNTIME_SHUTDOWN_GRACE|five-second]] `shutdown_timeout` instead of letting `Runtime`'s drop wait on the blocking pool without one, so no blocking call still in flight — a child's `waitpid`, a keystore round trip, an interface scan — can hold the process open after `main` itself is done, and `just restart-server` and `--upgrade` inherit that ceiling rather than a stuck child's lifetime.

For supported AI coding sessions, an  strips `\x1b[3J` before forwarding PTY output to the client and the server's Term. Prompt text, attention/error states, and inactive markers start a scrollback trim epoch; the first suppressed clear captures the baseline after replay, and later suppressed clears in that epoch trim both Terms back to it before replaying the redraw bytes. This keeps committed AI transcript history while preventing inline AI redraws from piling duplicate frames into scrollback. Suppression emits no synthetic `ScrollBottom`, so a client scrolled into history keeps its viewed anchor. The old `/clear` bypass no longer exists.

A chunk those filters empty is dropped rather than framed. [[crates/scribe-server/src/ipc_server.rs#send_pty_output|send_pty_output]] returns early on a zero-byte slice, so a read that is exactly `\x1b[3J`, or one held entirely in a filter's partial-match state, costs no allocation, no serialize, no queue slot, and no coalesce or parse pass on any attached client. The guard sits at the single place `PtyOutput` frames are built, so it covers the injected-DECRST path too. Nothing downstream loses a cursor value: an empty chunk leaves the commit cursor where it was, and an accompanying `TrimScrollback` carries its own cursor value.

The server-side ANSI processor also honors VTE synchronized updates (`CSI ? 2026 h/l`). If a sync block remains open past the parser timeout, the reader task flushes the buffered bytes into the server's Term before polling again so snapshots, reconnect, and search do not lag behind buffered Codex output forever.

Normal session PTY output now forwards those raw sync markers to the attached client too. The server no longer strips `CSI ? 2026 h/l` on the live path; instead the client preserves each synchronized-update commit boundary from a single PTY chunk and drains them across redraws so inline Codex and any other DECSYNCUPDATES user can animate normally without diverging from the server's authoritative `Term`.

Metadata events trigger title, provider task label, CWD, AI state, prompt text, and bell updates. CWD changes also trigger workspace auto-naming and git branch detection.

Live sessions retain shell basename, OSC 0/2 window title, OSC 0/1 icon title, and provider task label independently. Blank title events clear only their source; provider exit clears stale task labels with the rest of its AI chrome.

A shell prompt returning (OSC 133 `A`) while mouse-reporting or focus-event modes are still active means the foreground program died without cleanup — e.g. a force-closed SSH session whose remote TUI never sent DECRST, which otherwise turns every mouse move into `\x1b[<…M` garbage echoed at the prompt until the user runs `reset`.  injects DECRST for the active modes (1000/1002/1003 protocols, 1005/1006 encodings, 1004 focus) into both the server Term and the client-bound `PtyOutput` stream, so attached clients stop forwarding mouse events and replay snapshots no longer restore the stale modes. Bracketed paste and application cursor/keypad are deliberately untouched (shells manage those across prompts), a lingering encoding bit alone does not fire, and a `CommandStart` (133 `C`) later in the same chunk suppresses the reset so a type-ahead-launched TUI that just re-enabled mouse reporting is not clobbered ().

#### Focus State On Reporting Enable

Focus reports become PTY focus events only for sessions with DECSET 1004 active, so [[crates/scribe-server/src/ipc_server.rs#handle_focus_changed]] also records each report in `LiveSession.has_focus`, and the reader replays that state when the mode turns on.

An application that enables focus reporting *after* the client's last focus report — an AI CLI doing so during startup in the already-focused pane — would otherwise never learn it holds focus; Claude Code gates its own input-box cursor on focus-in, so it drew none until some unrelated focus change (the reason clicking into an already-focused Claude pane showed no cursor). After each chunk, [[crates/scribe-server/src/ipc_server.rs#deliver_focus_state_when_reporting_enables]] latches `FOCUS_IN_OUT` against the previous chunk's value via [[crates/scribe-server/src/ipc_server.rs#focus_mode_newly_enabled]] and, on the off→on edge, writes `\x1b[I` or `\x1b[O` — mirroring tmux, which reports the current state at enable time. It runs after the stale-mode reset so a 1004 that reset just cleared is not treated as newly enabled.

Before persisting and broadcasting `AiStateChanged`,  folds optional metadata (`context`, `model`, `tool`, `agent`, `conversation_id`) from the previously-stored live-session state into the incoming event when those fields are `None` and the provider matches. State-only hook OSC sequences (e.g. `ClaudeState=permission_prompt`) therefore preserve the live context-window fill set by the statusLine producer instead of clobbering it. An event that names a *different* conversation than the stored state inherits nothing, exactly as a provider switch does — see [[common#Common#AI State]].

Shell integration can also emit OSC 1337 `ScribeContext` metadata describing whether the current pane is remote, which host it is attached to, and the current tmux session name. The server stores that session context in the live session registry and rebroadcasts it on reconnect so the client can label panes before the next prompt redraw.

Terminal query callbacks share that same reader-task path. Clipboard loads, text-area-size reports, device-status replies, and dynamic colour queries are written back to the PTY from the live session state; colour queries fall back to the configured Scribe theme so foreground/background-sensitive TUIs see the real palette.

### Detach and Reattach

Client disconnection clears the client writer, while PTY EOF removes that session from live and ownership state before reconnect or handoff.

`CloseWindow` removes the whole window and its persisted tree. `CloseSession` and `CloseWindow` tear down the session's [[crates/scribe-server/src/pty_guard.rs#PtyGuard|PtyGuard]] for fresh sessions, which sends SIGHUP off-worker, but handoff-restored sessions have `pty: None` so  sends `kill(child_pid, SIGHUP)` explicitly. Neither signal guarantees the reader ever sees EOF — the child may trap SIGHUP, and the master fd stays open in the reader and the resize fd regardless — so both close paths cancel the session's exit gate, join the reader under the two-second bound, and drive the same finalizer described under [[server#Server#Sessions#PTY Reader Task|PTY Reader Task]]. `CloseWindow` does all three after its `WindowClosed` reply so the pre-existing reply ordering is preserved.

Each live session also tracks the current client's attached-session set alongside its writer. Reattach swaps both handles together, disconnect clears both, and PTY EOF removes the session ID from that per-client set before the connection loop sees the exit. Long-lived clients therefore do not accumulate stale attachment IDs as short-lived sessions churn.

`ipc_server.rs` remains the transport and message-dispatch layer for `AttachSessions`, but `attach_flow.rs` owns the reattach sequence itself: attach-entry preparation from live sessions, buffering sink install, pre-snapshot Term and PTY resize, `take_session_replay` for the zstd-compressed ANSI replay, and the buffered flush. Per-session metadata and per-workspace names travel on the preceding `SessionList` response, so the attach fan-out is just `SessionCreated` + `SessionReplay` per session.

A `CreateSession` needs none of that machinery, because it *is* an attach. [[crates/scribe-server/src/ipc_server.rs#initial_client_writer|initial_client_writer]] seeds the new session's sink set with the requesting connection's writer, already `Live` because a fresh session has no history for its output to race, and [[crates/scribe-server/src/ipc_server.rs#handle_create_session|handle_create_session]] records the id in that connection's attached set — so the answer arrives with the pane streaming and the PTY already on the grid the request named. A client that follows it with `AttachSessions` re-points a sink that is already correct, pays for a whole replay of a terminal that has emitted nothing, and drives the PTY through a second grid; the GPUI client and the `scribe-test` harness therefore both send only the `Subscribe`.

Each session's attach work runs on its own `tokio::spawn`ed task and the per-session futures are driven via `futures::future::join_all`, but the fan-out is bounded twice over because `AttachSessions` is reachable from LAN peers. The request is first collapsed to one entry per distinct session id, so a repeated id can neither pay for a second whole-grid snapshot nor re-open that sink's buffering window while the first attach is still building its replay. A process-wide semaphore then admits at most eight replay builds at a time, which holds transient allocation at a constant instead of letting one message make it a function of how many sessions that message names. The CPU-heavy steps (`snapshot_term`, `snapshot_to_ansi`, zstd) run on the blocking pool via `spawn_blocking` rather than on a runtime worker, so a batch of replay builds cannot stall every other session's I/O scheduled on those threads. The shared IPC writer is a `tokio::sync::Mutex`, which serializes only the final enqueue without blocking the parallel replay builds.

When a new client attaches, the attach flow usually resizes each session's Term and PTY to the client-provided dimensions before taking the replay. This ensures the replay matches the client's pane grid and absorbs the shell's SIGWINCH response before the replay is taken. Sessions still serving a preserved v4 legacy handoff snapshot skip that pre-replay resize so a live foreground process cannot redraw over the pre-upgrade history before the first replay reaches the client.

#### Lossless Attach

Attach installs the sink BEFORE the replay snapshot and reconciles the two with a per-session commit cursor, so no output falls into the window between them.

Attach used to snapshot the Term first and install the sink afterwards. Every byte the PTY emitted in between went to a sink-less no-op send and was lost — a real gap on any session with live output. Installing first instead is not a fix on its own: the new sink would receive live chunks the later snapshot also carries, and the client would render them twice.

The reconciler is `TermCommit`, a monotonic per-session cursor read and written ONLY inside the session's `Term` critical section. A PTY chunk advances it by its byte count in the same `feed_term` call that feeds the Term; a Term mutation that carries no bytes but has a client-side frame of its own — the AI-scrollback trim in `trim_term_scrollback` — ticks it by one. So "the cursor reads C" and "the Term reflects everything tagged at or below C" are the same statement, with no second critical section to keep in sync.

The sequence per session is: take a replay-build slot; install the sink in the **Buffering** state (`begin_sink_attach`); emit `SessionCreated`; take the snapshot and the cursor value `O_snap` together under one `Term` lock (`take_session_replay`); build and send the replay off-lock; then flush (`finish_sink_attach`) — buffered frames tagged above `O_snap` go out in emission order, frames at or below it are dropped because the replay already carries them, and the sink flips to **Live**.

The slot is taken before the sink goes in rather than around the build alone. A sink that started buffering and then queued behind the cap would keep accumulating frames it must eventually shed, and a shed backlog costs a full resync replay — the cap would feed the work it exists to bound. Waiting first is free instead, because the snapshot taken after the wait already contains everything emitted during it. The one exception is the session ending mid-wait: `SessionExited` carries no cursor value for any snapshot to reproduce, so an attach that finds its session's exit gate already claimed when its slot arrives is abandoned rather than installing a sink on a session the client could never retire.

Buffering holds every sink-bound frame, not just `PtyOutput`: `process_pty_chunk` also emits `TrimScrollback` on the same path, and replaying a scrollback trim against a snapshot that already reflects it would corrupt the client's history. Frames no snapshot can carry (metadata, `SessionExited`, workspace naming) are tagged with no cursor value at all and always flush. A buffer that outgrows the per-connection output budget while waiting sheds its backlog and marks the session replay-dirty instead, so the connection's writer task catches the client up with a fresh full `SessionReplay`.

Attach, subscribe, and snapshot requests are scoped to the caller's attached sessions and window ownership. A new connection may claim a persisted window ID only when that window is not already connected;  resolves and registers that decision under one write lock so concurrent claims cannot race into a duplicate. On disconnect,  removes the window's connected-client entry only when the stored writer is still this connection's (`Arc::ptr_eq` identity), so a stale disconnect from a client already superseded by a newer client for the same window cannot evict the new owner and make the window look unconnected.

When the connected-client map drops to zero, the server starts a short 250 ms grace timer before asking the singleton settings process to quit over `settings.sock`. If a client reconnects during that grace window, the settings shutdown is skipped so hot-reload or restart handoffs do not spuriously close the settings window.

### Terminal Resize

Resize updates the alacritty_terminal grid and sends `TIOCSWINSZ` via ioctl to notify the foreground process group.

That apply is paced per session to at most four per second. A drag republishes a pane's grid every frame, and every report used to drive its own full `Term` reflow plus a `TIOCSWINSZ`, so the reflows ran at event rate and the child paid one `SIGWINCH` per step for a single gesture. [[crates/scribe-server/src/ipc_server.rs#ResizePacer|ResizePacer]] admits the first report immediately — an isolated resize is never delayed — then holds the newest report as pending and arms one trailing apply for the remainder of the 250 ms interval; reports landing while it counts down replace the pending size instead of arming a timer of their own. The drag therefore costs a leading apply, one per interval it spans, and a trailing one at the size it stopped on. Cell pixel dimensions are still recorded from every report, since winsize replies read them and they cost no reflow. A session that leaves the registry mid-drag cancels its armed apply by construction: the trailing task finds nothing to resize.

An armed timer holds a size, and a size only stays applicable while the drag that reported it is still the last word on the grid. Registry presence alone is not that guarantee: a drag can end in a disconnect, and the session survives it. Detach therefore drops the held size once the last sink leaves ([[crates/scribe-server/src/ipc_server.rs#ResizePacer#discard_pending|discard_pending]]), and the two applies that bypass the pacer entirely — the attach-time resize in [[crates/scribe-server/src/attach_flow.rs#send_attach_replay|send_attach_replay]] and the shared-window authoritative grid — report themselves to it afterwards through [[crates/scribe-server/src/ipc_server.rs#note_unpaced_resize_apply|note_unpaced_resize_apply]] ([[crates/scribe-server/src/ipc_server.rs#ResizePacer#note_external_apply|note_external_apply]]). Without that, a client that dragged and disconnected mid-interval left a timer that matured up to 250 ms later over the geometry the *next* client had just attached at, reverting a fresh pane to the departed client's size. Both clears leave `armed` set, so the in-flight task still owns the disarm and a report admitted before it fires cannot start a second, overlapping timer. Recording the direct apply also folds it into the pacing window, so the first report after an attach is spaced like any other instead of buying an immediate extra reflow. A detach that leaves other sinks attached keeps the pending size, since the drag it belongs to is still someone's.

A deferred apply owes the client a repaint, because it lands after the client stopped asking. Each pane-size report is paired with a `RequestSnapshot` (see [[client#Input#Resize Coordination]]), and that request is answered against whatever grid the `Term` holds at the instant it arrives — so a report the pacer is still holding gets an answer at the pre-reflow size, and a client that only asks again when *it* next changes size would render that abandoned grid indefinitely. [[crates/scribe-server/src/ipc_server.rs#broadcast_post_resize_snapshot|broadcast_post_resize_snapshot]] closes that window: once the trailing apply — or the shared-window authoritative-grid apply — has reflowed the `Term`, a fresh compressed `SessionReplay` is fanned out to every sink attached to the session. `RequestSnapshot` uses that same bounded whole-pane replacement instead of the legacy per-cell `ScreenSnapshot`, whose 10,000-row form can exceed the 64 MiB message limit before a byte reaches the socket. Pacing itself is untouched; the push rides the apply that pacing already scheduled.

Two cases deliberately push nothing. A leading apply completes before the `RequestSnapshot` its own report carries is dispatched, so that reply already describes the applied grid and a push would only duplicate it. An apply that moved no grid is likewise redundant, which is why [[crates/scribe-server/src/ipc_server.rs#resize_term|resize_term]] reports whether the dimensions actually changed and the deferred callers push only when they did. The pushed replay is tagged with the session's [[crates/scribe-server/src/ipc_server.rs#TermCommit]] cursor like any other sink-bound frame, so a sink still buffering behind its attach replay drops it when that replay already reflects the reflow.

### Find Snapshot Reuse

[[crates/scribe-server/src/ipc_server.rs#handle_search_request]] answers each query edit from a full [[crates/scribe-common/src/screen.rs#ScreenSnapshot]] of the session grid. That snapshot is taken once per query burst and reused, not once per keystroke (spec 017 US8-2).

The snapshot is the expensive half: 27.6 MiB at 120x36 and 46.0 MiB at 200x50 with the default 10,000-line scrollback, in three allocations, taken under the same `Term` mutex the PTY reader needs for every chunk it feeds. [[crates/scribe-server/src/search_cache.rs#SearchSnapshotCache]] holds it against a [[crates/scribe-server/src/search_cache.rs#SnapshotKey]] of the session's [[crates/scribe-server/src/ipc_server.rs#TermCommit]] cursor plus the grid shape, both read under that same guard. A later edit that finds the key unchanged reuses the picture and holds the lock only for the comparison; a mismatch discards the entry and re-snapshots. The cursor covers new output and the shape covers a resize, which mutates the coordinate space matches are reported in without advancing the cursor.

Two things drop the entry outright. [[crates/scribe-server/src/ipc_server.rs#feed_term]] invalidates it inside the critical section it already holds, so the bytes that made the picture stale also release its allocation instead of leaving tens of megabytes resident until the next query; an atomic `populated` flag keeps that to one relaxed load per chunk when no overlay is open. The client's `SearchClosed` does the same when the find overlay closes. Both are advisory in the same direction — the cache never serves a stale picture, because the key check is what decides, not the release.

Reuse and the client's 150 ms debounce cover disjoint cases. Against an idle session the debounce still leaves one request per typing pause and reuse makes every later request cost a key comparison: a 10-character query holds the `Term` for 38.5 ms instead of 339.4 ms at 120x36. Against a session producing output the cache is always cold — every feed drops it — and the debounce is the whole fix, cutting the reader's wait 11.5x over a fixed window.

### CWD Change Suppression

Shells emit OSC 7 from their prompt hook, so the same directory is reported on every command. The server tracks the last CWD it pushed through the metadata pipeline per session and drops a repeat before any further work.

Suppression runs ahead of the registry write, the `CwdChanged` and `GitBranch` frames, the `.git/HEAD` walk and the workspace-manager write lock, so an unchanged directory costs one registry compare per prompt instead of two client frames and a filesystem walk. A report that survives the check is the invalidation signal for anything caching per-CWD state. The tracked value is per server process and is not carried in handoff state, so the first report after a hot reload still reaches clients that only saw the previous process. The `/proc` fallback and the hook channel feed the same pipeline and are deduplicated by the same check.

### Retained Prompt History

The server keeps each session's prompt-bar history next to its AI state, so a client that attaches after the prompts were submitted gets the bar from `SessionList` instead of waiting for the provider's next hook event.

`MetadataEvent::PromptReceived` used to be forwarded and forgotten: the only copy of a session's prompt history lived in the client that happened to be attached when the prompt was typed, and a client restart against a surviving server therefore lost every prompt bar with no path back — an idle conversation emits no further hook events, so "wait for the next one" can mean forever. [[crates/scribe-common/src/protocol.rs#SessionPromptState#record_prompt|SessionPromptState::record_prompt]] now folds each prompt onto the live session's [[crates/scribe-common/src/protocol.rs#SessionPromptState|SessionPromptState]] from the same [[crates/scribe-server/src/ipc_server.rs#persist_session_metadata|persist_session_metadata]] funnel that already retains title, CWD, context, and AI state, and [[crates/scribe-server/src/ipc_server.rs#handle_list_sessions|handle_list_sessions]] ships it on `SessionInfo.prompt_state`.

The fold *is* the client's — `AiChrome::record_prompt` calls the same method on the same shared record, so the first prompt is latched once, the latest is replaced, the timer restarts, and a bar painted from a session list is the same bar the live path would have built. `AiStateCleared` clears the history, live state, and provider hint, matching the client's own `forget` boundary: the provider exiting ends the conversation the history belongs to, and keeping any of those fields would repaint dead AI chrome or re-enable provider-specific behavior for the next client to attach. Conversation *switches* clear history on the same boundary: [[crates/scribe-server/src/ipc_server.rs#persist_session_metadata|persist_session_metadata]] compares the incoming `AiStateChanged` against the stored state with [[crates/scribe-common/src/ai_state.rs#AiProcessState#switched_conversation_from|switched_conversation_from]] before overwriting it, and drops `prompt_state` when the two name different conversations. The client retires its own copy on the same edge, so without the server half a hot restart — or a snapshot written after the switch — would repaint the retired conversation's rows and resume its count from where the dead conversation left off.

Instants travel as Unix-epoch seconds rather than `SystemTime`, matching how the restore snapshot already persists them — [[crates/scribe-common/src/protocol.rs#epoch_secs|epoch_secs]] is the single encoder both sides call. The elapsed timer's freeze is shared the same way: [[crates/scribe-common/src/protocol.rs#SessionPromptState#note_prompt_progress|SessionPromptState::note_prompt_progress]] stamps `latest_prompt_finished_at` on the first state edge that leaves `Processing` and clears it on the way back in, so a reattaching client reads back the frozen figure instead of a timer that starts running again from the original prompt instant.

#### Retained prompt history

Two prompts folded onto an empty record latch the first, track the latest, and count both; the AI state edges that follow freeze the elapsed timer once and release it when work resumes.

#### SessionEnd clears reattach chrome

After attention dismissal leaves only a provider hint, prompt history, or task label, `AiStateCleared` removes all three so a later session list cannot restore AI chrome for the exited provider.

### Git Branch Detection

On a CWD change that survives suppression, the server walks up from the working directory (depth limit 50) looking for `.git/HEAD`. It extracts the branch name from `ref: refs/heads/...` or returns the first 8 characters of a detached HEAD commit.

The walk is memoized per session by the directory it was taken in — a `(session, cwd)` key, since the [[crates/scribe-server/src/ipc_server.rs#GitBranchCache|cache]] hangs off the live session. A CWD report that survives suppression names a different directory and therefore misses, which makes the report itself the invalidation signal; a 5 s TTL bounds how stale a cached answer can be for a session that never moves, since a `git checkout` from another terminal produces no report at all. "Not in a repository" is cached like any other answer. The metadata pipeline never holds the registry guard across the walk: it looks the entry up, releases the guard, walks, then stores. `ListSessions` shares the same cache, so a client polling the session list costs one walk per TTL window instead of one per read.

Detection is skipped outright when the session's attached-sink set is empty. A detached session has nobody to render a branch, and the `SessionList` reply that precedes the next attach resolves it, so the walk would be pure waste.

#### Branch Resolution in ListSessions

A `SessionList` build splits branch resolution around its guards: the cache probe runs inline, and every miss is [[crates/scribe-server/src/ipc_server.rs#resolve_pending_git_branches|resolved]] once the live-session and workspace-manager read guards are released.

Probing the memo is a path compare and a clone, so it stays under the guards; a miss is filesystem I/O and would otherwise block every registry and workspace writer for the length of the walk — the reply is built while holding both read guards, and the walk count scales with the number of panes in the window.

Misses are keyed on the directory rather than the session for the second half of the same problem. Panes are independent sessions with independent memos, so a split window sitting in one repository misses once per pane; keying the walk on the directory collapses those into one `.git/HEAD` walk that every pane in that directory shares. Each pending session is then handed the answer and stores it in its own memo, so per-session invalidation semantics are unchanged.

### Git Push Detection

The server detects local pushes from Git's ref state and never treats PTY text as evidence of network activity.

[[crates/scribe-server/src/git_ref_watcher.rs#GitRefWatcherControl|GitRefWatcherControl]] is owned by `IpcServerState`. Fresh and handoff-restored sessions submit their initial CWD, and later `CwdChanged` metadata uses the same path. Registration runs on the blocking pool, never a PTY reader's Tokio worker.

Git resolves the worktree root, private git dir, and shared common dir, so a linked worktree's `.git` file is followed instead of parsed as a directory. Repositories sharing a common dir share one logical snapshot and watcher while retaining each worktree's private git-dir watch.

Each repository watches its git dirs and common dir non-recursively, `refs` recursively, and `reftable` non-recursively. Native `notify` events debounce for 250 ms. A watch error or rescan request replaces that repository's native watcher with a 2 s `PollWatcher`; the fallback reads only local ref paths.

After a burst, Git plumbing reads local branch and tag tips plus every configured remote's tracking namespace. Tag tips are peeled to their tagged commit, so annotated and lightweight tags at the same head read the same OID. A changed remote-tracking OID qualifies only when it equals a local branch tip.

The debounce retains exact paths from mutating notify events. An exact loose remote-tracking ref or tag can qualify a repeated generation at the same OID when that OID is both a local branch tip and an existing tracked remote tip.

Access events cannot qualify a generation. A burst that touches `packed-refs` or `reftable` also cannot infer one, so storage rewrites emit nothing. Tags at untracked OIDs infer no destination. A qualifying event emits [[crates/scribe-server/src/git_ref_watcher.rs#PushDetected|PushDetected]] with the canonical repository root, head SHA, remote name, and push URL.

[[crates/scribe-server/src/git_ref_watcher.rs#GitRefWatcher#start|GitRefWatcher::start]] returns `None` before allocating channels, a worker thread, or filesystem watchers when the feature is disabled. Live disable drops and joins the worker; live enable starts it and submits every retained session CWD. Enabled operation still makes no network request; the event only gates later CI polling.

### GitHub Actions Tracking

The server polls GitHub Actions only inside a window opened by a verified local push event.

[[crates/scribe-server/src/github_ci.rs#spawn_tracker]] owns one [[crates/scribe-server/src/github_ci.rs#GithubCiTracker|tracker]] per process. It consumes the watcher receiver, drops active polling on live disable, and uses [[crates/scribe-server/src/git_ref_watcher.rs#GitRefWatcherControl#subscribe_changes]] to reacquire a fresh receiver after re-enable. Startup, enable, and idle paths invoke neither `gh` nor HTTP.

Push-target HTTPS, SSH, and scp URLs must resolve to `github.com/{owner}/{repo}`. The pushed remote wins over any fetch remote, so fork pushes track the repository that received the push. Other hosts and malformed coordinates open no window.

One window covers one `(github.com, owner/repo)` head. Roots pushing the same head share that head's window, but a second head opens its own window beside the first instead of replacing it, so branches and worktrees running at once each keep their own bar and their own roots. [[crates/scribe-common/src/protocol.rs#MAX_CI_TRACKED_HEADS]] bounds every side of the feature — poll windows, dismissal memory, and the client's stacked bands — so they cannot drift apart. A push past it retires the head opened first, dropping the detail demand for that head at those roots and publishing `Cleared`.

[[crates/scribe-server/src/github_ci.rs#HttpGithubApi]] validates the fixed production host or an explicit loopback-only `SCRIBE_GITHUB_API_URL` before invoking `gh auth token --hostname github.com`. The token stays inside the server and only enters an Authorization header for that validated URL.

The run-list request uses `head_sha` and `per_page=100`, retains an ETag for the exact URL, and handles `304` without replacing state. It applies no event filter, so every workflow run GitHub attaches to the pushed commit — `push`, `pull_request`, and any dispatched or scheduled run sharing that head — reaches the bar. The server-wide scheduler permits one attempt every 5 seconds and at most 720 attempts per rolling hour. A no-run window expires after 120 seconds, before a 25th request. Transient failures back off through 5, 10, 20, then 30 seconds.

Up to 100 returned workflow runs contribute to the worst-status rollup. Queued or running remains non-terminal until every workflow completes; terminal precedence is failure, cancelled, then success. Each run records its first server observation and latest observation without a date-parsing dependency.

Each poll response is normalized to the newest run per workflow at the tracked head before rollup, publication, detail selection, and the terminal-stop decision: [[crates/scribe-server/src/github_ci.rs#newest_per_workflow|newest_per_workflow]] keeps only the highest-id run for each `(workflow_id, event)` pair, so a retag's superseded run cannot poison the rollup while distinct workflows — and one workflow file triggered by two different events — all survive together.

A terminal rollup settles its window instead of dropping it: the head stops scheduling requests and leaves hot handoff, but stays tracked so the head cap can retire it and a later generation can reopen it. The next expiry sweep past a settled head's own discovery window retires it, because no same-OID generation can still be arriving for it by then; sweeps run on tracker activity, so a fully idle tracker holds its last snapshots and wakes for nothing.

A trusted same-OID generation at a window's unchanged head reopens it in place, settled or not — observed state and roots carry forward and polling timers restart, but nothing is cleared or published as `Cleared`. The reopened window records the highest run id it had already published; while GitHub keeps returning only that previous generation, the response publishes but cannot settle the window, so a slow new run is not missed. The discovery deadline still bounds that wait, and a re-observed run whose status and conclusion did not change keeps its previous observation timestamp, so waiting for a generation cannot drift the elapsed clock of a bar that has no news. Ordinary same-head events merge repository roots without resetting polling, and a changed head only opens its own window, so neither duplicate watcher delivery nor a push on another branch can discard live state.

Authentication or permission failure before observation logs once and hides the window without HTTP after failed `gh` auth. Offline failures retry with bounded backoff. Failures after observation publish the last state as stale; terminal state stops polling. Tracker updates route through [[crates/scribe-server/src/ipc_server.rs#publish_ci_run_delta]], which retains capability, repository, and dismissal gates.

Job detail reuses that tracker, token, HTTP implementation, and server-wide
scheduler. [[crates/scribe-server/src/github_ci.rs#GithubCiTracker#set_detail_interest]]
creates demand only for an observed matching root and head, and removes it when
the last interested writer closes or disconnects. The loop alternates ready
run and detail work on the shared cadence; [[crates/scribe-server/src/github_ci.rs#HttpGithubApi#prepare_jobs]]
requests the trusted per-run jobs endpoint only while demand exists. Each
response keeps at most 100 jobs, each job keeps at most 100 steps, and provider
strings stop at a valid UTF-8 boundary no later than 256 bytes.

[[crates/scribe-server/src/ipc_server.rs#set_ci_detail_interest]] accepts capable
owning and read-only viewers for roots visible in their window. Only
repository/head/open state crosses IPC; `gh` authentication and its token stay
inside the server. Detail snapshots return directly to interested writers and
are not stored in hot-handoff state.

[[crates/scribe-server/src/handoff.rs#HandoffState]] carries active descriptors, roots, discovery time, and last bounded state without credentials. The successor re-polls each descriptor once, then resumes normal cadence; older named-map handoffs default to no active CI windows.

### Clipboard Gating

OSC 52 clipboard reads and writes from PTY-side programs flow through a per-session policy engine before reaching the host clipboard (spec 010). The in-memory `ServerClipboard` buffer is gone; the host clipboard is now the single source of truth.

The session's alacritty `Term` is constructed with  which sets `osc52: Osc52::CopyPaste`; the upstream default (`OnlyCopy`) silently drops OSC 52 read sequences inside the terminal core, so without this override `ClipboardLoad` events would never reach the gating layer and prompt mode would appear broken even though writes still worked. Scribe's own  is the only policy gate; alacritty is configured as a pass-through forwarder.

Each session's PTY reader holds a  snapshot of `terminal.clipboard.*` (read mode, write mode, max write bytes, focus-gate, burst window). The `SessionEvent::ClipboardStore` / `ClipboardLoad` arms branch on the relevant axis ( and ): on Allow the server forwards `ServerMessage::ClipboardBridgeWrite` or `ClipboardBridgeReadRequest` to the attached client; on Deny it silently drops writes and replies an empty OSC 52 payload for reads; on Prompt it allocates a monotonic  and emits `ServerMessage::ClipboardPromptRequest` to the client, parking the pending op in `pending_clipboard_prompt` until the user resolves the overlay. Oversize writes (`text.len() > max_write_bytes`) are silently dropped before any prompt or bridge dispatch (FR-009 / FR-015); the rejection emits a `tracing::debug!` line carrying the payload size, configured cap, write mode, and selection target so operators can observe the cap firing on demand without a PTY-side surface (UX-002). The cap check is positioned ahead of the policy branch, so it covers Allow forwards, Prompt prompts, burst-reuse hits, and the deferred-queue drain uniformly.

The Prompt path implements the full FR-016 / FR-017 burst-state machine. When an OSC 52 event arrives while the 's `outstanding_prompt` field is set and the in-flight prompt is for the same op, the request is enqueued onto `pending_for_prompt` (a  vector bounded at `MAX_PENDING_FOR_PROMPT` = 64). Same-op requests beyond the cap, and mismatched-op requests arriving while a prompt is open, fall back to the silent-drop / silent-empty-reply path with a `debug!` log so the cap is observable. With no prompt in flight, the reader consults `last_decision` via : when the cached `(op, decision, Instant)` matches the new op and is still within `policy.burst_window_ms` (default 500 ms), the decision is replayed without re-prompting via  /  (FR-017). Otherwise a fresh prompt opens.

On prompt resolution,  clears `outstanding_prompt`, records `(op, decision, now)` into `last_decision`, applies the decision to the originating pending op, then drains every deferred same-op request out of `pending_for_prompt` and replays the same decision against each one. `AlwaysAllow` / `AlwaysDeny` decisions also mutate the in-memory `policy.{read,write}_mode` on the matching axis so the next OSC 52 op outside the burst window already sees the persisted mode; the eventual `ConfigReloaded` round-trip is idempotent against the same value. `DenyOnce` / `AlwaysDeny` are reused identically to their allow counterparts so a tmux-style "no, deny everything" choice silences the burst rather than re-prompting per op.

Prompt resolutions and host clipboard read replies travel back into the reader task over a per-session  channel; the client message dispatcher fans `ClientMessage::ClipboardPromptResponse` and `ClipboardBridgeReadReply` onto every session attached to the window, and each reader task matches replies against its own `PromptId` issuance so stale or mis-routed responses are harmless. Reads stash the alacritty `ClipboardFormatter` keyed by request id; the formatter runs once the host clipboard text arrives (or once the user denies) and the resulting OSC 52 reply is written back to the PTY.

The attach-time `clipboard_gating: bool` capability bit on `ClientMessage::Hello` and `ServerMessage::Welcome` () is recorded on the window's participant entries in  (feature 015 consolidated the former per-window gating map into the share registry). When the attached client did not advertise gating, or no client is attached, all non-Deny arms short-circuit to the headless deny path (research decision 7): writes drop, prompts deny, reads reply empty.

`ConfigReloaded` () now snapshots the fresh `terminal.clipboard.*` policy via the server-local  and fans a `ClipboardCommand::RefreshPolicy` to every live PTY reader;  replaces `ClipboardBurstState.policy` in place so the next OSC 52 op resolves against the new mode without a server restart (FR-010). Any prompt that was already in flight when the refresh lands keeps its original op semantics — only subsequent ops see the new policy.

The X11 primary-selection branch and the FR-019 focus-gate-for-writes opt-in land in subsequent waves of the same spec.

## Workspaces

Managed by , workspaces group sessions and track per-window split layouts.

### Session Membership

Sessions join a workspace at creation, `MoveSession` re-keys one into another workspace, and `CloseWorkspace` removes a collapsed region's workspace.

The client's workspace-split flow seeds its session through the old workspace and re-keys it with `ClientMessage::MoveSession` once the new region's pane adopts it — [[crates/scribe-server/src/workspace_manager.rs#WorkspaceManager#move_session]] moves the membership and the live-session record follows, so `SessionList`, CWD auto-naming, and handoff persistence all agree with the client's regions. A collapsed region's `CloseWorkspace` removes the workspace via [[crates/scribe-server/src/workspace_manager.rs#WorkspaceManager#close_workspace]]. Both variants were protocol TODOs the server silently dropped until 2026-08: every split workspace stayed empty server-side, so an upgrade handoff restored sessions pooled into each window's first workspace and the split layout flattened.

### Destructive close refusal

`CloseWorkspace` naming a workspace that still has sessions is refused rather than honoured: a truthful close only arrives after the region's sessions ended, so a close with live members means the sender's layout is stale.

The failure this guards against is real: a leftover pre-update client redialing across a server upgrade imposed its collapsed layout back onto a reconnecting window and closed seven rebuilt workspaces in one burst, unlinking their live sessions into an unlisted limbo no `SessionList` could show. [[crates/scribe-server/src/workspace_manager.rs#WorkspaceManager#close_workspace]] now removes only an empty workspace and logs a refusal warning otherwise; the stale sender at worst renders fewer regions than exist, which the next `SessionList` corrects.

### Auto-Naming

When a session's CWD changes (via OSC 7 or /proc fallback), the server matches it against configured workspace roots and derives the workspace name and project root.

The first path component after the matching root becomes the workspace name; the full `root / name` path becomes the project root. Moving to a different project under the same root updates both. When the CWD moves outside all configured roots, the name and project root are cleared (an empty-string name is sent to the client). The project root is sent to the client so AI tabs can open at the workspace root directory instead of inheriting the current tab's CWD.

Only the first session in a workspace's stored order controls its shared name and project root. Other sessions' CWD reports do nothing. Reordering the session list transfers authority to the new first session on its next report.

On `ConfigReloaded`, the server replaces live workspace roots and re-evaluates each session's stored CWD or `/proc` fallback so newly added roots name already-open panes without requiring a server restart or another `cd`.

### CWD Fallback Detection

If a title change is detected without an accompanying OSC 7 event, the server falls back to reading `/proc/{pid}/cwd` on Linux or calling `proc_pidinfo` on macOS to detect CWD changes.

### Accent Colors

Workspaces cycle through an 8-color palette (indigo, cyan, emerald, rose, amber, lime, pink, cyan) as they are created.

### Per-Window Trees

Each connected window can report its workspace split layout via `ReportWorkspaceTree`. The server persists these trees for handoff and reconnection. A legacy global tree is supported for backward compatibility.

The reported tree's leaves carry per-tab `session_ids`, per-tab pane split trees, and the per-workspace `active_tab_index` — the server stores the tree opaquely and ships it back unchanged on the next `SessionList`, so adding new per-leaf fields requires no server-logic changes. Clients are responsible for reporting the tree after every layout-mutating action (split, close, tab switch, divider drag), which guarantees the server's stored tree is fresh enough that the next handoff or reconnect round-trips the user's focused tab.

#### Session order follows the reported tree

Verifies  answers in the order the window itself reported, so a reconnecting client whose tab strip is built from `SessionList` gets the user's order rather than a hash order.

The window's tree leads, region by region and, inside a region, tab by tab. A session created since that report follows the ones the tree names instead of displacing them, and another window's sessions never leak in. Walking `self.workspaces` — a `HashMap` — as the only source made a multi-region window's tabs come back grouped differently on every server process.

### Window Assignment

A connecting client's `Hello` resolves to one window through [[crates/scribe-server/src/ipc_server.rs#resolve_window_assignment]], which also names the still-unconnected session windows the client should fan out as separate processes.

A named `Hello` claims that window when no current client owns it — the ordinary bootstrap, which names the window id of the cold-restart snapshot it claimed, so identity, layout, and geometry all line up. An unnamed one (a launch that found nothing claimable) adopts an existing unconnected window instead of minting a new id, and that pick is taken over [[crates/scribe-server/src/ipc_server.rs#windows_in_stable_order]] rather than straight over the `HashSet` of windows with sessions: hash iteration order made the adopted window arbitrary per process, and the adopting client then wrote its opening default over that window's saved geometry.

If a local named claim finds its window connected, it falls through to the same different-window assignment unless `Hello.join_window` explicitly requests a share join. A restore claim alone never proves join intent.

#### Adoption order is stable

Verifies an unnamed `Hello` adopts the same window and fans the rest out in the same order however the window set was built, which is what lets the adopting client line up with the window it landed on.

The set is walked in window-id order, so two sets holding the same ids resolve identically, and `other_windows` follows in that same order rather than in a hash order that changed on every server process.

## Workspace Transfer

[[crates/scribe-server/src/ipc_server.rs#run_workspace_transfer]] owns the
capability-gated, idempotent `TransferWorkspace` transaction.

It holds the [[crates/scribe-server/src/workspace_transfer.rs#TransferGate]]
across validation, env staging, commit, viewer refresh, and ledger write. The
same gate now serializes existing-window workspace moves. The handoff/state-dump
snapshotter and agent world capture take it too, so those readers see strictly
old or new state.

### Transfer gate and ledger

The gate retains the most recent 64 transfer or move outcomes in one shared
capacity-bounded order.

Every success and typed refusal is recorded; retrying the same operation id
returns it without re-running validation. Move records also retain whether the
source shell closed, so a lost-ACK retry replays both `WorkspaceMoveResult` and
`WindowClosed`. Handoff state carries the ledger with `#[serde(default)]`; the
phase-1 transfer entry map remains decode-compatible. A latched handoff refuses
new transactions, and handoff failure clears the latch.

### In-gate commit

The request carries ids only; the server derives and commits both post-move
trees plus session ownership.

[[crates/scribe-server/src/workspace_manager.rs#WorkspaceManager#transfer_workspace]]
extracts the authoritative leaf through the shared tree operation. Registries
are acquired live sessions → window shares → workspace manager; env owner
coordinates and source sinks change before any guard releases.

### Typed refusals leave state byte-identical

Every pre-commit failure returns a typed `WorkspaceTransferResult::Refused` and
leaves authoritative source state unchanged.

Reasons cover unknown or foreign workspace, missing control or capability,
sole workspace, target collision, handoff, and env staging. Commit-time
revalidation failure discards staged target env copies before refusing.

### Staged env re-bind

[[crates/scribe-server/src/env_store/store.rs#stage_envelope_transfer]] stages
existing DEK and sealed bytes at target coordinates before commit.

Source coordinates remain untouched until commit. Keystore or filesystem
failure becomes `EnvironmentRebindFailed`, never a generic error. After commit,
old copies are deleted best-effort and persist schedulers restart on the target.

### Viewer severance and authoritative ownership

Commit severs every source participant's moved-session sinks and attached-id
entries, then refreshes each over existing `SessionList` frames.

Session-addressed key, resize, and close mutations re-check authoritative
session→window ownership. Active agent leases are re-announced after commit and
resolve to the destination window.

### Strict pre/post snapshots

[[crates/scribe-server/src/handoff.rs#serialize_state]] and transient agent-world
capture take the transaction gate before their ordered registry reads.

A snapshot that begins first completes as pre-state; one arriving during a
transfer or move waits and captures post-state.

## Workspace Move

[[crates/scribe-server/src/ipc_server.rs#run_workspace_move]] owns the
capability-gated, idempotent `MoveWorkspace` transaction.

Source and target window control are validated before env staging and
revalidated under the gate at commit. The server advertises `workspace_move`
only now that both operations are handled.

### Edge insertion

[[crates/scribe-server/src/workspace_manager.rs#WorkspaceManager#move_workspace]]
extracts the authoritative source leaf and inserts it at the target edge.

Shared tree operations preserve workspace/session, tab, pane-tree, and
active-tab payloads while deterministically re-equalizing split ratios. Live
PTYs are re-owned in place; no session is created.

### Bidirectional swap

A swap is one gate-held bidirectional commit, not two chained moves.

The manager joins cloned source and target trees under a temporary root, uses
the shared leaf-swap operation once, then stores the two children. Both window
shapes and outgoing slots remain fixed while session ownership and env
coordinates flip in both directions.

### Sole-source reattachment

An edge move may detach the source window's only leaf.

Commit removes the empty source tree and `WindowShare`, moves its live sessions
directly into the populated target, and records `source_closed` beside the
result. The requester receives `WorkspaceMoveResult::Moved` followed by
`WindowClosed`; retries before or after handoff replay both acknowledgements. A
sole-source swap returns the specific `SoleWorkspace` refusal.

### Typed refusals

Every pre-commit move failure is typed and recorded.

Reasons cover capability, source/target control, target availability, workspace
ownership, handoff, and env staging. Fallible env copies are staged in both
directions before commit; revalidation failure discards them. No refusal
changes live sessions, workspace/env ownership, shares, handoff state, or
agent-world routing.

## Beads Flow source cache

The cache retains the full parsed list beside its paintable snapshot, so Flow
needs no additional `bd` command.

A successful `bd list --all --limit 0 --skip-labels --sort created` refresh
produces one [[crates/scribe-server/src/beads_board.rs#CachedBoard]]: its
paintable five-queue snapshot and the complete parsed list share the same cache
generation. The source list retains the native `parent` id, every typed
`blocks` dependency (including an edge whose blocker is already closed), and
node metadata `assignee` plus `updated_at`. The board still derives open
blocker lane placement from `bd blocked`; retaining historical closed edges does
not alter classification or totals.

This is deliberately one list result, not another `bd` command. The graph needs
no extra invocation because the list already answers it: `bd list` returns the
native `parent` id and typed `blocks` dependencies *including satisfied ones*,
and the parse simply used to discard all three. Retaining them is what lets the
epic subgraph ride the existing cache generation, so Flow graph assembly cannot
see a graph from a different tracker read than the board card that opened it,
and a second subprocess never enters the interaction path.

Satisfied edges are the reason the full list is retained rather than a
blocked-only view. `bd blocked` reports what still blocks; a dependency graph
that omits a closed blocker draws a false picture of what an issue waited on.

Missing assignee and timestamp fields default safely; `updated_at`
stays an opaque string because only the client presents relative time, so a
tracker timestamp outside the expected ISO form cannot make the board
unavailable — [[client#Client#Beads Flow Layout Engine]] owns that formatting.

### Flow graph admission

[[crates/scribe-server/src/ipc_server.rs#handle_request_beads_epic_graph]]
reuses the exact local-owner, `SingleController`, and workspace-root gate from
[[crates/scribe-server/src/ipc_server.rs#beads_detail_request_root]]. A remote,
shared, displaced, or wrong-workspace requester receives no graph, and
[[crates/scribe-server/src/ipc_server.rs#handle_client_hello]] advertises
`Welcome.beads_flow` only to that same eligible owner.

[[crates/scribe-server/src/beads_board.rs#BeadsBoardCache#epic_graph]] reads
only the retained source. Admission is one server-side predicate rather than
degenerate-case handling spread through the renderer, so the client only ever
receives a graph it can lay out. Every refusal is logged and sent as typed
`NoGraph`, never as a partial graph, because an epic that never opens has to
stay diagnosable from the server alone.

Each refusal earns its place for a different reason:

- **Absent or empty epic** — there is nothing to rank. This is also the ordinary
  answer for a card whose `parent_epic_id` names an epic in another workspace's
  root.
- **Cycle** — longest-path ranking requires acyclicity and would not terminate.
  `bd` refuses to store one, so this guards the algorithm rather than a shape
  the tracker can actually produce.
- **Disconnected member** — a node with no edge in either direction has no
  defensible rank, and placing it arbitrarily would assert an ordering the data
  does not support.
- **External blocker** — an edge leaving the epic cannot be drawn inside it, and
  silently dropping it would show a node as ready when it is not.
- **Over 200 members, or over 16 `blocks` edges from one member** — the bound
  that keeps layout inside its frame budget. There is deliberately no partial
  graph: serving a truncated one could cut the opened card out of its own
  picture.

The bound is the epic subgraph's own, independent of the board's per-queue paint
cap. That separation is the point — the full retained list means a closed epic
member falling past the board's 200-card Done cap still appears in Flow, where a
graph assembled from the painted snapshot would have silently holed the epic.

A write advances the cache generation before its authoritative refresh,
so `graph_source` refuses to serve the preceding source generation during that
interval. The client's own fence is the other half of that guarantee — see
[[client#Client#Beads Flow Layout Engine#Flow mode entry, exit, and scrolling]].
Refusals are asserted end to end by
[[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Flow epic admission]].

### Focused issue liveness

A `LiveSession` keeps ephemeral `focused_issue` state, not an assignee-derived
approximation.

[[crates/scribe-server/src/ipc_server.rs#set_focused_issue]] is the sole
registry seam, and [[server#Server#Hook Channel#Focused issue events]] is its
one writer: it silently drops an unknown session, writes the exact issue id for
a live one, and emits
[[crates/scribe-common/src/protocol.rs#ServerMessage]] `IssueFocused` only to
the local owner of that session's unshared `SingleController` window. A session
can have multiple output sinks while shared, so liveness deliberately resolves
the window owner instead of fanning out through the session writer; remote and
shared participants see neither set nor clear frames.

Routing every writer through one seam is what makes the join exact rather than
inferred. The issue id and the session id arrive from the same observation, so
the registry never has to decide whether a generated assignee slug names the
agent running in this pane. [[client#Client#Beads Flow Layout Engine#Reading liveness from a node]]
spends that guarantee: a missing halo is a gap in what Scribe observed, while a
false one would assert a process that is not there.

`None` is the clear shape. The metadata pipeline sends it after
`AiStateCleared`; clean session/window close, reader/child-exit finalization,
and client disconnect clear the registry while the session is still live, so an
attached owner cannot retain a halo for a dead or detached agent. The field is
not serialized into handoff state: no process restart or reconnect revives a
claim that was only observed locally.

## Beads issue writes

The server admits one typed issue mutation, serializes Scribe writers per project root, and publishes only tracker-confirmed state.

### Capability and admission

Write capability combines connection ownership with an installed `bd` executable; neither check substitutes for the other.

[[crates/scribe-server/src/ipc_server.rs#beads_detail_connection_available]]
admits only the local owner of a `SingleController` window. The workspace must
belong to that window and supply its project root. Remote, shared, displaced,
and foreign-workspace writers never reach `bd`; the server silently ignores
their frames.

[[crates/scribe-server/src/beads_board.rs#BeadsBoardCache#write_available]]
advertises writes when executable discovery finds `bd` on PATH or a standard
user install path. Each write resolves it again, so disappearance returns a
typed `Failed` result without invoking a fallback binary.

### Serialized executor

One canonical project root owns Scribe serialization, fresh guard checks, argv validation, the write deadline, and result mapping.

[[crates/scribe-server/src/ipc_server.rs#handle_beads_issue_write]] resolves the
authorized project root, then calls
[[crates/scribe-server/src/beads_board.rs#BeadsBoardCache#write_issue]]. The
method canonicalizes the root and takes a standard-library advisory file lock
whose SHA-256 filename identifies that root under
`/tmp/scribe-beads-writes-{uid}`. Scribe verifies the directory and file are
owned by the effective uid and enforces modes 0700 and 0600.

Every Scribe server process for that uid uses the same lock path. Different
roots use different files and proceed independently. Direct external `bd`
processes do not take Scribe's lock, so this is serialization among Scribe
writers, not a database-wide transaction boundary.

After entering the lock, the server runs a fresh `bd show` and compares each
supplied status and assignee guard. An explicit empty assignee means the issue
must still be unassigned. A mismatch returns `PreconditionFailed` without a
write. An external process can still race after that read because it does not
honor the Scribe lock.

[[crates/scribe-server/src/beads_board.rs#compose_write_argv]] maps every
[[protocol#Client Messages#Beads issue writes|typed verb]] directly to ordinary
`bd` argv without private compare-and-set flags. Claim, close, reopen, and
comments retain the official CLI's actor, start time, lifecycle, and event behavior.

The argv builder rejects an empty, dash-prefixed, or NUL-containing issue id,
priority above P4, and statuses outside `open`, `in_progress`, and `closed`. It
caps comment bodies at 64 KiB. An omitted guard means no precondition.

[[crates/scribe-server/src/beads_board.rs#run_bd_write]] requires a schema-1
success envelope and uses bounded stdout, stderr, process-group cleanup, and a
15-second write deadline. Nonzero exit, timeout, spawn failure, invalid argv,
and invalid success JSON map to `Failed`. Those paths do not advance the
generation or last-good board, and dropping the file releases the lock.

### Generation fence and fan-out

Only a committed generation may replace the board cache or reach another authorized workspace on its project root.

An applied write increments the process-local generation for that canonical
root while the write lock is held; the counter is not a Beads revision. The
server sends its correlated result first, then
[[crates/scribe-server/src/beads_board.rs#BeadsBoardCache#refresh_after_write]]
loads the authoritative board while retaining the file lock. Only after the
refresh settles does it release the lock and fan the board out.
[[crates/scribe-server/src/beads_board.rs#apply_refresh_if_current]] rejects a
load whose generation is already stale.

[[crates/scribe-server/src/ipc_server.rs#push_beads_board_for_root]] sends the
accepted snapshot to every authorized local `SingleController` workspace on
the same canonical root. Other roots and shared or remote participants receive
nothing. One structured completion log records root, issue, verb, generation,
outcome, and elapsed milliseconds. The executor and fan-out proofs live under
[[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Server Beads Issue Writes]].

## Handoff

Zero-downtime server upgrades are implemented in  using Unix file descriptor passing.

### Protocol

The new server (with `--upgrade`) connects to the old server's handoff socket, sends `SCRIBE_UPGRADE` magic bytes, and receives serialized state plus PTY master fds via SCM_RIGHTS.

On Linux and macOS, the old server also verifies that the peer PID is a permitted Scribe server executable running with `--upgrade` before sending state or PTY fds. This prevents arbitrary same-UID clients from speaking the raw handoff protocol.

An ACK confirms receipt. If the ACK is not received (version mismatch, peer crash), the old server logs the failure and loops back to accept the next connection — it keeps serving until a compatible upgrade succeeds or `postinst` cold-restarts it. The handoff version is tracked to detect incompatible format changes. After the ACK, restoration, and session activation succeed, the new server emits `"IPC server listening"` immediately before its accept loop starts. This is the Debian hot-reload watchdog's readiness signal; the socket itself was already bound before the ACK (see [[server#Server#Handoff#Socket Takeover]]), so clients queue in its backlog during restoration. The watchdog reads the line from the state-dir `upgrade.log` it truncated at spawn, while the upgraded server's durable tracing lives in the state-dir `server.log` (see [[server#Server#Startup#Upgrade Path]]).

Readiness also commits predecessor retirement. Debian `postinst` stops the old
service main process under `KillMode=process`, waits for every exact PID/start
identity captured before installation, then escalates TERM to KILL for an older
detached server wedged in runtime teardown. These signals never target the
successor or handed-off PTY children.

### Socket Takeover

The receiver claims the IPC socket path before it sends the ACK, because the ACK is what tells the old server to exit.

[[crates/scribe-server/src/handoff.rs#receive_handoff]] calls [[crates/scribe-server/src/ipc_server.rs#acquire_server_socket]] between the last SCM_RIGHTS receive and `send_ack`, and returns the bound listener to its caller. Claiming it later — after the successor has rebuilt its sessions — left the path naming an exited server for the whole restore, which is proportional to session count. The client polls a lost connection every 100 ms and cold-starts a stateless server through systemd on the first refusal ([[client#Client#GPUI Client Spike#Server Lifecycle Wiring]]), so a large enough session set turned every hot reload into a guaranteed total loss: the cold server won the socket and the successor died with `EADDRINUSE`, holding the only PTY master fds.

Acquisition failure now aborts with the ACK unsent, so the old server loops back and keeps every session instead of exiting into a takeover that never completed.

[[crates/scribe-server/src/ipc_server.rs#bind_over]] performs the replacement by binding a fixed `.upgrade` sibling path and renaming it over the live socket. The old server handles one peer through ACK at a time, so a fixed staging name is exclusive to that receiver and lets the next attempt clear a socket left by a receiver that died before rename. `remove_file` followed by `bind` on the live path leaves a window in which the path does not exist, and one failed `connect` is all the client's autostart needs. `rename` is atomic: a concurrent dial reaches the old server or the new one, never nothing.

After sending its ACK, an upgrade receiver waits until it can acquire `server.lock` from the exiting predecessor before restoring sessions or accepting clients. The socket rename prevents a connection gap, while the reacquired flock preserves singleton enforcement for the successor's full lifetime.

Both platforms reach this path. Debian `postinst` spawns the receiver directly. On macOS, launchd starts the inactive managed slot with `--upgrade`; both jobs overlap through ACK, then the old slot exits successfully and stays inactive under `KeepAlive.SuccessfulExit = false`.

One window survives by construction: bind and ACK are two syscalls, so abrupt receiver death between them leaves the old server serving its established connections on a path that names a dead inode, and new dials cold-start a stateless server. The alternative ordering — ACK first, rename after — needs only ordinary preemption to strand the path, the same class of scheduling race that produced the original defect, so it is the worse trade. Closing the remaining window means teaching the old server to rebind and swap its listener after a failed handoff.

### State Transfer

The HandoffState contains per-session metadata, per-session replay payload, and workspace layout state for restart handoff.

Per-session payloads include independent optional window and icon titles, shell basename, remote context, provider task label, CWD, AI state (including optional provider conversation IDs used for resume behavior), and a  carrying the zstd-compressed ANSI replay for the session's visible grid plus scrollback. Missing native titles remain `None` across takeover instead of becoming a literal fallback title. File descriptors are transferred one-for-one with the serialized session list.

Each session also carries an additive `#[serde(default)]` prompt-history payload, so a server upgrade leaves every AI pane's prompt bar standing; see [[server#Server#Sessions#Retained Prompt History]]. A sender that predates the field just means the first prompt after the upgrade rebuilds the history.

Each session carries additive `#[serde(default)]` environment window and envelope identities. The successor uses them for later persistence cleanup; older payloads omit them and retain the previous workspace-membership window fallback with no envelope id.

Each session also carries an additive `#[serde(default)]` image-state payload — the committed scene as a bounded replay burst, the framer's paused control-string prefix, and any chunked transfer still accumulating — so a hot reload does not blank a session's images; see [[terminal-images#Terminal Images#Image State Across Handoff]].

Each session also carries the child's PID and an additive `#[serde(default)]` child-identity token, so the receiver can prove the PID still names that child before hanging it up (see ). A sender that predates the field leaves it absent and the receiver treats the child as unproven.

Per-workspace payloads include name, accent color, split direction, session list, and project root path. The project root is an additive `#[serde(default)]` field so handoff from older servers defaults to `None`.

The per-window workspace tree rides separately in  and carries the per-leaf `active_tab_index` (also `#[serde(default)]` for cross-version compatibility — a pre-active-tab-aware sender degrades to 0, and the next client report restores the correct value). This is why focused-tab state survives `--upgrade` without a dedicated per-window state struct.

#### Membership self-healing

After rebuilding its maps from the handoff payload, [[crates/scribe-server/src/workspace_manager.rs#WorkspaceManager#restore_from_handoff]] re-files every restored session those maps lost, using the workspace id the session's own record carries.

The maps and the per-session records are separate truths, and only the maps rot: membership lost in one server generation (historically to a stale client's destructive closes) used to survive every later handoff, because restore trusted the maps alone — an unmapped session appears in no window's `SessionList` and is unreachable forever even though its shell keeps running. Healing re-adds the membership, auto-creates a workspace the maps lost entirely, and assigns windowless sessions to a sibling's window or the busiest restored window, so orphans resurface as ordinary tabs the user can re-file instead of leaking invisibly.

### Session Replay Encoding

Both server-to-server hot-reload handoff and server-to-client reattach use the same primitive: a zstd-compressed ANSI replay that receivers feed through VTE to rebuild the `Term` durably.

The unified format replaces the legacy per-cell `ScreenSnapshot` on the reattach wire, shrinking attach payloads by 20-100x and eliminating the duplicate snapshot → ANSI round-trip the old two-format split produced.

Producers call , which snapshots the `Term` via  (reading the application's DEC private modes from `term.mode()` into the snapshot's `active_dec_modes: Vec<DecPrivateMode>` list), runs  to emit RIS followed by the target screen, mode-restoring DECSET sequences, scrollback, visible grid, and cursor, then zstd-compresses the result. Consumers feed the bytes through `vte::ansi::Processor::advance`, so one self-resetting stream replaces dirty or fresh terminal state and every subsequent attach sees the restored content.

RIS resets both screen buffers, history, margins, modes, attributes, and cursor before reconstruction. The encoder emits no ED 2: Alacritty implements it by scrolling one blank row into history even on a fresh grid. Receivers still normalize to the declared `scrollback_rows` after feed for N-1 payloads produced by a pre-RIS server; the client also conditionally prefixes RIS when decoding that old form.

Alt-screen sessions carry only the visible grid in the replay; alt-grid history is a resize artifact rather than user content, and alt-screen applications (vim, Claude Code) redraw their own UI on reconnect.

[[crates/scribe-common/src/screen_replay.rs#decompress_session_replay|Decoding streams]] the zstd frame in 64 KiB chunks and stops the moment the inflated stream would cross [[crates/scribe-common/src/screen_replay.rs#MAX_REPLAY_INFLATED_BYTES|an absolute 64 MiB ceiling]]. The earlier decoder sized a single flat buffer at eight bytes per declared cell, which the encoder overshoots by 4-5x on a truecolor-dense screen — every cell forcing its own SGR run costs 30+ bytes — so those sessions failed to decode and arrived blank. Nothing about the bound now comes from `cols`, `rows`, or `scrollback_rows`: an untrusted sender sets those independently of the bytes it actually ships, so only the observed encoded length seeds the initial allocation and only the ceiling stops the stream.

Two further limits keep the streamed decode from becoming its own denial-of-service lever, which the old one-shot bulk decode did not need. The output buffer starts at 8x the encoded length, floored at 64 KiB and capped at 4 MiB, and grows only as bytes arrive, so a small frame can never reserve the ceiling up front. The decoder also refuses a frame whose header declares a back-reference window past 8 MiB (window log 23): streaming decode allocates the declared window before producing output, and the encoder's level 3 never exceeds a window log of 21. Raising the compression level past a window log of 23 therefore needs an N/N-1 release window, since receivers on the old cap would reject the new encoder's frames.

A truncated or corrupt frame is an error rather than a short read, so a partial ANSI stream is never replayed into a `Term` — a garbled grid would be worse than the blank one this fix removes.

### Defuse Strategy

Before the old server exits, each session's [[crates/scribe-server/src/pty_guard.rs#PtyGuard|PtyGuard]] is [[crates/scribe-server/src/pty_guard.rs#PtyGuard#defuse|defused]], leaking the inner Pty through `ManuallyDrop` so its Drop impl never sends SIGHUP to the child.

Defuse is the guard's opposite of teardown: teardown exists to run `Pty::Drop` somewhere harmless, defuse exists to guarantee it never runs at all, so it consumes the guard inline rather than handing it to the blocking pool. The new server already holds the master fds via SCM_RIGHTS. Because defused sessions have `pty: None`, close handlers use  to send SIGHUP explicitly when those sessions are later destroyed.

That explicit SIGHUP aims at a bare PID rather than at a `Pty` the server owns, so it is gated on child identity. Handoff-inherited children reparent to init when the old server exits, which means init reaps them the instant they die and the PID can be handed to a stranger without this server ever noticing.

[[crates/scribe-server/src/child_identity.rs#read_child_identity|read_child_identity]] records a per-boot token for the child when it is spawned — the `starttime` field of `/proc/<pid>/stat` on Linux, the process start timestamp on macOS — and the token rides the handoff wire alongside `child_pid`. [[crates/scribe-server/src/ipc_server.rs#signal_if_handoff_session|signal_if_handoff_session]] re-reads it and compares before signalling. Anything short of a match logs and sends nothing: a recycled PID, a child that already exited, or a payload from a sender that predates the field. Those sessions still clean up through the reader's EOF path, which is the documented inherited-session exemption.

Reading the observed token at signal time narrows the reuse window to the gap between that read and the `kill` rather than closing it outright. Closing it outright would need a pidfd held since the fork, and inherited children never have one — the [[crates/scribe-server/src/child_watch.rs#open_child_pidfd|child-exit watcher]] deliberately leaves `child_pidfd` at `None` for them because a different process spawned them.

### Size Limits

Maximum handoff state size is 256 MiB. Maximum file descriptors transferred is 1024. Both sides verify peer UID, and Linux/macOS senders validate the peer process before sending sensitive state.

Restore is bounded by the live-session cap as well: [[crates/scribe-server/src/session_manager.rs#SessionManager#restore_from_handoff]] admits at most 256 sessions and logs the excess, so a corrupt or hostile payload cannot start the successor already over budget even though the fd limit is four times higher.

Typical v5 compressed payloads are in the low tens of megabytes even for many sessions at the default `scrollback_lines = 10_000`, since the ANSI replay + zstd combination is roughly 20-100x denser than the v4 per-cell MessagePack encoding.

Those are limits on the payload as it arrives. Inflation is bounded separately and per session: one replay may expand to at most [[crates/scribe-common/src/screen_replay.rs#MAX_REPLAY_INFLATED_BYTES|64 MiB]], matching [[crates/scribe-common/src/framing.rs#MAX_MESSAGE_SIZE|the IPC frame limit]] on the grounds that a replay too large to have crossed one frame has no legitimate consumer. The 256 MiB handoff figure caps the compressed state for the whole process, so it cannot serve as the per-session post-inflate bound.

### Version Bumps

Bump  when  changes incompatibly. Additive per-session fields that use `#[serde(default)]` stay on the current version because the wire format is named MessagePack: missing fields are filled with their defaults regardless of insertion position.

Feature 013 (remote window control) added no handoff fields — the remote listener is re-derived from config by the receiver rather than carried on the wire — so it stayed at v6; see .

The child-identity token added for the PID check is exactly such an additive field and also stays at v6: a receiver that has never heard of it decodes the payload and ignores the key, and a receiver that expects it fills the missing value with `None`.

Per-session terminal-image state is the one additive field that could not stay on the current version, because "the receiver ignores the key" is the failure: a pre-image server would restore every session's text with its images silently gone. An image-bearing pre-Pi payload declares v7; an image-free one still declares v6. See [[terminal-images#Terminal Images#Image State Across Handoff]].

`AiProvider::Pi` requires the same fail-safe treatment for a different reason: an older receiver cannot deserialize the new enum value. [[crates/scribe-server/src/handoff.rs#handoff_state_version]] declares v8 whenever any session's live state or provider hint names Pi, ahead of the image check. A v8 receiver accepts v6, v7, and v8 senders so both image-free and image-bearing forward upgrades remain hot; v6/v7 receivers refuse v8 before acknowledging, leaving the current server and its Pi session running instead of silently losing or misreading state. See [[test#Test Harness#Pi Provider Compatibility#Remote and handoff version gates]].

The sender uses `rmp_serde::to_vec_named` so `HandoffState` and `HandoffSession` serialize as MessagePack **maps** keyed by field name (since v6). Earlier versions used the default `rmp_serde::to_vec` which emitted MessagePack **arrays** — positional encoding where any field insertion in the middle of the struct silently mis-aligned every later field, breaking even "previous-version" hot-reloads despite `#[serde(default)]` annotations. Named encoding makes the invariant honest: as long as renames go through `#[serde(rename = "old_name")]` or `#[serde(alias = "old_name")]`, every additive struct change preserves backward compatibility. Cross-encoding handoff (v5 positional sender → v6 named receiver) is not supported; the old server remains active while the client asks for cold-restart approval.

Cold-restart is permitted only when hot-reload is genuinely impossible: incompatible state format (deserialization error — the underlying `rmp_serde` error is now propagated verbatim instead of being masked as "version mismatch"), version number outside the receiver's supported range, operational failure (OOM, fd/size limits, socket or zstd decode error, corrupted payload), or downgrade. A normal forward upgrade through any two consecutive releases that both use the named-map wire must hot-reload without terminating sessions.

On Linux that cold-restart path must also clean up any detached `scribe-server --upgrade` process left behind by the failed handoff before starting the user service again; otherwise the stale process can keep `server.sock` and `server.lock`, causing the restarted unit to fail with "another scribe-server is already running". On macOS, a failed handoff returns the old live connection with `CompletedRestartRequired`; only the helper started after user confirmation may unregister both jobs, terminate survivors, clear sockets, and register a fresh primary slot.

### Binary Change Detection

All three binaries (server, client, settings) use SHA-256 hash comparison to skip unnecessary restarts during upgrades, and Linux server upgrades also track a persisted runtime-generation stamp for launcher and service behavior changes.

On Linux, `postinst` compares each running binary (`/proc/PID/exe`) against the installed copy and also checks whether the desired `server-runtime-generation` differs from the stamp recorded in `/run/user/{uid}/{app}/server-runtime-generation`. That stamp is an opaque SHA-256 signature derived from the launch-critical `postinst` behavior plus the installed user service unit, so package installs hot-reload after launch-contract changes even when the server binary is byte-identical. `postinst` also refreshes service enablement so older `default.target` symlinks are removed and the service is enabled for `graphical-session.target`. Replacement processes inherit GUI session variables from `systemctl --user show-environment`, falling back to the invoking shell only when needed. Client relaunches wait for every previous PID to exit and skip relaunch if one survives; the deferred helper follows the same rule after `QuitAll`, treating zombie tasks as exited. The Debian hot-reload watchdog waits up to 30 seconds for large handoff snapshots. On macOS, the updater compares old (`.app.prev`) and new server binaries plus both bundled LaunchAgent definitions, treating hash failures as changed, then asks the replacement bundle's client to register the inactive LaunchAgent. A manually replaced DMG has no old-bundle hash comparison, so launchd readiness records the serving executable's device/inode identity; a newly launched client compares it with the installed server and uses the later modification/change timestamp only for legacy markers that predate that field.

## Crash Recovery Dump

The server checkpoints its live state to an encrypted file on a dirty-gated interval and at graceful shutdown, and loads it back on the next cold start, so a crash, SIGKILL, or ordinary service stop no longer loses every terminal's contents.

[[crates/scribe-server/src/state_dump.rs|state_dump]] persists the same [[crates/scribe-server/src/handoff.rs#HandoffState|HandoffState]] the hot handoff sends — sessions with their replay scrollback, metadata, and workspace trees — minus the PTY fds only a live SCM_RIGHTS transfer can carry, reusing [[crates/scribe-server/src/handoff.rs#serialize_state|serialize_state]]. Terminal contents are as sensitive as env values, so the dump takes the env store's at-rest posture: the MessagePack payload is AEAD-sealed via [[crates/scribe-server/src/env_store/envelope.rs#seal_bytes|seal_bytes]] with a dedicated DEK under the keystore's `state-dump-key` account ([[crates/scribe-server/src/env_store/keystore.rs#get_named_dek|get_named_dek]] / [[crates/scribe-server/src/env_store/keystore.rs#set_named_dek|set_named_dek]]), and keystore failure stops dumping rather than falling back to plaintext. The file lives under the flavor's state dir at `recovery/server-state.envz`, written 0600 in a 0700 dir via the env store's same-directory temp-file, fsync, atomic-rename pattern ([[crates/scribe-server/src/state_dump.rs#write_private_atomic|write_private_atomic]]). Serialized dumps above the handoff receiver's 256 MiB cap are skipped, so a dump can never persist what a handoff would refuse; a state with zero sessions removes the file instead, so stale ciphertext cannot outlive the sessions it described.

Dumping is dirty-gated, not periodic-unconditional: every state mutation worth persisting — the PTY feed funnels, hook ingress, the session lifecycle funnels, and the workspace-manager mutators — bumps a relaxed atomic generation through [[crates/scribe-server/src/state_dump.rs#mark_dirty|mark_dirty]], and [[crates/scribe-server/src/state_dump.rs#spawn_dump_task|the dump task]] re-checkpoints on its 30 s tick ([[crates/scribe-server/src/state_dump.rs#DUMP_INTERVAL|DUMP_INTERVAL]]) only when the generation moved, sampling it before collection so a mutation landing mid-dump forces a re-dump rather than being lost. Ctrl+C and SIGTERM (systemd stop, launchd removal, reboot) both take the same dump-then-exit path via [[crates/scribe-server/src/state_dump.rs#dump_now|dump_now]]; a completed handoff deliberately writes no final dump, because the successor owns the sessions and a stale dump from the predecessor could only shadow a fresher one.

Recovery is content-only and self-gating. A cold start loads the dump through [[crates/scribe-server/src/state_dump.rs#load_recovered_sessions|load_recovered_sessions]] — bounded by a 2 s timeout so a wedged keystore cannot hold the accept loop hostage — into a map keyed by env-envelope id (the client's launch id); a handoff start uses an empty map because the handed-off sessions are alive. A cold-restart replay's `CreateSession` naming a recovered launch consumes its entry and [[crates/scribe-server/src/ipc_server.rs#inject_recovered_replay|inject_recovered_replay]] seeds the fresh Term with the pre-crash scrollback before the PTY reader task exists, so the shell's first byte lands after it by construction; a trailing epilogue leaves the alt screen, resets margins, mouse reporting, and SGR, and paints a dim separator line so the dead session's terminal state cannot leak into the fresh shell. The session then has its full replay marked dirty so the drain sends the seeded history to the client, ordered ahead of live output. Consuming the entry keeps a second replay of the same snapshot (a claim-TTL reclaim after a crash loop) from stacking the history twice. Workspace and window state in the dump is ignored on load — the client's replay re-reports the layout — so the dump cannot fight the client over topology, and a window the user killed replays nothing and never shows recovered content.

Every load failure degrades to an empty map: recovery is best-effort and a server must never refuse to start over it. [[crates/scribe-server/src/state_dump.rs#decode_dump|decode_dump]] gates the payload's declared version through the same [[crates/scribe-server/src/handoff.rs#handoff_version_accepted|handoff_version_accepted]] an upgrade receiver applies, and a rejected dump is deleted; an unreadable file or missing key is merely skipped. A successfully loaded file is left in place — the next dirty dump supersedes it, so a server that crashes again before its first dump still recovers the same content.

### Dump round-trips through the sealed envelope

A `HandoffState` sealed with the dump DEK decodes back with its version and sessions intact, and the recovered map keeps only sessions carrying both an envelope id and a replay — the id-less session has no key a cold-restart replay could present.

### Dump rejects foreign versions and keys

A dump declaring a future handoff version is refused exactly as an upgrade receiver would refuse it, and a dump sealed under a different DEK fails AEAD open — both degrade to no recovery rather than to garbage state.

### Sessions without a replay or launch id are dropped

Reducing a decoded dump to the per-launch replay map drops sessions missing a replay payload or an envelope id, keeping exactly the launches a cold-restart replay can name.

### Recovered scrollback seeds a fresh Term

Injecting a recovered replay into a not-yet-started session's Term puts the dead session's content on the fresh grid, with the dim "scrollback restored" marker trailing it where the fresh shell's prompt will follow.

The epilogue undoes the dead session's terminal state — alt screen exited, cursor re-shown, mouse reporting, SGR encoding, and focus events off — so none of it leaks into the fresh shell. An undecodable replay seeds nothing and leaves the session blank.

## Crash Recovery Dump

The server checkpoints its session and workspace state to disk so a crash, SIGKILL, or ordinary service stop no longer loses every terminal's contents — the next cold start hands each replayed pane its pre-crash scrollback.

Before this, the server's authoritative state lived only in RAM plus the `--upgrade` handoff socket: recovery after a dead server rode entirely on the client's own snapshots, which restore layout and relaunch commands but cannot restore what the terminals showed. [[crates/scribe-server/src/state_dump.rs]] closes that by reusing the handoff pipeline verbatim — [[crates/scribe-server/src/handoff.rs#serialize_state]] collects the same `HandoffState` (per-session zstd ANSI replays, titles, CWDs, AI state, prompt history, env identities, workspace and window trees), the dump simply drops the fds only a live SCM_RIGHTS transfer can carry — so the on-disk dump and the handoff wire can never drift apart in what they capture.

### Dirty tracking and cadence

[[crates/scribe-server/src/state_dump.rs#spawn_dump_task]] samples a process-wide generation counter every [[crates/scribe-server/src/state_dump.rs#DUMP_INTERVAL|30 seconds]] and writes only when it moved, so an idle server costs nothing.

[[crates/scribe-server/src/state_dump.rs#mark_dirty]] is bumped from the PTY feed funnels (`feed_term` and the image-result feed), hook ingress (AI state and env events arrive off the byte path), the session lifecycle funnels (`start_session`, `finalize_session_exit`), and the [[crates/scribe-server/src/workspace_manager.rs|WorkspaceManager]] mutators (membership, moves, tree reports, window removal). The generation is sampled before collection, so a mutation landing mid-dump keeps the counter ahead and the next tick re-dumps. The loop initialises one behind the live generation, which is what makes a successor that restored handoff sessions write its first dump on the first tick. Each tick that fires is a full-state checkpoint — every live Term is re-snapshotted and re-encoded — which is the deliberate ceiling: per-session dirty tracking with cached replays is the upgrade path if the 30-second tick ever shows up in a profile.

Graceful shutdown writes one final dump: the signal arm of [[crates/scribe-server/src/main.rs#run_server_loop]] runs [[crates/scribe-server/src/state_dump.rs#dump_now]] before `shutdown_pty_readers`, and the server now handles SIGTERM ([[crates/scribe-server/src/main.rs#wait_for_sigterm]]) alongside Ctrl+C — `systemctl stop`, launchd job removal, and reboot all deliver SIGTERM, so every ordinary service stop is content-preserving instead of a silent kill. The handoff exit path dumps nothing and aborts the task: the successor owns the sessions now and writes its own dumps, so a stale dump from the predecessor could only shadow a fresher one.

### Sealed at rest

Terminal contents are as sensitive as env values, so the dump takes the env store's exact at-rest posture: AEAD-sealed, DEK in the OS keystore, no plaintext fallback.

The named-MessagePack payload is sealed by [[crates/scribe-server/src/env_store/envelope.rs#seal_bytes]] (the raw-bytes generalisation of the env envelope format) under a dedicated flavor-scoped DEK filed at the fixed keystore account `state-dump-key` ([[crates/scribe-server/src/env_store/keystore.rs#get_named_dek]]); a keystore failure skips the dump rather than writing plaintext. The file lands at `<state_dir>/recovery/server-state.envz` through the same 0700-dir/0600-file write-temp-fsync-rename dance as the env store, and a serialized state above the handoff receiver's 256 MiB cap is refused, so a dump can never persist what a handoff would not accept. A state with zero sessions removes the file instead of writing an empty one — stale ciphertext should not outlive the sessions it described — but only once this process has itself dumped live sessions: a fresh idle server's zero-session state says nothing about its predecessor's dump, and deleting it on the first tick would destroy a crash's recovery file before any client replayed it.

### Load and injection

A cold start loads the dump into a map keyed by env-envelope id (the client's launch id), and a cold-restart `CreateSession` naming a recovered launch gets the dead session's scrollback fed into its fresh Term before the shell's first byte.

[[crates/scribe-server/src/state_dump.rs#load_recovered_sessions]] runs only on the normal (non-upgrade) startup path, bounded by a two-second timeout so a wedged keystore cannot hold the accept loop hostage — dialing clients queue in the already-bound listener's backlog either way, and a timeout just means blank replayed panes, exactly the pre-dump behavior. The payload's declared handoff version is gated through [[crates/scribe-server/src/handoff.rs#handoff_version_accepted]] exactly as an upgrade receiver would, so a dump written by an incompatible server is discarded, not misread. Only sessions carrying both an envelope id and a replay enter the map ([[crates/scribe-server/src/state_dump.rs#recovered_map_from_state]]); the dump's workspace and window state is deliberately ignored, because the client's replay re-reports the layout and a server-side copy could only fight it.

Recovery is self-gating: an entry is consumed only when a replay presents its launch id ([[crates/scribe-server/src/ipc_server.rs#seed_recovered_scrollback]]), so a window the user killed — whose snapshot is gone — replays nothing and its content is never shown, and the claim-TTL reclaim path cannot stack the same history twice in one server generation. [[crates/scribe-server/src/ipc_server.rs#inject_recovered_replay]] feeds the decompressed replay through the session's own ANSI processor before the reader task exists (the kernel buffers the shell's opening output until the first read, so ordering is by construction), then appends [[crates/scribe-server/src/ipc_server.rs#RECOVERED_SCROLLBACK_EPILOGUE]]: mouse/focus DECRSTs, cursor re-show, SGR reset, and a dim marker line separating history from the fresh shell. Alt-screen exit is prepended only when the replay says the dead session was in the alt screen, and there is no `CSI r`, because both sequences home the cursor as a side effect and would paint the marker over the restored content on a primary-screen replay. Because a fresh session streams from byte zero and never receives an attach replay, the create path marks the session replay-dirty on the creating connection ([[crates/scribe-server/src/ipc_server.rs#ClientSink#mark_session_replay_dirty]]), and the writer's existing resync machinery ships the seeded Term as one compressed `SessionReplay` behind `SessionCreated`. The replay is encoded at the dead session's grid; a replayed pane is recreated at its snapshot geometry so the dims normally match, and a mismatch wraps at the old width rather than dropping content.

### Dump round-trips through the sealed envelope

A `HandoffState` sealed with the dump DEK decodes back with its version and sessions intact, and reduces to a recovery map holding exactly the launch-id-keyed replay payloads.

### Dump rejects foreign versions and keys

A payload declaring a handoff version outside the receiver's accepted range is refused after decryption, and a payload sealed under a different DEK fails AEAD authentication outright.

### Sessions without a replay or launch id are dropped

Only sessions carrying both an env-envelope id and a replay payload enter the recovery map; an id-less session has no key a cold-restart replay could ever present, and a replay-less one has nothing to show.

### Recovered scrollback seeds a fresh Term

Injecting a recovered replay puts the dead session's content and the dim marker line on the fresh grid in order, and leaves the Term untouched when the frame is corrupt.

A session that died inside a full-screen app is cleaned up on the way in: the epilogue exits the alt screen, re-shows the hidden cursor, and turns off mouse and focus reporting, so none of the dead app's modes leak into the fresh shell.

## Updater

Background update checker in  that polls GitHub releases and installs verified updates with platform-specific strategies.

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

The mount-and-replace step lives in [[crates/scribe-server/src/updater/macos_install.rs#swap_bundle_from_dmg]], separated from the surrounding process orchestration so its ordering can be unit-tested on any platform (see [[test#Test Harness#macOS updater bundle swap]]). The DMG is attached read-only at a mount point the caller pins with `-mountpoint`, inside the same private staging directory that holds the verified asset. The mount point is never recovered by parsing `hdiutil` output: `-quiet` suppresses that output entirely, and even without it a second mount of the same volume name lands at `/Volumes/<name> 1`, which a whitespace-split misreads. Mounting outside `/Volumes` also keeps the volume out of the shared namespace, so a leaked mount from an earlier attempt cannot misdirect the copy.

### Rollback

Restores the previous installation if an update fails mid-install.

On macOS, the existing `.app` bundle is renamed to an adjacent `.app.prev` backup before `ditto` copies the new version. If `ditto` fails, that adjacent backup is renamed back to restore the previous version. A stale `.app.prev` from an earlier failed update is cleared first so the rename cannot collide with it. On Linux, rollback relies on dpkg's own transactional behavior.

The mount is released on every path out of the swap once the attach has succeeded, not only on the success path. An early return between attach and copy was what left a mounted volume behind when the mount-point parse failed, and the detach is now paired with the attach by construction.

A failed attach is cleaned up too. `hdiutil attach` can attach an image's devices and then fail at the mount step, leaving them with no mount point for a detach to target, so the failure path runs [[crates/scribe-server/src/updater/macos_install.rs#attached_devices_for]] over `hdiutil info -plist` and detaches the whole-disk node for that image. The same teardown is the fallback when detaching by mount point fails. Both are best-effort: unparsable output yields no devices, because a cleanup failure must never mask the install failure that triggered it.

### Completion Reporting

The server that performs a macOS update cannot report its own outcome, so the survivor does it — see [[test#Test Harness#Post-upgrade announcement]].

On a successful hot reload the old server exits inside `install_update` while the handoff completes, before `run_install` can broadcast `Completed`. A client is left on a stale progress label, and the staging directory is never dropped because `UpdateDownloadStage::drop` never runs. The `--upgrade` successor therefore calls [[crates/scribe-server/src/updater/post_upgrade.rs#record_upgrade]] at startup, reaps any orphaned `update-*` directories, and broadcasts `Completed` once a client reconnects. This is the rule every established updater follows: nginx lets the new master report while the old drains, and Sparkle and Squirrel let the relaunched app report for the installer that replaced it.

### macOS Hot-Reload

After a successful `ditto`, the updater activates the inactive launchd slot so the newly installed server starts with `--upgrade` and overlaps the predecessor.

The app carries two agents under `Contents/Library/LaunchAgents`, with relative `BundleProgram` paths and distinct primary/alternate labels. [[crates/scribe-common/src/macos_launchd.rs#activate_replacement]] follows Apple's update contract: it serializes registrars, accepts an already-running target only after its PID-validated marker and command line prove serving readiness, otherwise asynchronously unregisters the proven-inactive job, waits until Service Management confirms its process is gone, then registers the new bundle's definition. The Objective-C calls are isolated in a private adapter with an audited `unsafe_code` exception; the lifecycle state machine stays under the workspace-wide deny. The registrar remains serialized until the bootstrapped process acquires that slot lock, closing the duplicate-activation gap. Registration immediately bootstraps the LaunchAgent, so no `kickstart` or detached long-lived server is involved. Once serving, the successor launches the new bundle's one-shot client to wait for and unregister its predecessor, leaving only one login agent registered. The old updater invokes the same new-bundle client for activation, ensuring Service Management never resolves the moved-aside predecessor. Legacy `~/Library/LaunchAgents` entries migrate from either historical launchd domain only while inactive.

The active slot comes from its command line or PID-validated marker; an observed no-slot legacy command maps to primary, while unreadable process metadata fails closed. Only the other slot may activate. The updater snapshots the prior replacement owner, then waits 30 seconds for both ACK and a newly owned replacement-slot readiness marker, so stale marker content or socket disappearance alone cannot trigger client relaunch. A candidate that fails before ACK while the predecessor still serves exits successfully so launchd does not crash-loop; after ACK, any failure restarts into normal ownership because the successor's own renamed listener is not evidence that the predecessor survived. Registration failures fail the install without offering a cold restart; only a successfully started candidate that cannot take over produces `CompletedRestartRequired`.

When the client binary changed, the predecessor starts a one-shot relay from the replacement bundle before activating the server successor. The relay carries the old server PID plus each pre-install client's PID and process start time. It waits until a different socket peer also owns the PID-validated serving marker, asks the settings singleton to quit, sends local-only transient `QuitAll` so terminal clients flush through their already-supported `QuitRequested` path, and waits ten seconds. The `QuitAll` connection is held open until the server closes it: a dropped peer fails the accept-time `peer_cred()` check with ENOTCONN and the frame is rejected unread, which is how the 0.1.8 update's quit request was lost. Because the broadcast reaches only windows registered at that instant — and pre-install clients are still reconnecting during the first seconds after the handoff — the relay re-sends both shutdown requests roughly once per second while survivors remain. A surviving process blocks relaunch rather than receiving a signal an older client may not handle gracefully; a clean exit launches exactly one replacement client. A failed server handoff never reaches the shutdown phase, and remote controllers never receive the local lifecycle message.

### Configuration

`UpdateConfig` in  controls update behavior: `enabled` (bool) to globally toggle the updater, `check_interval_secs` (u64, minimum 300) for the polling period, and `channel` (Stable/Beta) to filter which releases are considered.

The GitHub API endpoint defaults to the official releases URL and can be overridden with the `SCRIBE_UPDATE_API_URL` environment variable.

## Releases

Server-side release-history fetcher and cache that backs the  panel. Independent of the  auto-update path; reuses only the shared HTTP client in  so connection pooling, DNS, and TLS sessions are shared across the updater and the catalog.

### Release Catalog

In-memory cache held in : an `Option<Vec<Release>>` plus `last_fetched_at`, `last_fetch_was_success`, a `ttl` (defaults to one hour via `ReleaseCatalog::DEFAULT_TTL`), and an `inflight_refresh` flag preventing thundering-herd refreshes.

A `last_refresh_error` string is carried forward into Stale responses. Entries are stale-while-revalidate: when `last_fetched_at` is older than `ttl`, the next request schedules a background refresh and returns `::Stale { releases, reason }` immediately. On no-cache + fetch failure,  returns `Failed { reason }` and does NOT poison the cache. Per-call branches are computed under the lock by  so concurrent callers see the same view of the cache.

### Fetcher

The fetcher is dependency-injected via  (trait); the production implementation is .

It hits `https://api.github.com/repos/sharaf-nassar/scribe/releases?per_page=30` (capped via `MAX_RELEASES = 30`), drops drafts, keeps pre-releases, and runs each release `body` through `pulldown-cmark` (CommonMark + GFM features) → `ammonia::clean` via  before storing it in `Release.body_html`. Tests inject `StaticFetcher` / `PanicFetcher` implementations via the same trait so the cache state machine and render-and-sanitize pipeline can be exercised without live HTTP.

### Dispatch

 routes `::ListReleases` to , which reads the catalog state machine and replies with `::ReleaseList { state }`.

Background refreshes scheduled by the Stale branch run on the existing tokio runtime via  and clear `inflight_refresh` when they finish, regardless of success or failure.

## Configuration

Server config in  holds workspace roots and scrollback lines. Roots are validated as absolute paths with tilde expansion. Scrollback is clamped to a maximum of 100,000 lines.

Live `ConfigReloaded` handling in  reapplies workspace roots to , then recomputes workspace names for live sessions.

The config also carries the feature-013 `[remote]` table; the same reload handler pokes the remote-control supervisor so the listener starts, stops, or rebinds live without a restart — see .

`github_ci.enabled` projects into [[crates/scribe-server/src/github_ci.rs#github_ci_enabled]] at startup and on every reload. This atomic eligibility gate defaults off; changing it performs no prerequisite check or network request.

## Hook Channel

Structured IPC by which AI-tool hook subprocesses report state to the server, replacing the OSC-over-`/dev/tty` path that Claude Code v2.1.139 made unusable.

CC v2.1.139 (2026-05-11) intentionally detached the controlling TTY from hook subprocesses, breaking every `printf > /dev/tty` Scribe hook. The replacement is a new `ClientMessage::HookEvent` variant carried on the existing IPC socket and consumed by . Claude Code, Codex, Pi, and the Claude statusline subprocess share its provider-neutral schema. See `specs/003-ai-hook-channel/`.

### Discovery

Scribe injects three env vars into every spawned PTY so hook subprocesses can discover the channel and the helper binary.

The injection site is : `SCRIBE_HOOK_SOCK` (absolute path to the existing server socket) and `SCRIBE_SESSION_ID` (per-PTY UUID minted by `SessionManager::create_session`). Both inherit through the user's shell and the AI tool to the hook subprocess. Absence of either signals "not under Scribe" — the helper exits 0 silently (FR-003).

`SCRIBE_HOOK_HELPER` is the third var: the absolute path to `scribe-hook-helper` for whichever install layout is live, resolved by `shell_integration::find_hook_helper` and injected unconditionally (AI hooks fire even with shell integration off). No packaged layout puts the helper on `PATH`, so a bare-name lookup only ever worked in dev shells with `target/<profile>` on `PATH`. Resolution probes two locations: a sibling of the server executable — which covers both the macOS bundle (`Contents/MacOS/`) and dev builds (`target/<profile>/`) — then `<exe_dir>/../share/<flavor>/scribe-hook-helper`, which covers the prod-deb (`/usr/share/scribe/`) and dev-deb (`/usr/share/scribe-dev/`) installs. When neither hits, the var is left unset and every script falls back to the bare name. All five shell-integration scripts and the three `dist/ai-hook-*.sh` adapters honour the var. It joins the env-persistence exclusion set alongside `SCRIBE_HOOK_SOCK` and `SCRIBE_SESSION_ID`, so an injected path never lands in a restored baseline.

### Emitter

The shared  binary sends one `HookEvent` per invocation, waits for server EOF, then exits 0.

CLI parsing via `clap`; both env vars read; payload built; `ClientMessage::HookEvent` length-prefix-msgpack-framed to the socket via the existing `framing::write_message`. After the write, the helper holds the connection open until the server consumes the transient frame and closes it. That EOF is a reply-free acknowledgement and prevents macOS `getpeereid` from returning `ENOTCONN` when an immediately exiting helper wins the race against the server's post-accept credential check. A 100 ms `tokio::time::timeout` bounds connect + write + server close (FR-012). The helper accepts `--provider=pi` with the same generic event schema; current shell adapters remain in `dist/ai-hook-{claude,codex,statusline}.sh`.

Claude Code and Codex `UserPromptSubmit` adapters both emit `StateChanged { Processing }` followed by `PromptReceived` when the hook payload contains prompt text, so the prompt bar is driven by the same structured hook event for both providers. Codex additionally derives a `TaskLabelChanged` event from the first non-empty non-slash prompt line and maps `PermissionRequest` to `PermissionPrompt`.

### Pi Extension Adapter

Pi has no hook-script mechanism, so its adapter is an in-process TypeScript
extension that drives the same helper CLI the shell hooks drive. Transport,
ingress, the stop classifier, and the schema are untouched.

The full lifecycle, queue, shutdown, and silent-failure oracle is
[[test#Test Harness#Pi Extension Harness]].

It is also the only adapter that reports issue focus. Its `tool_call` handler
reads `event.toolName === "bash"` and inspects `event.input.command` for a
`bd … --claim`, emitting [[server#Server#Hook Channel#Focused issue events]]
with the claimed id. Because the extension already knows `SCRIBE_SESSION_ID`
from its own environment, issue and session come from the same observation —
an exact join, not a guess at whether a generated assignee slug names the agent
in this pane.

Command matching is deliberately conservative, because a false positive pins
the halo to the wrong issue and that is worse than showing none: the segment's
command word must be `bd` (or end `/bd`), `--claim` must appear as its own
token, and the id must be a positional matching an anchored tracker-id pattern
rather than some flag's value. The handler is pure observation — it never
blocks the tool and never mutates `event.input`, so a claim Scribe cannot
report still runs normally.

[[dist/pi-extension.ts#scribePiExtension]] is the Scribe-owned adapter Pi loads
from its user-scope extension directory. It shells out to
`scribe-hook-helper --provider=pi --event=<name> --payload-stdin` and puts every
value-bearing field in the JSON document on stdin, so a prompt carrying
newlines, semicolons, or shell metacharacters lands in a payload rather than in
an argv the process table would publish. The three selector arguments are the
only argv the extension ever builds.

The discovery gate is the same "not under Scribe" signal the helper itself
uses: a missing `SCRIBE_HOOK_HELPER`, `SCRIBE_HOOK_SOCK`, or
`SCRIBE_SESSION_ID` registers **zero** handlers and returns, so an installed
extension is inert in a Pi run that Scribe did not spawn. `PI_SUBAGENT_CHILD=1`
registers nothing for a different reason: a Pi subagent inherits the foreground
pane's `SCRIBE_SESSION_ID` and would otherwise drive that pane's indicator from
a child turn. A third-party launcher that exposes no child marker is outside
what the extension can detect.

A `Symbol.for("scribe.pi.lifecycle-extension")` slot on `globalThis` makes a
second load a no-op rather than a second set of handlers, which is what keeps a
duplicate registration from emitting every event twice. Shutdown releases the
slot, so the successor load after a reload registers normally.

The adapter also consumes the shared `rpiv:ask-user:blocked` event by its
stable literal channel name, without importing the optional questionnaire
package. Only an object payload with boolean `active` is accepted: `true` emits
`state_changed { waiting_for_input }` and `false` emits `state_changed {
processing }` through the same bounded serial queue. This records a real
mid-tool human wait rather than inferring it from `tool_call`; malformed
payloads emit nothing. The subscription is released during shutdown.

Only documented Pi lifecycle events are used:

- `session_start` → `task_label_cleared`, then `state_changed { idle_prompt }`.
- `input` → `state_changed { processing }`, `prompt_received`, and a derived
  `task_label_changed`, in that order. `source === "extension"` returns without
  emitting, so a machine-injected turn never replaces the user's prompt bar or
  tab label — the same outcome [[crates/scribe-server/src/hook_ingress.rs#is_machine_injected]]
  produces for the shell adapters, decided one layer earlier.
- `agent_start` → `state_changed { processing }` **only** when no captured input
  already accounts for the run. A retry or command-triggered run still reports
  processing; an ordinary prompt does not report it twice.
- `message_end` retains the last assistant text and whether its stop reason was
  an error, and emits nothing. `agent_settled` carries no message payload, so
  the text has to be held from the message that produced it.
  [[dist/pi-extension.ts#assistantText]] tolerates a malformed message by
  returning nothing rather than throwing into Pi's event loop.
- `agent_settled` → `state_changed { error }` for an error stop, otherwise
  `session_stopped { last_message }` for the server's stop classifier to resolve
  into `IdlePrompt` or `WaitingForInput`. [[dist/pi-extension.ts#contextPercent]]
  then adds `context_changed` from `ctx.getContextUsage()`, rounded and clamped
  to 0-100 so an out-of-range reading cannot paint an impossible gauge.
- `session_shutdown` → a final `state_cleared` after the queue is retired.

`PermissionPrompt` is never emitted. Pi exposes no documented permission event,
and the recorded gap is that Pi panes show processing where Claude Code would
show a permission prompt — a missing state, never a false one.

[[dist/pi-extension.ts#taskLabel]] reproduces the Codex adapter's normalization
— control characters to spaces, `;` to `,`, whitespace collapsed, 120 code
points — with one deliberate difference: it *skips* slash-command lines and
keeps reading, where Codex stops at the first non-empty line and yields nothing
when that line is a slash command. A Pi turn that opens with `/reload` and then
states the task still labels the tab. Slicing by code point rather than by
UTF-16 unit keeps a multi-byte character from being cut in half.

Emission is serial and bounded. One helper runs at a time, queued behind the
active invocation; the queue holds at most 32 outstanding events and drops the
rest, and each child is `SIGKILL`ed after 100 ms — the same budget the helper
gives itself. Handlers return synchronously and never await an invocation, so a
hung or missing helper cannot stall a Pi turn, and there is no polling timer at
all. Every failure path resolves: a spawn that throws, a child that errors, a
timeout, and a helper path that does not exist all end the same way, silently.

`session_shutdown` is the one handler that returns a promise. It bumps a
generation counter and discards every queued event from the previous
generation, awaits only the in-flight child, then sends `state_cleared`. A
reload therefore clears the pane in roughly one helper timeout instead of
replaying a backlog Pi is no longer running.

### Pi Extension Installation

Scribe installs Pi integration once at user scope and repairs only extension content it owns.

`dist/setup-pi-extension.sh` copies the marked packaged source atomically to
`~/.pi/agent/extensions/scribe-ai-integration.ts` with mode 0644. Identical
content is a no-op; stale marked content is replaced through a temporary file.
An unmarked regular file, symlink, dangling symlink, directory, or other
non-regular target is left untouched with a readable error. Setup never edits
Pi's `settings.json`, never creates a project-local extension, and preserves
unrelated sibling extensions, avoiding Pi's duplicate user/project load.

[[crates/scribe-client/src/hook_setup.rs#repair_pi_extension_if_enabled]] finds
the packaged source in the macOS app bundle or the active Linux flavor's share
directory. Packaged startup and a settings false-to-true transition call the
same repair path when `terminal.pi_integration` is enabled; failures remain a
notice, never a blocked Scribe or Pi launch. New Pi processes load repaired
content, while disabling integration leaves the flavor-neutral file installed
so stable and development packages do not fight over ownership.

Both Debian flavors ship the source and setup script from
`crates/scribe-server/Cargo.toml`, and `postinst` installs for a known target
user or defers safely until that user starts Scribe. The macOS bundle copies the
same assets through `dist/macos/build-dmg.sh`. Rollback disables the provider
first; removing the managed target is optional, and the next enabled packaged
startup repairs it. See [[test#Test Harness#Pi Provider Compatibility#Installation, repair, and rollback]].

### Ingress

The server dispatches `ClientMessage::HookEvent` on a transient connection (no `Hello`, no `Welcome`, no reply), charged to the separate 16-slot transient pool described in [[server#Startup#Local Admission]].

The pattern mirrors `CheckForUpdates` / `ListReleases` at `ipc_server.rs` `establish_client_window`. `hook_ingress::handle` looks up the session in `LiveSessionRegistry`, translates the `HookEventKind` to a `MetadataEvent`, and forwards into  — the same downstream pipeline the deleted OSC parser used, unchanged.

`PromptReceived` and `TaskLabelChanged` payloads are additionally screened by [[crates/scribe-server/src/hook_ingress.rs#is_machine_injected|is_machine_injected]]: harness wakeups, task notifications, and continuity blocks arrive through `UserPromptSubmit` as user-role turns whose text opens with a bare XML tag (`<system-reminder>`, `<task-notification>`, …). Those events are dropped so the prompt bar and tab label only ever show text the user actually typed; the accompanying `StateChanged { Processing }` still flows, since the AI really is processing the injected turn.

`HookEventKind::EnvChanged` events take a separate path: they have no `MetadataEvent` representation and instead route to , which folds them into the server-owned  registry. `baseline_ready: true` records a ; `baseline_ready: false` builds an , folds it via , and (if the session has an `env_envelope_id`) arms the 100 ms persist debounce via . The entire path is gated on `terminal.env_persistence.enabled` — when off, the event is dropped with a debug log before any state mutation. A session that arrives without an `env_envelope_id` has one minted in place by [[crates/scribe-server/src/hook_ingress.rs#bootstrap_envelope_id|bootstrap_envelope_id]] on its first foldable delta, so persistence starts without waiting for a restart; see [[server#Server#Env Persistence#Envelope Id Minting|Envelope Id Minting]].

### Focused issue events

`HookEventKind::IssueFocused { issue_id }` binds a live agent to the exact tracker issue it is working on, feeding the Beads Flow live-agent halo.

It routes like `EnvChanged` rather than through `translate`: there is no `MetadataEvent` for it, so [[crates/scribe-server/src/hook_ingress.rs#handle|handle]] short-circuits and calls [[crates/scribe-server/src/ipc_server.rs#set_focused_issue|set_focused_issue]], the seam the focused-issue registry already owns. That seam keeps the unknown-session drop, the no-change short-circuit, and the local-owner delivery gate in one place, so ingress adds no second registry.

The id is validated by [[crates/scribe-server/src/hook_ingress.rs#accepted_issue_id|accepted_issue_id]] and **dropped, never truncated**, when blank or longer than `ISSUE_ID_CAP_BYTES` (128, matching the board's own `MAX_ID_CHARS`). Every other capped field on this channel is truncated because a clipped prompt or label still says something true; an identity does not survive that treatment, since a clipped id either matches no issue or silently names a different one.

There is no clearing variant. The binding already dies with the session through `StateCleared`, session exit, and disconnect, so an adapter that only ever sets it cannot strand a stale halo. That is what keeps the event provider-neutral: an adapter reports *what it observed*, never lifecycle bookkeeping.

The synthetic `AiProvider::System` variant in  is the provider id for non-AI hook events. The helper accepts `--provider=system` (via ) so env-delta events can flow through the same wire format as AI hooks. `System` is intentionally absent from  so UI surfaces that list AI providers (pickers, new-tab launchers, integration settings) never display it.

### Stop Classifier

 maps a `SessionStopped` event's last-message text to `IdlePrompt` or `WaitingForInput`.

One provider-independent Rust function (with inline `#[cfg(test)]` rule tests) replaces the per-provider shell heuristics in the deleted `detect-claude-question.sh` and `detect-codex-question.sh`. Rules: strip fenced code blocks, take the last ~20 non-empty lines, return `WaitingForInput` on trailing `?`, question phrases (`would you like`, `should i`, …), or approval/review phrases.

### Schema

`HookEvent { session_id, provider, kind }` with nine `kind` variants on the wire.

`StateChanged`, `SessionStopped` (server-classified), `StateCleared`, `PromptReceived`, `TaskLabelChanged`, `TaskLabelCleared`, `ContextChanged`, `EnvChanged`, `IssueFocused`. Server-side caps: prompt and task-label 256 chars, last-message 16 KiB. `EnvChanged` is the env-delta variant added by feature 006: `added` / `removed` are filtered through the  and the `baseline_ready: true` flag flips capture into baseline-record mode (see the `EnvStoreState` section below). See  and `specs/003-ai-hook-channel/data-model.md`.

### Adding a Provider

A new AI tool provider plugs in via one adapter, shell script or in-process extension. No transport, server, or env-var changes.

Concretely: (1) add a variant to `AiProvider` in `crates/scribe-common/src/ai_state.rs` with `id`, `display_name`, `binary_name`, and resume capability; (2) teach the provider's documented integration to invoke `scribe-hook-helper --provider=<id> --event=…`; (3) install that integration through the provider's supported user-scope mechanism; (4) package the integration and setup assets. Pi completes the shared Rust boundary first: `AiProvider::Pi`, config gating, helper parsing, IPC compatibility, and handoff safety exist before its provider-specific lifecycle adapter. Events from a provider not yet recognized by the running build are dropped silently per FR-014.

Step (2) is a shell script only because Claude Code and Codex expose hook
commands. A tool that instead exposes an in-process plugin API supplies the
same helper invocations from there — see [[server#Server#Hook Channel#Pi Extension Adapter]],
which replaces steps (2) and (3) with one TypeScript file and its installer
while leaving step (1) and step (4) unchanged.

### Safety Contract

Hook subprocesses must never break the AI tool — even outside Scribe.

The helper exits 0 in every code path (FR-007), writes nothing to stdout (FR-008) or stderr (FR-009), does not open `/dev/tty` (FR-010), and bounds its connect+write+server-close wait to 100 ms (FR-012). Absence of `SCRIBE_HOOK_SOCK` or `SCRIBE_SESSION_ID` is the canonical "not under Scribe" signal — the helper exits 0 silently (FR-003). The same holds for unreachable sockets, dead Scribe servers, malformed args, or any other failure. This contract is what makes the AI tool's view of "is Scribe installed?" identical to "is the channel reachable right now?", so Scribe-installed hooks run safely in cloud sessions, subagents, SSH, and CI (FR-025).

## Shell Integration

Shell integration detects the user's shell (Bash, Zsh, Fish, Nushell, PowerShell) once per launch and injects startup scripts via shell-specific mechanisms.

Bash uses `--rcfile` to load the integration script, which sources startup files itself; on macOS it mirrors Terminal's login-shell behavior by preferring `~/.bash_profile`/`~/.bash_login`/`~/.profile` before falling back to `~/.bashrc`. Zsh uses `ZDOTDIR` wrapping; because Scribe's zsh is likewise non-login, the integration script emulates the Darwin login-profile pass from the pre-rc `.zshenv` bootstrap — sourcing `/etc/zprofile` (path_helper) and `~/.zprofile` (typically `brew shellenv`) under a `_SCRIBE_LOGIN_PROFILE_SOURCED` guard — preserving the real zshenv → zprofile → zshrc order. The zsh startup tests source a process-unique copy of the shipped script with exactly its `uname` command substitution replaced by controlled platform text, so Darwin, guard, and non-Darwin coverage retains real source semantics without a test-owned nested `uname` subprocess; the helper also checks child status and reports stderr. Fish and Nushell extend `XDG_DATA_DIRS` so vendor autoload directories are discovered, and both scripts drop the injected entry again as their first act — autoload is over by the time they run, and left in place the server-private path is inherited by every child the session spawns and is recorded in the env baseline as if the user had exported it, so the server would restore it into later sessions. Each script matches the entry on its trailing `/shell-integration` component, which is how all four install layouts end the scripts directory, and unsets the variable outright when nothing else was in it. `nu_strips_the_injected_xdg_data_dirs_entry` pins the nushell half against the shipped script, reading the value back out of a child process and out of the snapshot that feeds the baseline. PowerShell starts with `-NoExit -File` so the integration script is dot-sourced into the interactive session. When `SHELL` is missing, Scribe falls back to the account's login shell from the user database, and default sessions spawn that resolved shell explicitly so Finder- and launchd-started macOS installs do not inherit a stale shell choice.

AI and shell-tool tabs use the same integration load as a plain tab; no `SCRIBE_AI_TAB` mode is injected. Bash applies a staged restore delta while sourcing the integration script. Zsh and fish normally defer that apply to a self-removing first-prompt initializer, but a provider launch exits before its first prompt, so the server-appended `-c` command consumes and deletes the staged file after user rc/config and before the provider runs. Plain zsh and fish tabs still let the initializer emit the baseline before recurring delta capture begins. Handler registration order plus a capture-ready guard prevents the first prompt from persisting rc-only exports as user-session deltas.

Shell prompt hooks emit OSC 7 CWD updates, OSC 133 prompt marks, and OSC 1337 `ScribeContext` payloads carrying remote-host and tmux-session labels. Each shell's preexec hook also emits an OSC 1337 `ScribeAiLaunch=<provider_id>` sentinel (see ) when the user runs `claude` or `codex`, so the  re-arms before the AI tool emits its initial `\x1b[3J`. This is the counterpart to clearing `ai_provider` on `OSC 133;A` (shell-prompt return): plain shell sessions cleanly leave the filter, and `<tool> --resume` cleanly re-enters it without losing scrollback in between.  also synthesizes a follow-up `ServerMessage::AiStateCleared` on this same `OSC 133;A` whenever the session's live `ai_state` was active, so the client clears its , notification tracker, and  `LaunchKind::Ai → Shell` binding in lockstep with the server's internal filter — covering the common case where Claude Code or Codex exit without an explicit `StateCleared` hook event. zsh/fish/nushell/powershell detect the AI binary inside their per-command preexec hook; bash uses a `trap … DEBUG` handler gated on `BASH_SUBSHELL == 0` so subshell expansions during `PROMPT_COMMAND`/`PS1` evaluation do not emit spurious sentinels. Because a DEBUG trap action runs as a command before every interactive command, the handler would otherwise leak its own name into the special `$_` variable; the trap captures `$_` in its action string (`trap '__scribe_emit_ai_launch "$_"' DEBUG`, where `$_` still holds the previous command's last argument at trap-fire time) and restores it as the handler's final command, so an interactive `echo $_` keeps the user's previous last argument. `$?` needs no such handling — bash preserves the exit status across DEBUG traps. zsh's `$_` is unaffected because its `preexec` hooks do not reset it the way a bash DEBUG trap does.

Both deb flavors and the macOS DMG ship all five integration scripts. `crates/scribe-server/Cargo.toml`'s `scribe` and `scribe-dev` asset tables install bash, zsh, fish, nushell, and powershell scripts under `/usr/share/<flavor>/shell-integration/`, mirroring the layout `find_scripts_dir` probes; `dist/macos/build-dmg.sh` copies the whole `dist/shell-integration` tree into `Contents/Resources/`. Nushell and PowerShell were previously missing from the deb tables, so those two shells had no integration on Linux installs at all.

Launch-path lookups are cached only where the answer cannot change. `find_scripts_dir` classifies its hit as packaged (a deb's `/usr/share/<flavor>/shell-integration` or a DMG's `Contents/Resources/shell-integration`) or dev (a repo checkout's `dist/shell-integration`, found by walking up from the executable), and memoizes only the packaged one in a process-wide `OnceLock`. Those scripts ship inside the install and cannot move while the server runs, so every launch after the first skips the probe stats; a dev build re-resolves on every launch, which is what keeps `dist/` editable and re-layout-able under a running server. A failed probe is likewise never cached. Shell detection is folded the same way: `ResolvedShell::for_request` is the launch's only `detect_shell` call, and both `integration_script_path` and `build_env` take that `ShellKind` instead of re-deriving it from the binary path, so the unknown-shell diagnostic is emitted once at the detection site rather than from the env builder.

`scribe.nu` is the version-sensitive one: a parse or eval error there is fatal to the whole autoload file, so the script silently contributes nothing — no OSC marks and no env-delta capture — rather than degrading. Four nushell rules it has to respect: a custom command whose parameter is a required positional cannot be fed from a pipeline (pass the value as an argument instead); `char esc`, `char bs`, and `char ff` no longer exist, so escapes are spelled `\u{1b}`/`\u{08}`/`\u{0c}` inside `$"…"` strings, which is also the only interpolation form that collapses `\\` into the single backslash the OSC ST terminator needs; `into string` maps over a list element-wise instead of joining it, so list-valued env vars such as `PATH` are joined on `char esep` before they enter a snapshot; and the JSON escaper spells leftover C0 controls as `\u00XX`, because the script appends OSC sequences to `PROMPT_INDICATOR` and a single raw ESC in the payload would make the server reject the entire baseline emit. Its snapshot is a table of `{name, value}` pairs rather than a record, which is what lets `group-by` do the diff and what keeps `$env.__SCRIBE_ENV_LAST` a single value the baseline emit writes once instead of twice.

`scribe.fish` is the list-sensitive one: fish has no scalars, so every value the env-delta path touches is a list, and the two failure modes are silent. Command substitution turns an empty string into a ZERO-element list and a multi-line one into an element per line, so `__scribe_json_escape` funnels every stage through `string collect` — a bare `printf` result would collapse the caller's accumulator into a cartesian product with nothing, and fish then drops an unquoted `$added` from the argv entirely rather than passing `{}`, which is why a single `set -gx EMPTY ''` used to make the server record an empty `StartupBaseline`. A `.` guard rides through the escaper because `string collect` trims the newline `string replace` prints after each result. The snapshot reads values as `"$$name"`, double-quoted: an unquoted indirect read expands a list-valued export such as `PATH` to one element per component, so the recorded value stops being what a child process receives. Quoting is also what gets the separator right, and the reason no explicit join belongs here: fish joins a quoted list on the variable's own delimiter — a colon for a path variable, a space for every other list — which is exactly the byte sequence it hands a child process, so `PATH` is recorded as `a:b:c` and restores through `set -gx PATH '…'` as the same multi-entry list rather than as one entry that breaks command lookup. Path-ness is a per-variable flag, not a naming convention (`set --path FOO` is colon-joined, `set --unpath MANPATH` is not), so a hand-rolled join would have to read it back out of `set --show` on every prompt. The baseline and the per-prompt delta are one function called with and without `--baseline`, so there is a single `printf` emit site and the two payloads cannot drift apart in escaping or quoting; `fish_payload_interpolations_are_quoted` pins that line's interpolations, and two tests drive the shipped script under the checked-in `tests/fixtures/fish-hook-helper-recorder.fish`, which records argv and stdin to the `SCRIBE_RECORD_PATH` environment path: `fish_env_payload_survives_empty_and_list_values` asserts the empty-valued, list-valued, and multi-line entries all survive into the baseline, that the delta pairs each changed name with its own value, and that no environment value reaches argv, and `fish_env_snapshot_joins_path_variables_like_fish_exports` pins `set --path`/`set --unpath` against the recorded separator and round-trips the real `PATH` — recorded value read back out of the diff cache, compared against what a child process actually received, then fed back through fish's own restore form.

Both fish and nushell diff the environment in linear time, which they reach through their own hash tables rather than through a data structure the script builds. Fish has no associative array, so the cache is dynamically named globals: `__scribe_envm_<NAME>` holds the value last emitted for NAME, `set -q`/`$$key` are variable-table lookups, and `__scribe_env_last_names` exists only so a removal sweep has something to walk — and that sweep is skipped entirely unless the number of names the pass found already cached is short of the cached count, which happens exactly when something was removed. The keys are written `--unpath` because fish makes any variable whose name ends in `PATH` a path variable and the key inherits the tracked name. Names fish cannot spell as identifiers are dropped from the delta rather than reported wrong: indirect expansion stops at the first character outside `[A-Za-z0-9_]`, so a `BASH_FUNC_x%%` inherited from bash reads back as `%%`, and `set` refuses it as a key outright. Nushell instead builds its snapshot with `items` — one pass handing over each value directly — and pairs the two snapshots with a single `group-by` on their concatenation, tagged with a `side` column so a lone row is recognisable as an addition or a removal. Both replaced per-variable scans: `contains -i` over the whole cache in fish, `$name in $prev_names` plus `$prev | get $name` (a list scan and a record scan) in nushell, with nushell's `reduce`/`upsert` snapshot rebuilding the entire record per variable on top. Measured against the merge base at 60/120/240 exported variables, per-variable cost rises with the variable count before and falls toward a constant after; at the 60-variable baseline the env-delta path costs 14.3 → 2.8 ms per fish prompt and 7.1 → 2.8 ms per nu prompt, and fish stops forking altogether (7 → 4 spawns and execs, its OSC-only floor) because the `seq`-driven index loops are gone. Figures in `specs/017-audit-findings-triage/baselines.md`.

Every helper emit — the five shells' env baseline/delta and the two AI adapters' prompt, task-label, and assistant-message events — hands its payload to `scribe-hook-helper` as a JSON document on **stdin** (`--payload-stdin`), leaving argv holding only fixed selectors. `/proc/<pid>/cmdline` is world-readable, so the previous `--added-json=`/`--text=` form widened every exported secret and every prompt to all local accounts; and a single argument cannot exceed `MAX_ARG_STRLEN` (128 KiB), so an environment or a prompt past that made `execve` fail with `E2BIG` and — because the callers ignore the exit status by design — dropped the event with no diagnostic at all. Measured on the shipped scripts, a ~185 KiB environment delivered zero baseline frames before the change and one full frame per shell after it. Composition is a `printf '{"added":%s,"removed":%s}'` (nu and PowerShell concatenate the same document), so no shell grew a second escaper; PowerShell additionally pins `$OutputEncoding` to UTF-8 inside the sender, because native-command stdin is re-encoded on the way out and an ASCII setting would rewrite every non-ASCII byte as `?`. The helper drains stdin before any other check so a writer holding more than a pipe buffer cannot block, caps the document at 8 MiB and 5 s, and still accepts each old `--flag` when the document omits its key — shells that were already running when the package upgraded keep the old integration functions in memory and call the old contract until they restart. The Claude and Codex `stop` adapters lost their `mktemp` hand-off in the same move, so the assistant's last message no longer touches disk and each `stop` event costs one exec less.

None of that capture machinery runs when persistence is off. [[crates/scribe-server/src/session_manager.rs#build_pty_options|build_pty_options]] exports `SCRIBE_ENV_PERSIST` (`1`/`0`) alongside `SCRIBE_HOOK_HELPER`. Bash and PowerShell read it after their direct restore apply; zsh and fish first register only their one-shot post-rc restore initializer, then return before defining or registering any capture helpers. Nushell, where a top-level `return` is a hard error, gates its two side-effecting blocks on the same value instead. Skipped are the baseline snapshot, its `--baseline-ready` fork, the recurring per-prompt hook, and with it every per-prompt snapshot, diff and helper fork; the one-shot zsh/fish initializer still consumes a staged restore file, and OSC 133/7/1337 marks are untouched. Absence of the var means a server that predates the gate, and keeps the old always-emit behavior. `SCRIBE_ENV_PERSIST` joins [[crates/scribe-server/src/env_store/delta.rs#EXCLUSION_SET|EXCLUSION_SET]] for the same reason `SCRIBE_HOOK_HELPER` does. One [[crates/scribe-server/src/session_manager.rs#env_persistence_enabled|env_persistence_enabled]] read per launch feeds both this var and the restore-apply decision, so a session cannot see the feature enabled in one half and disabled in the other; the value is then fixed for that shell's lifetime, which is the spawn-time half of [[server#Env Persistence#Runtime Enable/Disable Transitions]].

AI tool state and prompt/task-label/context-fill updates do **not** travel through shell integration. They use the structured hook channel — see . The installer scripts `setup-claude-hooks.sh` and `setup-codex-hooks.sh` register thin `dist/ai-hook-{claude,codex}.sh` adapters that invoke `scribe-hook-helper` for every event. Linux installs place them under `/usr/share/{scribe,scribe-dev}`; macOS DMGs place the scripts under `Contents/Resources` and the helper under `Contents/MacOS`. `setup-claude-hooks.sh` additionally points Claude's `statusLine` at `dist/ai-hook-statusline.sh`. Both installers register unmatched `SessionEnd` hooks whose fixed adapters drain the unused payload and emit `state_cleared` without starting an interpreter; this is the provider-owned exit boundary when shell integration is disabled or unavailable. macOS startup repair requires that exact adapter/event line, so a legacy Stop-only registration is upgraded instead of mistaken for a current install. `setup-codex-hooks.sh` canonicalizes its `--hook-source` install prefix, enables `[features].hooks = true`, removes the deprecated Codex hook feature alias when found, and writes Scribe entries to `~/.codex/hooks.json` unless an inline `[hooks]` config already exists; in that case it preserves inline form and migrates non-Scribe `hooks.json` entries into `config.toml`. It adds matching `[hooks.state.…]` trusted-hash entries so Scribe command hooks are trusted immediately. It registers `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PermissionRequest`, `PreToolUse`, `PostToolUse`, and `Stop` hooks — **exactly one adapter invocation each**, because the `post_tool_use` and `stop` invocations carry the context-percent refresh that used to be a second `context` registration on the same event. Inside an invocation the adapter starts **at most one interpreter**: a single `python3` run reads the hook JSON once and prints the whole emit plan, one `<helper-event> <json-document>` line per `scribe-hook-helper` call, which the shell replays; only `session_end` starts none, and `dirname` is gone in favour of `${0%/*}`. Before event-specific planning, a non-null Codex `subagent` object drops the child event: thread-spawn agents inherit the root terminal's `SCRIBE_SESSION_ID`, so applying their transcript context or tool state would overwrite the foreground root meter. A Codex `Stop` therefore costs 5 execs with 1 interpreter start instead of 14 with 3 (see `specs/017-audit-findings-triage/baselines.md`), and the parse of the rollout transcript's 64 KiB tail is memoized in a 0600 `codex-context.json` keyed on the transcript's size and mtime, so the tool calls of one model turn re-derive nothing. The pre-consolidation `tool_processing` and `context` event names stay accepted with their old one-emit behaviour: a package upgrade replaces the adapter before the installer rewrites `~/.codex`. That rewrite is the trusted-hash migration — consolidating the adapter commands changes both the hash-registered command strings and how many groups Scribe owns per event, so the installer strips the hook-state blocks the old layout occupied (by key, computed from the config it read) before writing the new ones, preserving each hook's `enabled` flag and re-keying previously trusted third-party hooks onto their shifted indices. Both installers route every config write through a shared `atomic_write_text` Python prelude — a `mkstemp` file in the destination directory, `fsync`, the previous file's permission bits (umask defaults for a new file), then `os.replace` — so a `kill -9` or power loss mid-install leaves the user's existing `config.toml`/`hooks.json`/`settings.json` intact rather than truncated. Each run is also a single read-modify-write per file: the Codex installer reads `config.toml` and `hooks.json` once, applies the `[features]` toggle and the hook/trust-state merge in memory, and writes each file at most once, and both installers compare the rendered text against what they read and skip the write when it matches — so re-running an install that changes nothing leaves mtime and inode untouched.

## Env Persistence

Encrypted on-disk persistence of per-terminal exported-env deltas across cold restart, gated by `terminal.env_persistence.enabled` and a one-shot OS-keystore preflight. Owned end-to-end by `crates/scribe-server/src/env_store/`.

The on-disk envelope is an AEAD-sealed MessagePack blob of the working `TerminalEnvDelta`; its 256-bit ChaCha20-Poly1305 data-encryption key (DEK) lives in the OS secret store, scoped by install flavor and the `(window_id, launch_id)` pair so stable and `scribe-dev` installs cannot collide. There is no plaintext fallback — keystore failure stops persistence and degrades the session's `EnvStatus` instead of writing unencrypted. See `specs/006-persist-terminal-env/` for the full design.

Q7 sanctions excluding `SHELL`, `ENV`, `ZDOTDIR`, `XDG_DATA_DIRS`, legacy `SCRIBE_AI_TAB`, and `SCRIBE_INTEGRATION_SCRIPT` from plain-tab capture too, preventing stale shell-startup or Scribe-integration controls from redirecting later launches. `SCRIBE_AI_TAB` is no longer injected, but remains excluded so an envelope written by an older build cannot revive it.

### Keystore Wrapper

 wraps the cross-platform `keyring` crate (macOS Keychain + Linux Secret Service) behind binary DEK get/set/delete primitives.

 returns the flavor-aware service name (`com.scribe.server` for stable, `com.scribe.dev.server` for dev) via .  formats the per-envelope account name `env-key-<window_id>-<launch_id>`. The DEK itself is a 32-byte  alias, generated by  from `chacha20poly1305::aead::OsRng`. , , and  use `keyring::Entry::{get_secret, set_secret, delete_credential}` — the binary secret API, not the UTF-8 `_password` variants — so the DEK never needs base64 round-tripping. All three are async wrappers around `tokio::task::spawn_blocking`; the underlying `keyring` API is synchronous and would otherwise stall the runtime on D-Bus / Keychain I/O.

 is the internal error enum. `keyring::Error::PlatformFailure` and `NoStorageAccess` carry boxed platform-specific errors with no machine-readable kind, so the `From<keyring::Error>` impl inspects the inner `Display` text for `"locked"`, `"dbus"`/`"secret service"`, and `"access"`/`"denied"` substrings to classify them — a deliberate trade-off against downcasting into `security-framework::Error` / `secret-service::Error`, which would double the platform surface for marginal precision.  maps `KeystoreError` to the wire-level  consumed by `ServerMessage::EnvPreflightResult`; `NotFound` collapses into `PreflightError::Unknown` because it is an internal lookup-failure signal with no actionable user message. `Unknown` is a struct variant (`{ reason }`), not a newtype: the enum is internally tagged with `#[serde(tag = "type")]`, under which a newtype variant wrapping a `String` cannot be encoded at all — every `EnvPreflightResult` carrying a reason failed to serialize and was dropped before it left the server, which stayed invisible while no client ever asked for a preflight.

 is the low-cost reachability probe invoked when the user toggles `terminal.env_persistence.enabled` ON, and again from the runtime fail-safe path after a `Degraded` transition. It performs a sentinel `set_secret` + `delete_credential` round-trip under the fixed account name `preflight` (held in ); both calls succeeding means the keystore is reachable, unlocked, and write-capable. Each call leaves no residual state on success — and the set being the gating success means a delete-failure is logged-and-ignored rather than promoted to an error, so a stale sentinel is the worst case. Wrapped in `tokio::task::spawn_blocking` for the same async-runtime reason as the DEK ops.

The user-triggered side is plumbed through the window dispatcher: `ClientMessage::EnvPreflight` is routed (alongside `CloseWindow`, `QuitAll`, and friends) into , whose arm calls . That handler awaits `keystore::preflight()` (the inner `spawn_blocking` keeps the dispatch loop unblocked), converts any error via , and replies with `ServerMessage::EnvPreflightResult { ok, error }`. No retry, throttle, or rate-limit is applied — the request is user-driven and infrequent. Failures are logged at `warn` against `target: "scribe_server::ipc_server"`.

### Envelope Format

 owns the binary on-disk format: `version: u8 = 1` + 7 reserved zero bytes + 12-byte nonce + ChaCha20-Poly1305 ciphertext with the 16-byte Poly1305 tag appended.

The plaintext is `rmp_serde::to_vec_named` of `TerminalEnvDelta` — `BTreeMap`/`BTreeSet` give deterministic byte output for the same logical delta.  and  hold the version byte and the 20-byte header length used by both seal and open.

 generates a fresh random nonce per call via `ChaCha20Poly1305::generate_nonce(&mut OsRng)` — nonce reuse would compromise confidentiality and is the most important invariant in the file.  validates the version byte, slices out the nonce, AEAD-decrypts the trailing ciphertext (Poly1305 authenticates against any bit-flip in header, ciphertext, or tag), and `rmp_serde::from_slice` deserializes back into a `TerminalEnvDelta`.

 distinguishes `Truncated` (envelope shorter than `HEADER_LEN + 16`), `UnsupportedVersion(u8)` (the version byte does not match `ENVELOPE_VERSION`), `Aead` (Poly1305 auth failure — wrong key, corrupted bytes, or wrong nonce), and `Encode`/`Decode` wrappers for `rmp_serde` errors via `#[from]`. The opaque `Aead` variant deliberately does not distinguish wrong-key from tamper since AEAD MAC failures are indistinguishable to the caller anyway.

### Envelope Store

 is the on-disk envelope I/O layer — path layout, atomic write-temp + rename, file permissions, and the create / update / read / delete lifecycle. It is the only env_store file that talks to the filesystem.

Path layout is `<state_dir>/restore/env/<window_id>/<launch_id>.envz`, where `<state_dir>` comes from  (`dirs::state_dir()`, or the platform data dir — `~/Library/Application Support` on macOS, `%APPDATA%` on Windows — where XDG's `state_dir` is undefined, joined with the flavor slug from ; that fallback keeps state writable off-Linux, where `dirs::state_dir()` is `None` and every state consumer — restore, window state, env store, LAN trust stores, device-identity cert — would otherwise fail closed). This is the same flavor-aware root that backs 's `restore/` subtree, so `scribe` and `scribe-dev` installs cannot collide and the env tree lives alongside the existing window-state tree.  returns the per-window directory;  returns the per-launch file path; both return `StoreError::NoStateDir` when no state directory can be resolved.

Atomicity follows the same write-temp + `rename(2)` pattern as `scribe-client::restore_state`:  creates a `.<stem>.tmp.<pid>.<nanos>.<attempt>` sibling file with `O_CREAT | O_EXCL | mode=0o600`, writes the sealed bytes, and `fsync`s before returning the temp path so the caller can `rename` it atomically over the final path. The atomic-write helpers are duplicated intentionally (rather than cross-crate-imported from `scribe-client`) to keep server-only ownership of `env_store`. Permission constants `PRIVATE_DIR_MODE = 0o700` and `PRIVATE_FILE_MODE = 0o600` are enforced by  /  after each create. All blocking I/O is wrapped in `tokio::task::spawn_blocking` so the async runtime is not held by `fsync` or directory walks.

 idempotently creates `<state_dir>/restore/env/<window_id>/` and re-applies 0o700 on the leaf.  fetches the DEK via , `tokio::fs::read`s the file, and AEAD-opens it via  — returning `Ok(None)` on `ErrorKind::NotFound` since "no envelope yet" is a normal state and not an error.  is the get-or-create write path: it tries `get_dek`, generates and `set_dek`s a fresh key on `KeystoreError::NotFound`, seals the delta, writes it through the temp-file dance, and (on `rename` failure) deletes the orphaned temp before propagating the error. It is idempotent so the T015 persist scheduler can call it on every debounce tick without bookkeeping.  removes both the on-disk file and its DEK; missing entries are not errors, and a DEK delete failure is logged at warn rather than propagated since the user-visible state is the disk entry being gone.  sweeps a whole window's env dir on clean window close or feature-disable, calling `delete_envelope` per launch (so DEKs come along) and then best-effort removing the now-empty directory.

 wraps `io::Error`, `EnvelopeError`, and `KeystoreError` via `#[from]`, plus a `NoStateDir` variant for the state-dir lookup failure. Callers (T015 / T017 / T019 / T035) match on it to decide between a `Degraded` `EnvStatus` transition (keystore errors) versus a hard server-internal log (filesystem or envelope errors).

### Per-Session Registry and Persist Scheduler

 is the in-memory runtime registry for env-store state. One `EnvStoreState` lives on the server-global state holder and is the single source of truth for per-session baseline, delta, status, and the persist scheduler.

The registry holds the post-rc , the live working , the per-session runtime , and one persist-scheduler mpsc sender per live session, all under a single inner `Mutex`.

The narrow API is intentional: every mutation routes through one of the `EnvStoreState` methods rather than letting callers reach into the inner maps.  writes a session's startup baseline and clears any prior delta (a re-baseline resets per-session state per `data-model.md::StartupBaseline`).  is the gate hook-ingress uses to decide whether to fold an `EnvChanged` event or drop it (events that arrive before the `baseline_ready: true` emit are meaningless and discarded at debug).  applies an  via `TerminalEnvDelta::apply_event` and returns `true` only when a delta now exists, which the persist scheduler uses to decide whether to arm its timer.  clones the working delta so the scheduler task can encrypt and write without holding the inner lock across I/O.  /  are the read/write pair for `EnvStatusState`; T015 / T036 are the only writers. `set_status` is transition-only: the new value is only broadcast when it actually differs from the previous one (a missing previous entry is treated as `Active`), so spurious same-value writes never produce duplicate client emits.

 mirrors the wire-level `EnvStatus` (`Active` ↔ `Degraded { reason }`) but is owned by the server crate so business logic does not import the protocol type — T015 / T036 translate to the wire form when emitting `ServerMessage::EnvStatus`. The `Degraded` reason is short and safe to surface in a tooltip per `research.md::R2.5`.

 returns a `tokio::sync::broadcast::Receiver<(SessionId, EnvStatusState)>` that fires whenever `set_status` observes a real transition. The IPC layer subscribes once at server startup via , which spawns the long-running task that drains the receiver, looks up the owning session's `client_writer` from the live-session registry (same pattern as `hook_ingress::lookup_client_writer`), converts the internal `EnvStatusState` to the wire form via `env_status_to_wire`, and sends `ServerMessage::EnvStatus { session_id, state }` to the client. The forwarder is fail-open: a missing live-session entry (session closed between transition and forward), a `RecvError::Lagged` (subscriber fell behind), and a `broadcast::send` with zero subscribers are all logged at `debug` against `target: "scribe_server::ipc_server"` and skipped — the current status is always recoverable via `get_status`, so a missed broadcast is informational only. The 64-slot ring buffer (`STATUS_BROADCAST_CAP`) comfortably absorbs any plausible per-session burst given the 100 ms persist debounce.

 is the entry point the  `EnvChanged` translation calls after every successful `fold_event`. On first call per session it lazily spawns a long-running  and stashes that task's `mpsc::UnboundedSender<()>` in `EnvStoreInner::schedulers` keyed by `SessionId`; every subsequent call just sends a tick. Because the channel is unbounded and the receiver folds N ticks into one debounce-reset, bursts of `EnvChanged` events coalesce into one disk write — the `schedule_is_idempotent_per_session` unit test pins this single-entry invariant. The method takes `self: &Arc<Self>` because the spawned task needs to call back into `current_delta` and `set_status`, so callers (the server-global `AppState` holder) MUST wrap the registry in `Arc`.

 is the per-session debounce loop. It owns a `tokio::time::Instant` deadline that each tick (re)arms to `now + PERSIST_DEBOUNCE` (100 ms per `research.md::R1.4` — below human perception, tight enough that keystroke-paced edits land within ~one window). The loop uses a `biased` `tokio::select!` over `rx.recv()` and a `sleep_until(deadline)` branch gated by `deadline.is_some()`; the branch falls through to `pending()` when no deadline is set so the select stays parked between ticks instead of spinning. When the deadline fires, the task snapshots the current delta (skipping the write if it vanished — baseline re-recorded mid-flight) and calls . Success transitions `EnvStatusState` to `Active`; any error transitions to `Degraded { reason }` and leaves the existing envelope file untouched per FR-007 / FR-016 (no plaintext fallback — the file may still be the most recent good state). The task exits cleanly when `rx.recv()` returns `None`, which happens after the matching `schedulers` entry is removed.

 is called on the session-close path and drops the baseline, delta, status, and scheduler entry in one lock-hold; dropping the scheduler entry drops its `Sender`, which closes the channel, which terminates the `persist_task`.  is the narrower variant used by the `terminal.env_persistence.enabled` `true → false` transition (T035) — it halts persistence without discarding the baseline + delta, so a fast re-enable can resume without re-capturing. Both paths are cancel-safe: the only way to keep a task alive is to keep its sender alive, and the only way to communicate is through the channel.

 is the 100 ms `Duration` constant — exposed `pub` so tests and the (future) T035 / T036 wiring can read the canonical value rather than redefining it.

### Envelope Id Minting

Every create path mints the launch id that names the session's envelope, client-side, before the `CreateSession` frame leaves.

[[crates/scribe-common/src/ids.rs#new_launch_id|new_launch_id]] is the one mint. The GPUI client reaches it through the `LaunchBinding` it queues for each new tab or pane and forwards that binding's id as [[crates/scribe-client/src/ipc_bridge.rs#SessionLaunch|SessionLaunch]]`.launch_id`, so the binding a cold restart will persist and the envelope the session writes agree from the first frame; a replayed pane fills the same request from its saved `LaunchRecord` instead, which is why fresh and restored launches share one [[crates/scribe-client/src/ipc_bridge.rs#IpcSink#create_session|create_session]]. `scribe-cli` and `scribe-test` mint one per `CreateSession` too. All three used to hardcode `None`, which left every GUI, CLI, and harness session unable to write an envelope at all — only a cold-restart replay ever carried an id — and made env persistence unobservable from the E2E harness.

[[crates/scribe-server/src/hook_ingress.rs#bootstrap_envelope_id|bootstrap_envelope_id]] covers what the client cannot: a session already live when id-minting shipped, and any future client that omits the field. On the first delta that would actually persist it mints an id under the live-sessions write lock and stores it on the `LiveSession`, re-checking the field under that lock so concurrent deltas agree on one id, and so the close path deletes the same envelope the scheduler wrote. It is gated on the session having a `StartupBaseline`: without one the fold is dropped anyway. A hot handoff carries any existing envelope id for later cleanup but does not transfer the baseline or scheduler. This is the T016 compromise closing — the old code logged the missing id and discarded the capture, so a session could never start persisting without being re-created.

### Close-Time Envelope Delete

Clean user-initiated closes delete the on-disk envelope and its keystore DEK; non-clean exits (child shell dies, PTY EOFs) preserve them so cold-restart restore stays available per FR-007.

Each  stashes its `env_window_id` and optional `env_envelope_id` at create time so the close path can route the delete after the `session_to_window` mapping has been torn down. The envelope id flows from `ClientMessage::CreateSession.env_envelope_id` →  →  → `LiveSession`, and is `Some` from creation for every client-issued launch now that all three create paths mint one. Hot handoff carries both coordinates into the successor so a later clean close still deletes the predecessor's envelope. Clients predating client-side minting leave the id `None`; the hook-ingress bootstrap fills it on the session's first persistable delta, so the close path still names the envelope the scheduler wrote.

 is the clean-close path. After removing the session from the live registry and tearing down its [[crates/scribe-server/src/pty_guard.rs#PtyGuard|PtyGuard]] (which SIGHUPs and reaps the child off-worker), it calls  with the stashed coordinates. The call is best-effort: `delete_envelope` is idempotent and swallows `NotFound`, so it is safe to call when the feature was off at create time (no envelope exists) or when the persist scheduler had not yet flushed a first write. Errors are logged at `warn` against `target: "scribe_server::ipc_server"` but never block the close.

 sweeps the whole window via  after destroying every session it owns. Same best-effort posture — a missing per-window directory is success.

`QuitAll` deliberately deletes nothing. A quit ends the clients while every session keeps running here, and each window's cold-restart snapshot survives the quit precisely so a later crash or reboot can replay those panes — a replay that asks for its saved env by the same launch id the envelope is filed under, and whose scrollback recovery ([[server#Server#Crash Recovery Dump]]) is keyed the same way. The earlier per-window pre-sweep in [[crates/scribe-server/src/ipc_server.rs#handle_quit_all]] silently broke env restore for every quit-then-crash sequence; its stated rationale ("clients follow up with `CloseWindow`") described a client that no longer exists, since the GPUI client's quit path flushes and exits without closing windows. Staleness is not a counter-argument: a fresh terminal mints a fresh launch id and reads no envelope, so a quit-surviving envelope can only ever reach the restored pane it belongs to, and the orphan GC below retires the ones no snapshot still names.

 — the path that runs when the child shell exits or the PTY EOFs — deliberately does NOT delete. A session that ended because the user typed `exit` is still eligible for cold-restart restore, so the envelope must remain on disk until the user issues a `CloseSession` themselves.

### Orphaned Envelope GC

One startup sweep deletes envelopes that no window snapshot still names and that nothing has written for [[crates/scribe-server/src/env_store/gc.rs#ORPHAN_RETENTION|30 days]].

Client-side minting means every session writes an envelope, so any launch whose window never closed cleanly — a crash, a SIGKILL, a cold restart that replays under a fresh window id — leaves a file and a live keystore DEK behind forever. [[crates/scribe-server/src/env_store/gc.rs#sweep_orphaned_envelopes|sweep_orphaned_envelopes]] is spawned (never awaited) from `main` ahead of the accept loop, because a wedged secret service must not delay serving. Startup is also the only sound moment for it: mid-run, a window that has not yet flushed its snapshot is indistinguishable from an orphan.

[[crates/scribe-server/src/env_store/gc.rs#collect_orphans|collect_orphans]] does the whole scan inside one `spawn_blocking` and is pure over the filesystem, so a test can stage a tree and assert the exact selection without a keystore. It walks [[crates/scribe-server/src/env_store/store.rs#env_root|env_root]] and parses each `<window_id>` directory name, leaving foreign entries alone, and selects a `<launch_id>.envz` only when both gates open: the launch id appears in no `restore/windows/*.toml` snapshot, and the file's mtime is at least the retention old. The reference set is global across snapshots rather than matched per window, so an envelope whose launch was replayed under a fresh window id survives on the strength of the new window's snapshot. A snapshot the server cannot read or parse suppresses the entire sweep — deleting against an incomplete reference set is worse than deleting nothing, and the client drops unreadable snapshots on its own next cold start. Deletion goes through [[crates/scribe-server/src/env_store/store.rs#delete_envelope|delete_envelope]] so each file takes its DEK with it, and emptied window directories are pruned with `remove_dir`, which by refusing a non-empty directory is its own guard against removing a window that still holds a live envelope.

### Cold-Restart Restore-Apply

Spawn-side resurrection of persisted env onto a freshly-created PTY by staging a per-spawn apply file in the target shell's dialect for a shell-specific post-startup consumer.

[[crates/scribe-server/src/session_manager.rs#SessionManager#create_session|create_session]] resolves the shell binary and its `ShellKind` first, then inspects `request.env_envelope_id`. When `Some`, it calls [[crates/scribe-server/src/session_manager.rs#prepare_restore_env_file|prepare_restore_env_file]] with the launch's `(window_id, session_id, envelope_id)` triple plus that kind; the resulting `Option<PathBuf>` is forwarded into [[crates/scribe-server/src/session_manager.rs#build_pty_options|build_pty_options]] which sets `SCRIBE_RESTORE_ENV_DELTA_FILE` in the PTY env when present. Detection must precede the render — both the file body and its extension are shell-specific — which is why the kind is resolved in `create_session` and passed down to `prepare_session_launch` rather than derived there. Handoff-restored sessions never reach this path because per `research.md::R3.5` the existing PTY's process keeps its env across handoff, so no apply is needed.

[[crates/scribe-server/src/session_manager.rs#prepare_restore_env_file|prepare_restore_env_file]] is the fail-safe shim around the env_store. It loads `terminal.env_persistence.enabled` from config (loading off the hot path is fine since this only runs on the cold-restart code path), calls [[crates/scribe-server/src/env_store/store.rs#read_envelope|read_envelope]] to fetch and decrypt the envelope, renders the resulting `TerminalEnvDelta` via [[crates/scribe-server/src/session_manager.rs#render_restore_env_source|render_restore_env_source]], and writes it to a 0o600 temp file under `$XDG_RUNTIME_DIR/<flavor>/env-apply/<session_id>-<pid>.<ext>`. Every failure mode — feature disabled, no envelope on disk, keystore unavailable, decrypt error, no `XDG_RUNTIME_DIR`, write failure — returns `None` with a warning log; the session still spawns with rc defaults per FR-016. A `tokio::spawn`ed 60-second defensive unlink protects against the shell never sourcing/removing the file.

#### Per-Shell Restore Rendering

The apply file is rendered per `ShellKind` because a single POSIX body restores correctly in bash and zsh only — fish has no `export`/`unset`, PowerShell has neither and different quoting, and nushell cannot source a runtime path at all.

[[crates/scribe-server/src/session_manager.rs#render_restore_env_source|render_restore_env_source]] selects the dialect and [[crates/scribe-server/src/session_manager.rs#restore_env_file_extension|restore_env_file_extension]] the extension. Bash/zsh (and `Unknown`) get `.sh` with `export NAME='value'` / `unset NAME`, single quotes escaped as the canonical `'\''` idiom. Fish gets `.fish` with `set -gx NAME 'value'` / `set -e NAME`; fish single quotes recognise only `\\` and `\'`, so backslashes are doubled before quotes are escaped. PowerShell gets `.ps1` with `${env:NAME} = 'value'` / `Remove-Item -LiteralPath 'env:NAME'`, quotes doubled — the extension is load-bearing, since dot-sourcing any other extension resolves as a native command and applies nothing without raising. Nushell gets `.json`: `source` is parse-time in nu and rejects a runtime path, so `scribe.nu` reads `{"added": {…}, "removed": […]}` with `from json` and feeds it to `load-env`/`hide-env` — the hand-rolled POSIX-line parser it replaced silently dropped `'\''` sequences and every multi-line value.

[[crates/scribe-server/src/session_manager.rs#is_assignable_env_name|is_assignable_env_name]] drops names that are not `[A-Za-z_][A-Za-z0-9_]*` from every dialect. `compgen -e` reports bash's exported functions as `BASH_FUNC_name%%` with a function body for a value; no shell can assign those by name, and rendering one would splice shell syntax into a file the shell then sources.

`tests_apply` pins the rendered text per dialect and `tests_apply_shells` round-trips a quote-bearing, multi-line, and backslash-bearing value through each real interpreter — reading the probes through the checked-in `tests/fixtures/restore-env-recorder.sh` child process, so the assertion covers export and not merely assignment. Text assertions alone cannot catch this finding class: the pwsh case additionally asserts that the same body under a `.sh` name applies nothing.

Round-tripping through real interpreters has to stay hermetic, because pwsh resolving that misnamed file as a native command hands the path to `xdg-open` and opened an editor window on whatever desktop ran `cargo test`. The test-only `desktop_isolation::seal_child` in `shell_integration.rs` therefore strips `DISPLAY`, `WAYLAND_DISPLAY`, and `DBUS_SESSION_BUS_ADDRESS` from every shell child a test spawns and puts the checked-in `crates/scribe-server/tests/fixtures/opener-stubs/` directory ahead of the inherited `PATH`, so the assertion still observes "applies nothing" without the fallback reaching a session. The scrub also drops `TERM_PROGRAM` and the `SCRIBE_*` live-session exports: a suite run from a terminal *inside* Scribe otherwise leaks them into the drivers, where `SCRIBE_ENV_PERSIST=0` (env persistence disabled in settings) makes the fish spawn gate return before installing any capture state and the emit tests see zero helper calls. A driver that needs one of these sets it explicitly after sealing, which overrides the removal.

[[crates/scribe-server/src/session_manager.rs#runtime_dir_for_env_apply|runtime_dir_for_env_apply]] computes the per-flavor staging directory under `$XDG_RUNTIME_DIR`; absence of that env var disables the apply path (the user-runtime tmpfs is the only sound location for ephemeral 0o600 secrets). The flavor segment matches [[crates/scribe-common/src/app.rs#AppIdentity#slug|AppIdentity::slug]] so stable and `scribe-dev` cannot collide on the same login user. [[crates/scribe-server/src/session_manager.rs#ensure_runtime_subdir|ensure_runtime_subdir]] creates the directory tree with `create_dir_all` and re-applies 0o700 on the leaf for idempotency. [[crates/scribe-server/src/session_manager.rs#write_private_owner_only|write_private_owner_only]] writes the body through `OpenOptions::mode(0o600)` and `fsync`s before returning so the integration script never races a partially-written file.

The shell integration scripts (under `dist/`) apply `$SCRIBE_RESTORE_ENV_DELTA_FILE` only after user startup files: bash does so at the tail of its rc-emulating init, while zsh and fish defer from their pre-rc bootstrap to a self-removing first-prompt hook. The initializer removes the apply file, captures the post-restore baseline, and runs before the recurring delta hook; rc-only exports therefore remain baseline state rather than entering the persisted delta. This satisfies FR-008: an rc-set value cannot mask the explicitly restored value. AI zsh/fish tabs keep the pre-rc guard and let their post-login server preamble consume the file instead.

### Runtime Enable/Disable Transitions

`terminal.env_persistence.enabled` toggles live via `ConfigReloaded`: the `true → false` flip stops every per-session timer and deletes every envelope; `false → true` is a no-op (machinery lazy-initializes on the next baseline event).

 holds a cached `last_enabled: AtomicBool` field seeded by  at server startup from `scribe_common::config::load_config()` (failing safe to `false` if the load fails — FR-009 makes the feature disabled by default).  performs the one-shot seed;  is the atomic read-modify-write the reload handler uses to detect a transition in one operation without needing a separate snapshot of the previous config.

 calls  after the existing scrollback / workspace-root / `preserve_ai_scrollback` fan-out. That helper loads the freshly-on-disk config, atomically swaps `last_enabled`, and acts only when the value changed. On `true → false` it snapshots the live-session registry under a single read-lock to collect both `SessionId`s and distinct `env_window_id`s, drops the lock, calls  per session (halting the debounce timer without discarding the baseline + delta, so the next user-driven re-enable does not need a re-baseline), and then calls  per window. The deletes are best-effort: a keystore-unavailable DEK-cleanup failure is logged at `warn` and the loop continues — per R4.6 the disable transition is the only path that wipes on-disk env state for sessions that are still live (clean window/session close is the other path; `finalize_pty_reader` for dead-shell paths deliberately preserves the envelope per FR-007), and a partial-delete failure must not poison the rest of the reload. On `false → true` the helper just logs a marker — the  path already lazy-initializes per-session schedulers on the next `EnvChanged`, and its own `load_config` feature-gate observes the new value automatically.

Neither direction needs a server restart to reach a session created without an envelope id. After a `false → true` flip the next newly started shell emits a baseline, and the first delta behind it mints the session's envelope id via [[crates/scribe-server/src/hook_ingress.rs#bootstrap_envelope_id|bootstrap_envelope_id]] — so persistence begins on that shell rather than waiting for a client restart. Shells already running when the flip landed still see nothing, because their baseline emit was dropped at the feature gate and a delta without a baseline is not foldable; that is the documented "restart or re-init required" semantic, not a bootstrap gap.

Disabling lands on two independent timescales, and only the second one is the `ConfigReloaded` path above. [[crates/scribe-server/src/hook_ingress.rs#handle_env_changed_dispatch|handle_env_changed_dispatch]] re-reads `terminal.env_persistence.enabled` from disk on every `EnvChanged`, so the first event after the config file changes is already dropped and no further delta can reach a scheduler — persist writes stop even if no client ever sends the reload. The reload handler is the state teardown that follows: it is what disarms a debounce already counting down and what removes the envelopes those writes produced. Both halves are load-bearing, because the gate alone would leave a pending 100 ms timer free to flush one last write after the user disabled the feature. [[crates/scribe-server/src/env_store/mod.rs#EnvStoreState#drop_scheduler|drop_scheduler]] closes the scheduler channel and [[crates/scribe-server/src/env_store/mod.rs#persist_task|persist_task]] selects `biased` with the receiver ahead of its own deadline, so the task observes the close and returns rather than firing — which is what makes "writes stop immediately" true rather than "within 100 ms".

`last_enabled` records what the previous `ConfigReloaded` observed, not what the gate is currently reading, so the teardown is exact only if the client emits a reload for every config write. A flip that goes `false → true → false` between two reloads presents no transition at all and skips both the scheduler drop and the envelope delete — the gate has already stopped the writes either way, but envelopes written before the flip would survive it. Every settings write does send `ConfigReloaded`, which is what keeps the two views in step; a client that batches or skips one trades away the delete, not the write-stop.

Neither transition is signalled to a running shell, and no wire message exists to signal one. A shell live across a disable keeps emitting its baseline and per-prompt deltas byte-for-byte as before — the server drops them at the gate, so the disable is server-side only and costs the shell nothing it can observe. A shell live across a disable/re-enable round trip inside one server run resumes persisting on its next delta, since `drop_scheduler` keeps the baseline and working delta; the "restart or re-init required" semantic binds only the shell whose baseline emit was itself dropped. The shell-side gate is therefore exported at spawn — `SCRIBE_ENV_PERSIST`, see [[server#Shell Integration]] — and follows the value current at shell start, never changing under a shell that is already running. That is what makes "costs the shell nothing" true only for shells that outlived the flip: a shell *started* while the feature is off skips the whole snapshot/diff/fork path instead of running it for the server to discard.

## Remote Control

Feature 013 lets another of the user's tailnet machines attach to and control a window over TCP, gated by Tailscale identity and off by default. The owning-side transport, authorization, takeover, flow control, and audit all live here.

The wire contract is  and design decisions are recorded in `specs/013-remote-window-control/` (research D1–D9). The listener is started, stopped, and rebound live off config — the server is NEVER restarted for this feature.

Feature 014 adds a second, Tailscale-free transport — a LAN link over mutual TLS gated by explicit device approval (`specs/014-lan-remote-control/`) — alongside the tailnet path. This splits the former single-transport state into , threads in the new  (, ), and reuses the SAME `serve_connection` dispatch past its  gate. The tailnet path is preserved byte-for-byte.

Feature 015 (`specs/015-multi-machine-sharing/`) moves the owning side beyond single-controller takeover: when the owner's `sharing_mode` permits it, multiple approved machines attach to ONE window at once (tmux/screen -x style), joining additively without displacing anyone (supersedes 013's FR-007 single-controller invariant). The three per-window maps and their fixed-order tri-lock collapse into one  entry per window; output fans out per participant and input authorization becomes mode-aware. The default `SingleController` mode keeps every 013/014 path byte-identical. See  for the participant-set model, and  (v3) for the wire delta.

### Listener Lifecycle

 is the shared handle threaded into ;  owns the listener and serializes every start, stop, and rebind through one task so config edits never overlap.

At startup the supervisor applies the current `[remote]` config (a no-op when disabled, the default). Each  `ConfigReloaded` reaches , which pokes ; the supervisor re-reads config and (per transport)  starts, stops, or rebinds accordingly. Enabling enumerates the machine's tailnet bind addresses (fail closed if unavailable) and binds one `TcpListener` per address on `remote.port`, spawning an accept task each; disabling severs live connections and drops the sockets; a port or address change rebinds fresh while leaving live connections alone. The supervisor is spawned, not awaited, so a wedged tailscaled never delays local serving.

The one supervisor reconciles BOTH transports on each poke: after `apply_tailnet` it runs  against the `[remote.lan]` table and the trusted-network gate, starting, stopping, or rebinding the LAN listener independently (). Reloads are also poked by the network-change watcher, so a roam off a trusted network takes LAN dormant without a config edit ().

### Tailnet Identity

 is a minimal hand-rolled Tailscale LocalAPI client plus the same-account authorization policy (research D2). It is the ONLY component that talks to `tailscaled`, and the GUI client never does.

The daemon is reached by a plain HTTP/1.1 request over its Unix socket on Linux, or the sandboxed localhost TCP LocalAPI on macOS. Two endpoints are consumed: `status` yields  (own tailnet user id, device name, tailnet IPs) plus the same-account peer list for the picker, and `whois?addr=ip:port` yields the  behind an accepted connection.  is the pure rule: authorize iff the peer carries a concrete tailnet user id equal to this machine's own and is not tagged;  wires status, whois, and policy together. Any LocalAPI failure fails closed as `IdentityUnavailable` (never a wildcard bind, never an unauthenticated accept); a tagged or identity-less peer refuses `Unauthorized` with a `tagged` qualifier for audit.  enumerates the tailnet IPs the listener may bind to.

 also carries a display-only `login_name` — the signed-in account login read from the LocalAPI User map — kept distinct from the `user_id` that anchors authorization: the id gates access, the login only names the account in UI. The local-only transient  (`GetRemoteEnv`) resolves it through  for the Settings → Remote panel, failing closed to `{ account: None, tailscale_detected: false }` (FR-015) on any LocalAPI error.

### Accept Path

 hands each accepted stream to , which reads the preamble with  (bounded and timed out so a silent peer cannot hold an accept slot), authorizes via , gates the version, and answers with  before any window data flows.

On accept the connection is registered against the 8-connection cap and given a sever channel; a refusal emits an audit line and closes the socket. An accepted connection then runs the SAME  dispatch as a local client, distinguished only by its writer being a `::Remote` (bounded queue) rather than `Local` (direct socket) and by carrying the resolved tailnet identity so the accepted and disconnect audit lines can name the peer.

Admission is layered so a pre-auth peer cannot exhaust memory or slots (FR-013):  reserves a pending-handshake permit (`REMOTE_PENDING_HANDSHAKE_CAP`, 64) the instant it accepts — before spawning any handler or reading a byte, the same shape the local pending pool uses ([[server#Startup#Local Admission]]) — and `read_remote_preamble` caps the preamble frame at `REMOTE_PREAMBLE_MAX_BYTES` (~8 KiB, far below the shared 64 MiB frame cap) so an unauthorized peer can never force a giant allocation. The authorized-connection cap (`REMOTE_CONNECTION_CAP`, 8) is unchanged and still refuses `Busy` after authorization.

The disable race now returns a typed refusal:  is reserved BEFORE , so a connection that raced a live disable is refused `RemoteHandshakeReply { refusal: Disabled }` rather than told `accepted` and then silently dropped (FR-016). After acceptance  runs as a loop: an authorized connection may send a read-only `ListWindows` probe before its `Hello` (see ) and registers no window for it; every other non-`Hello` first frame still closes.

### Takeover and Control

 (`Local`, or `Remote` with device/login names) is tracked per participant in a window's  — feature 015's consolidated registry folding the three per-window maps and their tri-lock into one entry. A claim resolves through .

The claim mode is derived by  from `Hello.takeover`, the window's sharing mode, and local/remote origin. A takeover swaps the window's sole controller. With sharing off, a local no-takeover claim falls through to today's assign-different-window path and a remote no-takeover claim of a still-connected window yields `::LostControl`. With sharing on, a no-takeover claim naming a connected window is an additive share join from either transport (). The controller identity, the attached-sink set, and the `clipboard_gating` bit all live on the one `WindowShare` entry and move together under a single write-lock hold, so a near-simultaneous takeover burst still resolves to exactly one controller and no stale clipboard or policy state survives the swap (FR-014). A takeover captures the displaced writer(s) in `::Owned`'s `displaced` vec so  can send each `WindowTakenOver` after the lock is released — the sole previous owner in `SingleController`, or EVERY attached participant when a `takeover: true` ends an active share (FR-003). A `LostControl` outcome completes `Welcome` but leaves the current controller untouched and sends the reclaimant an immediate `WindowTakenOver` (never a silent seizure, FR-011).  records each transition.

### Control Authorization

Input authorization is enforced server-side, not merely signalled, and is mode-aware:  decides whether a connection's gated frame reaches the session, dropping it safely otherwise (FR-006/007/011).

The guard (feature 015, replacing the single-writer `connection_controls_window`) branches on the window share's mode. In `SingleController` it is the legacy `Arc::ptr_eq` writer-identity test — the same one  and  apply — so a displaced connection (which keeps its stale, never-revoked `attached_ids`) is a no-op, byte-identical to feature 013. In `SharedSingleTypist` a gated frame is honored only from the current `SingleTypist` holder, while `Resize` is exempt — accepted from any attached participant as an ungated per-participant viewport report (). In `FreeForAll` `KeyInput` and `Resize` are admitted from any attached participant, and the lifecycle/focus/search frames (which have no single holder to follow) fall to the owning machine's always-present `ControllerIdentity::Local` participant (spec Assumptions).

 applies the guard via  to every window-mutating or scrollback-reading frame (`KeyInput`, `Resize`, `CloseSession`, `CloseWindow`, `FocusChanged`, `SearchRequest`), and  applies it to `AttachSessions` so a displaced or unauthorized peer cannot re-steal the `PtyOutput` stream and clipboard-bridge routing by re-attaching. A local Unix-socket client is always its own window's controller, so the guard is a no-op locally; a `LostControl` reconnect never registered, so it fails the guard too, as intended.

### Sharing

Feature 015's owning-side sharing: when the owner's `sharing_mode` permits it, several approved machines attach to one window at once, and the roster, control state, and grid all live in that window's  (FR-001/002/004).

Design lives in `specs/015-multi-machine-sharing/`. The mode and its options are a snapshot, taken at each mutation, of the owner's  keys: `sharing_mode` ( = `SingleController | SharedSingleTypist | FreeForAll`), `control_acquisition` ( = `FreeClaim | RequestAndGrant`), and `participant_limit` (`Option<u32>`, `None` = unlimited), all `#[serde(default)]` so an existing config loads legacy behavior (FR-004/005/018, SC-006). A remote no-takeover claim that the mode admits registers the joiner through  as a new  — no existing participant disturbed (FR-002); a join beyond `participant_limit` is refused as a lost-control landing (`::LostControl` naming the current controller), leaving the active share undisturbed (FR-018).

Input control passes without disconnecting anyone through , which routes the v3 `ControlClaim` / `ControlRequest` / `ControlGrant` frames.  is the acquisition decision: under `FreeClaim` a viewer becomes holder instantly (the previous holder demoted to a still-live viewer); under `RequestAndGrant` it records a  and routes `ControlRequested` to the current holder (or the owner when unheld), resolved by  on the answering `ControlGrant`. The owning machine can always claim regardless of the option (FR-005/007). A holder's detach or eject leaves control unheld — no silent inheritance (FR-016) — and  pushes the full-state `ShareRoster` to every participant on each membership, control, or mode change (FR-008, SC-005).

The session runs one authoritative grid sized smallest-attached-wins (FR-012): each participant's `Resize` is stored as its viewport (ungated in shared modes), and [[crates/scribe-server/src/ipc_server.rs#apply_authoritative_grid|apply_authoritative_grid]] recomputes `min(rows)` × `min(cols)` over the attached viewports once the reports settle, driving the existing `resize_term` + `TIOCSWINSZ` path; because it is a pure function of the participant set it never flaps, and a participant detaching regrows the grid to the next-smallest.

Because that recompute is debounced, its apply is a deferred one in exactly the sense [[server#Sessions#Terminal Resize]] describes: [[crates/scribe-server/src/ipc_server.rs#apply_grid_to_window_sessions|apply_grid_to_window_sessions]] pushes the post-reflow `ScreenSnapshot` to each session's attached sinks, so every participant repaints at the authoritative grid instead of holding the viewport it happened to report.

That 250 ms debounce is trailing and single-armed. The first report arms one timer ([[crates/scribe-server/src/ipc_server.rs#AuthoritativeGrid#arm_trailing_apply|arm_trailing_apply]]); every report that lands while it counts down bumps the grid's report generation and restarts the window rather than arming a timer of its own ([[crates/scribe-server/src/ipc_server.rs#await_settled_viewport_reports|await_settled_viewport_reports]]). A continuous drag therefore costs one apply once it stops. With a timer per report each apply matured 250 ms behind its own report, so a drag that outlived one window drove a stream of mid-drag `TIOCSWINSZ` calls — and one `SIGWINCH` each — into every session in the window.

Changing any sharing key applies live over `ConfigReloaded` with no restart:  takes a  and rewrites active shares immediately (FR-017) — `SharedSingleTypist` demotes all participants to viewers with control unheld, `SingleController` detaches every remote participant with the legacy `WindowTakenOver` displaced notice, `FreeForAll` makes everyone a typist — and cancels any pending control request, informing the requester.

Session-initiated clipboard requests route through : an OSC 52 write goes to the current control holder, falling back to the owning machine when control is unheld or in free-for-all where no single holder exists (FR-013). Revoking a device or severing a transport ejects only the affected participant via the existing sever→detach path (), which detaches its sessions, drops it from the share, and — through  on the 013 remote-audit target — records the departure as `eject` (versus `leave` for a clean disconnect); the share continues for everyone else (FR-011/015, SC-007).

#### Local Additive Join

A share belongs to the window, not to the transport a participant reached it over: with sharing on, a *local* second process that explicitly names a connected window and sets join intent attaches additively, exactly as a remote peer does.

Feature 015 gated `::ShareJoin` on a remote transport, so on one machine a shared window still admitted only its owner — a second local client asking for that window was handed an empty window of its own instead.  now selects `ShareJoin` for a local transport only when `Hello.join_window` is true and `sharing_mode` is not `SingleController`. A normal restore may also name a connected window, so the id alone is not intent. Local claims without the bit use `LocalPlain`; remote join, reconnect, and takeover selection is unchanged.

The consequence that matters is two attached sinks on one session: the share admits any participant in a shared mode, so `AttachSessions` adds the joiner's sink to the set additively instead of replacing the incumbent's, and both processes receive the same `PtyOutput`. That is what the visual E2E shared-pane rig runs on (): `scribe-test` keeps observing the very pane the GPUI client renders.

### Flow Control

EVERY connection's writer is an `OutputSink` handle on a bounded output queue (`OUTPUT_QUEUE_PTY_BYTES`, 4 MiB) drained by a dedicated task, so no stalled consumer can block the server's authoritative Term or other clients (research D5, PR-004).

Feature 013 interposed the queue on remote connections only; local Unix-socket clients kept writing inline from the fan-out path. That left the whole class open on the loopback side: a SIGSTOP'd or merely slow local client filled its socket buffer, `pty_reader_task` wedged on the write, the PTY back-pressured, and because the `Term` is shared every other viewer of that session froze with it. `handle_client` now builds the same queue and drain task the remote paths do, so the reader never awaits a socket — local or remote — inline.

Feature 015 fans that per-connection queue out per participant: a session's `client_writer` holds an `AttachedSinks` set (one sink in `SingleController`, N in a shared window) whose membership is maintained by `Arc::ptr_eq`, and `send_to_client` iterates the set so one `PtyOutput` frame reaches every attached participant, each buffered on its OWN queue. A slow participant's overflow, backlog-drop, and resync are therefore confined to its own queue — the session, the owning machine, and the other participants are never slowed (FR-009, SC-004).

`AttachedSinks` sits behind a **std** mutex, not a Tokio one. Every operation on it — buffer append, enqueue, membership edit — is non-blocking, so the compiler now rejects any attempt to hold the per-session set across a sink await. The prior Tokio mutex was held across the inline local write, which is how one stalled client could serialize the whole session's fan-out.

`PtyOutput` frames are coalesced in the queue; on overflow `drop_pty_backlog` drops that connection's queued output and marks the affected sessions replay-dirty, and `send_resync_replay` sends a fresh `SessionReplay` when the consumer drains (catch-up-to-current, mirroring tmux `%pause`). Control frames — the takeover notice, session-exit, workspace updates, the sever `RemoteDisconnect` — ride an `OutFrame::Keep` lane the backlog drop leaves intact, so they are never lost. Clients already treat any `SessionReplay` as full pane-state replacement, so resync needs no new client logic.

Beyond that per-`PtyOutput` overflow, the queue also bounds its TOTAL footprint so a stalled consumer cannot grow unbounded on the `Keep` (control/replay) frames the `PtyOutput` cap does not govern: `enforce_queue_ceiling` caps total queued bytes (`OUTPUT_QUEUE_TOTAL_BYTES`, 16 MiB) and frame count (`OUTPUT_QUEUE_MAX_FRAMES`) with a shed-then-close policy. It sheds droppable session backlog first and closes only when ordinary control traffic still cannot fit (FR-013); reconnect is not a recovery for a replay that deterministically exceeds the same ceiling.

Combined terminal-image replay therefore preflights the whole atomic burst against each sink's remaining Keep budget. If the canonical scene fits after droppable backlog is ignored, it stays non-droppable and is queued whole. If it does not fit, the sink receives the truthful two-record empty-scene `Begin`/`Commit` instead, with a typed `quota_exceeded` replay rejection on `Begin`; the pane drops stale placements, renders the existing localized diagnostic beside the application's text fallback, clears its replay debt, and keeps the connection alive. `drop_pty_backlog` still deliberately cannot remove Keep frames, and `enforce_queue_ceiling` still closes an already-over-ceiling Keep queue — the image path avoids constructing that impossible queue in the first place.

A vanished (FIN/RST-less) peer no longer holds its authorized slot indefinitely:  sets `SO_KEEPALIVE` plus tuned idle/interval timers (`REMOTE_KEEPALIVE_IDLE` 60 s, `REMOTE_KEEPALIVE_INTERVAL` 15 s) via `socket2`, which maps them to the right per-platform sockopts (`TCP_KEEPIDLE` on Linux, `TCP_KEEPALIVE` on macOS) so a dead TCP path is dropped in a few minutes with no false positives on a live-but-idle viewer. A remote-only idle-read timeout (`REMOTE_IDLE_READ_TIMEOUT`, 30 min) in both the pre-`Hello` and message loops — local reads stay untimed — remains the application backstop for a peer that is TCP-alive but app-silent.

### Disable and Sever

Turning `remote.enabled` off must sever within 2 s (FR-016).  stops accepting, then  fires every live connection's sever channel; each connection best-effort-sends `RemoteDisconnect { reason: Disabled }` under a bounded write timeout and closes.

`enabled` is flipped under the same lock the sever drains under, so a connection accepted mid-disable cannot slip past the sever. Owning-side sessions are untouched — only the remote client writers drop — and the connecting side auto-reconnects or, having received the notice, reports the disable as fact (). A bulk `severed` audit line records the event.

### Per-Transport State

Feature 014 (analysis C4/S1) split 013's single-transport `RemoteControl` into independent per-transport state so neither transport can disable, sever, or starve the other, and a per-device revoke can target exactly one device's connections.

Each transport (tailnet, LAN) owns a  carrying its own `enabled` flag, connection-cap semaphore, pending-handshake-cap semaphore, and ;  holds `tailnet` and `lan` instances plus a shared connection-id allocator. The tailnet caps (`REMOTE_CONNECTION_CAP` 8 / `REMOTE_PENDING_HANDSHAKE_CAP` 64) are unchanged; the LAN transport gets its OWN `LAN_CONNECTION_CAP` (8) / `LAN_PENDING_HANDSHAKE_CAP` (64), so a flood of LAN dialers cannot exhaust tailnet admission.  takes the transport and, for LAN, the pinned `device_id`, and its disable-race check reads that transport's `enabled` under the same lock the sever drains under — so tailnet behavior is byte-identical.

The `SeverRegistry` keeps a secondary `device_id → {connection ids}` index — populated only for LAN (whose connections carry a Scribe identity; tailnet entries carry `None`) — so a per-device revoke severs just that device's connections via  while every other connection and all owning sessions are untouched (FR-010).  is the per-device revoke consumer;  still severs one whole transport on disable or dormancy (FR-012).

### LAN Identity and TLS

The LAN link is mutual TLS 1.3 pinned to a per-install device identity.  mints the identity and  wraps the stream and enforces the pin; both fail closed when the identity or keyring is unavailable.

 is a self-signed Ed25519 X.509 cert whose  is `SHA-256(SubjectPublicKeyInfo)` — the 32-byte trust anchor and mDNS TXT `id`, stable across a cert re-mint.  generates it once on first LAN enable, sealing the private key in the OS keyring (never plaintext on disk);  and  derive the word-list SAS shown on the approval prompt and in Settings (research D8). A headless machine with no keyring cannot be an owning-side LAN host in v1 (fails closed).

The CONNECTING side is a spawned `scribe-client` process — a DIFFERENT binary from the `scribe-server` that sealed the key — so it does NOT read the keyring itself.  serves this machine's own identity (public cert + sealed key via ) over the local-socket-only  first frame, and the client rebuilds it with  (re-deriving the same `device_id` from the key's SPKI, no keyring touched). This exists because macOS's `keyring` `apple-native` backend uses the legacy `SecKeychain` API, whose per-item ACL trusts only the CREATING binary; the different client binary is denied (errSecInteractionNotAllowed) with no usable prompt, so a direct client-side keyring read fails closed before any TCP. Routing through the server (the sole keychain accessor) is cross-platform — used on all OSes — and needs no entitlements, signing, or FFI. The handler mints on first use (`load_or_generate`), and the reply is refused over any remote transport exactly like `GetLanEnv` (see ).

 builds the `tokio-rustls` mutual-TLS config — both peers present and verify certs — and  /  run the async handshake. The  fills BOTH rustls verifier roles: it classifies a handshaking peer's  against the live trusted-device store as  `Known` (verified) or unknown (pending TOFU, deferred to the app-layer approval gate) and — critically — still delegates the handshake-signature check to `rustls::crypto::verify_tls1{2,3}_signature`, so a peer lacking the private key hard-fails (research D3/D4). The pin check is a synchronous non-blocking store lookup via the  trait (adapted by ), so every approve/revoke is visible on the next handshake with no rebind.

### LAN Discovery and Network Trust

The LAN surface is dormant unless LAN access is enabled AND the machine is on a network the user marked trusted — a defense-in-depth activation gate (research D5), not authentication.  runs mDNS and  is the gate.

 advertises `_scribe._tcp.local.` (port in the SRV record; `id`/`protovers`/`host` in TXT) via  and browses peers via , exposing a deduped, subnet-filtered view through a  the supervisor publishes for the local-only `ListLanPeers` handler. Advertising stops (mDNS goodbye) on disable, on leaving a trusted network, or on shutdown.

 reads the physical-LAN  (default-gateway MAC as the primary anchor plus subnet, via `netdev`), failing closed on a zero/unresolved gateway MAC, no default route, or a VPN-tunnel default route.  persists the trusted networks and answers "am I on a trusted network?" () plus add/remove/list;  is what  consults each reload.  periodically re-evaluates trust and pokes a reload on a status flip, so a roam onto an untrusted network goes dormant promptly without a config edit (FR-018, analysis C5).

### LAN Accept and Approval

 binds the LAN listener only while enabled and on a trusted network; each accepted connection then runs TLS → `LanHello` → the device-approval gate → the 013 version gate → the SAME `serve_connection` dispatch.

When the gate passes,  binds one listener per physical-LAN address and spawns a ; going dormant or disabling calls , which aborts the accept tasks, severs live LAN connections, and stops advertising. The accept loop reserves the LAN pending-handshake permit the instant it accepts (mirroring the tailnet ) and hands each stream to .

 is the full sequence: the mutual-TLS handshake,  for the bounded `LanHello`,  for the trust decision, the exact protocol-version gate ( with `IncompatibleVersion`), the LAN connection cap,  recording the sever channel under the `device_id` index (re-checking the disable race), and finally the encrypted stream handed to  with a bounded `::Remote` queue and a `Remote(device label)` controller — identical to 013 past this point.

 is the trust decision (contract step 4): a  `Known` device proceeds immediately; an unknown device reserves a bounded, counted hold from  (cap , per-hold  so unapproved dialers cannot occupy admission indefinitely), sends  (`LanApprovalPending`), and raises the prompt on the owning machine's own local clients via . The owning user's `LanApprovalDecision` resolves the hold ( returns an ); on approve  writes the  pin () and the connection proceeds — revealing NO window or session data until then (SEC-001).

### LAN Trust Management

The trusted-device and trusted-network stores are managed through local-only dispatch handlers the connecting client and Settings call on their OWN server; the GUI never touches the trust stores or the remote TLS stream directly.

, , , , , and  answer the  from this machine's own view — refused over any remote transport, like 013's `ListRemotePeers`. Add-current-network fingerprints the physical LAN or explains why it cannot (); revoke calls , which drops the pin and severs only that device's live connection through the `device_id` index (FR-010). The owning-side approval decision arrives as , which resolves the matching pending hold. `AddCurrentNetworkTrusted` / `RemoveTrustedNetwork` mutate through  /  and poke a reload so  re-evaluates activation (removing the current network goes dormant).

### Audit Log

Every remote lifecycle event is a structured server-log line on the  tracing target (research D9, FR-017): `accepted`, `refused`, `disconnect`, and bulk `severed`.

 maps each  to its canonical log token (`disabled` / `unauthorized` / `identity-unavailable` / `version` / `busy`) — the same taxonomy the wire refusal uses — with an optional `detail=tagged` qualifier when an `unauthorized` refusal was a tagged node. There is no dedicated audit UI in v1; the server log is the record.

Feature 014 extends the same target with `lan:`-prefixed lifecycle lines (FR-017). `::Lan` carries the short device id so `serve_connection` emits `lan: accepted` / `lan: disconnect`;  maps each  to its token (`declined` / `not-trusted-network` / `disabled` / `version` / `busy`) for `lan: refused`; approvals, declines, and revocations log `lan: approved` / `declined` / `revoked`; and  tokenizes a  for the bulk `lan: dormant` line emitted when the transport goes dormant (network untrusted / disabled).

### Handoff Interaction

A zero-downtime upgrade carries NO remote-control state — neither the listener's enabled/bind flag nor any active remote-connection metadata — so  is unchanged by this feature (see ).

The receiver re-derives the `[remote]` listener purely from on-disk config via the same `remote_supervisor` startup both the normal and `--upgrade` paths share; the old server's remote TCP connections drop when its process exits, and the remote client auto-reconnects to the rebound listener (research D6).

Feature 014 is likewise handoff-stateless: the LAN listener, the device identity, and both trust stores re-derive after upgrade from on-disk config, the keyring, and the trusted-device / trusted-network stores — the device keypair lives on disk and in the keyring, so it need not ride the envelope — leaving  unchanged; live LAN connections drop and auto-reconnect exactly as the tailnet path does.
