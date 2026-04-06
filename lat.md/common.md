# Common

Shared types and utilities used by every Scribe crate: IPC [[protocol]], identity types, error definitions, screen snapshots, configuration, theme system, and socket path conventions.

## AI State

Tracks Claude Code and Codex Code process lifecycle by parsing OSC 1337 escape sequences into typed Rust values.

[[crates/scribe-common/src/ai_state.rs#AiState]] is a five-variant enum (`IdlePrompt`, `Processing`, `WaitingForInput`, `PermissionPrompt`, `Error`) shared by both integrations. [[crates/scribe-common/src/ai_state.rs#AiProvider]] distinguishes `ClaudeCode` from `CodexCode`, and [[crates/scribe-common/src/ai_state.rs#AiProcessState]] carries that provider alongside optional metadata fields (`tool`, `agent`, `model`, `context`, `conversation_id`). The [[pty]] crate's `MetadataParser` produces `AiProcessState` values; the [[server]] broadcasts them to connected clients and preserves backward compatibility by defaulting missing providers to Claude on deserialize.

## Configuration

Unified TOML config for server and client, deserialized from the active install flavor's XDG config root into [[crates/scribe-common/src/config.rs#ScribeConfig]].

Stable installs read `~/.config/scribe/config.toml`, while `scribe-dev` reads `~/.config/scribe-dev/config.toml`. `ScribeConfig` is the top-level struct with six sub-sections: `appearance`, `theme`, `terminal`, `keybindings`, `workspaces`, and `update`. The [[crates/scribe-common/src/config.rs#load_config]] function reads the file and returns `ScribeConfig::default()` if absent.

### Appearance

Font family, size, weight, ligatures, line padding, cursor shape, opacity, theme name, scrollbar, focus border, tab bar dimensions, status bar height, and content padding are all in [[crates/scribe-common/src/config.rs#AppearanceConfig]].

[[crates/scribe-common/src/config.rs#ContentPadding]] provides per-side padding (top/right/bottom/left) with a `clamped()` helper that enforces the `0.0..=50.0` range. [[crates/scribe-common/src/config.rs#CursorShape]] is a three-variant enum (`Block`, `Beam`, `Underline`).

### AI State Colors

Per-state visual config for Claude Code indicators lives in [[crates/scribe-common/src/config.rs#ClaudeStatesConfig]], which holds one [[crates/scribe-common/src/config.rs#AiStateEntry]] per `AiState` variant.

Each `AiStateEntry` carries a color, pulse animation duration (`pulse_ms`), auto-clear timeout (`timeout_secs`), and booleans for tab indicator and pane border. [[crates/scribe-common/src/config.rs#AiColor]] is a polymorphic color type that accepts either a fixed `#rrggbb` hex string or an `"ansi:N"` palette index (0–15) that adapts to the active theme at render time.

### Terminal

[[crates/scribe-common/src/config.rs#TerminalConfig]] groups scrollback, copy-on-select, AI toggles, indicator height, shell integration, status bar stats, prompt bar, and scroll pin settings.

`scroll_pin` (bool, default `false`) enables split-scroll in AI panes, but only while the pane is in the normal screen buffer; alternate-screen TUIs fall back to the regular live view. `preserve_ai_scrollback` (bool, default `true`) downgrades AI-session `CSI 3 J` scrollback clears into `CSI 2 J` full-screen clears so redraws keep their clean viewport without wiping prior history.

Prompt bar fields: `prompt_bar` (bool), `prompt_bar_font_size` (f32, 8–32, default 14), and `prompt_bar_position` ([[crates/scribe-common/src/config.rs#PromptBarPosition]]: Top or Bottom).

[[crates/scribe-common/src/config.rs#StatusBarStatsConfig]] independently toggles CPU, memory, GPU, and network display. [[crates/scribe-common/src/config.rs#ShellIntegrationConfig]] wraps a single `enabled` flag for shell prompt marks. [[crates/scribe-common/src/config.rs#TerminalConfig#ai_provider_enabled]] maps an [[crates/scribe-common/src/ai_state.rs#AiProvider]] to the matching integration toggle.

### Keybindings

[[crates/scribe-common/src/config.rs#KeybindingsConfig]] exposes 50+ configurable actions across pane navigation, workspace splits, tab management, clipboard, scrolling, zoom, and terminal word-motion shortcuts.

Each field uses [[crates/scribe-common/src/config.rs#KeyComboList]], which deserializes from either a bare TOML string (`"ctrl+shift+w"`) or an array (`["ctrl+shift+w", "ctrl+w"]`). Up to [[crates/scribe-common/src/config.rs#MAX_BINDINGS]] (5) combos per action are stored. Default bindings are platform-aware: macOS uses `cmd+`-prefixed combos where they do not collide with standard app shortcuts, with close-pane intentionally on `super+ctrl+w`, while other platforms use `ctrl+shift+`-prefixed equivalents.

On macOS, config load also migrates stale legacy non-mac defaults when a saved keybindings block still looks like an older generated config, so pre-existing Linux-style defaults do not mask the platform-native shortcuts after install.

### Profiles

Named config profiles are stored separately from `config.toml` so switching profiles can atomically rewrite the active config without losing the saved variants.

[[crates/scribe-common/src/profiles.rs#ProfileStore]] keeps a `BTreeMap<String, ScribeConfig>` plus the active profile name in `$XDG_CONFIG_HOME/scribe/profiles.toml` for stable installs or `$XDG_CONFIG_HOME/scribe-dev/profiles.toml` for the dev flavor. [[crates/scribe-common/src/profiles.rs#load_profile_store]], [[crates/scribe-common/src/profiles.rs#switch_profile]], [[crates/scribe-common/src/profiles.rs#export_profile]], and [[crates/scribe-common/src/profiles.rs#import_profile]] back the CLI profile commands and the client command palette's profile switcher.

### Unicode Width

Unicode width is currently fixed to alacritty_terminal's default width tables, so East Asian ambiguous-width code points still use the narrow policy everywhere in Scribe.

Both the server and client terminal cores inherit width from alacritty_terminal's built-in `Handler::input` logic, and the renderer's ligature run matcher mirrors that same policy via [[crates/scribe-renderer/src/lib.rs#RunAccum#matches]]. A user-selectable ambiguous-width mode is not implemented yet because it would require coordinated terminal-core and renderer changes.

### Workspaces

[[crates/scribe-common/src/config.rs#WorkspacesConfig]] holds a list of root directory paths scanned for projects and a badge color palette used to visually distinguish workspaces.

### Update

[[crates/scribe-common/src/config.rs#UpdateConfig]] controls auto-update behavior: `enabled` flag, `check_interval_secs` (default 86 400 s), and [[crates/scribe-common/src/config.rs#UpdateChannel]] (`Stable` or `Beta`).

The updater is intentionally disabled for `scribe-dev` installs so test builds cannot download and install the stable package over the main app.

[[crates/scribe-common/src/config.rs#ThemeConfig]] is an optional inline theme definition used when `appearance.theme == "custom"` to supply foreground, background, cursor, selection, and 16 ANSI colors directly in the config file.

## Errors

A single `thiserror`-derived enum covering all error conditions that cross crate boundaries or IPC channels.

[[crates/scribe-common/src/error.rs#ScribeError]] has variants for session/workspace lookup failures, PTY spawn failure, IPC and protocol errors, config parse errors, theme parse errors, serialization/deserialization failures (with `#[from]` for `rmp_serde`), and update check/install failures.

## Framing

Length-prefixed MessagePack framing over async streams, used for all IPC connections in the [[protocol]].

The wire format is a 4-byte big-endian `u32` length followed by an `rmp_serde` payload. The `MAX_MESSAGE_SIZE` constant caps messages at 256 MiB to accommodate large `ScreenSnapshot` batches sent during session reattach. [[crates/scribe-common/src/framing.rs#read_message]] and [[crates/scribe-common/src/framing.rs#write_message]] are generic async functions that work with any `AsyncReadExt`/`AsyncWriteExt` + `Unpin` stream.

## Identity Types

UUID-based newtype IDs generated by the `define_id!` macro, ensuring type safety across IPC boundaries.

Three ID types are defined in [[crates/scribe-common/src/ids.rs]]: `SessionId` (display prefix `session-`), `WorkspaceId` (prefix `ws-`), and `WindowId` (prefix `win-`). Each implements `new()`, `as_uuid()`, `to_full_string()`, `Display` (8-char prefix), `FromStr` (parses full UUID string), `Default`, and the standard `Copy`/`Hash`/`Serialize`/`Deserialize` traits.

## Screen Snapshots

Serializable terminal screen state for IPC transport, used when reconnecting a client to a running session.

[[crates/scribe-common/src/screen.rs#ScreenSnapshot]] carries the full visible grid as a flat `Vec<ScreenCell>`, dimensions, cursor position and style, alternate-screen flag, and scrollback rows. [[crates/scribe-common/src/screen.rs#ScreenCell]] holds a character, foreground/background color, and cell attribute flags. [[crates/scribe-common/src/screen.rs#ScreenColor]] is a three-variant enum (`Named(u16)`, `Indexed(u8)`, `Rgb`) that uses `u16` for named colors to accommodate alacritty_terminal's extended named color indices above 255. [[crates/scribe-common/src/screen.rs#CellFlags]] includes both wide-character placeholders and `WRAPLINE` state so reconnect snapshot replay can preserve logical soft-wrapped lines. [[crates/scribe-common/src/screen.rs#CursorStyle]] completes the rendering model.

## Socket Paths

Platform-specific socket and lock file paths for all Scribe singleton processes, centralizing path conventions so every crate stays consistent.

| Platform | Base directory |
| --- | --- |
| Linux | `/run/user/{uid}/scribe/` for stable installs, `/run/user/{uid}/scribe-dev/` for `scribe-dev` |
| macOS | `~/Library/Application Support/Scribe/run/` for stable installs, `~/Library/Application Support/Scribe Dev/run/` for `scribe-dev` |
| Other Unix | `$TMPDIR/scribe-{uid}/` for stable installs, `$TMPDIR/scribe-dev-{uid}/` for `scribe-dev` |

Named sockets in the base directory: `server.sock`, `settings.sock`, and `handoff.sock`, with lock files for the long-lived singleton processes. macOS uses a stable Application Support path so Finder-launched clients and `launchctl`-started background services resolve the same socket location. Public API: [[crates/scribe-common/src/socket.rs#server_socket_path]], [[crates/scribe-common/src/socket.rs#settings_socket_path]], [[crates/scribe-common/src/socket.rs#settings_lock_path]], [[crates/scribe-common/src/socket.rs#server_lock_path]], [[crates/scribe-common/src/socket.rs#handoff_socket_path]], and [[crates/scribe-common/src/socket.rs#current_uid]].

## Theme System

A theme engine providing 5 built-in and 187 community presets, plus a derivation algorithm that produces chrome (UI) colors from the terminal palette.

[[crates/scribe-common/src/theme.rs#Theme]] is the resolved theme struct with RGBA arrays for foreground, background, cursor, selection, 16 ANSI colors, and a [[crates/scribe-common/src/theme.rs#ChromeColors]] sub-struct. [[crates/scribe-common/src/theme.rs#ThemeColors]] is the input parameter bag passed to [[crates/scribe-common/src/theme.rs#resolve_preset]] produces a `Theme` from a preset name (case-insensitive) by checking curated presets first and falling back to the community set. [[crates/scribe-common/src/theme.rs#hex_to_rgba]] and [[crates/scribe-common/src/theme.rs#rgba_to_hex]] convert between `#rrggbb` strings and `[f32; 4]` RGBA arrays.

### Built-in Presets

Five curated themes ship with Scribe: `minimal-dark` (default), `tokyo-night`, `catppuccin-mocha`, `dracula`, and `solarized-dark`.

These are defined in `theme.rs` as `pub(crate)` builder functions returning `Theme` and are listed in the `CURATED_NAMES` constant. The default is `minimal-dark`, a dark neutral palette with zinc grays and vivid ANSI accents.

### Community Presets

187 color schemes imported from the Tabby terminal emulator, accessible via case-insensitive kebab-case names.

The presets are defined in `theme_community_presets.rs` as a static slice of `ThemeSpec` structs containing hex color strings. They are looked up at runtime and never eagerly constructed. The full name list is exposed via [[crates/scribe-common/src/theme.rs#all_preset_names]].

### Chrome Color Derivation

[[crates/scribe-common/src/theme.rs#ChromeColors]] is derived automatically from the terminal foreground, background, and ANSI palette — no manual chrome configuration is required.

The derivation algorithm lightens the background by 6% for the tab bar, uses ANSI blue (index 4) as the accent, and applies alpha-reduced foreground tones for separators, dividers, scrollbar, and status bar text. Prompt bar colors are also derived: background from lightened terminal background, first-row from darkened background, text at 50% foreground alpha, first icon from ANSI yellow (index 3), latest icon from ANSI blue (index 4). All prompt bar colors can be overridden via `AppearanceConfig` fields. This ensures chrome colors remain visually coherent when a user switches themes or defines a custom palette.
