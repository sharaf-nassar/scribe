# Settings

The GPUI client owns a second-window settings surface while preserving Scribe's
TOML configuration and live-apply behavior.

## Window

The retired GTK/wry settings application is gone. `crates/scribe-client/src/settings` now opens one GPUI window inside the running client process, from `scribe-client --settings` or from an in-app entry point.

[[crates/scribe-client/src/settings/window.rs#open_settings_window]] is the only
place the window is created. It sets the app id `scribe-client` so panels that
match by WM_CLASS group it with the terminal window, titles it `Scribe
Settings` with `appears_transparent` so the custom 54px titlebar replaces the
system one, and passes the geometry described in
[[settings#State Persistence]] — including `window_min_size`, which the
compositor keeps enforcing during an interactive resize.

The page content, palette, navigation, and accessibility contract are described
in [[settings#GPUI Settings Window]]. This section covers only the window
shell: how it is decorated, resized, and moved.

### Client-side decorations

The window opts into `WindowDecorations::Client`, so the window manager draws no frame and the app must supply its own resize border, move region, and window buttons.

On X11 that clears the decorations flag in `_MOTIF_WM_HINTS`; on Wayland it
requests client xdg-decoration mode. Either way every WM-provided resize border
and corner disappears, which is why the resize and move contracts below exist
at all. The request is not binding: X11 without a running compositor falls back
to server decorations, so
[[crates/scribe-client/src/settings/window.rs#SettingsWindow#render_window_frame]]
reads `Window::window_decorations()` on every render and takes the
pass-through path — no inset, no gutter, no hit-testing — whenever the answer
is `Decorations::Server`.

The window background stays `WindowBackgroundAppearance::Opaque`. Resize does
not need transparency (both backends delegate the drag to the compositor), and
an opaque window is the safer default on an X11 session with no compositor, so
the gutter is painted as deliberate chrome instead of Zed-style drop shadow: a
`#0c0d0e` graphite matte one step below every interior surface, with the body
carrying a one-pixel `#4f4f51` seam against it. It reads as a window frame
rather than as stray padding.

### Resize contract

Under client decorations the app pads each untiled side by [[crates/scribe-client/src/settings/window.rs#RESIZE_GUTTER]] and treats that band as the window's resize border.

`render_window_frame` calls `Window::set_client_inset` with the same value so
the platform layer knows the visual frame is inset, then pads only the sides
the compositor reports as untiled — a tiled or maximized edge cannot be
dragged, so it gets no gutter.
[[crates/scribe-client/src/settings/window.rs#resize_edge]] maps a
window-relative press onto one of the eight `ResizeEdge` values, giving each
corner a square of `gutter * 1.5` so diagonal drags stay reachable only when
both adjoining sides are untiled, and returning `None` inside the content area.
A left press that resolves to an edge calls `Window::start_window_resize`,
which issues `_NET_WM_MOVERESIZE` on X11 and `xdg_toplevel.resize` on Wayland.

The press handler lives on the outer frame wrapper, above the settings body's
own left-press handler; that inner handler only clears keyboard-navigation
styling and never stops propagation, so the edge press is not swallowed.
[[crates/scribe-client/src/settings/window.rs#resize_cursor_overlay]] paints
last and inserts a window-sized `HitboxBehavior::Normal` hitbox purely so
`Window::set_cursor_style` has something hovered to attach the directional
cursor to; because it never blocks the mouse, it cannot eat a click. A
`resize_edge` change recorded on the view triggers the repaint that swaps the
cursor glyph.

### Move contract

The titlebar keeps its `WindowControlArea::Drag` declarations — that hit-test hook is what moves the window on Windows — and adds an explicit press/move pair, because both Linux backends implement `on_hit_test_window_control` as an empty body.

[[crates/scribe-client/src/settings/window.rs#SettingsWindow#render_titlebar]]
arms a `should_move` flag on left press, and the first left-button move while
it is armed clears it and calls `Window::start_window_move`. Mouse-up and
mouse-down-out disarm it. The press only arms the flag when `resize_edge`
returns `None`, so in the few pixels where the top corner squares reach into
the titlebar the resize grab wins and the move never competes with it.

The min/max/close buttons stop mouse-down propagation before it reaches the
titlebar's arming handler, so a click carrying a pixel of pointer jitter cannot
start a window move and consume the click.

## Config Application

Settings changes are applied in  as JSON messages with a key path and value.

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

AI integration settings moved to the AI page. The paste-confirmation toggle is keyed `terminal.paste_confirmation`, defaults OFF, and routes through  like the other terminal bool toggles. It gates a multi-line or control-character paste client-side — only when the focused app has not enabled bracketed paste — per , with no server round-trip.

The persist-environment toggle is keyed `terminal.env_persistence.enabled`, defaults OFF, and is gated by an OS-secret-store preflight on enable — see .

The Clipboard (OSC 52) subsection exposes the four policy keys defined by spec 010: `terminal.clipboard.read_mode` and `terminal.clipboard.write_mode` (each Deny/Allow/Prompt), `terminal.clipboard.max_write_bytes` (bytes, default 16,777,216, hard ceiling 536,870,912 = 512 MiB), and the FR-019 opt-in `terminal.clipboard.focus_gate_writes` (bool, default false). The keys live under the `[terminal.clipboard]` TOML sub-table (serde-renamed from the Rust field `clipboard_policy` on `TerminalConfig` because the legacy flattened `TerminalClipboardConfig` already owns the unrenamed `clipboard` identifier). The webview ⇄ TOML round-trip is handled by , which clamps `max_write_bytes` to the public ceiling `CLIPBOARD_MAX_WRITE_BYTES_CEILING` and routes `focus_gate_writes` straight onto `config.terminal.clipboard_policy.focus_gate_writes`. Saving any of the keys triggers the file watcher → `ConfigReloaded` round-trip described in  so live PTY readers refresh their per-session policy snapshot without a restart; the client-side focus-gate is read off the same `App::config` snapshot the watcher already refreshes (no dedicated IPC variant).

The terminal-images toggle is keyed `terminal.images.enabled`, is labelled
"Terminal images", defaults ON, and stores as a plain boolean under the
`[terminal.images]` TOML sub-table. It is the rollback control for spec 020: the
server applies it live on the `ConfigReloaded` round-trip, so turning it off
stops advertising, releases retained image state, and leaves the text pipeline
alone without a restart. The same key is the viewer-side gate: the client reads
it when it builds its `Hello`, so clearing the toggle also stops the client
announcing a renderer on its next connection — see
[[terminal-images#Terminal Images#Image Master Switch]].
The `Control` model carries a key and a label but no description field, so the
toggle ships label-only; the user-facing explanation of what the switch covers,
what it refuses, and what turning it off does lives in the README's "Terminal
images" configuration subsection, and the operator sequence lives in
[[terminal-images#Terminal Images#Image Master Switch#Rollback procedure]].

Status bar stat toggles remain on the Terminal page under the Status Bar section.

### Smart Selection Keys

Smart Selection settings live in their own Terminal page section and persist as one global `terminal.smart_selection` payload.

The settings page manages activation (`double_click` or `quad_click`), ordered regex rules, enabled state, precision, and per-rule actions. `terminal.smart_selection.reset` restores the built-in recognizers. The apply path in  deserializes the full payload and validates enabled Rust regexes before saving, so bad rules are not written to config.

The frontend rule editor in  supports add, duplicate, remove, reorder, enable/disable, regex validation, preview text, and action editing for Open File, Open URL, Run Command, Run Coprocess, Send Text, Run Command in Window, and Copy. Smart Selection remains global; there are no profile-specific rule sets.

### AI Keys

AI page consolidates all AI integration settings including Prompt Bar, Scroll Pin, Preserve AI Scrollback, Indicator Height, and the AI Assistant States table.

The Prompt Bar section title includes a "Customize colors" crosslink that switches to the Colors page and scrolls to the Prompt Bar color overrides.

Clipboard cleanup remains persisted as `claude_copy_cleanup` for backward compatibility. `preserve_ai_scrollback` now trims repeated AI redraw clears inside prompt/attention epochs, capturing the baseline after the first filtered redraw so real AI transcript history survives while duplicate repaint frames are still pruned. The client no longer collapses blank rows after render because that heuristic could move legitimate Codex prompt/layout rows upward. `scroll_pin` now defaults to false so AI history keeps the normal contiguous scrollback unless the user explicitly opts into split-scroll.

AI tab shortcuts are configured through provider-specific keys: `new_claude_tab`, `new_claude_resume_tab`, `new_codex_tab`, and `new_codex_resume_tab`.

AI tab working directory offers Active pane (`pane`, default), Project root (`project_root`, falling back through pane to home), and Home (`home`, sent as no cwd so the server uses its validated home fallback). The selected variant's fallback behavior appears directly under the settings row, and saving a new value affects the next fresh AI tab without restarting the client.

Context threshold settings are persisted under `terminal.ai_context_thresholds` and control the warn/danger band boundaries and their display colors. `warn` (default 70) and `danger` (default 90) are integer percentages. `ok_color`, `warn_color`, and `danger_color` are `#rrggbb` hex strings (defaults `#5fa05f`, `#d4a017`, `#c83030`). These thresholds color both the prompt-bar AI context % indicator and the tab inline suffix; see  for band classification logic.

Shared indicator settings cover Claude Code and Codex. The persisted key is now `ai_states`, while `claude_states` remains accepted as a config alias for backward compatibility. Per-state configuration for processing, waiting_for_input, permission_prompt, and error. Each state has: tab indicator (bool), pane border (bool), colour (hex or ANSI index), pulse milliseconds (u32), and timeout seconds (f32, min 0.0). Both `IdlePrompt` and `WaitingForInput` AI states share the `waiting_for_input` config key. The old `idle_prompt` key is silently ignored if present in existing configs.

### Keybinding Keys

All keybinding actions accept a string or array of strings (combo list, max 5 per action).

Actions cover: pane splits, focus directions, workspace splits, workspace cycling, tab management (new, Claude Code new/resume, Codex new/resume, close, next, prev, select 1-9), clipboard, scrolling, jump to previous prompt, jump to next prompt, jump to last failed command, command palette, find, zoom, settings, new window, and terminal shortcuts (word left/right, delete word, line start/end).

### Update Keys

Controls the auto-update behavior: `enabled` (bool), `check_interval` (integer hours, 1–168, stored internally as seconds), and `channel` (stable/beta) to select the release track.

The Updates page also exposes a "Check Now" action button that bypasses the periodic schedule entirely and works even when `enabled = false`. Clicking it sends a webview IPC of type `request_update_check`, which the host translates into a transient connection to `server.sock` carrying a `CheckForUpdates` message — see  for the server-side path. The result (`NoUpdate`, `UpdateAvailable { version, release_url }`, or `Failed { reason }`) is rendered inline as status text next to the button via the JS callback `updateCheckResult`. When the result is `UpdateAvailable`, the same broadcast that the periodic checker would emit also fires, so the regular client-side CTA appears alongside the in-settings status.

The settings binary's transient `server.sock` connection is implemented in  using synchronous std I/O plus the same length-prefixed msgpack framing as the rest of the protocol. Cross-thread delivery of the response back onto the GTK main loop uses `glib::timeout_add_local` polling a `std::sync::mpsc` channel; on macOS it goes through a new `TaoUserEvent::UpdateCheckResult` variant on the existing event-loop proxy. The active glib timeout source is tracked so the window-close path can cancel any in-flight poll before the webview is dropped.

#### Update Now Mode

After a `UpdateAvailable` result the same action button morphs in place to a green `Update Now`, and a module-level `pendingUpdate` flag routes subsequent clicks to install instead of re-running the check.

The button is the single source of truth for state, switched by  across four modes (`check`, `checking`, `update`, `installing`) that map to label + disabled + `is-primary` class. Confirmation uses a native `window.confirm` — the wry webview supports it and the codebase has no in-app modal primitive worth reusing. On confirm the JS dispatches a `trigger_update` IPC, the button flips to disabled `Installing…` (still green), and the status line acknowledges the install is in flight.

The host-side `trigger_update` branch in  dispatches to , which spawns a worker thread that calls  — a fire-and-forget `TriggerUpdate` frame on a fresh transient socket. The server accepts it via a sibling first-message arm to `CheckForUpdates` / `ListReleases` (see ) and drives the install through the same `UpdaterHandle::trigger()` channel the in-client overlay uses. Install progress is broadcast only to registered clients, so the in-client overlay still owns the live download/verify/install feedback and the restart-required prompt; the settings UI deliberately stays optimistic — `Installing…` until the user re-clicks `Check Now` or reopens settings.

If the server is unreachable when the click lands (daemon stopped, socket path missing), the worker thread logs a `WARN` and the button stays in `Installing…` indefinitely — there is no automatic timeout-back-to-`Update Now` path, since success is unobservable from the transient socket. Recovery requires the user to reopen settings and re-click `Check Now`.

The version text rendered after `Update available:` is an inline link (`.update-check-link`) that does not navigate the OS browser. Instead,  calls `.click()` on `.nav-item[data-tab="releases"]`, so the existing `initNavigation` handler swaps the active page and lazy-loads the release list. This keeps the user inside the settings window with full notes for every version rather than opening a tag-specific page in the browser.

### Notification Keys

Desktop notification settings cover enablement, focus suppression, and Linux-only timeout behavior.

`enabled` (default true) toggles notifications on or off. `condition` selects `when_unfocused` (default, only when the OS window lacks focus), `when_unfocused_or_background_tab` (also when the session is on a background tab in a focused window), or `always` (never suppress for focus reasons). On Linux, `timeout_mode` selects `system_default`, `custom`, or `never`, and `timeout_secs` stores the custom timeout in seconds when that mode is active. On macOS the settings page hides those config keys and instead exposes a button that opens the system Notifications pane so the user can switch this app to the persistent notification style.

### Workspace Keys

Workspace roots and indexed badge colours can be edited in place and applied without restarting Scribe.

The GPUI Workspaces page renders the current root list, a path input with Add and Browse actions, a Remove action for each row, and the existing badge-colour reset. Browse opens the platform directory chooser and adds its selection; cancellation is a no-op. Paths must be absolute or start with `~/`. Rejected text stays in the input with an inline error so it can be corrected instead of retyped.

Accepted additions and removals use the existing `workspaces.add_root` and `workspaces.remove_root` apply paths, which deduplicate and persist the TOML list. Each mutation refreshes the rendered list and requests a live server config reload without restarting it.

One shared colour editor renders for each configured `workspaces.badge_colors` entry. It accepts RGB as `#rrggbb` or `rrggbb`, then persists the canonical lowercase `#rrggbb` form. Invalid input and config stay unchanged while an inline error leaves the text editable.

Reset badge colors restores the eight default palette entries and releases any active colour editor safely before the controls rerender.

### Remote Keys

The "Remote" page controls feature 013's opt-in Tailscale remote-control listener via the `[remote]` TOML table (), off by default. `remote.enabled` is the "Allow remote control from my devices" toggle; `remote.port` is the advanced TCP port.

Both route through , which clamps the port to 1024–65535 — the same range the webview stepper enforces — so a hand-crafted IPC cannot persist an out-of-range value. Under the toggle sits a permanent plain-language UX-003 statement naming the signed-in Tailscale account; a passive "Tailscale not detected — remote access stays off" notice shows when the host reports Tailscale is absent (both are pushed in by the host, never fetched by the webview: the CSP forbids network calls from settings, so  resolves them over IPC with a `GetRemoteEnv` probe —  — and evaluates the page's `setRemoteEnv` bridge, failing closed to no account and "Tailscale not detected" on any error per FR-015). Saving triggers the file-watcher → `ConfigReloaded` round-trip, which starts, stops, or rebinds the listener live (); the server is never restarted. The separate feature-014 LAN opt-in is .

### Local Network Keys

Feature 014's "Local network" section drives the `[remote.lan]` table and the LAN trust stores. The `remote.lan.enabled` toggle and `remote.lan.port` clamp route through the same  as the tailnet keys.

The `[remote.lan]` schema is  (`enabled` off by default — a separate opt-in from the tailnet `[remote]` toggle, FR-012; `port` default 46062, clamped 1024–65535). Saving rides the same file-watcher → `ConfigReloaded` round-trip that starts, stops, or rebinds the LAN listener live (never a restart), effective only while on a trusted network ().

Beyond the two config keys the section is populated over IPC — the CSP forbids network calls from the webview, exactly as for the  panel.  and  resolve this device's own fingerprint plus current-network-addable state ( via `GetLanEnv`), the trusted-networks list and current-trust flag (, driving the active/dormant status line, UX-004), and the approved-devices list (), then push them into the page's `setLanEnv` / `setTrustedNetworks` / `setTrustedDevices` bridges.  routes the section's mutations — , , and  — to the local server's , refreshing the section after each. This machine's own fingerprint (word list + grouped hex) is shown so the user can compare it out of band against the approval prompt on another machine (optional MITM check, FR-006).

### Window Sharing Keys

Feature 015's "Window Sharing" section governs who may type into a shared window. Its three controls span both transports and persist onto , applied live over `ConfigReloaded` with no restart (FR-004/FR-005/FR-018).

`remote.sharing_mode` is a three-option segmented control (Single controller default / Shared view, single typist / Collaborative free-for-all → ); `remote.control_acquisition` is a two-option control (Free claim default / Request and grant → ) whose row is shown only in single-typist mode, toggled by `updateControlAcquisitionVisibility` via the same `remote-hidden` class as the passive Tailscale notice; `remote.participant_limit` is a `number-control` stepper where `0` (the displayed default) persists as `None` (unlimited). All three route through , which parses the enum strings and the limit and rides the same file-watcher → `ConfigReloaded` round-trip that reconciles live shares (); the server is never restarted. Serde defaults on the new fields mean an older config file loads with legacy single-controller behavior (FR-014).

## Releases

Browse historical Scribe releases from inside the settings window. The panel uses a single-content-area layout with a native `<select>` picker, Newer / Older nav buttons, and a "View on GitHub" link, driven by a `selectedReleaseVersion` JS state.

Release data is fetched over IPC from  via a one-shot Unix-socket request implemented in . The host-side IPC dispatcher in  routes `request_releases` (spawns a worker thread, calls `request_release_list`, then `evaluate_script("window.SCRIBE_ON_RELEASE_LIST(...)")` on the UI thread) and `open_external_url` (http(s)-scheme-validated via , dispatched to `xdg-open` / `open`).

### Layout

The page header is a flex row: title and subtitle on the left, "View on GitHub" anchor on the right. The panel below centers `[Older]` `[picker]` `[Newer]` as a single flex row.

Vertical rhythm: `.page-header-row` carries a 16px bottom margin into the panel, and `.releases-header` carries a matching 16px bottom margin into the release-notes article — so the nav row reads as vertically centered between the page subtitle above and the article below.

The content area below is a single `<article id="release-notes">` that receives the pre-sanitized HTML for the selected release. Both nav buttons start `disabled`; `updateNavBoundaries()` is the single source of truth that toggles the `disabled` attribute as the selection moves — Newer disables at index 0, Older at index `releases.length - 1` — so the picker and buttons stay in sync.

The native `<select>` carries one `<option>` per release labeled `vX.Y.Z — YYYY-MM-DD` with a `[PRE] ` prefix when `prerelease` is true. Native `<select>` cannot render arbitrary HTML, so pre-release affordances live in the option label text and as a `.pre-release-badge` span inside the rendered notes header. Links inside rendered notes and the `[data-external]` GitHub link are delegated to `open_external_url` so the OS browser opens them instead of the webview.

### Failure UX

The status banner under the content area renders distinct loading, stale, and failed sub-views, all backed by the Fresh / Stale / Failed transitions in .

Loading shows a non-blocking "Loading releases…" message (class `is-loading`). Stale renders the cached releases plus a "may be stale" indicator with the last refresh timestamp and reason (class `is-stale`) and a Refresh button that re-posts `request_releases`. Failed renders the plain-language `reason` from the payload (class `is-error`) and a Retry button that re-posts `request_releases`. The Refresh / Retry buttons reuse the `.releases-nav-btn` styling for visual consistency.

## Sidebar Footer

The settings sidebar footer displays the running Scribe version, sourced at build time from `env!("CARGO_PKG_VERSION")` and injected into the webview via  as `window.SCRIBE_BOOTSTRAP.version`.

The `settings.js` `DOMContentLoaded` handler reads that value and writes `Scribe v<version>` into `#sidebar-footer`; a missing or falsy value degrades to just `Scribe` so the footer never shows a broken interpolation. The injection runs as a pre-page-load script so the bootstrap object is already defined before any other JS on the page runs.

## Singleton

The settings app uses the same singleton structure as the server: a lock file plus a Unix socket for focus handoff. It takes `settings.lock`, listens on `settings.sock`, and sends a `focus` command to an existing instance when one is already running.

Singleton socket commands are one-line JSON payloads capped at 4 KiB before parsing, so a same-UID peer cannot force unbounded line allocation in the settings process. Focus commands may carry the launcher terminal rectangle; new settings processes receive the same anchor via `SCRIBE_SETTINGS_ANCHOR`.

That same socket also accepts a `quit` command from the client and server shutdown paths. The client sends it immediately for explicit `Quit Scribe`, and the server sends it after a short grace period once the last client disconnects, so the standalone settings window does not outlive the app while still tolerating fast reconnect handoffs. Socket-driven `quit` exits preserve the persisted `open` flag on both Linux and macOS so the next fresh Scribe launch restores settings only when the window had been open before app shutdown; native user closes still mark it closed.

## State Persistence

Window geometry and open state are saved to the active flavor's state root, using `$XDG_STATE_HOME/scribe/settings_state.toml` for stable installs and `$XDG_STATE_HOME/scribe-dev/settings_state.toml` for `scribe-dev`, via .

The GPUI settings launch opens at its 1040×720 layout minimum when the display
can hold it. Saved geometry is restored only when its size is between that
minimum and the primary display's visible work area; undersized or oversized
legacy physical-pixel geometry migrates to the compact centered composition.

## GPUI Settings Window

The GPUI rebuild reproduces the deleted `scribe-settings` webview app as a window in the client process, opened from a running terminal window or from `scribe-client --settings`.

### Native Precision presentation

The settings window uses a spacious Obsidian Amber native workspace so repeated configuration work scans quickly without changing the underlying feature set.

[[crates/scribe-client/src/settings/window.rs#SettingsColors#resolve]] fixes the
settings palette independently from the active terminal theme: `#161719`
canvas, `#1e1f20` navigation, `#272829` controls, `#4f4f51` strong seams,
`#efede8` text, `#979692` secondary text, and `#f5b83a` amber. Persistent
surfaces stay flat; 2–4px corners, one-pixel rules, and monospace technical
values distinguish controls without cards, shadows, or decorative chrome.

The compact 32px custom titlebar centers `Scribe Settings` between symmetric
144px chrome reservations, matching the native macOS traffic-light band while
keeping clear of those buttons and the three right-side actions. Below it, an
independently scrollable 314px sidebar groups the eleven real pages under
Terminal, Intelligence, Workflow, System, and Connectivity.
Non-focusable group labels do not enter keyboard traversal; each 44px page row
uses a normalized outline glyph and selected rows use a warm fill with a 4px
amber seam. Inset separators and first-group top air establish group rhythm.

A real AccessKit search input at the top of the content pane receives focus
with Ctrl+K and filters page names, summaries, section names, control labels,
and dotted keys. Matching pages remain navigable, and matching controls filter
inside the selected page. Its visual placeholder disappears while the empty
field is focused, while the AccessKit placeholder remains available to
assistive technology.

Content uses 46px gutters, an 18px bold page title, 14px summary and body
copy, explicit "Changes apply instantly" status, 18px section headings, and
54px rows. A
stable 438px right column aligns values: choices and read-only fields are 42px
high, steppers are 207×38px with 48px actions, switches are 52×30px, and
actions are 40px high. Switch tracks and warm-light knobs are fully rounded.
Read-only text, keybinding, and gated values use a muted fill, open bottom rule,
and explicit `READ ONLY` marker instead of an interactive control outline.
Shared colour fields use an interactive RGB editor with a live swatch.

At a numeric bound, the unavailable stepper button has no click handler or focus/tab stop and its accessible label names the reached limit. Pointer use clears keyboard-only focus styling and records the clicked target so later keyboard traversal resumes from true UI state.

### Accessibility semantics

[[crates/scribe-client/src/settings/window.rs#SettingsWindow]] gives settings navigation, controls, values, and feedback stable AccessKit roles and IDs.

The sidebar is a selected tab list, toggles and steppers report state/value, and one status node announces preflight and trust outcomes without stale duplicates.

The config-write and singleton logic stay 1:1 with the old app; only the HTML/CSS/JS surface is replaced with GPUI elements.

The webview delivery is gone; its feature set lives in . The config-apply path is ported verbatim as  (routing every `{key, value}` edit through ), so the  semantics — clamps, enum parsing, keybinding routing, theme seeding — are unchanged. The one-shot server-action client is ported as  and its release/env/remote siblings.

### Page model

The eleven settings pages are described in : each owns an ordered control list keyed by the dotted config key the apply path understands.

The pages are appearance, colors, AI, terminal, environment, keybindings, workspaces, updates, releases, notifications, and remote. The first ten mirror the old `settings.html` nav; environment splits the env-persistence opt-in out of terminal because enabling it needs a live server round-trip rather than a plain config write.

 renders that model generically — toggles flip, choices cycle, and numeric steppers increment through , committing immediately like the old live-apply webview. Current values are read back by . Shared colour controls edit inline, general free-text controls remain read-only, keybinding rows list every action's combos via , and Workspaces owns its dedicated path editor plus its dynamic badge-colour controls.

The settings window has a window-local keyboard traversal order: Tab/Down and Up move through the sidebar followed by actionable controls on the selected page; Enter/Space activates the focused page, toggle, choice, stepper, or action; Left/Right adjust toggles, choices, and steppers. A high-contrast border marks the current stop, and the independently scrollable content pane remains reachable through that ordered traversal. These handlers only live on the settings window, so terminal-window shortcuts are unaffected.

The root takes focus when the window opens, so Ctrl+K and traversal work before any click. Escape clears transient menus or an active search from every focus stop; titlebar window controls claim Enter/Space only, letting settings-wide keys continue to the root.

Action controls route through [[crates/scribe-client/src/settings/window.rs#SettingsWindow#run_action]], which is the single live entry point into [[crates/scribe-client/src/settings/server_action.rs]] — the update check, the release list, the keystore preflight, and the whole LAN trust surface below.

### Environment preflight

The environment page pairs the `terminal.env_persistence.enabled` toggle with a manual "Check keystore availability" action; both reach .

Turning the toggle ON is gated:  sends `EnvPreflight` first and commits the config edit only when the server answers `ok`, matching the webview-era rule that persistence is never enabled behind an unreachable keystore (). A failing probe leaves the config untouched and renders the structured `PreflightError` as plain language; turning the toggle OFF is an ungated plain write. The standalone action re-runs the same probe without touching the setting, so a locked keychain can be diagnosed and retried in place.

### Local network trust

The remote page leads with a runtime "Local network" section — the GPUI port of the webview's `setLanEnv` / `setTrustedNetworks` / `setTrustedDevices` bridges described in .

 resolves the whole section in one pass:  for this machine's own fingerprint and whether the current network is addable,  for the list plus the current-network trust flag (UX-004), and  for the approved-device list. It runs on the first visit to the page (the analog of the webview's load-time injection) and on the section's Refresh action.

The section's mutations are the three fire-and-forget frames, each followed by a refresh so the lists re-render from the server rather than from a local guess:  behind "Trust it",  behind each network row's Remove, and  behind each device row's Revoke. "Trust it" renders and enters keyboard traversal only when fresh LAN state says the current untrusted network is addable. Its pending request reports success only after refreshed trusted-network state confirms the outcome; failed or unconfirmed refreshes explain how to retry. Per-row buttons carry their record key in the action id (`action.remove_trusted_network:<id>`, `action.revoke_trusted_device:<hex>`) so they still route through the single `run_action` entry point. Because the section renders server replies rather than config keys it is built in the window, not listed in `page_controls`, and it is rendered above the page's config controls so the lists stay above the fold.

### In-app entry points

Three surfaces in the running terminal window open the settings window, and all three end at  — the same call the `--settings` launch makes.

 is that single handler. The `settings` keybinding reaches it through ; the palette's "Open Settings" row lowers onto the same  via ; and the status bar's far-right settings gear activates it through the `on_settings` handler wired in `render_status_bar`. Because GPUI is multi-window in one process, the window is opened in place rather than by spawning a second binary the way the winit client had to.

The handle the open returns is retained on the view, and that handle *is* the deduplication: a later request updates it, which fails once the window has been closed, and a live update activates the existing window instead of stacking a duplicate. The cross-process singleton below is deliberately not consulted from this path — its primary holds an exclusive `flock` for the settings window's whole lifetime, so acquiring it from the terminal window would park the live shell on a lock rather than answer a keystroke.

### Singleton and launch

 absorbs the `settings.lock`/`settings.sock` singleton unchanged;  splits the path resolution out so a second `--settings` launch hands focus (with the launcher anchor) to the running window instead of opening a duplicate.

Window geometry persists via . During side-by-side development the old GTK app stays the sole live-config writer; this window is pointed at a separate dev config via the `SCRIBE_CONFIG_DIR` override that  already honours, so the two never race on `config.toml`.
