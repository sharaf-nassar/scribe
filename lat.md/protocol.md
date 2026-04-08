# Protocol

The IPC protocol defines all messages exchanged between the server and its clients over a Unix domain socket.

## Transport

Messages use length-prefixed MessagePack encoding defined in [[crates/scribe-common/src/framing.rs#read_message]].

### Frame Format

Each frame is a 4-byte big-endian u32 payload length followed by the MessagePack-serialized message body. The maximum payload size is 256 MiB to accommodate reattach scenarios that send many screen snapshots at once.

### Socket Path

The server socket lives at a platform-specific runtime directory selected by the active install flavor.

On Linux this is `/run/user/{uid}/scribe/server.sock` for stable installs and `/run/user/{uid}/scribe-dev/server.sock` for `scribe-dev`. On macOS it uses `~/Library/Application Support/Scribe*/run/server.sock` so GUI clients and launchd services share a stable path. All socket path logic is in [[crates/scribe-common/src/socket.rs#server_socket_path]].

### Security

Every connection is verified by checking the peer UID via `SO_PEERCRED` on Linux or `getpeereid` on macOS. Connections from a different UID are rejected. The server enforces a maximum of 32 concurrent connections per UID.

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

`Subscribe` registers for output from a list of session IDs (max 256). `RequestSnapshot` fetches a single session's full screen state.

`AttachSessions` reattaches to detached sessions with dimensions, receiving stored metadata (title, CWD, shell basename, session context, git branch, AI state) and a screen snapshot.

### Workspace Management

`CreateWorkspace` creates a new workspace with the next accent color. `ReportWorkspaceTree` sends the client's current split layout to the server for persistence.

### Automation

Window automation messages let the CLI inspect windows and ask a connected client to execute the same actions exposed by keyboard shortcuts and the command palette.

`ListWindows` returns every connected window with its ID, session count, and connection status. `DispatchAction` targets an optional window ID and carries an [[crates/scribe-common/src/protocol.rs#AutomationAction]] value such as settings, find, new tab, split, close, new window, or profile switch. The server answers each dispatch with either `ActionDispatched` naming the routed window or `Error` when no connected target exists.

### Connection

`Hello` is the first message sent, carrying an optional window ID for multi-window reconnection. The server responds with [[protocol#Server Messages]] `Welcome`.

### Configuration

`ConfigReloaded` notifies the server that the config file has changed, triggering scrollback limit and shell integration updates across all live sessions.

### Update Control

`TriggerUpdate` confirms a download. `DismissUpdate` suppresses the notification for the current version.

## Server Messages

Messages sent from the server to clients, defined in [[crates/scribe-common/src/protocol.rs#ServerMessage]].

### Terminal Output

`PtyOutput` carries raw PTY bytes for a session. `ScreenSnapshot` sends a full [[protocol#Screen Snapshots]] for reconnect or initial attach. `ScrolledSnapshot` returns scrollback at a requested offset.

`SearchResults` pairs with `SearchRequest` and returns absolute grid spans for the current query so the client can highlight and jump between matches without replaying search locally.

### Session Events

`SessionCreated` confirms a new session with its workspace and shell basename. `SessionExited` reports a session's exit code. `Bell` forwards BEL characters.

### Metadata

`AiStateChanged` and `AiStateCleared` report AI process state from OSC 1337. `CwdChanged` reports working directory from OSC 7.

`TitleChanged` reports window title from OSC 0/2. `SessionContextChanged` reports shell-emitted remote-host and tmux metadata from OSC 1337 `ScribeContext`. `CodexTaskLabelChanged` and `CodexTaskLabelCleared` report the separate Codex task-label channel used for tab naming. `GitBranch` reports the detected git branch for a session's CWD. `WorkspaceNamed` reports auto-detected workspace names. `PromptReceived` carries the session ID, AI provider, and submitted prompt text for display in the prompt bar UI.

### Connection

`Welcome` responds to Hello with the assigned window ID and a list of other connected windows. `WindowClosed` confirms that a `CloseWindow` request permanently removed that window. `QuitRequested` is the shutdown acknowledgment for `QuitAll`.

`SessionList` returns all sessions grouped by workspace in response to `ListSessions`. Each session entry includes the active AI state, if any, the last known AI provider hint, the last known Codex task label, the shell basename, and the last known session context so reconnect can restore provider-aware titles and remote labels without waiting for a fresh prompt. `WorkspaceInfo` sends workspace metadata (name, accent color, split direction).

When `SessionList` also includes a workspace tree, that tree is the authoritative workspace layout. The `split_direction` field is only needed for the legacy reconnect fallback where older servers omit the tree and the client must repair the linear default layout once during startup.

### Automation

Automation responses expose connected windows to the CLI and let the server forward actions into a specific client window.

`WindowList` returns the payload requested by `ListWindows`. `RunAction` delivers an [[crates/scribe-common/src/protocol.rs#AutomationAction]] to the target client, which executes it on the UI thread through the normal action handlers instead of a separate automation-only path. `ActionDispatched` acknowledges to the requester that the server successfully routed the action to a specific connected window.

### Update Notifications

`UpdateAvailable` announces a new version with a release URL. `UpdateProgress` reports download, verification, and installation state transitions.

### Error

`Error` carries a human-readable error message string.

## Screen Snapshots

The wire format for terminal state, defined in [[crates/scribe-common/src/screen.rs#ScreenSnapshot]].

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
