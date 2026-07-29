# Settings

The scribe-settings crate provides a webview-based configuration editor for terminal appearance, keybindings, colors, AI integrations, and workspace management.

## Window

The settings window in [[crates/scribe-settings/src/lib.rs]] uses an embedded webview (GTK on Linux, tao/wry on macOS) with inlined HTML/CSS/JS assets.

The inlined document carries a restrictive Content Security Policy: no default loads, no network connects, and only the inline script/style blocks produced by the asset embedder are allowed.

On Linux, the icon pixbuf is loaded from the hicolor theme and set directly on the window so that panels which match by WM_CLASS still display the correct icon.

The visible window title is flavor-aware: stable installs use `Scribe Settings`, while `scribe-dev` uses `devScribe Settings` so task bars distinguish dev windows from production ones.

On launch, five pieces of state are injected into the webview: the host platform, the current config, keybinding defaults (for reset-to-default UI), all theme preset colours, and a list of available monospace fonts from fontdb.

When Settings is opened from a client terminal, the client sends a [[crates/scribe-common/src/settings_window.rs#SettingsWindowAnchor|settings window anchor]] so the window can be centered over that terminal instead of replaying stale off-screen coordinates.

On X11, both the fresh-launch and singleton-refocus paths route the raise through [[crates/scribe-settings/src/lib.rs#raise_linux_window_above_launcher]], which fetches `gdk_x11_get_server_time` and calls `present_with_time` so the window manager accepts the cross-process raise instead of demoting it to a "demand attention" hint that would leave Settings behind the launcher terminal. Wayland falls back to bare `present` because position requests are no-ops there anyway.

### Font Discovery

The `list_monospace_fonts` function queries fontdb for all system monospace font families, returning a deduplicated sorted list.

### Platform Differences

Linux uses GTK3 with glib socket/signal watchers; macOS uses tao EventLoop with background threads.

The settings frontend formats shortcut badges from the injected platform flag. On macOS, both `cmd` and `super` modifiers render as the `⌘` glyph, and the platform is injected before config so reopened settings windows do not fall back to Linux-style `Super` labels. Search indexes the raw shortcut plus modifier aliases, so queries like `command`, `cmd`, or `super` still find `⌘`-rendered keybindings. Bare `cmd+w` is handled at the tao window layer and closes the settings window through the same path as a native close request. The Notifications page also uses the platform flag to show Linux-only timeout controls or, on macOS, a shortcut button that opens the system Notifications pane.

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

Terminal page general section — scrollback lines, natural scrolling, copy on select, enhanced keyboard protocol, the paste-confirmation toggle, the persist-environment toggle, and the OSC 52 Clipboard policy subsection.

AI integration settings moved to the AI page. The paste-confirmation toggle is keyed `terminal.paste_confirmation`, defaults OFF, and routes through [[crates/scribe-settings/src/apply.rs#apply_terminal_behavior_key]] like the other terminal bool toggles. It gates a multi-line or control-character paste client-side — only when the focused app has not enabled bracketed paste — per [[client#Dialogs#Paste Confirmation Dialog]], with no server round-trip.

The persist-environment toggle is keyed `terminal.env_persistence.enabled`, defaults OFF, and is gated by an OS-secret-store preflight on enable — see [[server#Env Persistence]].

The Clipboard (OSC 52) subsection exposes the four policy keys defined by spec 010: `terminal.clipboard.read_mode` and `terminal.clipboard.write_mode` (each Deny/Allow/Prompt), `terminal.clipboard.max_write_bytes` (bytes, default 16,777,216, hard ceiling 536,870,912 = 512 MiB), and the FR-019 opt-in `terminal.clipboard.focus_gate_writes` (bool, default false). The keys live under the `[terminal.clipboard]` TOML sub-table (serde-renamed from the Rust field `clipboard_policy` on `TerminalConfig` because the legacy flattened `TerminalClipboardConfig` already owns the unrenamed `clipboard` identifier). The webview ⇄ TOML round-trip is handled by [[crates/scribe-settings/src/apply.rs#apply_terminal_clipboard_key]], which clamps `max_write_bytes` to the public ceiling `CLIPBOARD_MAX_WRITE_BYTES_CEILING` and routes `focus_gate_writes` straight onto `config.terminal.clipboard_policy.focus_gate_writes`. Saving any of the keys triggers the file watcher → `ConfigReloaded` round-trip described in [[lat.md/server#Server#Sessions#Clipboard Gating]] so live PTY readers refresh their per-session policy snapshot without a restart; the client-side focus-gate is read off the same `App::config` snapshot the watcher already refreshes (no dedicated IPC variant).

Status bar stat toggles remain on the Terminal page under the Status Bar section.

### Smart Selection Keys

Smart Selection settings live in their own Terminal page section and persist as one global `terminal.smart_selection` payload.

The settings page manages activation (`double_click` or `quad_click`), ordered regex rules, enabled state, precision, and per-rule actions. `terminal.smart_selection.reset` restores the built-in recognizers. The apply path in [[crates/scribe-settings/src/apply.rs#apply_terminal_smart_selection_key]] deserializes the full payload and validates enabled Rust regexes before saving, so bad rules are not written to config.

The frontend rule editor in [[crates/scribe-settings/src/assets/settings.js]] supports add, duplicate, remove, reorder, enable/disable, regex validation, preview text, and action editing for Open File, Open URL, Run Command, Run Coprocess, Send Text, Run Command in Window, and Copy. Smart Selection remains global; there are no profile-specific rule sets.

### AI Keys

AI page consolidates all AI integration settings including Prompt Bar, Scroll Pin, Preserve AI Scrollback, Indicator Height, and the AI Assistant States table.

The Prompt Bar section title includes a "Customize colors" crosslink that switches to the Colors page and scrolls to the Prompt Bar color overrides.

Clipboard cleanup remains persisted as `claude_copy_cleanup` for backward compatibility. `preserve_ai_scrollback` now trims repeated AI redraw clears inside prompt/attention epochs, capturing the baseline after the first filtered redraw so real AI transcript history survives while duplicate repaint frames are still pruned. The client no longer collapses blank rows after render because that heuristic could move legitimate Codex prompt/layout rows upward. `scroll_pin` now defaults to false so AI history keeps the normal contiguous scrollback unless the user explicitly opts into split-scroll.

AI tab shortcuts are configured through provider-specific keys: `new_claude_tab`, `new_claude_resume_tab`, `new_codex_tab`, and `new_codex_resume_tab`.

Context threshold settings are persisted under `terminal.ai_context_thresholds` and control the warn/danger band boundaries and their display colors. `warn` (default 70) and `danger` (default 90) are integer percentages. `ok_color`, `warn_color`, and `danger_color` are `#rrggbb` hex strings (defaults `#5fa05f`, `#d4a017`, `#c83030`). These thresholds color both the prompt-bar AI context % indicator and the tab inline suffix; see [[common#Configuration#AI Context Thresholds]] for band classification logic.

Shared indicator settings cover Claude Code and Codex. The persisted key is now `ai_states`, while `claude_states` remains accepted as a config alias for backward compatibility. Per-state configuration for processing, waiting_for_input, permission_prompt, and error. Each state has: tab indicator (bool), pane border (bool), colour (hex or ANSI index), pulse milliseconds (u32), and timeout seconds (f32, min 0.0). Both `IdlePrompt` and `WaitingForInput` AI states share the `waiting_for_input` config key. The old `idle_prompt` key is silently ignored if present in existing configs.

### Keybinding Keys

All keybinding actions accept a string or array of strings (combo list, max 5 per action).

Actions cover: pane splits, focus directions, workspace splits, workspace cycling, tab management (new, Claude Code new/resume, Codex new/resume, close, next, prev, select 1-9), clipboard, scrolling, jump to previous prompt, jump to next prompt, jump to last failed command, command palette, find, zoom, settings, new window, and terminal shortcuts (word left/right, delete word, line start/end).

### Update Keys

Controls the auto-update behavior: `enabled` (bool), `check_interval` (integer hours, 1–168, stored internally as seconds), and `channel` (stable/beta) to select the release track.

The Updates page also exposes a "Check Now" action button that bypasses the periodic schedule entirely and works even when `enabled = false`. Clicking it sends a webview IPC of type `request_update_check`, which the host translates into a transient connection to `server.sock` carrying a `CheckForUpdates` message — see [[server#Server#Updater#Manual Check]] for the server-side path. The result (`NoUpdate`, `UpdateAvailable { version, release_url }`, or `Failed { reason }`) is rendered inline as status text next to the button via the JS callback `updateCheckResult`. When the result is `UpdateAvailable`, the same broadcast that the periodic checker would emit also fires, so the regular client-side CTA appears alongside the in-settings status.

The settings binary's transient `server.sock` connection is implemented in [[crates/scribe-settings/src/server_action.rs#request_update_check]] using synchronous std I/O plus the same length-prefixed msgpack framing as the rest of the protocol. Cross-thread delivery of the response back onto the GTK main loop uses `glib::timeout_add_local` polling a `std::sync::mpsc` channel; on macOS it goes through a new `TaoUserEvent::UpdateCheckResult` variant on the existing event-loop proxy. The active glib timeout source is tracked so the window-close path can cancel any in-flight poll before the webview is dropped.

#### Update Now Mode

After a `UpdateAvailable` result the same action button morphs in place to a green `Update Now`, and a module-level `pendingUpdate` flag routes subsequent clicks to install instead of re-running the check.

The button is the single source of truth for state, switched by [[crates/scribe-settings/src/assets/settings.js#setUpdateCheckButtonMode]] across four modes (`check`, `checking`, `update`, `installing`) that map to label + disabled + `is-primary` class. Confirmation uses a native `window.confirm` — the wry webview supports it and the codebase has no in-app modal primitive worth reusing. On confirm the JS dispatches a `trigger_update` IPC, the button flips to disabled `Installing…` (still green), and the status line acknowledges the install is in flight.

The host-side `trigger_update` branch in [[crates/scribe-settings/src/lib.rs#handle_settings_ipc_request]] dispatches to [[crates/scribe-settings/src/lib.rs#dispatch_trigger_update]], which spawns a worker thread that calls [[crates/scribe-settings/src/server_action.rs#request_trigger_update]] — a fire-and-forget `TriggerUpdate` frame on a fresh transient socket. The server accepts it via a sibling first-message arm to `CheckForUpdates` / `ListReleases` (see [[server#Server#Updater#Manual Check]]) and drives the install through the same `UpdaterHandle::trigger()` channel the in-client overlay uses. Install progress is broadcast only to registered clients, so the in-client overlay still owns the live download/verify/install feedback and the restart-required prompt; the settings UI deliberately stays optimistic — `Installing…` until the user re-clicks `Check Now` or reopens settings.

If the server is unreachable when the click lands (daemon stopped, socket path missing), the worker thread logs a `WARN` and the button stays in `Installing…` indefinitely — there is no automatic timeout-back-to-`Update Now` path, since success is unobservable from the transient socket. Recovery requires the user to reopen settings and re-click `Check Now`.

The version text rendered after `Update available:` is an inline link (`.update-check-link`) that does not navigate the OS browser. Instead, [[crates/scribe-settings/src/assets/settings.js#activateReleasesTab]] calls `.click()` on `.nav-item[data-tab="releases"]`, so the existing `initNavigation` handler swaps the active page and lazy-loads the release list. This keeps the user inside the settings window with full notes for every version rather than opening a tag-specific page in the browser.

### Notification Keys

Desktop notification settings cover enablement, focus suppression, and Linux-only timeout behavior.

`enabled` (default true) toggles notifications on or off. `condition` selects `when_unfocused` (default, only when the OS window lacks focus), `when_unfocused_or_background_tab` (also when the session is on a background tab in a focused window), or `always` (never suppress for focus reasons). On Linux, `timeout_mode` selects `system_default`, `custom`, or `never`, and `timeout_secs` stores the custom timeout in seconds when that mode is active. On macOS the settings page hides those config keys and instead exposes a button that opens the system Notifications pane so the user can switch this app to the persistent notification style.

### Workspace Keys

Add/remove root directories and badge colour customization per index with reset-to-defaults.

The workspace add row in [[crates/scribe-settings/src/assets/settings.js#initWorkspaces]] accepts absolute paths or `~/` roots, updates the displayed list immediately, and sends `workspaces.add_root`. Submitting an empty row asks the host to open a native directory chooser, then the selected path is injected back into the same add flow. The apply path in [[crates/scribe-settings/src/apply.rs#apply_workspace_key]] trims, deduplicates, and persists accepted roots.

### Remote Keys

The "Remote" page controls feature 013's opt-in Tailscale remote-control listener via the `[remote]` TOML table ([[crates/scribe-common/src/config.rs#RemoteConfig]]), off by default. `remote.enabled` is the "Allow remote control from my devices" toggle; `remote.port` is the advanced TCP port.

Both route through [[crates/scribe-settings/src/apply.rs#apply_remote_key]], which clamps the port to 1024–65535 — the same range the webview stepper enforces — so a hand-crafted IPC cannot persist an out-of-range value. Under the toggle sits a permanent plain-language UX-003 statement naming the signed-in Tailscale account; a passive "Tailscale not detected — remote access stays off" notice shows when the host reports Tailscale is absent (both are pushed in by the host, never fetched by the webview: the CSP forbids network calls from settings, so [[crates/scribe-settings/src/lib.rs#inject_remote_env]] resolves them over IPC with a `GetRemoteEnv` probe — [[crates/scribe-settings/src/server_action.rs#request_remote_env]] — and evaluates the page's `setRemoteEnv` bridge, failing closed to no account and "Tailscale not detected" on any error per FR-015). Saving triggers the file-watcher → `ConfigReloaded` round-trip, which starts, stops, or rebinds the listener live ([[server#Remote Control#Listener Lifecycle]]); the server is never restarted. The separate feature-014 LAN opt-in is [[settings#Config Application#Local Network Keys]].

### Local Network Keys

Feature 014's "Local network" section drives the `[remote.lan]` table and the LAN trust stores. The `remote.lan.enabled` toggle and `remote.lan.port` clamp route through the same [[crates/scribe-settings/src/apply.rs#apply_remote_key]] as the tailnet keys.

The `[remote.lan]` schema is [[crates/scribe-common/src/config.rs#LanRemoteConfig]] (`enabled` off by default — a separate opt-in from the tailnet `[remote]` toggle, FR-012; `port` default 46062, clamped 1024–65535). Saving rides the same file-watcher → `ConfigReloaded` round-trip that starts, stops, or rebinds the LAN listener live (never a restart), effective only while on a trusted network ([[server#Remote Control#LAN Accept and Approval]]).

Beyond the two config keys the section is populated over IPC — the CSP forbids network calls from the webview, exactly as for the [[settings#Config Application#Remote Keys]] panel. [[crates/scribe-settings/src/lib.rs#inject_lan_state]] and [[crates/scribe-settings/src/lib.rs#refresh_lan_state]] resolve this device's own fingerprint plus current-network-addable state ([[crates/scribe-settings/src/server_action.rs#request_lan_env]] via `GetLanEnv`), the trusted-networks list and current-trust flag ([[crates/scribe-settings/src/server_action.rs#request_trusted_networks]], driving the active/dormant status line, UX-004), and the approved-devices list ([[crates/scribe-settings/src/server_action.rs#request_trusted_devices]]), then push them into the page's `setLanEnv` / `setTrustedNetworks` / `setTrustedDevices` bridges. [[crates/scribe-settings/src/lib.rs#handle_lan_ipc_action]] routes the section's mutations — [[crates/scribe-settings/src/server_action.rs#request_add_current_network]], [[crates/scribe-settings/src/server_action.rs#request_remove_trusted_network]], and [[crates/scribe-settings/src/server_action.rs#request_revoke_trusted_device]] — to the local server's [[server#Remote Control#LAN Trust Management|trust handlers]], refreshing the section after each. This machine's own fingerprint (word list + grouped hex) is shown so the user can compare it out of band against the approval prompt on another machine (optional MITM check, FR-006).

### Window Sharing Keys

Feature 015's "Window Sharing" section governs who may type into a shared window. Its three controls span both transports and persist onto [[crates/scribe-common/src/config.rs#RemoteConfig]], applied live over `ConfigReloaded` with no restart (FR-004/FR-005/FR-018).

`remote.sharing_mode` is a three-option segmented control (Single controller default / Shared view, single typist / Collaborative free-for-all → [[crates/scribe-common/src/config.rs#SharingMode]]); `remote.control_acquisition` is a two-option control (Free claim default / Request and grant → [[crates/scribe-common/src/config.rs#ControlAcquisition]]) whose row is shown only in single-typist mode, toggled by `updateControlAcquisitionVisibility` via the same `remote-hidden` class as the passive Tailscale notice; `remote.participant_limit` is a `number-control` stepper where `0` (the displayed default) persists as `None` (unlimited). All three route through [[crates/scribe-settings/src/apply.rs#apply_remote_key]], which parses the enum strings and the limit and rides the same file-watcher → `ConfigReloaded` round-trip that reconciles live shares ([[server#Remote Control#Listener Lifecycle]]); the server is never restarted. Serde defaults on the new fields mean an older config file loads with legacy single-controller behavior (FR-014).

## Releases

Browse historical Scribe releases from inside the settings window. The panel uses a single-content-area layout with a native `<select>` picker, Newer / Older nav buttons, and a "View on GitHub" link, driven by a `selectedReleaseVersion` JS state.

Release data is fetched over IPC from [[server#Releases#Release Catalog]] via a one-shot Unix-socket request implemented in [[crates/scribe-settings/src/server_action.rs#request_release_list]]. The host-side IPC dispatcher in [[crates/scribe-settings/src/lib.rs]] routes `request_releases` (spawns a worker thread, calls `request_release_list`, then `evaluate_script("window.SCRIBE_ON_RELEASE_LIST(...)")` on the UI thread) and `open_external_url` (http(s)-scheme-validated via [[crates/scribe-settings/src/lib.rs#dispatch_open_external_url]], dispatched to `xdg-open` / `open`).

### Layout

The page header is a flex row: title and subtitle on the left, "View on GitHub" anchor on the right. The panel below centers `[Older]` `[picker]` `[Newer]` as a single flex row.

Vertical rhythm: `.page-header-row` carries a 16px bottom margin into the panel, and `.releases-header` carries a matching 16px bottom margin into the release-notes article — so the nav row reads as vertically centered between the page subtitle above and the article below.

The content area below is a single `<article id="release-notes">` that receives the pre-sanitized HTML for the selected release. Both nav buttons start `disabled`; `updateNavBoundaries()` is the single source of truth that toggles the `disabled` attribute as the selection moves — Newer disables at index 0, Older at index `releases.length - 1` — so the picker and buttons stay in sync.

The native `<select>` carries one `<option>` per release labeled `vX.Y.Z — YYYY-MM-DD` with a `[PRE] ` prefix when `prerelease` is true. Native `<select>` cannot render arbitrary HTML, so pre-release affordances live in the option label text and as a `.pre-release-badge` span inside the rendered notes header. Links inside rendered notes and the `[data-external]` GitHub link are delegated to `open_external_url` so the OS browser opens them instead of the webview.

### Failure UX

The status banner under the content area renders distinct loading, stale, and failed sub-views, all backed by the Fresh / Stale / Failed transitions in [[server#Releases#Release Catalog]].

Loading shows a non-blocking "Loading releases…" message (class `is-loading`). Stale renders the cached releases plus a "may be stale" indicator with the last refresh timestamp and reason (class `is-stale`) and a Refresh button that re-posts `request_releases`. Failed renders the plain-language `reason` from the payload (class `is-error`) and a Retry button that re-posts `request_releases`. The Refresh / Retry buttons reuse the `.releases-nav-btn` styling for visual consistency.

## Sidebar Footer

The settings sidebar footer displays the running Scribe version, sourced at build time from `env!("CARGO_PKG_VERSION")` and injected into the webview via [[crates/scribe-settings/src/lib.rs#bootstrap_script]] as `window.SCRIBE_BOOTSTRAP.version`.

The `settings.js` `DOMContentLoaded` handler reads that value and writes `Scribe v<version>` into `#sidebar-footer`; a missing or falsy value degrades to just `Scribe` so the footer never shows a broken interpolation. The injection runs as a pre-page-load script so the bootstrap object is already defined before any other JS on the page runs.

## Singleton

The settings app uses the same singleton structure as the server: a lock file plus a Unix socket for focus handoff. It takes `settings.lock`, listens on `settings.sock`, and sends a `focus` command to an existing instance when one is already running.

Singleton socket commands are one-line JSON payloads capped at 4 KiB before parsing, so a same-UID peer cannot force unbounded line allocation in the settings process. Focus commands may carry the launcher terminal rectangle; new settings processes receive the same anchor via `SCRIBE_SETTINGS_ANCHOR`.

That same socket also accepts a `quit` command from the client and server shutdown paths. The client sends it immediately for explicit `Quit Scribe`, and the server sends it after a short grace period once the last client disconnects, so the standalone settings window does not outlive the app while still tolerating fast reconnect handoffs. Socket-driven `quit` exits preserve the persisted `open` flag on both Linux and macOS so the next fresh Scribe launch restores settings only when the window had been open before app shutdown; native user closes still mark it closed.

## State Persistence

Window geometry and open state are saved to the active flavor's state root, using `$XDG_STATE_HOME/scribe/settings_state.toml` for stable installs and `$XDG_STATE_HOME/scribe-dev/settings_state.toml` for `scribe-dev`, via [[crates/scribe-settings/src/state.rs]].

On GTK/X11, saved settings geometry is restored only when it intersects a currently connected monitor work area. Explicit open/focus requests with an anchor override saved position and clamp the settings window to the anchor monitor work area.

## GPUI Settings Window

The GPUI rebuild reproduces the deleted `scribe-settings` webview app as a window in the client process, opened from a running terminal window or from `scribe-client --settings`.

The config-write and singleton logic stay 1:1 with the old app; only the HTML/CSS/JS surface is replaced with GPUI elements.

The webview delivery is gone; its feature set lives in [[crates/scribe-client/src/settings/mod.rs]]. The config-apply path is ported verbatim as [[crates/scribe-client/src/settings/apply.rs#apply_settings_change]] (routing every `{key, value}` edit through [[crates/scribe-client/src/settings/apply.rs#apply_config_key]]), so the [[settings#Config Application]] semantics — clamps, enum parsing, keybinding routing, theme seeding — are unchanged. The one-shot server-action client is ported as [[crates/scribe-client/src/settings/server_action.rs#request_update_check]] and its release/env/remote siblings.

### Page model

The eleven settings pages are described in [[crates/scribe-client/src/settings/model.rs#page_controls]]: each owns an ordered control list keyed by the dotted config key the apply path understands.

The pages are appearance, colors, AI, terminal, environment, keybindings, workspaces, updates, releases, notifications, and remote. The first ten mirror the old `settings.html` nav; environment splits the env-persistence opt-in out of terminal because enabling it needs a live server round-trip rather than a plain config write.

[[crates/scribe-client/src/settings/window.rs#SettingsWindow]] renders that model generically — toggles flip, choices cycle, and numeric steppers increment through [[crates/scribe-client/src/settings/apply.rs#apply_settings_change]], committing immediately like the old live-apply webview. Current values are read back by [[crates/scribe-client/src/settings/values.rs#current_value]]. Color and free-text controls render their current value read-only, and keybinding rows list every action's combos via [[crates/scribe-client/src/settings/values.rs#keybinding_combos]]; inline hex/text/path entry is a tracked follow-on.

The settings window has a window-local keyboard traversal order: Tab/Down and Up move through the sidebar followed by actionable controls on the selected page; Enter/Space activates the focused page, toggle, choice, stepper, or action; Left/Right adjust toggles, choices, and steppers. A high-contrast border marks the current stop, and the independently scrollable content pane remains reachable through that ordered traversal. These handlers only live on the settings window, so terminal-window shortcuts are unaffected.

Action controls route through [[crates/scribe-client/src/settings/window.rs#SettingsWindow#run_action]], which is the single live entry point into [[crates/scribe-client/src/settings/server_action.rs]] — the update check, the release list, the keystore preflight, and the whole LAN trust surface below.

### Environment preflight

The environment page pairs the `terminal.env_persistence.enabled` toggle with a manual "Check keystore availability" action; both reach [[crates/scribe-client/src/settings/server_action.rs#request_env_preflight]].

Turning the toggle ON is gated: [[crates/scribe-client/src/settings/window.rs#SettingsWindow#enable_env_persistence]] sends `EnvPreflight` first and commits the config edit only when the server answers `ok`, matching the webview-era rule that persistence is never enabled behind an unreachable keystore ([[server#Env Persistence]]). A failing probe leaves the config untouched and renders the structured `PreflightError` as plain language; turning the toggle OFF is an ungated plain write. The standalone action re-runs the same probe without touching the setting, so a locked keychain can be diagnosed and retried in place.

### Local network trust

The remote page leads with a runtime "Local network" section — the GPUI port of the webview's `setLanEnv` / `setTrustedNetworks` / `setTrustedDevices` bridges described in [[settings#Config Application#Local Network Keys]].

[[crates/scribe-client/src/settings/window.rs#SettingsWindow#refresh_trust]] resolves the whole section in one pass: [[crates/scribe-client/src/settings/server_action.rs#request_lan_env]] for this machine's own fingerprint and whether the current network is addable, [[crates/scribe-client/src/settings/server_action.rs#request_trusted_networks]] for the list plus the current-network trust flag (UX-004), and [[crates/scribe-client/src/settings/server_action.rs#request_trusted_devices]] for the approved-device list. It runs on the first visit to the page (the analog of the webview's load-time injection) and on the section's Refresh action.

The section's mutations are the three fire-and-forget frames, each followed by a refresh so the lists re-render from the server rather than from a local guess: [[crates/scribe-client/src/settings/server_action.rs#request_add_current_network]] behind "Trust it", [[crates/scribe-client/src/settings/server_action.rs#request_remove_trusted_network]] behind each network row's Remove, and [[crates/scribe-client/src/settings/server_action.rs#request_revoke_trusted_device]] behind each device row's Revoke. Per-row buttons carry their record key in the action id (`action.remove_trusted_network:<id>`, `action.revoke_trusted_device:<hex>`) so they still route through the single `run_action` entry point. Because the section renders server replies rather than config keys it is built in the window, not listed in `page_controls`, and it is rendered above the page's config controls so the lists stay above the fold.

### In-app entry points

Three surfaces in the running terminal window open the settings window, and all three end at [[crates/scribe-client/src/settings/window.rs#open_settings_window]] — the same call the `--settings` launch makes.

[[crates/scribe-client/src/main.rs#TerminalView#open_or_focus_settings]] is that single handler. The `settings` keybinding reaches it through [[crates/scribe-client/src/main.rs#TerminalView#dispatch_key_action]]; the palette's "Open Settings" row lowers onto the same [[crates/scribe-client/src/keybindings.rs#KeyAction]] via [[crates/scribe-client/src/main.rs#key_action_for_automation]]; and the titlebar gear's `TitlebarEvent::OpenSettings` is subscribed in [[crates/scribe-client/src/main.rs#TerminalView#build_titlebar]]. Because GPUI is multi-window in one process, the window is opened in place rather than by spawning a second binary the way the winit client had to.

The handle the open returns is retained on the view, and that handle *is* the deduplication: a later request updates it, which fails once the window has been closed, and a live update activates the existing window instead of stacking a duplicate. The cross-process singleton below is deliberately not consulted from this path — its primary holds an exclusive `flock` for the settings window's whole lifetime, so acquiring it from the terminal window would park the live shell on a lock rather than answer a keystroke.

### Singleton and launch

[[crates/scribe-client/src/settings/singleton.rs#acquire]] absorbs the `settings.lock`/`settings.sock` singleton unchanged; [[crates/scribe-client/src/settings/singleton.rs#acquire_at]] splits the path resolution out so a second `--settings` launch hands focus (with the launcher anchor) to the running window instead of opening a duplicate.

Window geometry persists via [[crates/scribe-client/src/settings/state.rs]]. During side-by-side development the old GTK app stays the sole live-config writer; this window is pointed at a separate dev config via the `SCRIBE_CONFIG_DIR` override that [[crates/scribe-common/src/config.rs#load_config]] already honours, so the two never race on `config.toml`.
