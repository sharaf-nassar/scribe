# Phase 1 Data Model — OSC 8 Explicit Hyperlinks

**Branch**: `009-osc8-hyperlinks`
**Date**: 2026-05-21
**Scope**: Client-side only (per `research.md` decision 7).

Following the research conclusions, this feature touches three logical
data entities. The first two live in upstream `alacritty_terminal` and are
*used*, not re-implemented. The third is the small piece of Scribe-owned
state needed to surface the tooltip and route the disallowed-scheme path.

## E1 — Hyperlink (upstream, used as-is)

**Owner**: `alacritty_terminal::term::cell::Hyperlink`
**Lifetime**: Lives inside `Arc<CellExtra>` on each cell whose OSC 8 span
contains it. Reference-counted; freed when the last referencing cell is
trimmed.

| Field | Type | Notes |
|---|---|---|
| `uri` | `String` | The full URI from the OSC 8 sequence. Length cap honoured upstream (no Scribe-side check needed for FR-010 v1; see Outstanding Risks in research.md if upstream cap differs from the spec target). |
| `id` | `Option<String>` | The `id=` parameter, if any. Used for span reconnection per FR-005 (scoped per the *open/close run* by upstream, matching the Q3 clarify outcome). |

**Access**: Cells expose this via `Cell::hyperlink() -> Option<&Hyperlink>`
(upstream public API).

**Scribe-side implication**: Scribe holds no Hyperlink storage of its own.
The conceptual "per-pane URI table" from spec Key Entities is satisfied by
the existing `Arc<CellExtra>` sharing that alacritty_terminal already
performs across the cells of a single span.

## E2 — UrlSpan (extended)

**Owner**: `crates/scribe-client/src/url_detect.rs#UrlSpan`
**Lifetime**: Per-pane span cache, rebuilt when `PaneUrlCache::dirty`.

Currently `UrlSpan { start_row, start_col, end_row, end_col, url: String,
kind: SpanKind }` where `SpanKind ∈ { Url, FilePath }` (per the existing
heuristic detector).

**Change required**:

| Change | Description |
|---|---|
| Extend `SpanKind` with `Osc8Hyperlink` | A new variant marking the span as originating from OSC 8 (not heuristic), so the hit-test prefers it (FR-004) and the context menu can show the new "Copy hyperlink address" item (FR-007). |
| Extend `scan_visible_urls` | Add a second pass that walks the visible grid via the existing display iterator, reads `cell.hyperlink()`, and emits an `UrlSpan { kind: Osc8Hyperlink, url: <cell.hyperlink().uri()>, … }` for each contiguous run of cells sharing the same hyperlink Arc. The new pass runs **before** the heuristic pass; the heuristic pass MUST NOT add a span over cells already covered by an OSC 8 span (FR-004 precedence). |
| `url_at(row, col)` resolution | When multiple spans overlap, prefer `Osc8Hyperlink` first, then `Url`, then `FilePath`. Same shape as today (linear scan over cache) — no algorithmic change. |

**Invariants**:
- Two contiguous cells with the same `Arc<CellExtra>` (i.e., same
  hyperlink) merge into one `UrlSpan` with `kind: Osc8Hyperlink`.
- Two non-adjacent cells sharing the same `id` (per FR-005) are
  represented as TWO `UrlSpan` entries pointing at the same URI; both
  activate to the same destination. (We don't need to physically merge
  them in the cache; the precedence rules in `url_at` and the activation
  path are unaffected.)
- Cache rebuilds dirty whenever the grid mutates (existing behavior; the
  OSC 8 pass piggybacks on the same `mark_dirty` triggers).

## E3 — HoverState (new, Scribe-owned)

**Owner**: `crates/scribe-client/src/main.rs#App` (new field) — analogous
to `affordance_hovered_workspace` and `hovered_tab_close`.

**Purpose**: Track the current cell-hover candidate for OSC 8 tooltip
display (FR-006). One-per-window state because at most one cell can be
hovered at a time.

| Field | Type | Notes |
|---|---|---|
| `hover_cell` | `Option<(PaneId, row: i32, col: usize)>` | The cell the cursor currently rests on. `None` outside terminal panes. |
| `hover_started_at` | `Option<Instant>` | When the cursor first landed on `hover_cell`. Reset on every move. |
| `hover_tooltip_visible` | `bool` | Whether the dwell threshold has been crossed and the tooltip is currently being rendered. |
| `hover_tooltip_uri` | `Option<String>` | The full URI string fetched at dwell-threshold time. Cached so we don't re-read `cell.hyperlink()` per frame. |

**State transitions**:

```
        cursor moves
         to new cell
   ─────────────────────►   hover_cell = Some(…)
                            hover_started_at = Some(now)
                            hover_tooltip_visible = false
                            hover_tooltip_uri = None

   elapsed > 300 ms
   and cell has OSC 8 URI
   ─────────────────────►   hover_tooltip_visible = true
                            hover_tooltip_uri = Some(uri)

        cursor leaves
        the cell
   ─────────────────────►   hover_cell = None
                            hover_tooltip_visible = false
                            hover_tooltip_uri = None
```

**Tooltip render**: On each frame where `hover_tooltip_visible == true`,
compute the anchor Rect from the hovered cell's pixel coordinates (using
the existing `cell_to_pixel` helper) and call
`tooltip::render_tooltip(uri_for_display, anchor, Position::Below)`. URI
truncation for display: cap at the pane's width in monospace columns; full
URI is preserved in `hover_tooltip_uri` for activation.

**Dwell timing**: 300 ms is the default, mirroring common GUI hover
tooltips. Confirmed during US2 quickstart (see `quickstart.md`); tuning is
a quickstart-time observation, not a config key.

## E4 — DisallowedSchemeDialog (new, Scribe-owned)

**Owner**: `crates/scribe-client/src/disallowed_scheme_dialog.rs`

**Pattern**: Models `update_dialog.rs` and `close_dialog.rs` (per
`research.md` decision 5). One stateful struct, two buttons, a
`build_instances` render method, and a routing seam in `main.rs` that
chooses between direct activation (allowed scheme) and dialog display
(disallowed scheme).

| Field | Type | Notes |
|---|---|---|
| `pending_uri` | `String` | The full URI awaiting confirmation. |
| `scheme` | `String` | Extracted scheme name, used in the warning text. |
| `focused_button` | `enum { Cancel, OpenAnyway }` | Defaults to `Cancel` (FR-015). |
| `hovered_button` | `Option<…>` | Standard mouse-hover tracking matching the existing dialog pattern. |

**Lifecycle**:

```
   user activates OSC 8 cell
         (Ctrl+click or context-menu OpenUrl)
   ─────────────────────────────────►
         scheme on allowlist?
                 │
        ┌────────┴────────┐
       yes               no
        │                 │
   open via existing      DisallowedSchemeDialog
   open_url path          shown; focus = Cancel
                                  │
                          ┌───────┴───────┐
                       Cancel          Open Anyway
                          │                │
                     dismiss,         dismiss,
                     no open          open via existing
                                      open_url path
```

**Action wiring**: A new `KeyAction::CancelDisallowedSchemeDialog`-shaped
internal action isn't required — Esc on an open dialog dismisses, matching
existing dialog conventions. Tab cycles focus; Enter activates focused
button. Mouse click on a button activates that button.

**Decoupling from scheme allowlist**: The allowlist itself lives in
`crates/scribe-client/src/url_detect.rs#PREFIXES`. The
DisallowedSchemeDialog reads the URI's scheme via lightweight parsing
(everything up to the first `:`) and surfaces it verbatim in the warning
text. No allowlist *expansion* happens via this dialog — the user opens a
single URI ad-hoc.

## E5 — ContextMenuAction (extended)

**Owner**: `crates/scribe-client/src/context_menu.rs#ContextMenuAction`

| Change | Description |
|---|---|
| Add `CopyHyperlinkAddress(String)` variant | Payload is the verbatim OSC 8 URI from the click target. |
| Extend `ContextMenuRequest` | Add `osc8_uri: Option<String>` alongside the existing `url`/`file_path` fields. When `osc8_uri` is `Some`, the menu builder appends a "Copy hyperlink address" item *in addition to* the existing "Open URL" item, AND the "Open URL" item carries the OSC 8 URI (not the heuristic-detected URL) per FR-003. |

**Behavior contract** (FR-006, FR-007):
- When the right-click target carries an OSC 8 URI: menu shows
  `Copy / Paste / Select All / Open URL (osc8_uri) / Open File / Copy
  hyperlink address`.
- When the right-click target carries only a heuristic URL (today's
  behavior): menu shows the existing
  `Copy / Paste / Select All / Open URL / Open File` — no "Copy hyperlink
  address" item.

## E6 — Pane (untouched on the cell-storage axis)

`crates/scribe-client/src/pane.rs#Pane` does **not** gain a new field for
OSC 8 storage. Hyperlinks live on cells inside the `Term` it already owns.
The HoverState (E3) lives on `App`, not `Pane`, because hover is
window-scoped.

## Cross-references

- Upstream cells:
  `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/alacritty_terminal-0.26.0-rc1/src/term/cell.rs:128,202,219`
- `url_detect.rs` cache + scan:
  `crates/scribe-client/src/url_detect.rs:40-79,133-236`
- Tooltip renderer:
  `crates/scribe-client/src/tooltip.rs:43-98`
- Existing dialog patterns to model from:
  `crates/scribe-client/src/close_dialog.rs:75-209`,
  `crates/scribe-client/src/update_dialog.rs:75-224`
- Context menu integration:
  `crates/scribe-client/src/context_menu.rs:14-138`
- PTY output passthrough (no change):
  `crates/scribe-common/src/protocol.rs#ServerMessage::PtyOutput`
