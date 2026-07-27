# GPUI client reachability audit

Every row of [`parity-inventory.md`](parity-inventory.md) carries a verdict on
whether the feature is reachable by a user of the running GPUI client, rather
than merely implemented and unit-tested. Audited at `f56ef95`.

> **Line-number anchors are as of `f56ef95` and have since shifted.** In
> particular the two catch-alls this audit cites as evidence — `_ => {}` at
> `main.rs:1564` and `_ => tracing::debug!` at `main.rs:430` — no longer exist:
> `e0d47c1` replaced them with `dispatch_server_message` /
> `unhandled_server_message`, an exhaustive `server_message_variant` table, and
> an exhaustive `handle_layout_action` routing to `unhandled_layout_action`.
> The verdicts and counts below remain accurate; only the anchors are stale.
> The live metric is now produced mechanically by `tools/check-reachability.sh`
> against `tools/reachability-baseline.txt`, which is the authoritative,
> ratcheted source going forward.

> **The 173-row census below was bounded by the inventory, not by the
> requirement set (amended 2026-07-27, bead `scribe-38e.94`).** This audit's
> method statement is "every row of `parity-inventory.md` carries a verdict", so
> a spec requirement that had never been given a row could not surface here at
> all — and nine of them had not: mouse reporting, mouse-wheel scrolling, IME
> composition, cold-restart restore, the command-mark scrollbar, window geometry
> persistence, the desktop notification dispatcher, server lifecycle management,
> and file drag-and-drop. Fix units FU-1..FU-23 accordingly contain none of
> them. The 2026-07-24 retraction established that a green unit suite does not
> prove reachability; this is the next layer of the same lesson, that a complete
> reachability census does not prove parity when the census is not derived from
> the requirements. `spec.md` now carries a requirement register and
> `parity-inventory.md` a coverage index over it; see
> [Requirement-derived rows](#requirement-derived-rows-amended-2026-07-27)
> below for the verdicts the widened census produced, and FU-24..FU-28 for the
> work it exposed.

## Why this audit exists

The 016 launch gate treated a green `cargo test` as proof of parity. It is not.
The GPUI crate ships 54 library modules totalling ~35k lines; the live binary
(`main.rs` plus `ipc_bridge`, `session_lifecycle`, `sync_frames`, `terminal`,
`terminal_element`) is ~3.4k lines and imports only 19 of those modules. The
remaining 35 modules compile, pass their `#[gpui::test]` suites, and are never
constructed by the running application. Several parity rows were signed off on
exactly that evidence.

## Method

Verdicts are derived from a live-path call chain, traced outward from the four
entry points of the shipped binary:

- **App/render path** — `main()` → `open_window` → `TerminalView::render`
  (`crates/scribe-client-gpui/src/main.rs:1229`, `:1045`).
- **Key path** — the `on_key_down` listener at `main.rs:1075`, which runs
  `handle_overlay_key` → `handle_binding` → `on_key_down` in that order.
- **Outbound IPC** — `IpcSink` (`ipc_bridge.rs:186`) plus the two raw
  `out_tx.send` calls in `run_connection` (`main.rs:1295`, `:1297`).
- **Inbound IPC** — the `match message` in `run_reader` (`main.rs:1476`).

A row is judged against these definitions:

- **WIRED** — a chain of non-test calls exists from one of the entry points
  above to the implementation, and the implementation performs the row's
  behaviour.
- **UNWIRED** — an implementation exists in the crate but nothing on a live path
  reaches it. The module is referenced only by `lib.rs`, its own file, and its
  tests; or the dispatch site swallows the value.
- **MISSING** — no implementation exists anywhere in `crates/scribe-client-gpui`.
- **UNKNOWN** — could not be determined; the reason is stated inline.

Determination techniques, in order of authority:

1. `grep -rn` for each protocol variant across `crates/scribe-client-gpui/src`,
   partitioned into live-path files, unimported library modules, and `tests.rs`.
2. Import-closure analysis: `main.rs`'s `use scribe_client_gpui::…` list is the
   complete set of library modules the binary can reach, because the five
   `mod` submodules of the binary reference only `crate::terminal`,
   `crate::sync_frames`, and each other (verified by grep).
3. Reading the dispatch sites (`handle_layout_action`, `handle_binding`,
   `handle_overlay_key`, `run_reader`) to distinguish "handled" from
   "intercepted and dropped".

### Limits of this method

- **Runtime corroboration was attempted and is not load-bearing.** The installed
  `/usr/bin/scribe-dev` is the GPUI client (verified by `strings`) but was built
  `2026-07-23 17:00`, predating the wiring commits `d3e8d32` (.52), `a50a5f2`
  (.57), and `f56ef95` (.55). A 20 s read-only launch under `DISPLAY=:0`
  produced only EGL warnings and a zbus executor panic, no client tracing
  output. It is strictly *less* wired than `main` and would bias verdicts
  pessimistic, so all verdicts below come from source. No stray process was
  left; the prod and dev servers were not touched.
- **WIRED does not mean correct or complete.** It means the code is on a live
  path. Where a wired path is degenerate (demo-only trigger, fabricated ID,
  reply dropped) it is flagged in the evidence column and still appears in the
  fix units.
- **Server-message verdicts are conservative in the client's favour.** The
  reader ends in a catch-all `_ => {}` (`main.rs:1564`), so any variant not
  named in the `match` is definitively dropped. There is no ambiguity here.
- **Parity is scoped to the GPUI client.** Rows whose surface is another binary
  (`scribe-hook-helper`, `scribe-cli`) are marked WIRED against that binary and
  called out as out-of-client.

## Client messages (46)

| Variant | Verification method | Verdict | Evidence |
| --- | --- | --- | --- |
| `KeyInput` | golden | WIRED | `main.rs:819` `send_key_bytes` → `ipc_bridge.rs:197`; reached from the render key listener |
| `Resize` | scripted-E2E | WIRED | `main.rs:361` `report_cell_metrics`, `:467` `attach`, `:1640` `attach_session` → `ipc_bridge.rs:210` |
| `CreateSession` | scripted-E2E | WIRED | `main.rs:438` `create_tab` → `ipc_bridge.rs:237`; wired by bead .55 (`f56ef95`) |
| `CloseSession` | scripted-E2E | WIRED | `main.rs:482` `close_active_tab` → `ipc_bridge.rs:272`; the `close_tab` chord that reaches it was unshadowed by bead .61 |
| `CreateWorkspace` | scripted-E2E | WIRED (bead .66) | `TerminalView::split_workspace` → `IpcSink::create_workspace`; the answering `WorkspaceInfo` re-keys the region |
| `CloseWorkspace` | scripted-E2E | WIRED (bead .66) | `TerminalView::close_pane` and `reconcile_panes` → `TerminalView::close_workspace` → `IpcSink::close_workspace`, for server-minted regions only |
| `MoveSession` | scripted-E2E | WIRED (bead .66) | `TerminalView::follow_session_to_region` → `IpcSink::move_session`, when an adopting pane sits in another region |
| `Subscribe` | scripted-E2E | WIRED | `main.rs` `attach_session` and `TerminalView::attach` → `IpcSink::subscribe`, behind the `AttachSessions` on the same ordered channel; wired by bead .79 |
| `RequestSnapshot` | scripted-E2E | WIRED | `main.rs` `report_cell_metrics` (after the post-font-reload `Resize`) and `forward_replay` (replay-decode fallback) → `IpcSink::request_snapshot`; wired by bead .79 |
| `ListSessions` | scripted-E2E | WIRED | `main.rs:1297`, sent on every connect |
| `AttachSessions` | scripted-E2E | WIRED | `main.rs:1640` `attach_session`; `ipc_bridge.rs:259` from `attach` |
| `ConfigReloaded` | scripted-E2E | WIRED | `main.rs:347` in `apply_config_reload`; watcher wired by bead .57 (`a50a5f2`) |
| `ReportWorkspaceTree` | scripted-E2E | WIRED (bead .66) | `TerminalView::report_workspace_tree` → `PaneShell::wire_tree` → `IpcSink::report_workspace_tree`, after every layout mutation |
| `SearchRequest` | scripted-E2E | WIRED (bead .69) | `TerminalView::send_search_request` → `IpcSink::search_request`, on every find-overlay query edit |
| `WorkspaceNotesGet` | scripted-E2E | WIRED | `main.rs` `open_workspace_notes_modal` → `IpcSink::workspace_notes_get`, on the workspace `notes_workspace_id` resolves from live state |
| `WorkspaceNotesMutate` | scripted-E2E | WIRED | `main.rs` `route_workspace_notes_action` → `IpcSink::workspace_notes_mutate`, against that same workspace |
| `Hello` | scripted-E2E | WIRED | `main.rs:1295`, sent on every connect |
| `CloseWindow` | scripted-E2E | WIRED (bead .72) | `TerminalView::route_close_action` → `IpcSink::close_window` |
| `QuitAll` | scripted-E2E | WIRED (bead .72) | `TerminalView::route_close_action` → `IpcSink::quit_all` |
| `TriggerUpdate` | scripted-E2E | UNWIRED | `settings/server_action.rs:81` `request_trigger_update` has no caller |
| `DismissUpdate` | gpui-test | MISSING | never constructed anywhere in the crate |
| `CheckForUpdates` | scripted-E2E | WIRED | `settings/window.rs:161` from `action.check_for_updates` (`settings/model.rs:386`). Reached through the settings window, which bead .82 made reachable from inside the running client (settings chord, palette row, titlebar gear) as well as via `scribe-client-gpui --settings` |
| `ListReleases` | scripted-E2E | WIRED | `settings/window.rs:165` from `action.list_releases` (`settings/model.rs:387`); same settings-window reachability |
| `ListWindows` | scripted-E2E | WIRED (bead .72) | `TerminalView::poll_window_list` → `IpcSink::list_windows` |
| `DispatchAction` | scripted-E2E | WIRED | `IpcSink::dispatch_action`, from a viewer's window-mutating palette row |
| `FocusChanged` | scripted-E2E | WIRED (bead .72) | `TerminalView::report_focus` → `IpcSink::focus_changed` |
| `HookEvent` | scripted-E2E | WIRED | out-of-client by design: `crates/scribe-hook-helper/src/main.rs:119` |
| `EnvPreflight` | scripted-E2E | WIRED | `settings/window.rs` `run_action` (`action.env_preflight`) and the gated `enable_env_persistence` ON transition; same settings-window reachability. Asserted on the wire by `tests/e2e/visual/settings-trust.sh` |
| `ClipboardPromptResponse` | scripted-E2E | UNWIRED | built in `clipboard.rs`; module not imported by `main.rs` |
| `ClipboardBridgeReadReply` | scripted-E2E | UNWIRED | built in `clipboard.rs`; module not imported by `main.rs` |
| `RemoteHandshake` | scripted-E2E | WIRED | `run_remote_connection` → `remote_handshake::perform_remote_handshake` |
| `ListRemotePeers` | scripted-E2E | WIRED | `adopt_remote_surface` and `refresh_remote_peers` → `IpcSink::list_remote_peers` |
| `GetRemoteEnv` | gpui-test | WIRED | `probe_remote_env` at startup; `SettingsWindow::refresh_trust` on the Remote page |
| `LanHello` | scripted-E2E | MISSING | never constructed anywhere in the crate |
| `LanApprovalDecision` | scripted-E2E | UNWIRED | built in `lan_approval.rs`; module not imported by `main.rs` |
| `ListLanPeers` | scripted-E2E | MISSING | never constructed anywhere in the crate |
| `ListTrustedDevices` | scripted-E2E | WIRED | `settings/window.rs` `refresh_trust`, reached from `run_action` (`action.refresh_trust`) and the first visit to the Remote page; same settings-window reachability. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `RevokeTrustedDevice` | scripted-E2E | WIRED | `settings/window.rs` `run_action` (`action.revoke_trusted_device:<hex>` from each approved-device row); same settings-window reachability. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `ListTrustedNetworks` | scripted-E2E | WIRED | `settings/window.rs` `refresh_trust`, same callers as `ListTrustedDevices`; same settings-window reachability. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `AddCurrentNetworkTrusted` | scripted-E2E | WIRED | `settings/window.rs` `run_action` (`action.add_current_network`); same settings-window reachability. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `RemoveTrustedNetwork` | scripted-E2E | WIRED | `settings/window.rs` `run_action` (`action.remove_trusted_network:<id>` from each trusted-network row); same settings-window reachability. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `GetLanEnv` | scripted-E2E | WIRED | `settings/window.rs` `refresh_trust`, alongside the two trust list queries; same settings-window reachability. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `GetLanDialIdentity` | scripted-E2E | MISSING | never constructed anywhere in the crate |
| `ControlClaim` | scripted-E2E | UNWIRED | built in `share.rs`; module not imported by `main.rs` |
| `ControlRequest` | golden | UNWIRED | built in `share.rs`, unimported. Not emitting it is by design, but its live substitute `ControlClaim` is itself UNWIRED, so the sharing surface is unreachable either way |
| `ControlGrant` | scripted-E2E | UNWIRED | built in `share.rs`; module not imported by `main.rs` |

**Client-message subtotals:** WIRED 19 · UNWIRED 15 · MISSING 12 · UNKNOWN 0.
(Was WIRED 13 · MISSING 16 at `f56ef95`; bead `.79` moved `Subscribe` and
`RequestSnapshot` from MISSING to WIRED, and bead `.75` moved `RemoteHandshake`,
`ListRemotePeers`, `GetRemoteEnv` and `DispatchAction`. Rows landed by FU-17 /
FU-18 / FU-19 are recorded in the fix-unit list below rather than restated here;
`tools/reachability-baseline.txt` is the authoritative live count.)

## Server messages (59)

The live reader (`main.rs:1476`) matches exactly twelve variants and ends in
`_ => {}` at `main.rs:1564`. Two more (`UpdateCheckResult`, `ReleaseList`) are
consumed by the settings window's synchronous request/reply helper (bead .82
made that window reachable from inside the running client, not only via
`--settings`).
Everything else is silently discarded on the wire.

| Variant | Verification method | Verdict | Evidence |
| --- | --- | --- | --- |
| `PtyOutput` | golden | WIRED | `run_reader` arm; gated on the attached session |
| `ScreenSnapshot` | scripted-E2E | WIRED | `run_reader` arm → `session_lifecycle::snapshot_reset_bytes` |
| `SessionReplay` | scripted-E2E | WIRED | `run_reader` arm → `session_lifecycle::decode_replay` |
| `AiStateChanged` | visual-E2E | WIRED | `run_reader` arm → `AiStateTracker::update`; wired by bead .52 (`d3e8d32`) |
| `AiStateCleared` | visual-E2E | WIRED | `run_reader` arm → `tracker.remove` + `clear_context` |
| `CwdChanged` | gpui-test | MISSING | no reference in the crate. `StatusBarData.cwd` is hardcoded `None` in `build_status_model` |
| `SessionContextChanged` | gpui-test | MISSING | no reference in the crate |
| `TitleChanged` | visual-E2E | MISSING | no reference in the crate; tab titles only ever come from `SessionList`/`SessionCreated` |
| `CodexTaskLabelChanged` | visual-E2E | MISSING | no reference in the crate |
| `CodexTaskLabelCleared` | visual-E2E | MISSING | no reference in the crate |
| `TaskLabelChanged` | visual-E2E | MISSING | no reference in the crate |
| `TaskLabelCleared` | visual-E2E | MISSING | no reference in the crate |
| `PromptReceived` | gpui-test | WIRED | `run_reader` arm → `AiChrome::record_prompt`; wired by bead .52 |
| `WorkspaceNamed` | visual-E2E | MISSING | no reference; `StatusBarData.workspace_name` hardcoded `None` |
| `SessionCreated` | scripted-E2E | WIRED | `run_reader` arm → `open_created_tab` |
| `SessionExited` | scripted-E2E | WIRED | `run_reader` arm → tab removal + `AiChrome::forget` |
| `Bell` | scripted-E2E | WIRED | `run_reader` arm → `on_bell_message` queue → `TerminalView::poll_bells` → `BellController` gate → `Window::request_attention`; wired by bead .81 |
| `Error` | visual-E2E | WIRED | `run_reader` arm → `set_status` |
| `GitBranch` | visual-E2E | MISSING | no reference; `StatusBarData.git_branch` hardcoded `None` |
| `SessionList` | scripted-E2E | WIRED | `run_reader` arm → `sync_tab_strip` |
| `WorkspaceInfo` | scripted-E2E | WIRED (bead .66) | `dispatch_server_message` arm → `on_workspace_info` → `ChromeMetadata::name_workspace` (status bar) and parked for `TerminalView::adopt_workspace_info` → `PaneShell::apply_workspace_info` |
| `WorkspaceNotesSnapshot` | scripted-E2E | WIRED | `main.rs` `on_workspace_notes_message` → `WorkspaceNotesStore::apply_collections` → `TerminalView::sync_workspace_notes` |
| `WorkspaceNotesChanged` | scripted-E2E | WIRED | `main.rs` `on_workspace_notes_message` → `WorkspaceNotesStore::apply_collection` → `TerminalView::sync_workspace_notes` |
| `SearchResults` | scripted-E2E | WIRED (bead .69) | `on_search_results` → `FindResults` → `FindOverlayView::adopt_results` → `TerminalElement::with_highlights` |
| `Welcome` | scripted-E2E | WIRED | `run_reader` arm → `SessionRegistry::adopt_window` |
| `WindowClosed` | scripted-E2E | WIRED (bead .72) | `on_window_lifecycle_message` → `WindowLifecycle::on_window_closed` |
| `WindowList` | scripted-E2E | WIRED (bead .72) | `on_window_lifecycle_message` → `WindowLifecycle::set_windows` |
| `RunAction` | scripted-E2E | WIRED | queued by `on_remote_message`, run by `poll_remote_actions` |
| `ActionDispatched` | scripted-E2E | WIRED | `on_remote_message` — the routing ack for a dispatch this client sent |
| `QuitRequested` | scripted-E2E | WIRED (bead .72) | `on_window_lifecycle_message` → `WindowLifecycle::on_quit_requested` |
| `UpdateAvailable` | visual-E2E | MISSING | no reference; `StatusBarData.update_available` hardcoded `None` |
| `UpdateProgress` | visual-E2E | MISSING | no reference; `StatusBarData.update_progress` hardcoded `None` |
| `UpdateCheckResult` | gpui-test | WIRED | `settings/server_action.rs:46`, reached from `settings/window.rs:161` in the settings window |
| `ReleaseList` | gpui-test | WIRED | `settings/server_action.rs:124`, reached from `settings/window.rs:165` in the settings window |
| `PromptMark` | gpui-test | MISSING | no reference. `session_lifecycle` tracks trim offsets but no marks are ever ingested |
| `TrimScrollback` | golden | WIRED | `run_reader` arm → `SessionRegistry::on_trim_scrollback` |
| `ScrollBottom` | gpui-test | MISSING | no `ServerMessage::ScrollBottom` reference (the `keybindings.rs` hit is `LayoutAction::ScrollBottom`) |
| `EnvPreflightResult` | scripted-E2E | WIRED | parsed by `parse_env_preflight_response` and rendered into the Environment page's status line; same settings-window reachability. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `EnvStatus` | visual-E2E | MISSING | no reference; `StatusBarData.env_status` hardcoded `None` |
| `ClipboardPromptRequest` | visual-E2E | MISSING | no reference. The `ClipboardDialog` demo is built from literals at `main.rs:721` |
| `ClipboardBridgeWrite` | scripted-E2E | UNWIRED | handled in `clipboard.rs`; module not imported by `main.rs` |
| `ClipboardBridgeReadRequest` | scripted-E2E | UNWIRED | handled in `clipboard.rs`; module not imported by `main.rs` |
| `RemoteHandshakeReply` | scripted-E2E | WIRED | `perform_remote_handshake` during the preamble; `on_remote_message` on the live reader |
| `WindowTakenOver` | visual-E2E | WIRED | `on_remote_message` → `RemoteChrome::displace` → `lost_control_overlay` |
| `RemoteDisconnect` | visual-E2E | WIRED | `on_remote_message` → `RemoteChrome::sever` → the status strip |
| `RemotePeerList` | visual-E2E | WIRED | `on_remote_message` → `RemoteChrome::set_peers` → the status strip |
| `RemoteEnv` | gpui-test | WIRED | `on_remote_message` → `RemoteChrome::set_env`; also parsed by `settings/server_action.rs` |
| `LanApprovalPending` | visual-E2E | MISSING | no reference in the crate |
| `LanApprovalResult` | visual-E2E | MISSING | no reference in the crate |
| `LanApprovalRequest` | visual-E2E | UNWIRED | handled in `lan_approval.rs`; module not imported by `main.rs` |
| `LanPeerList` | visual-E2E | MISSING | no reference in the crate |
| `TrustedDeviceList` | scripted-E2E | WIRED | parsed by `parse_trusted_devices_response` and rendered as the Remote page's approved-device rows; same settings-window reachability. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `TrustedNetworkList` | scripted-E2E | WIRED | parsed by `parse_trusted_networks_response` and rendered as the Remote page's trusted-network rows; same settings-window reachability. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `LanEnv` | scripted-E2E | WIRED | parsed by `parse_lan_env_response` and rendered as the Remote page's own-fingerprint / addability notes; same settings-window reachability. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `LanDialIdentity` | scripted-E2E | MISSING | no reference in the crate |
| `ShareRoster` | visual-E2E | UNWIRED | handled in `share.rs`; module not imported by `main.rs` |
| `ControlRequested` | visual-E2E | UNWIRED | handled in `share.rs`; module not imported by `main.rs` |
| `ControlDenied` | visual-E2E | MISSING | no reference in the crate |
| `ShareEnded` | visual-E2E | MISSING | no reference in the crate |

**Server-message subtotals:** WIRED 21 · UNWIRED 10 · MISSING 28 · UNKNOWN 0.
(Bead `.75` moved the seven feature-013 rows — `RemoteHandshakeReply`,
`WindowTakenOver`, `RemoteDisconnect`, `RemotePeerList`, `RemoteEnv`,
`RunAction`, `ActionDispatched` — to WIRED. As above, rows landed by FU-17 /
FU-18 / FU-19 are recorded in the fix-unit list rather than restated here.)

## Input and keybinding actions (54 named actions)

Every named action parses and translates correctly — `translate_key_action`
(`keybindings.rs:420`) maps all 54 onto a `KeyAction`. The break is at the
dispatch site: `handle_layout_action` (`main.rs:409`) implements nine `LayoutAction`
variants and sends the other twenty-six to `_ => tracing::debug!` at
`main.rs:430`, where they are swallowed (correctly never leaked to the PTY, but
also never executed).

| Action | Subsystem | Verdict | Evidence |
| --- | --- | --- | --- |
| `split_vertical` | Pane layout | WIRED | `handle_layout_action` `SplitVertical` arm -> `TerminalView::split_pane` -> `PaneShell::split_focused_pane` + `CreateSession` (bead .58) |
| `split_horizontal` | Pane layout | WIRED | as above, with `SplitDirection::Vertical` |
| `close_pane` | Pane layout | WIRED | `TerminalView::close_pane` -> `PaneShell::close_focused_pane`; falls back to `close_active_tab` on the last pane (bead .58) |
| `cycle_pane` | Pane layout | WIRED | `TerminalView::focus_next_pane` -> `PaneShell::focus_next_pane` (bead .58) |
| `focus_left` | Pane layout | WIRED | `TerminalView::focus_pane` -> `PaneShell::focus_pane_in_direction` (bead .58) |
| `focus_right` | Pane layout | WIRED | as above |
| `focus_up` | Pane layout | WIRED | as above |
| `focus_down` | Pane layout | WIRED | as above |
| `workspace_split_vertical` | Workspace layout | WIRED | `TerminalView::split_workspace` -> `PaneShell::split_workspace` -> `WorkspaceTree::split_workspace` (bead .58) |
| `workspace_split_horizontal` | Workspace layout | WIRED | as above, with `SplitDirection::Vertical` |
| `workspace_focus_left` | Workspace layout | WIRED | `TerminalView::focus_workspace` -> `PaneShell::focus_workspace_in_direction` (bead .58) |
| `workspace_focus_right` | Workspace layout | WIRED | as above |
| `workspace_focus_up` | Workspace layout | WIRED | as above |
| `workspace_focus_down` | Workspace layout | WIRED | as above |
| `new_tab` | Tabs and windows | WIRED | `main.rs:410` → `create_tab` → `CreateSession` |
| `new_claude_tab` | Tabs and windows | WIRED | `main.rs:411` → `ai_tab_command(ClaudeCode, false)` |
| `new_claude_resume_tab` | Tabs and windows | WIRED | `main.rs:414` |
| `new_codex_tab` | Tabs and windows | WIRED | `main.rs:417` |
| `new_codex_resume_tab` | Tabs and windows | WIRED | `main.rs:420` |
| `close_tab` | Tabs and windows | WIRED | `handle_layout_action` `CloseTab` arm → `close_active_tab`. The Linux default chord `ctrl+shift+q` used to be consumed by the close-dialog demo before `handle_binding` ran; `translate_overlay_chord` now yields to any configured binding and the demo moved to `ctrl+shift+d` (bead .61) |
| `next_tab` | Tabs and windows | WIRED | `main.rs:423` → `TabSessions::focus_next` |
| `prev_tab` | Tabs and windows | WIRED | `main.rs:424` → `TabSessions::focus_prev` |
| `select_tab_1`…`select_tab_9` | Tabs and windows | WIRED (9 rows) | `main.rs:425` → `TabSessions::select`; bindings at `keybindings.rs:561-569` |
| `new_window` | Tabs and windows | WIRED | `handle_layout_action` `NewWindow` arm → `open_new_window`, which builds a second window's `Shared` + IPC connection through `start_window_backend` and opens it with `open_window` (bead .61) |
| `copy` | Clipboard | UNWIRED | `LayoutAction::CopySelection` hits `main.rs:430`; `clipboard.rs`/`selection.rs` unimported |
| `paste` | Clipboard | UNWIRED | `LayoutAction::PasteClipboard` hits `main.rs:430`; `paste.rs` unimported |
| `scroll_up` | Navigation | WIRED | `handle_layout_action` `ScrollUp` arm → `TerminalView::scroll_terminal` → `DisplayOnlyTerminal::scroll`; the snapshot now reads the viewport through the grid's display offset (bead .59) |
| `scroll_down` | Navigation | WIRED | as above, `Scroll::PageDown` |
| `scroll_top` | Navigation | WIRED | as above, `Scroll::Top` |
| `scroll_bottom` | Navigation | WIRED | as above, `Scroll::Bottom`; also the snap-to-bottom a keystroke performs in `snap_to_bottom_for_input` |
| `find` | Navigation | WIRED (bead .69) | `dispatch_key_action` → `TerminalView::open_find_overlay`; `search.rs` is on the import closure |
| `prompt_jump_up` | Navigation | UNWIRED | hits `main.rs:430`; no prompt marks are ingested |
| `prompt_jump_down` | Navigation | UNWIRED | as above |
| `jump_to_failure` | Navigation | UNWIRED | as above |
| `zoom_in` | View and overlays | WIRED | `handle_layout_action` `ZoomIn` arm → `TerminalView::apply_zoom` → `ZoomState::zoom_in`, rebuilding `GridFont` through `rebuild_font` (bead .59) and re-laying the grid into the measured window area (bead .70) |
| `zoom_out` | View and overlays | WIRED | as above, `ZoomState::zoom_out` |
| `zoom_reset` | View and overlays | WIRED | as above, `ZoomState::reset` |
| `command_palette` | View and overlays | WIRED | `main.rs:700` opens the overlay. Degenerate: `CommandPaletteEvent::Execute(_)` is discarded at `main.rs:502`, so no palette entry does anything |
| `settings` | View and overlays | WIRED (bead .82) | `dispatch_key_action` → `TerminalView::open_or_focus_settings` → `settings::open_settings_window`, the same window `run_settings` opens for `--settings`. The palette row and the titlebar gear (`TitlebarEvent::OpenSettings`) land on the same handler; the retained `WindowHandle` raises the open window instead of opening a second one. Asserted on the mapped X11 window by `tests/e2e/visual/settings-entry.sh` |
| `word_left` | Terminal shortcuts | WIRED | `keybindings.rs:477` → `KeyAction::Terminal` → `main.rs:795` `send_key_bytes` |
| `word_right` | Terminal shortcuts | WIRED | `keybindings.rs:478` → same path |
| `delete_word_backward` | Terminal shortcuts | WIRED | `keybindings.rs:479` → same path |
| `delete_word_backward_ctrl` | Terminal shortcuts | WIRED | `keybindings.rs:481` → same path |
| `delete_word_forward` | Terminal shortcuts | WIRED | `keybindings.rs:484` → same path |
| `line_start` | Terminal shortcuts | WIRED | `keybindings.rs:485` → same path |
| `line_end` | Terminal shortcuts | WIRED | `keybindings.rs:486` → same path |

`KeyAction` variants: `Terminal` WIRED (`main.rs:795`), `Layout` partially wired
(9 of 35 `LayoutAction` variants), `OpenCommandPalette` WIRED (claimed earlier in
`handle_overlay_key`, `main.rs:700`), `OpenFind` WIRED (bead .69), `OpenSettings`
WIRED (bead .82).

**Input subtotals:** WIRED 24 · UNWIRED 30 · MISSING 0 · UNKNOWN 0.

## Rendering and window (5)

`TerminalElement::paint` (`terminal_element.rs:75`) renders `Content.rows:
Vec<String>` (`terminal.rs:18`) as plain text in one foreground colour
(`0xd9dde3`) on one background (`0x101318`). There is no per-cell colour, no
glyph overlay, and no font-fallback or shaping configuration in the paint path
at all.

| Surface | Verification method | Verdict | Evidence |
| --- | --- | --- | --- |
| Box drawing | visual-E2E | UNWIRED | `box_drawing.rs` implements the rasterizer; its only references outside itself are `lib.rs:5` and `lib.rs:47`. No paint-quad overlay exists in `terminal_element.rs` |
| Font fallback | gpui-test | MISSING | `FontFallbacks` and `Symbols Nerd Font` appear nowhere in `crates/scribe-client-gpui/src`. `terminal_element.rs:82` sets a bare `.font_family(...)` |
| Ligatures | visual-E2E | MISSING | `shape_line` and `calt` appear nowhere in the crate; `appearance.ligatures` is never read by the client |
| Opacity | manual | UNWIRED | `apply_opacity_change` (`main.rs:384`) only emits `tracing::info!`; the root background is a hardcoded opaque `rgb(0x0010_1318)` at `main.rs:1087` and `terminal_element.rs:80`. In flight as bead .56 |
| X11 focus guard | scripted-E2E | UNWIRED | `x11_focus.rs` exists; its only references are `lib.rs:98` and its own doc comment. Never started by `main` |

**Rendering subtotals:** WIRED 0 · UNWIRED 3 · MISSING 2 · UNKNOWN 0.

**Post-audit:** bead `.63` (FU-1) landed the cell-accurate paint path after this
baseline, closing Box drawing and Ligatures. `Content` now carries a `Cell` per
grid position, and `TerminalElement::paint` resolves it on one `gpui::canvas` —
background quads, then `box_drawing::mask_quads` overlay quads, then
`shape_line` runs carrying the ordered `FontFallbacks` chain and `calt` gated on
`appearance.ligatures`. `box_drawing` and `color` left
`tools/reachability-baseline.txt` as a result (`modules-wired` 21 → 23). Bead
`.56` had already closed Opacity (`771794d`). The verdicts above are preserved
as the `f56ef95` record and are not restated per bead.

Font fallback closed with the same bead. The chain wired onto every run was
initially inert on gpui `f96212f`: `CosmicTextSystem::load_family` removes any
face whose charmap lacks an `'m'` glyph — which is every stock symbols-only
Nerd Font — and GPUI exposes no counterpart to the legacy `forbidden_fallback`
that bans `Unifont Sample`, so `U+F09B`/`U+F121` rendered as hex boxes whether
or not the Nerd Font families were named. The client now embeds
`Symbols Nerd Font Mono` with a `U+006D` cmap alias
(`tools/patch-nerd-symbols-font.py`, registered by
`fonts::register_embedded_fonts` before the first frame), so the face survives
eviction and the user chain resolves it ahead of cosmic-text's platform
fallback. Live capture shows `U+F09B`/`U+F121` as the octocat/code icons. The
new `fonts` module is imported on the live path, so it enters
`tools/reachability-baseline.txt` as wired, never as an unwired entry.

## Removed configuration keys (9)

All nine behave correctly in the running client. `crates/scribe-common/src/config.rs`
declares no `deny_unknown_fields`, so `appearance.splash` and
`appearance.splash_duration_ms` — which have no struct field at all — are
dropped by serde. The remaining seven deserialize into `AppearanceConfig` but
are never read by the GPUI client: the only hits under
`crates/scribe-client-gpui/src` are in `settings/apply.rs`, which is a
config *writer*, and `prompt_bar.rs:75-77`, which reads the identically named
fields of `scribe_common::theme::ChromeColors`, not `AppearanceConfig`. The live
load path is `ConfigRuntime::start` (`config.rs:312`) → `load_config`
(`config.rs:215`), the same function used by `main()`.

| Legacy TOML key | Verification method | Verdict | Evidence |
| --- | --- | --- | --- |
| `appearance.splash` | gpui-test | WIRED | no struct field; unknown key ignored by serde |
| `appearance.splash_duration_ms` | gpui-test | WIRED | no struct field; covered by `config/tests.rs:29` |
| `appearance.scrollbar_width` | gpui-test | WIRED | field exists, never read by the client |
| `appearance.scrollbar_color` | gpui-test | WIRED | field exists, never read by the client |
| `appearance.prompt_bar_second_row_bg` | gpui-test | WIRED | field exists, never read; the prompt bar reads `ChromeColors` |
| `appearance.prompt_bar_first_row_bg` | gpui-test | WIRED | as above |
| `appearance.prompt_bar_text` | gpui-test | WIRED | as above |
| `appearance.prompt_bar_icon_first` | gpui-test | WIRED | as above |
| `appearance.prompt_bar_icon_latest` | gpui-test | WIRED | as above |

**Removed-key subtotals:** WIRED 9 · UNWIRED 0 · MISSING 0 · UNKNOWN 0.

## Requirement-derived rows (amended 2026-07-27)

The 29 rows of `parity-inventory.md`'s "Spec behaviour requirements" table did
not exist at `f56ef95` and so carry no baseline verdict. They are censused here
because they are the half of parity this audit's method could not see: derived
from `spec.md`'s requirement register rather than from the legacy client's IPC
and keybinding surface.

Verdicts are measured against `main` at `c50724b`, using the same live-path
definitions as the `f56ef95` census. Twenty-four are WIRED and are not restated
row by row — `parity-inventory.md` names each one's live-path chain and its
oracle. The five that are not reachable are the finding:

| Requirement row | spec.md | Verdict | Evidence |
| --- | --- | --- | --- |
| Server-upgrade reattach | `US2-4` | MISSING | `main.rs::start_ipc_thread` awaits `run_connection` exactly once; when it returns the thread publishes a status line and exits. No redial path exists, so an `--upgrade` handoff leaves the window attached to nothing. `tests/e2e/visual/reconnect.sh` relaunches the client *process*, so it never exercised this |
| Remote connect picker overlay | `US4-4` | MISSING | `remote::RemoteConnect` is ported and unit-tested; no GPUI view renders it. `main.rs::TerminalView::refresh_remote_peers` puts the peer count on the status strip instead. Already noted as a presentation gap in `parity-inventory.md`'s "LAN and sharing boundary", which could not cost a row while no row existed |
| Pane dividers and drag-resize | `US3-10` | UNWIRED | `divider.rs` has no reference outside `lib.rs` and its own tests; `tools/reachability-baseline.txt` lists `unwired-module divider` |
| AI indicator borders and tab tint | `US4-1` | UNWIRED | `ai_indicator::AiStateTracker::tab_indicator_color`, `workspace_border_color`, `tick`, `needs_animation`, `clear_stale_processing`, `note_activity`, `remember_provider`, `clear_attention_states` and `ai_indicator::pane_border_edges` have no caller outside `ai_indicator.rs`. `main.rs` reaches only `update`, `remove`, `clear_context`, `provider_for_session` and `context_for`. The module is *imported*, so the module-level ratchet counts it wired while the painted half is unreachable |
| Workspace notes hover preview | `US4-3` | UNWIRED | `workspace_notes_preview.rs` has no reference outside `lib.rs` and its own tests; `tools/reachability-baseline.txt` lists `unwired-module workspace_notes_preview` |

Two of these were invisible to *both* gates. `ai_indicator` is imported by
`main.rs`, so the module ratchet in `tools/check-reachability.sh` scores it
wired, and no inventory row asked whether the border ever painted. Module-level
reachability is a floor, not a substitute for a per-requirement row — the same
distinction the `f56ef95` census drew between a green `#[gpui::test]` and a live
call site, one level up.

**Requirement-derived subtotals:** WIRED 24 · UNWIRED 3 · MISSING 2 · UNKNOWN 0.

## Summary

| Verdict | Count | Share |
| --- | --- | --- |
| WIRED | 60 | 34.7% |
| UNWIRED | 63 | 36.4% |
| MISSING | 50 | 28.9% |
| UNKNOWN | 0 | 0% |
| **Total rows** | **173** | |

Row totals by table: client messages 46, server messages 59, named input actions
54, rendering/window 5, removed config keys 9.

Excluding the nine removed-config-key rows (which are satisfied by *absence* of
behaviour), the user-facing parity surface is 164 rows of which **51 are
reachable (31%)** and **113 are not**.

**These totals are a census of the inventory as it stood at `f56ef95`, not of
the requirement set.** The inventory has since grown by the command-mark
scrollbar row and the 29 requirement-derived rows above, to 203 rows / 194
user-facing. `parity-inventory.md`'s roll-up is the live figure; the numbers in
this section are the historical baseline the fix units were sequenced against.

## Prioritized fix units

Grouped so each unit can become one fix bead. Units already covered by an
in-flight bead are marked and must not be re-filed.

### P0 — terminal rendering fidelity

The paint path renders monochrome text. This is the single largest parity gap
and blocks every `visual-E2E` row.

- **FU-1 Cell-accurate paint path.** `Content` must carry per-cell fg/bg/attrs
  instead of `Vec<String>`; `TerminalElement` must paint them. Blocks box
  drawing, ligatures, font fallback, and colour parity.
  Rows: Box drawing, Font fallback, Ligatures.
- **FU-2 Terminal chrome from server metadata.** Rows: `TitleChanged`,
  `CwdChanged`, `GitBranch`, `WorkspaceNamed`, `SessionContextChanged`,
  `EnvStatus`, plus the hardcoded `None`s in `build_status_model`.
- **FU-3 AI tab labels.** Rows: `TaskLabelChanged`, `TaskLabelCleared`,
  `CodexTaskLabelChanged`, `CodexTaskLabelCleared`.
- **FU-4 Opacity.** Row: Opacity. **Covered by bead .56 — do not re-file.**

### P1 — core interaction

- **FU-5 Pane tree.** Rows: `split_vertical`, `split_horizontal`, `close_pane`,
  `cycle_pane`, `focus_left/right/up/down`. **Closed by bead .58.** `PaneShell`
  (`pane_shell.rs`) owns one `WorkspaceTree` and one `PaneTree` per region on
  the live path, `PaneGrids` gives every pane its own display grid, and all
  eight rows are WIRED above.
- **FU-6 Workspace tree.** Rows: `workspace_split_vertical`,
  `workspace_split_horizontal`, `workspace_focus_left/right/up/down`,
  `CreateWorkspace`, `CloseWorkspace`, `MoveSession`, `ReportWorkspaceTree`,
  `WorkspaceInfo`. **Closed by beads .58 and .66.** .58 wired the six workspace
  key actions through `PaneShell`; .66 wired the workspace IPC, so a split now
  asks the server for a real workspace, adopts the `WorkspaceInfo` it answers
  with, re-files the seeded session with `MoveSession`, reports the tree after
  every mutation, and closes the workspace when a region collapses. Asserted on
  the wire by `tests/e2e/visual/workspace-ipc.sh`.
- **FU-7 Scrollback navigation and marks.** Rows: `scroll_up`, `scroll_down`,
  `scroll_top`, `scroll_bottom`, `prompt_jump_up`, `prompt_jump_down`,
  `jump_to_failure`, `PromptMark`, `ScrollBottom`. **The four `scroll_*` rows
  are closed by bead .59**, which also fixed the underlying defect: the content
  snapshot read the live screen and ignored the grid's display offset, so no
  scroll could have changed a pixel even once dispatched. The prompt-mark
  ingestion rows are not covered.
- **FU-8 Clipboard and selection.** Rows: `copy`, `paste`,
  `ClipboardPromptResponse`, `ClipboardBridgeReadReply`,
  `ClipboardPromptRequest`, `ClipboardBridgeWrite`,
  `ClipboardBridgeReadRequest`. Requires wiring `clipboard.rs`, `selection.rs`,
  `paste.rs`, and routing `DialogEvent::Chosen` (`main.rs:542`) to a real
  response. **Selection groundwork is landed by bead .59**: `selection.rs` and
  `smart_selection.rs` are in the import closure and a right-click resolves live
  smart-selection rows, so what remains is mouse-drag selection and the
  clipboard itself.
- **FU-9 Find overlay.** Rows: `find`, `SearchRequest`, `SearchResults`.
  **Closed by bead .69.** `KeyAction::OpenFind` opens `search::FindOverlayView`,
  each query edit sends a real `SearchRequest`, and the `SearchResults` arm in
  `dispatch_server_message` drives per-cell highlights through
  `TerminalElement::paint`. All three rows are WIRED above.
- **FU-10 Zoom.** Rows: `zoom_in`, `zoom_out`, `zoom_reset`. **Dispatch wired
  by bead .59, completed by bead .70.** `ZoomState` is folded into `GridFont`
  by `TerminalView::rebuild_font`, the one place a zoom step and a config font
  reload share, so a saved font-size edit rebases the zoom instead of
  discarding it. Bead .70 found the rescale stopped at the glyphs: pane
  geometry was resolved against a viewport stated in the font's own cells, so
  every zoom level published the same `cols`x`rows` and the freed pixels stayed
  dead. `TerminalView::pane_viewport` now returns the grid area's measured
  pixel rect and `sync_grid_geometry` republishes when it moves. Verified on
  screen and on the wire by `tests/e2e/visual/terminal-zoom.sh`, with the
  coarser zoom phase of `tests/e2e/visual/terminal-viewport.sh` alongside it.
- **FU-11 close_tab chord and new_window.** Rows: `close_tab`, `new_window`.
  **Closed by bead .61.** The overlay chords now yield to the configured
  bindings (`translate_overlay_chord`), the close dialog and notes modal moved
  off `ctrl+shift+q` / `ctrl+shift+n`, and `NewWindow` opens a second window
  with its own IPC backend. Both rows are WIRED above.
- **FU-12 Command palette and context menu actions.** No parity row names them
  directly, but `CommandPaletteEvent::Execute(_)` (`main.rs:502`) and
  `ContextMenuEvent::Selected(_)` (`main.rs:524`) are discarded, so both
  overlays are inert. This is the delivery mechanism several other units assume.

### P2 — window and lifecycle

- **FU-13 Window lifecycle.** Rows: `CloseWindow`, `QuitAll`, `WindowClosed`,
  `QuitRequested`, `ListWindows`, `WindowList`, `FocusChanged`.
  **Covered by bead .72 — do not re-file.** All seven are wired through
  `window_lifecycle.rs` and asserted on the wire by
  `tests/e2e/visual/window-lifecycle.sh`.
- **FU-14 Update surfaces in the terminal window.** Rows: `TriggerUpdate`,
  `DismissUpdate`, `UpdateAvailable`, `UpdateProgress`. (`CheckForUpdates` /
  `ListReleases` / `UpdateCheckResult` / `ReleaseList` are reachable from the
  settings window, which bead .82 made openable from inside the running
  client.)
- **FU-15 X11 focus guard.** Row: X11 focus guard. `x11_focus.rs` needs starting
  from `open_window`.

### P2 — remote, LAN, and sharing

The whole of features 013/014/015 is unreachable from the GPUI client.

- **FU-16 Remote (tailnet).** Rows: `RemoteHandshake`, `ListRemotePeers`,
  `GetRemoteEnv`, `RemoteHandshakeReply`, `RemotePeerList`, `RemoteEnv`,
  `RemoteDisconnect`, `WindowTakenOver`, `DispatchAction`, `RunAction`,
  `ActionDispatched`. **Landed.** `remote_handshake.rs` and `lost_control.rs`
  are now in `main.rs`'s import closure, and all seven inbound variants have
  explicit arms in `dispatch_server_message`, routed to `on_remote_message` and
  folded into a new shared `remote_chrome.rs::RemoteChrome`. The startup probe
  puts `GetRemoteEnv` (transient socket) and `ListRemotePeers` (session
  connection) on the wire when `remote.enabled`, and the Settings → Remote page
  reaches `GetRemoteEnv` a second way through `SettingsWindow::refresh_trust`.
  `SCRIBE_REMOTE_DIAL` reaches a tailnet peer over plain TCP and the mandatory
  `RemoteHandshake` preamble, with the picker's window claim and explicit-attach
  `takeover` riding the ordinary `Hello`. A `WindowTakenOver` freezes the window
  under `lost_control.rs::lost_control_overlay` — every keystroke suppressed but
  the Enter/click reclaim, which leaves as the v3 `ControlClaim` — and a
  `RemoteDisconnect` names its typed reason on the status strip. `RunAction` is
  queued for the foreground's lifecycle tick because the actions it names touch
  GPUI entities; `DispatchAction` is its outbound twin, sent when a feature-015
  viewer picks a window-mutating palette row the server would refuse from a
  non-controller. Verified on two wires and on screen by
  `tests/e2e/visual/remote-control.sh`, whose `scribe-test remote-peer` stand-in
  terminates the real TCP dial. The connect-picker OVERLAY remains unported:
  `remote.rs::RemoteConnect` has no GPUI view, so the peer lists it would render
  surface on the status strip instead.
- **FU-17 LAN (mTLS) dial and approval.** Rows: `LanHello`,
  `LanApprovalDecision`, `ListLanPeers`, `GetLanDialIdentity`,
  `LanApprovalPending`, `LanApprovalResult`, `LanApprovalRequest`, `LanPeerList`,
  `LanDialIdentity`, `LanEnv`, `GetLanEnv`. **Landed.** `lan_approval.rs` is now
  imported by `main.rs` and wrapped as `AnyDialog::LanApproval`, so the owning
  side's prompt is raised from a new shared `lan.rs::LanChrome` by the
  foreground tick and answered through `IpcSink::lan_approval_decision`. The six
  inbound LAN variants have explicit arms in `dispatch_server_message`, routed to
  `on_lan_message`; the startup probe puts `GetLanEnv` (transient socket) and
  `ListLanPeers` (session connection) on the wire when `remote.lan.enabled`. A
  new `lan_dial.rs` reaches a peer over TCP + pinned mutual TLS behind
  `SCRIBE_LAN_DIAL`, fetching the dial identity over `GetLanDialIdentity` and
  running the `LanHello` preamble and approval gate. Verified on two wires and
  on screen by `tests/e2e/visual/lan-approval.sh`, whose `scribe-test lan-peer`
  stand-in terminates a real mutual-TLS handshake with the server-owned
  `LanTls`.
- **FU-18 Trusted devices and networks in the settings window.** Rows:
  `ListTrustedDevices`, `RevokeTrustedDevice`, `ListTrustedNetworks`,
  `AddCurrentNetworkTrusted`, `RemoveTrustedNetwork`, `TrustedDeviceList`,
  `TrustedNetworkList`, `EnvPreflight`, `EnvPreflightResult`. **Landed.** The
  Remote page now leads with a runtime "Local network" section and a new
  Environment page owns the env-persistence opt-in; every one of those transport
  helpers is reached from `settings/window.rs::run_action`, the same live path
  that already served `CheckForUpdates` / `ListReleases`. `GetLanEnv` / `LanEnv`
  came along with the section; FU-17 has since wired the dial/approval rows and
  put both on the terminal window's live path as well.
  Wiring the path also surfaced a latent protocol defect: `PreflightError`'s
  `Unknown` variant was a newtype under `#[serde(tag = "type")]`, which msgpack
  cannot encode, so every failing `EnvPreflightResult` was dropped before it
  left the server. It is now a struct variant. Verified on the wire and on
  screen by `tests/e2e/visual/settings-trust.sh`.
- **FU-19 Sharing and control.** Rows: `ControlClaim`, `ControlRequest`,
  `ControlGrant`, `ShareRoster`, `ControlRequested`, `ControlDenied`,
  `ShareEnded`. **Landed.** `share.rs` is now imported by `main.rs` and
  `ipc_bridge.rs`; the four inbound notices have explicit arms in
  `dispatch_server_message`, and the claim/grant frames leave through
  `IpcSink::control_intent` from the viewer hint and the modal grant/deny
  prompt. Verified on the wire and on screen by
  `tests/e2e/visual/share-control.sh`.

### P2 — session lifecycle gaps

- **FU-20 Subscribe / snapshot tooling.** Rows: `Subscribe`, `RequestSnapshot`.
  **Landed.** Both frames now leave through `IpcSink`: `Subscribe` rides every
  attach (reader `attach_session` and the tab-switch `TerminalView::attach`),
  and `RequestSnapshot` is the display-only client's resync — sent after the
  post-font-reload `Resize` and as the fallback when a reattach replay fails to
  decode. The reply is consumed by the existing `ScreenSnapshot` arm, now
  `apply_screen_snapshot`. Verified on the wire and on screen by
  `tests/e2e/visual/session-tooling.sh`.
- **FU-21 Workspace notes on a real workspace.** Rows: `WorkspaceNotesSnapshot`,
  `WorkspaceNotesChanged`, plus de-demoing `WorkspaceNotesGet` /
  `WorkspaceNotesMutate`. **Landed.** The modal now opens on the workspace
  `TerminalView::notes_workspace_id` resolves from live state — the focused
  region when the server minted it, otherwise the focused tab's workspace — and
  declines to open at all when no server workspace is known yet, so no
  fabricated `WorkspaceId` reaches the wire. Both server answers have an arm:
  `on_workspace_notes_message` folds them into the shared
  `WorkspaceNotesStore` and `TerminalView::sync_workspace_notes` adopts them
  into the open modal on the next redraw, version-gated so a late snapshot
  never eats a typed draft. Verified on the wire and on screen by
  `tests/e2e/visual/workspace-notes.sh`.
- **FU-22 Bell.** Row: `Bell`. **Landed.** `bell.rs` is now in `main.rs`'s
  import closure and `ServerMessage::Bell` has its own reader arm. The reader
  queues the belling session; the window-lifecycle tick refreshes the gate's
  focus / focused-pane / update-in-flight inputs and drains the queue through
  `BellController`, and a surviving bell calls `Window::request_attention` —
  GPUI's equivalent of the winit client's `request_user_attention`. Verified on
  a real BEL byte and on the resulting `WM_HINTS` urgency flag by
  `tests/e2e/visual/bell.sh`.
- **FU-23 In-app settings entry point.** Row: `settings` action. **Closed by
  bead .82 — do not re-file.** `KeyAction::OpenSettings` now reaches
  `TerminalView::open_or_focus_settings`, which opens the same window
  `run_settings` opens for `--settings`; the palette row lowers onto the same
  key action, and the titlebar gear's `TitlebarEvent::OpenSettings` — emitted
  since the titlebar landed and never subscribed to — reaches it too. The
  retained `WindowHandle` raises the open window instead of stacking a second
  one. All three entry points and the no-duplicate rule are asserted against the
  mapped X11 window by `tests/e2e/visual/settings-entry.sh`.

### P1 — requirement-derived gaps (added 2026-07-27)

These five units come from the widened census above. None was reachable by the
`f56ef95` method, because none of their requirements had a parity row.

- **FU-24 Server-upgrade reattach.** Row: `Server-upgrade reattach` (`US2-4`).
  `start_ipc_thread` must supervise `run_connection` rather than await it once:
  redial with backoff when the stream closes, re-send `Hello` / `ListSessions`,
  and rebuild the topology through the existing `on_session_list` path. Highest
  priority of the five — it is the only one that breaks the multiplexer promise
  US2 is built on, and it fails silently on every server upgrade.
- **FU-25 AI indicator painting.** Row: `AI indicator borders and tab tint`
  (`US4-1`). The tracker's state half is wired and its painted half is not:
  `tab_indicator_color`, `workspace_border_color`, `pane_border_edges`, `tick`,
  `needs_animation` and `clear_stale_processing` need call sites on the render
  path and the idle tick. This is a differentiating feature, and the module
  ratchet cannot see it because `ai_indicator` is imported.
- **FU-26 Pane dividers and drag-resize.** Row: `Pane dividers and drag-resize`
  (`US3-10`). `divider.rs` needs a quad overlay in the pane shell and a pointer
  path that maps a drag back to a split ratio.
- **FU-27 Workspace notes hover preview.** Row: `Workspace notes hover preview`
  (`US4-3`). `workspace_notes_preview.rs` needs a hover trigger and a view; the
  notes modal it complements is already wired.
- **FU-28 Remote connect picker overlay.** Row: `Remote connect picker overlay`
  (`US4-4`). `remote::RemoteConnect` needs a GPUI view so the peer lists reach a
  picker rather than a status-strip count.

### In-flight bead coverage map

| Bead | Rows it covers |
| --- | --- |
| .56 (opacity) | Opacity |
| .58 (pane/workspace) | 8 pane-layout actions, 6 workspace-layout actions (not the 4 workspace IPC rows) |
| .59 (vi/smart-selection/split-scroll/zoom) | `scroll_up/down/top/bottom`, `zoom_in/out/reset`; vi mode, split-scroll, and the smart-selection context menu made reachable (selection groundwork for `copy`) |
| .61 (close_tab/new_window) | `close_tab`, `new_window` |
| .77 (FU-18 settings trust) | the nine FU-18 rows plus `GetLanEnv` / `LanEnv` |
| .76 (FU-17 LAN dial and approval) | all eleven FU-17 rows |
| .82 (FU-23 in-app settings entry) | the `settings` action, and the in-app reach of every settings-window row |

Everything else in the fix units above is currently unfiled.

## Gate methodology fix

The root cause is that `parity-inventory.md` declares a verification *method*
per row but never binds that method to a *live entry point*. A `gpui-test` can
instantiate a module directly, so it proves the module works and says nothing
about whether the app constructs it. Concretely:

### 1. Add a mandatory "reachable from" column

Every row gains a fourth column naming the live-path symbol that reaches it —
`main.rs:handle_layout_action`, `main.rs:run_reader`, `TerminalElement::paint`,
`settings/window.rs:run_action`. A row with an empty cell cannot be marked
`required`-and-done. This makes the gap in this audit impossible to reintroduce
silently, and it is cheap to verify.

### 2. Add a mechanical reachability check to CI

The decisive evidence in this audit came from three greps that a script can run:

- Every `ServerMessage` variant marked `required` must appear as a match arm in
  the live reader. Assert `run_reader`'s arm set equals the inventory's WIRED
  set; fail on drift. The `_ => {}` catch-all is the hazard — replace it with an
  explicit `tracing::warn!` arm listing the variant, so an unhandled message is
  observable at runtime instead of silent.
- Every `ClientMessage` variant marked `required` must be constructed in a file
  reachable from `main.rs`'s import closure. Compute that closure from the
  `use scribe_client_gpui::…` list and fail on any variant built only outside it.
- Every module in `lib.rs` must either be in `main.rs`'s import closure or carry
  an explicit `#![doc = "unwired: <bead-id>"]` marker. Today 35 of 54 modules
  would need markers — that count is itself the launch-gate metric.

### 3. Upgrade verification methods that cannot detect unreachability

These rows are currently `gpui-test` and must move to `scripted-E2E` or
`visual-E2E`, because a headless entity test passes identically whether or not
the app constructs the entity:

- **To `scripted-E2E`** (a real client + real server, asserting on the wire):
  `CreateWorkspace`, `MoveSession`, `ReportWorkspaceTree`, `SearchRequest`,
  `WorkspaceNotesGet`, `WorkspaceNotesMutate`, `GetRemoteEnv`,
  `ListTrustedDevices`, `ListTrustedNetworks`, `GetLanEnv`, `DismissUpdate`
  (the three trust queries have since been moved and are covered by
  `tests/e2e/visual/settings-trust.sh`; the outbound workspace frames and the
  inbound `WorkspaceInfo` by `tests/e2e/visual/workspace-ipc.sh`; the four
  workspace-notes rows by `tests/e2e/visual/workspace-notes.sh`);
  and inbound `CwdChanged`, `SessionContextChanged`, `WorkspaceInfo`,
  `WorkspaceNotesSnapshot`, `WorkspaceNotesChanged`, `SearchResults`,
  `EnvPreflightResult`, `PromptMark`, `ScrollBottom`, `TrustedDeviceList`,
  `TrustedNetworkList`, `LanEnv`, `RemoteEnv`, `UpdateCheckResult`,
  `ReleaseList`, `PromptReceived`.
- **To `visual-E2E`**: Font fallback (a Nerd Font glyph must actually render),
  and the whole "Input and keybinding checklist" — all 54 named actions must be
  driven through `xdotool` against the real window, not through
  `translate_key_action` in isolation. The existing
  `tests/e2e/func/keybindings-validation.sh` should be extended from validating
  the binding table to asserting each action's observable effect.
- **Keep `gpui-test` only** for the nine removed-config-key rows, which assert
  the *absence* of behaviour and are genuinely load-path tests
  (`config/tests.rs:29` is the right shape).

### 4. Make "no-op dispatch" a test failure, not a debug log

`main.rs:430` and `main.rs:799` swallow 26 `LayoutAction` variants and two
`KeyAction` variants behind `tracing::debug!`. Replace the catch-alls with
exhaustive matches whose unimplemented arms call a single
`unimplemented_action(action)` helper that logs at `warn` and increments a
counter. The scripted-E2E harness then asserts the counter is zero after a run
that exercises every binding, which turns "intercepted and dropped" from an
invisible state into a hard gate signal.

### 5. Re-baseline the launch gate

The 850-test suite should not be quoted as parity evidence again. The gate
metric should be the reachable-row count from this audit's table (currently
51/164 user-facing rows), regenerated mechanically by the checks in §2, with an
explicit go threshold.

### 6. Derive the row set from the requirement set (added 2026-07-27)

§1–§5 make every *tabulated* row prove reachability. They say nothing about
whether the table spans the requirements, and it did not: nine spec requirements
had no row, so no oracle scored them and the gate read 163 of 164 rows reachable
while nine requirements were missing from the product. A reachable-row count is
only a parity metric if the rows are the requirements.

The fix, landed with this amendment:

- `spec.md` carries a **requirement register** — every acceptance criterion and
  porting obligation tagged with a stable `US<n>-<n>` / `PO-<n>` id. Numbering
  is append-only so a row can cite an id permanently.
- `parity-inventory.md` carries a **coverage index** mapping every register id
  onto the row or rows that carry it, plus a `Spec behaviour requirements` table
  holding the rows no message, keybinding or rendering row already carried.
- `tools/check-parity-inventory.sh` fails when a register id has no carrying
  row, when the index names a row no table contains, and when the index names an
  id `spec.md` does not declare. Adding a requirement therefore breaks the build
  until someone gives it a row and a verdict.

The escape hatch is deliberate and narrow: tree, licensing and CI requirements
(`US5-*`, `US6-*`) are marked `not a parity row` with the artifact that gates
them, because no reachable client symbol can carry them.
