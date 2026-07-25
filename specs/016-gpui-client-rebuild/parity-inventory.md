# GPUI client parity inventory

This committed oracle enumerates the frozen IPC, input, and UI surface the GPUI
client must preserve before cutover. It is derived from the legacy client and
`scribe-common` at the 016 planning baseline.

Every row carries a **Reachable from** cell naming the live-path symbol that
must call the feature. The cells were populated from the per-row evidence in
[`reachability-audit.md`](reachability-audit.md) (audited at `f56ef95`). A row
whose cell is a `— (unwired)` / `— (missing)` marker **cannot be marked done**,
regardless of how many tests pass.

## Definition of done

A parity row is done only when **both** hold:

1. **Reachable.** Its "Reachable from" cell names a real symbol on one of the
   four live paths of the shipped binary — the render/paint path
   (`main.rs::open_window` → `TerminalView::render` → `TerminalElement::paint`),
   the key path (`main.rs::handle_overlay_key` → `handle_binding` →
   `on_key_down`), outbound IPC (`ipc_bridge::IpcSink`, plus the raw sends in
   `main.rs::run_connection`), or inbound IPC (`main.rs::run_reader`). The
   settings window's `settings/window.rs::SettingsWindow::run_action` is a
   fifth, narrower entry point reachable only via `scribe-client-gpui
   --settings`; rows resting on it are flagged inline because they are not
   reachable from the terminal window.
2. **Verified against the running app.** Its verification method (below, as
   upgraded) passes while driving the real client, not a directly constructed
   entity.

A green `#[gpui::test]` is necessary but never sufficient for a user-facing
row: a headless entity test passes identically whether or not the application
constructs the entity. That failure mode is what the reachability audit found
across 113 of 164 user-facing rows.

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
did not — the `manual` Bell and Opacity rows, and the `golden`/`visual-E2E`
rows that were never headless-only — the method is unchanged, because those
oracles already require the running app and so satisfy the principle without
an upgrade.

## Client messages (46 sent)

Every `ClientMessage` variant from `crates/scribe-common/src/protocol.rs` must
remain serializable and be emitted by the corresponding GPUI interaction.

| Variant | Surface | Verification method | Reachable from | Status |
| --- | --- | --- | --- | --- |
| `KeyInput` | terminal input | golden | `main.rs::send_key_bytes` → `ipc_bridge::IpcSink::key_input` | required |
| `Resize` | terminal resize | scripted-E2E | `main.rs::report_cell_metrics`, `main.rs::attach_session` → `IpcSink::resize` | required |
| `CreateSession` | tabs, panes, and AI tabs | scripted-E2E | `main.rs::create_tab` → `IpcSink::create_session` | required |
| `CloseSession` | pane/tab close | scripted-E2E | `main.rs::close_active_tab` → `IpcSink::close_session` | required |
| `CreateWorkspace` | workspace creation | scripted-E2E | — (missing, FU-6) | required |
| `CloseWorkspace` | workspace close | scripted-E2E | — (missing, FU-6) | required |
| `MoveSession` | session relocation | scripted-E2E | — (missing, FU-6) | required |
| `Subscribe` | session stream subscription | scripted-E2E | `main.rs::attach_session` / `TerminalView::attach` → `IpcSink::subscribe` | required |
| `RequestSnapshot` | snapshot tooling | scripted-E2E | `main.rs::report_cell_metrics` / `forward_replay` → `IpcSink::request_snapshot` | required |
| `ListSessions` | startup/reconnect | scripted-E2E | `main.rs::run_connection` (sent on every connect) | required |
| `AttachSessions` | reconnect restore | scripted-E2E | `main.rs::attach_session` → `IpcSink::attach_sessions` | required |
| `ConfigReloaded` | live config reload | scripted-E2E | `main.rs::apply_config_reload` → `IpcSink::config_reloaded` | required |
| `ReportWorkspaceTree` | layout persistence | scripted-E2E | — (unwired, FU-6) — built only in `workspace_tree.rs`, outside `main.rs`'s import closure | required |
| `SearchRequest` | find overlay | scripted-E2E | — (missing, FU-9) | required |
| `WorkspaceNotesGet` | workspace notes | scripted-E2E | `main.rs::open_workspace_notes_modal` → `IpcSink::workspace_notes_get` — **degenerate** (demo chord, fabricated `WorkspaceId`, reply dropped by the reader catch-all); FU-21 | required |
| `WorkspaceNotesMutate` | workspace notes | scripted-E2E | `main.rs::route_workspace_notes_action` → `IpcSink::workspace_notes_mutate` — same demo caveat; FU-21 | required |
| `Hello` | registration/adoption | scripted-E2E | `main.rs::run_connection` (sent on every connect) | required |
| `CloseWindow` | close dialog | scripted-E2E | `main.rs::TerminalView::route_close_action` → `ipc_bridge::IpcSink::close_window`, from the close dialog the WM close request and the quit chord raise | required |
| `QuitAll` | quit-all dialog | scripted-E2E | `main.rs::TerminalView::route_close_action` → `ipc_bridge::IpcSink::quit_all`, from the same close dialog | required |
| `TriggerUpdate` | update dialog | scripted-E2E | `main.rs::TerminalView::route_update_action` → `ipc_bridge::IpcSink::trigger_update`, from the status-bar CTA's confirmation | required |
| `DismissUpdate` | update dialog | scripted-E2E | `main.rs::TerminalView::route_update_action` → `ipc_bridge::IpcSink::dismiss_update`, from the status-bar CTA's confirmation | required |
| `CheckForUpdates` | release settings | scripted-E2E | `settings/window.rs::SettingsWindow::run_action` — `--settings` window only, not the terminal window | required |
| `ListReleases` | release settings | scripted-E2E | `settings/window.rs::SettingsWindow::run_action` — `--settings` window only | required |
| `ListWindows` | window management | scripted-E2E | `main.rs::TerminalView::poll_window_list` → `ipc_bridge::IpcSink::list_windows`, on the lifecycle tick while `remote.enabled` | required |
| `DispatchAction` | remote automation | scripted-E2E | — (missing, FU-16) — `scribe-cli` is the only sender | required |
| `FocusChanged` | focus reporting | scripted-E2E | `main.rs::TerminalView::report_focus` → `ipc_bridge::IpcSink::focus_changed`, from the window activation observer and the pane-focus reconciliation | required |
| `HookEvent` | hook helper ingress | scripted-E2E | `crates/scribe-hook-helper/src/main.rs::main` — out-of-client by design | required |
| `EnvPreflight` | environment persistence | scripted-E2E | `settings/window.rs::SettingsWindow::run_action` (`action.env_preflight`) and `SettingsWindow::enable_env_persistence`, the toggle's gated ON transition — `--settings` window only | required |
| `ClipboardPromptResponse` | OSC 52 prompt | scripted-E2E | — (unwired, FU-8) — `clipboard.rs` outside the import closure | required |
| `ClipboardBridgeReadReply` | OSC 52 bridge | scripted-E2E | — (unwired, FU-8) — `clipboard.rs` outside the import closure | required |
| `RemoteHandshake` | tailnet connect | scripted-E2E | — (unwired, FU-16) — `remote_handshake.rs` outside the import closure | required |
| `ListRemotePeers` | remote connect picker | scripted-E2E | — (missing, FU-16) | required |
| `GetRemoteEnv` | remote settings | scripted-E2E | — (unwired, FU-16) — `settings/server_action.rs::request_remote_env` has no caller | required |
| `LanHello` | mTLS LAN-dial preamble before session attachment | scripted-E2E | — (missing, FU-17) | required |
| `LanApprovalDecision` | owner-side fingerprint approval overlay | scripted-E2E | — (unwired, FU-17) — `lan_approval.rs` outside the import closure | required |
| `ListLanPeers` | merged Local network source in remote connect picker | scripted-E2E | — (missing, FU-17) | required |
| `ListTrustedDevices` | Remote settings trusted-device list | scripted-E2E | `settings/window.rs::SettingsWindow::run_action` → `SettingsWindow::refresh_trust` — `--settings` window only | required |
| `RevokeTrustedDevice` | Remote settings device-revocation action | scripted-E2E | `settings/window.rs::SettingsWindow::run_action`, per approved-device row — `--settings` window only | required |
| `ListTrustedNetworks` | Remote settings trusted-network list | scripted-E2E | `settings/window.rs::SettingsWindow::run_action` → `SettingsWindow::refresh_trust` — `--settings` window only | required |
| `AddCurrentNetworkTrusted` | Remote settings trust-current-network action | scripted-E2E | `settings/window.rs::SettingsWindow::run_action` (`action.add_current_network`) — `--settings` window only | required |
| `RemoveTrustedNetwork` | Remote settings trusted-network removal action | scripted-E2E | `settings/window.rs::SettingsWindow::run_action`, per trusted-network row — `--settings` window only | required |
| `GetLanEnv` | Remote settings LAN listener/environment summary | scripted-E2E | `settings/window.rs::SettingsWindow::refresh_trust` — `--settings` window only | required |
| `GetLanDialIdentity` | local-server identity fetch before mTLS dialing | scripted-E2E | — (missing, FU-17) | required |
| `ControlClaim` | viewer claim/request affordance for a shared window | scripted-E2E | `main.rs::TerminalView::run_share_key` → `share.rs::ShareChrome::intercept_key` → `ipc_bridge.rs::IpcSink::control_intent` | required |
| `ControlRequest` | v3 compatibility alias; the client emits `ControlClaim` | golden | not emitted by design; its live substitute `ControlClaim` is wired through `ipc_bridge.rs::IpcSink::control_intent` | required |
| `ControlGrant` | holder grant/deny prompt for a control request | scripted-E2E | `main.rs::TerminalView::handle_overlay_key` → `share.rs::ShareChrome::intercept_key` → `ipc_bridge.rs::IpcSink::control_intent` | required |

**Reachability:** 16 of 46 rows name a live-path symbol; 14 are unwired and 16
are missing.

## Server messages (59 handled)

Every `ServerMessage` variant from `crates/scribe-common/src/protocol.rs` must
be handled without loss, including additive sharing and LAN variants.

The planning note named 57 variants; the frozen source at this inventory's
baseline contains 59. This table intentionally follows the source so neither
additive sharing variant is omitted.

The live reader `main.rs::run_reader` matches exactly twelve variants and ends
in a `_ => {}` catch-all; two more (`UpdateCheckResult`, `ReleaseList`) are
consumed by the `--settings` window's synchronous request/reply helper.
Everything else is silently discarded on the wire, so a variant absent from the
reader is definitively unreachable — there is no ambiguity in this column.

| Variant | Surface | Verification method | Reachable from | Status |
| --- | --- | --- | --- | --- |
| `PtyOutput` | terminal stream | golden | `main.rs::run_reader` arm (gated on the attached session) | required |
| `ScreenSnapshot` | tooling snapshot | scripted-E2E | `main.rs::run_reader` arm → `session_lifecycle::snapshot_reset_bytes` | required |
| `SessionReplay` | reconnect replay | scripted-E2E | `main.rs::run_reader` arm → `session_lifecycle::decode_replay` | required |
| `AiStateChanged` | AI indicator | visual-E2E | `main.rs::run_reader` arm → `ai_indicator::AiStateTracker::update` | required |
| `AiStateCleared` | AI indicator | visual-E2E | `main.rs::run_reader` arm → `AiStateTracker::remove` + `AiStateTracker::clear_context` | required |
| `CwdChanged` | tab metadata | scripted-E2E | `main.rs::on_chrome_message` arm → `ChromeMetadata::set_cwd` → `StatusBarData.cwd` | required |
| `SessionContextChanged` | session metadata | scripted-E2E | `main.rs::on_chrome_message` arm → `ChromeMetadata::set_context` → `SessionChrome::host_label` / `SessionChrome::tmux_label` | required |
| `TitleChanged` | tab title | visual-E2E | `main.rs::on_chrome_message` arm → `TabSessions::set_title` | required |
| `CodexTaskLabelChanged` | Codex tab label | visual-E2E | `main.rs::on_task_label_message` arm → `TabSessions::set_task_label` → `TabEntry::display_title` | required |
| `CodexTaskLabelCleared` | Codex tab label | visual-E2E | `main.rs::on_task_label_message` arm → `TabSessions::set_task_label` → `TabEntry::display_title` | required |
| `TaskLabelChanged` | AI tab label | visual-E2E | `main.rs::on_task_label_message` arm → `TabSessions::set_task_label` → `TabEntry::display_title` | required |
| `TaskLabelCleared` | AI tab label | visual-E2E | `main.rs::on_task_label_message` arm → `TabSessions::set_task_label` → `TabEntry::display_title` | required |
| `PromptReceived` | prompt history | scripted-E2E | `main.rs::run_reader` arm → `AiChrome::record_prompt` | required |
| `WorkspaceNamed` | workspace chrome | visual-E2E | `main.rs::on_chrome_message` arm → `ChromeMetadata::name_workspace` → `StatusBarData.workspace_name` | required |
| `SessionCreated` | pane lifecycle | scripted-E2E | `main.rs::run_reader` arm → `main.rs::open_created_tab` | required |
| `SessionExited` | pane lifecycle | scripted-E2E | `main.rs::run_reader` arm → tab removal + `AiChrome::forget` | required |
| `Bell` | terminal bell | manual | — (unwired, FU-22) — `bell.rs` outside the import closure | required |
| `Error` | error presentation | visual-E2E | `main.rs::run_reader` arm → `main.rs::set_status` | required |
| `GitBranch` | status/tab metadata | visual-E2E | `main.rs::on_chrome_message` arm → `ChromeMetadata::set_git_branch` → `StatusBarData.git_branch` | required |
| `SessionList` | startup/reconnect | scripted-E2E | `main.rs::run_reader` arm → `main.rs::sync_tab_strip` | required |
| `WorkspaceInfo` | workspace layout | scripted-E2E | — (missing, FU-6) — only a doc-comment mention in `workspace_layout.rs` | required |
| `WorkspaceNotesSnapshot` | workspace notes | scripted-E2E | — (missing, FU-21) — the modal sends `WorkspaceNotesGet` and never receives a reply | required |
| `WorkspaceNotesChanged` | workspace notes | scripted-E2E | — (missing, FU-21) | required |
| `SearchResults` | find overlay | scripted-E2E | — (missing, FU-9) | required |
| `Welcome` | registration/adoption | scripted-E2E | `main.rs::run_reader` arm → `session_lifecycle::SessionRegistry::adopt_window` | required |
| `WindowClosed` | close lifecycle | scripted-E2E | `main.rs::on_window_lifecycle_message` → `window_lifecycle::WindowLifecycle::on_window_closed` → the shell's lifecycle tick quits the app | required |
| `WindowList` | window management | scripted-E2E | `main.rs::on_window_lifecycle_message` → `window_lifecycle::WindowLifecycle::set_windows` → `StatusBarData.remote` | required |
| `RunAction` | remote automation | scripted-E2E | — (missing, FU-16) | required |
| `ActionDispatched` | remote automation | scripted-E2E | — (missing, FU-16) | required |
| `QuitRequested` | quit dialog | scripted-E2E | `main.rs::on_window_lifecycle_message` → `window_lifecycle::WindowLifecycle::on_quit_requested` → the shell's lifecycle tick quits the app | required |
| `UpdateAvailable` | update dialog | visual-E2E | `main.rs::dispatch_server_message` arm → `update::UpdateState::on_available` → `StatusBarData.update_available` | required |
| `UpdateProgress` | update dialog | visual-E2E | `main.rs::dispatch_server_message` arm → `update::UpdateState::on_progress` → `StatusBarData.update_progress` | required |
| `UpdateCheckResult` | release settings | scripted-E2E | `settings/server_action.rs::request_update_check`, reached from `SettingsWindow::run_action` — `--settings` window only | required |
| `ReleaseList` | release settings | scripted-E2E | `settings/server_action.rs::request_release_list`, reached from `SettingsWindow::run_action` — `--settings` window only | required |
| `PromptMark` | prompt navigation | scripted-E2E | — (missing, FU-7) — `session_lifecycle` tracks trim offsets but no marks are ingested | required |
| `TrimScrollback` | terminal history | golden | `main.rs::run_reader` arm → `session_lifecycle::SessionRegistry::on_trim_scrollback` | required |
| `ScrollBottom` | terminal viewport | scripted-E2E | — (missing, FU-7) | required |
| `EnvPreflightResult` | environment settings | scripted-E2E | `settings/server_action.rs::parse_env_preflight_response`, rendered into the Environment page's status line — `--settings` window only | required |
| `EnvStatus` | environment status | visual-E2E | `main.rs::on_chrome_message` arm → `ChromeMetadata::set_env_status` → `StatusBarData.env_status` | required |
| `ClipboardPromptRequest` | OSC 52 dialog | visual-E2E | — (missing, FU-8) — the `ClipboardDialog` demo is built from literals | required |
| `ClipboardBridgeWrite` | OSC 52 bridge | scripted-E2E | — (unwired, FU-8) — handled in `clipboard.rs`, outside the import closure | required |
| `ClipboardBridgeReadRequest` | OSC 52 bridge | scripted-E2E | — (unwired, FU-8) — handled in `clipboard.rs`, outside the import closure | required |
| `RemoteHandshakeReply` | tailnet connect | scripted-E2E | — (unwired, FU-16) — handled in `remote_handshake.rs`, outside the import closure | required |
| `WindowTakenOver` | remote-control landing | visual-E2E | — (unwired, FU-16) — handled in `lost_control.rs`, outside the import closure | required |
| `RemoteDisconnect` | remote-control landing | visual-E2E | — (missing, FU-16) | required |
| `RemotePeerList` | remote connect picker | visual-E2E | — (missing, FU-16) | required |
| `RemoteEnv` | remote settings | scripted-E2E | — (unwired, FU-16) — parsed in `settings/server_action.rs`; its request function has no caller | required |
| `LanApprovalPending` | cancelable connecting-side waiting-for-approval overlay | visual-E2E | — (missing, FU-17) | required |
| `LanApprovalResult` | terminal LAN dial acceptance/refusal outcome | visual-E2E | — (missing, FU-17) | required |
| `LanApprovalRequest` | owner-side device fingerprint approval overlay | visual-E2E | — (unwired, FU-17) — handled in `lan_approval.rs`, outside the import closure | required |
| `LanPeerList` | Local network entries merged into remote connect picker | visual-E2E | — (missing, FU-17) | required |
| `TrustedDeviceList` | Remote settings trusted-device rows | scripted-E2E | `settings/server_action.rs::parse_trusted_devices_response`, rendered by `SettingsWindow::trusted_device_rows` — `--settings` window only | required |
| `TrustedNetworkList` | Remote settings trusted-network rows | scripted-E2E | `settings/server_action.rs::parse_trusted_networks_response`, rendered by `SettingsWindow::trusted_network_rows` — `--settings` window only | required |
| `LanEnv` | Remote settings LAN listener/environment summary | scripted-E2E | `settings/server_action.rs::parse_lan_env_response`, rendered by `SettingsWindow::trust_status_notes` — `--settings` window only | required |
| `LanDialIdentity` | client mTLS identity returned by the local server | scripted-E2E | — (missing, FU-17) | required |
| `ShareRoster` | presence badge and live-viewer role/claim state | visual-E2E | `main.rs::dispatch_share_message` → `share.rs::ShareChrome::apply_roster` | required |
| `ControlRequested` | holder or owner grant/deny control prompt | visual-E2E | `main.rs::dispatch_share_message` → `share.rs::ShareChrome::request` | required |
| `ControlDenied` | requester control-denied notice | visual-E2E | `main.rs::dispatch_share_message` → `share.rs::ShareChrome::deny` | required |
| `ShareEnded` | shared-viewer end landing and state cleanup | visual-E2E | `main.rs::dispatch_share_message` → `share.rs::ShareChrome::end` | required |

**Reachability:** 30 of 59 rows name a live-path symbol; 11 are unwired and 18
are missing. (Recounted from the table above after the task-label rows landed;
the audit's original figures were 18 / 11 / 30.)

## Input and keybinding checklist (54 named actions)

The GPUI port retains every parsed `Bindings` action from
`crates/scribe-client/src/input.rs`. All 54 are enumerated individually below,
because the previous per-subsystem grouping hid where they break: parsing is
fine — `keybindings.rs::translate_key_action` maps all 54 onto a `KeyAction` —
but `main.rs::handle_layout_action` implements nine `LayoutAction` variants and
routes the other twenty-six to a `tracing::debug!` catch-all that swallows them.

Every row's method is `visual-E2E`: each action must be driven through
`xdotool` against the real window and asserted by its observable effect.
`tests/e2e/func/keybindings-validation.sh` currently validates the binding
*table*; it must be extended to assert effects.

| Action | Subsystem | Verification method | Reachable from | Status |
| --- | --- | --- | --- | --- |
| `split_vertical` | Pane layout | visual-E2E | — (unwired, FU-5) — `LayoutAction::SplitVertical` hits the `handle_layout_action` catch-all | required |
| `split_horizontal` | Pane layout | visual-E2E | — (unwired, FU-5) — hits the `handle_layout_action` catch-all | required |
| `close_pane` | Pane layout | visual-E2E | — (unwired, FU-5) — hits the `handle_layout_action` catch-all | required |
| `cycle_pane` | Pane layout | visual-E2E | — (unwired, FU-5) — `LayoutAction::FocusNext` hits the catch-all | required |
| `focus_left` | Pane layout | visual-E2E | — (unwired, FU-5) — hits the `handle_layout_action` catch-all | required |
| `focus_right` | Pane layout | visual-E2E | — (unwired, FU-5) — hits the `handle_layout_action` catch-all | required |
| `focus_up` | Pane layout | visual-E2E | — (unwired, FU-5) — hits the `handle_layout_action` catch-all | required |
| `focus_down` | Pane layout | visual-E2E | — (unwired, FU-5) — hits the `handle_layout_action` catch-all | required |
| `workspace_split_vertical` | Workspace layout | visual-E2E | — (unwired, FU-6) — hits the catch-all; `workspace_layout.rs`/`workspace_tree.rs` outside the import closure | required |
| `workspace_split_horizontal` | Workspace layout | visual-E2E | — (unwired, FU-6) — hits the catch-all; modules outside the import closure | required |
| `workspace_focus_left` | Workspace layout | visual-E2E | — (unwired, FU-6) — hits the catch-all; modules outside the import closure | required |
| `workspace_focus_right` | Workspace layout | visual-E2E | — (unwired, FU-6) — hits the catch-all; modules outside the import closure | required |
| `workspace_focus_up` | Workspace layout | visual-E2E | — (unwired, FU-6) — hits the catch-all; modules outside the import closure | required |
| `workspace_focus_down` | Workspace layout | visual-E2E | — (unwired, FU-6) — hits the catch-all; modules outside the import closure | required |
| `new_tab` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `NewTab` arm → `main.rs::create_tab` | required |
| `new_claude_tab` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `NewClaudeTab` arm → `ai_tab_command` | required |
| `new_claude_resume_tab` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `NewClaudeResumeTab` arm → `ai_tab_command` | required |
| `new_codex_tab` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `NewCodexTab` arm → `ai_tab_command` | required |
| `new_codex_resume_tab` | Tabs and windows | visual-E2E | `main.rs::handle_layout_action` `NewCodexResumeTab` arm → `ai_tab_command` | required |
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
| `copy` | Clipboard | visual-E2E | — (unwired, FU-8) — `CopySelection` hits the catch-all; `clipboard.rs`/`selection.rs` outside the import closure | required |
| `paste` | Clipboard | visual-E2E | — (unwired, FU-8) — `PasteClipboard` hits the catch-all; `paste.rs` outside the import closure | required |
| `scroll_up` | Navigation | visual-E2E | — (unwired, FU-7) — hits the catch-all; `split_scroll.rs` outside the import closure | required |
| `scroll_down` | Navigation | visual-E2E | — (unwired, FU-7) — hits the catch-all; `split_scroll.rs` outside the import closure | required |
| `scroll_top` | Navigation | visual-E2E | — (unwired, FU-7) — hits the catch-all; `split_scroll.rs` outside the import closure | required |
| `scroll_bottom` | Navigation | visual-E2E | — (unwired, FU-7) — hits the catch-all; `split_scroll.rs` outside the import closure | required |
| `find` | Navigation | visual-E2E | — (unwired, FU-9) — `KeyAction::OpenFind` is swallowed in `handle_binding`; `search.rs` outside the import closure | required |
| `prompt_jump_up` | Navigation | visual-E2E | — (unwired, FU-7) — hits the catch-all; no prompt marks are ingested | required |
| `prompt_jump_down` | Navigation | visual-E2E | — (unwired, FU-7) — hits the catch-all; no prompt marks are ingested | required |
| `jump_to_failure` | Navigation | visual-E2E | — (unwired, FU-7) — hits the catch-all; no prompt marks are ingested | required |
| `zoom_in` | View and overlays | visual-E2E | — (unwired, FU-10) — hits the catch-all; `zoom.rs` outside the import closure | required |
| `zoom_out` | View and overlays | visual-E2E | — (unwired, FU-10) — hits the catch-all; `zoom.rs` outside the import closure | required |
| `zoom_reset` | View and overlays | visual-E2E | — (unwired, FU-10) — hits the catch-all; `zoom.rs` outside the import closure | required |
| `command_palette` | View and overlays | visual-E2E | `main.rs::handle_overlay_key` opens the overlay — **degenerate**: `CommandPaletteEvent::Execute(_)` is discarded, so no palette entry does anything; FU-12 | required |
| `settings` | View and overlays | visual-E2E | — (unwired, FU-23) — `KeyAction::OpenSettings` is swallowed in `handle_binding`; the settings window opens only via the `--settings` CLI flag | required |
| `word_left` | Terminal shortcuts | visual-E2E (+ golden bytes) | `keybindings.rs::translate_key_action` → `KeyAction::Terminal` → `main.rs::send_key_bytes` | required |
| `word_right` | Terminal shortcuts | visual-E2E (+ golden bytes) | `keybindings.rs::translate_key_action` → `KeyAction::Terminal` → `main.rs::send_key_bytes` | required |
| `delete_word_backward` | Terminal shortcuts | visual-E2E (+ golden bytes) | `keybindings.rs::translate_key_action` → `KeyAction::Terminal` → `main.rs::send_key_bytes` | required |
| `delete_word_backward_ctrl` | Terminal shortcuts | visual-E2E (+ golden bytes) | `keybindings.rs::translate_key_action` → `KeyAction::Terminal` → `main.rs::send_key_bytes` | required |
| `delete_word_forward` | Terminal shortcuts | visual-E2E (+ golden bytes) | `keybindings.rs::translate_key_action` → `KeyAction::Terminal` → `main.rs::send_key_bytes` | required |
| `line_start` | Terminal shortcuts | visual-E2E (+ golden bytes) | `keybindings.rs::translate_key_action` → `KeyAction::Terminal` → `main.rs::send_key_bytes` | required |
| `line_end` | Terminal shortcuts | visual-E2E (+ golden bytes) | `keybindings.rs::translate_key_action` → `KeyAction::Terminal` → `main.rs::send_key_bytes` | required |

**Reachability:** 24 of 54 actions name a live-path symbol (one of them —
`command_palette` — degenerately); 30 are unwired. None are missing: every
action parses and translates, so the gap is entirely dispatch.

`LayoutAction` variants are explicitly: `SplitVertical`, `SplitHorizontal`,
`ClosePane`, `FocusNext`, `FocusLeft`, `FocusRight`, `FocusUp`, `FocusDown`,
`WorkspaceSplitVertical`, `WorkspaceSplitHorizontal`, `WorkspaceFocusLeft`,
`WorkspaceFocusRight`, `WorkspaceFocusUp`, `WorkspaceFocusDown`, `NewTab`,
`NewClaudeTab`, `NewClaudeResumeTab`, `NewCodexTab`, `NewCodexResumeTab`,
`CloseTab`, `NextTab`, `PrevTab`, `SelectTab`, `NewWindow`, `CopySelection`,
`PasteClipboard`, `ScrollUp`, `ScrollDown`, `ScrollTop`, `ScrollBottom`,
`PromptJumpUp`, `PromptJumpDown`, `JumpToFailure`, `ZoomIn`, `ZoomOut`, and
`ZoomReset` — 9 of these 35 are executed by `main.rs::handle_layout_action`;
the remaining 26 reach the catch-all. `KeyAction` variants are `Terminal`
(reachable via `main.rs::send_key_bytes`), `Layout` (partially, per the above),
`OpenCommandPalette` (reachable via `main.rs::handle_overlay_key`),
`OpenSettings` (unwired), and `OpenFind` (unwired).

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
| Opacity | `appearance.opacity` is clamped to `0.0..=1.0`; Wayland and composited X11 repaint alpha-aware terminal and chrome backgrounds live on a transparent surface, without restart. | manual | — (unwired at the audit baseline `f56ef95`, FU-4) — bead `.56` landed afterwards (`771794d`); the cell stays a marker until `.53` re-verifies it against the running client | required |
| X11 focus guard | The guard reads GPUI's `RawWindowHandle::Xcb` XID and compares it directly with `_NET_ACTIVE_WINDOW`; non-X11 backends do not enable the guard. | scripted-E2E | `main.rs::open_window` → `TerminalView::new` (FU-15) — starts the guard from the live `Window`, polls it from `drive_x11_focus_polls`, clears the debounce in `TerminalView::on_activation`, and gates the key path in `TerminalView::compositor_overlay_active`; scripted oracle `tests/e2e/visual/x11-focus-guard.sh` | required |

**Reachability:** 0 of 5 rows name a live-path symbol; 3 are unwired and 2 are
missing.

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

**Reachability:** 9 of 9 rows name a live-path symbol.

## Reachability roll-up

Counts are the reachability audit's, measured at `f56ef95`. They are the launch
gate's metric — not the unit-test count.

| Table | Rows | Reachable | Unwired | Missing |
| --- | --- | --- | --- | --- |
| Client messages | 46 | 13 | 17 | 16 |
| Server messages | 59 | 14 | 13 | 32 |
| Input and keybinding actions | 54 | 24 | 30 | 0 |
| Rendering and window | 5 | 0 | 3 | 2 |
| Removed configuration keys | 9 | 9 | 0 | 0 |
| **Total** | **173** | **60** | **63** | **50** |

Excluding the nine removed-configuration-key rows (satisfied by *absence* of
behaviour), the user-facing parity surface is **164 rows, of which 51 are
reachable (31%)** and 113 are not.

Fix units FU-1..FU-23 are defined in
[`reachability-audit.md`](reachability-audit.md); the plan's remaining phases
are sequenced around them, P0 first.

## LAN and sharing boundary

Feature 015 is present in `fd04540` (`feat: remote window control and
multi-machine sharing`). The rows above follow its final client dispatch:
`ipc_client.rs` performs the LAN handshake and maps its outcomes, `main.rs`
renders the LAN and sharing states, and `share_view.rs` supplies the roster and
control UI. `ControlRequest` remains a serializable protocol alias because the
server handles it as `ControlClaim`; the client deliberately emits only the
latter.

That describes the *legacy* client's dispatch, which is what these rows were
written against. In the GPUI client the LAN and remote surface is still
unreachable (FU-16 through FU-18). The 015 sharing rows are no longer: FU-19
put `share.rs` on the live path, so the roster, the control notices, and the
claim/grant frames are wired end to end and verified against the running app by
`tests/e2e/visual/share-control.sh`.
