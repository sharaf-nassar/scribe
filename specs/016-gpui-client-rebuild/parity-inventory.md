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
| `LanHello` | mTLS LAN-dial preamble before session attachment | scripted-E2E | required |
| `LanApprovalDecision` | owner-side fingerprint approval overlay | scripted-E2E | required |
| `ListLanPeers` | merged Local network source in remote connect picker | scripted-E2E | required |
| `ListTrustedDevices` | Remote settings trusted-device list | gpui-test | required |
| `RevokeTrustedDevice` | Remote settings device-revocation action | scripted-E2E | required |
| `ListTrustedNetworks` | Remote settings trusted-network list | gpui-test | required |
| `AddCurrentNetworkTrusted` | Remote settings trust-current-network action | scripted-E2E | required |
| `RemoveTrustedNetwork` | Remote settings trusted-network removal action | scripted-E2E | required |
| `GetLanEnv` | Remote settings LAN listener/environment summary | gpui-test | required |
| `GetLanDialIdentity` | local-server identity fetch before mTLS dialing | scripted-E2E | required |
| `ControlClaim` | viewer claim/request affordance for a shared window | scripted-E2E | required |
| `ControlRequest` | v3 compatibility alias; the client emits `ControlClaim` | golden | required |
| `ControlGrant` | holder grant/deny prompt for a control request | scripted-E2E | required |

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
| `LanApprovalPending` | cancelable connecting-side waiting-for-approval overlay | visual-E2E | required |
| `LanApprovalResult` | terminal LAN dial acceptance/refusal outcome | visual-E2E | required |
| `LanApprovalRequest` | owner-side device fingerprint approval overlay | visual-E2E | required |
| `LanPeerList` | Local network entries merged into remote connect picker | visual-E2E | required |
| `TrustedDeviceList` | Remote settings trusted-device rows | gpui-test | required |
| `TrustedNetworkList` | Remote settings trusted-network rows | gpui-test | required |
| `LanEnv` | Remote settings LAN listener/environment summary | gpui-test | required |
| `LanDialIdentity` | client mTLS identity returned by the local server | scripted-E2E | required |
| `ShareRoster` | presence badge and live-viewer role/claim state | visual-E2E | required |
| `ControlRequested` | holder or owner grant/deny control prompt | visual-E2E | required |
| `ControlDenied` | requester control-denied notice | visual-E2E | required |
| `ShareEnded` | shared-viewer end landing and state cleanup | visual-E2E | required |

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

## Rendering and window checklist

These spike-resolved rendering and native-window requirements preserve the
legacy terminal's output and platform behavior through the GPUI cutover.

| Surface | Required behavior | Verification method | Status |
| --- | --- | --- | --- |
| Box drawing | U+2500–U+259F cells bypass text shaping and use the existing procedural alpha-mask rasterizer through a `TerminalElement` paint-quad overlay after backgrounds and before text. | visual-E2E | required |
| Font fallback | Every terminal run uses `FontFallbacks::from_fonts` with `Symbols Nerd Font Mono`, `Symbols Nerd Font`, `Nerd Font Symbols Mono`, and `Nerd Font Symbols` before existing generic fallbacks; `Unifont Sample` remains excluded. | gpui-test | required |
| Ligatures | `appearance.ligatures` keeps its semantics: same-style runs call `shape_line` with `Some(cell_width)` and disable `calt` only when false, without drifting later cell origins. | visual-E2E | required |
| Opacity | `appearance.opacity` is clamped to `0.0..=1.0`; Wayland and composited X11 repaint alpha-aware terminal and chrome backgrounds live on a transparent surface, without restart. | manual | required |
| X11 focus guard | The guard reads GPUI's `RawWindowHandle::Xcb` XID and compares it directly with `_NET_ACTIVE_WINDOW`; non-X11 backends do not enable the guard. | scripted-E2E | required |

## Removed configuration keys

These legacy appearance keys must deserialize harmlessly at cutover but have no
GPUI behavior. The table is intentionally narrow: only splash and bespoke
renderer-pipeline controls are removed. The spikes retain
`appearance.ligatures` and `appearance.opacity` with their current semantics,
so neither belongs in this table.

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

## LAN and sharing boundary

Feature 015 is present in `fd04540` (`feat: remote window control and
multi-machine sharing`). The rows above follow its final client dispatch:
`ipc_client.rs` performs the LAN handshake and maps its outcomes, `main.rs`
renders the LAN and sharing states, and `share_view.rs` supplies the roster and
control UI. `ControlRequest` remains a serializable protocol alias because the
server handles it as `ControlClaim`; the client deliberately emits only the
latter.
