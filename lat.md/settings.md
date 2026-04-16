# Settings

The scribe-settings crate provides a webview-based configuration editor for terminal appearance, keybindings, colors, AI integrations, and workspace management.

## Window

The settings window in [[crates/scribe-settings/src/lib.rs]] uses an embedded webview (GTK on Linux, tao/wry on macOS) with inlined HTML/CSS/JS assets.

On Linux, the icon pixbuf is loaded from the hicolor theme and set directly on the window so that panels which match by WM_CLASS still display the correct icon.

The visible window title is flavor-aware: stable installs use `Scribe Settings`, while `scribe-dev` uses `devScribe Settings` so task bars distinguish dev windows from production ones.

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

Preset selection converts underscore-separated names to hyphen-separated and clears any custom theme if not "custom". Custom theme colours include foreground, background, cursor, cursor text, selection, selection text, and all 16 ANSI colours (normal 0-7 and bright 0-7). When switching to custom, colours are seeded from the current preset. Subsequent edits keep writing the inline `[theme]` section while `appearance.theme` stays `custom`, so the client must treat `[theme]` mutations as live theme changes rather than waiting for the preset name to change again.

The Colors page also exposes five prompt bar color overrides labeled First Row, Second Row, Text, First Icon, and Latest Icon, with reset-to-theme-default buttons. The settings page writes the second-row surface to `appearance.prompt_bar_second_row_bg` and still accepts legacy `appearance.prompt_bar_bg` values when loading older configs, so reopening Settings shows the saved value without reviving a generic prompt-bar background control. Debian package upgrades now migrate that old key on disk before relaunch, and when a legacy `prompt_bar_first_row_bg` is paired with it the installer remaps both overrides through the old mixed-row formulas so customized prompt bars keep their prior visual intent under the new exact-fill renderer. The prompt-bar swatches also resync live when the active theme changes or the custom theme editor is edited, unless an explicit override is present. These `Option<String>` fields on `AppearanceConfig` override the auto-derived `ChromeColors` values.

### Terminal Keys

Terminal page general section — scrollback lines, natural scrolling, and copy on select. AI integration settings moved to the AI page.

Status bar stat toggles remain on the Terminal page under the Status Bar section.

### AI Keys

AI page consolidates all AI integration settings including Prompt Bar, Scroll Pin, Preserve AI Scrollback, Indicator Height, and the AI Assistant States table.

The Prompt Bar section title includes a "Customize colors" crosslink that switches to the Colors page and scrolls to the Prompt Bar color overrides.

Clipboard cleanup remains persisted as `claude_copy_cleanup` for backward compatibility. `hide_codex_hook_logs` defaults to false and suppresses full Codex hook log blocks for the documented hook events (`SessionStart`, `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, and `Stop`), including bullet-prefixed `Running ... hook: ...` lines, non-`completed` trailers, nested hook output, and only the first trailing raw whitespace-only spacer line. Interactive Codex can repaint completion rows without newline terminators, so the filter hides only the visible hook portion and leaves the following cursor-move redraw bytes intact; if the removed hook prefix had already established the gray prompt background or other inherited SGR styling, the kept tail restores that active style state before replaying the remaining bytes. Synchronized-update trimming keeps prompt repaint tails in the same atomic block, splits legacy `Running ... hook` rows away from later repaint bytes, and drops reset-only newline tails so hidden hook rows do not become blank spacer rows, while still preserving ANSI-painted blank redraw lines for Codex startup chrome. `preserve_ai_scrollback` now trims repeated AI redraw clears back to the first preserved history size, so earlier shell output survives without duplicate Claude/Codex transcript frames stacking up in scrollback. The client no longer collapses blank rows after render because that heuristic could move legitimate Codex prompt/layout rows upward. `scroll_pin` now defaults to false so AI history keeps the normal contiguous scrollback unless the user explicitly opts into split-scroll. The filter still fails open if the block never closes.

`terminal.ai_tab_provider` is compatibility-only legacy state; AI tab shortcuts are configured through `new_claude_tab`, `new_claude_resume_tab`, `new_codex_tab`, and `new_codex_resume_tab`.

Shared indicator settings cover both Claude Code and Codex Code, even though the persisted key prefix remains `claude_states` for backward compatibility. Per-state configuration for processing, waiting_for_input, permission_prompt, and error. Each state has: tab indicator (bool), pane border (bool), colour (hex or ANSI index), pulse milliseconds (u32), and timeout seconds (f32, min 0.0). Both `IdlePrompt` and `WaitingForInput` AI states share the `waiting_for_input` config key. The old `idle_prompt` key is silently ignored if present in existing configs.

### Keybinding Keys

All keybinding actions accept a string or array of strings (combo list, max 5 per action).

Actions cover: pane splits, focus directions, workspace splits, workspace cycling, tab management (new, Claude Code new/resume, Codex new/resume, close, next, prev, select 1-9), clipboard, scrolling, command palette, find, zoom, settings, new window, and terminal shortcuts (word left/right, delete word, line start/end).

### Update Keys

Controls the auto-update behavior: `enabled` (bool), `check_interval` (integer hours, 1–168, stored internally as seconds), and `channel` (stable/beta) to select the release track.

### Notification Keys

Desktop notification settings: `enabled` (bool) and `condition` (when to fire).

`enabled` (default true) toggles notifications on or off. `condition` selects `when_unfocused` (default, only when the OS window lacks focus), `when_unfocused_or_background_tab` (also when the session is on a background tab in a focused window), or `always` (never suppress for focus reasons).

### Workspace Keys

Add/remove root directories and badge colour customization per index with reset-to-defaults.

## Singleton

The settings app uses the same singleton structure as the server: a lock file plus a Unix socket for focus handoff. It takes `settings.lock`, listens on `settings.sock`, and sends a `focus` command to an existing instance when one is already running.

That same socket also accepts a `quit` command from the client and server shutdown paths. The client sends it immediately for explicit `Quit Scribe`, and the server sends it after a short grace period once the last client disconnects, so the standalone settings window does not outlive the app while still tolerating fast reconnect handoffs. Socket-driven `quit` exits preserve the persisted `open` flag on both Linux and macOS so the next fresh Scribe launch restores settings only when the window had been open before app shutdown; native user closes still mark it closed.

## State Persistence

Window geometry and open state are saved to the active flavor's state root, using `$XDG_STATE_HOME/scribe/settings_state.toml` for stable installs and `$XDG_STATE_HOME/scribe-dev/settings_state.toml` for `scribe-dev`, via [[crates/scribe-settings/src/state.rs]].
