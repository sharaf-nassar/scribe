# Settings

The scribe-settings crate provides a webview-based configuration editor for terminal appearance, keybindings, colors, AI integrations, and workspace management.

## Window

The settings window in [[crates/scribe-settings/src/lib.rs]] uses an embedded webview (GTK on Linux, tao/wry on macOS) with inlined HTML/CSS/JS assets.

On launch, five pieces of state are injected into the webview: the host platform, the current config, keybinding defaults (for reset-to-default UI), all theme preset colours, and a list of available monospace fonts from fontdb.

### Font Discovery

The `list_monospace_fonts` function queries fontdb for all system monospace font families, returning a deduplicated sorted list.

### Platform Differences

Linux uses GTK3 with glib socket/signal watchers; macOS uses tao EventLoop with background threads.

The settings frontend formats shortcut badges from the injected platform flag. On macOS, both `cmd` and `super` modifiers render as the `⌘` glyph, and the platform is injected before config so reopened settings windows do not fall back to Linux-style `Super` labels. Search indexes the raw shortcut plus modifier aliases, so queries like `command`, `cmd`, or `super` still find `⌘`-rendered keybindings. Bare `cmd+w` is handled at the tao window layer and closes the settings window through the same path as a native close request.

## Config Application

Settings changes are applied in [[crates/scribe-settings/src/apply.rs#apply_settings_change]] as JSON messages with a key path and value.

The function loads the current config, applies the change, and saves to disk. The client's file watcher detects the change and triggers a `ConfigChanged` event.

### Appearance Keys

Controls font, cursor, opacity, scrollbar, tab bar, status bar, content padding, and focus border settings.

Font family, font size (f32), font weight (u16, 100-900), bold weight, ligatures (bool), line padding, cursor shape (Block/Beam/Underline), cursor blink, opacity (0.0-1.0), scrollbar width (2.0-20.0), tab bar padding (0.0-20.0), tab width (8-50), status bar height (8.0-48.0), tab height (16.0-60.0), content padding per side (0.0-50.0), focus border colour (hex or empty for None), and focus border width (1.0-10.0).

### Colors Keys

Colors page (formerly Theme) — preset selection and custom theme colours with full ANSI color names and descriptions.

Preset selection converts underscore-separated names to hyphen-separated and clears any custom theme if not "custom". Custom theme colours include foreground, background, cursor, cursor text, selection, selection text, and all 16 ANSI colours (normal 0-7 and bright 0-7). When switching to custom, colours are seeded from the current preset.

### Terminal Keys

Terminal page general section — scrollback lines, natural scrolling, and copy on select. AI integration settings moved to the AI page.

Status bar stat toggles remain on the Terminal page under the Status Bar section.

### AI Keys

AI page consolidates all AI integration settings: Claude Code Integration, Codex Code Integration, AI Tab Provider, Clipboard Cleanup, Hide Codex Hook Logs, Prompt Bar, Indicator Height, and the AI Assistant States table.

Clipboard cleanup remains persisted as `claude_copy_cleanup` for backward compatibility. `hide_codex_hook_logs` defaults to false and suppresses full Codex hook log blocks for the documented hook events (`SessionStart`, `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, and `Stop`), including bullet-prefixed `Running ... hook: ...` lines, non-`completed` trailers, nested hook output, and only the first trailing blank spacer line, while failing open if the block never closes. The AI tab provider is persisted separately so the existing `new_claude_tab` and `new_claude_resume_tab` keybinding keys stay backward compatible while switching between Claude Code and Codex behavior.

Shared indicator settings cover both Claude Code and Codex Code, even though the persisted key prefix remains `claude_states` for backward compatibility. Per-state configuration for processing, waiting_for_input, permission_prompt, and error. Each state has: tab indicator (bool), pane border (bool), colour (hex or ANSI index), pulse milliseconds (u32), and timeout seconds (f32, min 0.0). Both `IdlePrompt` and `WaitingForInput` AI states share the `waiting_for_input` config key. The old `idle_prompt` key is silently ignored if present in existing configs.

### Keybinding Keys

All keybinding actions accept a string or array of strings (combo list, max 5 per action).

Actions cover: pane splits, focus directions, workspace splits, workspace cycling, tab management (new, provider-selected AI tab, provider-selected AI resume tab, close, next, prev, select 1-9), clipboard, scrolling, command palette, find, zoom, settings, new window, and terminal shortcuts (word left/right, delete word, line start/end). The AI Tab Provider setting that controls which provider `new_claude_tab` uses is configured on the AI page, not here — the Keybindings page shows a cross-link note pointing to it.

### Update Keys

Controls the auto-update behavior: `enabled` (bool), `check_interval` (integer hours, 1–168, stored internally as seconds), and `channel` (stable/beta) to select the release track.

### Workspace Keys

Add/remove root directories and badge colour customization per index with reset-to-defaults.

## Singleton

The settings app uses the same singleton structure as the server: a lock file plus a Unix socket for focus handoff. It takes `settings.lock`, listens on `settings.sock`, and sends a `focus` command to an existing instance when one is already running.

## State Persistence

Window geometry and open state are saved to the active flavor's state root, using `$XDG_STATE_HOME/scribe/settings_state.toml` for stable installs and `$XDG_STATE_HOME/scribe-dev/settings_state.toml` for `scribe-dev`, via [[crates/scribe-settings/src/state.rs]].
