# Contracts

**Date**: 2026-05-20
**Spec**: [../spec.md](../spec.md) | **Plan**: [../plan.md](../plan.md)

## Status: no new external contracts

This feature **does not introduce any new protocol message, RPC variant, file format, configuration field, or external interface**. It is a purely additive client-side UI change that flows through Scribe's existing workspace-notes protocol.

## Existing contracts exercised

The feature reads and writes the following protocol variants, all of which already exist as of feature `004-workspace-notes`:

### Client → Server

| Variant | Source | Use in this feature |
|---|---|---|
| `WorkspaceNotesMutation::SaveDraft { workspace_id, text }` | `crates/scribe-common/src/protocol.rs#WorkspaceNotesMutation` | Debounced draft writes from the inline editor (FR-002, FR-020). Bandwidth bounded by `WORKSPACE_NOTES_DEBOUNCE` (PR-002). |
| `WorkspaceNotesMutation::CreateActiveNote { workspace_id, text }` | (same) | Commit on Enter (FR-006). On server ack the active list grows by one and the saved draft is cleared per the existing modal semantics. |
| `WorkspaceNotesMutation::ArchiveNote { workspace_id, note_id, reason: Done }` | (same) | Continues to be written by clicks on the read-only rows in the preview (FR-011). Behavior unchanged from `004-workspace-notes`. |
| `WorkspaceNotesGet { workspace_ids }` | (same) | No new use site — the existing `request_workspace_notes_snapshot` path covers any reconnect / focus-change hydration. |

### Server → Client

| Variant | Source | Use in this feature |
|---|---|---|
| `WorkspaceNotesChanged` | `crates/scribe-common/src/protocol.rs#ServerMessage` | Drives the preview's read-only list update after the inline editor commits (FR-006). Late snapshots that carry a saved draft do NOT overwrite `AddingNoteState::draft_text` while it is dirty (FR-015 + FR-020 pristine-draft policy). |
| `WorkspaceNotesSnapshot` | (same) | Initial hydration of `WorkspaceNotesStore`. Same draft-pristine rule applies. |
| `Error` | (same) | Surfaced into `AddingNoteState::last_server_error` on `CreateActiveNote` rejection (FR-012). The editor retains its `draft_text` so the user can retry. |

### Configuration

| File / Field | Use in this feature |
|---|---|
| `KeybindingsConfig` (`crates/scribe-common/src/config.rs`) | **Not touched.** The notes-editor keymap (Enter / Ctrl+Enter / Escape / Space) is hardcoded in `handle_workspace_notes_keyboard` per Assumption #9 and Q4 resolution. No new entries are added to the config or the Settings keybindings page. |

## Why no new contracts

- **Shared draft buffer (FR-020)** is a client-side framing, not a protocol concern — the server already stores exactly one draft per workspace; whether the client writes that draft from the modal or from the inline editor is invisible to the server.
- **Per-workspace inline-editor state (FR-021)** is transient client UI state with no durability requirement — preserving the state across hover gaps requires no protocol because the state lives in the client's `App` struct.
- **3/4-pane growth cap + 3-input scroll model (FR-019, FR-022)** are render-layer decisions that don't touch the wire.
- **Modal keymap flip (FR-017) and spacebar fix (FR-018)** are pure client keyboard-handling changes that don't affect mutations sent to the server.

## Migration / compatibility

| Surface | Compatibility |
|---|---|
| Server file (`workspace_notes.toml`) | **Unchanged.** Format and `owner = "server"` requirement preserved. |
| Wire protocol | **Unchanged.** Reuses existing variants. |
| Client config (`scribe.toml` / `KeybindingsConfig`) | **Unchanged.** No new fields. |
| User-facing modal keymap | **Breaking UX change** (deliberately) — Enter and Ctrl+Enter swap meanings. Documented in spec Assumption #8 with no legacy toggle. |
| `lat.md/protocol.md` | **No change** — protocol section unchanged. |
| `lat.md/server.md` | **No change** — server section unchanged. |
| `lat.md/client.md` | **Update required** — Workspace Notes section reflects the affordance, per-workspace editor state, scroll model, and keymap flip. |
