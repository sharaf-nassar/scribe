# GPUI client parity inventory

This committed oracle enumerates the frozen IPC, input, and UI surface the GPUI
client must preserve before cutover. It is derived from the legacy client and
`scribe-common` at the 016 planning baseline.

## Verification methods

Each row declares its intended parity oracle. `golden` is a captured
byte/serialization fixture; `gpui-test` is a headless `#[gpui::test]`;
`visual-E2E` is a deterministic screenshot comparison; `scripted-E2E` drives
the app and server; `manual` requires a human interaction or platform check.

## Client messages (46 sent)

Every `ClientMessage` variant from `crates/scribe-common/src/protocol.rs` must
remain serializable and be emitted by the corresponding GPUI interaction.

| Variant | Surface | Verification method | Status |
| --- | --- | --- | --- |
| `KeyInput` | terminal input | golden | required |
| `Resize` | terminal resize | scripted-E2E | required |
| `CreateSession` | tabs, panes, and AI tabs | scripted-E2E | required |
| `CloseSession` | pane/tab close | scripted-E2E | required |
| `CreateWorkspace` | workspace creation | gpui-test | required |
| `CloseWorkspace` | workspace close | scripted-E2E | required |
| `MoveSession` | session relocation | gpui-test | required |
| `Subscribe` | session stream subscription | scripted-E2E | required |
| `RequestSnapshot` | snapshot tooling | scripted-E2E | required |
| `ListSessions` | startup/reconnect | scripted-E2E | required |
| `AttachSessions` | reconnect restore | scripted-E2E | required |
| `ConfigReloaded` | live config reload | scripted-E2E | required |
| `ReportWorkspaceTree` | layout persistence | gpui-test | required |
| `SearchRequest` | find overlay | gpui-test | required |
| `WorkspaceNotesGet` | workspace notes | gpui-test | required |
| `WorkspaceNotesMutate` | workspace notes | gpui-test | required |
| `Hello` | registration/adoption | scripted-E2E | required |
| `CloseWindow` | close dialog | scripted-E2E | required |
| `QuitAll` | quit-all dialog | scripted-E2E | required |
| `TriggerUpdate` | update dialog | scripted-E2E | required |
| `DismissUpdate` | update dialog | gpui-test | required |
| `CheckForUpdates` | release settings | scripted-E2E | required |
| `ListReleases` | release settings | scripted-E2E | required |
| `ListWindows` | window management | scripted-E2E | required |
| `DispatchAction` | remote automation | scripted-E2E | required |
| `FocusChanged` | focus reporting | scripted-E2E | required |
| `HookEvent` | hook helper ingress | scripted-E2E | required |
| `EnvPreflight` | environment persistence | scripted-E2E | required |
| `ClipboardPromptResponse` | OSC 52 prompt | scripted-E2E | required |
| `ClipboardBridgeReadReply` | OSC 52 bridge | scripted-E2E | required |
| `RemoteHandshake` | tailnet connect | scripted-E2E | required |
| `ListRemotePeers` | remote connect picker | scripted-E2E | required |
| `GetRemoteEnv` | remote settings | gpui-test | required |
| `LanHello` | LAN connect | scripted-E2E | provisional — 015 reconcile |
| `LanApprovalDecision` | LAN approval dialog | scripted-E2E | provisional — 015 reconcile |
| `ListLanPeers` | LAN connect picker | scripted-E2E | provisional — 015 reconcile |
| `ListTrustedDevices` | LAN settings | gpui-test | provisional — 015 reconcile |
| `RevokeTrustedDevice` | LAN settings | scripted-E2E | provisional — 015 reconcile |
| `ListTrustedNetworks` | LAN settings | gpui-test | provisional — 015 reconcile |
| `AddCurrentNetworkTrusted` | LAN settings | scripted-E2E | provisional — 015 reconcile |
| `RemoveTrustedNetwork` | LAN settings | scripted-E2E | provisional — 015 reconcile |
| `GetLanEnv` | LAN settings | gpui-test | provisional — 015 reconcile |
| `GetLanDialIdentity` | LAN dialing | scripted-E2E | provisional — 015 reconcile |
| `ControlClaim` | shared-window control | scripted-E2E | provisional — 015 reconcile |
| `ControlRequest` | shared-window control | scripted-E2E | provisional — 015 reconcile |
| `ControlGrant` | shared-window control | scripted-E2E | provisional — 015 reconcile |

## Server messages (59 handled)

Every `ServerMessage` variant from `crates/scribe-common/src/protocol.rs` must
be handled without loss, including additive sharing and LAN variants.

The planning note named 57 variants; the frozen source at this inventory's
baseline contains 59. This table intentionally follows the source so neither
additive sharing variant is omitted.

| Variant | Surface | Verification method | Status |
| --- | --- | --- | --- |
| `PtyOutput` | terminal stream | golden | required |
| `ScreenSnapshot` | tooling snapshot | scripted-E2E | required |
| `SessionReplay` | reconnect replay | scripted-E2E | required |
| `AiStateChanged` | AI indicator | visual-E2E | required |
| `AiStateCleared` | AI indicator | visual-E2E | required |
| `CwdChanged` | tab metadata | gpui-test | required |
| `SessionContextChanged` | session metadata | gpui-test | required |
| `TitleChanged` | tab title | visual-E2E | required |
| `CodexTaskLabelChanged` | Codex tab label | visual-E2E | required |
| `CodexTaskLabelCleared` | Codex tab label | visual-E2E | required |
| `TaskLabelChanged` | AI tab label | visual-E2E | required |
| `TaskLabelCleared` | AI tab label | visual-E2E | required |
| `PromptReceived` | prompt history | gpui-test | required |
| `WorkspaceNamed` | workspace chrome | visual-E2E | required |
| `SessionCreated` | pane lifecycle | scripted-E2E | required |
| `SessionExited` | pane lifecycle | scripted-E2E | required |
| `Bell` | terminal bell | manual | required |
| `Error` | error presentation | visual-E2E | required |
| `GitBranch` | status/tab metadata | visual-E2E | required |
| `SessionList` | startup/reconnect | scripted-E2E | required |
| `WorkspaceInfo` | workspace layout | gpui-test | required |
| `WorkspaceNotesSnapshot` | workspace notes | gpui-test | required |
| `WorkspaceNotesChanged` | workspace notes | gpui-test | required |
| `SearchResults` | find overlay | gpui-test | required |
| `Welcome` | registration/adoption | scripted-E2E | required |
| `WindowClosed` | close lifecycle | scripted-E2E | required |
| `WindowList` | window management | scripted-E2E | required |
| `RunAction` | remote automation | scripted-E2E | required |
| `ActionDispatched` | remote automation | scripted-E2E | required |
| `QuitRequested` | quit dialog | scripted-E2E | required |
| `UpdateAvailable` | update dialog | visual-E2E | required |
| `UpdateProgress` | update dialog | visual-E2E | required |
| `UpdateCheckResult` | release settings | gpui-test | required |
| `ReleaseList` | release settings | gpui-test | required |
| `PromptMark` | prompt navigation | gpui-test | required |
| `TrimScrollback` | terminal history | golden | required |
| `ScrollBottom` | terminal viewport | gpui-test | required |
| `EnvPreflightResult` | environment settings | gpui-test | required |
| `EnvStatus` | environment status | visual-E2E | required |
| `ClipboardPromptRequest` | OSC 52 dialog | visual-E2E | required |
| `ClipboardBridgeWrite` | OSC 52 bridge | scripted-E2E | required |
| `ClipboardBridgeReadRequest` | OSC 52 bridge | scripted-E2E | required |
| `RemoteHandshakeReply` | tailnet connect | scripted-E2E | required |
| `WindowTakenOver` | remote-control landing | visual-E2E | required |
| `RemoteDisconnect` | remote-control landing | visual-E2E | required |
| `RemotePeerList` | remote connect picker | visual-E2E | required |
| `RemoteEnv` | remote settings | gpui-test | required |
| `LanApprovalPending` | LAN approval dialog | visual-E2E | provisional — 015 reconcile |
| `LanApprovalResult` | LAN approval dialog | visual-E2E | provisional — 015 reconcile |
| `LanApprovalRequest` | LAN approval dialog | visual-E2E | provisional — 015 reconcile |
| `LanPeerList` | LAN connect picker | visual-E2E | provisional — 015 reconcile |
| `TrustedDeviceList` | LAN settings | gpui-test | provisional — 015 reconcile |
| `TrustedNetworkList` | LAN settings | gpui-test | provisional — 015 reconcile |
| `LanEnv` | LAN settings | gpui-test | provisional — 015 reconcile |
| `LanDialIdentity` | LAN dialing | scripted-E2E | provisional — 015 reconcile |
| `ShareRoster` | shared-window roster | visual-E2E | provisional — 015 reconcile |
| `ControlRequested` | control request dialog | visual-E2E | provisional — 015 reconcile |
| `ControlDenied` | control request dialog | visual-E2E | provisional — 015 reconcile |
| `ShareEnded` | shared-window landing | visual-E2E | provisional — 015 reconcile |

## Input and keybinding checklist

The GPUI port retains every parsed `Bindings` action from
`crates/scribe-client/src/input.rs`. Each named action is verified by golden
fixtures when it emits terminal bytes; otherwise it has a headless GPUI test.

| Subsystem | Explicit actions | Verification method |
| --- | --- | --- |
| Pane layout | `split_vertical`, `split_horizontal`, `close_pane`, `cycle_pane`, `focus_left`, `focus_right`, `focus_up`, `focus_down` | gpui-test |
| Workspace layout | `workspace_split_vertical`, `workspace_split_horizontal`, `workspace_focus_left`, `workspace_focus_right`, `workspace_focus_up`, `workspace_focus_down` | gpui-test |
| Tabs and windows | `new_tab`, `new_claude_tab`, `new_claude_resume_tab`, `new_codex_tab`, `new_codex_resume_tab`, `close_tab`, `next_tab`, `prev_tab`, `select_tab_1`, `select_tab_2`, `select_tab_3`, `select_tab_4`, `select_tab_5`, `select_tab_6`, `select_tab_7`, `select_tab_8`, `select_tab_9`, `new_window` | gpui-test |
| Clipboard | `copy`, `paste` | scripted-E2E |
| Navigation | `scroll_up`, `scroll_down`, `scroll_top`, `scroll_bottom`, `find`, `prompt_jump_up`, `prompt_jump_down`, `jump_to_failure` | gpui-test |
| View and overlays | `zoom_in`, `zoom_out`, `zoom_reset`, `command_palette`, `settings` | gpui-test |
| Terminal shortcuts | `word_left`, `word_right`, `delete_word_backward`, `delete_word_backward_ctrl`, `delete_word_forward`, `line_start`, `line_end` | golden |

`LayoutAction` variants are explicitly: `SplitVertical`, `SplitHorizontal`,
`ClosePane`, `FocusNext`, `FocusLeft`, `FocusRight`, `FocusUp`, `FocusDown`,
`WorkspaceSplitVertical`, `WorkspaceSplitHorizontal`, `WorkspaceFocusLeft`,
`WorkspaceFocusRight`, `WorkspaceFocusUp`, `WorkspaceFocusDown`, `NewTab`,
`NewClaudeTab`, `NewClaudeResumeTab`, `NewCodexTab`, `NewCodexResumeTab`,
`CloseTab`, `NextTab`, `PrevTab`, `SelectTab`, `NewWindow`, `CopySelection`,
`PasteClipboard`, `ScrollUp`, `ScrollDown`, `ScrollTop`, `ScrollBottom`,
`PromptJumpUp`, `PromptJumpDown`, `JumpToFailure`, `ZoomIn`, `ZoomOut`, and
`ZoomReset` (all `gpui-test`). `KeyAction` variants are explicitly `Terminal`
(golden), `Layout` (gpui-test), `OpenSettings` (gpui-test),
`OpenCommandPalette` (gpui-test), and `OpenFind` (gpui-test).

## Removed configuration keys

These legacy appearance keys must deserialize harmlessly at cutover but have no
GPUI behavior. The table is intentionally narrow: only splash and bespoke
renderer-pipeline controls are removed; surviving settings retain their current
semantics.

| Legacy TOML key | Reason removed | Load behavior | Verification method |
| --- | --- | --- | --- |
| `appearance.splash` | GPUI cutover deletes the splash screen. | Silently ignored. | gpui-test |
| `appearance.splash_duration_ms` | GPUI cutover deletes splash timing. | Silently ignored. | gpui-test |
| `appearance.scrollbar_width` | Bespoke pipeline hover/geometry constant. | Silently ignored. | gpui-test |
| `appearance.scrollbar_color` | Bespoke renderer scrollbar colour override. | Silently ignored. | gpui-test |
| `appearance.prompt_bar_second_row_bg` | Bespoke prompt-bar pipeline colour override. | Silently ignored. | gpui-test |
| `appearance.prompt_bar_first_row_bg` | Bespoke prompt-bar pipeline colour override. | Silently ignored. | gpui-test |
| `appearance.prompt_bar_text` | Bespoke prompt-bar pipeline colour override. | Silently ignored. | gpui-test |
| `appearance.prompt_bar_icon_first` | Bespoke prompt-bar pipeline colour override. | Silently ignored. | gpui-test |
| `appearance.prompt_bar_icon_latest` | Bespoke prompt-bar pipeline colour override. | Silently ignored. | gpui-test |

## 015-derived provisional boundary

All rows labeled **provisional — 015 reconcile** cover feature 015 sharing
(roster and control passing) or its feature-014 LAN dialogs/settings. The
separate 015-reconcile bead owns their final semantics and removes this marker;
this inventory only preserves their current protocol and surface coverage.
