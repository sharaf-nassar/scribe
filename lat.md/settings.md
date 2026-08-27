# Settings

The GPUI client owns a second-window settings surface while preserving Scribe's
TOML configuration and live-apply behavior.

## Window

The retired GTK/wry settings application is gone. `crates/scribe-client/src/settings` now opens one GPUI window inside the running client process, from `scribe-client --settings` or from an in-app entry point.

[[crates/scribe-client/src/settings/window.rs#open_settings_window]] is the only
place the window is created. It sets the app id `scribe-client` so panels that
match by WM_CLASS group it with the terminal window, titles it `Scribe
Settings` with `appears_transparent` so the custom 38px titlebar replaces the
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
`#0b0c0e` matte one step below the interior ground, with the body carrying a
one-pixel white-alpha hairline seam against it. It reads as a window frame
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

Controls font, cursor, opacity, scrollbar, tab bar, status bar, and focus border settings.

Font family, font size (f32), font weight (u16, 100-900), bold weight, ligatures (bool), line padding, cursor shape (Block/Beam/Underline), cursor blink, opacity (0.0-1.0), scrollbar width (2.0-20.0), tab bar padding (0.0-20.0), status bar height (8.0-48.0), tab height (16.0-60.0), focus border colour (hex or empty for None), and focus border width (1.0-10.0).

Tab width is deliberately absent: `TAB_WIDTH` is flex basis and drag-fallback
geometry, not a saved setting. Legacy `appearance.tab_width` is ignored on load
and omitted by the next Settings save.

### Colors Keys

Colors page (formerly Theme) — preset selection and custom theme colours with full ANSI color names and descriptions.

Preset selection converts underscore-separated names to hyphen-separated and clears any custom theme if not "custom". Custom theme colours include foreground, background, cursor, cursor text, selection, selection text, and all 16 ANSI colours (normal 0-7 and bright 0-7). When switching to custom, colours are seeded from the current preset. Subsequent edits keep writing the inline `[theme]` section while `appearance.theme` stays `custom`, so the client must treat `[theme]` mutations as live theme changes rather than waiting for the preset name to change again.

The Colors page also exposes five prompt bar color overrides labeled First Row, Second Row, Text, First Icon, and Latest Icon, with reset-to-theme-default buttons. The settings page writes the second-row surface to `appearance.prompt_bar_second_row_bg` and still accepts legacy `appearance.prompt_bar_bg` values when loading older configs, so reopening Settings shows the saved value without reviving a generic prompt-bar background control. Debian package upgrades now migrate that old key on disk before relaunch, and when a legacy `prompt_bar_first_row_bg` is paired with it the installer remaps both overrides through the old mixed-row formulas so customized prompt bars keep their prior visual intent under the new exact-fill renderer. The settings window resolves the active `Theme` once at construction and after each config reload; per-frame prompt-bar swatches map from that cache, so preset and custom edits resync without repeated theme resolution. These `Option<String>` fields on `AppearanceConfig` override the auto-derived `ChromeColors` values.

### Terminal Keys

Terminal page general section — scrollback lines, natural scrolling, copy on select, focus follows mouse, enhanced keyboard protocol, paste confirmation, environment persistence, and the OSC 52 Clipboard policy subsection.

The "Focus follows mouse" toggle writes the flat
`terminal.focus_follows_mouse` boolean through
[[crates/scribe-client/src/settings/apply.rs#apply_config_key]]. It defaults OFF,
reads back through the settings value model, and applies live through the
running client's existing config watcher.

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

[[crates/scribe-client/src/settings/window.rs#SettingsWindow#render_smart_selection_panel]] uses progressive disclosure: activation first, then a scrollable tab list beside the selected rule editor. Rules can be added, duplicated, removed, reordered, enabled, named, assigned precision, and tested at a chosen sample-text cursor.

Each selected rule expands its ordered context-menu actions. Kind, parameter, and legacy/interpolated mode are editable for Open File, Open URL, Run Command, Run Coprocess, Send Text, Run Command in Window, and Copy; actions can be added, duplicated, removed, or reordered.

Every durable edit sends the whole `terminal.smart_selection` payload through the existing apply path, which validates enabled Rust regexes before saving. Invalid regexes stay in the field with inline recovery text and can be disabled without discarding the draft; valid changes live-reload already-open panes. Keyboard focus scrolls offscreen rule rows into view. Smart Selection remains global.

### AI Keys

AI page consolidates shared AI integration settings including Prompt Bar, Scroll Pin, Preserve AI Scrollback, Indicator Height, and the AI Assistant States table.

The Prompt Bar section title includes a "Customize colors" crosslink that switches to the Colors page and scrolls to the Prompt Bar color overrides.

Clipboard cleanup remains persisted as `claude_copy_cleanup` for backward compatibility. `preserve_ai_scrollback` now trims repeated AI redraw clears inside prompt/attention epochs, capturing the baseline after the first filtered redraw so real AI transcript history survives while duplicate repaint frames are still pruned. The client no longer collapses blank rows after render because that heuristic could move legitimate Codex prompt/layout rows upward. `scroll_pin` now defaults to false so AI history keeps the normal contiguous scrollback unless the user explicitly opts into split-scroll.

AI tab shortcuts are configured through provider-specific keys: `new_claude_tab`, `new_claude_resume_tab`, `new_codex_tab`, and `new_codex_resume_tab`. Pi keeps the existing `new_pi_tab` action and has no resume action.

The config model accepts `terminal.pi_integration`, defaulting to `true` like the Claude Code and Codex integration toggles. [[crates/scribe-common/src/config.rs#TerminalConfig#ai_provider_enabled|ai_provider_enabled]] gates Pi independently while continuing to reject the synthetic `System` provider. The AI page exposes the same key as a keyboard-reachable Pi integration row. A false-to-true commit invokes [[crates/scribe-client/src/hook_setup.rs#repair_pi_extension_if_enabled|repair_pi_extension_if_enabled]] once and reports whether setup succeeded; packaged startup runs that repair too when the key is enabled. Disabling takes effect live but leaves the global extension file installed, and repaired extension code is loaded only by new Pi processes. See [[test#Test Harness#Pi Provider Compatibility#Installation, repair, and rollback]] and [[test#Test Harness#Pi Provider Compatibility#End-to-end Pi recipes]].

AI tab working directory offers Active pane (`pane`, default), Project root (`project_root`, falling back through pane to home), and Home (`home`, sent as no cwd so the server uses its validated home fallback). The selected variant's fallback behavior appears directly under the settings row, and saving a new value affects the next fresh AI tab without restarting the client.

Context threshold settings are persisted under `terminal.ai_context_thresholds` and control the warn/danger band boundaries and their display colors. `warn` (default 70) and `danger` (default 90) are integer percentages. `ok_color`, `warn_color`, and `danger_color` are `#rrggbb` hex strings (defaults `#5fa05f`, `#d4a017`, `#c83030`). These thresholds color both the prompt-bar AI context % indicator and the tab inline suffix; see  for band classification logic.

Shared indicator settings cover Claude Code, Codex, and Pi. The persisted key is now `ai_states`, while `claude_states` remains accepted as a config alias for backward compatibility. Per-state configuration for processing, waiting_for_input, permission_prompt, and error. Each state has: tab indicator (bool), pane border (bool), colour (hex or ANSI index), pulse milliseconds (u32), and timeout seconds (f32, min 0.0). Both `IdlePrompt` and `WaitingForInput` AI states share the `waiting_for_input` config key. The old `idle_prompt` key is silently ignored if present in existing configs.

### Keybinding Keys

All keybinding actions accept a string or array of strings (combo list, max 5 per action).

Actions cover: pane splits, focus directions, workspace splits, workspace cycling, tab management (new, Claude Code new/resume, Codex new/resume, Pi, close, next, prev, select 1-9), clipboard, scrolling, jump to previous prompt, jump to next prompt, jump to last failed command, command palette, find, zoom, settings, new window, and terminal shortcuts (word left/right, delete word, line start/end).

`new_pi_tab` is listed and rebound beside the AI rows. Pi is a first-class provider, while the shortcut uses the [[client#Client#GPUI Client Spike#Tab Strip And Key Dispatch#A tool tab binds to its tool|negotiated launch path]] and deliberately has no resume partner. [[crates/scribe-client/src/settings/model.rs#keybinding_actions]] remains the single list the page renders from.

The settings window writes these keys itself — see [[settings#Settings#GPUI Settings Window#Shortcut capture]] — so a rebind is a keystroke rather than a config-file edit. A captured combo replaces the action's whole list; alternates beyond the first stay a `config.toml` feature.

### Update Keys

The Updates page exposes `update.enabled` and `update.check_interval_hours` for automatic update behavior. `update.channel` remains supported in `config.toml` for updater compatibility, but the settings window no longer renders a Channel control.

The old Releases page is folded into Updates. "Check for updates" remains beside the automatic-update controls and still bypasses the periodic schedule when `update.enabled = false`.

Opening Updates lazily requests `ListReleases`; the worker also converts every sanitized body into typed GPUI blocks before returning to the UI thread. The bottom viewer uses its own 360px scroll region; a centered title sits between equal arrow slots, and an arrow exists only when that direction has another release.

`github_ci.enabled` appears on the Terminal page under Status bar as "GitHub CI run status". Its existing reader and apply path persist it immediately, then `ConfigReloaded` updates [[crates/scribe-server/src/github_ci.rs#github_ci_enabled]] without restarting Scribe. Enabling it only makes later qualifying local pushes eligible for CI tracking; saving performs no `gh` authentication probe or GitHub request, so idle traffic stays zero.

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

Release history shares the Updates page instead of owning a sidebar page. [[crates/scribe-client/src/settings/window.rs#SettingsWindow#load_releases]] lazily sends `ListReleases`; [[crates/scribe-client/src/settings/release_notes.rs]] converts sanitized HTML into GPUI heading, paragraph, list, code, quote, and rule blocks.

The newest release opens first. Left moves newer, right moves older, unavailable directions render no arrow, and navigation resets only the nested note scroller. Only the compact centered title/date block is the GitHub release link—not the surrounding header row—and its zero-delay "View in Github" tooltip is anchored above the block so it never covers the notes.

Body links retain sanitized HTTP(S) targets and open through the client's allowlisted URL path. A persistent right-edge track/thumb makes overflow visible immediately; the document is also a keyboard stop whose Page Up, Page Down, Home, and End keys control only its nested scroll.

Fresh results render immediately. Stale results keep cached notes and expose Refresh; failed results explain the problem and expose Retry. The server-side catalog and protocol remain unchanged.

## Sidebar Footer

The settings sidebar footer displays the running Scribe version as quiet monospace text pinned under the contents list, compiled in from `env!("CARGO_PKG_VERSION")`.

The GPUI window renders `Scribe v<version>` directly in [[crates/scribe-client/src/settings/window.rs#SettingsWindow#render_nav]]; because the string is baked at compile time there is no runtime injection step and no degraded state to handle.

## Singleton

The settings app uses the same singleton structure as the terminal client: a lock file plus a Unix socket for focus handoff. `settings.lock` serializes bind-or-connect, while the bound `settings.sock` owns the singleton lifetime.

Singleton socket commands are one-line JSON payloads capped at 4 KiB before parsing, so a same-UID peer cannot force unbounded line allocation in the settings process. Focus commands may carry the launcher terminal rectangle; `run_settings` parses the same anchor from `SCRIBE_SETTINGS_ANCHOR` before singleton acquisition and window creation.

That same socket also accepts a `quit` command from the client and server shutdown paths. The client sends it immediately for explicit `Quit Scribe`, and the server sends it after a short grace period once the last client disconnects, so the standalone settings window does not outlive the app while still tolerating fast reconnect handoffs. Socket-driven `quit` exits preserve the persisted `open` flag on both Linux and macOS so the next fresh Scribe launch restores settings only when the window had been open before app shutdown; native user closes still mark it closed.

## State Persistence

Window geometry and open state are saved to the active flavor's state root, using `$XDG_STATE_HOME/scribe/settings_state.toml` for stable installs and `$XDG_STATE_HOME/scribe-dev/settings_state.toml` for `scribe-dev`, via .

The GPUI settings launch opens at its 1040×720 layout minimum when the display can hold it. [[crates/scribe-client/src/main.rs#TerminalView#open_or_focus_settings]] passes the launching terminal's live screen-space rectangle to [[crates/scribe-client/src/settings/window.rs#open_settings_window]].

An in-app anchor owns position: the saved settings size is retained when sane, then centered over the source terminal and clamped to the source display's visible work area. Saved `x`/`y` apply only when no launcher anchor exists, so a prior top-right placement cannot override the window that opened Settings.

Undersized or oversized legacy physical-pixel geometry migrates to the compact composition. Platforms that do not expose a source origin fall back to GPUI/compositor placement rather than inventing `(0, 0)`.

Computing the anchor is not enough to be placed by it. On X11 the creation
bounds handed to `open_window` are only a hint — GPUI sets no
`USPosition`/`PPosition`, and under ICCCM a window without one is placed at the
window manager's discretion, which is why a correctly centred settings window
still opened in the corner of the screen. [[crates/scribe-client/src/settings/window.rs#open_settings_window]]
therefore re-asserts the computed origin through
[[crates/scribe-client/src/monitor.rs#apply_saved_position]], the same EWMH
move the terminal windows already use, once before the window is mapped and
again from the first frame via `SettingsWindow::apply_pending_position` — a
window manager may ignore a move for a window it has not mapped yet. The
pending value is taken, so it runs once and never fights a window the user has
since moved.

Only an anchored position is asserted. Without a launcher anchor every
candidate is a guess — a stale saved position, or a centring on whichever
display GPUI calls primary — and forcing a guess is worse than the window
manager's own placement, which at least lands on the active monitor. An
unanchored open therefore stays a hint, as it was before the assertion existed.

The anchor itself comes from the launching window's live rect
([[crates/scribe-client/src/main.rs#TerminalView#live_settings_anchor]]),
refreshed by the same bounds observer that drives geometry capture, and falls
back to the persisted record. The record alone was not enough: it carries an
origin only on platforms that expose one, and a window that has never moved has
no saved position at all.

Raising counts as opening. A second chord raises the existing settings window
rather than stacking a duplicate, and
[[crates/scribe-client/src/settings/window.rs#recenter_settings_window]] moves
it back over the window that asked. Without it the chord answered from wherever
the window was left — after a move, or on a second monitor, an entirely
different screen from the one the user pressed it on.

## GPUI Settings Window

The GPUI rebuild reproduces the deleted `scribe-settings` webview app as a window in the client process, opened from a running terminal window or from `scribe-client --settings`.

### Console presentation

The settings window is one instrument surface: a single ground, one hairline
between rows, and one accent that means live state and nothing else.

No cards, no tiles, no boxed controls, and no standing status chrome —
hierarchy comes from type scale, spacing, and restraint.

[[crates/scribe-client/src/settings/window.rs#SettingsColors#resolve]] fixes
the settings palette independently from the active terminal theme: `#0c0d0f`
ground, `#17181c` for the one lifted surface (the anchored menu), white-alpha
hairlines (6% rules, 8% window edge), `#edeef1` text, `#9aa0a8` data values,
`#7d838d` for every secondary text role, and `#666c75` for non-text marks
only. That last split is load-bearing: `quiet_text` clears WCAG 1.4.3 at
4.6:1 and carries section labels, units, captions, and placeholders, while
`glyph` clears only 1.4.11 at 3.7:1 and is restricted to chevrons, arrows,
the search magnifier, and window controls. Putting text in `glyph` is the
regression this pair exists to prevent.

`#6e8bff` is spent only on live state — the keyboard focus ring, an
in-progress shortcut capture, and the mark on a selected menu option. Active
state is white (`text`): the ON toggle track, the selected nav row's label,
a selected inline option. Keeping "on" and "live" in different channels is
what lets the accent stay rare enough to read. Validation failures use a
separate `#ff7a70` error ink.

The 44px titlebar shares the ground, carries no seam, and leads with
`Settings` at the same 18px spine the sidebar labels use; macOS keeps its
traffic-light reservation. The 212px sidebar has no seam either — the measure
and the gutter separate the columns. Its contents list is text-only at 28px
per row (no page glyphs; the label is the affordance), grouped under quiet
10px tracked labels with 26px of air above each group, and it ends in a
monospace `v<version>` footer. Non-focusable group labels do not enter
keyboard traversal.

The real AccessKit search input at the top of the sidebar receives focus with
Ctrl+K and filters page names, summaries, section names, control labels, and
dotted keys. It has no resting chrome: a wash on hover, an accent ring on
focus, and the magnifier at the trailing edge so the placeholder starts on
the label spine. Matching pages remain navigable, and matching controls
filter inside the selected page.

Content uses 34px gutters and caps every row at a 660px measure centred in
the pane, so the shared right edge the controls align on stays put as the
window grows. The page header is a 17px medium title over a quiet 12.5px
summary and carries no corner note: live apply is the contract, not a
caption, and the config path belongs in a command rather than in chrome.
Section labels are 10px tracked caps with 42px above and 4px below.

Rows run 52px, growing only for a rejected edit or a capture hint, separated
by one hairline that the first row of each section suppresses. Controls carry
no resting fill: switches are 32×18 with a white on-track and a ground-tone
knob, steppers are a bare monospace value whose `−`/`+` are revealed by the
row's own hover group so the number column never shifts, choices are the
value plus a chevron, text and hex fields are an underline that appears on
hover and turns accent on focus, and actions are text with a rule under them.
Activating a stepper value opens the shared native exact-entry field seeded
from its formatted number; Enter or blur commits only finite in-range values
at the control's integer/decimal precision, while Escape closes the field on
the saved number and a rejected value stays in the open field with its inline
error. Only the open field is a fixed width — the resting number still sizes
itself, so the `−`/`+` gutter is unchanged until an edit starts.
A gated row mutes to `quiet_text` and appends `· off` rather than dropping to
an opacity that would push its explanation below 4.5:1. Keybinding and
read-only values render as plain right-aligned quiet monospace — the missing
outline is the read-only mark, and AccessKit still says `Read-only value`.

The Colors page leads with the palette rather than the controls that adjust
it. [[crates/scribe-client/src/settings/window.rs#SettingsWindow#render_ansi_palette]]
renders the sixteen ANSI entries as one 8×2 swatch grid instead of sixteen
near-identical hex rows — the palette shown as a palette — and
[[crates/scribe-client/src/settings/window.rs#SettingsWindow#render_terminal_preview]]
follows it with a live sample line, so an edit is judged against terminal
output instead of against abstract chips. Theme, prompt-bar, and the rest of
the rows sit underneath; the reader never scrolls to see what a control just
did. Each grid cell keeps the same anchored editor the rows used — presets,
hue strip, custom palette, exact-value entry.

### Preset preview

A preset row answers one question — is this readable — so it renders one line
of terminal set in the candidate theme rather than a strip of colour chips.

[[crates/scribe-client/src/settings/window.rs#theme_preview_strip]] paints the
row's tile from that preset's own background, foreground, and prompt colours,
and the menu widens to `THEME_MENU_WIDTH` to hold a name beside it. Long
preset names elide instead of wrapping, because a wrapped label in a
fixed-height row overlaps the row beneath it.

Hovering a row previews it across the whole page: `preview_theme` holds the
pointed-at theme, and
[[crates/scribe-client/src/settings/window.rs#SettingsWindow#displayed_theme]]
is what the swatch grid, its hex captions, and the terminal preview paint
from. The preview pane draws its own background from that theme and takes
every colour in it — including the dim text, which is the theme's bright black
rather than a settings token. Background is the half of a theme that decides
whether anything else on it is legible, so a preview that kept the settings
ground would misreport every contrast it showed. Nothing is written until the row is clicked — browsing costs no config
edit — and closing the menu clears it.

Sliding between rows fires the entered row's hover before the left row's, so
the leave branch clears `preview_theme` only when the theme still on screen is
its own. Without that guard the row being left erases the preview its
neighbour just set, and the page snaps back to the saved theme mid-browse.

The content pane's page-length affordance is the terminal's own overlay
scrollbar rather than a second one: [[crates/scribe-client/src/settings/window.rs#SettingsWindow#tick_content_scrollbar]]
drives [[crates/scribe-client/src/scrollbar.rs#ScrollbarState]] on the render
pass, so the same 6px thumb fades in on scroll and out after 1.5s of idle. It
also shows once on open and on every page switch, which is when page length
is the thing the reader needs told. The thumb is a quiet white wash, not
the accent — length is structure, not live state — and it is painted absolutely
over the scroller, reserving no column. Under `reduce_motion` it simply stays
put on an overflowing page instead of animating. Pixels are the scroll unit
the pure geometry counts in, converted through
[[crates/scribe-client/src/scrollbar.rs#round_scroll_units]] because the
workspace denies lossy float-to-int casts.

It is a control, not only a hint: the same pointer grammar the terminal pane
uses is wired onto the window root, where a gesture keeps being driven after
the pointer leaves the thin track. Hover in the 3x hit zone widens the thumb
and pins the overlay open ([[crates/scribe-client/src/settings/window.rs#SettingsWindow#hover_content_scrollbar]]),
a press on the thumb starts a drag and a press elsewhere on the track jumps
the page ([[crates/scribe-client/src/settings/window.rs#SettingsWindow#press_content_scrollbar]]),
and a release re-arms the fade. The press is claimed in the capture phase so
overlay chrome never also arms the control it covers — and a page that fits
has no track, so `hit_test_scrollbar` declines and the press stays the
control's. Every hit test and both gestures resolve through the one
[[crates/scribe-client/src/settings/window.rs#SettingsWindow#content_scrollbar_layout]]
the paint pass uses, so they cannot disagree about where the track is;
[[crates/scribe-client/src/settings/window.rs#content_scroll_offset]] is the
only new math, the inverse mapping back onto the pixel scroller.

At a numeric bound, the unavailable stepper button has no click handler or focus/tab stop and its accessible label names the reached limit. Pointer use clears keyboard-only focus styling and records the clicked target so later keyboard traversal resumes from true UI state.

### Accessibility semantics

[[crates/scribe-client/src/settings/window.rs#SettingsWindow]] gives settings navigation, controls, values, and feedback stable AccessKit roles and IDs.

The sidebar is a selected tab list, toggles and steppers report state/value, and one status node announces preflight and trust outcomes without stale duplicates.

The config-write and singleton logic stay 1:1 with the old app; only the HTML/CSS/JS surface is replaced with GPUI elements.

The webview delivery is gone; its feature set lives in . The config-apply path is ported verbatim as  (routing every `{key, value}` edit through ), so the  semantics — clamps, enum parsing, keybinding routing, theme seeding — are unchanged. The one-shot server-action client is ported as  and its release/env/remote siblings.

### Page model

The eleven settings pages are described by
[[crates/scribe-client/src/settings/model.rs#page_controls]]: each owns an
ordered control list keyed by the dotted config key the apply path understands.

The pages are appearance, colors, AI, terminal, environment, keybindings, workspaces, updates, notifications, remote, and agent API. Updates combines the old update and release actions; environment splits the env-persistence opt-in out of terminal because enabling it needs a live server round-trip rather than a plain config write.

The Colors page keeps only the `Custom` escape hatch in its declarative preset
control. [[crates/scribe-client/src/settings/window.rs#build_theme_preset_cache]]
resolves 192 installed presets into window-local entries at construction and
reload. Each resolved row previews background, foreground, and ANSI 0-7; the
`Custom` row previews inline colors only while Custom is active. The native
search input filters display labels case-insensitively, reports filtered
AccessKit positions, and names an empty result without changing selection or
preview behavior. Keyboard focus therefore clones a constant-sized control
instead of every resolved theme.

[[crates/scribe-client/src/settings/window.rs#SettingsWindow#render_control]]
renders that model generically: toggles flip, choices cycle, and numeric
steppers step through their `−`/`+` controls or closed Left/Right keys.
Enter/Space or a pointer activation on a stepper value opens exact entry using
the shared native input state, which is why every `ControlKind::Stepper` — the
28 declarative ones and Smart Selection's `Test cursor` — inherits typing
without its own editor. Its finite, precision-shaped, in-range value is
committed as a JSON number through
[[crates/scribe-client/src/settings/window.rs#SettingsWindow#commit_control_value]],
never as text through the config apply path. Other controls commit immediately
like the old live-apply webview. Closed `theme.preset` Left/Right cycling
updates its pending label on every step and restarts one 250 ms `cx.spawn`
timer; settle or focus movement sends only the final token through the existing
apply path, while Escape cancels the task and reveals the saved value without a
write. Current values come from
[[crates/scribe-client/src/settings/values.rs#current_value]]. Shared colour
controls open the selector below, general free-text controls edit inline,
keybinding rows list every action's combos, and Workspaces owns its path editor
and dynamic badge-colour controls.

The settings window has a window-local keyboard traversal order: Tab/Down and Up move through the sidebar followed by actionable controls on the selected page; Enter/Space activates the focused page, toggle, choice, stepper, color selector, or action; Left/Right adjust toggles, closed choices, closed steppers, and color presets. A stepper whose exact entry is open keeps Left/Right in its text field instead, and Tab or an arrow that leaves the row commits the typed value first — a rejected one holds focus where it is. An open color selector sends Tab into its exact-value field. A high-contrast border marks the current stop, and the independently scrollable content pane remains reachable through that ordered traversal. These handlers only live on the settings window, so terminal-window shortcuts are unaffected.

A closed choice opens with Enter or Space. While its menu is open, Up/Down move a neutral-wash highlight through the filtered rows, Enter applies that row, Left/Right and forward or reverse Tab stay inside the menu, and Escape unwinds without applying. AccessKit reports the highlighted row as the active descendant while `aria-selected` continues to name the applied value.

The root takes focus when the window opens, so Ctrl+K and traversal work before
any click. Escape unwinds the innermost state first: an exact color edit restores
its opening value and closes its picker, and a stepper's exact entry closes on
the saved number because a stepper rests closed; otherwise
[[crates/scribe-client/src/settings/window.rs#SettingsWindow#dismiss_transient_state]]
discards a pending closed-preset step, closes a color picker, clears a preset
filter, closes its menu, then clears page search. Titlebar window controls claim
Enter/Space only, letting settings-wide keys continue to the root.

Action controls route through [[crates/scribe-client/src/settings/window.rs#SettingsWindow#run_action]], which is the single live entry point into [[crates/scribe-client/src/settings/server_action.rs]] — the update check, the release list, the keystore preflight, and the whole LAN trust surface below.

### Color selection

Every color control uses one anchored selector with named presets, a hue strip, a continuous saturation/brightness palette, and an exact-value field for hex or AI `ansi:N` input.

[[crates/scribe-client/src/settings/window.rs#SettingsWindow#render_color_selector]]
keeps the saved canonical value and its swatch visible on the compact trigger.
The 280px palette opens 4px below the trigger and aligns its right edge with
the 240px control, so the source row stays readable. Enter or Space opens the
anchored palette, Left and Right apply presets, and Tab enters the exact-value
field. Preset and custom-palette choices commit immediately through
[[crates/scribe-client/src/settings/window.rs#SettingsWindow#select_color]],
which reuses the existing validator and config writer rather than adding a
second persistence path. The custom canvas maps pointer position to continuous
saturation and brightness for the chosen hue. Preset, hue, and canvas nodes are
pointer-only images rather than false tab stops: the outer selector owns
keyboard preset stepping, and the exact field owns arbitrary keyboard input.

The five prompt-bar controls show their active theme-derived swatches when no
override is saved. Each trigger is followed by a keyboard-reachable Reset;
[[crates/scribe-client/src/settings/window.rs#SettingsWindow#render_prompt_bar_color_control]]
commits an empty value so the optional TOML key is omitted and derivation resumes.

### Inline editing

The color selector's exact-value field, general free-text rows, and numeric
stepper values share one inline editor, so there is a single native-input
target and commit path rather than one editor per control kind.

[[crates/scribe-client/src/settings/window.rs#SettingsWindow#begin_inline_edit]]
opens it from a free-text row, the color palette's exact-value action, or a
stepper activation, seeding the field from the saved value and retaining the
whole [[crates/scribe-client/src/settings/model.rs#Control]], because the
commit needs its kind. Enter commits through
[[crates/scribe-client/src/settings/window.rs#SettingsWindow#save_inline_edit]],
which routes the typed value into the same `{key, value}` apply path every
other control uses; there is no parallel persistence route. Losing focus runs
the same helper for a stepper, through
[[crates/scribe-client/src/settings/window.rs#SettingsWindow#save_stepper_edit_on_blur]]
on traversal and on the `edit_handle` blur subscription that catches pointer
focus changes. Escape cancels. For a color exact-value edit, Escape
also closes the picker and restores focus to its selector; Tab or Shift-Tab
closes it before continuing traversal. Free-text rows are therefore focusable
and tab-stop, where they previously rendered read-only.

#### Commit routes by control kind

[[crates/scribe-client/src/settings/window.rs#inline_commit_value]] canonicalizes a colour through the apply path's own hex/ansi validator, so a rejected colour never reaches the config writer, and commits general free text verbatim.

[[crates/scribe-client/src/settings/window.rs#numeric_inline_value]] parses a
stepper's finite integer or decimal shape, checks its bounds and displayed
precision, and produces a JSON number before
[[crates/scribe-client/src/settings/window.rs#SettingsWindow#commit_control_value]]
reaches the config or Smart Selection consumer. It shapes that number through
[[crates/scribe-client/src/settings/window.rs#stepper_number]], the same helper
[[crates/scribe-client/src/settings/window.rs#SettingsWindow#step]] commits with:
a whole number stays a JSON integer because most steppers deserialize into
`u16`/`u32`/`u64` config fields and `Test cursor` reads its value back as
`as_u64`, and a fractional one keeps two decimals. Invalid or out-of-range
numeric text stays editable with a `Role::Alert` error and never changes config.

The apply path stays the single authority on what each key accepts, so no second validator exists here.

#### Escape cancels the edit

[[crates/scribe-client/src/settings/window.rs#revert_inline_input]] restores the opening value and clears rejection ink, so abandoning an edit never touches config.

Color edits also close their picker, preventing focus from remaining in hidden exact-entry chrome.

### Shortcut capture

Keybinding rows are recorded from the keyboard rather than typed: activating a
row puts it in listening state and the next keystroke becomes the binding, so a
shortcut is entered the way it will later be pressed.

[[crates/scribe-client/src/settings/window.rs#SettingsWindow#begin_capture]]
opens the state on click or Enter/Space and moves focus to the window root,
because a recording row has to read raw keys through
[[crates/scribe-client/src/settings/window.rs#SettingsWindow#on_key_down]]
rather than through a text input.
[[crates/scribe-client/src/settings/window.rs#SettingsWindow#capture_keystroke]]
then claims every keystroke ahead of the window's own Ctrl+K and traversal
handling — otherwise those chords could never be bound to anything. Modifier
presses keep the row listening, Escape abandons it, a bare Backspace unbinds
the action to an empty combo list, and anything else commits through the same
`{key, value}` apply path every other control uses, under
`keybindings.<action>`. Keybinding rows are therefore focusable and tab-stop,
where they previously rendered read-only and were skipped by traversal
entirely.

#### What a capture is allowed to bind

[[crates/scribe-client/src/settings/window.rs#combo_for_capture]] refuses a keystroke that carries no Ctrl, Alt, or Super, because a plain key bound to a layout action stops reaching the terminal at all — the letter becomes untypeable in every pane.

It then spells the keystroke in the combo grammar and runs it through [[crates/scribe-client/src/keybindings.rs#Keybinding#parse]], the client's own dispatcher parser, so a key the runtime could never match (an F-key, an unknown name) is rejected at the point of entry instead of being written as a shortcut that silently does nothing.

#### Conflicts are named, not silently won

[[crates/scribe-client/src/settings/window.rs#conflicting_action]] refuses a combo another action already owns and names that action on the row, leaving it listening so the next press is the correction.

Both sides of the comparison are folded by [[crates/scribe-client/src/settings/window.rs#canonical_combo]] first, so a hand-written `super+ctrl+w` in `config.toml` and a captured `ctrl+cmd+w` are recognized as the same shortcut. Re-pressing a row's own current combo is not a conflict with itself.

#### Reading the page

Combos render as key caps through [[crates/scribe-client/src/settings/window.rs#key_cap]] — one bordered monospace chip per token, `Page Down` rather than `pagedown` — because a shortcut is a thing to press, not a string to decode.

Rows are labelled by [[crates/scribe-client/src/settings/model.rs#keybinding_label]] in the same sentence case the rest of the pages use (`New Claude resume tab`), where the page previously showed the raw action name with its underscores swapped for spaces. Search also matches a row's combos, so the page answers "what is on Ctrl+Shift?" as well as "what runs the palette?".

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
