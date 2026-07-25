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
| `CreateWorkspace` | gpui-test | MISSING | no `ClientMessage::CreateWorkspace` anywhere in the crate |
| `CloseWorkspace` | scripted-E2E | MISSING | no `ClientMessage::CloseWorkspace` anywhere in the crate |
| `MoveSession` | gpui-test | MISSING | no `ClientMessage::MoveSession` anywhere in the crate |
| `Subscribe` | scripted-E2E | WIRED | `main.rs` `attach_session` and `TerminalView::attach` → `IpcSink::subscribe`, behind the `AttachSessions` on the same ordered channel; wired by bead .79 |
| `RequestSnapshot` | scripted-E2E | WIRED | `main.rs` `report_cell_metrics` (after the post-font-reload `Resize`) and `forward_replay` (replay-decode fallback) → `IpcSink::request_snapshot`; wired by bead .79 |
| `ListSessions` | scripted-E2E | WIRED | `main.rs:1297`, sent on every connect |
| `AttachSessions` | scripted-E2E | WIRED | `main.rs:1640` `attach_session`; `ipc_bridge.rs:259` from `attach` |
| `ConfigReloaded` | scripted-E2E | WIRED | `main.rs:347` in `apply_config_reload`; watcher wired by bead .57 (`a50a5f2`) |
| `ReportWorkspaceTree` | gpui-test | UNWIRED | built only in `workspace_tree.rs`; module not imported by `main.rs` |
| `SearchRequest` | gpui-test | MISSING | `search.rs` is a pure matcher; the message is never constructed |
| `WorkspaceNotesGet` | gpui-test | WIRED | `main.rs:558` `open_workspace_notes_modal` → `ipc_bridge.rs:281`. Demo-only: Ctrl+Shift+N, fabricated `WorkspaceId::new()` (`main.rs:561`), and the reply is dropped by the reader catch-all |
| `WorkspaceNotesMutate` | gpui-test | WIRED | `main.rs` `route_workspace_notes_action` → `ipc_bridge.rs:291`; same demo caveat |
| `Hello` | scripted-E2E | WIRED | `main.rs:1295`, sent on every connect |
| `CloseWindow` | scripted-E2E | WIRED (bead .72) | `TerminalView::route_close_action` → `IpcSink::close_window` |
| `QuitAll` | scripted-E2E | WIRED (bead .72) | `TerminalView::route_close_action` → `IpcSink::quit_all` |
| `TriggerUpdate` | scripted-E2E | UNWIRED | `settings/server_action.rs:81` `request_trigger_update` has no caller |
| `DismissUpdate` | gpui-test | MISSING | never constructed anywhere in the crate |
| `CheckForUpdates` | scripted-E2E | WIRED | `settings/window.rs:161` from `action.check_for_updates` (`settings/model.rs:386`). Reachable only via `scribe-client-gpui --settings`; the in-app `settings` shortcut is swallowed |
| `ListReleases` | scripted-E2E | WIRED | `settings/window.rs:165` from `action.list_releases` (`settings/model.rs:387`); same `--settings`-only caveat |
| `ListWindows` | scripted-E2E | WIRED (bead .72) | `TerminalView::poll_window_list` → `IpcSink::list_windows` |
| `DispatchAction` | scripted-E2E | MISSING | not in the GPUI client; `scribe-cli` is the only sender |
| `FocusChanged` | scripted-E2E | WIRED (bead .72) | `TerminalView::report_focus` → `IpcSink::focus_changed` |
| `HookEvent` | scripted-E2E | WIRED | out-of-client by design: `crates/scribe-hook-helper/src/main.rs:119` |
| `EnvPreflight` | scripted-E2E | WIRED | `settings/window.rs` `run_action` (`action.env_preflight`) and the gated `enable_env_persistence` ON transition; same `--settings`-only caveat. Asserted on the wire by `tests/e2e/visual/settings-trust.sh` |
| `ClipboardPromptResponse` | scripted-E2E | UNWIRED | built in `clipboard.rs`; module not imported by `main.rs` |
| `ClipboardBridgeReadReply` | scripted-E2E | UNWIRED | built in `clipboard.rs`; module not imported by `main.rs` |
| `RemoteHandshake` | scripted-E2E | UNWIRED | built in `remote_handshake.rs`; module not imported by `main.rs` |
| `ListRemotePeers` | scripted-E2E | MISSING | never constructed anywhere in the crate |
| `GetRemoteEnv` | gpui-test | UNWIRED | `settings/server_action.rs:213` `request_remote_env` has no caller |
| `LanHello` | scripted-E2E | MISSING | never constructed anywhere in the crate |
| `LanApprovalDecision` | scripted-E2E | UNWIRED | built in `lan_approval.rs`; module not imported by `main.rs` |
| `ListLanPeers` | scripted-E2E | MISSING | never constructed anywhere in the crate |
| `ListTrustedDevices` | scripted-E2E | WIRED | `settings/window.rs` `refresh_trust`, reached from `run_action` (`action.refresh_trust`) and the first visit to the Remote page; same `--settings`-only caveat. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `RevokeTrustedDevice` | scripted-E2E | WIRED | `settings/window.rs` `run_action` (`action.revoke_trusted_device:<hex>` from each approved-device row); same `--settings`-only caveat. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `ListTrustedNetworks` | scripted-E2E | WIRED | `settings/window.rs` `refresh_trust`, same callers as `ListTrustedDevices`; same `--settings`-only caveat. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `AddCurrentNetworkTrusted` | scripted-E2E | WIRED | `settings/window.rs` `run_action` (`action.add_current_network`); same `--settings`-only caveat. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `RemoveTrustedNetwork` | scripted-E2E | WIRED | `settings/window.rs` `run_action` (`action.remove_trusted_network:<id>` from each trusted-network row); same `--settings`-only caveat. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `GetLanEnv` | scripted-E2E | WIRED | `settings/window.rs` `refresh_trust`, alongside the two trust list queries; same `--settings`-only caveat. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `GetLanDialIdentity` | scripted-E2E | MISSING | never constructed anywhere in the crate |
| `ControlClaim` | scripted-E2E | UNWIRED | built in `share.rs`; module not imported by `main.rs` |
| `ControlRequest` | golden | UNWIRED | built in `share.rs`, unimported. Not emitting it is by design, but its live substitute `ControlClaim` is itself UNWIRED, so the sharing surface is unreachable either way |
| `ControlGrant` | scripted-E2E | UNWIRED | built in `share.rs`; module not imported by `main.rs` |

**Client-message subtotals:** WIRED 15 · UNWIRED 17 · MISSING 14 · UNKNOWN 0.
(Was WIRED 13 · MISSING 16 at `f56ef95`; bead `.79` moved `Subscribe` and
`RequestSnapshot` from MISSING to WIRED.)

## Server messages (59)

The live reader (`main.rs:1476`) matches exactly twelve variants and ends in
`_ => {}` at `main.rs:1564`. Two more (`UpdateCheckResult`, `ReleaseList`) are
consumed by the separate `--settings` window's synchronous request/reply helper.
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
| `Bell` | manual | UNWIRED | `bell.rs` implements the routing; module not imported by `main.rs` |
| `Error` | visual-E2E | WIRED | `run_reader` arm → `set_status` |
| `GitBranch` | visual-E2E | MISSING | no reference; `StatusBarData.git_branch` hardcoded `None` |
| `SessionList` | scripted-E2E | WIRED | `run_reader` arm → `sync_tab_strip` |
| `WorkspaceInfo` | gpui-test | MISSING | only a doc-comment mention at `workspace_layout.rs:92`; never matched |
| `WorkspaceNotesSnapshot` | gpui-test | MISSING | no reference. The modal sends `WorkspaceNotesGet` and never receives a reply |
| `WorkspaceNotesChanged` | gpui-test | MISSING | no reference in the crate |
| `SearchResults` | gpui-test | MISSING | no reference in the crate |
| `Welcome` | scripted-E2E | WIRED | `run_reader` arm → `SessionRegistry::adopt_window` |
| `WindowClosed` | scripted-E2E | WIRED (bead .72) | `on_window_lifecycle_message` → `WindowLifecycle::on_window_closed` |
| `WindowList` | scripted-E2E | WIRED (bead .72) | `on_window_lifecycle_message` → `WindowLifecycle::set_windows` |
| `RunAction` | scripted-E2E | MISSING | no reference in the crate |
| `ActionDispatched` | scripted-E2E | MISSING | no reference in the crate |
| `QuitRequested` | scripted-E2E | WIRED (bead .72) | `on_window_lifecycle_message` → `WindowLifecycle::on_quit_requested` |
| `UpdateAvailable` | visual-E2E | MISSING | no reference; `StatusBarData.update_available` hardcoded `None` |
| `UpdateProgress` | visual-E2E | MISSING | no reference; `StatusBarData.update_progress` hardcoded `None` |
| `UpdateCheckResult` | gpui-test | WIRED | `settings/server_action.rs:46`, reached from `settings/window.rs:161` (`--settings` only) |
| `ReleaseList` | gpui-test | WIRED | `settings/server_action.rs:124`, reached from `settings/window.rs:165` (`--settings` only) |
| `PromptMark` | gpui-test | MISSING | no reference. `session_lifecycle` tracks trim offsets but no marks are ever ingested |
| `TrimScrollback` | golden | WIRED | `run_reader` arm → `SessionRegistry::on_trim_scrollback` |
| `ScrollBottom` | gpui-test | MISSING | no `ServerMessage::ScrollBottom` reference (the `keybindings.rs` hit is `LayoutAction::ScrollBottom`) |
| `EnvPreflightResult` | scripted-E2E | WIRED | parsed by `parse_env_preflight_response` and rendered into the Environment page's status line; same `--settings`-only caveat. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `EnvStatus` | visual-E2E | MISSING | no reference; `StatusBarData.env_status` hardcoded `None` |
| `ClipboardPromptRequest` | visual-E2E | MISSING | no reference. The `ClipboardDialog` demo is built from literals at `main.rs:721` |
| `ClipboardBridgeWrite` | scripted-E2E | UNWIRED | handled in `clipboard.rs`; module not imported by `main.rs` |
| `ClipboardBridgeReadRequest` | scripted-E2E | UNWIRED | handled in `clipboard.rs`; module not imported by `main.rs` |
| `RemoteHandshakeReply` | scripted-E2E | UNWIRED | handled in `remote_handshake.rs`; module not imported by `main.rs` |
| `WindowTakenOver` | visual-E2E | UNWIRED | handled in `lost_control.rs`; module not imported by `main.rs` |
| `RemoteDisconnect` | visual-E2E | MISSING | no reference in the crate |
| `RemotePeerList` | visual-E2E | MISSING | no reference in the crate |
| `RemoteEnv` | gpui-test | UNWIRED | parsed at `settings/server_action.rs:242`; request function has no caller |
| `LanApprovalPending` | visual-E2E | MISSING | no reference in the crate |
| `LanApprovalResult` | visual-E2E | MISSING | no reference in the crate |
| `LanApprovalRequest` | visual-E2E | UNWIRED | handled in `lan_approval.rs`; module not imported by `main.rs` |
| `LanPeerList` | visual-E2E | MISSING | no reference in the crate |
| `TrustedDeviceList` | scripted-E2E | WIRED | parsed by `parse_trusted_devices_response` and rendered as the Remote page's approved-device rows; same `--settings`-only caveat. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `TrustedNetworkList` | scripted-E2E | WIRED | parsed by `parse_trusted_networks_response` and rendered as the Remote page's trusted-network rows; same `--settings`-only caveat. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `LanEnv` | scripted-E2E | WIRED | parsed by `parse_lan_env_response` and rendered as the Remote page's own-fingerprint / addability notes; same `--settings`-only caveat. Asserted by `tests/e2e/visual/settings-trust.sh` |
| `LanDialIdentity` | scripted-E2E | MISSING | no reference in the crate |
| `ShareRoster` | visual-E2E | UNWIRED | handled in `share.rs`; module not imported by `main.rs` |
| `ControlRequested` | visual-E2E | UNWIRED | handled in `share.rs`; module not imported by `main.rs` |
| `ControlDenied` | visual-E2E | MISSING | no reference in the crate |
| `ShareEnded` | visual-E2E | MISSING | no reference in the crate |

**Server-message subtotals:** WIRED 14 · UNWIRED 13 · MISSING 32 · UNKNOWN 0.

## Input and keybinding actions (54 named actions)

Every named action parses and translates correctly — `translate_key_action`
(`keybindings.rs:420`) maps all 54 onto a `KeyAction`. The break is at the
dispatch site: `handle_layout_action` (`main.rs:409`) implements nine `LayoutAction`
variants and sends the other twenty-six to `_ => tracing::debug!` at
`main.rs:430`, where they are swallowed (correctly never leaked to the PTY, but
also never executed).

| Action | Subsystem | Verdict | Evidence |
| --- | --- | --- | --- |
| `split_vertical` | Pane layout | UNWIRED | `LayoutAction::SplitVertical` hits `main.rs:430`; `layout.rs`/`pane_tree.rs` unimported |
| `split_horizontal` | Pane layout | UNWIRED | as above |
| `close_pane` | Pane layout | UNWIRED | as above |
| `cycle_pane` | Pane layout | UNWIRED | `LayoutAction::FocusNext` hits `main.rs:430` |
| `focus_left` | Pane layout | UNWIRED | hits `main.rs:430` |
| `focus_right` | Pane layout | UNWIRED | hits `main.rs:430` |
| `focus_up` | Pane layout | UNWIRED | hits `main.rs:430` |
| `focus_down` | Pane layout | UNWIRED | hits `main.rs:430` |
| `workspace_split_vertical` | Workspace layout | UNWIRED | hits `main.rs:430`; `workspace_layout.rs`/`workspace_tree.rs` unimported |
| `workspace_split_horizontal` | Workspace layout | UNWIRED | as above |
| `workspace_focus_left` | Workspace layout | UNWIRED | as above |
| `workspace_focus_right` | Workspace layout | UNWIRED | as above |
| `workspace_focus_up` | Workspace layout | UNWIRED | as above |
| `workspace_focus_down` | Workspace layout | UNWIRED | as above |
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
| `scroll_up` | Navigation | UNWIRED | hits `main.rs:430`; `split_scroll.rs` unimported |
| `scroll_down` | Navigation | UNWIRED | as above |
| `scroll_top` | Navigation | UNWIRED | as above |
| `scroll_bottom` | Navigation | UNWIRED | as above |
| `find` | Navigation | UNWIRED | `KeyAction::OpenFind` swallowed at `main.rs:799`; `search.rs` unimported |
| `prompt_jump_up` | Navigation | UNWIRED | hits `main.rs:430`; no prompt marks are ingested |
| `prompt_jump_down` | Navigation | UNWIRED | as above |
| `jump_to_failure` | Navigation | UNWIRED | as above |
| `zoom_in` | View and overlays | UNWIRED | hits `main.rs:430`; `zoom.rs` unimported |
| `zoom_out` | View and overlays | UNWIRED | as above |
| `zoom_reset` | View and overlays | UNWIRED | as above |
| `command_palette` | View and overlays | WIRED | `main.rs:700` opens the overlay. Degenerate: `CommandPaletteEvent::Execute(_)` is discarded at `main.rs:502`, so no palette entry does anything |
| `settings` | View and overlays | UNWIRED | `KeyAction::OpenSettings` swallowed at `main.rs:799`; the settings window opens only via the `--settings` CLI flag (`main.rs:1199` `run_settings`) |
| `word_left` | Terminal shortcuts | WIRED | `keybindings.rs:477` → `KeyAction::Terminal` → `main.rs:795` `send_key_bytes` |
| `word_right` | Terminal shortcuts | WIRED | `keybindings.rs:478` → same path |
| `delete_word_backward` | Terminal shortcuts | WIRED | `keybindings.rs:479` → same path |
| `delete_word_backward_ctrl` | Terminal shortcuts | WIRED | `keybindings.rs:481` → same path |
| `delete_word_forward` | Terminal shortcuts | WIRED | `keybindings.rs:484` → same path |
| `line_start` | Terminal shortcuts | WIRED | `keybindings.rs:485` → same path |
| `line_end` | Terminal shortcuts | WIRED | `keybindings.rs:486` → same path |

`KeyAction` variants: `Terminal` WIRED (`main.rs:795`), `Layout` partially wired
(9 of 35 `LayoutAction` variants), `OpenCommandPalette` WIRED (claimed earlier in
`handle_overlay_key`, `main.rs:700`), `OpenSettings` UNWIRED, `OpenFind` UNWIRED.

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
  `cycle_pane`, `focus_left/right/up/down`. **Covered by bead .58.**
- **FU-6 Workspace tree.** Rows: `workspace_split_vertical`,
  `workspace_split_horizontal`, `workspace_focus_left/right/up/down`,
  `CreateWorkspace`, `CloseWorkspace`, `MoveSession`, `ReportWorkspaceTree`,
  `WorkspaceInfo`. **Partly covered by bead .58** (which is scoped to
  `pane_tree`/`workspace_layout` wiring); the four `ClientMessage`/`ServerMessage`
  rows are *not* named in .58 and need a follow-on.
- **FU-7 Scrollback navigation and marks.** Rows: `scroll_up`, `scroll_down`,
  `scroll_top`, `scroll_bottom`, `prompt_jump_up`, `prompt_jump_down`,
  `jump_to_failure`, `PromptMark`, `ScrollBottom`. **Partly covered by bead .59**
  (`split_scroll`); the prompt-mark ingestion rows are not.
- **FU-8 Clipboard and selection.** Rows: `copy`, `paste`,
  `ClipboardPromptResponse`, `ClipboardBridgeReadReply`,
  `ClipboardPromptRequest`, `ClipboardBridgeWrite`,
  `ClipboardBridgeReadRequest`. Requires wiring `clipboard.rs`, `selection.rs`,
  `paste.rs`, and routing `DialogEvent::Chosen` (`main.rs:542`) to a real
  response. **Selection is partly covered by bead .59** (`smart_selection`).
- **FU-9 Find overlay.** Rows: `find`, `SearchRequest`, `SearchResults`.
- **FU-10 Zoom.** Rows: `zoom_in`, `zoom_out`, `zoom_reset`.
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
  `ListReleases` / `UpdateCheckResult` / `ReleaseList` are reachable, but only
  from the `--settings` window.)
- **FU-15 X11 focus guard.** Row: X11 focus guard. `x11_focus.rs` needs starting
  from `open_window`.

### P2 — remote, LAN, and sharing

The whole of features 013/014/015 is unreachable from the GPUI client.

- **FU-16 Remote (tailnet).** Rows: `RemoteHandshake`, `ListRemotePeers`,
  `GetRemoteEnv`, `RemoteHandshakeReply`, `RemotePeerList`, `RemoteEnv`,
  `RemoteDisconnect`, `WindowTakenOver`, `DispatchAction`, `RunAction`,
  `ActionDispatched`.
- **FU-17 LAN (mTLS) dial and approval.** Rows: `LanHello`,
  `LanApprovalDecision`, `ListLanPeers`, `GetLanDialIdentity`,
  `LanApprovalPending`, `LanApprovalResult`, `LanApprovalRequest`, `LanPeerList`,
  `LanDialIdentity`, `LanEnv`, `GetLanEnv`.
- **FU-18 Trusted devices and networks in the settings window.** Rows:
  `ListTrustedDevices`, `RevokeTrustedDevice`, `ListTrustedNetworks`,
  `AddCurrentNetworkTrusted`, `RemoveTrustedNetwork`, `TrustedDeviceList`,
  `TrustedNetworkList`, `EnvPreflight`, `EnvPreflightResult`. **Landed.** The
  Remote page now leads with a runtime "Local network" section and a new
  Environment page owns the env-persistence opt-in; every one of those transport
  helpers is reached from `settings/window.rs::run_action`, the same live path
  that already served `CheckForUpdates` / `ListReleases`. `GetLanEnv` / `LanEnv`
  came along with the section, so FU-17 is down to the dial/approval rows.
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
  `WorkspaceNotesMutate` (fabricated `WorkspaceId` at `main.rs:561`, reply
  dropped by the reader catch-all).
- **FU-22 Bell.** Row: `Bell`.
- **FU-23 In-app settings entry point.** Row: `settings` action. The settings
  window exists and works but has no in-app trigger.

### In-flight bead coverage map

| Bead | Rows it covers |
| --- | --- |
| .56 (opacity) | Opacity |
| .58 (pane/workspace) | 8 pane-layout actions, 6 workspace-layout actions (not the 4 workspace IPC rows) |
| .59 (vi/smart-selection/split-scroll) | `scroll_up/down/top/bottom`; selection groundwork for `copy` |
| .61 (close_tab/new_window) | `close_tab`, `new_window` |
| .77 (FU-18 settings trust) | the nine FU-18 rows plus `GetLanEnv` / `LanEnv` |

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
  `tests/e2e/visual/settings-trust.sh`);
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
