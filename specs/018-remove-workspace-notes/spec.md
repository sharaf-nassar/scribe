# 018 — Remove Workspace Notes

**Status:** CLARIFIED — the clarify gate is closed. Every question raised in
[Open Questions](#open-questions) and [Spec Review](#spec-review) has been
researched and answered in [Clarifications](#clarifications); the sections below
have been reconciled with those answers and are now the plannable spec. Where a
sentence below and a clarification disagree, the clarification wins.

## Problem

Scribe ships a "workspace notes" feature: server-owned, per-workspace notes
persisted as TOML, exposed through four protocol messages, and surfaced in the
GPUI client as a titlebar button, a modal (Active/Archive tabs, note list,
New-note editor, Save), and a hover preview over the workspace badge.

The user wants it removed completely — client UI, server store, protocol
messages, tests, docs, and the persisted user data — with **no legacy shims, no
dead code, no compatibility fallbacks, and no backups of the stored note
data**.

A substantial part of the feature is already dead code that no user can reach
(see [Constraints § Already-dead subset](#already-dead-subset)): the inline
note editor half, the draft-debounce machinery, and a set of identifiers that
exist only in `lat.md` prose describing the retired winit client. Removal
therefore retires never-reachable machinery in addition to live surface area,
and the "does removing this change behavior?" answer differs per component.

## Goals

Each goal is stated so it can be checked mechanically or by a single
reproducible observation.

1. **The two-stage completion gate returns zero.** The repo-wide gate defined
   in [Constraints § Completion gate](#completion-gate) — GATE A (hard-banned
   workspace-notes identifiers) and GATE B (every remaining `note`-bearing
   identifier minus the documented allowlist) — both return no lines. Today
   they return 709 lines across 24 files and 842 tokens respectively. The gate
   deliberately covers the whole repo including `lat.md/`, and deliberately
   excludes `specs/**` and `.beads/**` as historical records. The
   false-positive allowlist in
   [Constraints § False positives](#false-positives--do-not-touch) is the
   normative companion to GATE B, not a decorative aside.
2. **Quality gate clean.** `just ready` passes with the workspace's clippy
   `pedantic`/`cargo` denials and with **no new `allow`/`expect`
   suppressions** — `tools/check-no-new-lint-suppressions.sh` stays green.
3. **Both CI count-gates recompute cleanly.** `tools/check-reachability.sh`
   reports `64/64` modules, `52/57` server messages, `36/36` layout actions
   against an updated `tools/reachability-baseline.txt`, and
   `tools/check-parity-inventory.sh` exits 0 with `199 rows, 199 reachable,
   0 unwired, 0 missing (190 user-facing, 189 reachable in-client, 48 spec
   requirements carried)` and `--gate` reporting
   `GO — 190 of 190 user-facing rows reachable (100%)`. See
   [Constraints § CI count-gates](#ci-count-gates--mandatory-in-scope).
4. **Docs consistent.** `lat check` passes: every workspace-notes section is
   removed and every `[[wiki link]]` that pointed into one is resolved (either
   retargeted or deleted along with its sentence).
5. **Titlebar is visually correct.** The titlebar renders with no notes button
   and no dead gap — equalize sits adjacent to the gear. The notes button is a
   plain flex child, so removing it cannot leave a reachable hole; the only
   hard-coded pixel geometry in `titlebar.rs` is the single `on_mouse_move` hit
   band being deleted.
6. **Chord is unbound.** `ctrl+shift+m` maps to no overlay; `OVERLAY_CHORDS` has
   4 entries and the array type literal drops `5` -> `4` (it will not compile
   otherwise). The chord is left free, not reassigned and not swallowed.
7. **Persisted data deleted.** `workspace_notes.toml` and any
   `.workspace_notes.toml.<pid>.<ms>.tmp` siblings are gone from
   `~/.local/state/scribe/` and `~/.local/state/scribe-dev/` on this machine for
   this user, with no backup copy created, and deleted **after** the approved
   rebuild/reinstall/restart so no running binary can recreate them.

## Non-Goals

- **No replacement feature.** Notes are not being swapped for a different
  annotation, scratchpad, or per-workspace metadata surface.
- **Do not touch the settings-window "note row" helper.** `note_row`,
  `Role::Note`, `NOTE_MAX_CHARS`, `tailnet_note`, and `trust_status_notes` in
  `crates/scribe-client/src/settings/window.rs` are an unrelated annotation-row
  helper. Same for the other false positives listed in Constraints.
- **No `HANDOFF_VERSION` bump.** `HandoffState` never carried notes, so the
  handoff payload shape is unchanged.
- **No `REMOTE_PROTOCOL_VERSION` bump.** The constant stays `3`. See
  [Clarifications Q2](#clarifications) — bumping protects nothing on the local
  socket and actively arms a silent LAN-picker drop.
- **No staged deprecation and no no-op compatibility arms.** All four message
  variants are deleted in one atomic change. No variant is retained "for one
  release", no arm is left commented as temporarily-dead, and no removal
  follow-up task is created for protocol leftovers.
- **No migration or data-export path.** The user explicitly asked for **no
  backups** of stored note data. Nothing writes a copy, an archive, or a
  "notes were removed" export.
- **No deletion of `specs/004-workspace-notes/` or
  `specs/007-add-note-from-hover/`.** They stay as the record of why the feature
  existed and why it was cut; only stale cross-references elsewhere get scrubbed.
  This is a different act from the mandatory `specs/016-*` gate edits, which are
  fully in scope.
- **No cross-machine or multi-user data deletion.** Data removal is bounded to
  this machine and this user's state dirs. Nothing is done for other hosts,
  other accounts, or `/etc/`.
- **No macOS or Windows deletion mechanism.** macOS ships as a `.dmg` with no
  maintainer scripts and no Windows target is built, so there is nothing to hook.
  Those clauses are dropped rather than deferred.
- **No reassignment of the freed `ctrl+shift+m`.** It becomes a clean free slot,
  exactly as `ctrl+shift+n` was when the notes modal was relocated off it.
  Binding it to something else is separate future work.
- **No fix for the LAN-picker silent-drop bug** (`remote.rs:264` bare
  `continue`) **and no change to the undecodable-frame disconnect policy**
  (`ipc_server.rs` mapping decode errors to `LoopExit::Disconnected` even though
  length-prefixed framing means the stream never desyncs). Both are real latent
  bugs surfaced by this research and both get their own follow-up beads.
- **Constitution principle 4 (Performance Budgets) is explicitly marked
  INAPPLICABLE.** This is a pure removal: no new hot path, no new allocation, no
  new render work, no new IO. There is no budget to state and none to regress.
  The constitution requires the explicit mark, so this is it.

## Backlog Inputs

None — this run originates from a direct user request, not from a P4 backlog
issue.

## Target Epic

No existing epic was supplied by the user, and none could be inferred from the
backlog (there are no backlog inputs to infer from). This run will therefore
**create a new feature epic at the `create-beads` step**, with the user stories
below decomposed into task beads under it.

## User Stories

Constitution principle 3 (Explicit Risk-Based Verification) requires each story
to have an independently user-reachable verification path. Where the only
existing automated oracle is being deleted along with the feature (the
`workspace-notes.sh` E2E), that is called out. New test code is written **only**
where existing coverage must change, per principle 3.

Acceptance criteria below name **symbols, not line numbers** — see the leading
note in [Constraints](#constraints).

### Story 1 — UI affordances are gone

*As a Scribe user, I want the notes button, modal, and hover preview gone from
the titlebar and workspace badge, so the UI has no affordance for a feature
that no longer exists.*

Acceptance criteria:

- No notes button renders in the titlebar, in any window state.
- The `WorkspaceNotesHover` hit band in the root `on_mouse_move` closure is
  gone. Only the trailing band lines are removed (`let width`, `let x`, and the
  `if (width - 188.0..width - 154.0)` emit); the rest of that closure —
  including `advance_move_arm`'s early return and the `update_drag` call
  introduced by `e530da7` — is preserved verbatim. Window dragging still works.
- After the deletion, `titlebar.rs` contains **no** hard-coded pixel hit band at
  all; that closure was the only one.
- Hovering the workspace badge shows no preview overlay, for a server workspace
  or otherwise.
- Equalize sits adjacent to the gear with no gap. The notes button is a plain
  flex child inside the control row, so no unreachable dead region can survive
  its removal — the siblings simply reflow. Gear and window controls remain
  right-anchored and visually unmoved.
- Keyboard tab-order skips the removed control — `has_keyboard_focus` drops
  exactly one clause and the remaining focus chain still cycles correctly.
- Verification is a client-only launch against the already-running server; the
  server itself is not restarted for this story.

Constitution: principle 2 (Session-Safe Consistent UX) — a visible titlebar
control is disappearing from a running product.

### Story 2 — `ctrl+shift+m` opens nothing

*As a Scribe user, I want `ctrl+shift+m` to no longer open a notes modal.*

Acceptance criteria:

- `ctrl+shift+m` opens no overlay and shows no status message.
- The chord **falls through to the PTY as `0x0D`**, exactly like the eleven
  other unbound `ctrl+shift+<letter>` combos that already do so today (`a e g h
  i j l o r s y`). This is the expected outcome, not a defect: the encoder runs
  `TerminalMode::legacy()`, `translate_character_with_modifiers` never reads
  `modifiers.shift`, and `char_to_control_byte` maps `'m'` and `'M'` alike.
  `ctrl+shift+j` already emits `0x0A` and submits the line; `ctrl+shift+s`
  already sends XOFF. The chord is **not** swallowed — swallowing a legitimate
  Ctrl-M that readline and TUI apps expect would itself be the regression, and
  would be arbitrary unless `j`/`s`/`r`/`l` were swallowed too.
- `OVERLAY_CHORDS` has exactly 4 entries and the `[(&str, OverlayChord); 5]`
  type literal is updated to `4`. This is compiler-enforced.
- The four remaining overlay chords still open their overlays, and
  `open_overlay_chord` still dispatches them correctly after its notes arm is
  removed.
- No user keybinding config can break: `OverlayChord` sits outside
  `KeybindingsConfig` by design, and `translate_overlay_chord` already yields to
  any configured binding, so a user who had rebound `ctrl+shift+m` was never
  reaching the notes modal in the first place.
- `keybindings/tests.rs` iterates `OVERLAY_CHORDS` generically and needs no
  edit — it simply covers 4 rows. The keyboard-byte golden fixture has no
  `ctrl+shift+<letter>` case at all and is untouched.

Constitution: principle 2 — removing a configurable shortcut is a UX contract
change.

### Story 3 — Stored note data is deleted, with no backup

*As a Scribe user, I want my stored note data deleted with no backup left
behind.*

Acceptance criteria:

Scope is **this machine, this user**. `dpkg` shows `scribe` and `scribe-dev`
installed as systemd *user* units, `/etc/scribe*` does not exist, and no other
account holds Scribe state. macOS and Windows clauses are dropped as
unreachable, not deferred.

The mechanism is a documented manual `rm`, run **last**, in this exact order:

1. Land the client + server + protocol removal.
2. Rebuild and reinstall.
3. Restart the servers and clients so no live process retains notes code or the
   in-memory store. **The user has explicitly approved this restart for this
   step**, which is the standing rule's required exception.
4. Delete the files. After step 3 resurrection is impossible.

Acceptance criteria:

- Steps 1-3 complete before any `rm` is issued. Deleting first is a defect:
  the running server holds the entire store in memory and `persist_next` writes
  a full clone with `.truncate(true)` while `ensure_private_parent` recreates
  the directory, so one mutation on an old binary restores the whole file.
- The deletion command is exactly:
  `rm -f ~/.local/state/scribe/workspace_notes.toml
  ~/.local/state/scribe-dev/workspace_notes.toml`, plus a
  `.workspace_notes.toml.*.tmp` sweep in both directories (which currently
  matches nothing — no `.tmp` leftovers exist).
- Verification is
  `find ~/.local/state/scribe ~/.local/state/scribe-dev -name '*workspace_notes*'`
  returning empty. This replaces the previous unbounded "no copy exists
  anywhere" negative, which no command could check.
- **The state directories are NOT removed wholesale.** They hold `restore/`,
  `windows/`, `settings_state.toml`, `driver_state.toml`, and the LAN trust and
  certificate files, all in active use.
- No backup, archive, or `.bak` is written at any step.
- **Stale-file startup check.** With a `workspace_notes.toml` deliberately left
  in place, start a dev daemon on the post-removal build and confirm it neither
  reads the file, nor logs a warning about it, nor recreates it. This is the one
  cheap mechanical check available for this story and it does not touch the
  live server.

Supporting facts: the write path is mutation-only (`write_toml_atomic` <-
`persist_next` <- `apply_mutation` <- `handle_workspace_notes_mutate`, reachable
only from a `WorkspaceNotesMutate` frame); `WorkspaceNotesStore::load()` never
writes and a missing file yields a default with no write; there is no periodic
flush, no startup write, and no shutdown flush. The literal
`"workspace_notes.toml"` appears exactly once in the repo, and `env_store/gc.rs`
walks only `restore/windows` and the env-envelope root so it can neither read nor
delete it.

### Story 4 — No dead code left in any crate

*As a developer, I want no dead notes code, imports, or match arms left in any
crate.*

Acceptance criteria:

- The Goal 1 two-stage gate (GATE A and GATE B, verbatim in
  [Constraints § Completion gate](#completion-gate)) both return zero lines.
- `just ready` is clean. This is a real gate, not a formality: the workspace
  denies clippy `pedantic`/`cargo`, so **unused imports and newly
  single-variant matches FAIL CI rather than warn**. Every shared match arm
  listed under [Constraints § Shared/ordering hot spots](#sharedordering-hot-spots-edit-dont-bulk-delete)
  must be edited down rather than deleted, or the build breaks.
- No new `allow`/`expect` attribute is introduced to silence a removal
  fallout lint.
- Retained-by-design items are still present and still compile:
  `connected_window_writers`, `PaneShell::is_server_workspace`, the `toml`
  dependency in `scribe-server`.

### Story 5 — `lat.md` reflects reality

*As a developer, I want `lat.md` to reflect reality.*

Acceptance criteria:

- Notes sections are removed from `lat.md/client.md`, `lat.md/server.md`,
  `lat.md/protocol.md`, `lat.md/test.md`, and `lat.md/architecture.md` — located
  by heading text, per [Constraints § Docs](#docs-to-update).
- Surrounding prose that *mentions* notes in passing (titlebar tab order,
  overlay chord precedence, keyboard chain level 2, hit-testing rects, IME
  immutable-surface gate list, state transfer, `REMOTE_PROTOCOL_VERSION`
  paragraph, and the `### Window move region` closing prose added by `e530da7`
  that lists "workspace-notes" among controls stopping propagation on left
  press) is edited so the remaining sentences are true, not merely truncated.
- `lat.md/protocol.md` records the compatibility decision: four variants deleted
  outright, `REMOTE_PROTOCOL_VERSION` deliberately left at `3`, and why.
- `lat.md/` is inside the completion gate and gets line-by-line treatment, not a
  bulk section delete.
- `lat check` is green — no dangling `[[wiki link]]`, no `// @lat:` anchor
  pointing at a deleted section, no leading-paragraph violation introduced by
  deleting a section's first paragraph.
- All 16 `// @lat:` note anchors disappear with the client files that carry
  them; zero anchor edits are needed in `scribe-server` or `scribe-common`.

Constitution: principle 7 (Compatible Documented Operationally Safe Change) —
`lat.md` must stay synchronized with the code.

### Story 6 — A live upgrade degrades to a recoverable blip, not data loss

*As an operator upgrading Scribe, I want the removal not to silently break a
live session.*

Acceptance criteria:

- All four protocol variants (`ClientMessage::WorkspaceNotesGet`,
  `ClientMessage::WorkspaceNotesMutate`, `ServerMessage::WorkspaceNotesSnapshot`,
  `ServerMessage::WorkspaceNotesChanged`) are **deleted outright** in a single
  atomic change. No no-op arm, no deprecation comment, no named removal release,
  no follow-up removal task.
- `REMOTE_PROTOCOL_VERSION` is **unchanged at `3`**. A diff that touches that
  constant fails this story.
- The compatibility decision — deletion without a bump, and the reasoning behind
  both halves — is written into `lat.md/protocol.md` and into
  [Clarifications](#clarifications) as the durable decision record.
- The mixed-version behaviour is documented as an accepted, bounded outcome: an
  old client that emits a deleted frame to a new server hits a decode error, the
  server runs `finish_served_connection` and releases window ownership with the
  owning sessions untouched, the client's `supervise_connection` retries the
  local socket forever at 100 ms -> 2 s backoff, reconnect writes
  `Hello` + `ListSessions`, the first `SessionList` drives
  `reattach_visible_sessions` at each pane's retained grid dimensions, and the
  server replies `SessionReplay` per session so scrollback is rebuilt. PTYs and
  scrollback survive; typed input is buffered in the 1024-frame outbound queue
  and replayed; `cx.quit()` is unreachable from a connection failure. The
  user-visible worst case is a red status dot plus one status line — a
  sub-2-second blip.
- The residual risk is recorded honestly rather than minimised: the exposure
  window is roughly 1-4 s on the packaged `postinst` path, but **indefinite**
  under `just restart-server` / `restart-server-release` (which do not touch
  clients at all) and in four other `postinst` fallback branches. The mitigation
  is operational, not code — restart server and clients together.
- The Scribe server is not restarted to verify *this* story. (The one approved
  restart belongs to Story 3 step 3.)

Constitution: principle 7 — document the compatibility decision and never
disrupt the live server.

## Constraints

Full scope survey.

> **Symbol names govern. Every line number in this section is a stale
> pre-rebase snapshot, not a criterion.** The original survey was taken against
> the then-dirty primary checkout, before this worktree was rebased onto main
> `cfcc84d`; `e530da7` alone shifted everything below `lat.md/client.md:719` by
> +27, and `titlebar.rs` moved by tens of lines. Treat every `path:NNN` below as
> a hint that must be re-verified immediately before touching it, and locate
> constructs by symbol, heading, or literal text instead. No acceptance criterion
> anywhere in this spec depends on a line number.

### Delete outright

| Path | Lines | Notes |
|---|---|---|
| `crates/scribe-client/src/workspace_notes.rs` | 431 | |
| `crates/scribe-client/src/workspace_notes_modal.rs` | 907 | |
| `crates/scribe-client/src/workspace_notes_modal/tests.rs` | 227 | |
| `crates/scribe-client/src/workspace_notes_preview.rs` | 523 | |
| `crates/scribe-client/src/workspace_notes_preview/tests.rs` | 105 | |
| `crates/scribe-server/src/workspace_notes.rs` | 436 | `PersistedWorkspaceNotes`, `WorkspaceNotesStore`, atomic private-TOML writer |
| `tests/e2e/visual/workspace-notes.sh` | 553 | the only automated oracle for the UI |

`specs/004-workspace-notes/` and `specs/007-add-note-from-hover/` are **KEPT** —
see [Non-Goals](#non-goals) and [Clarifications Q-B](#clarifications). Only stale
cross-references to them elsewhere are scrubbed.

### Surgical edits

**`crates/scribe-common/src/protocol.rs`**

- `WorkspaceNoteStatus` L205-210
- `ArchiveReason` L212-217 — notes-only despite the generic name
- `WorkspaceNoteEntry` L218-230
- `WorkspaceNoteDraft` L232-238
- `WorkspaceNotesCollection` L240-250
- `WorkspaceNotesMutation` L252-259
- `ClientMessage::WorkspaceNotesGet` L368-371
- `ClientMessage::WorkspaceNotesMutate` L372-375
- `ServerMessage::WorkspaceNotesSnapshot` L728-732
- `ServerMessage::WorkspaceNotesChanged` L733-736

There is **no `ServerError` enum** — note failures ride the generic
`ServerMessage::Error`, so no error-variant surgery is needed.

**`crates/scribe-server/src/ipc_server.rs`**

- L35 import; L65 `use`
- L954-955 state field
- L5867-5868 dispatch arms
- L6101-6117 workspace-dispatch arms
- L7761-7768 `handle_workspace_notes_get`
- L7770-7784 `handle_workspace_notes_mutate`
- L7786-7793 `broadcast_workspace_notes_changed`
- L7796-7798 doc prose
- **KEEP `connected_window_writers`** — shared with `QuitRequested`, share
  rosters, and updater notices.

**Other server/client module wiring**

- `crates/scribe-server/src/main.rs` L56, L226-227, L264
- `crates/scribe-server/src/lib.rs` L29
- `crates/scribe-client/src/lib.rs` L119-121 (three `pub mod`)

**`crates/scribe-client/src/keybindings.rs`**

- `OverlayChord::WorkspaceNotes` L474-475
- chord entry L497
- `OVERLAY_CHORDS` array arity at L493 must drop `5` -> `4`

**`crates/scribe-client/src/titlebar.rs`**

- `TitlebarEvent::WorkspaceNotesHover` L61-62, `OpenWorkspaceNotes` L63-64
- `notes_focus_handle` L135 + init L160
- `render_workspace_notes_button` L681-726, call site L866, child insertion
  L934
- `has_keyboard_focus` chain — drop **one** clause
- root `on_mouse_move` hard-coded hit band `width-188..width-154` — see the
  `e530da7` carry-forward below; delete **only** the trailing band lines
- Layout consequence: removing the 34px button shifts equalize right by 34px;
  gear and window controls are right-anchored and unaffected. The button is a
  plain flex child, so no unreachable gap can be left behind.

### `e530da7` carry-forwards (post-rebase)

The uncommitted work the original survey worried about has landed as `e530da7
"fix: restore settings window interactions"`, and main advanced 15 commits to
`cfcc84d`. This worktree is rebased onto `cfcc84d`; there is no concurrent
in-flight work and no sequencing decision left to make. Two of its changes land
directly on files this removal edits.

- **`titlebar.rs` gained an imperative window-move system** — `move_arm`,
  `WINDOW_MOVE_THRESHOLD`, `advance_move_arm` — because `WindowControlArea::Drag`
  is a no-op on X11/Wayland in the pinned GPUI revision. The root
  `on_mouse_move` closure now **interleaves** `advance_move_arm`'s early return
  and the `update_drag` call with the `WorkspaceNotesHover` hit band, which sits
  at the bottom. Delete only the trailing band lines (`let width`, `let x`, and
  the `if (width - 188.0..width - 154.0)` emit) and preserve everything else in
  that closure. Deleting the closure or its head breaks window dragging with no
  compiler error.
- **`render_workspace_notes_button` gained an `.on_mouse_down` stop-propagation
  guard.** It is absorbed when the function is deleted; no separate edit.
- **`lat.md/client.md` gained a `### Window move region` section** whose closing
  prose lists "workspace-notes" among the controls that stop propagation on left
  press. This is a **new** doc edit the original plan did not have.

**`crates/scribe-client/src/ipc_bridge.rs`**

- L56 import
- `workspace_notes_get` L1242-1250
- `workspace_notes_mutate` L1333-1345
- L1256 neighbouring doc prose

**`crates/scribe-client/src/main.rs` (bulk)**

- imports L115-122; `ArchiveReason` in the `use` list L141
- `Shared::notes` L386-390
- `WorkspaceNotesPreviewSurface` L703-712
- `TerminalView` fields L875-882 + init L1094-1096
- `TitlebarEvent` arms L1316-1319
- `notes_workspace_id` L4987-5005
- `open_workspace_notes_modal` L5007-5056
- `set_workspace_notes_preview` L5058-5112
- `sync_workspace_notes` L5115-5127
- `sync_workspace_notes_modal` L5129-5146
- `sync_workspace_notes_preview` L5148-5158
- `route_workspace_notes_action` L5160-5226
- `send_workspace_notes_mutation` L5228-5236
- `handle_notes_modal_key` L5238-5256
- `build_workspace_notes_preview_overlay` L6312-6321
- `Shared` ctor L6731
- `ReaderCtx` clone L7366 + field L7842-7844
- `WORKSPACE_NOTES_ERROR_PREFIX` L8580-8585
- `on_workspace_notes_message` L8587-8623

### Shared/ordering hot spots (edit, don't bulk-delete)

These sit inside constructs that survive. Deleting the enclosing block breaks
unrelated behavior; ordering matters where noted.

- `open_overlay_chord` arm L5313
- `overlay_free` conjunct L5388-5391
- keyboard routing chain L5424-5427 — sits **between** the dialog and
  find-overlay handlers; preserve the relative order of the survivors
- `Render::render` sync pass L6384 — between `sync_find_results` and
  `sync_remote_connect`
- `notes_preview` build L6406
- render child order L6476-6477 — the `displaced` banner **must remain the last
  child**
- `server_message_variant` log table L8166-8167
- reader routing table L8397-8403
- `on_server_error` L8626-8636 — **only** the leading
  `WORKSPACE_NOTES_ERROR_PREFIX` block L8629-8635 goes; the trailing
  `set_status` is shared
- **KEEP `PaneShell::is_server_workspace`** — other caller at `main.rs:4073`
- `crates/scribe-test/src/daemon.rs` L394-395 — drop two variants from the
  `dispatch_notice_message` match arm; the arm **survives** on its other
  variants
- `justfile` — the `e2e-visual-workspace-notes` recipe plus its comment block
  (the draft's `L276-283` range was wrong; locate the recipe by name)

### Completion gate

This is the normative "done" oracle referenced by Goal 1 and Story 4. It
replaces the draft's single case-sensitive `crates/`-scoped grep, which was both
identifier-incomplete and blind to the hyphenated `workspace-notes` form used by
the `justfile` recipe, the E2E filenames, and the parity inventory. Run both
stages from the repo root; **both must return zero lines**.

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

Pre-removal these return 709 lines across 24 files and 842 tokens respectively.
Simulating the file deletions leaves only genuine workspace-notes identifiers
and zero false positives, so the gate is clean **iff** the removal is complete.

`specs/**` and `.beads/**` are excluded by design — they are historical records,
and `.beads/interactions.jsonl` is an append-only audit log that must not be
rewritten. This exclusion is *not* a licence to skip the mandatory
`specs/016-*` gate edits below, which are checked by a different mechanism.
`lat.md/` is deliberately **inside** the gate and needs line-by-line treatment.

### False positives — do not touch

This table is the definitive GATE B allowlist. Anything hit by a widened `note`
search that is not listed here is in scope for removal.

| File | Identifier / text |
|---|---|
| `crates/scribe-client/src/settings/window.rs` | `note_row`, `Role::Note` (a **gpui** type, not ours), `id(("settings-note", …))`, `NOTE_MAX_CHARS`, `tailnet_note`, `trust_status_notes` |
| `crates/scribe-client/src/ai_indicator.rs` | `note_activity` (also called from `main.rs`) |
| `crates/scribe-client/src/x11_focus.rs` | `note_active`, `note_inactive` |
| `crates/scribe-server/src/ipc_server.rs` | `ResizePacer::note_external_apply`, `note_unpaced_resize_apply` |
| `crates/scribe-server/src/attach_flow.rs` | `note_unpaced_resize_apply` |
| `crates/scribe-client/src/releases.rs` | `Options::ENABLE_FOOTNOTES` |
| `crates/scribe-client/src/remote/tests.rs` | `awaiting_approval_swaps_loading_note_until_settled` |
| `crates/scribe-client/src/remote.rs` | "loading note" prose |
| `.github/workflows/release.yml` | release-notes step |
| `tools/perf-ab-rig/run-perf-ab.sh` | `STARTUP_NOTE` |
| `dist/shell-integration/fish/.../scribe.fish`, `dist/debian/postinst` | prose only |
| `AGENTS.md` | one prose mention |
| `lat.md/settings.md` | "release notes" |
| `lat.md/server.md` | `note_unpaced_resize_apply` / `ResizePacer#note_external_apply` links |

**Trap:** `render_note_row` and `note_count` *also* exist in
`workspace_notes_modal.rs` and `workspace_notes_preview.rs` and are **not**
survivors. Only `settings/window.rs::note_row` is allowlisted. Match on file,
not on identifier alone.

Also keep the `toml` dependency in `scribe-server` — used by `lan/network.rs`,
`lan/trust.rs`, `env_store/gc.rs`.

### Already-dead subset

Removing these changes no user-visible behavior, because nothing reaches them
today:

- `DraftDebounce` / `DraftDebounceEvent` / `WORKSPACE_NOTES_DEBOUNCE`
  (modal L826-907) — referenced only by their own unit tests.
- `AddingNoteState`, `set_inline_editor`, and the `OpenEditor`/`FocusEditor`
  inline-editor half are entirely unwired in the GPUI client: `main.rs:5093`
  maps `OpenEditor` to opening the modal, and `5103` maps `FocusEditor` to
  `{}`.
- `adding_note_states`, `focused_inline_editor`, `affordance_hovered_workspace`,
  `workspace_notes_save_pending`, `PreviewLayout`, `draw_affordance` exist
  **only in `lat.md` prose** describing the retired winit client — they are not
  in `crates/` at all. Removing their prose is a docs edit, not a code edit.

### Protocol / wire compatibility facts

- `REMOTE_PROTOCOL_VERSION = 3` at `crates/scribe-common/src/protocol.rs:27`.
- Both message enums are serde **internally tagged** (`tag = "type"`) over
  named msgpack. An unknown tag is a **HARD deserialize error, not a skip**.
- `ipc_server.rs:5578-5582` turns any decode error into
  `LoopExit::Disconnected` — the whole connection drops.
- The local socket has **no version negotiation**: `Hello` carries only
  `window_id` / `clipboard_gating` / `takeover`.
- Framing is **length-prefixed** (`crates/scribe-common/src/framing.rs`), so an
  undecodable frame does **not** desync the stream. Dropping the connection is a
  policy choice, not a necessity — but changing that policy is out of scope and
  gets its own bead.
- Real exposure window: the `--upgrade` hot handoff and the Debian `postinst`
  reload **deliberately** keep an OLD client connected to a NEWLY started
  server. Roughly 1-4 s on the packaged path, **indefinite** under
  `just restart-server` / `restart-server-release` (which never touch clients)
  and in four other `postinst` fallback branches.
- The consequence is **recoverable, not destructive**: the decode-error path
  runs `finish_served_connection`, releasing window ownership with owning
  sessions untouched, so PTYs and scrollback survive; the client's
  `supervise_connection` retries the local socket forever with 100 ms -> 2 s
  backoff (`retry_local` is true whenever `SCRIBE_LAN_DIAL` /
  `SCRIBE_REMOTE_DIAL` are unset, which is the case for the running clients),
  reconnect writes `Hello` + `ListSessions`, and the first `SessionList` drives
  `reattach_visible_sessions` at each pane's retained grid dimensions with the
  server replying `SessionReplay` per session. `cx.quit()` is unreachable from a
  connection failure.
- The notes messages **are** remote-visible — routed with no `is_remote` gate,
  absent from `requires_window_control`, and `broadcast_workspace_notes_changed`
  reaches a remote controller writer. The constant is nonetheless **not bumped**;
  see [Clarifications Q2](#clarifications) for the three grounds (precedent,
  zero protection, active harm).

### Persisted-data facts

- Exactly one path, written and read only by
  `crates/scribe-server/src/workspace_notes.rs:74` —
  `current_state_dir().join("workspace_notes.toml")` via
  `AppIdentity::state_dir()` (`app.rs:157-161`), slug `scribe` or
  `scribe-dev`.
- In scope on this machine: `~/.local/state/scribe/workspace_notes.toml`
  (2005 bytes, mode 0600, 1 workspace, 0 active + 5 archived) and
  `~/.local/state/scribe-dev/workspace_notes.toml` (1657 bytes, 0600,
  3 active + 1 archived + 1 dirty draft). Content is scratch test text
  throughout. No `.tmp` leftovers exist; a filesystem-wide search found no other
  copies. macOS and Windows paths are **dropped** — macOS ships as a `.dmg` with
  no maintainer scripts and no Windows target is built.
- Crash-leftover temps `.workspace_notes.toml.<pid>.<ms>.tmp` in the same dir
  (`private_temp_path`) are swept anyway even though none currently exist.
- A leftover file is inert to the *code* — nothing else opens that filename, and
  `env_store/gc.rs` only walks `restore/windows` and the env-envelope tree — but
  "inert" is not "gone." It is user-authored free text the product will no
  longer have any UI to view, export, or delete, so removing the feature
  obligates removing the data.
- No migration, no schema change, no DB, no embedded field in any other state
  file.
- **Nothing in build, uninstall, or GC removes it.** `dist/debian/postrm` only
  clears `/etc/scribe*` on purge, and `/etc/scribe*` does not exist here.
  Deletion is therefore an explicit manual step, ordered last — see Story 3.
- **Deleting source does not disarm installed binaries.** `/usr/bin/scribe-client`
  and `/usr/bin/scribe-dev` are running now and still contain the notes UI; a
  single mutation would restore the entire file. Hence the mandatory
  land -> rebuild/reinstall -> restart -> delete ordering.

### Docs to update

All line numbers below predate the `cfcc84d` rebase; `lat.md/client.md` shifted
+27 below its line 719. Locate by heading text.

**`lat.md/client.md`**

- delete `## GPUI Workspace Notes` and all subsections, up to `## App State`
- delete `## Workspace Notes` + `### Inline Note Editor`, up to `## Input`
- surgical prose edits: titlebar tab order (three sentences), overlay chord
  precedence, input keyboard chain level 2, hit-testing rects, IME
  immutable-surface gate list
- **new since the original survey:** the closing prose of `### Window move
  region` (added by `e530da7`) lists "workspace-notes" among the controls that
  stop propagation on left press

**`lat.md/server.md`** — L208-216 (`### Workspace Notes`) and the L242 sentence
in `### State Transfer`.

**`lat.md/protocol.md`** — L59-65, L167-173, and the L211 trailing "and notes
messages" in the `REMOTE_PROTOCOL_VERSION` paragraph.

**`lat.md/test.md`** — L337-347 (`### Workspace notes on the wire`).

**`lat.md/architecture.md`** — L148 ("the workspace-notes hover preview remains
unwired").

All 16 `// @lat:` note anchors live in client files being deleted; zero in
server or common.

**Stale historical mentions** (genuinely optional, unlike the gate documents
below): `specs/016-gpui-client-rebuild/{reachability-audit.md,accessibility-audit.md}`
and `specs/006-persist-terminal-env/research.md`.

`tests/e2e/visual/tab-window-chords.sh` is **left alone**: its comments contain
no `ctrl+shift+m` reference at all — the text is historical rationale about
`ctrl+shift+N` (`new_window`) having once collided with the notes modal.
Scrubbing it would delete the test's justification, not a stale claim.

### CI count-gates — mandatory, in scope

These are **not** stale historical mentions. Two `specs/016-*` files are
machine-checked build gates run by `just ready` and by CI, and both run as
pre-commit hooks in `--staged` mode (`.pre-commit-config.yaml`, ids
`reachability-baseline` and `parity-inventory`). Goal 2 is unreachable without
editing them.

**Consequence: the change must be atomic.** Because the hooks run staged, the
code deletions and these document edits **must be staged in the same commit**.
Splitting them across commits fails the hook. This forcibly rules out any
staged/phased landing.

Both checkers run instantly with `--working-tree` and need no build — run them
after every doc edit rather than at the end. Discovering them after a cold GPUI
build is the single largest effort risk in this change.

Baselines re-verified unchanged at `cfcc84d`: reachability `67/67, 54/59,
36/36`; parity `204 rows, 204 reachable, 0 unwired, 0 missing (195 user-facing,
194 reachable in-client, 48 spec requirements carried)`. Post-edit values below
were confirmed by extracting both gates' Perl cores and re-running them in a
sandbox against a tree with the deletions applied.

**`tools/reachability-baseline.txt`** — `modules-total` 67 -> 64,
`modules-wired` 67 -> 64, `server-messages-total` 59 -> 57,
`server-messages-handled` 54 -> 52. `layout-actions-*` unchanged at 36/36.
The five `unhandled-server-message` lines are unchanged — none names a notes
variant, because both deleted `ServerMessage` variants are currently *handled*,
so handled drops by exactly 2. No module becomes newly unwired: the notes
modules reference only `crate::tab_bar` and each other, and `tab_bar` stays
wired. Post-edit: `64/64`, `52/57`, `36/36`.

**`specs/016-gpui-client-rebuild/parity-inventory.md`** — delete 5 rows
(`WorkspaceNotesGet`, `WorkspaceNotesMutate`, `WorkspaceNotesSnapshot`,
`WorkspaceNotesChanged`, `Workspace notes hover preview`); headings `(47 sent)`
-> `(45 sent)`, `(59 handled)` -> `(57 handled)`, `(29)` -> `(28)`; footers
47 -> 45, 59 -> 57, 29 -> 28; roll-up table Client messages 47 -> 45, Server
messages 57, Spec behaviour requirements 29 -> 28, **Total** 204 -> 199; prose
"195 rows … 195 are reachable (100%)" -> 190/190, "1 of those 195" -> "1 of
those 190", "194 of 195" -> "189 of 190". The Input/keybinding (54) and
Rendering/window (6) tables are **UNCHANGED** — `keybindings.rs::pub struct
Bindings` has no notes field. Post-edit the gate exits 0 with `199 rows, 199
reachable, 0 unwired, 0 missing (190 user-facing, 189 reachable in-client, 48
spec requirements carried)` and `--gate` reports
`GO — 190 of 190 user-facing rows reachable (100%)`.

**`specs/016-gpui-client-rebuild/spec.md` register id `US4-3`** — **amended, not
deleted**, because accent colours, badges, and workspace splits survive it. Use
the existing inline-annotation precedent (`US1-8` and `US3-10`, both annotated
`*(added 2026-07-27, bead …)*`) and rewrite as:

```markdown
- **US4-3** *(descoped 2026-08-01, bead <EPIC-ID>: the workspace notes modal and
  hover preview are removed from the product)* Workspace system (accent colors,
  badges, workspace splits) works as today.
```

Then delete the `Workspace notes hover preview` and `WorkspaceNotesSnapshot`
carriers from the `US4-3` coverage cell, leaving `Workspace accent colours and
badges` and `workspace_split_vertical`. `US4-3` is the **only** coverage cell
naming a notes row, so `48 spec requirements carried` stays unchanged. Also add
a dated decision paragraph at the tail of `## Requirement register` recording the
descope.

Note the "recorded decision" phrase in `tools/check-parity-inventory.sh` is a
`print STDERR` inside the NO-GO branch and is never parsed; the normative rule is
prose in `specs/016-gpui-client-rebuild/plan.md`. That passage cites a "Re-gate
criterion B1" that exists nowhere in the repo — a dangling reference worth its
own follow-up bead.

**Hard prerequisites that would `die` the reachability gate.** The edits must
preserve all of these or the gate aborts rather than reporting a count:

- `fn server_message_variant` must still exist and name every surviving
  `ServerMessage` variant.
- `fn dispatch_server_message` and `fn handle_layout_action` must still exist
  with balanced bodies.
- The `pub enum ClientMessage`, `pub enum ServerMessage`, `pub enum
  LayoutAction`, and `pub struct Bindings` declarations must keep their exact
  syntax and 4-space variant indentation.

**Non-gated stale figures, fix opportunistically:**
`parity-inventory.md` (two nearby count paragraphs), `plan.md` (two figures),
and `launch-gate-checklist.md` — "41-script visual suite" -> 40, plus the
`workspace-notes` entry in its E2E name list.

### Quality gates

- The workspace denies clippy `pedantic` and `cargo`.
- `tools/check-no-new-lint-suppressions.sh` bans new `allow`/`expect`
  attributes.
- Unused imports and newly-single-variant matches **FAIL CI rather than warn** —
  this is the dominant source of removal fallout.
- `tools/check-reachability.sh` and `tools/check-parity-inventory.sh` are
  mandatory in-scope work with recomputed expected outputs — see
  [CI count-gates](#ci-count-gates--mandatory-in-scope). They also run as
  pre-commit hooks in `--staged` mode, which forces one atomic commit.
- Run `just ready`. Note it is unverified on this baseline: the three script
  gates are green but `clippy` and `test` have not been run here, and this
  worktree's `target/` is cold. Decide up front whether to build in the warm
  primary checkout or accept a long cold first gate.
- `CLAUDE.md` requires `lat.md` updated and `lat check` green before
  completion.
- Run the two-stage [completion gate](#completion-gate); both stages must return
  zero.
- **NEVER restart the Scribe server without explicit user approval.** The user
  has granted exactly **one** approval: Story 3 step 3, the post-reinstall
  restart of servers and clients that makes the data deletion final. No other
  step may restart the server — in particular, Story 6 is verified by reading
  and documentation, not by exercising a live upgrade.

### Constitution

- **Principle 2 — Session-Safe Consistent UX.** Removing a configurable
  shortcut (`ctrl+shift+m`) and a titlebar control changes the UX contract for
  a running session. Stories 1 and 2 carry this.
- **Principle 3 — Explicit Risk-Based Verification.** Each user story needs an
  independent, user-reachable verification path. Add test code only when
  explicitly requested or when existing coverage must change — deleting
  `workspace-notes.sh` and the modal/preview unit tests is a coverage *removal*,
  not a coverage gap to backfill. It does leave Story 1 without an automated
  oracle, so Story 1's path is a **client-only launch** against the
  already-running server — permitted, since the standing rule forbids restarting
  the *server*, not launching a client. `titlebar.sh` and
  `window-chrome-bands.sh` survive but assert nothing about the notes button, so
  the gap is real. Story 2's path is the keybinding unit tests plus a manual
  chord press; Story 3's is the `find` check plus the stale-file startup check;
  Story 6's is the documented decision record, not a live upgrade.
- **Principle 4 — Performance Budgets: INAPPLICABLE.** Explicitly marked, as the
  constitution requires. This is a pure removal — no new hot path, allocation,
  render work, or IO — so there is no budget to state and none to regress.
- **Principle 7 — Compatible, Documented, Operationally Safe Change.** Document
  the compatibility decision, keep `lat.md` synchronized, never disrupt the
  live server. Story 6 carries this.

### Rollback

The code is fully revertible: `git revert` restores every deleted module, the
protocol variants, the chord, and the docs, and because
`REMOTE_PROTOCOL_VERSION` is never touched a revert cannot strand peers on a
version they already moved past. The `specs/016-*` gate numbers revert with the
same commit, so the gates stay self-consistent in both directions.

The **note data is not revertible**. Story 3 step 4 is the point of no return:
once the two `workspace_notes.toml` files are removed there is, by explicit
design, no backup, archive, or export anywhere to restore from. Everything
before step 4 is undoable; nothing after it is.

Release communication is optional but cheap: there is no `CHANGELOG` and no
README mention, but release notes are the annotated git tag body and the client
renders them in-app on the settings Releases page. Given "no backups," a user's
first signal could otherwise be data already gone — one sentence in the tag body
naming the removal and the data deletion closes that.

## Open Questions

**None remain.** All five questions raised at the draft stage were carried into
the [Spec Review](#spec-review), researched, and answered in
[Clarifications](#clarifications). They are recorded here as a resolution map so
the trail from question to decision stays readable.

| Draft question | Resolution | Recorded in |
|---|---|---|
| 1. Protocol removal vs. staged deprecation | Delete all four variants outright, one atomic change, no no-op arms | Q1 |
| 1b. Bump `REMOTE_PROTOCOL_VERSION` `3 -> 4`? | No. It stays `3` | Q2 |
| 2. Data-deletion mechanism | Documented manual `rm`, run **last**, after an approved rebuild/reinstall/restart; this machine and this user only | Q5, Q-A |
| 3. Historical `specs/` archive | Keep `specs/004` and `specs/007`; scrub only stale cross-references. Separately, the `specs/016-*` gate documents are mandatory in-scope edits | Q-B, Q3 |
| 4a. `tab-window-chords.sh` chord narrative | Leave it alone — it contains no `ctrl+shift+m` reference; the text is `ctrl+shift+N` rationale | Q6 note, Constraints |
| 4b. Freed `ctrl+shift+m` | Leave unbound. Not swallowed, not reassigned | Q6 |
| 5. Verification beyond the gates | Client-only launch is permitted and is Story 1's path; the server is restarted exactly once, for Story 3 step 3 | Q7, Constitution |

Two follow-up beads are filed **outside** this spec's scope, both surfaced by
the research rather than caused by the removal: the silent LAN-picker drop at
the bare `continue` in `remote.rs`, and the server's policy of dropping a
connection on an undecodable frame when length-prefixed framing means it could
skip the frame instead. A third, smaller one covers the dangling "Re-gate
criterion B1" reference in `specs/016-gpui-client-rebuild/plan.md`.

## Spec Review

Six independent review passes (requirements, gaps, ambiguity, feasibility,
scope, stakeholders) were run against this draft and the repository. Findings
below are ordered by confidence and blast radius; the parenthetical names the
dimensions that independently flagged each item, so multi-dimension hits are
the higher-confidence ones. Several passes verified spec claims against the
source and found some of them wrong — those corrections are folded in.

### Critical Questions (answer before planning)

**1. What single release-boundary policy governs both the protocol removal and
the data deletion?** — flagged by: ambiguity, scope, stakeholders, gaps.

Open Questions 1 and 2 are presented as independent but are the same decision.
"No legacy code anywhere" forces the data deletion to a packaging snippet or a
manual `rm`; a safe upgrade window forces at least one release of deliberately
retained notes-aware code. They cannot be answered inconsistently.

The review also found the exposure model in Open Question 1 to be materially
understated, in three ways:

- The trigger is **hover, not a chord press**. `set_workspace_notes_preview`
  calls `workspace_notes_get` from the titlebar's `on_mouse_move` band
  (`crates/scribe-client/src/main.rs:5108`, wired at `:1316` from
  `TitlebarEvent::WorkspaceNotesHover`, band at `titlebar.rs:845`). A post-
  upgrade old client severs its connection when the user moves the mouse across
  the titlebar — not when they deliberately invoke a retired feature. Note this
  also refutes `lat.md/architecture.md:148`, which claims the hover preview
  "remains unwired"; that line is stale and this spec inherited its framing.
- The window is **not brief**. `dist/debian/postinst:598-638` skips the client
  relaunch in three branches (deferred cold restart, failed Vulkan probe, failed
  server restart), each leaving an old client attached to a new server
  indefinitely.
- **Both directions break, and the blast radius is plural.** A new-client/old-
  server window also exists (`postinst:601-602`, `:666`), and decoding is
  symmetric (`crates/scribe-common/src/framing.rs:36`). Under staged
  deprecation, `broadcast_workspace_notes_changed`
  (`crates/scribe-server/src/ipc_server.rs:7786-7793`) fans
  `WorkspaceNotesChanged` to *every* connected window writer, so one old
  client's mutation hard-fails decode in every new client at once.

Also decide what the staged-deprecation option actually degrades to: if the two
`ClientMessage` variants become no-op arms while the `ServerMessage` variants
are deleted, an old client's modal stays permanently empty with no error —
indistinguishable from "you have no notes", which a user may read as silent
data loss a release early.

**2. Should `REMOTE_PROTOCOL_VERSION` be bumped at all?** — flagged by: scope,
stakeholders, feasibility.

The draft offers the `3 -> 4` bump as mitigation for the upgrade window. It is
not. The constant gates only tailnet/LAN handshakes
(`crates/scribe-server/src/ipc_server.rs:3460`, `:3954`,
`crates/scribe-client/src/remote.rs:264`), and the local Unix socket has no
version negotiation at all — so the bump buys **zero** protection for the
`--upgrade`/`postinst` exposure that motivates it.

What it does buy is a regression unrelated to notes: LAN peers on the old
version are filtered out of the connect picker by a bare `continue`
(`crates/scribe-client/src/remote.rs:264`), so the other machine silently
**disappears** with no error and no greyed-out row. `specs/015-multi-machine-
sharing/spec.md` FR-014 (line 119) explicitly forbids this — a version mismatch
must "resolve to an explicit, understandable outcome — never silent
misbehavior." The explicit tailnet dial path does have mismatch copy
(`remote.rs:911`, `:938`); the LAN picker path does not.

Mechanically the bump is trivial and safe — all ~20 consumers use the symbol,
with zero hardcoded `3` literals in code, tests, or fixtures. The question is
whether to accept a cross-machine pairing break in exchange for a guarantee it
does not provide.

**3. Confirm the two CI count-gates are mandatory in-scope work, and who owns
the recompute.** — flagged by: requirements, gaps, scope, feasibility (4/6).

The draft files `specs/016-*` under "stale historical mentions" deferred to
Open Question 3. That is wrong: two of those files are machine-checked build
gates run by `just ready` (`justfile:52-58`) and by CI
(`.github/workflows/quality.yml:50`). Goal 2 ("`just ready` passes") is
unreachable without editing them.

- `tools/reachability-baseline.txt` hard-codes `modules-total 67`,
  `modules-wired 67`, `server-messages-total 59`, `server-messages-handled 54`.
  I verified `compare_count` (`tools/check-reachability.sh:322-347`) fails on
  *any* delta, including "neutral" ones. Deleting 3 `pub mod`s and 2
  `ServerMessage` variants moves all four numbers.
- `specs/016-gpui-client-rebuild/parity-inventory.md` is parsed live against
  `crates/scribe-common/src/protocol.rs`; deleting the four variants fails with
  *"the 'Client messages' table names unknown entries."* This is not a five-row
  deletion — eight interlocking hand-maintained numbers must be recomputed:
  rows `:116,117,198,199,408`, headings `:94` (47→45), `:154` (59→57), `:367`
  (29→28), footers `:149,237,417`, the roll-up Total `:449+`, prose `:466`,
  `:467`, `:469`, and the `US4-3` coverage cell `:519`. Additionally
  `specs/016-gpui-client-rebuild/spec.md:254` is a **live register id** whose
  text names the notes modal and hover preview, and
  `tools/check-parity-inventory.sh:580` requires descoping a register id "with a
  recorded decision" — an unscoped deliverable of this spec.

Both checkers run instantly with `--working-tree` and need no build; they should
be run after every doc edit rather than at the end, since discovering them after
a cold GPUI build is the single largest effort risk in this change.

**4. Which tree is the baseline, and do symbol names or line numbers govern?**
— flagged by: requirements, ambiguity, scope, feasibility, gaps (5/6).

Every `titlebar.rs` line reference in Constraints is stale, and the cause is not
the one the draft gives. The refs match the **dirty primary checkout**, not the
clean worktree they claim to be verified against: `WorkspaceNotesHover` is at 57
(draft says 61-62), `notes_focus_handle` 125 (135), `has_keyboard_focus` 289
(326-336 — which is actually the ctrl+shift+arrow tab-reorder handler),
`render_workspace_notes_button` 640 (681-726), hit band 845 (905-911 — inside a
`#[gpui::test]` block). `lat.md/client.md` has the same drift (1056/1250/944 vs.
the draft's 1083/1277/971). `protocol.rs` is off by one in two places;
`keybindings.rs`, `main.rs`, and `ipc_server.rs` are accurate.

This matters twice over. First, Story 1 and Goal 5 encode line numbers as
literal acceptance criteria, so an implementer following them edits the wrong
construct. Second — and more urgent — the uncommitted work in the primary
checkout touches `crates/scribe-client/src/titlebar.rs` (+72 lines) and
`lat.md/client.md` (+29 lines), which are two of the exact files this removal
must edit, plus 249 uncommitted lines in `crates/scribe-client/src/
settings/window.rs`, the file this spec declares off-limits. Decide whether that
in-flight work lands before this change, after it, or is rebased into it —
concurrent is a guaranteed conflict.

Resolution needed: state that **symbol names govern and line numbers are a
re-verify-before-touch snapshot**, and strip line numbers out of all acceptance
criteria.

**5. Narrow Story 3's data-deletion scope to something achievable, and decide
whether a server restart is authorized.** — flagged by: requirements, gaps,
stakeholders, scope (4/6).

Story 3's AC cannot pass as written under any Open Question 2 option:

- **The running old server will undo it.** Persistence is write-through on every
  mutation (`crates/scribe-server/src/workspace_notes.rs:118` → `persist_next`
  `:128` → `write_toml_atomic` `:366`), so a manual `rm` today is silently
  reversed by the next note mutation on the currently-running binary. The AC's
  proof ("the only writer is deleted") is a property of the source tree, not of
  the live process. This needs an explicit ordering — which server generation
  must be gone first — and that collides with the standing rule against
  restarting the server without approval.
- **macOS is unreachable.** It is a shipped target
  (`.github/workflows/release.yml` matrix) but ships as a `.dmg` with no
  maintainer scripts of any kind — `dist/macos/` holds only `build-dmg.sh`,
  `Info.plist`, and a launchd plist. A Debian `postinst`/`postrm` snippet cannot
  reach it.
- **Windows is dead text.** No Windows target is built; drop that clause.
- **"No copy or archive exists anywhere afterwards"** is an unbounded universal
  negative with no command that could check it.

Recommend bounding the AC to named directories for a named user on this machine,
with a concrete check command, and stating the cross-machine position
explicitly. Note the privacy framing the draft has inverted: a leftover file is
not merely "inert" — it is user-authored free text the product no longer has any
UI to view, export, or delete. Removing a feature obligates removing its data,
and "inert" is not "gone."

**6. What happens to `ctrl+shift+m` once it is unbound?** — flagged by:
stakeholders; corroborated by gaps.

Story 2's AC says "no swallowed keystroke," but unbinding does not make the
chord inert — it makes it fall through to the PTY encoder. The live encoder runs
`TerminalMode::legacy()` and `translate_character_with_modifiers` ignores
`shift` for ctrl combos, so `ctrl+shift+m` becomes `char_to_control_byte('m')` =
`0x0D` = carriage return (`crates/scribe-client/src/main.rs:5395-5401`,
`:5916-5926`; `crates/scribe-client/src/input.rs:425-465`). A user with muscle
memory for the notes chord submits whatever is sitting at their shell prompt.
Decide: leave it unbound and accept CR, explicitly swallow it, or reassign.

Good news on the compatibility side, verified by two passes: `OverlayChord` is
deliberately outside `KeybindingsConfig` (`keybindings.rs:486-492`), so no user
config can reference the notes chord and none can fail to parse. Further,
`translate_overlay_chord` (`:509-518`) already yields to any configured binding,
so a user who rebound `ctrl+shift+m` was never getting the notes modal — freeing
it is a no-op for them. Open Question 4(b) needs no compatibility analysis.

**7. Define the exact command that gates "done", and which stories get a
user-reachable verification path.** — flagged by: requirements, ambiguity,
scope, feasibility (4/6).

Goal 1's grep is not a sufficient completion oracle. It is case-sensitive and
scoped to `crates/`, so it returns clean while these survive in files that are
*not* being deleted: `notes_focus_handle` (`titlebar.rs:125,149,293,646`),
`notes_workspace_id` (`main.rs:4999,5020,5075`), `.children(notes_preview)`
(`main.rs:6477`), `WORKSPACE_NOTES_ERROR_PREFIX` (`main.rs:8585,8600`), and
`handle_notes_modal_key`. It also omits the hyphenated `workspace-notes` form,
which is the only form used in the `justfile` recipe, the E2E filenames, and
`parity-inventory.md:408` — so Story 4 could pass while the `justfile` still
ships a recipe invoking a deleted script. And nothing outside `crates/` has any
mechanical gate at all.

The draft's own parenthetical concedes the false-positive allowlist does not
match the stated pattern, which makes the allowlist meaningful only under a
widened `-i note` search that is never mandated. Pick one and write the literal
command.

On verification: constitution principle 3 fails for Stories 1, 2, and 6, not
just Story 1 as the draft states. Two sub-decisions are needed — whether a
**client-only launch against the already-running server** is permitted (the
standing rule forbids restarting the *server*, and the draft never says whether
launching a *client* is blocked by it), and whether Story 6 is verified by a
`scribe-test` daemon harness, by a documented note, or not at all. Note one
correction that reduces the worry: `tests/e2e/visual/workspace-notes.sh` was not
the only titlebar oracle — `titlebar.sh` and `window-chrome-bands.sh` exist, but
neither asserts anything about the notes button, so the gap is real even if the
draft overstates it.

### Non-Blocking Observations

- **The "already dead code" framing understates what is being given up.** The
  genuinely dead subset is narrow — `DraftDebounce`, the inline-editor half, and
  a set of identifiers that exist only in `lat.md` prose and are not in
  `crates/` at all. The modal, preview, store, protocol, and persistence are all
  live. This is removing a working feature plus a modest dead tail, not
  reclaiming dead weight.

- **`specs/016-*/launch-gate-checklist.md:124`** lists `workspace-notes` in an
  E2E name list and is missing from the draft's stale-mentions enumeration,
  which an implementer will treat as exhaustive.

- **Open Question 4(a) rests on a factual error.** `tab-window-chords.sh` L7/
  L214 contains no `ctrl+shift+m` reference at all — the text is historical
  rationale about `ctrl+shift+N` (`new_window`) having once collided with the
  notes modal. Scrubbing it removes the test's justification rather than a stale
  claim. Re-ask against the real text.

- **Stakeholder weight leans toward keeping `specs/004` and `specs/007`.** They
  are the record of why the feature existed and why it was cut — the first thing
  a future maintainer wants when someone proposes re-adding notes. Deleting them
  also orphans this spec's own citations and punches holes in a contiguous
  001-018 sequence. Recommend keeping the archives and scrubbing only stale
  cross-references. Note this is a different act from editing the 016 *gate*
  documents, which is mandatory per Critical Question 3.

- **No user-facing announcement path is required, though one exists.** There is
  no `CHANGELOG` and no README mention, but release notes are the annotated git
  tag body (`.github/workflows/release.yml:161-183`) and the client renders them
  in-app on the settings Releases page. Given "no backups," a user's first signal
  could be data already gone. Recommend a line in the tag body naming the removal
  and the data deletion; cost is one sentence.

- **No rollback position is stated.** If the protocol bump lands, reverting the
  commit reverts the constant and re-breaks peers that already moved to 4; the
  deleted note data is unrecoverable by design. State what is and is not
  revertible, and at which step the change becomes irreversible.

- **Constitution principle 4 (Performance Budgets) is never addressed.** The
  principle requires stating measurable goals *or* explicitly marking them
  inapplicable. For a pure removal "inapplicable" is almost certainly right, but
  the explicit mark is currently absent.

- **The two edits the draft calls highest-risk are the low-risk ones.** The
  reader routing table (`main.rs:8397-8403`) ends in a catch-all drop counter and
  the variants are being deleted, so any mistake is a compile error; render
  ordering (`:6476-6477`) is guarded by an explanatory comment with
  `.children(displaced)` already last. The genuinely compiler-invisible hazards
  are `crates/scribe-test/src/daemon.rs:394-395` and `main.rs:8166-8167` —
  grouped or-patterns whose enclosing arm *survives*, so deleting the arm rather
  than the two `|` lines silently reroutes `PromptMark`/`GitBranch`/`Error`.

- **`ArchiveReason` notes-only claim CONFIRMED** by two passes — all 15
  references are notes code. Same for the `HANDOFF_VERSION` non-goal
  (`HandoffState` at `handoff.rs:128` carries no notes field) and the 16
  `// @lat:` anchors, all in deleted client files. Still worth an explicit
  re-verify step before deleting a generically-named public type from
  `scribe-common`.

- **Clippy casualty list is short and compiler-caught:** the three `use` blocks
  at `main.rs:115-121`, `ArchiveReason` at `:141`, `Shared::notes` `:390`,
  `ReaderCtx::notes` `:7844` + clone `:7366`, `WorkspaceNotesPreviewSurface`
  `:705`, `WORKSPACE_NOTES_ERROR_PREFIX` `:8585`. Verified **not** casualties:
  `PaneShell::is_server_workspace`, `active_workspace`, `focused_workspace_id`,
  `window_shares`, and the `toml` dep in `scribe-server`.

- **Verified-clean surfaces needing no work:** `README.md`, `AGENTS.md`,
  `CLAUDE.md`, `dist/**`, the command palette, client config, `restore_state.rs`,
  `window_state.rs`, `update.rs`, and `keybindings/tests.rs` (it iterates
  `OVERLAY_CHORDS` generically). No metrics or telemetry surface exists. No open
  bead references notes — the only `.beads/` hits are two closed, immutable
  history records.

- **`just ready` is unverified on this baseline.** The three script gates
  (`lint-suppressions`, `reachability`, `parity-inventory`) are green, but
  `clippy` and `test` were not run — the worktree `target/` is cold and the
  primary is 64 GB. Decide whether to build in the warm primary checkout or
  accept a long cold first gate.

- **The stale-data-file startup case has no AC.** Starting a dev daemon with
  `workspace_notes.toml` present and confirming no read, no warning, and no
  recreation is the one cheap mechanical check available for Story 3 that does
  not require touching the live server.

- **Two smaller allowlist gaps:** `lat.md/server.md:125` links
  `note_unpaced_resize_apply` and `ResizePacer#note_external_apply`, and
  `lat.md/settings.md:112,156-160` uses "release notes" throughout — both are
  hit by a widened `note` grep and neither is in the false-positive list.

- **The `justfile` recipe is at L282**, not the L276-283 range the draft cites.

- **Backlog Inputs "None" is consistent** and needs nothing further. One small
  omission: the draft does not say whether any existing beads from the
  `specs/004` or `specs/007` eras should be closed or re-parented.

## Clarifications

Every critical question above has been researched against the repository and
answered. These are the decisions the plan is built from; where the body of this
spec and an answer below ever disagree, the answer below wins. Dated 2026-08-01,
against main `cfcc84d`.

**Q1: What single release-boundary policy governs both the protocol removal and
the data deletion — outright deletion, or a staged deprecation that keeps the
two `ClientMessage` variants as no-op arms for one release?**

A: **Delete outright, as a single atomic change.** No staged deprecation, no
no-op arms, no named removal release, no follow-up removal task.

The deciding evidence is that the client recovers cleanly from a
decode-disconnect, so the feared failure mode is a blip rather than data loss.
`supervise_connection` retries the local socket forever with 100 ms -> 2 s
backoff, and `retry_local` is true whenever `SCRIBE_LAN_DIAL` /
`SCRIBE_REMOTE_DIAL` are unset — confirmed true for the running clients.
Reconnect writes `Hello` + `ListSessions`; the first `SessionList` triggers
`reattach_visible_sessions`, which re-attaches with each pane's retained grid
dimensions, and the server replies `SessionReplay` per session so scrollback is
rebuilt. Server-side, the decode-error path runs `finish_served_connection`,
which releases window ownership with the owning sessions untouched — PTYs and
scrollback survive. `cx.quit()` is unreachable from a connection failure. Typed
input is buffered in the 1024-frame outbound queue and replayed. The
user-visible worst case is a red status dot, one status line reading "server
connection lost; retrying in 100 ms", then recovery: a sub-2-second blip.

Precedent is unanimous. Four prior protocol-variant removals — `528a932`
(Driver variants), `2bd35bb` (`ScrollRequest`/`ScrolledSnapshot`), `a716efb`
(AutomationAction variants), `cec34e0` (`PreflightError` reshape) — all deleted
outright with zero deprecation window, and `git log -S 'deprecated' --
protocol.rs` returns no commits at all.

Staged deprecation was rejected as **actively worse**, not merely unnecessary:
no-op `ClientMessage` arms with the `ServerMessage` variants deleted leave an
old client's modal permanently empty with no error, which is indistinguishable
from silent data loss.

Residual risk, recorded honestly: the exposure window is roughly 1-4 s on the
packaged `postinst` path, but **indefinite** under `just restart-server` /
`just restart-server-release` — which do not touch clients at all — and in four
other `postinst` fallback branches. The mitigation is operational, not code:
restart the server and the clients together.

**Q2: Should `REMOTE_PROTOCOL_VERSION` be bumped `3 -> 4`?**

A: **No. It stays `3`.**

The notes messages genuinely *are* remote-visible — they are routed with no
`is_remote` gate, they are absent from `requires_window_control`, and
`broadcast_workspace_notes_changed` reaches a remote controller writer — so the
constant's doc comment ("bump on ANY change to remote-visible message
semantics") appears on its face to apply. The bump is still wrong, on three
grounds:

1. **Precedent contradicts the doc comment.** The constant has changed exactly
   once in mainline history (`fd04540`, landing directly at `3`). The "1 -> 2 ->
   3" narration inside the doc comment is prose written in that one squashed
   commit, not observed history. Three later commits changed remote-visible
   semantics without bumping: `fbab056` added the remote-reachable
   `ClientMessage::SearchClosed`, `ba626d4` added a field to
   `ServerMessage::SessionExited`, and `cec34e0` was a wire-breaking
   `PreflightError::Unknown` reshape.
2. **It protects nothing.** The only real exposure here is the local Unix
   socket, which has no version negotiation whatsoever — `Hello` carries only
   `window_id`, `clipboard_gating`, and `takeover`.
3. **It causes harm.** Bumping activates a currently-dormant bare `continue` in
   `crates/scribe-client/src/remote.rs` that filters mismatched LAN peers out of
   the connect picker with no row, no error, and no explanation — precisely what
   `specs/015-multi-machine-sharing/spec.md` FR-014 forbids ("A version mismatch
   … MUST resolve to an explicit, understandable outcome — never silent
   misbehavior"). No released build speaks any version but `3`, so that path is
   unreachable today; the bump is exactly what would make it reachable.

Two latent bugs surfaced by this analysis are filed as follow-up beads,
independent of this work: the silent LAN-picker drop above, and the server's
choice to drop a connection on an undecodable frame when framing is
length-prefixed and the stream therefore never desyncs — skipping the frame is
possible and the disconnect is a policy decision.

**Q3: Are the two CI count-gates mandatory in-scope work, and who owns the
recompute?**

A: **Both are mandatory in-scope work, and this spec owns the recompute.** This
overrides the draft's placement of `specs/016-*` under "stale historical
mentions."

Verified by sandbox simulation — both gates' Perl cores were extracted and
re-run against a tree with the deletions applied. Post-edit, reachability
recounts to `64/64, 52/57, 36/36`, and the parity gate exits 0 with `199 rows,
199 reachable, 0 unwired, 0 missing (190 user-facing, 189 reachable in-client,
48 spec requirements carried)`, with `--gate` reporting `GO — 190 of 190
user-facing rows reachable (100%)`. Baselines were re-verified unchanged at
`cfcc84d`: reachability `67/67, 54/59, 36/36`; parity `204 rows, 204 reachable,
0 unwired, 0 missing (195 user-facing, 194 reachable in-client, 48 spec
requirements carried)`.

The full edit list, the `US4-3` amendment (amended, not deleted — accent
colours, badges, and workspace splits survive it), the gate's hard
prerequisites, and the non-gated stale figures are enumerated in
[Constraints § CI count-gates](#ci-count-gates--mandatory-in-scope).

Two consequences worth restating here. First, both gates run as pre-commit hooks
in `--staged` mode, so the code deletions and these document edits **must be
staged in the same commit** — the change is forcibly atomic. Second, the
"recorded decision" phrase in `tools/check-parity-inventory.sh` is a `print
STDERR` in the NO-GO branch and is never parsed; the normative rule is prose in
`specs/016-gpui-client-rebuild/plan.md`, and that passage cites a "Re-gate
criterion B1" that exists nowhere in the repo — a dangling reference worth its
own bead.

**Q4: Which tree is the baseline, do symbol names or line numbers govern, and
how does the in-flight uncommitted work sequence against this change?**

A: **The premise is obsolete — there is no uncommitted work left to sequence.**
It landed as `e530da7 "fix: restore settings window interactions"`, main
advanced 15 commits to `cfcc84d`, and this worktree has been rebased onto
`cfcc84d` with `specs/018-remove-workspace-notes/` surviving as the only
untracked path.

Two carry-forwards from `e530da7` change the edit plan:

- `titlebar.rs` gained an imperative window-move system (`move_arm`,
  `WINDOW_MOVE_THRESHOLD`, `advance_move_arm`) because `WindowControlArea::Drag`
  is a no-op on X11/Wayland in the pinned GPUI revision. The root
  `on_mouse_move` closure now interleaves `advance_move_arm`'s early return and
  the `update_drag` call with the `WorkspaceNotesHover` hit band, which sits at
  the bottom. Delete **only** the trailing band lines (`let width`, `let x`, and
  the `if (width - 188.0..width - 154.0)` emit) and preserve the rest — removing
  more breaks window dragging with no compiler error. Separately,
  `render_workspace_notes_button` gained an `.on_mouse_down` stop-propagation
  guard, which is absorbed when the function is deleted.
- `lat.md/client.md` gained a `### Window move region` section whose closing
  prose lists "workspace-notes" among controls that stop propagation on left
  press. This is a **new** doc edit the original plan did not have.

**Critically: every line number in the Constraints section is stale.** They were
surveyed against the then-dirty primary checkout; everything below
`lat.md/client.md:719` shifted by +27 and `titlebar.rs` moved by tens of lines.
**Symbol names govern, and every line number is a re-verify-before-touch
snapshot.** Line numbers have been removed from all acceptance criteria
accordingly.

**Q5: What is the achievable scope and mechanism for deleting the stored note
data, and is a server restart authorized?**

A: **Delete the data LAST, after rebuild/reinstall/restart. The user has
approved restarting the server and clients for this.** Scope is this machine and
this user only.

The actual data is two files: `~/.local/state/scribe/workspace_notes.toml`
(2005 bytes, mode 0600, mtime 2026-05-25, 1 workspace, 0 active + 5 archived
notes) and `~/.local/state/scribe-dev/workspace_notes.toml` (1657 bytes, 0600,
mtime 2026-05-20, 1 workspace, 3 active + 1 archived + 1 dirty draft). All
content is scratch test text. No `.tmp` leftovers exist, and a filesystem-wide
search found no other copies. This is a single-user machine: `dpkg` shows
`scribe` and `scribe-dev` installed, both systemd **user** units, `/etc/scribe*`
does not exist, and no other account has state.

The write path is mutation-only — `write_toml_atomic` <- `persist_next` <-
`apply_mutation` <- `handle_workspace_notes_mutate`, reachable only from a
`WorkspaceNotesMutate` frame. `WorkspaceNotesStore::load()` never writes, and a
missing file yields a default with no write. There is no periodic flush, no
startup write, and no shutdown flush. The literal `"workspace_notes.toml"`
appears exactly once in the repo, and `env_store/gc.rs` walks only
`restore/windows` and the env-envelope root, so it can neither read nor delete
it.

The hazard is that **deleting source does not disarm installed binaries**.
`/usr/bin/scribe-client` and `/usr/bin/scribe-dev` are running now and still
contain the notes UI; one mutation would restore the *entire* file, because the
server holds the whole store in memory and `persist_next` writes a full clone
with `.truncate(true)` while `ensure_private_parent` recreates the directory.

Approved sequence: (1) land the client + server removal; (2) rebuild and
reinstall; (3) restart the servers and clients so no process retains notes code
or the in-memory store — **the user has approved this restart**; (4) delete the
files. After step 3, resurrection is impossible.

Commands: `rm -f ~/.local/state/scribe/workspace_notes.toml
~/.local/state/scribe-dev/workspace_notes.toml`, plus a
`.workspace_notes.toml.*.tmp` sweep in both directories (currently matching
nothing). Verify with
`find ~/.local/state/scribe ~/.local/state/scribe-dev -name '*workspace_notes*'`
returning empty. **Do not remove the state directories wholesale** — they hold
`restore/`, `windows/`, `settings_state.toml`, `driver_state.toml`, and the LAN
trust and certificate files, all in use.

The macOS and Windows clauses are **dropped** from Story 3: macOS ships as a
`.dmg` with no maintainer scripts and no Windows target is built. The unbounded
"no copy exists anywhere" negative is replaced by the concrete `find` check
above.

**Q6: What happens to `ctrl+shift+m` once it is unbound — leave it, swallow it,
or reassign it?**

A: **Leave it unbound. It is not a hazard. Do not swallow it and do not reassign
it.**

The review's factual claim is correct: `TerminalMode::legacy()` is hard-coded,
`translate_character_with_modifiers` never reads `modifiers.shift`, and
`char_to_control_byte` maps both `'m'` and `'M'` to `0x0D`. But this is ordinary
behaviour, not a regression. Eleven other `ctrl+shift+<letter>` combos already
fall through the identical path today (`a e g h i j l o r s y`).
`ctrl+shift+j` already produces `0x0A` and submits the line — the exact outcome
flagged — and `ctrl+shift+s` already sends XOFF and freezes terminal output,
which is arguably worse.

Swallowing `m` would itself be the regression: Ctrl-M is a legitimate keystroke
that readline and TUI applications expect, and swallowing it would be arbitrary
unless `j`, `s`, `r`, and `l` were swallowed too. Reassigning is orthogonal
future work. `ctrl+shift+m` simply becomes a clean free slot, exactly as
`ctrl+shift+n` was when the notes modal was relocated off it.

No user configuration can be affected: `OverlayChord` is deliberately outside
`KeybindingsConfig`, and `translate_overlay_chord` already yields to any
configured binding, so a user who had rebound `ctrl+shift+m` was never reaching
the notes modal. `keybindings/tests.rs` iterates `OVERLAY_CHORDS` generically
and will simply cover 4 rows, and `tests/fixtures/gpui-client/keyboard-byte-
golden.json` has no `ctrl+shift+<letter>` case at all.

One mechanical note: the `keybindings.rs` array type is
`[(&str, OverlayChord); 5]`, and the length literal **must** drop to `4` or the
crate will not compile.

**Q7: What is the exact command that gates "done"?**

A: **A two-stage `rg` gate, recorded verbatim in
[Constraints § Completion gate](#completion-gate); it replaces Goal 1's original
grep.** The draft's grep was case-sensitive, scoped to `crates/`, and
identifier-incomplete — it would return clean while `notes_focus_handle`,
`notes_workspace_id`, `WORKSPACE_NOTES_ERROR_PREFIX`, `handle_notes_modal_key`,
and the entire hyphenated `workspace-notes` form (the `justfile` recipe, the E2E
filenames, the parity inventory) all survived.

GATE A is a hard ban on workspace-notes identifiers in every form. GATE B sweeps
every remaining `note`-bearing identifier and subtracts a definitive
false-positive allowlist, which is recorded as a table in
[Constraints § False positives](#false-positives--do-not-touch). Both must
return zero. Pre-removal they return 709 lines across 24 files and 842 tokens
respectively; simulating the file deletions leaves only genuine workspace-notes
identifiers and zero false positives, so the gate is clean **iff** the removal
is complete.

`specs/**` and `.beads/**` are excluded by design — they are historical records,
and `.beads/interactions.jsonl` is an append-only audit log that must not be
rewritten. `lat.md/` is deliberately **inside** the gate and needs line-by-line
treatment. One trap to call out explicitly: `render_note_row` and `note_count`
also exist in `workspace_notes_modal.rs` and `workspace_notes_preview.rs` and
are **not** survivors — only `settings/window.rs::note_row` is allowlisted, so
the allowlist must be matched per file, not per identifier.

**Q-A (user decision): Is restarting the Scribe server and clients authorized?**

A: **Yes — approved, for step 3 of the Q5 deletion sequence only.** This is the
one exception to the standing rule in `CLAUDE.md`. No other step in this change
may restart the server; in particular Story 6 is verified by documentation, not
by exercising a live upgrade.

**Q-B (user decision): Delete `specs/004-workspace-notes/` and
`specs/007-add-note-from-hover/`?**

A: **Keep both.** They record why the feature existed and why it was cut, which
is the first thing a future maintainer wants if someone proposes re-adding
notes. Deleting them would also orphan this spec's own citations and punch holes
in an otherwise contiguous 001-018 sequence. Only stale cross-references to them
get scrubbed. This is a separate act from the mandatory `specs/016-*` gate edits
in Q3, which are fully in scope.
