# GPUI client parity inventory

This committed oracle enumerates the frozen IPC, input, and UI surface the GPUI
client must preserve before cutover, plus every behaviour requirement in
[`spec.md`](spec.md)'s requirement register. The message, keybinding and
removed-key tables are derived from the legacy client and `scribe-common` at the
016 planning baseline; the rendering and spec-behaviour tables are derived from
the register.

**Both derivations are load-bearing.** Until 2026-07-27 the row set came only
from the legacy client's IPC and keybinding surface, so nine spec requirements
had no row, no oracle scored them, and the reachable-row count measured the
tabulated subset rather than parity. `tools/check-parity-inventory.sh` now fails
when a register id in `spec.md` has no carrying row, which is what makes the
total a parity measure.

Every row carries a **Reachable from** cell naming the live-path symbol that
must call the feature. The cells were first populated from the per-row evidence
in [`reachability-audit.md`](reachability-audit.md) (audited at `f56ef95`) and
have been re-derived from the source since, as the wiring beads landed. A row
whose cell is a `— (unwired)` / `— (missing)` marker **cannot be marked done**,
regardless of how many tests pass.

Every count in this file — the per-section footers, the roll-up table, and the
user-facing sentence under it — is derived from those marker cells by
`tools/check-parity-inventory.sh`, which fails when the document disagrees with
itself or with the source. Nothing here is hand-maintained, so the launch gate
can read the reachable-row total off this file and trust it. Run it directly:

```bash
just parity-inventory
```

## Definition of done

A parity row is done only when **both** hold:

1. **Reachable.** Its "Reachable from" cell names a real symbol on one of the
   four live paths of the shipped binary — the render/paint path
   (`main.rs::open_window` → `TerminalView::render` → `TerminalElement::paint`),
   the key path (`main.rs::handle_overlay_key` → `handle_binding` →
   `on_key_down`), outbound IPC (`ipc_bridge::IpcSink`, plus the raw sends in
   `main.rs::run_connection`), or inbound IPC (`main.rs::run_reader` and the
   `main.rs::dispatch_server_message` table it feeds). The
   settings window's `settings/window.rs::SettingsWindow::run_action` is a
   fifth, narrower entry point. Bead .82 gave it an in-app trigger, so it is now
   reached from the terminal window (settings chord, palette row, titlebar gear)
   as well as from `scribe-client --settings`; rows resting on it are
   flagged inline as settings-window rows.
2. **Verified against the running app.** Its verification method (below, as
   upgraded) passes while driving the real client, not a directly constructed
   entity.

A green `#[gpui::test]` is necessary but never sufficient for a user-facing
row: a headless entity test passes identically whether or not the application
constructs the entity. That failure mode is what the reachability audit found
across 113 of the 164 user-facing rows that existed at `f56ef95`; the wiring
beads have since closed all of them.

## Verification methods

Each row declares its intended parity oracle. `golden` is a captured
byte/serialization fixture; `gpui-test` is a headless `#[gpui::test]`;
`visual-E2E` is a deterministic screenshot comparison, driven through
`xdotool` against the real window; `scripted-E2E` drives the app and server and
asserts on the wire; `manual` requires a human interaction or platform check.

### Method upgrades applied (audit recommendation §3)

`gpui-test` is retained **only** for the nine removed-configuration-key rows,
which assert the *absence* of behaviour and are genuinely load-path tests.
Every other row moved:

- **27 rows `gpui-test` → `scripted-E2E`** — the 11 outbound and 16 inbound
  variants the audit names by hand, all of which need a real client plus a real
  server asserting on the wire.
- **1 row `gpui-test` → `visual-E2E`** — Font fallback, because a Nerd Font
  glyph must actually rasterize.
- **All 54 named keybinding actions → `visual-E2E`**, driven by `xdotool`
  against the real window (45 were `gpui-test`, 2 were `scripted-E2E`, 7 were
  `golden`). For the seven terminal shortcuts the golden byte fixtures are
  retained as a supplementary encoder oracle, but the reachability gate is the
  `visual-E2E` run: golden fixtures assert the encoder, not that the key ever
  reaches it.

Where the audit named specific rows, this table follows it exactly. Where it
did not — the `manual` Opacity row, and the `golden`/`visual-E2E` rows that
were never headless-only — the method is unchanged, because those oracles
already require the running app and so satisfy the principle without an
upgrade. The `Bell` row was `manual` for the same reason and has since been
upgraded to `scripted-E2E`: FU-22 found the routed behaviour lands on the
window's `WM_HINTS` urgency flag, which a script can read directly.

## Client messages (51 variants, 49 reachable)

Every `ClientMessage` variant from `crates/scribe-common/src/protocol.rs` must
remain serializable and be emitted by the corresponding GPUI interaction.

| Variant | Surface | Verification method | Reachable from | Status |
| --- | --- | --- | --- | --- |
| `KeyInput` | terminal input | golden | `main.rs::send_key_bytes` → `ipc_bridge::IpcSink::key_input` | required |
| `Resize` | terminal resize | visual-E2E | `main.rs::report_cell_metrics`, `main.rs::attach_session`, `TerminalView::grid_area_probe` → `sync_grid_geometry` (bead .98) → `IpcSink::resize` — `tests/e2e/visual/window-resize.sh` | required |
| `CreateSession` | tabs, panes, and AI tabs | scripted-E2E | `main.rs::create_tab` → `IpcSink::create_session` | required |
| `CloseSession` | pane/tab close | scripted-E2E | `main.rs::close_active_tab` → `IpcSink::close_session` | required |
| `CreateWorkspace` | workspace creation | scripted-E2E | `main.rs::TerminalView::split_workspace` → `IpcSink::create_workspace` | required |
| `CloseWorkspace` | workspace close | scripted-E2E | `main.rs::TerminalView::close_pane` / `reconcile_panes` → `TerminalView::close_workspace` → `IpcSink::close_workspace` | required |
| `MoveSession` | session relocation | scripted-E2E | `main.rs::TerminalView::follow_session_to_region` → `IpcSink::move_session` | required |
| `Subscribe` | session stream subscription | scripted-E2E | `main.rs::attach_session` / `TerminalView::attach` → `IpcSink::subscribe` | required |
| `RequestSnapshot` | snapshot tooling | scripted-E2E | `main.rs::report_cell_metrics` / `forward_replay` → `IpcSink::request_snapshot` | required |
| `ListSessions` | startup/reconnect | scripted-E2E | `main.rs::run_connection` (sent on every connect) | required |
| `AttachSessions` | reconnect restore | scripted-E2E | `main.rs::attach_session` → `IpcSink::attach_sessions` | required |
| `ConfigReloaded` | live config reload | scripted-E2E | `main.rs::apply_config_reload` → `IpcSink::config_reloaded` | required |
| `ReportWorkspaceTree` | layout persistence | scripted-E2E | `main.rs::TerminalView::report_workspace_tree` → `PaneShell::wire_tree` → `IpcSink::report_workspace_tree`, after every layout mutation | required |
| `SearchRequest` | find overlay | scripted-E2E | `main.rs::TerminalView::send_search_request` → `ipc_bridge::IpcSink::search_request`, once a find-overlay query settles past the 150 ms debounce | required |
| `SearchClosed` | find overlay | scripted-E2E | `main.rs::TerminalView::close_find_overlay` → `ipc_bridge::IpcSink::search_closed`, when the overlay is dismissed or rebuilt by a theme reload | required |
| `Hello` | registration/adoption | scripted-E2E | `main.rs::run_connection` (sent on every connect) | required |
| `CloseWindow` | close dialog | scripted-E2E | `main.rs::TerminalView::route_close_action` → `ipc_bridge::IpcSink::close_window`, from the close dialog the WM close request and the quit chord raise | required |
| `QuitAll` | quit-all dialog | scripted-E2E | `main.rs::TerminalView::route_close_action` → `ipc_bridge::IpcSink::quit_all`, from the same close dialog | required |
| `TriggerUpdate` | update dialog | scripted-E2E | `main.rs::TerminalView::route_update_action` → `ipc_bridge::IpcSink::trigger_update`, from the status-bar CTA's confirmation | required |
| `DismissUpdate` | update dialog | scripted-E2E | `main.rs::TerminalView::route_update_action` → `ipc_bridge::IpcSink::dismiss_update`, from the status-bar CTA's confirmation | required |
| `DismissCiRun` | workspace CI run bar | visual-E2E | `main.rs::TerminalView::render_ci_run_bars` → `ipc_bridge::IpcSink::dismiss_ci_run`, from owning local clients only | required |
| `SetCiRunDetailsInterest` | expanded workspace CI trace | visual-E2E | `main.rs::TerminalView::toggle_ci_trace` → `ipc_bridge::IpcSink::set_ci_run_details_interest`, from owning and read-only capable windows — `tests/e2e/visual/ci-run-details.sh` | required |
| `CheckForUpdates` | release settings | scripted-E2E | `settings/window.rs::SettingsWindow::run_action` — settings-window row (in-app since bead .82) | required |
| `ListReleases` | release settings | scripted-E2E | `settings/window.rs::SettingsWindow::run_action` — settings-window row (in-app since bead .82) | required |
| `ListWindows` | window management | scripted-E2E | `main.rs::TerminalView::poll_window_list` → `ipc_bridge::IpcSink::list_windows`, on the lifecycle tick while `remote.enabled` | required |
| `DispatchAction` | remote automation | scripted-E2E | `main.rs::TerminalView::offer_action_to_controller` → `ipc_bridge::IpcSink::dispatch_action`, for a feature-015 viewer's window-mutating palette row | required |
| `FocusChanged` | focus reporting | scripted-E2E | `main.rs::TerminalView::report_focus` → `ipc_bridge::IpcSink::focus_changed`, from the window activation observer and the pane-focus reconciliation | required |
| `HookEvent` | hook helper ingress | scripted-E2E | `crates/scribe-hook-helper/src/main.rs::main` — out-of-client by design | required |
| `EnvPreflight` | environment persistence | scripted-E2E | `settings/window.rs::SettingsWindow::run_action` (`action.env_preflight`) and `SettingsWindow::enable_env_persistence`, the toggle's gated ON transition — settings-window row (in-app since bead .82) | required |
| `ClipboardPromptResponse` | OSC 52 prompt | scripted-E2E | `main.rs::TerminalView::answer_clipboard_prompt` → `clipboard::prompt_response` → `ipc_bridge::IpcSink::clipboard_answer`, from the confirmation dialog's Allow/Deny including the Esc default (FU-8) — `tests/e2e/visual/clipboard-osc52.sh` | required |
| `ClipboardBridgeReadReply` | OSC 52 bridge | scripted-E2E | `main.rs::TerminalView::poll_clipboard` → `TerminalView::run_bridge_job` → `clipboard::read_reply` → `ipc_bridge::IpcSink::clipboard_answer`, draining the bridge's job queue on the lifecycle tick (FU-8) — `tests/e2e/visual/clipboard-osc52.sh` | required |
| `RemoteHandshake` | tailnet connect | scripted-E2E | `main.rs::run_remote_connection` → `remote_handshake.rs::perform_remote_handshake` | required |
| `ListRemotePeers` | remote connect picker | scripted-E2E | `main.rs::adopt_remote_surface` and `TerminalView::refresh_remote_peers` → `ipc_bridge::IpcSink::list_remote_peers` | required |
| `GetRemoteEnv` | remote settings | scripted-E2E | `main.rs::probe_remote_env` on the startup transient socket, and `settings/window.rs::SettingsWindow::refresh_trust` → `settings/server_action.rs::request_remote_env` | required |
| `LanHello` | mTLS LAN-dial preamble before session attachment | scripted-E2E | `main.rs::run_lan_connection` → `lan_dial.rs::handshake` | required |
| `LanApprovalDecision` | owner-side fingerprint approval overlay | scripted-E2E | `main.rs::TerminalView::route_lan_approval_action` → `ipc_bridge.rs::IpcSink::lan_approval_decision` | required |
| `ListLanPeers` | merged Local network source in remote connect picker | scripted-E2E | `main.rs::adopt_lan_surface` → `ipc_bridge.rs::IpcSink::list_lan_peers` | required |
| `ListTrustedDevices` | Remote settings trusted-device list | scripted-E2E | `settings/window.rs::SettingsWindow::run_action` → `SettingsWindow::refresh_trust` — settings-window row (in-app since bead .82) | required |
| `RevokeTrustedDevice` | Remote settings device-revocation action | scripted-E2E | `settings/window.rs::SettingsWindow::run_action`, per approved-device row — settings-window row (in-app since bead .82) | required |
| `ListTrustedNetworks` | Remote settings trusted-network list | scripted-E2E | `settings/window.rs::SettingsWindow::run_action` → `SettingsWindow::refresh_trust` — settings-window row (in-app since bead .82) | required |
| `AddCurrentNetworkTrusted` | Remote settings trust-current-network action | scripted-E2E | `settings/window.rs::SettingsWindow::run_action` (`action.add_current_network`) — settings-window row (in-app since bead .82) | required |
| `RemoveTrustedNetwork` | Remote settings trusted-network removal action | scripted-E2E | `settings/window.rs::SettingsWindow::run_action`, per trusted-network row — settings-window row (in-app since bead .82) | required |
| `GetLanEnv` | Remote settings LAN listener/environment summary | scripted-E2E | `settings/window.rs::SettingsWindow::refresh_trust`; also `main.rs::probe_lan_env` → `lan_dial.rs::probe_lan_env` in the terminal window | required |
| `GetLanDialIdentity` | local-server identity fetch before mTLS dialing | scripted-E2E | `main.rs::run_lan_connection` → `lan_dial.rs::LanDialer::build` | required |
| `ControlClaim` | viewer claim/request affordance for a shared window | scripted-E2E | `main.rs::TerminalView::run_share_key` → `share.rs::ShareChrome::intercept_key` → `ipc_bridge.rs::IpcSink::control_intent` | required |
| `ControlRequest` | v3 compatibility alias; the client emits `ControlClaim` | golden | not emitted by design; its live substitute `ControlClaim` is wired through `ipc_bridge.rs::IpcSink::control_intent` | required |
| `ControlGrant` | holder grant/deny prompt for a control request | scripted-E2E | `main.rs::TerminalView::handle_overlay_key` → `share.rs::ShareChrome::intercept_key` → `ipc_bridge.rs::IpcSink::control_intent` | required |
| `RequestBeadsBoard` | workspace Beads board open, hover refresh and pinned poll | visual-E2E | `main.rs::park_workspace_info`, `main.rs::TerminalView::poll_beads_board`, titlebar/region bead hover and click → `ipc_bridge.rs::IpcSink::request_beads_board` — `tests/e2e/visual/beads-board.sh` | required |
| `RequestBeadsIssueDetail` | workspace Beads issue detail panel | unit | — (unwired; protocol-only slice, panel wiring pending) | required |
| `BeadsIssueWrite` | workspace Beads issue detail edits | unit | — (unwired; protocol-only slice, editing pending a guard-capable bd) | required |
| `RequestBeadsEpicGraph` | workspace Beads Flow epic dependency graph | unit | `beads_board.rs::BeadsBoards::request_card_flow` → `main.rs::TerminalView::sync_beads_board_strips` → `ipc_bridge.rs::IpcSink::request_beads_epic_graph` | required |

**Reachability:** 49 of 51 rows name a live-path symbol; 2 are unwired and 0
are missing. One of them — `HookEvent` — names `scribe-hook-helper`'s `main`
rather than a client symbol, because the hook ingress is a separate binary by
design; it is the only out-of-client row in the whole inventory.

## Server messages (68 variants, 65 reachable)

Every `ServerMessage` variant from `crates/scribe-common/src/protocol.rs` must
be handled without loss, including additive sharing and LAN variants.

The live reader's dispatcher `main.rs::dispatch_server_message` handles 60 of
68 variants and routes the rest to `main.rs::unhandled_server_message`, which
logs the variant name and increments a process counter rather than dropping it
silently; the `_ => {}` catch-all the audit found is gone. Five variants —
`UpdateCheckResult`, `ReleaseList`, `EnvPreflightResult`, `TrustedDeviceList`,
`TrustedNetworkList` — are consumed by the settings
window's synchronous request/reply helper in `settings/server_action.rs`, and
each of those rows says so. `BeadsIssueDetail`, `BeadsIssueWriteResult`, and `IssueFocused` are the
remaining unwired replies; their protocol slices land before their panel
consumers. `tools/check-parity-inventory.sh` enforces that:
any variant the dispatcher does not handle must either carry a marker cell or
be annotated a settings-window row, so this column cannot claim a reader arm
that does not exist.

`scribe-gygu.8` owns the server tracker that produces capability-gated CI run
frames. Each clear names its head, so a delayed clear for an older run cannot
erase a replacement.

| Variant | Surface | Verification method | Reachable from | Status |
| --- | --- | --- | --- | --- |
| `PtyOutput` | terminal stream | golden | `main.rs::dispatch_server_message` → `main.rs::on_pane_output_message` → `main.rs::forward_output` (gated on the attached session) | required |
| `ScreenSnapshot` | tooling snapshot | scripted-E2E, visual-E2E | `main.rs::on_pane_output_message` → `main.rs::apply_screen_snapshot` → `screen_replay::snapshot_to_ansi`; `tests/e2e/visual/session-tooling.sh` preserves a scrolled viewport through resize resync | required |
| `SessionReplay` | reconnect replay | scripted-E2E, visual-E2E | `main.rs::on_pane_output_message` → `main.rs::forward_replay` → `session_lifecycle::decode_replay`; `tests/e2e/visual/tab-switching.sh` preserves a returned tab's scrolled viewport | required |
| `AiStateChanged` | AI indicator | visual-E2E | `main.rs::on_ai_message` → `ai_indicator::AiStateTracker::update` | required |
| `AiStateCleared` | AI indicator | visual-E2E | `main.rs::on_ai_message` → `AiStateTracker::remove` + `AiStateTracker::clear_context` | required |
| `CwdChanged` | tab metadata | scripted-E2E | `main.rs::on_chrome_message` arm → `ChromeMetadata::set_cwd` → `StatusBarData.cwd` | required |
| `SessionContextChanged` | session metadata | scripted-E2E | `main.rs::on_chrome_message` arm → `ChromeMetadata::set_context` → `SessionChrome::host_label` / `SessionChrome::tmux_label` | required |
| `TitleChanged` | window title | visual-E2E | `main.rs::on_chrome_message` arm → `TabSessions::set_title` → `TabEntry::display_title` | required |
| `IconTitleChanged` | icon/tab title | visual-E2E | `main.rs::on_chrome_message` arm → `TabSessions::set_icon_title` → `TabEntry::display_title` | required |
| `CodexTaskLabelChanged` | Codex tab label | visual-E2E | `main.rs::on_task_label_message` arm → `TabSessions::set_task_label` → `TabEntry::display_title` | required |
| `CodexTaskLabelCleared` | Codex tab label | visual-E2E | `main.rs::on_task_label_message` arm → `TabSessions::set_task_label` → `TabEntry::display_title` | required |
| `TaskLabelChanged` | AI tab label | visual-E2E | `main.rs::on_task_label_message` arm → `TabSessions::set_task_label` → `TabEntry::display_title` | required |
| `TaskLabelCleared` | AI tab label | visual-E2E | `main.rs::on_task_label_message` arm → `TabSessions::set_task_label` → `TabEntry::display_title` | required |
| `PromptReceived` | prompt history | scripted-E2E | `main.rs::on_ai_message` → `AiChrome::record_prompt` | required |
| `WorkspaceNamed` | workspace chrome | visual-E2E | `main.rs::on_chrome_message` arm → `ChromeMetadata::name_workspace` → `StatusBarData.workspace_name` | required |
| `CiRunState` | workspace CI run bar | visual-E2E (`just e2e-visual-ci-run-bar`) | `main.rs::on_ci_run_message` → `ci_bar::CiRunBars::apply` → `TerminalView::render_ci_run_bars` | required |
| `CiRunDetails` | expanded workspace CI trace | visual-E2E | `main.rs::on_ci_run_message` → `ci_bar::CiRunBars::apply_details` → `TerminalView::render_ci_run_bars` — `tests/e2e/visual/ci-run-details.sh` | required |
| `SessionCreated` | pane lifecycle | scripted-E2E | `main.rs::dispatch_server_message` arm → `session_lifecycle::SessionRegistry::on_session_created` + `main.rs::open_created_tab` | required |
| `SessionExited` | pane lifecycle | scripted-E2E | `main.rs::dispatch_server_message` arm → `main.rs::on_session_exited` → tab removal + `AiChrome::forget` | required |
| `Bell` | terminal bell | scripted-E2E | `main.rs::on_bell_message` queue → `main.rs::TerminalView::poll_bells` → `BellController::on_bell` → `Window::request_attention` | required |
| `Error` | error presentation | visual-E2E | `main.rs::dispatch_server_message` arm → `main.rs::on_server_error` → `main.rs::set_status` | required |
| `GitBranch` | status/tab metadata | visual-E2E | `main.rs::on_chrome_message` arm → `ChromeMetadata::set_git_branch` → `StatusBarData.git_branch` | required |
| `SessionList` | startup/reconnect | scripted-E2E | `main.rs::dispatch_server_message` arm → `main.rs::on_session_list` → `main.rs::sync_tab_strip` | required |
| `WorkspaceInfo` | workspace layout | scripted-E2E | `main.rs::on_workspace_info` → `ChromeMetadata::name_workspace` + parked for `TerminalView::adopt_workspace_info` → `PaneShell::apply_workspace_info` | required |
| `SearchResults` | find overlay | scripted-E2E | `main.rs::on_search_results` → `search::FindResults` → `search::FindOverlayView::adopt_results` → `terminal_element::TerminalElement::with_highlights` | required |
| `Welcome` | registration/adoption | scripted-E2E | `main.rs::dispatch_server_message` arm → `main.rs::on_welcome` → `session_lifecycle::SessionRegistry::adopt_window` | required |
| `TerminalImageLive` | ordered image scene updates | scripted-E2E | `main.rs::on_terminal_image_message` → `ipc_bridge::InboundEvent::TerminalImageLive` → `main.rs::apply_pane_op` → `terminal.rs::DisplayOnlyTerminal::apply_image_live` → `terminal_image_scene::LiveImageScene::apply` (`tests/e2e/terminal-image-client-scene.sh`) | required |
| `TerminalImageReplay` | generation-tagged image snapshot | scripted-E2E | `main.rs::on_terminal_image_message` → `ipc_bridge::InboundEvent::TerminalImageReplay` → `main.rs::apply_pane_op` → `terminal.rs::DisplayOnlyTerminal::apply_image_replay` → `terminal_image_scene::LiveImageScene::apply_replay` (`tests/e2e/terminal-image-client-replay.sh`) | required |
| `TerminalImageCapabilityMismatch` | incapable-viewer refusal | scripted-E2E | `main.rs::on_terminal_image_message` → `terminal_image_scene::capability_mismatch_message` → visible window status bar (`tests/e2e/terminal-image-client-scene.sh`) | required |
| `WindowClosed` | close lifecycle | scripted-E2E | `main.rs::on_window_lifecycle_message` → `window_lifecycle::WindowLifecycle::on_window_closed` → the shell's lifecycle tick quits the app | required |
| `WindowList` | window management | scripted-E2E | `main.rs::on_window_lifecycle_message` → `window_lifecycle::WindowLifecycle::set_windows` → `StatusBarData.remote` | required |
| `RunAction` | remote automation | scripted-E2E | `main.rs::on_remote_message` → `remote_chrome::RemoteChrome::queue_action` → `TerminalView::poll_remote_actions` runs it on the lifecycle tick | required |
| `ActionDispatched` | remote automation | scripted-E2E | `main.rs::on_remote_message` arm — the routing ack for a dispatch this client sent | required |
| `QuitRequested` | quit dialog | scripted-E2E | `main.rs::on_window_lifecycle_message` → `window_lifecycle::WindowLifecycle::on_quit_requested` → the shell's lifecycle tick quits the app | required |
| `UpdateAvailable` | update dialog | visual-E2E | `main.rs::dispatch_server_message` arm → `update::UpdateState::on_available` → `StatusBarData.update_available` | required |
| `UpdateProgress` | update dialog | visual-E2E | `main.rs::dispatch_server_message` arm → `update::UpdateState::on_progress` → `StatusBarData.update_progress` | required |
| `UpdateCheckResult` | release settings | scripted-E2E | `settings/server_action.rs::request_update_check`, reached from `SettingsWindow::run_action` — settings-window row (in-app since bead .82) | required |
| `ReleaseList` | release settings | scripted-E2E | `settings/server_action.rs::request_release_list`, reached from `SettingsWindow::run_action` — settings-window row (in-app since bead .82) | required |
| `PromptMark` | prompt navigation | scripted-E2E | `main.rs::dispatch_server_message` → `main.rs::on_positional_pane_message` → `ipc_bridge::InboundEvent::PromptMark` → `main.rs::apply_pane_op` → `main.rs::apply_prompt_mark` → `session_lifecycle::PromptMarks::record` (FU-7) — `tests/e2e/visual/prompt-marks.sh` | required |
| `TrimScrollback` | terminal history | visual-E2E | `main.rs::on_positional_pane_message` → `ipc_bridge::InboundEvent::TrimScrollback` → `main.rs::apply_pane_op` → `terminal.rs::DisplayOnlyTerminal::trim_history` + `main.rs::apply_trim_scrollback` (bead `.88`) | required |
| `ScrollBottom` | terminal viewport compatibility | scripted-E2E | `main.rs::dispatch_server_message` → `main.rs::on_positional_pane_message` → `ipc_bridge::InboundEvent::ScrollBottom` → `main.rs::apply_pane_op`, which preserves any nonzero display offset for an old-server frame — `tests/e2e/visual/prompt-marks.sh` | required |
| `EnvPreflightResult` | environment settings | scripted-E2E | `settings/server_action.rs::parse_env_preflight_response`, rendered into the Environment page's status line — settings-window row (in-app since bead .82) | required |
| `EnvStatus` | environment status | visual-E2E | `main.rs::on_chrome_message` arm → `ChromeMetadata::set_env_status` → `StatusBarData.env_status` | required |
| `ClipboardPromptRequest` | OSC 52 dialog | visual-E2E | `main.rs::dispatch_server_message` → `main.rs::on_clipboard_message` → `clipboard::ClipboardBridge::park_prompt` → `TerminalView::poll_clipboard` → `dialog::ClipboardDialog` raised on the lifecycle tick (FU-8) — `tests/e2e/visual/clipboard-osc52.sh` | required |
| `ClipboardBridgeWrite` | OSC 52 bridge | scripted-E2E | `main.rs::on_clipboard_message` → `clipboard::ClipboardBridge::push_job` → `TerminalView::poll_clipboard` → `TerminalView::run_bridge_job` → `clipboard::bridge_write` under the FR-019 focus gate (FU-8) — `tests/e2e/visual/clipboard-osc52.sh` | required |
| `ClipboardBridgeReadRequest` | OSC 52 bridge | scripted-E2E | `main.rs::on_clipboard_message` → `clipboard::ClipboardBridge::push_job` → `TerminalView::run_bridge_job` → `clipboard::read_reply`, answered on the wire by `IpcSink::clipboard_answer` (FU-8) — `tests/e2e/visual/clipboard-osc52.sh` | required |
| `RemoteHandshakeReply` | tailnet connect | scripted-E2E | `remote_handshake.rs::perform_remote_handshake` during the preamble; `main.rs::on_remote_message` on the live reader | required |
| `WindowTakenOver` | remote-control landing | visual-E2E | `main.rs::on_remote_message` → `remote_chrome::RemoteChrome::displace` → `lost_control.rs::lost_control_overlay` freezes the window under the reclaim banner | required |
| `RemoteDisconnect` | remote-control landing | visual-E2E | `main.rs::on_remote_message` → `remote_chrome::RemoteChrome::sever` → the status bar's typed reason | required |
| `RemotePeerList` | remote connect picker | visual-E2E | `main.rs::on_remote_message` → `remote_chrome::RemoteChrome::set_peers` → picker overlay | required |
| `RemoteEnv` | remote settings | scripted-E2E | `main.rs::on_remote_message` → `remote_chrome::RemoteChrome::set_env`; also parsed in `settings/server_action.rs` for the Remote page's Tailscale note | required |
| `LanApprovalPending` | cancelable connecting-side waiting-for-approval overlay | visual-E2E | `lan_dial.rs::handshake` during the preamble; `main.rs::on_lan_message` on the live reader | required |
| `LanApprovalResult` | terminal LAN dial acceptance/refusal outcome | visual-E2E | `lan_dial.rs::handshake` during the preamble; `main.rs::on_lan_message` on the live reader | required |
| `LanApprovalRequest` | owner-side device fingerprint approval overlay | visual-E2E | `main.rs::on_lan_message` → `lan.rs::LanChrome::park_approval` → `main.rs::TerminalView::poll_lan_approval` | required |
| `LanPeerList` | Local network entries merged into remote connect picker | visual-E2E | `main.rs::on_lan_message` → `lan.rs::LanChrome::set_peers` | required |
| `TrustedDeviceList` | Remote settings trusted-device rows | scripted-E2E | `settings/server_action.rs::parse_trusted_devices_response`, rendered by `SettingsWindow::trusted_device_rows` — settings-window row (in-app since bead .82) | required |
| `TrustedNetworkList` | Remote settings trusted-network rows | scripted-E2E | `settings/server_action.rs::parse_trusted_networks_response`, rendered by `SettingsWindow::trusted_network_rows` — settings-window row (in-app since bead .82) | required |
| `LanEnv` | Remote settings LAN listener/environment summary | scripted-E2E | `settings/server_action.rs::parse_lan_env_response` in the settings window; `main.rs::on_lan_message` → `lan.rs::LanChrome::set_env` in the terminal window | required |
| `LanDialIdentity` | client mTLS identity returned by the local server | scripted-E2E | `lan_dial.rs::fetch_dial_identity`; named (never stored or logged) by `main.rs::on_lan_message` | required |
| `ShareRoster` | presence badge and live-viewer role/claim state | visual-E2E | `main.rs::dispatch_share_message` → `share.rs::ShareChrome::apply_roster` | required |
| `ControlRequested` | holder or owner grant/deny control prompt | visual-E2E | `main.rs::dispatch_share_message` → `share.rs::ShareChrome::request` | required |
| `ControlDenied` | requester control-denied notice | visual-E2E | `main.rs::dispatch_share_message` → `share.rs::ShareChrome::deny` | required |
| `ShareEnded` | shared-viewer end landing and state cleanup | visual-E2E | `main.rs::dispatch_share_message` → `share.rs::ShareChrome::end` | required |
| `BeadsBoard` | workspace Beads board snapshot, loading, unavailable and not-detected states | visual-E2E | `main.rs::dispatch_workspace_message` → `beads_board.rs::BeadsBoards::update` → `beads_board.rs::render` — `tests/e2e/visual/beads-board.sh` | required |
| `BeadsIssueDetail` | workspace Beads issue detail panel | unit | — (unwired; protocol-only slice, panel wiring pending) | required |
| `BeadsIssueWriteResult` | workspace Beads issue detail write outcome | unit | — (unwired; protocol-only slice, editing pending a guard-capable bd) | required |
| `BeadsEpicGraph` | workspace Beads Flow epic dependency graph reply | unit | `main.rs::dispatch_workspace_message` → `beads_board.rs::BeadsBoards::apply_epic_graph` → `beads_board.rs::flow_strip` | required |
| `IssueFocused` | local unshared Flow live-agent issue binding | unit | — (unwired; halo rendering pending) | required |

**Reachability:** 65 of 68 rows name a live-path symbol; 3 are unwired and 0
are missing. (The audit's original figures at `f56ef95` were 18 reachable, 11
unwired and 30 missing.)

## Input and keybinding checklist (56 named actions)

The GPUI port retains every parsed `Bindings` action from
`crates/scribe-client/src/input.rs`. All 56 are enumerated individually below,
because the previous per-subsystem grouping hid where they break: parsing was
never the problem — `keybindings.rs::translate_key_action` maps all 55 onto a
`KeyAction` — the gap was dispatch, where `main.rs::handle_layout_action`
implemented nine `LayoutAction` variants and routed the other twenty-six to a
`tracing::debug!` catch-all that swallowed them. That catch-all is gone: the
match is exhaustive over all 38 variants, and `tools/check-reachability.sh`
fails the build if a new one is ever swallowed again.

Every row's method is `visual-E2E`: each action must be driven through
`xdotool` against the real window and asserted by its observable effect.
`tests/e2e/func/keybindings-validation.sh` currently validates the binding
*table*; it must be extended to assert effects.

| Action | Subsystem | Verification method | Reachable from | Status |
| --- | --- | --- | --- | --- |
| `split_vertical` | Pane layout | visual-E2E | `main.rs::handle_layout_action` `SplitVertical` arm → `TerminalView::split_pane` → `PaneShell::split_focused_pane` (`tests/e2e/visual/pane-workspace-layout.sh` phase 1) | required |
| `split_horizontal` | Pane layout | visual-E2E | as `split_vertical`, with `SplitDirection::Vertical` | required |
| `close_pane` | Pane layout | visual-E2E | `TerminalView::close_pane` → `PaneShell::close_focused_pane`, falling back to `close_active_tab` on the last pane (`tests/e2e/visual/pane-workspace-layout.sh` phase 5) | required |
| `cycle_pane` | Pane layout | visual-E2E | `TerminalView::focus_next_pane` → `PaneShell::focus_next_pane` (`tests/e2e/visual/pane-workspace-layout.sh` phase 4) | required |
| `focus_left` | Pane layout | visual-E2E | `TerminalView::focus_pane` → `PaneShell::focus_pane_in_direction` (`tests/e2e/visual/pane-workspace-layout.sh` phase 3) | required |
| `focus_right` | Pane layout | visual-E2E | as `focus_left`, `FocusDirection::Right` | required |
| `focus_up` | Pane layout | visual-E2E | as `focus_left`, `FocusDirection::Up` | required |
| `focus_down` | Pane layout | visual-E2E | as `focus_left`, `FocusDirection::Down` | required |
| `equalize` | Pane layout | visual-E2E | `main.rs::handle_layout_action` `Equalize` arm → `TerminalView::equalize_layout` → `PaneShell::equalize_all` — the same handler behind the titlebar icon and the status-bar balance button | required |
| `workspace_split_vertical` | Workspace layout | visual-E2E | `TerminalView::split_workspace` → `PaneShell::split_workspace` → `WorkspaceTree::split_workspace` (`tests/e2e/visual/pane-workspace-layout.sh` phase 6) | required |
| `workspace_split_horizontal` | Workspace layout | visual-E2E | as `workspace_split_vertical`, with `SplitDirection::Vertical` | required |
| `workspace_focus_left` | Workspace layout | visual-E2E | `TerminalView::focus_workspace` → `PaneShell::focus_workspace_in_direction` (`tests/e2e/visual/pane-workspace-layout.sh` phase 7, on a rebound chord: openbox grabs the `ctrl+alt+arrow` default) | required |
| `workspace_focus_right` | Workspace layout | visual-E2E | as `workspace_focus_left`, `FocusDirection::Right` | required |
| `workspace_focus_up` | Workspace layout | visual-E2E | as `workspace_focus_left`, `FocusDirection::Up` | required |
| `workspace_focus_down` | Workspace layout | visual-E2E | as `workspace_focus_left`, `FocusDirection::Down` | required |
| `new_tab` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `NewTab` arm → `main.rs::create_tab` | required |
| `new_claude_tab` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `NewClaudeTab` arm → `ai_tab_command` | required |
| `new_claude_resume_tab` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `NewClaudeResumeTab` arm → `ai_tab_command` | required |
| `new_codex_tab` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `NewCodexTab` arm → `ai_tab_command` | required |
| `new_codex_resume_tab` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `NewCodexResumeTab` arm → `ai_tab_command` | required |
| `new_pi_tab` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `NewPiTab` arm → `main.rs::create_shell_tool_tab` → `create_tab` with `ShellTool::Pi`; launch-only, so no AI provider is tracked (`tests/e2e/visual/tab-window-chords.sh` phase 4) | required |
| `close_tab` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `CloseTab` arm → `close_active_tab`; the chord now outranks the close-dialog overlay via `translate_overlay_chord` (`tests/e2e/visual/tab-window-chords.sh` phase 1) | required |
| `next_tab` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `NextTab` arm → `TabSessions::focus_next` | required |
| `prev_tab` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `PrevTab` arm → `TabSessions::focus_prev` | required |
| `select_tab_1` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `SelectTab` arm → `TabSessions::select` | required |
| `select_tab_2` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `SelectTab` arm → `TabSessions::select` | required |
| `select_tab_3` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `SelectTab` arm → `TabSessions::select` | required |
| `select_tab_4` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `SelectTab` arm → `TabSessions::select` | required |
| `select_tab_5` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `SelectTab` arm → `TabSessions::select` | required |
| `select_tab_6` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `SelectTab` arm → `TabSessions::select` | required |
| `select_tab_7` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `SelectTab` arm → `TabSessions::select` | required |
| `select_tab_8` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `SelectTab` arm → `TabSessions::select` | required |
| `select_tab_9` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `SelectTab` arm → `TabSessions::select` | required |
| `new_window` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `NewWindow` arm → `open_new_window` → `start_window_backend` + `open_window` (`tests/e2e/visual/tab-window-chords.sh` phase 2) | required |
| `copy` | Clipboard | visual-E2E | `main.rs::handle_layout_action` `CopySelection` arm → `TerminalView::copy_selection` → `TerminalView::selection_copy_text` → `TerminalView::write_clipboard` → `clipboard::ArboardClipboard` (FU-8) — `tests/e2e/visual/clipboard-osc52.sh` | required |
| `paste` | Clipboard | visual-E2E | `main.rs::handle_layout_action` `PasteClipboard` arm → `TerminalView::paste_clipboard` → `TerminalView::request_paste` → `paste::PasteGate` → `TerminalView::deliver_paste` (FU-8) — `tests/e2e/visual/clipboard-osc52.sh`, confirmation gate in `tests/e2e/visual/paste-confirmation.sh` | required |
| `scroll_up` | Navigation | visual-E2E | `TerminalView::scroll_terminal` (bead .59) — `tests/e2e/visual/terminal-viewport.sh` | required |
| `scroll_down` | Navigation | visual-E2E | `TerminalView::scroll_terminal` (bead .59) | required |
| `scroll_top` | Navigation | visual-E2E | `TerminalView::scroll_terminal` (bead .59) | required |
| `scroll_bottom` | Navigation | visual-E2E | `TerminalView::scroll_terminal` (bead .59) — `tests/e2e/visual/terminal-viewport.sh` | required |
| `find` | Navigation | visual-E2E | `main.rs::TerminalView::dispatch_key_action` → `TerminalView::open_find_overlay` → `search::FindOverlayView` | required |
| `prompt_jump_up` | Navigation | visual-E2E | `main.rs::handle_layout_action` `PromptJumpUp` arm → `TerminalView::jump_to_prompt` → `TerminalView::jump_to_mark` → `session_lifecycle::PromptMarks::jump_target` (FU-7) — `tests/e2e/visual/prompt-marks.sh` | required |
| `prompt_jump_down` | Navigation | visual-E2E | as `prompt_jump_up`, with `JumpDirection::Down` | required |
| `jump_to_failure` | Navigation | visual-E2E | `main.rs::handle_layout_action` `JumpToFailure` arm → `TerminalView::jump_to_failure` → `TerminalView::jump_to_mark` → `session_lifecycle::PromptMarks::failure_target` (FU-7) — `tests/e2e/visual/prompt-marks.sh` | required |
| `zoom_in` | View and overlays | visual-E2E | `TerminalView::apply_zoom` (beads .59, .70) — `tests/e2e/visual/terminal-zoom.sh` | required |
| `zoom_out` | View and overlays | visual-E2E | `TerminalView::apply_zoom` (beads .59, .70) — `tests/e2e/visual/terminal-zoom.sh` | required |
| `zoom_reset` | View and overlays | visual-E2E | `TerminalView::apply_zoom` (beads .59, .70) — `tests/e2e/visual/terminal-zoom.sh` | required |
| `command_palette` | View and overlays | visual-E2E | `main.rs::handle_overlay_key` opens the overlay and the subscription on `CommandPaletteView` routes `CommandPaletteEvent::Execute` to `TerminalView::execute_palette_action`, so a palette row runs the same seam the winit client's automation used (FU-12) — `tests/e2e/visual/overlay-actions.sh` | required |
| `settings` | View and overlays | visual-E2E | `main.rs::TerminalView::dispatch_key_action` → `TerminalView::open_or_focus_settings` → `settings::open_settings_window` (bead .82) — also from the palette row and the titlebar gear; `tests/e2e/visual/settings-entry.sh` | required |
| `word_left` | Terminal shortcuts | visual-E2E (+ golden bytes) | `keybindings.rs::translate_key_action` → `KeyAction::Terminal` → `main.rs::send_key_bytes` | required |
| `word_right` | Terminal shortcuts | visual-E2E (+ golden bytes) | `keybindings.rs::translate_key_action` → `KeyAction::Terminal` → `main.rs::send_key_bytes` | required |
| `delete_word_backward` | Terminal shortcuts | visual-E2E (+ golden bytes) | `keybindings.rs::translate_key_action` → `KeyAction::Terminal` → `main.rs::send_key_bytes` | required |
| `delete_word_backward_ctrl` | Terminal shortcuts | visual-E2E (+ golden bytes) | `keybindings.rs::translate_key_action` → `KeyAction::Terminal` → `main.rs::send_key_bytes` | required |
| `delete_word_forward` | Terminal shortcuts | visual-E2E (+ golden bytes) | `keybindings.rs::translate_key_action` → `KeyAction::Terminal` → `main.rs::send_key_bytes` | required |
| `line_start` | Terminal shortcuts | visual-E2E (+ golden bytes) | `keybindings.rs::translate_key_action` → `KeyAction::Terminal` → `main.rs::send_key_bytes` | required |
| `line_end` | Terminal shortcuts | visual-E2E (+ golden bytes) | `keybindings.rs::translate_key_action` → `KeyAction::Terminal` → `main.rs::send_key_bytes` | required |

**Reachability:** 56 of 56 rows name a live-path symbol; 0 are unwired and 0
are missing. The audit's figure at `f56ef95` was 24, with 30 unwired and none
missing, because every action already parsed and translated and the whole gap
was dispatch.

`LayoutAction` variants are explicitly: `SplitVertical`, `SplitHorizontal`,
`ClosePane`, `FocusNext`, `FocusLeft`, `FocusRight`, `FocusUp`, `FocusDown`,
`Equalize`, `WorkspaceSplitVertical`, `WorkspaceSplitHorizontal`,
`WorkspaceFocusLeft`, `WorkspaceFocusRight`, `WorkspaceFocusUp`,
`WorkspaceFocusDown`, `NewTab`,
`NewClaudeTab`, `NewClaudeResumeTab`, `NewCodexTab`, `NewCodexResumeTab`,
`NewPiTab`,
`CloseTab`, `NextTab`, `PrevTab`, `SelectTab`, `NewWindow`, `CopySelection`,
`PasteClipboard`, `ScrollUp`, `ScrollDown`, `ScrollTop`, `ScrollBottom`,
`PromptJumpUp`, `PromptJumpDown`, `JumpToFailure`, `ZoomIn`, `ZoomOut`, and
`ZoomReset` — all 38 are executed by `main.rs::handle_layout_action`, whose
match is exhaustive with no catch-all arm and whose 38/38 figure
`tools/check-reachability.sh` ratchets. The 56 rows above cover more ground
than 38 variants because ten of them are not `LayoutAction` at all — the seven
terminal shortcuts, `find`, `settings` and `command_palette` — while the nine
`select_tab_N` bindings collapse onto the single index-carrying `SelectTab`.
`KeyAction` variants are `Terminal` (reachable via `main.rs::send_key_bytes`),
`Layout` (per the above), `OpenCommandPalette` (reachable via
`main.rs::handle_overlay_key`), `OpenSettings` (reachable via
`main.rs::TerminalView::open_or_focus_settings`), and `OpenFind` (reachable via
`main.rs::TerminalView::open_find_overlay`).

## Rendering and window checklist

These spike-resolved rendering and native-window requirements preserve the
legacy terminal's output and platform behavior through the GPUI cutover.

`terminal_element.rs::TerminalElement::paint` resolves every visible cell
property on the live paint call: `terminal.rs::Content` carries a `Cell` per
grid position (character, raw `vte::ansi::Color` fg/bg, alacritty `Flags`), and
one `gpui::canvas` paints background quads, then the `box_drawing::mask_quads`
overlay, then `shape_line` runs. FU-1 (bead `.63`) landed that rebuild, so the
first three rows below are reachable rather than blocked.

| Surface | Required behavior | Verification method | Reachable from | Status |
| --- | --- | --- | --- | --- |
| Box drawing | U+2500–U+259F cells bypass text shaping and use the existing procedural alpha-mask rasterizer through a `TerminalElement` paint-quad overlay after backgrounds and before text. | visual-E2E | `main` → `open_window` → `TerminalView::render` → `TerminalElement::paint` → `TerminalElement::paint_grid` → `paint_box_drawing` → `box_drawing::mask_quads` | required |
| Font fallback | Every terminal run uses `FontFallbacks::from_fonts` with `Symbols Nerd Font Mono`, `Symbols Nerd Font`, `Nerd Font Symbols Mono`, and `Nerd Font Symbols` before existing generic fallbacks; `Unifont Sample` remains excluded. | visual-E2E | `TerminalElement::paint_grid` → `FontVariants::new` → `GridFont::font_for` → `GridFont::fallbacks`, carried on every `TextRun` handed to `shape_line`. The chain resolves because `fonts::register_embedded_fonts` registers an embedded `Symbols Nerd Font Mono` whose cmap maps `U+006D` (see `tools/patch-nerd-symbols-font.py`), surviving gpui `f96212f` `CosmicTextSystem::load_family`'s `'m'`-glyph face eviction that silently dropped every stock symbols-only font. Live capture: `U+F09B`/`U+F121` render as the octocat/code icons, not `Unifont Sample` hex boxes | required |
| Ligatures | `appearance.ligatures` keeps its semantics: same-style runs call `shape_line` with `Some(cell_width)` and disable `calt` only when false, without drifting later cell origins. | visual-E2E | `ConfigRuntime` → `GridFont::from_appearance` → `GridFont::features` on every run; `paint_row_text` shapes each row with `Some(cell_width)` | required |
| Opacity | `appearance.opacity` is clamped to `0.0..=1.0`; Wayland and composited X11 repaint alpha-aware terminal and chrome backgrounds live on a transparent surface, without restart. | manual | `main.rs::open_window` opens the surface with `WindowBackgroundAppearance::Transparent`, `TerminalView::apply_opacity_change` re-clamps through `opacity::clamp_opacity` on every config reload, and `TerminalView::render` paints through `opacity::surface` / `opacity::opaque_slot` (FU-4, bead `.56`); bead `.53` pixel-confirmed live bleed-through and restart-free reload against the running client | required |
| Command-mark scrollbar | Each pane paints a non-reserving overlay scrollbar on its right edge with command-status ticks anchored to absolute scrollback rows, which shift when the server trims scrollback. | visual-E2E | `TerminalView::render_panes` → `TerminalView::scrollbar_paint` → `TerminalElement::with_scrollbar` → `TerminalElement::paint_scrollbar` → `scrollbar::build_scrollbar_render` (bead `.88`); pointer half via `TerminalView::press_grid` → `press_scrollbar` / `update_scrollbar_hover`; oracle `tests/e2e/visual/scrollbar.sh` | required |
| X11 focus guard | The guard reads GPUI's `RawWindowHandle::Xcb` XID and compares it directly with `_NET_ACTIVE_WINDOW`; non-X11 backends do not enable the guard. | scripted-E2E | `main.rs::open_window` → `TerminalView::new` (FU-15) — starts the guard from the live `Window`, polls it from `drive_x11_focus_polls`, clears the debounce in `TerminalView::on_activation`, and gates the key path in `TerminalView::compositor_overlay_active`; scripted oracle `tests/e2e/visual/x11-focus-guard.sh` | required |

**Reachability:** 6 of the 6 rows name a live-path symbol; 0 are unwired and 0
are missing — the three `TerminalElement::paint` rows (FU-1), Opacity (FU-4,
beads `.56` and `.53`), the command-mark scrollbar (bead `.88`), and the X11
focus guard (FU-15). The audit's figure at `f56ef95` was 0 of 5, with 3 unwired
and 2 missing; the scrollbar row was added afterwards with bead `.88`.

## Spec behaviour requirements (28)

These rows are derived from [`spec.md`](spec.md)'s requirement register rather
than from the legacy client's IPC surface: every acceptance criterion and
porting obligation that no message, keybinding, rendering or removed-key row
already carries gets a row here. The "spec.md" column names the register ids the
row satisfies, and `tools/check-parity-inventory.sh` fails when a register id
has no carrying row.

This table exists because the 2026-07-27 launch gate found nine spec
requirements — mouse reporting, mouse-wheel scrolling, IME composition,
cold-restart restore, the command-mark scrollbar, window geometry persistence,
the desktop notification dispatcher, server lifecycle management, and file
drag-and-drop — that had never been tabulated at all. A requirement with no row
is scored by no oracle, so the reachable-row count measured the tabulated subset
rather than parity. Enumerating the register closed that gap and surfaced four
further requirements that are not reachable today.

| Requirement | spec.md | Surface | Verification method | Reachable from | Status |
| --- | --- | --- | --- | --- | --- |
| Mouse reporting | `US1-2` | terminal pointer input | visual-E2E | `main.rs::TerminalView::forward_mouse_press` / `forward_mouse_release` / `forward_mouse_motion` → `mouse_reporting::encode_mouse_press` / `encode_mouse_release` / `encode_mouse_motion` → `main.rs::send_key_bytes`, gated by `mouse_reporting::should_report_mouse_motion` against the pane's live DEC modes — `tests/e2e/visual/mouse-reporting.sh` | required |
| Mouse-wheel and touchpad scrolling | `US1-4`, `US3-7` | wheel routing and viewport scroll | visual-E2E | `main.rs::TerminalView::scroll_wheel` → `mouse_reporting::wheel_lines` + `mouse_reporting::wheel_action`, which routes each notch to `encode_mouse_scroll` (tracking app), `alternate_scroll_keys` (mode 1007 alt screen) or `TerminalView::scroll_terminal` (client viewport) — `tests/e2e/visual/mouse-reporting.sh`, `tests/e2e/visual/terminal-viewport.sh` | required |
| Selection and copy-on-select | `US1-3` | cell/word/line selection | visual-E2E | `main.rs::TerminalView::press_grid` → `selection::SelectionMode` / `selection::SelectionSpan` → `TerminalView::selection_copy_text` → `TerminalView::write_clipboard` → `clipboard::ArboardClipboard` — `tests/e2e/visual/clipboard-osc52.sh` | required |
| Smart-selection actions | `US1-3` | context-menu smart actions | visual-E2E | `main.rs::TerminalView::open_context_menu` → `smart_selection::CompiledSmartSelection` → `smart_selection::ResolvedSmartSelectionAction` → `main.rs::TerminalView::dispatch_context_menu_action` — `tests/e2e/visual/overlays.sh` | required |
| URL, path, and OSC 8 hyperlink handling | `US1-3` | Ctrl+click open, hover tooltip, scheme gate | visual-E2E | `main.rs::TerminalView::dispatch_context_menu_action` and `TerminalView::route_osc8_activation` → `url_detect::is_allowed_scheme` → `url_detect::open_url` / `url_detect::open_path`, with a rejected scheme raised as `dialog::DisallowedSchemeDialog` and the hover label painted by `tooltip::tooltip_element` — `tests/e2e/visual/overlays.sh` | required |
| Split-scroll | `US1-4` | pinned live bottom in AI panes | visual-E2E | `main.rs::TerminalView::sync_split_scroll` → `split_scroll::SplitScrollState`, re-armed per pane by `TerminalView::set_split_scroll_eligibility` — `tests/e2e/visual/terminal-viewport.sh` | required |
| IME preedit composition | `US1-6` | Wayland and X11 composition | visual-E2E | `main.rs::TerminalView::start_ime` registers `preedit::Ime` as the window's input handler, `TerminalView::sync_ime` paints the in-flight composition over the grid, and `TerminalView::clear_preedit` retires it on focus loss or any encoded keystroke — `tests/e2e/visual/ime-preedit.sh` | required |
| Sync-update frames (CSI ?2026) | `US1-7`, `PO-6` | tear-free output bursts | scripted-E2E | `main.rs::forward_output` → `main.rs::forward_inbound` → `main.rs::spawn_drain` → `main.rs::apply_pane_op` → `sync_frames::SyncFrameQueue`, committed by `sync_frames::drain_all_committed` and expiry-flushed by `main.rs::run_sync_expiry` → `main.rs::flush_expired_sync` | required |
| File drag-and-drop | `US1-8` | dropped path insertion | visual-E2E | `main.rs::TerminalView::handle_dropped_paths`, registered as the window root's `ExternalPaths` drop handler → `drag_drop::dropped_path_insertion` quoted for the shell `ChromeMetadata` recorded for that session → `send_key_bytes` — `tests/e2e/visual/drag-drop.sh` | required |
| Cold-restart restore | `US2-3` | window, workspace, tab and pane replay | visual-E2E | `main.rs::ColdStart::resolve` → `restore_state::RestoreStore::claim_first_window` plus the `--restore-child` fan-out, replayed by `main.rs::TerminalView::poll_restore` → `restore_replay::prepare_replay`; the snapshot is written back by `TerminalView::flush_snapshot_now` → `RestoreStore::save_window` / `RestoreStore::upsert_index` — `tests/e2e/visual/cold-restart.sh` | required |
| Server-upgrade reattach | `US2-4` | zero-downtime `--upgrade` handoff | scripted-E2E | `main.rs::start_ipc_thread` → `main.rs::supervise_connection` → `main.rs::run_connection`, which redials local streams with bounded backoff and writes `Hello` / `ListSessions` before resuming the shared writer; `main.rs::on_session_list` rebuilds the topology — `tests/e2e/visual/server-upgrade-reattach.sh` drives a live server upgrade without restarting the GPUI process. | required |
| Colour emoji in the grid | `US3-1` | colour emoji glyphs | visual-E2E | `terminal_element.rs::TerminalElement::paint_grid` → `GridFont::fallbacks`, which names `Noto Color Emoji` after the Nerd Font entries on every `TextRun` handed to `shape_line` — `tests/e2e/visual/color-emoji.sh` | required |
| Cell text decorations | `US3-4` | underline, undercurl, strikethrough | visual-E2E | `terminal_element.rs::TerminalElement::paint_grid` builds each `TextRun` with `UnderlineStyle { wavy: Flags::UNDERCURL }` from `Flags::ALL_UNDERLINES` and `StrikethroughStyle` from `Flags::STRIKEOUT`. Fidelity gap flagged: gpui's `UnderlineStyle` has no double-underline variant, so `Flags::DOUBLE_UNDERLINE` currently paints a single rule | required |
| Overlay chrome polish | `US3-5` | rounded corners, drop shadow, hover/pressed | visual-E2E | `command_palette::CommandPaletteView::render`, `dialog::DialogView::render`, `context_menu::ContextMenuView::render` and `tooltip::tooltip_element`, each applying `.rounded_*()` + `.shadow_*()` on the live overlay layer — `tests/e2e/visual/overlays.sh`, `tests/e2e/visual/dialogs.sh` | required |
| Overlay and focus animations | `US3-6` | ≤150 ms interruptible easing | visual-E2E | `main.rs::main` and `main.rs::run_settings` → `animation::AnimationSettings::resolve` → `AnimationSettings::apply_to_app`, which installs the reduce-motion-aware durations every overlay animates against | required |
| Custom titlebar with integrated tab bar | `US3-8` | window chrome | visual-E2E | `main.rs::TerminalView::build_titlebar` → `titlebar::TitlebarView`, rendered above the grid with `window_chrome` supplying the client-side decoration geometry — `tests/e2e/visual/titlebar.sh` | required |
| Pane dividers and drag-resize | `US3-10` | divider paint and split resize | visual-E2E | `main.rs::TerminalView::render_dividers` → `PaneShell::dividers` → `divider::collect_dividers`; the grid pointer path claims the divider hit band, maps motion through `divider::drag_ratio`, then calls `PaneShell::set_pane_ratio` and republishes both grids — `tests/e2e/visual/pane-workspace-layout.sh` phase 5 | required |
| Focused pane and workspace accent border | `US3-10` | focus indication | visual-E2E | `main.rs::TerminalView::render_panes` → `main.rs::pane_border` → the pane element's border colour, using the owning region's accent when focused. The shared strip math in `focus_border::border_edges` also reaches the live AI border through `ai_indicator::pane_border_edges`. | required |
| AI indicator borders and tab tint | `US4-1` | pulsing borders, tab tint, stale-clear | visual-E2E | `main.rs::on_ai_message` → `AiStateTracker::{update,remember_provider}`; `on_pane_output_message` → `note_activity`; `TerminalView::{tick_ai_animation,poll_window_lifecycle,on_key_down}` → `{tick,needs_animation,clear_stale_processing,clear_attention_states}`; `render_panes` aggregates `workspace_border_color` and paints `pane_border_edges`; `sync_tabs` feeds `tab_indicator_color` to `TitlebarView` — `tests/e2e/visual/ai-indicator.sh` | required |
| Prompt bar and tab context meter | `US4-1` | elapsed timer, context meter, dismiss/copy, `%` suffix | visual-E2E | `main.rs::TerminalView::build_prompt_model` → `prompt_bar::build_model` → `prompt_bar::render`; the tab suffix via `main.rs::TerminalView::sync_tabs` → `tab_bar::context_suffix`, both fed by `ai_indicator::AiStateTracker::context_for` | required |
| Workspace accent colours and badges | `US4-3` | region accents and status badges | visual-E2E | `main.rs::TerminalView::next_region_accent` → `PaneShell::split_workspace`, painted through `main.rs::pane_border`; the tmux/session/share badges via `status_bar::build_model` — `tests/e2e/visual/workspace-split.sh` | required |
| Remote connect picker overlay | `US4-4` | peer picker UI | visual-E2E | `main.rs::TerminalView::open_remote_connect` → `remote::RemoteConnect` → `remote_picker::remote_picker_overlay`; the picker receives fresh peer snapshots through `TerminalView::sync_remote_connect`, probes a selected tailnet peer with `RemoteHandshake` + `ListWindows`, then launches the chosen window with its dial environment — `tests/e2e/visual/remote-control.sh` | required |
| Status bar segments | `US4-5` | full segment set | visual-E2E | `main.rs::TerminalView::build_status_model` → `status_bar::build_model` → `status_bar::render`, with the CPU/mem/GPU/net sparklines fed by `sys_stats::SystemStatsCollector` — `tests/e2e/visual/window-chrome-bands.sh` | required |
| xterm-256 palette | `PO-1` | indexed colour resolution | golden | `main.rs` builds one `color::TerminalColors` per theme, which owns `palette::ColorPalette`; `TerminalColors::resolve_color` resolves every indexed cell on the paint path. The reachability ratchet follows that transitive `main → color → palette` import chain. | required |
| Colour semantics (bold→bright, DIM, sRGB) | `PO-3` | per-cell colour resolution | golden | `terminal_element.rs::TerminalElement::paint_grid` → `color::TerminalColors::resolve_cell_colors` → `color::bold_to_bright`, `color::apply_dim`, `color::srgb_to_linear_rgba`, `color::boost_srgb_brightness` | required |
| Window geometry persistence | `PO-8` | size and position across restarts | visual-E2E | `main.rs::TerminalView::start_geometry_tracking` (window-bounds subscription) → `TerminalView::capture_geometry` → `window_state::geometry_from_bounds`, debounce-flushed by `TerminalView::flush_geometry_now` → `window_state::WindowRegistry::save`; read back by `main.rs::open_window` → `window_state::window_bounds_for` — `tests/e2e/visual/cold-restart.sh` | required |
| Desktop notification dispatcher | `PO-9` | AI attention notifications | visual-E2E | `main.rs::TerminalView::start_notifications` → `notification_dispatcher::spawn_dispatcher` (the one D-Bus connection), gated by `notifications::NotificationCenter::on_ai_state_changed` against the live config and focus position; a reported click routes back to select the session's tab and raise the window — `tests/e2e/visual/notifications.sh` | required |
| Server lifecycle management | `PO-10` | autostart, stale socket, staleness check | scripted-E2E | `main.rs::run_local_connection` → `server_lifecycle::connect_or_start_server`, which names a missing socket apart from a stale one and carries the diagnosis into the status line, then `server_lifecycle::connected_server_staleness` holds the connected server up against the installed binary — `tests/e2e/visual/server-lifecycle.sh` | required |

**Reachability:** 28 of 28 rows name a live-path symbol; 0 are unwired and 0 are
missing. None of the original five had a row
before 2026-07-27, so none was scored by the gate.

## Removed configuration keys

These legacy appearance keys must deserialize harmlessly at cutover but have no
GPUI behavior. The table is intentionally narrow: only splash and bespoke
renderer-pipeline controls are removed. The spikes retain
`appearance.ligatures` and `appearance.opacity` with their current semantics,
so neither belongs in this table.

These are the only rows that keep `gpui-test`: they assert the *absence* of
behaviour on the live load path (`config.rs::ConfigRuntime::start` →
`scribe-common/src/config.rs::load_config`, the same function `main()` uses),
which a headless load-path test genuinely covers.

| Legacy TOML key | Reason removed | Load behavior | Verification method | Reachable from | Status |
| --- | --- | --- | --- | --- | --- |
| `appearance.splash` | GPUI cutover deletes the splash screen. | Silently ignored. | gpui-test | `config.rs::ConfigRuntime::start` → `scribe-common/src/config.rs::load_config` — no struct field, so serde drops the key | required |
| `appearance.splash_duration_ms` | GPUI cutover deletes splash timing. | Silently ignored. | gpui-test | `config.rs::ConfigRuntime::start` → `load_config` — no struct field | required |
| `appearance.scrollbar_width` | Bespoke pipeline hover/geometry constant. | Silently ignored. | gpui-test | `config.rs::ConfigRuntime::start` → `load_config` — field deserializes, never read by the client | required |
| `appearance.scrollbar_color` | Bespoke renderer scrollbar colour override. | Silently ignored. | gpui-test | `config.rs::ConfigRuntime::start` → `load_config` — field deserializes, never read | required |
| `appearance.prompt_bar_second_row_bg` | Bespoke prompt-bar pipeline colour override. | Silently ignored. | gpui-test | `config.rs::ConfigRuntime::start` → `load_config` — never read; the prompt bar reads `ChromeColors` | required |
| `appearance.prompt_bar_first_row_bg` | Bespoke prompt-bar pipeline colour override. | Silently ignored. | gpui-test | `config.rs::ConfigRuntime::start` → `load_config` — never read; the prompt bar reads `ChromeColors` | required |
| `appearance.prompt_bar_text` | Bespoke prompt-bar pipeline colour override. | Silently ignored. | gpui-test | `config.rs::ConfigRuntime::start` → `load_config` — never read; the prompt bar reads `ChromeColors` | required |
| `appearance.prompt_bar_icon_first` | Bespoke prompt-bar pipeline colour override. | Silently ignored. | gpui-test | `config.rs::ConfigRuntime::start` → `load_config` — never read; the prompt bar reads `ChromeColors` | required |
| `appearance.prompt_bar_icon_latest` | Bespoke prompt-bar pipeline colour override. | Silently ignored. | gpui-test | `config.rs::ConfigRuntime::start` → `load_config` — never read; the prompt bar reads `ChromeColors` | required |

**Reachability:** 9 of 9 rows name a live-path symbol; 0 are unwired and 0 are
missing.

## Reachability roll-up

Counts are recomputed from the marker cells in the five tables above by
`tools/check-parity-inventory.sh`, which fails if any number here disagrees
with them. They are the launch gate's metric — not the unit-test count.

| Table | Rows | Reachable | Unwired | Missing |
| --- | --- | --- | --- | --- |
| Client messages | 51 | 49 | 2 | 0 |
| Server messages | 68 | 65 | 3 | 0 |
| Input and keybinding actions | 56 | 56 | 0 | 0 |
| Rendering and window | 6 | 6 | 0 | 0 |
| Spec behaviour requirements | 28 | 28 | 0 | 0 |
| Removed configuration keys | 9 | 9 | 0 | 0 |
| **Total** | **218** | **213** | **5** | **0** |

Excluding the nine removed-configuration-key rows (satisfied by *absence* of
behaviour), the user-facing parity surface is **209 rows, of which 204 are
reachable (98%)** and 5 are not. **1 of those 209** rows — `HookEvent`, whose
named symbol is `scribe-hook-helper`'s `main` — is out-of-client by design, so
the in-client figure is **203 of 209**.

At the `f56ef95` audit baseline the same surface was 164 rows with 51 reachable
(31%), against a roll-up total of 173 rows and 60 reachable; the sixth
rendering row (the command-mark scrollbar) was added later with bead `.88`.
Fix units FU-1..FU-23 are defined in
[`reachability-audit.md`](reachability-audit.md); the plan's phases were
sequenced around them, P0 first. Every fix unit that owns a row in the tables
above has landed — that is what a zero unwired/missing column means — so the
audit's per-FU notes are now history rather than a work queue.

## Spec requirement coverage

Every register id in [`spec.md`](spec.md) maps to the row or rows that carry it.
`tools/check-parity-inventory.sh` fails when an id is absent, duplicated,
unknown, or points at a row that no table contains — that check is what makes
the reachable-row total a parity measure instead of a measure of whichever
surface happened to be tabulated.

A carrier is written as a row name in backticks, as `§` plus a table label when
a whole table carries the requirement, or as `not a parity row` for the tree,
licensing and CI requirements that are gated by the launch-gate checklist rather
than by a reachable client symbol.

| spec.md requirement | Carried by |
| --- | --- |
| `US1-1` | §Input and keybinding actions (the seven terminal shortcuts and their golden fixtures) plus `KeyInput` |
| `US1-2` | `Mouse reporting` |
| `US1-3` | `Selection and copy-on-select`, `Smart-selection actions`, `URL, path, and OSC 8 hyperlink handling`, `find`, `SearchRequest`, `SearchResults` |
| `US1-4` | `TrimScrollback`, `scroll_up`, `scroll_bottom`, `Mouse-wheel and touchpad scrolling`, `Split-scroll` |
| `US1-5` | `paste`, `ClipboardPromptRequest`, `ClipboardPromptResponse` |
| `US1-6` | `IME preedit composition` |
| `US1-7` | `Sync-update frames (CSI ?2026)` |
| `US1-8` | `File drag-and-drop` |
| `US2-1` | `SessionReplay` |
| `US2-2` | `ScreenSnapshot`, `SessionList`, `Hello`, `Welcome` |
| `US2-3` | `Cold-restart restore` |
| `US2-4` | `Server-upgrade reattach` |
| `US3-1` | `Font fallback`, `Colour emoji in the grid` |
| `US3-2` | `Box drawing` |
| `US3-3` | `Ligatures` |
| `US3-4` | `Cell text decorations` |
| `US3-5` | `Overlay chrome polish` |
| `US3-6` | `Overlay and focus animations` |
| `US3-7` | `Mouse-wheel and touchpad scrolling` |
| `US3-8` | `Custom titlebar with integrated tab bar`, `Opacity` |
| `US3-9` | `X11 focus guard` |
| `US3-10` | `Pane dividers and drag-resize`, `Focused pane and workspace accent border` |
| `US4-1` | `AI indicator borders and tab tint`, `Prompt bar and tab context meter`, `AiStateChanged`, `TaskLabelChanged` |
| `US4-2` | `Command-mark scrollbar` |
| `US4-3` | `Workspace accent colours and badges`, `workspace_split_vertical` |
| `US4-4` | `Remote connect picker overlay`, `LanApprovalRequest`, `WindowTakenOver`, `ShareRoster`, `ControlRequested` |
| `US4-5` | `Status bar segments` |
| `US5-1` | not a parity row — deletion sweep, gated by bead `scribe-38e.45` |
| `US5-2` | not a parity row — deletion sweep, gated by bead `scribe-38e.45`; the ported feature set is covered by the settings-window rows |
| `US5-3` | not a parity row — dead-code and dependency audit, gated by bead `scribe-38e.46` and `tools/check-reachability.sh` |
| `US5-4` | not a parity row as a whole; its named logic is carried by `xterm-256 palette`, `Box drawing` and `Colour semantics (bold→bright, DIM, sRGB)` |
| `US5-5` | not a parity row — `lat check` runs in CI, gated by bead `scribe-38e.47` |
| `US5-6` | not a parity row — licensing, gated by the launch-gate checklist |
| `US6-1` | not a parity row — `scribe-test` is a frozen server-only suite |
| `US6-2` | not a parity row — harness capability; every `visual-E2E` row depends on it |
| `US6-3` | not a parity row — headless logic coverage, explicitly insufficient on its own |
| `US6-4` | not a parity row — the gate rule this document implements; enforced by `tools/check-reachability.sh` and `tools/check-parity-inventory.sh` |
| `PO-1` | `xterm-256 palette` |
| `PO-2` | `Box drawing` |
| `PO-3` | `Colour semantics (bold→bright, DIM, sRGB)` |
| `PO-4` | `Font fallback` |
| `PO-5` | `Command-mark scrollbar` |
| `PO-6` | `Sync-update frames (CSI ?2026)` |
| `PO-7` | `X11 focus guard` |
| `PO-8` | `Window geometry persistence` |
| `PO-9` | `Desktop notification dispatcher` |
| `PO-10` | `Server lifecycle management` |
| `PO-11` | `RemoteHandshake`, `LanHello`, `GetLanDialIdentity` |

## LAN and sharing boundary

Feature 015 is present in `fd04540` (`feat: remote window control and
multi-machine sharing`), and the rows above now name the **GPUI** client's
dispatch, not the legacy one they were originally written against.
`lan_dial.rs::handshake` performs the LAN handshake during the connection
preamble and `main.rs::on_lan_message` folds its live-reader answers into
`lan.rs::LanChrome`; `main.rs::on_remote_message` does the same for the tailnet
family through `remote_chrome::RemoteChrome`; and
`main.rs::dispatch_share_message` drives `share.rs::ShareChrome` for the roster
and control UI. `ControlRequest` remains a serializable protocol alias because
the server handles it as `ControlClaim`; the client deliberately emits only the
latter, which is why its row names that substitute rather than a sender.

Three fix units closed this family: FU-19 put `share.rs` on the live path
(roster, control notices, claim/grant frames, verified by
`tests/e2e/visual/share-control.sh`), FU-17 and FU-18 wired the feature-014 LAN
dial, approval prompt and trust settings (`tests/e2e/visual/lan-approval.sh`,
`tests/e2e/visual/settings-trust.sh`), and FU-16 wired the feature-013 tailnet
handshake, peer/environment probe, displaced banner and automation round trip
(`tests/e2e/visual/remote-control.sh`). FU-28 now adds the GPUI presentation:
`TerminalView::open_remote_connect` and `sync_remote_connect` feed
`remote_picker::remote_picker_overlay`, whose selected-tailnet-peer probe is
asserted on the stand-in peer's real TCP wire.
