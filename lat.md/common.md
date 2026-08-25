# Common

Shared types and utilities used by every Scribe crate: IPC , identity types, error definitions, screen snapshots, configuration, theme system, and socket path conventions.

## AI State

Tracks supported AI coding tool lifecycles through structured hook events and typed Rust values.

 is a five-variant enum (`IdlePrompt`, `Processing`, `WaitingForInput`, `PermissionPrompt`, `Error`) shared by Claude Code, Codex, and Pi.  supplies stable provider IDs, display and binary names, task-label support, and resume capability;  carries that provider alongside optional metadata fields (`tool`, `agent`, `model`, `context`, `conversation_id`). `AiProvider::all()` contains the three user-visible providers and excludes the synthetic `System` hook provider. Pi's stable id and binary are both `pi`; [[crates/scribe-common/src/ai_state.rs#AiProvider#supports_resume|supports_resume]] is false and its resume argument list is empty, while Claude Code and Codex remain resumable. See [[test#Test Harness#Pi Provider Compatibility#Provider identity and config]].

[[crates/scribe-common/src/protocol.rs#AiLaunchSpec]] is the shared wire shape for AI session intent: provider, [[crates/scribe-common/src/protocol.rs#AiResumeMode|new/resume mode]], and optional conversation id. `CreateSession.ai_launch` is additive and defaults to `None`; AI creates set it with `command: None`, making the server the sole argv owner. Generic custom commands retain `command: Some(...)` and no structured AI intent. `ShellTool::Pi` remains the old-peer launch and restore representation; [[crates/scribe-common/src/protocol.rs#pi_launch_metadata]] chooses a fresh structured Pi launch only when the peer negotiated it. [[crates/scribe-common/src/protocol.rs#REMOTE_PROTOCOL_VERSION]] is `8` because Pi provider state can cross a remote connection. `AiResumeMode` remains the canonical persisted restore-state type, whose serde names stay exactly `New` and `Resume`. See [[test#Test Harness#Pi Provider Compatibility#Local capability negotiation]] and [[test#Test Harness#Pi Provider Compatibility#Remote and handoff version gates]].

The `context` field (`Option<u8>`) carries the AI tool's context-window fill percentage (0–100). It is populated by the Claude statusLine adapter and the Codex hook adapter via context-only `ContextChanged` hook events that the server applies as a partial patch on the live `AiProcessState`. Producers never assert state, only context. Codex reads the hook `transcript_path` and derives the percentage from that rollout's latest `last_token_usage.total_tokens` over `model_context_window`; cumulative `total_token_usage` is ignored because it is session-wide billing usage. The  config controls the warn/danger band boundaries used by the prompt bar and tab-bar displays.

State transitions are owned by the per-provider hook adapters. Claude Code hooks report tool, notification, stop, and prompt-submit events through `dist/ai-hook-claude.sh`; Codex reports prompt, permission, tool, stop, and context events through `dist/ai-hook-codex.sh`. Each state-only event would otherwise carry `None` for `context`, `model`, `tool`, `agent`, and `conversation_id`, clobbering values set by an earlier same-provider event.  carries those optional fields forward from the previously stored state when the new event leaves them unset and the provider matches, so a state-only hook firing between context refreshes does not erase the live percentage. Switching providers (e.g. Claude → Codex) skips the merge so cross-provider state does not bleed, and so does switching conversations: [[crates/scribe-common/src/ai_state.rs#AiProcessState#switched_conversation_from|switched_conversation_from]] reports a break whenever both states name a conversation and the names differ, and a break inherits nothing. That guard is what makes the context meter actually reset — a new conversation opens an empty window, so merging the retired conversation's `context` into its first state-only hook would hand the old fill straight back to the client that had just cleared it. Only two *named* ids can disagree; an event that omits the id says nothing about which conversation it belongs to, and a first sighting is never a switch. The dedicated `ContextChanged` hook event applied by  keeps the producer/state separation honest: a status-line refresh patches only the percentage and re-broadcasts the existing state, never replacing it.

### A conversation switch breaks the metadata merge

Verifies that a state-only hook from the same conversation still inherits the live `context`, while the same hook naming a different conversation inherits neither the percentage nor the model, so the new conversation's meter starts empty.

## AI Context Chrome

Single source of truth for how a context-window fill percentage is spelled on screen, shared by every surface that displays one.

Two surfaces show the percentage in every band but use different shapes: the per-pane prompt bar draws a segmented meter (`▰▰▱ 72%`), while the tab label appends a bare suffix (`72%`).  and  own those strings and pulse suppression, so the GPUI prompt bar (), the GPUI tab bar (), and the E2E harness that asserts on them () cannot drift apart.

Only text is shared. Band colors stay with each surface because they resolve through different palettes — the prompt bar reads the configured  hex colors while the tab bar uses its own fixed band colors.

### Meter label fills and clamps

Verifies  lights segments by `div_ceil` (any non-zero percentage fills at least one) and clamps above 100%.

### Tab suffix shows known context unless pulsing

Verifies  returns the bare `NN%` suffix for every known percentage and `None` while a pulsing attention state owns the UX.

## Agent Control Contract

Shared agent types expose a narrow, versioned local contract without serializing the server's broader internal session and AI records.

### Request and response DTOs

[[crates/scribe-common/src/agent.rs#AgentRequest]] and [[crates/scribe-common/src/agent.rs#AgentPayload]] are tagged enums for world, sibling, screen, action, input, and capability operations.

Every request carries `request_id`, a caller-supplied label, and optional origin session. [[crates/scribe-common/src/agent.rs#AgentResponse]] echoes the id and wraps either a successful payload or [[crates/scribe-common/src/agent.rs#AgentError]]. `AGENT_SURFACE_VERSION` is 1 and is reported by the capability payload.

World DTOs expose window/workspace/session identity and status, with optional title, CWD, provider, AI state, task label, and context fill omitted when unavailable. They deliberately exclude launch ids, retained prompt state and text, conversation ids, model/tool/agent metadata, environment envelopes, controller identity, and participant identity. Screen replies identify the pane and include normalized text, line count, truncation, capture time, and snapshot id.

### Capabilities, actions, and errors

Five independent capabilities separate metadata, content, ordinary actions, destructive actions, and input injection.

[[crates/scribe-common/src/agent.rs#AgentCapability#for_action]] maps close-pane, close-tab, and open-update-dialog to the destructive axis and exhaustively maps every other current automation action to the ordinary axis. `AgentPolicyMode` is `Deny`, `Allow`, or `Prompt`, defaulting to `Deny`. Capability status replies pair each axis with its current mode.

`AgentError` carries typed failures for policy, timeout, lookup, ambiguity, compatibility, bounds, capacity, action completion, and internal faults. `AgentActionResult` reports the original action, completed/failed outcome, and optional created session id; there is no queued-success state.

### Configuration

`ScribeConfig.agent_api` is an additive top-level policy and bounds table whose absence is safe.

[[crates/scribe-common/src/config.rs#AgentApiConfig]] defaults all five capabilities to `Deny`: all-Deny is the off state. Defaults are 256 KiB responses, 1,000 scrollback lines, 4,096 input bytes, 60,000 ms prompt timeout, 500 ms burst reuse, and 1,500 ms activity dwell. Deserialization clamps them respectively to 256 KiB, 10,000 lines, 65,536 bytes, 300,000 ms, 5,000 ms, and 10,000 ms, while unknown keys remain forward-compatible.

The shared table is loaded at startup and on `ConfigReloaded`; the settings client and server projection both use the same type, so no translation vocabulary can drift from the wire capability names.

## Configuration

Unified TOML config for server and client, deserialized from the active install
flavor's XDG config root into
[[crates/scribe-common/src/config.rs#ScribeConfig]].

Stable installs read `~/.config/scribe/config.toml`, while `scribe-dev` reads
`~/.config/scribe-dev/config.toml`. `ScribeConfig` has ten top-level sections:
`appearance`, `theme`, `terminal`, `keybindings`, `workspaces`, `update`,
`notifications`, `remote`, `github_ci`, and `agent_api`.
[[crates/scribe-common/src/config.rs#load_config]] returns
`ScribeConfig::default()` when the file is absent. Prompt-bar configs still
accept legacy `prompt_bar_bg` as an alias for `prompt_bar_second_row_bg`.

### Cached Snapshot

[[crates/scribe-common/src/config.rs#config_snapshot]] hands out a process-wide `ConfigSnapshot` — the parsed [[crates/scribe-common/src/config.rs#ScribeConfig]] plus the [[crates/scribe-common/src/theme.rs#Theme]] resolved from it — so hot paths pay neither the disk read nor the parse.

The snapshot exists for callers that resolve config on a per-sequence basis. Dynamic color queries are the motivating case: an OSC 4 probe over all 256 palette indices used to cost one `config.toml` read and parse per index, plus a second read and parse of the external theme file when the active theme is not a built-in preset. Warm, the same probe costs zero disk reads.

A load failure is not cached, so a transiently unreadable config does not pin an error for the life of the process. Two paths drop the snapshot: [[crates/scribe-common/src/config.rs#save_config]] after it writes the file, and the server's `ConfigReloaded` handler before it re-reads. Callers that must observe the freshly-on-disk bytes — the env-persistence transition check, the settings round-trip — keep using [[crates/scribe-common/src/config.rs#load_config]] directly.

### Appearance

Font, cursor, opacity, motion, theme name, scrollbar, focus border, tab and status
dimensions, and the five optional prompt-bar color overrides live in
[[crates/scribe-common/src/config.rs#AppearanceConfig]].

 provides per-side padding (top/right/bottom/left) with a `clamped()` helper that enforces the `0.0..=50.0` range.  is a three-variant enum (`Block`, `Beam`, `Underline`).

### Inline Custom Theme

[[crates/scribe-common/src/config.rs#ThemeConfig]] supplies foreground,
background, cursor, selection, and 16 ANSI colors when
`appearance.theme == "custom"`.

### AI Context Thresholds

Two-boundary band model classifying context-window fill into Ok, Warn, and Danger, used to color the prompt-bar context indicator and tab inline % display.

 holds `warn` (default 70) and `danger` (default 90) as `u8` percentages, plus three hex-string color fields: `ok_color`, `warn_color`, and `danger_color`.  is a three-variant enum (`Ok`, `Warn`, `Danger`) returned by `AiContextThresholds::band(pct)`. A fill value at or above `danger` maps to `Danger`; at or above `warn` maps to `Warn`; below `warn` maps to `Ok`. Inverted configs (`warn > danger`) are normalized by treating `min(warn, danger)` as the effective warn threshold so the Warn band remains reachable.

### AI State Colors

Per-state visual config for AI indicators lives in , which holds one  per `AiState` variant.

Each `AiStateEntry` carries a color, pulse animation duration (`pulse_ms`), auto-clear timeout (`timeout_secs`), and booleans for tab indicator and pane border.  is a polymorphic color type that accepts either a fixed `#rrggbb` hex string or an `"ansi:N"` palette index (0–15) that adapts to the active theme at render time.

`pulse_ms` is interpreted as a full millisecond duration across the stored `u32` range, so very slow indicator pulses are preserved instead of being clamped to 16-bit timing.

### Terminal

[[crates/scribe-common/src/config.rs#TerminalConfig]] groups terminal behavior,
input, chrome, AI integration, clipboard policy, environment persistence, and
terminal images.

`paste_confirmation` (bool, default `false`) gates a multi-line or control-character paste behind a confirmation dialog before it reaches the PTY, but only when the focused application has not enabled bracketed paste (spec 011); see .

 (TOML namespace `terminal.clipboard.*`) carries the OSC 52 read/write policy modes ( — `deny`/`allow`/`prompt`), a `max_write_bytes` cap (default 16 MiB, ceiling 512 MiB), a `focus_gate_writes` opt-in toggle, and a `burst_window_ms` reuse window. Wired into the foundational types only as of this checkpoint; the server-side gating engine and client-side prompt dialog land in the OSC 52 clipboard-gating feature work.

`scroll_pin` (bool, default `false`) enables split-scroll in AI panes, but only while the pane is in the normal screen buffer; alternate-screen TUIs fall back to the regular live view. `preserve_ai_scrollback` (bool, default `true`) strips AI-session `CSI 3 J` scrollback clears, resets its trim epoch on prompt/attention boundaries, captures the epoch baseline after the first filtered redraw, and trims later redraw clears back to that baseline so committed transcript history survives without duplicate inline frames piling up.

Prompt bar fields: `prompt_bar` (bool), `prompt_bar_font_size` (`Option<f32>`, 8–32, unset by default), `prompt_bar_position` (: Top or Bottom), and optional row-surface overrides for the first row, second row, text, first icon, and latest icon. Unset, the strip follows `appearance.font_size` so its text matches the terminal text beside it; a value pins an explicit size and is clamped to 8–32 when written through the settings stepper.

 independently toggles CPU, memory, GPU, and network display.  wraps a single `enabled` flag for shell prompt marks.  maps an  to the matching integration toggle.

`focus_follows_mouse` is a flat `[terminal]` boolean that defaults to `false`.
The client reads it from the live config on each pointer decision, so the file
watcher and settings save path change running windows without a restart.

#### Focus follows mouse defaults off and persists an opt-in

Verifies the absent key keeps click-to-focus and an explicit `true` survives the same TOML serialization round trip used by [[crates/scribe-common/src/config.rs#save_config]].

#### Prompt bar font size follows the terminal

Verifies `prompt_bar_font_size` defaults to `None`, that an unset override survives the flattened `toml::to_string_pretty` round trip [[crates/scribe-common/src/config.rs#save_config]] performs without writing a key, and that an explicit value parses back as `Some`.

### Keybindings

 exposes 50+ configurable actions across pane navigation, workspace splits, tab management, clipboard, scrolling, command-jump navigation, zoom, and terminal word motion, including Claude Code and Codex open/resume shortcuts and `new_pi_tab`.

Each field uses , which deserializes from either a bare TOML string (`"ctrl+shift+w"`) or an array (`["ctrl+shift+w", "ctrl+w"]`). Up to  (5) combos per action are stored. Default bindings are platform-aware: macOS uses `cmd+`-prefixed combos where they do not collide with standard app shortcuts, with close-pane intentionally on `super+ctrl+w`, while other platforms use `ctrl+shift+`-prefixed equivalents. `new_pi_tab` is the exception: it defaults to `ctrl+alt+z` on every platform, because the tool it launches is not a macOS-native app action and `cmd+alt+z` is redo territory.

On macOS, config load also migrates stale legacy non-mac defaults when a saved keybindings block still looks like an older generated config, so pre-existing Linux-style defaults do not mask the platform-native shortcuts after install.

### Profiles

Named config profiles are stored separately from `config.toml` so switching profiles can atomically rewrite the active config without losing the saved variants.

 keeps a `BTreeMap<String, ScribeConfig>` plus the active profile name in `$XDG_CONFIG_HOME/scribe/profiles.toml` for stable installs or `$XDG_CONFIG_HOME/scribe-dev/profiles.toml` for the dev flavor. , , , and  back the CLI profile commands and the client command palette's profile switcher.

### Unicode Width

Unicode width is currently fixed to alacritty_terminal's default width tables, so East Asian ambiguous-width code points still use the narrow policy everywhere in Scribe.

Both the server and client terminal cores inherit width from alacritty_terminal's built-in `Handler::input` logic, and the renderer's ligature run matcher mirrors that same policy via . A user-selectable ambiguous-width mode is not implemented yet because it would require coordinated terminal-core and renderer changes.

### Workspaces

 holds a list of root directory paths scanned for projects and a badge color palette used to visually distinguish workspaces.

When a session leaves every configured root, the server clears its workspace name and the client removes the badge, leaving only terminal tabs. A nonempty published name selects its badge colour from the configured palette; region accents still style borders and active underlines.

### GitHub CI

[[crates/scribe-common/src/config.rs#GithubCiConfig]] holds the global GitHub Actions tracking opt-in.

`github_ci.enabled` defaults to `false`. Existing config files therefore keep GitHub integration disabled until the user explicitly enables it.

### Update

 controls auto-update behavior: `enabled` flag, `check_interval_secs` (default 86 400 s), and  (`Stable` or `Beta`).

The updater is intentionally disabled for `scribe-dev` installs so test builds cannot download and install the stable package over the main app.

## Errors

A single `thiserror`-derived enum covering all error conditions that cross crate boundaries or IPC channels.

 has variants for session/workspace lookup failures, PTY spawn failure, IPC and protocol errors, config parse errors, theme parse errors, serialization/deserialization failures (with `#[from]` for `rmp_serde`), and update check/install failures.

## Framing

Length-prefixed MessagePack framing over async streams, used for all IPC connections in the .

The wire format is a 4-byte big-endian `u32` length followed by an `rmp_serde` payload. The `MAX_MESSAGE_SIZE` constant caps messages at 256 MiB; the cap survives from the legacy per-cell `ScreenSnapshot` era and is now headroom for `RequestSnapshot` tooling — live reattach uses zstd-compressed ANSI replays that are typically orders of magnitude smaller.  and  are generic async functions that work with any `AsyncReadExt`/`AsyncWriteExt` + `Unpin` stream.

## Identity Types

UUID-based newtype IDs generated by the `define_id!` macro, ensuring type safety across IPC boundaries.

Three ID types are defined in : `SessionId` (display prefix `session-`), `WorkspaceId` (prefix `ws-`), and `WindowId` (prefix `win-`). Each implements `new()`, `as_uuid()`, `to_full_string()`, `Display` (8-char prefix), `FromStr` (parses full UUID string), `Default`, and the standard `Copy`/`Hash`/`Serialize`/`Deserialize` traits.

## Screen Snapshots

Serializable per-cell terminal screen state used inside the server and for `RequestSnapshot` tooling. Client reattach uses the denser  encoding instead.

 carries the full visible grid as a flat `Vec<ScreenCell>`, dimensions, cursor position and style, alternate-screen flag, the application's enabled DEC private modes as an `active_dec_modes: Vec<DecPrivateMode>` list so the reattach replay can restore them, and scrollback rows.  is a `Copy` enum (mouse reporting 1000/1002/1003, SGR/UTF-8 mouse 1006/1005, alternate scroll 1007, bracketed paste 2004, focus events 1004, app-cursor DECCKM, app-keypad DECPAM) whose  returns the DECSET escape that re-enables it on the receiving `Term`.  holds a character, foreground/background color, and cell attribute flags.  is a three-variant enum (`Named(u16)`, `Indexed(u8)`, `Rgb`) that uses `u16` for named colors to accommodate alacritty_terminal's extended named color indices above 255.  includes both wide-character placeholders and `WRAPLINE` state so the ANSI encoder can preserve logical soft-wrapped lines.  completes the rendering model.

## Session Replay

 is the unified primitive for transferring a terminal's state to a receiver that will rebuild a VTE `Term`.

Carries cols/rows/scrollback-rows, cursor fields, alt-screen flag, and a zstd-compressed ANSI byte stream produced by  and compressed by . Both server hot-reload handoff and server → client reattach use this same encoding — receivers decompress via  and feed the bytes through `vte::ansi::Processor::advance`. `snapshot_to_ansi` starts with RIS, selects the captured screen, then re-emits enabled DEC private modes (mouse reporting, SGR/UTF-8 mouse, alternate scroll, bracketed paste, focus events, app-cursor, app-keypad), so dirty and fresh receivers converge without synthetic scrollback.

### Control chars in cells replay as spaces

A cell can hold a control character — alacritty's `put_tab` stores the literal `'\t'` in the cell it started on for selection-copy fidelity — and [[crates/scribe-common/src/screen_replay.rs#write_snapshot_row]] emits a space for any such cell rather than the raw byte.

A replayed control char would be *executed* by the receiving parser: a tab re-advances the cursor to the next tab stop on top of the padding cells the row already carries, pushing the row past the right edge, so every tabbed row autowraps a spurious blank line under itself and the rows above scroll into history. This was visible in the field as `git status` output re-indenting and double-spacing on every tab switch or focus-driven re-attach. The advance the control char performed is already materialized in the cells that follow it, so a space reproduces the exact visual row; only the `'\t'` copy marker degrades. tmux and mosh serialize their grids the same way — redraw and state-sync streams carry printable cell content plus SGR, never stored control bytes.

The declared cols/rows/scrollback-rows describe the grid for the receiver to rebuild; they are not a size bound on the payload. [[crates/scribe-common/src/screen_replay.rs#decompress_session_replay|decompress_session_replay]] streams the frame and enforces [[crates/scribe-common/src/screen_replay.rs#MAX_REPLAY_INFLATED_BYTES|its own 64 MiB ceiling]] instead, because both carrying paths accept payloads from an untrusted peer. See [[server#Server#Handoff#Session Replay Encoding|Session Replay Encoding]] for the reasoning.

## Socket Paths

Platform-specific socket and lock file paths for all Scribe singleton processes, centralizing path conventions so every crate stays consistent.

| Platform | Base directory |
| --- | --- |
| Linux | `/run/user/{uid}/scribe/` for stable installs, `/run/user/{uid}/scribe-dev/` for `scribe-dev` |
| macOS | `~/Library/Application Support/Scribe/run/` for stable installs, `~/Library/Application Support/Scribe Dev/run/` for `scribe-dev` |
| Other Unix | `$TMPDIR/scribe-{uid}/` for stable installs, `$TMPDIR/scribe-dev-{uid}/` for `scribe-dev` |

Named sockets in the base directory are `server.sock`, `client.sock`, `settings.sock`, and `handoff.sock`. `client.lock` and `settings.lock` serialize singleton acquisition; the bound sockets own process lifetime. Flavor-specific base directories prevent stable and dev clients from handing focus to each other.

Restore children use [[crates/scribe-common/src/socket.rs#ClientFocusGeneration]] to bind one `client-focus-<16 lowercase hex>.sock` endpoint through [[crates/scribe-client/src/settings/singleton.rs#BoundFocusEndpoint]]. The full random generation stays in every frame while the short tag keeps Unix paths portable. The binder sets the runtime directory to 0700 and the socket to 0600.

[[crates/scribe-client/src/settings/singleton.rs#TerminalFocusCommand]], [[crates/scribe-client/src/settings/singleton.rs#FocusEndpointRequest]], and [[crates/scribe-client/src/settings/singleton.rs#FocusEndpointResult]] use newline-delimited JSON capped at 4096 bytes. Reads and writes time out after 100 ms. The old `{"cmd":"focus","anchor":...}` bytes remain unchanged.

[[crates/scribe-client/src/settings/singleton.rs#verify_focus_peer]] checks kernel UID and PID, executable path, install flavor, and plain-owner or `--restore-child` role. Announcement validation also requires the exact generation-derived path, 0600 socket mode, and matching publisher and endpoint PIDs.

[[crates/scribe-client/src/settings/singleton.rs#spawn_focus_endpoint_cleanup]] runs one detached scan of at most 64 strict-prefix sockets for at most 500 ms. It preserves live authenticated endpoints and indeterminate entries, removes only sockets that refuse a local connection, and never scans unrelated runtime files.

## Theme System

A theme engine providing 5 built-in and 187 community presets, plus a derivation algorithm that produces chrome (UI) colors from the terminal palette.

[[crates/scribe-common/src/theme.rs#Theme]] holds foreground, background, cursor,
selection, 16 ANSI colors, and derived
[[crates/scribe-common/src/theme.rs#ChromeColors]].
[[crates/scribe-common/src/theme.rs#ThemeColors]] is its construction input.
[[crates/scribe-common/src/theme.rs#resolve_preset]] resolves preset names
case-insensitively, while [[crates/scribe-common/src/theme.rs#hex_to_rgba]] and
[[crates/scribe-common/src/theme.rs#rgba_to_hex]] convert color representations.

RGBA-to-hex conversion rounds each clamped channel to the nearest byte, and the server reuses that same helper when it synthesizes fallback terminal colors from the configured theme.

### Built-in Presets

Five curated themes ship with Scribe: `minimal-dark` (default), `tokyo-night`, `catppuccin-mocha`, `dracula`, and `solarized-dark`.

Public builders in `theme.rs` return each curated `Theme`;
[[crates/scribe-common/src/theme.rs#resolve_preset]] also includes them in the
same lookup used for community presets. The default is `minimal-dark`.

### Community Presets

187 color schemes imported from the Tabby terminal emulator, accessible via case-insensitive kebab-case names.

The presets are a static slice of hex `ThemeSpec` values in
`theme_community_presets.rs`. Runtime resolution stays lazy except for the
settings window, which resolves each preset once when its window-local cache is
built or reloaded. [[crates/scribe-common/src/theme.rs#all_preset_names]] exposes
all 192 curated and community names.

### Chrome Color Derivation

[[crates/scribe-common/src/theme.rs#Theme#derive_chrome]] derives
[[crates/scribe-common/src/theme.rs#ChromeColors]] from terminal foreground,
background, and ANSI colors.

The derivation algorithm lightens the background by 6% for the tab bar, uses ANSI blue (index 4) as the accent, and applies alpha-reduced foreground tones for separators, dividers, scrollbar, and status bar text. Prompt bar colors are also derived as restrained first-row and second-row surfaces, plus muted text, first icon, and latest icon; `AppearanceConfig` can override those surfaces directly. This keeps the chrome visually coherent when a user switches themes or defines a custom palette.
