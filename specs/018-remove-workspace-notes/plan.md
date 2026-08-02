# Plan: 018 — Remove Workspace Notes

Implementation plan for the complete removal of workspace notes — client
UI, server store, protocol messages, tests, docs, CI gate documents, and
the persisted user data. Built from `spec.md` at status CLARIFIED, whose
`## Clarifications` answers (Q1-Q7, Q-A, Q-B) are binding inputs and win
over any older prose in the spec body. Baseline is this worktree, rebased
onto main `cfcc84d`, clean except for the untracked `specs/018-remove-
workspace-notes/`.

## Architecture Approach

**One atomic removal. No phases, no deprecation window, no shims.** Every
protocol variant, module, chord, doc section, and gate number moves in a
single commit; the only step that lands after it is the data deletion,
which is an operator action rather than a code change. Nothing is built to
replace notes — no annotation, scratchpad, or per-workspace metadata
surface takes their place, so the plan is subtraction end to end.

Two independent forces make this the only workable shape.

**The failure mode a phased removal would protect against is a blip, not
data loss.** An old client that emits `WorkspaceNotesGet` at a new server
hits a hard serde decode error (both enums are internally tagged, so an
unknown tag is an error and not a skip), and the server maps that to
`LoopExit::Disconnected`. What follows is bounded and self-healing:
`finish_served_connection` releases window ownership while leaving the
owning sessions untouched, so PTYs and scrollback survive server-side;
the client's `supervise_connection` retries the local socket **forever**
at 100 ms -> 2 s backoff (`retry_local` is true whenever `SCRIBE_LAN_DIAL`
and `SCRIBE_REMOTE_DIAL` are unset, which holds for the running clients);
reconnect writes `Hello` + `ListSessions`; the first `SessionList` drives
`reattach_visible_sessions` at each pane's retained grid dimensions; and
the server answers `SessionReplay` per session so scrollback is rebuilt.
Typed input is buffered in the 1024-frame outbound queue and replayed.
`cx.quit()` is unreachable from a connection failure. The user-visible
worst case is a red status dot plus one status line — a sub-2-second
blip. Nothing that a deprecation window would buy is worth its cost.

**Precedent is unanimous.** Four prior protocol-variant removals in this
repo — `528a932` (Driver variants), `2bd35bb` (`ScrollRequest` /
`ScrolledSnapshot`), `a716efb` (AutomationAction variants), `cec34e0`
(`PreflightError` reshape) — all deleted outright with no deprecation
window, and `git log -S 'deprecated' -- protocol.rs` returns no commits
at all. There is no house pattern to break.

### Rejected alternatives

- **(a) Staged deprecation with no-op `ClientMessage` arms.** Rejected as
  *actively worse* than deletion, not merely unnecessary. Retaining
  `WorkspaceNotesGet` / `WorkspaceNotesMutate` as no-ops while deleting
  the two `ServerMessage` variants leaves an old client's modal
  permanently empty with no error — indistinguishable from "you have no
  notes", which a user reads as silent data loss a release early. It also
  directly contradicts the user's "no legacy code, no dead code, no
  compatibility fallbacks" requirement, and it would demand a follow-up
  removal task that the spec's Non-Goals forbid.
- **(b) Bumping `REMOTE_PROTOCOL_VERSION` 3 -> 4.** Rejected on all three
  of Q2's grounds. It **protects nothing**: the exposure that motivates it
  is the local Unix socket, which has no version negotiation at all —
  `Hello` carries only `window_id`, `clipboard_gating`, and `takeover`.
  It **causes harm**: bumping activates a currently-dormant bare
  `continue` in `crates/scribe-client/src/remote.rs` that filters
  mismatched LAN peers out of the connect picker with no row, no error,
  and no explanation, which `specs/015-multi-machine-sharing/spec.md`
  FR-014 explicitly forbids. And **precedent contradicts the constant's
  own doc comment**: it has changed exactly once in mainline history
  (`fd04540`, landing directly at `3`), while `fbab056`, `ba626d4`, and
  `cec34e0` each changed remote-visible semantics without bumping. The
  constant stays `3`; a diff touching it fails Story 6.
- **(c) A one-shot server-startup unlink of `workspace_notes.toml`.**
  Rejected as self-contradicting: it is notes-aware code shipped by a
  release whose entire purpose is that no notes-aware code remains. It
  would also have to be removed later, re-creating exactly the follow-up
  task the Non-Goals rule out. Deletion is an explicit, documented,
  ordered manual step instead (see [Data Model](#data-model)).

### The atomicity constraint is mechanical, not stylistic

`tools/check-reachability.sh` and `tools/check-parity-inventory.sh` are
run by `just ready`, by CI (`.github/workflows/quality.yml`), **and as
pre-commit hooks in `--staged` mode** (`.pre-commit-config.yaml`, hook ids
`reachability-baseline` and `parity-inventory`). Both derive their numbers
from the source tree and compare against hand-maintained figures in
`tools/reachability-baseline.txt` and
`specs/016-gpui-client-rebuild/parity-inventory.md`.

Consequence: **the code deletions and the `specs/016-*` document edits
must be staged in the same commit.** A commit carrying only the code fails
because the baselines still claim 67 modules and 59 server messages; a
commit carrying only the documents fails because the counts no longer
match the source. This forcibly rules out any staged or phased landing and
means the [Sequencing](#sequencing) section describes *work order inside a
single commit*, not a series of commits.

Corollary for bead acceptance criteria: **mid-flight items cannot use
"`cargo check` passes" as an acceptance criterion**, because the tree does
not compile between the protocol deletion and the last consumer edit.
Per-item criteria are diff-shaped — a named symbol is absent, a named
construct is preserved verbatim, a named count is updated. Compilation is
the acceptance criterion of the quality-gate item alone.

### Constitution check

- **P1 (Clear Boundaries and Typed Failure) — PASS.** Nothing crosses a
  crate boundary that did not already; the change only removes. No new
  dependency, no new abstraction, no new cross-cutting helper. Note
  failures already ride the generic `ServerMessage::Error` (there is no
  `ServerError` enum), so no error taxonomy is disturbed. The retained
  `toml` dependency in `scribe-server` stays because `lan/network.rs`,
  `lan/trust.rs`, and `env_store/gc.rs` use it.
- **P2 (Session-Safe, Consistent UX) — PASS with a documented tension.** A
  visible titlebar control disappears and a shortcut becomes unbound. This
  is the intended product change, and it is bounded: the notes button is a
  plain flex child so its siblings simply reflow, and `ctrl+shift+m` joins
  the eleven `ctrl+shift+<letter>` combos (`a e g h i j l o r s y`) that
  already fall through to the PTY today. Long-lived server-owned sessions
  are untouched — the store is a sidecar, not session state. Carried by
  Stories 1 and 2; verification is in [Testing Strategy](#testing-strategy).
- **P3 (Explicit, Risk-Based Verification) — PASS.** Every story gets an
  independent, user-reachable path; **no new test code is written**, which
  the principle permits because none was requested and the only coverage
  that changes is coverage being deleted alongside its feature. See
  [Testing Strategy](#testing-strategy) for the per-story map and for the
  honest statement of the Story 1 oracle gap.
- **P4 (Performance Budgets and Measurement) — INAPPLICABLE, explicitly
  marked.** The constitution requires the mark rather than silence. This
  is a pure removal: no new hot path, no new allocation, no new render
  work, no new IO. There is no budget to state and none to regress. The
  only measurable direction is downward — one fewer titlebar child, one
  fewer sync pass in `Render::render`, one fewer reader routing arm — and
  none of it is worth instrumenting.
- **P5 (Default-Safe Trust Boundaries) — PASS, trivially.** No capability
  is added. One data-exfiltration-adjacent surface is *removed*: the notes
  messages were routed with no `is_remote` gate and absent from
  `requires_window_control`, so a remote controller could read and mutate
  workspace notes. Deleting them shrinks the remote-reachable surface.
- **P6 (Local-First Data Locality) — PASS.** Nothing gains network
  behavior; a locally-persisted TOML file and its protocol are removed.
- **P7 (Compatible, Documented, Operationally Safe Change) — PASS with a
  resolved tension.** The compatibility decision (four variants deleted
  outright, `REMOTE_PROTOCOL_VERSION` deliberately unchanged, and the
  reasoning for both halves) is recorded in `lat.md/protocol.md` and in
  the spec's `## Clarifications` as the durable decision record. `lat.md`
  is kept synchronized and `lat check` gates it. The standing rule against
  restarting the live server is honored: the user has granted **exactly
  one** approval, for step 3 of the data-deletion sequence, and no other
  work item may restart the server. Story 6 is verified by reading and
  documentation, never by exercising a live upgrade. The residual
  mixed-version risk is recorded rather than minimized in
  [Risks](#risks).

## Affected Components

> **Symbol names govern. Every line number carried over from the spec is a
> stale pre-rebase snapshot, not a criterion.** The original survey was
> taken against the then-dirty primary checkout before this worktree was
> rebased onto `cfcc84d`; `e530da7` alone shifted everything below
> `lat.md/client.md:719` by +27 and moved `titlebar.rs` by tens of lines.
> Locate every construct by symbol, heading, or literal text, and
> re-verify immediately before touching it. No acceptance criterion in
> this plan or the spec depends on a line number.

### crates/scribe-client

**Deleted outright (5 files):**

- `src/workspace_notes.rs` — client-side store
- `src/workspace_notes_modal.rs` and `src/workspace_notes_modal/tests.rs`
- `src/workspace_notes_preview.rs` and
  `src/workspace_notes_preview/tests.rs`

All 16 `// @lat:` note anchors in the repo live in these files and vanish
with them (3 in `workspace_notes.rs`, 5 in `workspace_notes_preview/tests.rs`,
8 in `workspace_notes_modal/tests.rs`); zero anchor edits are needed in
`scribe-server` or `scribe-common`. Every one of them points into a
`client#GPUI Workspace Notes` subsection, which is why the deletion of
these files must precede the deletion of that `lat.md` section — see
[Sequencing](#sequencing). The already-dead subset (`DraftDebounce`,
`DraftDebounceEvent`, `WORKSPACE_NOTES_DEBOUNCE`, `AddingNoteState`,
`set_inline_editor`, and the `OpenEditor`/`FocusEditor` inline-editor
half) is absorbed here — removing it changes no user-visible behavior
because `main.rs` maps `OpenEditor` to opening the modal and `FocusEditor`
to `{}`.

**The already-dead subset's third bullet, carried with a correction.** The
spec's third bullet lists six identifiers it claims survive only as
`lat.md` prose. Four of them check out: `adding_note_states`,
`focused_inline_editor`, `affordance_hovered_workspace`, and
`draw_affordance` appear nowhere in `crates/` and exist only in
`lat.md/client.md` (plus historical `specs/` records, which are out of
scope). Those four are docs edits and route to the `lat.md` work item.
**Plan-time correction, in the same style as the `server_message_variant`
correction below: the spec is wrong about the other two.**
`workspace_notes_save_pending` is live source in
`crates/scribe-client/src/workspace_notes_modal.rs`, and `PreviewLayout` is
live source in `crates/scribe-client/src/workspace_notes_preview.rs`. Both
sit inside files this plan deletes outright, so **nothing changes
operationally** — the correction matters only so that no work item goes
hunting for a `lat.md`-only identifier that is actually code, and so the
`lat.md` item does not claim to remove something it never owned. The
re-verify item confirms all six.

**Surgical edits:**

- `src/lib.rs` — three `pub mod` declarations. This is what moves
  `modules-total` and `modules-wired` 67 -> 64. No other module becomes
  newly unwired: the notes modules reference only `crate::tab_bar` and
  each other, and `tab_bar` stays wired.
- `src/main.rs` — the bulk of the work. Imports and the `ArchiveReason`
  entry in the `use` list; `Shared::notes` and the `Shared` constructor;
  `WorkspaceNotesPreviewSurface`; `TerminalView` note fields and their
  init; the `TitlebarEvent` arms; `notes_workspace_id`;
  `open_workspace_notes_modal`; `set_workspace_notes_preview`;
  `sync_workspace_notes`, `sync_workspace_notes_modal`,
  `sync_workspace_notes_preview`; `route_workspace_notes_action`;
  `send_workspace_notes_mutation`; `handle_notes_modal_key`;
  `build_workspace_notes_preview_overlay`; `ReaderCtx` field and its
  clone; `WORKSPACE_NOTES_ERROR_PREFIX`; `on_workspace_notes_message`.
  Plus the shared hot spots below, which are *edited down*, never
  bulk-deleted: the `open_overlay_chord` arm; the `overlay_free`
  conjunct; the keyboard routing chain entry (which sits **between** the
  dialog and find-overlay handlers — preserve the survivors' relative
  order); the `Render::render` sync pass (between `sync_find_results` and
  `sync_remote_connect`); the `notes_preview` build; the render child
  order (the `displaced` banner **must remain the last child**); the
  `server_message_variant` log table; the reader routing table; and
  `on_server_error`, where **only** the leading
  `WORKSPACE_NOTES_ERROR_PREFIX` block goes because the trailing
  `set_status` is shared.
- `src/titlebar.rs` — `TitlebarEvent::WorkspaceNotesHover` and
  `TitlebarEvent::OpenWorkspaceNotes`; `notes_focus_handle` and its init;
  `render_workspace_notes_button` (its `e530da7` `.on_mouse_down`
  stop-propagation guard is absorbed with the function, needing no
  separate edit), its call site, and its child insertion;
  `has_keyboard_focus` drops **exactly one** clause; and the root
  `on_mouse_move` hit band — see the carry-forward hazard below.
  Layout consequence: removing the 34px button shifts equalize right by
  34px; gear and window controls are right-anchored and unaffected.
- `src/keybindings.rs` — `OverlayChord::WorkspaceNotes`, the
  `("ctrl+shift+m", …)` entry, and the `OVERLAY_CHORDS` array type
  literal `[(&str, OverlayChord); 5]` -> `; 4]`.
- `src/ipc_bridge.rs` — the protocol import, `workspace_notes_get`,
  `workspace_notes_mutate`, and the neighbouring doc prose.

### crates/scribe-server

- `src/workspace_notes.rs` **deleted** — `PersistedWorkspaceNotes`,
  `WorkspaceNotesStore`, and the atomic private-TOML writer
  (`write_toml_atomic` / `persist_next` / `private_temp_path` /
  `ensure_private_parent`).
- `src/lib.rs` and `src/main.rs` — module declaration, the store's
  construction, and its wiring into server state.
- `src/ipc_server.rs` — the import and `use`, the state field, the
  dispatch arms, the workspace-dispatch arms, `handle_workspace_notes_get`,
  `handle_workspace_notes_mutate`, `broadcast_workspace_notes_changed`,
  and the neighbouring doc prose.

### crates/scribe-common

`src/protocol.rs` loses 6 types and 4 message variants: `WorkspaceNoteStatus`,
`ArchiveReason` (notes-only despite the generic name — confirmed by two
review passes across all 15 references, and still worth an explicit
re-verify before deleting a generically-named public type),
`WorkspaceNoteEntry`, `WorkspaceNoteDraft`, `WorkspaceNotesCollection`,
`WorkspaceNotesMutation`; and `ClientMessage::WorkspaceNotesGet`,
`ClientMessage::WorkspaceNotesMutate`,
`ServerMessage::WorkspaceNotesSnapshot`,
`ServerMessage::WorkspaceNotesChanged`. `REMOTE_PROTOCOL_VERSION` is not
touched. There is no `ServerError` enum, so no error-variant surgery.

**Hard prerequisites the reachability gate `die`s on** — the edits must
preserve all of these or the gate aborts instead of reporting a count:
`fn server_message_variant` must still exist and name every surviving
`ServerMessage` variant; `fn dispatch_server_message` and
`fn handle_layout_action` must still exist with balanced bodies; and
`pub enum ClientMessage`, `pub enum ServerMessage`, `pub enum
LayoutAction`, and `pub struct Bindings` must keep their exact syntax and
4-space variant indentation.

### crates/scribe-test

`src/daemon.rs` — drop **two `|` lines** from the `dispatch_notice_message`
or-pattern. See the compiler-invisible hazards below.

### Build and test infrastructure

- `justfile` — the `e2e-visual-workspace-notes` recipe plus its leading
  comment block. Locate by recipe name; the spec draft's cited line range
  was wrong.
- `tests/e2e/visual/workspace-notes.sh` **deleted** — the only automated
  oracle for the notes UI.
- `tests/e2e/visual/tab-window-chords.sh` **survives as a test, but two of
  its comment lines must be reworded.** Its comments contain no
  `ctrl+shift+m` reference at all — the text is historical rationale about
  `ctrl+shift+N` (`new_window`) having once collided with the notes modal,
  and scrubbing that rationale would delete the test's justification. But
  the file *does* carry the literal hyphenated token `workspace-notes`
  (currently lines 7 and 214), and GATE A's leading alternative
  `(?i)workspace[-_ ]?notes` matches it. `COMMON` excludes only `specs/**`
  and `.beads/**`, so this file is squarely **inside** the gate: it is one
  of the 24 current GATE A hit files.

  **Plan-time correction.** An earlier draft of this plan concluded the
  file was "left alone" from the true premise that it names no
  `ctrl+shift+m`. That conclusion was wrong, and it left the only hit file
  in the repo with no owning work item — which would have made the
  quality-gate item's "GATE A returns zero lines" unachievable. The fix is
  a minimal reword that keeps the rationale and loses the banned token:

  - the phrase `opened the workspace-notes modal.` becomes
    `opened the since-removed notes modal.`
  - the phrase `The workspace-notes modal used to own this chord` becomes
    `A since-removed modal used to own this chord`

  Both are located by their surrounding text, never by line number. Nothing
  executable in the script changes. Owned by the `justfile`/E2E work item
  in [Sequencing](#sequencing).

### CI gate documents (mandatory, in scope)

- `tools/reachability-baseline.txt` — `modules-total` 67 -> 64,
  `modules-wired` 67 -> 64, `server-messages-total` 59 -> 57,
  `server-messages-handled` 54 -> 52; `layout-actions-*` unchanged at
  36/36. The five `unhandled-server-message` lines are unchanged — none
  names a notes variant, because both deleted `ServerMessage` variants are
  currently *handled*, so handled drops by exactly 2.
- `specs/016-gpui-client-rebuild/parity-inventory.md` — five rows deleted
  and eight interlocking hand-maintained figures recomputed. Detailed in
  [API / Interface Changes](#api--interface-changes) and owned by its own
  work item.
- `specs/016-gpui-client-rebuild/spec.md` — register id `US4-3` is
  **amended, not deleted** (accent colours, badges, and workspace splits
  survive it), plus a dated decision paragraph at the tail of
  `## Requirement register`.

### lat.md (5 files)

`lat.md/` is deliberately **inside** the completion gate and needs
line-by-line treatment, not a bulk section delete: `client.md`,
`server.md`, `protocol.md`, `test.md`, `architecture.md`. Targets are
enumerated under the `lat.md` work item in [Sequencing](#sequencing).

### Compiler-invisible hazards

Everything else in this removal is caught by `rustc` or clippy. These are
not.

1. **`crates/scribe-test/src/daemon.rs` — grouped or-pattern whose
   enclosing arm survives.** The `WorkspaceNotesSnapshot` and
   `WorkspaceNotesChanged` lines sit in the middle of a `|`-chain that
   also carries `Error`, `SessionList`, `SearchResults`, `PromptMark`, and
   `PromptReceived`, all routed to `dispatch_notice_message`. Deleting the
   **arm** instead of the two `|` lines silently reroutes those unrelated
   variants to whatever follows, with no compiler error. Delete exactly
   two lines.
2. **`crates/scribe-client/src/main.rs` — the reader routing table's
   bound or-pattern.** The notes arm is `notes @ (ServerMessage::
   WorkspaceNotesSnapshot { .. } | ServerMessage::WorkspaceNotesChanged
   { .. }) => on_workspace_notes_message(ctx, notes)`. Here the whole arm
   *is* notes and is deleted wholesale — but the shape is one
   `match`-arm-edit away from the daemon hazard, so treat it with the same
   care. **Plan-time correction:** the spec's non-blocking observation
   claims `server_message_variant` is a second grouped or-pattern. In the
   rebased tree it is **two independent single-variant arms**
   (`ServerMessage::WorkspaceNotesSnapshot { .. } => "WorkspaceNotesSnapshot"`
   and likewise for `Changed`), each safe to delete on its own. The
   re-verify work item must confirm this before the main.rs surgery
   proceeds; if it has changed, treat it as hazard 1.
3. **The `e530da7` carry-forward in `titlebar.rs`.** The root
   `on_mouse_move` closure now **interleaves** `advance_move_arm`'s
   early-return and the `update_drag` call with the `WorkspaceNotesHover`
   hit band, which sits at the bottom. Delete **only** the trailing band
   lines — the `let width`, the `let x`, and the
   `if (width - 188.0..width - 154.0)` emit. Deleting the closure, its
   head, or the `advance_move_arm` guard breaks window dragging on
   X11/Wayland with **no compiler error**, because
   `WindowControlArea::Drag` is a no-op in the pinned GPUI revision and
   this imperative move system is the only thing making the titlebar
   draggable. After the deletion `titlebar.rs` contains no hard-coded
   pixel hit band at all; that closure was the only one.

### Must NOT be touched

- `connected_window_writers` in `ipc_server.rs` — shared with
  `QuitRequested`, share rosters, and updater notices.
- `PaneShell::is_server_workspace` — another live caller in `main.rs`.
- The `toml` dependency in `scribe-server` — used by `lan/network.rs`,
  `lan/trust.rs`, `env_store/gc.rs`.
- `crates/scribe-client/src/settings/window.rs` in its entirety:
  `note_row`, `Role::Note` (a **gpui** type, not ours),
  `id(("settings-note", …))`, `NOTE_MAX_CHARS`, `tailnet_note`,
  `trust_status_notes`. Unrelated annotation-row helper.
- The rest of the GATE B false-positive allowlist: `note_activity`
  (`ai_indicator.rs`), `note_active`/`note_inactive` (`x11_focus.rs`),
  `note_external_apply`/`note_unpaced_resize_apply` (`ipc_server.rs`,
  `attach_flow.rs`), `Options::ENABLE_FOOTNOTES` (`releases.rs`),
  `awaiting_approval_swaps_loading_note_until_settled`
  (`remote/tests.rs`), the "loading note" prose in `remote.rs`, the
  release-notes step in `.github/workflows/release.yml`, `STARTUP_NOTE`
  in `tools/perf-ab-rig/run-perf-ab.sh`, prose in
  `dist/shell-integration/fish/.../scribe.fish` and
  `dist/debian/postinst`, the `AGENTS.md` mention, "release notes" in
  `lat.md/settings.md`, and the `note_unpaced_resize_apply` /
  `ResizePacer#note_external_apply` links in `lat.md/server.md`.
- `specs/004-workspace-notes/` and `specs/007-add-note-from-hover/` — kept
  as the record of why the feature existed and why it was cut. Only stale
  cross-references elsewhere are scrubbed. This is a **different act**
  from the mandatory `specs/016-*` gate edits, which are fully in scope.

**Trap:** `render_note_row` and `note_count` *also* exist in
`workspace_notes_modal.rs` and `workspace_notes_preview.rs` and are **not**
survivors. Only `settings/window.rs::note_row` is allowlisted. Match the
allowlist per file, never per identifier.

## Data Model

**No schema change and no migration.** There is no database, no embedded
notes field in any other state file, and no versioned on-disk structure to
evolve. The only persisted artifact is a single TOML file per app
identity, and the change deletes both the writer and the file.

The file is `current_state_dir().join("workspace_notes.toml")` via
`AppIdentity::state_dir()` — slug `scribe` or `scribe-dev`. The literal
`"workspace_notes.toml"` appears exactly once in the repo. The write path
is **mutation-only**: `write_toml_atomic` <- `persist_next` <-
`apply_mutation` <- `handle_workspace_notes_mutate`, reachable only from a
`WorkspaceNotesMutate` frame. `WorkspaceNotesStore::load()` never writes,
and a missing file yields a default with no write. There is no periodic
flush, no startup write, and no shutdown flush. `env_store/gc.rs` walks
only `restore/windows` and the env-envelope root, so it can neither read
nor delete it. Nothing in build, uninstall, or GC removes it —
`dist/debian/postrm` only clears `/etc/scribe*` on purge, and
`/etc/scribe*` does not exist on this machine.

In scope on this machine, for this user:
`~/.local/state/scribe/workspace_notes.toml` (2005 bytes, mode 0600, one
workspace, 0 active + 5 archived) and
`~/.local/state/scribe-dev/workspace_notes.toml` (1657 bytes, 0600, 3
active + 1 archived + 1 dirty draft). Content is scratch test text
throughout. No `.workspace_notes.toml.<pid>.<ms>.tmp` leftovers exist, and
a filesystem-wide search found no other copies. macOS and Windows are
dropped, not deferred: macOS ships as a `.dmg` with no maintainer scripts
of any kind, and no Windows target is built.

### Data destruction is an ordered operator step, not code

**Deleting source does not disarm installed binaries.**
`/usr/bin/scribe-client` and `/usr/bin/scribe-dev` are running now and
still contain the notes UI. One mutation on an old binary restores the
*entire* file, because the server holds the whole store in memory and
`persist_next` writes a full clone with `.truncate(true)` while
`ensure_private_parent` recreates the directory. A manual `rm` issued
before the restart is therefore silently reversible, and the AC's naive
proof ("the only writer is deleted") is a property of the source tree, not
of the live process.

The approved 4-step ordering, from Q5 and Q-A:

1. **Land** the client + server + protocol removal (the single atomic
   commit).
2. **Rebuild and reinstall.**
3. **Restart the servers and clients** so no live process retains notes
   code or the in-memory store. **The user has explicitly approved this
   restart, for this step only** — it is the one authorized exception to
   the standing rule in `CLAUDE.md`. The approval covers this step and
   nothing else.
3b. **Run the stale-file startup check, while the files still exist.** On
   the post-removal build, with
   `~/.local/state/scribe-dev/workspace_notes.toml` **still in place**,
   start a **dev** daemon — a separate, short-lived process, not the live
   server, so it consumes none of the step-3 approval — and confirm it
   neither reads the file, nor logs a warning naming it, nor recreates or
   rewrites it (mtime and size unchanged before and after). This check
   **MUST precede step 4** and **may NOT** be satisfied by retaining a copy
   of the file past step 4: the Non-Goals and Goal 7 forbid any backup,
   archive, or `.bak`, and a copy kept "just for the check" is exactly that
   backup. The check is only meaningful on a build that already has the
   notes code removed, which is why it sits after the rebuild and restart
   rather than before them.
4. **Delete the files.** After step 3, resurrection is impossible.

Steps 3 and 3b are load-bearing, not ceremonial. Skipping step 3 makes
step 4 a no-op that the next note mutation undoes; deleting first is a
defect, not an optimization. Running step 4 before step 3b destroys the
only artifact the stale-file check can observe, and Story 3's acceptance
criterion then has no evidence behind it.

Commands:

```bash
# Step 4 — deletion (only after steps 1-3).
rm -f ~/.local/state/scribe/workspace_notes.toml \
      ~/.local/state/scribe-dev/workspace_notes.toml
rm -f ~/.local/state/scribe/.workspace_notes.toml.*.tmp \
      ~/.local/state/scribe-dev/.workspace_notes.toml.*.tmp

# Verification — must return empty.
find ~/.local/state/scribe ~/.local/state/scribe-dev -name '*workspace_notes*'
```

The `.tmp` sweep currently matches nothing; it runs anyway to cover
crash leftovers. **Do not remove the state directories wholesale** — they
hold `restore/`, `windows/`, `settings_state.toml`, `driver_state.toml`,
and the LAN trust and certificate files, all in active use. No backup,
archive, or `.bak` is written at any step; the `find` check replaces the
unbounded "no copy exists anywhere" negative, which no command could
verify.

Scope is bounded to this machine and this user: `dpkg` shows `scribe` and
`scribe-dev` installed as systemd **user** units, `/etc/scribe*` does not
exist, and no other account holds Scribe state. Nothing is done for other
hosts or accounts.

A leftover file would be inert to the code — nothing else opens that
filename — but "inert" is not "gone." It is user-authored free text that
the product will no longer have any UI to view, export, or delete, so
removing the feature obligates removing the data.

## API / Interface Changes

### Breaking wire changes

Four message variants and six supporting types are deleted from
`crates/scribe-common/src/protocol.rs`:

| Kind | Removed |
|---|---|
| `ClientMessage` | `WorkspaceNotesGet`, `WorkspaceNotesMutate` |
| `ServerMessage` | `WorkspaceNotesSnapshot`, `WorkspaceNotesChanged` |
| Types | `WorkspaceNoteStatus`, `ArchiveReason`, `WorkspaceNoteEntry`, `WorkspaceNoteDraft`, `WorkspaceNotesCollection`, `WorkspaceNotesMutation` |

Both enums are serde internally tagged (`tag = "type"`) over named
msgpack, so an unknown tag is a **hard deserialize error, not a skip**.
Framing is length-prefixed (`crates/scribe-common/src/framing.rs`), so an
undecodable frame does not desync the stream — dropping the connection is
a policy choice, not a necessity, and changing that policy is out of scope
with its own follow-up bead.

**`REMOTE_PROTOCOL_VERSION` stays `3`.** Not bumped, for the three
reasons in [Architecture Approach](#architecture-approach): precedent, zero
protection on the local socket that carries the actual exposure, and the
active harm of arming the silent LAN-peer drop that spec 015 FR-014
forbids. `HANDOFF_VERSION` is likewise unchanged — `HandoffState` never
carried notes, so the handoff payload shape is identical.

Mixed-version behavior is an accepted, bounded outcome, documented in
`lat.md/protocol.md` and in the spec's `## Clarifications` as the durable
decision record. It is not verified by exercising a live upgrade.

### User-facing surface changes

- **Titlebar notes button gone.** Equalize sits adjacent to the gear with
  no gap; the button is a plain flex child, so the siblings reflow and no
  unreachable dead region can survive. Gear and window controls remain
  right-anchored and visually unmoved. Keyboard tab-order skips the
  removed control — `has_keyboard_focus` drops exactly one clause and the
  remaining focus chain still cycles.
- **Workspace-badge hover preview gone.** Hovering the badge shows no
  overlay, for a server workspace or otherwise. (Note this also retires
  `lat.md/architecture.md`'s stale claim that the hover preview "remains
  unwired" — it was wired, via the titlebar hit band.)
- **`ctrl+shift+m` unbound.** It is not swallowed and not reassigned; it
  becomes a clean free slot exactly as `ctrl+shift+n` was when the notes
  modal was relocated off it. The chord **falls through to the PTY as
  `0x0D`**, because the encoder runs `TerminalMode::legacy()`,
  `translate_character_with_modifiers` never reads `modifiers.shift`, and
  `char_to_control_byte` maps `'m'` and `'M'` alike. **This is documented
  behavior, not a defect to fix**: eleven other `ctrl+shift+<letter>`
  combos (`a e g h i j l o r s y`) already do exactly this today —
  `ctrl+shift+j` emits `0x0A` and submits the line, `ctrl+shift+s` sends
  XOFF and freezes output. Swallowing `m` would itself be the regression,
  since Ctrl-M is a legitimate keystroke readline and TUI apps expect, and
  swallowing it would be arbitrary unless `j`, `s`, `r`, and `l` were
  swallowed too.
- **No user configuration can break.** `OverlayChord` sits outside
  `KeybindingsConfig` by design, so no config can name the notes chord and
  none can fail to parse; and `translate_overlay_chord` already yields to
  any configured binding, so a user who had rebound `ctrl+shift+m` was
  never reaching the notes modal in the first place.

**Compile-enforced detail:** `OVERLAY_CHORDS` is declared
`[(&str, OverlayChord); 5]`. The length literal **must** drop to `4` or
the crate will not compile. `keybindings/tests.rs` iterates
`OVERLAY_CHORDS` generically and needs no edit — it simply covers 4 rows —
and `tests/fixtures/gpui-client/keyboard-byte-golden.json` has no
`ctrl+shift+<letter>` case at all.

### Gate-document interface changes

`specs/016-gpui-client-rebuild/parity-inventory.md` is parsed live against
`protocol.rs`, so it is an interface in the mechanical sense. Five rows go
(`WorkspaceNotesGet`, `WorkspaceNotesMutate`, `WorkspaceNotesSnapshot`,
`WorkspaceNotesChanged`, `Workspace notes hover preview`); headings
`(47 sent)` -> `(45 sent)`, `(59 handled)` -> `(57 handled)`, `(29)` ->
`(28)`; footers 47 -> 45, 59 -> 57, 29 -> 28; roll-up Client messages
47 -> 45, Server messages 57, Spec behaviour requirements 29 -> 28, and
**Total** 204 -> 199; prose "195 rows … 195 are reachable (100%)" ->
190/190, "1 of those 195" -> "1 of those 190", "194 of 195" -> "189 of
190". The Input/keybinding (54) and Rendering/window (6) tables are
**UNCHANGED** — `keybindings.rs::pub struct Bindings` has no notes field.

`US4-3` in `specs/016-gpui-client-rebuild/spec.md` is amended using the
existing inline-annotation precedent (`US1-8`, `US3-10`):

```markdown
- **US4-3** *(descoped 2026-08-01, bead <EPIC-ID>: the workspace notes modal and
  hover preview are removed from the product)* Workspace system (accent colors,
  badges, workspace splits) works as today.
```

Its coverage cell then drops the `Workspace notes hover preview` and
`WorkspaceNotesSnapshot` carriers, leaving `Workspace accent colours and
badges` and `workspace_split_vertical`. `US4-3` is the only coverage cell
naming a notes row, so `48 spec requirements carried` stays unchanged.

## Testing Strategy

**No new test code is written.** Constitution principle 3 permits adding
test code only when explicitly requested or when existing coverage must
change; neither applies. Deleting `tests/e2e/visual/workspace-notes.sh`
and the modal/preview unit tests is a coverage *removal* that follows its
feature out the door, not a coverage gap to backfill. Every story instead
gets an independent, user-reachable verification path, which principle 3
requires regardless.

The mechanical oracle is the spec's **two-stage completion gate**, run
verbatim from the repo root, with **both stages returning zero lines**. It
is reproduced here verbatim so the quality-gate work item is self-contained
and needs no cross-document lookup:

```bash
COMMON=(--hidden --pcre2 -g '!.git' -g '!target' -g '!.worktrees' \
        -g '!node_modules' -g '!*.lock' -g '!specs/**' -g '!.beads/**')

# GATE A: hard ban. Any hit = removal incomplete.
A='(?i)workspace[-_ ]?notes|WorkspaceNote|ArchiveReason|AddingNote|WORKSPACE_NOTES|notes_(focus_handle|workspace_id|preview|modal|adopted)|(Create|Archive|Edit|Empty)Note\b|\bnote_id\b|(active|archived)_notes|hovered_note_id|NOTE_LIST_ROWS|NOTE_TEXT|hover_notes_affordance|requested_notes_workspace'
rg -n "${COMMON[@]}" "$A" .

# GATE B: remaining note-bearing identifiers minus the false-positive allowlist.
ALLOW='note_row|Role::Note|settings-note|NOTE_MAX_CHARS|tailnet_note|trust_status_notes|note_activity|note_active|note_inactive|note_unpaced_resize_apply|note_external_apply|ENABLE_FOOTNOTES|loading_note|STARTUP_NOTE|RELEASE_NOTES|notes-file'
rg -on "${COMMON[@]}" '(?i)\w*note\w*' . \
  | rg -v  --pcre2 "$ALLOW" \
  | rg -iv --pcre2 ':(notes?|noted|noting)$'
```

- **GATE A** — hard ban on every workspace-notes identifier form,
  including the hyphenated `workspace-notes` used by the `justfile`
  recipe, the E2E filenames, and the parity inventory. Any hit means the
  removal is incomplete.
- **GATE B** — every remaining `note`-bearing identifier minus the
  documented false-positive allowlist, matched **per file** (the
  `render_note_row` / `note_count` trap).

`specs/**` and `.beads/**` are excluded by design as historical records —
`.beads/interactions.jsonl` is an append-only audit log that must not be
rewritten — and that exclusion is emphatically *not* a licence to skip the
mandatory `specs/016-*` gate edits, which a different mechanism checks.
`lat.md/` is deliberately inside the gate.

**Plan-time measurement, replacing the spec's baseline sentence.** Run
against this worktree at a clean `cfcc84d`, GATE A returns **710 lines
across 24 files** — one more than the 709 the spec records — and GATE B
returns 842 tokens. More importantly, the spec's claim that simulating the
file deletions "leaves only genuine workspace-notes identifiers and zero
false positives" is **false for one file**:
`tests/e2e/visual/tab-window-chords.sh` survives the removal and still
matches GATE A on its two `workspace-notes` comment tokens. It is the only
hit file that no deletion accounts for, which is why it now carries an
explicit reword under the `justfile`/E2E work item. With that reword the
gate is clean **iff** the removal is complete; without it, GATE A can never
reach zero.

**The gates are not a substitute for reading the `lat.md` prose.** Two
independent filters make the singular form invisible. GATE A's leading
alternative is `(?i)workspace[-_ ]?notes`, which requires the trailing `s`
and therefore does **not** match `workspace-note`. GATE B reduces every hit
to the bare captured token and then drops anything ending in
`:(notes?|noted|noting)`, which strips the bare `note` that
`workspace-note` yields. `lat.md/architecture.md:16` — "socket path
conventions, and workspace-note wire types" — is exactly this case: it
becomes false the moment `protocol.rs` loses the six types, and **neither
gate reports it**. The `lat.md` sweep must therefore be read line by line,
not merely grepped, and its acceptance criteria name the prose targets
explicitly rather than leaning on a green gate.

Alongside it:

- **`just ready`** — the workspace denies clippy `pedantic` and `cargo`,
  so unused imports and newly-single-variant matches **FAIL rather than
  warn**; this is the dominant source of removal fallout and the reason
  every shared hot spot is edited down rather than bulk-deleted.
  `tools/check-no-new-lint-suppressions.sh` must stay green with **no new
  `allow`/`expect`** introduced to silence removal fallout.
- **Both count-gates.** `tools/check-reachability.sh` must report `64/64`
  modules, `52/57` server messages, `36/36` layout actions.
  `tools/check-parity-inventory.sh` must exit 0 with `199 rows, 199
  reachable, 0 unwired, 0 missing (190 user-facing, 189 reachable
  in-client, 48 spec requirements carried)` and `--gate` reporting
  `GO — 190 of 190 user-facing rows reachable (100%)`. Run both with
  `--working-tree` after **every** doc edit: they are instant, need no
  build, and discovering a mismatch after a cold GPUI build is the single
  largest effort risk in this change.
- **`lat check`** — no dangling `[[wiki link]]`, no `// @lat:` anchor
  pointing at a deleted section, and no leading-paragraph violation
  introduced by deleting a section's first paragraph.

### Per-story verification map

| Story | Verification path |
|---|---|
| 1 — UI affordances gone | **Client-only launch** against the already-running server. Confirm: no notes button in any window state; equalize adjacent to the gear with no gap; gear and window controls unmoved; hovering the workspace badge shows no overlay; window dragging still works (the `e530da7` regression check); tab-order cycles correctly. |
| 2 — `ctrl+shift+m` opens nothing | `keybindings/tests.rs` (unchanged, now covering 4 rows) plus a manual chord press in the launched client: no overlay, no status message, and `0x0D` reaching the PTY as expected. Array arity is compiler-enforced. |
| 3 — Stored data deleted | **Ordered, and the order is the criterion.** First the **stale-file startup check** (step 3b): on the post-removal build, with `~/.local/state/scribe-dev/workspace_notes.toml` *still present*, start a short-lived **dev** daemon and confirm it neither reads the file, nor logs a warning naming it, nor recreates or rewrites it (mtime and size unchanged). *Then* step 4's deletion, and *then* the `find` check returning empty. Running the deletion first destroys the only observable this check has, and keeping a copy across step 4 to recover it is the backup the Non-Goals and Goal 7 forbid. The dev daemon is a separate process; it does not touch the live server, and the single approved restart belongs to step 3. |
| 4 — No dead code | GATE A + GATE B returning zero, `just ready` clean with no new suppressions, and the retained-by-design items (`connected_window_writers`, `PaneShell::is_server_workspace`, the `toml` dep) still present and compiling. |
| 5 — `lat.md` reflects reality | `lat check` green, plus GATE A/B over `lat.md/`, which is inside the gate. |
| 6 — Live upgrade degrades to a blip | **Documentation, not execution.** The decision record in `lat.md/protocol.md` and `## Clarifications`, plus a diff assertion that `REMOTE_PROTOCOL_VERSION` is untouched. The live server is **not** restarted for this story; the single approved restart belongs to Story 3 step 3. |

**The client-only launch is permitted.** The standing rule forbids
restarting the *server*, not launching a *client* — and the user has since
authorized restarts outright for the Story 3 sequence, so this is doubly
clear.

**The Story 1 oracle gap is real and stated honestly.**
`tests/e2e/visual/titlebar.sh` and `window-chrome-bands.sh` survive but
assert nothing about the notes button, so after `workspace-notes.sh` is
deleted there is no automated titlebar-notes oracle at all. The manual
launch is the mitigation, and per principle 3 that is an acceptable
documented path rather than a reason to write new tests.

## Risks

**Mixed-version window during upgrade.** An old client emitting a deleted
frame at a new server severs the connection — and the trigger is **hover,
not a deliberate chord press**, since `set_workspace_notes_preview` fires
from the titlebar's `on_mouse_move` band. The window is roughly 1-4 s on
the packaged `postinst` path but **indefinite** under `just restart-server`
/ `just restart-server-release`, which **do not touch clients at all**, and
in four other `postinst` fallback branches. Both directions break, and
under the rejected staged-deprecation option the blast radius would have
been plural (`broadcast_workspace_notes_changed` fans to every connected
window writer). *Mitigation is operational, not code:* restart the server
and the clients together — which the Story 3 step 3 sequence does anyway.
*Residual severity:* a red status dot and one status line; PTYs,
scrollback, and typed input all survive.

**The parity-inventory recompute is eight interlocking hand-maintained
numbers.** Rows, three headings, three footers, the roll-up Total, three
prose figures, and the `US4-3` coverage cell must all move together, and
the gate parses the file live against `protocol.rs` — a partial edit fails
with *"the 'Client messages' table names unknown entries."* *Mitigation:*
run `tools/check-parity-inventory.sh --working-tree` after **every** doc
edit, not at the end. It is instant and needs no build.

**Cold `target/` makes the first `just ready` slow.** This worktree has
never been built and GPUI compiles at `opt-level = 3` even in debug. The
three script gates are green on this baseline but `clippy` and `test` have
not been run here. **Decision, made here rather than deferred: accept the
cold gate in this worktree.** The warm-primary-checkout alternative was
considered and rejected — it means carrying a ~64 GB `target/` and
cross-checkout state over the very files this change edits, which turns a
slow build into a correctness hazard on the exact surface under surgery.
*Mitigation:* front-load every script gate (both count-gates, `lat check`,
GATE A/B) so nothing waits on the build, and treat the cold build as a
scheduling cost rather than a risk. This is recorded as a stated
precondition on the quality-gate work item.

**Compiler-invisible or-pattern hazards.** `daemon.rs`'s
`dispatch_notice_message` arm survives on `Error`, `SessionList`,
`SearchResults`, `PromptMark`, and `PromptReceived`; deleting the arm
instead of two `|` lines silently reroutes them. The `e530da7`
`on_mouse_move` interleave is worse: over-deleting breaks window dragging
on X11/Wayland with no compiler error and no test coverage. *Mitigation:*
both are called out as explicit acceptance criteria on their own work
items, and the manual client launch checks dragging.

**Stale line numbers.** Every `path:NNN` in the spec predates the rebase;
`titlebar.rs` moved by tens of lines and `lat.md/client.md` shifted +27
below its line 719. In the original survey `has_keyboard_focus`'s cited
range actually pointed at the ctrl+shift+arrow tab-reorder handler and the
hit band's range pointed inside a `#[gpui::test]` block. *Mitigation:* the
re-verify work item blocks everything else and produces a symbol-based
authoritative edit list; no criterion anywhere depends on a line number.

**Rollback.** The code is fully revertible: `git revert` restores every
deleted module, the protocol variants, the chord, the docs, and — because
they land in the same commit — the `specs/016-*` gate numbers, so the gates
stay self-consistent in both directions. The revert is *safe precisely
because `REMOTE_PROTOCOL_VERSION` is untouched*: there is no version a peer
could have moved past and be stranded on. **The note data is not
revertible.** Story 3 step 4 is the point of no return — by explicit
design there is no backup, archive, or export to restore from. Everything
before step 4 is undoable; nothing after it is.

**Communication (optional, cheap) — ownership resolved.** There is no
`CHANGELOG` and no README mention, but release notes are the annotated git
tag body and the client renders them in-app on the settings Releases page.
Given "no backups," a user's first signal could otherwise be data already
gone. One sentence in the tag body naming the removal and the data deletion
closes that. **No work item is created for it.** Tagging is not part of
this epic, the sentence cannot be written until a release is actually cut,
and inventing a bead for it would leave an item that can never be closed
inside this epic's lifetime. The decision is therefore **explicitly
deferred to the user at release time**, and recorded here so it is a
deliberate omission rather than a floating sentence with no owner.

## Sequencing

This section is the bead DAG. Dependencies are stated as explicit "X
blocks Y" edges rather than numbered title prefixes; there are no step
codes to cite.

**Read this as work order inside ONE commit, not as a series of commits.**
Both count-gates run as pre-commit hooks in `--staged` mode, so all code
edits and all gate-document edits must be staged together. Nothing here
implies an intermediate landing, and the tree does not compile between the
protocol deletion and the last consumer edit — which is why intermediate
items' acceptance criteria are diff-shaped rather than
"`cargo check` passes."

**Twenty-three work items.** None is P4 and none carries a placeholder or
TBD acceptance criterion.

**Re-verify the scope survey against the rebased tree** (P0). Walk every
construct named in `spec.md` `## Constraints` by symbol, heading, or
literal text against `cfcc84d` and produce the authoritative edit list.
**The edit list is written to
`specs/018-remove-workspace-notes/edit-list.md`.** Naming a destination is
not bookkeeping: every downstream item consumes "the authoritative edit
list", and without a path they consume a phrase. Downstream beads cite that
file as an input. Must explicitly confirm: `ArchiveReason` has no non-notes
reference; the `daemon.rs` or-pattern shape; whether
`server_message_variant` is two independent arms (as observed at plan time)
or a grouped or-pattern (as the spec claims); the `titlebar.rs`
`on_mouse_move` interleave and the exact trailing band lines; the
`OVERLAY_CHORDS` arity literal; the `justfile` recipe name; the exact
current comment text at both `workspace-notes` occurrences in
`tests/e2e/visual/tab-window-chords.sh`; the six identifiers of the
already-dead subset's third bullet — that `adding_note_states`,
`focused_inline_editor`, `affordance_hovered_workspace`, and
`draw_affordance` are `lat.md/client.md`-only, and that
`workspace_notes_save_pending` and `PreviewLayout` are live source inside
files deleted outright; and the two count-gate baselines at `cfcc84d`
(reachability `67/67, 54/59, 36/36`; parity `204 rows … 195 user-facing`).
Acceptance is the existence of `edit-list.md` with a symbol-located entry
for every construct above and an explicit verdict on each confirm item.
**Blocks every other item.**

**Delete the protocol types and variants in `scribe-common`** (P0). Six
types and four variants out of `protocol.rs`; `REMOTE_PROTOCOL_VERSION`
untouched; the gate's hard prerequisites (enum declarations, 4-space
variant indentation, balanced function bodies) preserved. Blocked by the
re-verify. **Blocks exactly two items: the reachability recompute and the
parity-inventory recompute** — the two gate scripts that actually parse
`protocol.rs` and derive their numbers from it. It deliberately does **not**
block the server-side, client-side, or `scribe-test` items. Nothing
compiles mid-flight, so there is no mechanical edge to enforce; the edit
list those items consume comes from the re-verify item, not from this one;
and `keybindings.rs` (`OverlayChord`) and `titlebar.rs` (`TitlebarEvent`)
have no protocol coupling whatsoever — neither file so much as imports
`scribe_common::protocol`. Removing the informational edges widens the
post-re-verify frontier from one item to roughly nine.

**Remove the server side** (P0). Delete
`crates/scribe-server/src/workspace_notes.rs`; edit `lib.rs`, `main.rs`,
and `ipc_server.rs` (import, `use`, state field, dispatch arms,
workspace-dispatch arms, the three handlers, doc prose). Acceptance
includes **`connected_window_writers` still present** and the `toml`
dependency retained. Blocked by the re-verify. Runs in **parallel** with
all client-side items.

**Delete the client notes modules and their `lib.rs` declarations** (P0).
Five files removed; three `pub mod` lines gone. Acceptance: the 16
`// @lat:` note anchors leave the repo with these files, and
`rg -n '@lat.*Workspace Notes' crates/` returns nothing. Blocked by the
re-verify. **Blocks the `lat.md` deletions item** — see that item for why
the direction matters.

**Delete the notes-only surface in client `main.rs`** (P0). Everything in
`main.rs` that exists solely to serve notes and has no shared consumer:
the imports and the `ArchiveReason` entry in the `use` list; `Shared::notes`
and its constructor line; `WorkspaceNotesPreviewSurface`; the `TerminalView`
note fields and their init; the eleven notes functions
(`notes_workspace_id`, `open_workspace_notes_modal`,
`set_workspace_notes_preview`, `sync_workspace_notes`,
`sync_workspace_notes_modal`, `sync_workspace_notes_preview`,
`route_workspace_notes_action`, `send_workspace_notes_mutation`,
`handle_notes_modal_key`, `build_workspace_notes_preview_overlay`, and
`on_workspace_notes_message`); the `ReaderCtx` field and its clone; and
`WORKSPACE_NOTES_ERROR_PREFIX`. Acceptance is diff-shaped: each named
symbol is absent from `main.rs`, and no shared hot spot listed in the next
item has been touched yet. Blocked by the client-module deletion. **Blocks
the shared-hot-spots item.**

**Edit down the shared hot spots in client `main.rs`** (P0). The hazardous
half, and the reason the surgery is split: every construct here has a
surviving non-notes consumer and must be **edited down, never
bulk-deleted** — `open_overlay_chord`, `overlay_free`, the keyboard routing
chain entry, the `Render::render` sync pass, the `notes_preview` build,
the render child order, `server_message_variant`, the reader routing arm,
and `on_server_error`. All ordering-preservation criteria live here:

- the keyboard routing chain's survivors keep their relative order around
  the deleted entry (the notes entry sits **between** the dialog and
  find-overlay handlers);
- the `Render::render` sync pass loses only the notes call, with
  `sync_find_results` and `sync_remote_connect` still adjacent in that
  order;
- **`displaced` remains the last render child**;
- `on_server_error` loses **only** the leading `WORKSPACE_NOTES_ERROR_PREFIX`
  block; the trailing `set_status` is shared and survives verbatim;
- `server_message_variant` still names every surviving `ServerMessage`
  variant (the reachability gate `die`s otherwise);
- **`open_overlay_chord` has exactly four dispatch arms after the edit, one
  per surviving `OverlayChord` variant.** This last one is the only
  verification path Story 2's "the four remaining overlay chords still open
  their overlays" has at the code level: `keybindings/tests.rs` validates
  the chord *table*, not the `match`, so an over-deleted arm would compile
  as a non-exhaustive-match error only if the variant survived, and would
  pass silently if both went.

Blocked by the notes-only-surface item.

**`titlebar.rs`, including the `e530da7` carry-forward** (P0). Events,
focus handle and init, `render_workspace_notes_button` and its call
site/child insertion, one `has_keyboard_focus` clause, and the trailing
`on_mouse_move` band lines **only**. Explicit acceptance: `advance_move_arm`'s
early-return and the `update_drag` call are preserved verbatim, and
`titlebar.rs` afterwards contains no hard-coded pixel hit band. Blocked by
the re-verify. Parallel with the `main.rs` chain.

**`keybindings.rs` chord and array arity** (P0). Remove
`OverlayChord::WorkspaceNotes` (including its doc comment, which is itself
a GATE A hit) and the `ctrl+shift+m` entry; drop the array type literal
`5` -> `4`. `keybindings/tests.rs` is **not** edited. Blocked by the
re-verify. Parallel with `main.rs` and `titlebar.rs`.

**`ipc_bridge.rs`** (P0). Import, `workspace_notes_get`,
`workspace_notes_mutate`, neighbouring doc prose. Blocked by the
re-verify. Parallel with the other client items.

**`scribe-test` daemon or-pattern edit** (P0). Delete exactly two `|`
lines from the `dispatch_notice_message` arm; the arm survives on its
other variants. Acceptance names the surviving variants explicitly
(`Error`, `SessionList`, `SearchResults`, `PromptMark`, `PromptReceived`)
and asserts the arm itself is still present. Blocked by the re-verify.
Parallel with everything client-side.

**Remove the `justfile` recipe and the E2E script, and reword
`tab-window-chords.sh`** (P1). Delete `tests/e2e/visual/workspace-notes.sh`
and the `e2e-visual-workspace-notes` recipe plus its comment block, located
by name. Then reword the two comment lines in
`tests/e2e/visual/tab-window-chords.sh` that carry the literal
`workspace-notes` token — the file is **inside** GATE A and is otherwise
the one hit file no deletion accounts for. `opened the workspace-notes
modal.` becomes `opened the since-removed notes modal.`, and `The
workspace-notes modal used to own this chord` becomes `A since-removed
modal used to own this chord`. Acceptance: the script's executable body is
byte-identical, both comments still explain why the test exists, and
`rg -n 'workspace-notes' tests/` returns nothing. Blocked by the re-verify
only; parallel with all code items.

**Recompute `tools/reachability-baseline.txt`** (P0). `modules-total` and
`modules-wired` 67 -> 64; `server-messages-total` 59 -> 57;
`server-messages-handled` 54 -> 52; `layout-actions-*` unchanged; the five
`unhandled-server-message` lines unchanged. Verify with
`tools/check-reachability.sh --working-tree`. Blocked by the `lib.rs`
module deletion and the protocol item (the script derives counts from
source).

**Recompute the parity inventory and amend `US4-3`** (P0). Five rows,
three headings, three footers, the roll-up Total 204 -> 199, three prose
figures; then the `US4-3` inline descope annotation, its coverage-cell
trim, and a dated decision paragraph at the tail of
`## Requirement register`. The Input/keybinding and Rendering/window
tables stay unchanged. Verify with
`tools/check-parity-inventory.sh --working-tree` and `--gate` after each
sub-edit. Blocked by the protocol item. **Instruction, not a dependency
edge:** the `US4-3` annotation embeds a `<EPIC-ID>` placeholder that must
be replaced with **this bead's own parent epic id**. An earlier draft
expressed this as "blocked by the epic existing", which is not a DAG edge
at all — the epic is minted by the same `create-beads` step that mints this
bead, so converting that phrase to an edge produces a dangling reference to
a node the DAG does not contain.

**Scrub the stale `specs/**` cross-references** (P3). The opportunistic,
non-gating tail, split out of the parity recompute because a single bead
cannot be both P0 and P3: the two stale count paragraphs in
`parity-inventory.md`, two figures in
`specs/016-gpui-client-rebuild/plan.md`, `launch-gate-checklist.md`'s
"41-script visual suite" -> 40 plus its `workspace-notes` E2E name entry,
and the stale historical mentions in
`specs/016-gpui-client-rebuild/{reachability-audit.md,accessibility-audit.md}`
and `specs/006-persist-terminal-env/research.md`. **All of these sit under
`specs/**`, which the completion gate excludes by design and which neither
`--staged` count-gate hook matches** (the parity hook's file regex covers
only `parity-inventory.md` and `spec.md`). So none of them can block Goal 1
and none of them gates the atomic commit — this item may land inside that
commit if it happens to be done in time, or in a separate follow-up commit
afterwards, with no difference to any gate. Acceptance is diff-shaped: each
named figure updated, each named stale mention removed or corrected.
Blocked by the P0 parity recompute, whose recomputed numbers it echoes.

**Delete the `lat.md` notes sections and fix the surrounding prose** (P1).
Delete `## GPUI Workspace Notes` and all subsections up to `## App State`,
and `## Workspace Notes` + `### Inline Note Editor` up to `## Input`, in
`client.md`; delete `### Workspace Notes` and fix the `### State
Transfer` sentence in `server.md`; delete the two notes blocks in
`protocol.md` and fix the trailing "and notes messages" in the
`REMOTE_PROTOCOL_VERSION` paragraph; delete `### Workspace notes on the
wire` in `test.md`. In `architecture.md` fix **two** lines, not one: the
stale "the workspace-notes hover preview remains unwired" claim at
`architecture.md:148`, **and `architecture.md:16`**, whose `scribe-common`
summary reads "socket path conventions, and workspace-note wire types" —
false the moment `protocol.rs` loses the six types, and invisible to both
gates because GATE A's pattern requires the plural `notes` and GATE B
strips the bare `note` token that the singular form yields. Surgical prose
edits so remaining sentences are **true, not merely truncated**: titlebar
tab order, overlay chord precedence, input keyboard chain level 2,
hit-testing rects, IME immutable-surface gate list, and the closing prose
of `### Window move region` (new since the original survey — it lists
"workspace-notes" among controls that stop propagation on left press).
Also drop the four docs-only identifiers confirmed by the re-verify:
`adding_note_states`, `focused_inline_editor`,
`affordance_hovered_workspace`, `draw_affordance`. Line-by-line treatment,
not a bulk section delete.

**Acceptance is diff-shaped**, per this plan's standing rule: each named
heading is absent, each named prose sentence reads true against the
post-removal tree, and `architecture.md:16` and `:148` are both edited.
`lat check` green is **not** an acceptance criterion here — it is a
cross-cutting check and belongs at the join point, so it moves to the
quality-gate item.

**Ordering, and the direction matters.** Blocked by the re-verify **and by
the client-module deletion**. All 16 `// @lat:` anchors in the five deleted
client files point into `client#GPUI Workspace Notes` subsections; deleting
that section while those files still exist leaves 16 dangling code refs and
`lat check` fails. The reverse direction is safe: `lat.md/client.md` has no
`require-code-mention` frontmatter, so a section with no code mentioning it
is not an error. Files first, sections second.

**Record the compatibility decision in `lat.md/protocol.md`** (P1). Split
from the sweep above because it is authoring rather than deleting, with a
different acceptance shape. Write the durable decision record: the four
message variants and six types are deleted outright with no deprecation
window and no no-op arms; `REMOTE_PROTOCOL_VERSION` is deliberately left at
`3`; and the reasoning for both halves — the local Unix socket has no
version negotiation so a bump protects nothing, a bump arms the silent
LAN-peer drop that `specs/015-multi-machine-sharing/spec.md` FR-014
forbids, and precedent has changed remote-visible semantics without bumping
three times. Acceptance: `lat.md/protocol.md` contains a section stating
all three grounds and the `3` decision, its leading paragraph is under 250
characters, and no `[[wiki link]]` in it points at a deleted section.
Blocked by the `lat.md` deletions item, since it writes into the same file
the deletions touch.

**Run the quality gate** (P0). The join point of the DAG and the first
moment the tree compiles. `just ready` clean; **and `pre-commit
run --all-files`** — a distinct runner, not a synonym: the two count-gates
run there in `--staged` mode rather than `--working-tree`, and `cargo fmt
--check` is exercised only there. Find the runner via `git config --get
core.hooksPath` and the entrypoint it invokes. Also: no new
`allow`/`expect`, both count-gates reporting their recomputed figures,
`lat check` green (moved here from the `lat.md` item), and GATE A + GATE B
both returning zero lines using the block reproduced in
[Testing Strategy](#testing-strategy). **Stated precondition:** this runs
cold in this worktree by decision (see [Risks](#risks)) — `target/` is
empty and GPUI builds at `opt-level = 3`, so every script gate is
front-loaded and finished before the build starts. **Depends on every code
item, both gate-document items, and both `lat.md` items.**

**Manual client-launch verification** (P2). Launch **only a client**,
against the already-running server, from the freshly built debug binary at
`target/debug/scribe-client` with no `SCRIBE_LAN_DIAL` or
`SCRIBE_REMOTE_DIAL` set so it dials the local socket. Confirm Story 1's
and Story 2's criteria: no notes button in any window state; equalize
adjacent to the gear with no gap; gear and window controls unmoved;
hovering the workspace badge shows no overlay; window dragging still works
(the `e530da7` regression check); tab-order cycles; `ctrl+shift+m` opens
nothing and no status message appears. **Also press each of the four
surviving overlay chords and confirm each opens its overlay** — this is
the only end-to-end check that `open_overlay_chord` was edited down rather
than damaged. **Evidence: capture a screenshot of the titlebar** and attach
it to the bead; "looked fine" is not a record.

**Known hazard, so it is not misread as a regression.** At this point the
client is new but the **server is still old** — it is not restarted until a
later item. If any *other*, still-old client mutates notes while this
verification runs, the old server's `broadcast_workspace_notes_changed`
fans a `WorkspaceNotesChanged` frame to every connected window writer,
including this new client, which cannot decode it and disconnects. That is
the documented mixed-version blip: a red status dot, one status line, and
an automatic reconnect with sessions and scrollback intact. It is expected,
harmless, and **not** a defect in this change. Blocked by the quality gate.

**Stage all code and gate-document edits and create the single atomic
commit** (P0). Its own item because it is the highest-risk mechanical step
in the plan and, until this revision, appeared only as a buried sub-step.
The whole atomicity argument in
[Architecture Approach](#architecture-approach) rests on the `--staged`
pre-commit hooks, which behave differently from the `--working-tree`
invocations every earlier item used. **Plan-time correction:** there are
**three** `--staged` hooks in `.pre-commit-config.yaml`, not two —
`reachability-baseline` and `parity-inventory` are the count-gates, and
`no-new-lint-suppressions` also runs `--staged`. `cargo fmt --check` runs
only under the hook runner. Acceptance: `pre-commit run --all-files` green
(runner located via `git config --get core.hooksPath`); all three
`--staged` hooks pass; **exactly one commit** contains every changed code
file plus `tools/reachability-baseline.txt`,
`specs/016-gpui-client-rebuild/parity-inventory.md`, and
`specs/016-gpui-client-rebuild/spec.md`; and `git revert` of that single
commit restores gate self-consistency in both directions, verified by
running both count-gates `--working-tree` on the reverted tree and then
resetting. Blocked by the quality gate **and** by the manual verification.
**Blocks the rebuild/reinstall/restart item.**

**Rebuild, reinstall, and restart** (P1). Steps 2 and 3 of the Q5 sequence:
rebuild and reinstall from the committed tree, then restart the servers and
clients so no live process retains notes code or the in-memory store.
**This is the one item authorized to restart the server**, and the approval
covers this step alone. Acceptance: the installed binaries at
`/usr/bin/scribe-client` and `/usr/bin/scribe-dev` are the post-removal
build, and no running process predates the restart. Blocked by the commit
item. **The state files are still on disk at the end of this item** — they
are not deleted here.

**Stale-file startup check on a dev daemon** (P1). Step 3b, and it exists
as its own item precisely so it cannot be reordered after the deletion.
With `~/.local/state/scribe-dev/workspace_notes.toml` **still in place**,
start a short-lived **dev** daemon on the post-removal build and confirm it
neither reads the file, nor logs a warning naming it, nor recreates or
rewrites it. Acceptance: mtime and size are identical before and after, and
the daemon's log contains no line naming `workspace_notes.toml`. The dev
daemon is a separate process from the live server and consumes none of the
single restart approval. **This check may NOT be satisfied by keeping a
copy of the file past the deletion** — that copy is the backup the
Non-Goals and Goal 7 forbid. Blocked by the rebuild/reinstall/restart item.
**Blocks the deletion.**

**Delete the note data** (P1). Step 4, the point of no return, alone in its
own item. Run the two `rm -f` commands from
[Data Model](#data-model) against both state directories, then the `find`
verification, which must return empty. **MUST be last.** No backup,
archive, or `.bak` is written; the state directories are **not** removed
wholesale. Blocked by the stale-file startup check.

**File the follow-up beads outside this epic** (P3). The four items under
[Follow-up beads](#follow-up-beads-out-of-scope-for-this-epic) — the silent
LAN-peer drop, the whole-connection drop on an undecodable frame, the
reconnect backoff that never resets, and the dangling "Re-gate criterion
B1" reference — are named in prose but nothing has ever created them.
Acceptance: four beads exist outside this epic, each carrying its file and
symbol pointer, and none is parented to this epic. Blocked by nothing in
the code path; may run any time after the re-verify.

### Parallelism

**The re-verify is the only universal blocker.** The moment it completes,
nine items open at once, because dropping the protocol item's informational
edges removed the artificial bottleneck: the protocol deletion itself,
server-side removal, `titlebar.rs`, `keybindings.rs`, `ipc_bridge.rs`, the
`scribe-test` edit, the `justfile`/E2E/`tab-window-chords` item, the
client-module deletion, and the follow-up-bead filing. The protocol item
still blocks the two recompute items, which are the only things that parse
`protocol.rs`.

Two short serial chains run inside that frontier. The client `main.rs`
chain is module deletion -> notes-only surface -> shared hot spots. The
docs chain is module deletion -> `lat.md` deletions and prose ->
`lat.md/protocol.md` decision record; the module deletion must come first
or `lat check` sees 16 dangling anchors. The `specs/**` scrub trails the
P0 parity recompute and gates nothing.

Everything converges on the quality gate, and from there the tail is
strictly serial with no parallelism at all: quality gate -> manual
client-launch verification -> the single atomic commit -> rebuild /
reinstall / restart -> stale-file startup check -> delete the data. Each
arrow in that tail is a real ordering constraint, not a preference.

### Follow-up beads, out of scope for this epic

These are surfaced by the research, not caused by the removal, and are
filed separately so the removal stays atomic. **They are filed by their own
work item** — see "File the follow-up beads outside this epic" in
[Sequencing](#sequencing) — because naming them in prose has never yet
caused a bead to exist.

**Reconciliation with the spec.** `spec.md` enumerates three; this plan
lists four. The difference is not a disagreement: the third bullet below,
**reconnect backoff never resets**, was surfaced at *plan* time while
tracing the mixed-version blip, after the spec reached CLARIFIED. It is
verified accurate — `crates/scribe-client/src/main.rs:7040` reads
`reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY)` inside
`supervise_connection`'s retry loop, with no assignment back to
`INITIAL_RECONNECT_DELAY` anywhere after a successful connection. It is
marked as plan-time-surfaced here so the two lists reconcile without
editing the spec.

- **Silent LAN-peer drop** — the bare `continue` at
  `crates/scribe-client/src/remote.rs:264` filters version-mismatched LAN
  peers out of the connect picker with no row, no error, and no
  explanation, violating `specs/015-multi-machine-sharing/spec.md` FR-014
  ("a version mismatch MUST resolve to an explicit, understandable
  outcome — never silent misbehavior"). Dormant today because no released
  build speaks any version but `3`; a future bump arms it. The explicit
  tailnet dial path already has mismatch copy; the LAN picker does not.
- **Whole-connection drop on an undecodable frame** — `ipc_server.rs`
  maps any decode error to `LoopExit::Disconnected`, even though
  length-prefixed framing (`crates/scribe-common/src/framing.rs`) means
  the stream never desyncs and the frame could simply be skipped. The
  disconnect is a policy choice, not a necessity.
- **Reconnect backoff never resets** *(surfaced at plan time, not in
  `spec.md`)* — `supervise_connection`'s backoff climbs 100 ms -> 2 s via
  `reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY)` and is
  never reset after a successful reconnect, so a session that has
  reconnected once pays the ceiling on every later blip.
- *(Optional, smallest)* **Dangling "Re-gate criterion B1" reference** —
  `specs/016-gpui-client-rebuild/plan.md:535-540` cites a criterion that
  exists nowhere in the repo. Note the related "recorded decision" phrase
  in `tools/check-parity-inventory.sh` is a `print STDERR` inside the
  NO-GO branch and is never parsed; the normative rule is that `plan.md`
  prose.

## Backlog Refinement

None — this run originates from a direct user request, not from a P4
backlog issue. No backlog inputs to refine.

## Target Epic

No existing epic was supplied by the user and none could be inferred —
there are no backlog inputs to infer from. A **new feature epic will be
created at the `create-beads` step**, with the twenty-three work items
above decomposed into task beads under it.

The epic id is load-bearing for one edit: the `US4-3` descope annotation
in `specs/016-gpui-client-rebuild/spec.md` carries a `<EPIC-ID>`
placeholder that must be replaced with the real id. This is recorded as an
**instruction on the parity-inventory work item** — "replace `<EPIC-ID>`
with this bead's parent epic id" — and deliberately **not** as a dependency
edge. The epic is minted by the same `create-beads` step that mints the
task beads, so "blocked by the epic existing" would convert into an edge
pointing at a node outside the DAG.

No existing beads from the `specs/004` or `specs/007` eras need closing or
re-parenting: the only `.beads/` hits for notes are two closed, immutable
history records, and no open bead references the feature.

## Alignment fixes applied

- **(coverage, must)** `tests/e2e/visual/tab-window-chords.sh` no longer
  claimed "left alone": verified it carries the literal `workspace-notes`
  token at lines 7 and 214, that GATE A's `(?i)workspace[-_ ]?notes`
  matches it, and that `COMMON` does not exclude `tests/**`. A two-comment
  reword preserving the rationale is now owned by the `justfile`/E2E work
  item, and `## Testing Strategy`'s "zero false positives" sentence is
  corrected.
- **(coverage, must)** The `lat.md` item is now blocked by the
  client-module deletion. Verified all 16 `// @lat:` anchors (3 + 5 + 8)
  point into `client#GPUI Workspace Notes` and that `lat.md/client.md` has
  no `require-code-mention` frontmatter, so files-first is the safe
  direction. Its acceptance criteria are now diff-shaped and `lat check`
  green moved to the quality-gate item.
- **(quality, must)** Minted "Stage all code and gate-document edits and
  create the single atomic commit" as its own P0 item, blocked by the
  quality gate and the manual verification and blocking the rebuild. Its
  criteria name `pre-commit run --all-files`, the `--staged` hooks, the
  one-commit contents, and the `git revert` self-consistency check.
- **(quality, must)** Added `lat.md/architecture.md:16` ("workspace-note
  wire types") to the `lat.md` item's targets, and documented in
  `## Testing Strategy` why neither gate reports the singular form.
- **(coverage, must)** Inserted step 3b into `## Data Model`'s ordered
  list, into `## Sequencing` as its own work item, and into the
  `## Testing Strategy` Story 3 row: the stale-file startup check runs
  before the deletion, on a separate short-lived dev daemon, and may not be
  satisfied by keeping a copy past step 4.
- **(quality, should)** Dropped the protocol item's informational blocking
  edges. Verified neither `keybindings.rs` nor `titlebar.rs` references
  `scribe_common::protocol`. The protocol item now blocks only the two
  recomputes that parse `protocol.rs`.
- **(quality, should)** Split client `main.rs` surgery into the notes-only
  surface deletion and the shared-hot-spots edit-down, with all
  ordering-preservation criteria on the latter.
- **(quality, should)** Split the parity item's P3 tail into its own P3
  `specs/**` scrub item, noting it matches no `--staged` hook regex and so
  cannot gate the commit.
- **(coverage, should)** Split the final item into commit -> rebuild /
  reinstall / restart -> stale-file check -> delete, leaving the point of
  no return alone in the last item.
- **(quality, should)** Split the `lat.md` work into deletions/prose edits
  and the separate authoring of the `lat.md/protocol.md` decision record.
- **(coverage, should)** Named
  `specs/018-remove-workspace-notes/edit-list.md` as the re-verify item's
  artifact so downstream beads have a path to cite.
- **(quality, should)** Added a P3 "File the follow-up beads outside this
  epic" item; the four were named in `## Risks` but nothing created them.
- **(coverage, should)** Resolved the release-note sentence: no work item
  is created and the decision is explicitly deferred to the user at release
  time, recorded in `## Risks`.
- **(quality, should)** Made the cold-vs-warm build decision — accept the
  cold gate in this worktree, rejecting the warm primary checkout because
  of its ~64 GB `target/` and cross-checkout state on the edited files —
  and recorded it as a precondition on the quality-gate item.
- **(coverage, should)** Gave the manual-verification item a procedure
  (binary, env, screenshot evidence), added the four surviving overlay
  chords, and documented the old-server `broadcast_workspace_notes_changed`
  disconnect hazard so it is not misread as a regression.
- **(quality, should)** Restated the parity item's "blocked by the epic
  existing" as an instruction to substitute the parent epic id, in both
  `## Sequencing` and `## Target Epic`.
- **(quality, should)** Named `pre-commit run --all-files` on the
  quality-gate item alongside `just ready`.
- **(coverage, should)** Added the `open_overlay_chord`-has-four-arms
  criterion to the shared-hot-spots item as Story 2's missing verification
  path.
- **(coverage, should)** Carried the already-dead subset's third bullet
  with a plan-time correction: verified `adding_note_states`,
  `focused_inline_editor`, `affordance_hovered_workspace`, and
  `draw_affordance` are `lat.md/client.md`-only, while
  `workspace_notes_save_pending` and `PreviewLayout` are live source in
  files deleted outright. Added to the re-verify item's confirm list.
- **(quality, should)** Copied the `COMMON` / `A` / `ALLOW` gate block
  verbatim from `spec.md` into `## Testing Strategy`.
- **(coverage, should)** Marked "reconnect backoff never resets" as
  plan-time-surfaced so the plan's four follow-ups reconcile with the
  spec's three; verified at `crates/scribe-client/src/main.rs:7040`.
- **(plan-time, new)** Measured GATE A at a clean `cfcc84d` in this
  worktree: **710 lines across 24 files**, not the 709 the spec records.
  Neither review caught this; the plan now carries the measured figure.
- **(plan-time, new)** `.pre-commit-config.yaml` has **three** `--staged`
  hooks, not two — `no-new-lint-suppressions` runs `--staged` alongside the
  two count-gates. Both reviews said two; the commit item now says three.
