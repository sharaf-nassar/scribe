# Rebased Edit List: Remove Workspace Notes

This is the authoritative edit inventory for the removal. It was surveyed at
`ac46d1c6413aaab1cb52c687cb3501aec29e92ba`; symbols, headings, and literal
text govern, while the current line coordinates below are only navigation aids.

**2026-08-02 supersession:** the source-removal inventory remains authoritative,
but its manual two-flavor data-deletion order is superseded by the current
`spec.md` and `plan.md`. Debian `postinst` now retries one flavor-scoped,
fail-closed migration per configure; agents do not run the host lifecycle.

## Survey verdicts

- `lat search` resolved the live notes, overlay-chord, and visual-E2E sections;
  `lat expand '$implement-ready remove-workspace-notes'` contained no wiki refs
  to expand.
- `ArchiveReason` has no non-notes consumer. Every non-spec match is in
  `protocol.rs`, the three outright-deleted client notes modules, the outright-
  deleted server notes module, or the notes-only `main.rs` surface listed below.
- `crates/scribe-test/src/daemon.rs` uses one grouped or-pattern for notice
  messages. Delete only the two notes-variant alternatives; the arm survives.
- `server_message_variant` has two independent match arms, not a grouped
  or-pattern: `WorkspaceNotesSnapshot` at `main.rs:8170` and
  `WorkspaceNotesChanged` at `main.rs:8171`. The reader dispatch table is the
  grouped or-pattern at `main.rs:8406-8407`.
- `titlebar.rs` interleaves window movement and notes hover in one root
  `on_mouse_move`: `advance_move_arm` and its early return are at `:900-902`,
  `update_drag` is at `:903-905`, and the exact notes-only trailing band is
  `:906-910` (`let width`, `let x`, and the
  `width - 188.0..width - 154.0` emit). Delete only `:906-910`.
- `OVERLAY_CHORDS` has the literal type arity `5` at `keybindings.rs:493`.
  Removing the notes entry at `:497` changes that literal to `4`.
- The Just recipe is named exactly `e2e-visual-workspace-notes` at
  `justfile:282`; its notes-only comment block is `:276-281` and body is `:283`.
- Four retired-winit identifiers are `lat.md/client.md`-only:
  `adding_note_states`, `focused_inline_editor`,
  `affordance_hovered_workspace`, and `draw_affordance`. Two other literals are
  not lat-only: `workspace_notes_save_pending` occurs in source doc prose in the
  outright-deleted `workspace_notes_modal.rs:839`, and `PreviewLayout` occurs in
  source doc prose in the outright-deleted `workspace_notes_preview.rs:52`.
- Current count gates are exactly `67/67` modules, `54/59` server messages, and
  `36/36` layout actions. Current parity is `204` rows, all reachable, with
  `195` user-facing rows (`194` reachable in-client). These values were
  re-derived by both checker scripts, not copied from the pre-rebase survey.

## Delete outright

Delete each complete file. The counts still match the old survey, but are not
criteria.

| Path | Current lines | Located constructs |
|---|---:|---|
| `crates/scribe-client/src/workspace_notes.rs` | 431 | `AddingNoteState`, `WorkspaceNotesStore`, wrap/caret helpers, inline tests |
| `crates/scribe-client/src/workspace_notes_modal.rs` | 907 | modal view/state, `DraftDebounce`, `DraftDebounceEvent`, `WORKSPACE_NOTES_DEBOUNCE` |
| `crates/scribe-client/src/workspace_notes_modal/tests.rs` | 227 | modal/debounce unit tests |
| `crates/scribe-client/src/workspace_notes_preview.rs` | 523 | preview entity, `OpenEditor`, `FocusEditor`, `set_inline_editor`, preview layout/rendering |
| `crates/scribe-client/src/workspace_notes_preview/tests.rs` | 105 | preview unit tests |
| `crates/scribe-server/src/workspace_notes.rs` | 436 | `PersistedWorkspaceNotes`, `WorkspaceNotesStore`, private atomic TOML writer |
| `tests/e2e/visual/workspace-notes.sh` | 553 | sole workspace-notes visual oracle |

Keep `specs/004-workspace-notes/` and `specs/007-add-note-from-hover/` as
historical records. Only the separately named stale cross-references are
rewritten.

## Protocol surface

All entries are in `crates/scribe-common/src/protocol.rs` and are removed by
symbol. There is no `ServerError` enum anywhere under `crates/`; note failures
use the surviving generic `ServerMessage::Error`.

| Verdict | Construct | Current location |
|---|---|---:|
| Remove | `WorkspaceNoteStatus` | `:206-210` |
| Remove | `ArchiveReason` | `:213-216` |
| Remove | `WorkspaceNoteEntry` | `:219-230` |
| Remove | `WorkspaceNoteDraft` | `:233-238` |
| Remove | `WorkspaceNotesCollection` | `:241-250` |
| Remove | `WorkspaceNotesMutation` | `:253-259` |
| Remove | `ClientMessage::WorkspaceNotesGet` | `:368-371` |
| Remove | `ClientMessage::WorkspaceNotesMutate` | `:372-375` |
| Remove | `ServerMessage::WorkspaceNotesSnapshot` | `:729-732` |
| Remove | `ServerMessage::WorkspaceNotesChanged` | `:733-736` |
| Preserve | `REMOTE_PROTOCOL_VERSION = 3` | `:27` |
| Preserve | `#[serde(tag = "type")]` on `ClientMessage` | `:264-265` |
| Preserve | `#[serde(tag = "type")]` on `ServerMessage` | `:601-602` |
| Preserve | `ClientMessage::Hello` fields | `:379-390` |

The exact enum declarations and four-space variant indentation must survive so
the reachability checker can parse them.

## Server removal

### `crates/scribe-server/src/ipc_server.rs`

| Verdict | Construct | Current location |
|---|---|---:|
| Trim | protocol import `WorkspaceNotesMutation` | `:35` |
| Remove | `use crate::workspace_notes::WorkspaceNotesStore` | `:65` |
| Remove | `ServerState::workspace_notes` field and prose | `:954-955` |
| Trim | top-level dispatch alternatives `WorkspaceNotesGet` / `WorkspaceNotesMutate` | `:5867-5868` |
| Remove | `dispatch_workspace_message` notes arms | `:6101-6117` |
| Remove | `handle_workspace_notes_get` | `:7761-7768` |
| Remove | `handle_workspace_notes_mutate` | `:7770-7784` |
| Remove | `broadcast_workspace_notes_changed` | `:7786-7793` |
| Reword | `connected_window_writers` doc prose naming workspace-notes changes | `:7796-7798` |
| Preserve | `connected_window_writers` itself | `:7800` |

`connected_window_writers` remains live for `QuitRequested` at `:8488-8490`,
share rosters, and updater notices. `requires_window_control` at `:5455` does
not contain either notes message, confirming the notes routes are remotely
visible rather than locally gated.

### Module wiring

| Verdict | Construct | Current location |
|---|---|---:|
| Remove | `mod workspace_notes` | `crates/scribe-server/src/main.rs:56` |
| Remove | notes-store load | `crates/scribe-server/src/main.rs:226-227` |
| Remove | `ServerState` notes initializer | `crates/scribe-server/src/main.rs:264` |
| Remove | `pub mod workspace_notes` | `crates/scribe-server/src/lib.rs:29` |
| Remove | `pub mod workspace_notes` | `crates/scribe-client/src/lib.rs:119` |
| Remove | `pub mod workspace_notes_modal` | `crates/scribe-client/src/lib.rs:120` |
| Remove | `pub mod workspace_notes_preview` | `crates/scribe-client/src/lib.rs:121` |

## Client keybindings and titlebar

### `crates/scribe-client/src/keybindings.rs`

| Verdict | Construct | Current location |
|---|---|---:|
| Remove | `OverlayChord::WorkspaceNotes` and its doc comment | `:474-475` |
| Remove | `("ctrl+shift+m", OverlayChord::WorkspaceNotes)` | `:497` |
| Edit | `OVERLAY_CHORDS` arity literal `5` to `4` | `:493` |
| Preserve | remaining entries: tooltip, close, clipboard, vi mode | `:494-498` |

`crates/scribe-client/src/keybindings/tests.rs` is not edited; its iteration over
`OVERLAY_CHORDS` at `:251` automatically checks the shortened table.

### `crates/scribe-client/src/titlebar.rs`

| Verdict | Construct | Current location |
|---|---|---:|
| Remove | `TitlebarEvent::WorkspaceNotesHover` | `:61-62` |
| Remove | `TitlebarEvent::OpenWorkspaceNotes` | `:63-64` |
| Remove | `TitlebarView::notes_focus_handle` | `:135` |
| Remove | focus-handle initialization | `:160` |
| Trim | one `has_keyboard_focus` clause only | `:332` |
| Remove | `render_workspace_notes_button` including its stop-propagation guard | `:681-726` |
| Remove | local `workspace_notes` render call | `:866` |
| Remove | `.child(workspace_notes)` insertion | `:934` |
| Remove | trailing notes hit band only | `:906-910` |
| Preserve | `WINDOW_MOVE_THRESHOLD` | `:38` |
| Preserve | `move_arm` field/init and press/up lifecycle | `:130`, `:155`, `:892`, `:897`, `:915` |
| Preserve | `advance_move_arm`, including early return in root listener | `:286-306`, `:900-902` |
| Preserve | tab-drag `update_drag` call | `:903-905` |

The button is exactly `34px` wide at `:697`. Removing the plain flex child
moves equalize next to gear by 34px; gear and window controls remain
right-anchored. No hard-coded notes hit band remains afterward.

## IPC bridge

All entries are in `crates/scribe-client/src/ipc_bridge.rs`.

| Verdict | Construct | Current location |
|---|---|---:|
| Trim | `WorkspaceNotesMutation` import | `:56` |
| Remove | `IpcSink::workspace_notes_get` and doc block | `:1242-1250` |
| Reword | `create_workspace` prose: remove “and the notes collection” | `:1256` |
| Remove | `IpcSink::workspace_notes_mutate` and doc block | `:1333-1345` |

## Client shell bulk removal

All entries are in `crates/scribe-client/src/main.rs` and are notes-only.

| Verdict | Construct | Current location |
|---|---|---:|
| Remove | three notes-module import blocks | `:115-122` |
| Trim | `ArchiveReason` from protocol imports | `:141` |
| Remove | `Shared::notes` field and prose | `:386-390` |
| Remove | `WorkspaceNotesPreviewSurface` | `:705-712` |
| Remove | `TerminalView::workspace_notes_modal` | `:875-876` |
| Remove | `TerminalView::workspace_notes_preview` | `:877-878` |
| Remove | `TerminalView::workspace_notes_adopted` | `:879-882` |
| Remove | three `TerminalView` initializers | `:1094-1096` |
| Remove | two workspace-notes `TitlebarEvent` arms | `:1316-1319` |
| Remove | `TerminalView::notes_workspace_id` | `:5003-5011` |
| Remove | `TerminalView::open_workspace_notes_modal` | `:5020-5059` |
| Remove | `TerminalView::set_workspace_notes_preview` | `:5068-5119` |
| Remove | `TerminalView::sync_workspace_notes` | `:5128-5130` |
| Remove | `TerminalView::sync_workspace_notes_modal` | `:5133-5149` |
| Remove | `TerminalView::sync_workspace_notes_preview` | `:5152-5163` |
| Remove | `TerminalView::route_workspace_notes_action` | `:5166-5230` |
| Remove | `TerminalView::send_workspace_notes_mutation` | `:5233-5241` |
| Remove | `TerminalView::handle_notes_modal_key` | `:5244-5256` |
| Remove | `TerminalView::build_workspace_notes_preview_overlay` | `:6316-6321` |
| Remove | `Shared` notes constructor | `:6735` |
| Remove | `ReaderCtx` notes clone | `:7370` |
| Remove | `ReaderCtx::notes` field and prose | `:7846-7848` |
| Remove | `WORKSPACE_NOTES_ERROR_PREFIX` | `:8589` |
| Remove | `on_workspace_notes_message` and prose | `:8593-8626` |

`AddingNoteState` is defined in the outright-deleted `workspace_notes.rs:39`.
`set_inline_editor`, `OpenEditor`, and `FocusEditor` are in the outright-deleted
preview module (`workspace_notes_preview.rs:155`, `:40`, `:45`). The current
shell maps `OpenEditor` to modal open at `main.rs:5097` and maps `FocusEditor`
to an empty arm at `:5107`, confirming the intended inline-editor half is
unwired even though its entity types compile.

## Shared and ordering-sensitive client edits

Edit each surviving construct in place; do not delete its enclosing block.

| Verdict | Construct | Exact edit and current location |
|---|---|---|
| Trim | `open_overlay_chord` | Delete only `OverlayChord::WorkspaceNotes` at `main.rs:5317`; four arms survive. |
| Trim | `overlay_free` | Delete only `&& self.workspace_notes_modal.is_none()` at `:5394`. |
| Trim | keyboard routing chain | Delete only modal handling at `:5428-5432`; dialog remains before find-overlay, and find-overlay remains after it. |
| Trim | `Render::render` sync pass | Delete only `sync_workspace_notes(cx)` at `:6388`; `sync_find_results` then directly precedes `sync_remote_connect`. |
| Remove local | preview build | Delete `notes_preview` build at `:6410`. |
| Trim | render children | Delete modal and preview children at `:6480-6481`; `displaced` remains the last child at `:6488`. |
| Trim independently | `server_message_variant` | Delete the two independent arms at `:8170-8171`; preserve the function and every surviving variant arm. |
| Trim grouped | `dispatch_server_message` reader table | Delete the comments and grouped notes or-pattern at `:8401-8407`; preserve the surrounding routing table. |
| Trim | `on_server_error` | Delete only the leading notes-prefix block at `:8633-8640`; preserve `set_status` at `:8641`. |
| Preserve | `PaneShell::is_server_workspace` | Definition `pane_shell.rs:300`; surviving caller `main.rs:4077` remains after notes caller `:5005` is deleted. |

The reachability hard prerequisites remain: `server_message_variant` at
`main.rs:8147`, `dispatch_server_message` at `:8331`, and
`handle_layout_action` all retain balanced bodies and their exact names.

## Test daemon, Just recipe, and visual script comments

- In `crates/scribe-test/src/daemon.rs`, the
  `dispatch_notice_message` grouped or-pattern spans `:384-397`. Delete only
  `WorkspaceNotesSnapshot` at `:394` and `WorkspaceNotesChanged` at `:395`.
  The arm continues to include, among its other alternatives, `Error`,
  `SessionList`, `SearchResults`, `PromptMark`, and `PromptReceived`, then calls
  `dispatch_notice_message(msg)` at `:398`.
- In `justfile`, delete comment block `:276-281`, recipe
  `e2e-visual-workspace-notes` at `:282`, and its command at `:283`.
- In `tests/e2e/visual/tab-window-chords.sh`, preserve the executable body
  byte-for-byte and reword exactly two comments. Current text is:

  - `:7`: `# opened the workspace-notes modal. Both keystrokes were claimed by`
  - `:214`: `# The workspace-notes modal used to own this chord, and \`NewWindow\` had no`

  The planned replacements are `opened the since-removed notes modal.` and
  `A since-removed modal used to own this chord`, retaining the remainder of
  each sentence. This rebased finding supersedes the stale `spec.md` statement
  that the file should be left alone.

## `lat.md` edits

Locate by heading or literal prose, never by the coordinates alone.

### `lat.md/client.md`

| Verdict | Construct | Current location |
|---|---|---:|
| Delete section | `## GPUI Workspace Notes` through before `## App State` | `:1083-1134` |
| Delete section | `## Workspace Notes`, including `### Inline Note Editor`, through before `## Input` | `:1280-1327` |
| Reword | titlebar interactive-control description and event prose | paragraph beginning `:717` |
| Reword | `### Window move region` closing control list | `:743-745` |
| Reword | keyboard tab order, removing workspace notes | `:749-752` |
| Reword | overlay-chord table, history, and surviving chords | `:967-972` |
| Reword | input keyboard-chain prose | notes entries in `:1316`, `:1375` |
| Delete stale prose | workspace-notes preview hit-test rects | `:1438` |
| Reword | IME immutable-surface gate list | `:1532` |

The two section deletions remove all four lat-only identifiers named in the
survey verdict. They also remove prose containing `workspace_notes_save_pending`
and `PreviewLayout`, while their non-lat source occurrences disappear with the
outright-deleted modules.

### Other `lat.md` files

| Verdict | Construct | Current location |
|---|---|---:|
| Delete | `lat.md/server.md` `### Workspace Notes` | `:208-217` |
| Delete/rewrite | state-transfer sentence beginning “Workspace notes are not embedded” | `lat.md/server.md:242` |
| Delete | first `lat.md/protocol.md` `### Workspace Notes` | `:59-66` |
| Delete | second `lat.md/protocol.md` `### Workspace Notes` | `:167-174` |
| Reword | remote-transport phrase “and notes messages” | `lat.md/protocol.md:211` |
| Delete | `lat.md/test.md` `### Workspace notes on the wire` | `:337-348` |
| Reword | `scribe-common` crate-map summary, removing singular workspace-note wire types | `lat.md/architecture.md:16` |
| Reword | stale unwired-preview census claim | `lat.md/architecture.md:148` |

After the deletions, add the protocol compatibility decision record in
`lat.md/protocol.md`: delete four variants and six types without deprecation or
no-op arms; retain version `3`; record that local IPC has no version
negotiation, a bump offers no local protection, and a bump would arm the silent
LAN-peer rejection forbidden by FR-014 despite three prior remote-visible
semantic changes without a bump. Its leading paragraph must remain at most 250
characters and contain no link to a deleted section.

All 16 notes `// @lat:` anchors are inside the five outright-deleted client
files. Delete those files before deleting their lat sections; the reverse order
temporarily leaves dangling code references.

## Count-gate edits

### Reachability baseline

`tools/reachability-baseline.txt:27-32` currently contains:

- modules `67/67`;
- server messages `54/59`;
- layout actions `36/36`.

After the three client modules and two handled server messages are removed,
write modules `64/64`, server messages `52/57`, and keep layout actions
`36/36`. Preserve the five unhandled-server-message rows at `:34-38`.

### Parity inventory

In `specs/016-gpui-client-rebuild/parity-inventory.md`:

- delete `WorkspaceNotesGet` and `WorkspaceNotesMutate` at `:116-117`;
- delete `WorkspaceNotesSnapshot` and `WorkspaceNotesChanged` at `:198-199`;
- delete `Workspace notes hover preview` at `:408`;
- update section headings `Client messages (47 sent)` at `:94` to 45,
  `Server messages (59 handled)` at `:154` to 57, and
  `Spec behaviour requirements (29)` at `:367` to 28;
- update the matching three table footers;
- update roll-up rows at `:457-463`: client messages 45, server messages 57,
  spec requirements 28, and total 199;
- update prose at `:466-469` from 195/195, “1 of those 195”, and 194/195 to
  190/190, “1 of those 190”, and 189/190;
- leave Input/keybinding 54 and Rendering/window 6 unchanged.

Expected post-edit checker output is 199 reachable rows with 190 user-facing,
189 reachable in-client, and 48 carried requirements.

### `US4-3`

In `specs/016-gpui-client-rebuild/spec.md:254-255`, amend rather than delete
`US4-3`. Use parent epic `scribe-1am` in the inline descope annotation, retain
accent colors, badges, and workspace splits, then add the dated descope
decision at the end of `## Requirement register`. In the parity coverage cell
at `parity-inventory.md:519`, remove `Workspace notes hover preview` and
`WorkspaceNotesSnapshot`; retain `Workspace accent colours and badges` and
`workspace_split_vertical`. The carried-requirements count stays 48.

### Non-gated historical follow-through

These do not block the completion grep but are explicitly located for the
separate scrub item:

- recompute nearby count prose in `parity-inventory.md:466-469`;
- correct old parity figures in `specs/016-gpui-client-rebuild/plan.md`, notably
  `:502-503`, `:512`, `:518`, and `:531`;
- change `launch-gate-checklist.md:83` from 41 visual scripts to 40 and remove
  `workspace-notes` from the E2E list at `:124`;
- scrub workspace-notes rows/prose in
  `specs/016-gpui-client-rebuild/reachability-audit.md` (client rows
  `:120-121`, server rows `:192-193`, behavior row `:385`, and FU-21/history
  occurrences);
- reword the stale titlebar accessibility row in
  `specs/016-gpui-client-rebuild/accessibility-audit.md:29`;
- reword the persistence-pattern list in
  `specs/006-persist-terminal-env/research.md:119`.

## Preserve list and false-positive guard

These widened-`note` matches are unrelated and must remain.

| File | Preserved construct or text | Current location |
|---|---|---:|
| `crates/scribe-client/src/settings/window.rs` | `tailnet_note`, `trust_status_notes`, `note_row`, `settings-note`, `Role::Note`, `NOTE_MAX_CHARS` | `:1755`, `:1771`, `:1899-1902`, `:1914`, `:2512` |
| `crates/scribe-client/src/ai_indicator.rs` | `note_activity` | `:158` (live caller `main.rs:8746`) |
| `crates/scribe-client/src/x11_focus.rs` | `note_inactive`, `note_active` | `:88`, `:94` |
| `crates/scribe-server/src/ipc_server.rs` | `ResizePacer::note_external_apply`, `note_unpaced_resize_apply` | `:1207`, `:7270` |
| `crates/scribe-server/src/attach_flow.rs` | `note_unpaced_resize_apply` | `:17`, `:272` |
| `crates/scribe-server/src/releases.rs` | `Options::ENABLE_FOOTNOTES` | `:135` |
| `crates/scribe-client/src/remote/tests.rs` | `awaiting_approval_swaps_loading_note_until_settled` | `:191` |
| `crates/scribe-client/src/remote.rs` | “loading note” prose | `:374`, `:821` |
| `.github/workflows/release.yml` | release-notes / `RELEASE_NOTES` / `notes-file` | `:161-182` |
| `tools/perf-ab-rig/run-perf-ab.sh` | `STARTUP_NOTE` | `:1021`, `:1068`, `:1298` |
| `dist/shell-integration/fish/vendor_conf.d/scribe.fish` | ordinary “Note:” prose | `:68`, `:74` |
| `dist/debian/postinst` | ordinary multi-window note prose | `:624` |
| `AGENTS.md` | lat.md sync note prose | `:27` |
| `lat.md/settings.md` | release-notes prose | `:162`, `:206-210` |
| `lat.md/server.md` | resize-pacer note links | `:125` |

The old survey named two paths incorrectly: `ENABLE_FOOTNOTES` is in server
`releases.rs`, not client `releases.rs`, and the fish file is under
`vendor_conf.d`, not `vendor_completions.d`. The symbols still belong to the
allowlist. Also preserve the server `toml` dependency at
`crates/scribe-server/Cargo.toml:262`; live users include
`lan/network.rs`, `lan/trust.rs`, and `env_store/gc.rs`.

Only `settings/window.rs::note_row` is allowlisted. Same-named note-row/count
helpers inside the outright-deleted notes modules do not survive.

## Compatibility and persisted-data coordinates

These facts constrain sequencing; they are not extra edit targets.

- Named msgpack and length-prefix framing are in
  `crates/scribe-common/src/framing.rs:20-35` and `:45-56`. A frame decode error
  reaches `LoopExit::Disconnected` at `ipc_server.rs:5579-5582`; framing itself
  remains synchronized.
- The local `Hello` write followed by `ListSessions` is at
  `main.rs:7310-7324`; there is no local version field.
- `supervise_connection` at `main.rs:6976` retries local sockets from 100ms to
  2s (`:6977-6982`, `:7039-7040`) when LAN and tailnet dial targets are absent.
  It contains no `cx.quit()` path. The first new `SessionList` calls
  `reattach_visible_sessions` at `:8489`, whose definition is `:8502`.
- `justfile` restart recipes are `restart-server` at `:71` and
  `restart-server-release` at `:75`; neither restarts clients. Debian upgrade
  handoff uses `--upgrade` in `dist/debian/postinst:500-504`.
- The only persisted notes filename is joined in
  `workspace_notes.rs:74`. `AppIdentity::state_dir` is
  `crates/scribe-common/src/app.rs:151`, with stable/dev slugs `scribe` and
  `scribe-dev` at `:57-61`. `private_temp_path` is
  `workspace_notes.rs:424-427`.
- Env GC reads only its env tree and `restore/windows` at
  `env_store/gc.rs:76-86`; Debian `postrm` purges only `/etc/scribe*` at
  `dist/debian/postrm:1-17`. Neither removes notes data.

Operational order remains strict: land removal; complete the isolated stale-file
startup check; land the installer migration; then let each user-controlled
package configure replace its exact-flavor server and delete only that flavor's
legacy file/temp siblings without backup. Deferred or indeterminate lifecycle
state remains pending for a later configure.

## Completion and quality gates

The implementation join point must run:

- both completion-gate searches from `spec.md`, with `specs/**` and `.beads/**`
  excluded but `lat.md/**` included; both return zero lines;
- `tools/check-reachability.sh --working-tree` and
  `tools/check-parity-inventory.sh --working-tree`, plus parity `--gate`;
- `just ready`;
- `lat check`;
- the actual configured commit-hook runner, including staged reachability,
  parity, no-new-lint-suppressions, and formatting gates.

No test code is added. Existing notes tests and the notes visual oracle are
deleted with their retired surface. No new `allow` or `expect` attributes are
introduced. The removal has no performance budget because it adds no hot path,
allocation, render work, or I/O.

Code and both machine-checked gate documents must land atomically. Reverting
that one commit must restore source and baseline consistency together. Runtime
note-data deletion is deliberately irreversible and is not part of rollback.
