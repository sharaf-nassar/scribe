# Internal Contract — OSC 8 Pipeline

**Branch**: `009-osc8-hyperlinks`
**Scope**: This feature exposes **no new external IPC, protocol, config,
persistence, or webview surface** (per `research.md` decision 7). All
work is client-side, riding the existing `ServerMessage::PtyOutput` raw-
byte path. This document records the *internal* contracts between
client-side modules so the implementation is verifiable without an
external interface.

## C1 — PTY byte stream → cell hyperlink

**Producer**: alacritty_terminal's VTE Perform impl (upstream).
**Consumer**: `Cell::hyperlink()` accessor on each affected cell.

**Contract**:

- When the byte stream contains a well-formed `OSC 8 ; <params> ; <URI>
  ST`, every cell emitted before the matching `OSC 8 ; ; ST` close MUST
  have `cell.hyperlink() == Some(Hyperlink { uri, id })`.
- On `OSC 8 ; ; ST` (close), subsequent cells MUST have
  `cell.hyperlink() == None`.
- A nested open inside an active hyperlink replaces the active URI; the
  prior span ends at the boundary. (Matches the "replace" rule in spec
  Assumptions; honoured by upstream.)
- Cells sharing a single open/close run share a single
  `Arc<CellExtra>`, so the same Arc identity proves "same span" without
  any Scribe-side merging.

**Verification**: Manual quickstart US1 emits a known OSC 8 sequence and
inspects activation behavior. There is no Scribe code under test here —
it's an observational contract against upstream.

## C2 — Cell grid → UrlSpan cache

**Producer**: `url_detect::scan_visible_urls()` (extended).
**Consumer**: `PaneUrlCache::spans` and `url_at(row, col)`.

**Contract**:

- `scan_visible_urls` MUST run an OSC 8 pass BEFORE the heuristic pass.
- For each contiguous run of cells sharing the same `Arc<CellExtra>` (so
  same `Hyperlink`), the OSC 8 pass MUST emit exactly one `UrlSpan {
  kind: Osc8Hyperlink, url: <hyperlink.uri()>, start_*/end_*: <run
  bounds> }`.
- The heuristic pass MUST skip any cell already covered by an
  `Osc8Hyperlink` span (FR-004 precedence). It MAY still detect URLs in
  cells *outside* OSC 8 spans (no regression on heuristic detection).
- `url_at(row, col)` MUST return `Osc8Hyperlink` spans first when
  multiple span kinds overlap the same cell.

**Verification**: Manual quickstart US1 scenarios 1, 2, and 3 (precedence
over heuristic, close-then-emit clears the URI). Implementation can also
add a small Rust unit test in `url_detect.rs` if convenient (no new test
*tasks* are required by this spec; constitution allows unrequested unit
tests that match an existing harness).

## C3 — Hover dwell → tooltip

**Producer**: App-level mouse move handler.
**Consumer**: `tooltip::render_tooltip()`.

**Contract**:

- After the cursor settles on a cell carrying an OSC 8 hyperlink for
  ≥300 ms with no cursor movement, the tooltip MUST be rendered above
  *or* below the hovered cell (`Position::Below` preferred; flip to
  `Above` if the cell is in the bottom row of the pane).
- The tooltip text MUST be the verbatim URI from `cell.hyperlink()`,
  truncated for display to the pane's column width (full URI is kept
  for the activation path).
- Cursor movement to a new cell MUST reset the dwell timer and hide the
  tooltip until the new cell's threshold elapses.
- Cells without an OSC 8 URI MUST NOT trigger the tooltip path (no
  regression on non-OSC-8 hover behavior).

**Verification**: Manual quickstart US2 scenario 1.

## C4 — Activation routing

**Producer**: Ctrl+click handler, right-click context menu OpenUrl
action, smart-selection open action.
**Consumer**: either `url_detect::open_url(uri)` (existing path) OR
`DisallowedSchemeDialog::show(uri)` (new).

**Contract**:

- On activation, resolve the URI:
  - If the target cell carries an OSC 8 URI, use that URI (FR-003).
  - Otherwise, fall back to the heuristic URL string at that
    row/col (unchanged behavior).
- If the resolved URI's scheme is on the existing outbound allowlist
  (`https/http/ftp/file/mailto/ssh/telnet`), call `open_url(uri)`
  directly — no dialog (SC-006: no regression in common-case latency).
- If the scheme is NOT on the allowlist, show
  `DisallowedSchemeDialog`. **Open Anyway** routes to `open_url(uri)`;
  **Cancel** dismisses. (FR-015)
- Smart-selection actions over an OSC 8 span MUST receive the OSC 8 URI
  (FR-003 explicit on smart selection).

**Verification**: Manual quickstart US1 scenarios 1-5, US2 scenario 5.

## C5 — Context menu integration

**Producer**: Right-click handler that constructs `ContextMenuRequest`.
**Consumer**: `context_menu::ContextMenu` builder.

**Contract**:

- When the click target is a cell with `cell.hyperlink() == Some(_)`:
  - `ContextMenuRequest.osc8_uri` MUST be `Some(uri)`.
  - The "Open URL" item MUST reference the OSC 8 URI (not a heuristic
    URL, even if one is also detected on the same cell).
  - A "Copy hyperlink address" item MUST be appended (FR-007).
- When the click target has no OSC 8 hyperlink:
  - `ContextMenuRequest.osc8_uri` MUST be `None`.
  - The existing items (Open URL for heuristic URL, Open File, Copy,
    Paste, Select All) appear unchanged.

**Verification**: Manual quickstart US2 scenarios 2, 3, 4.

## C6 — Replay handoff

**Producer**: `crates/scribe-common/src/screen_replay.rs#snapshot_to_ansi`
(unchanged in this spec).
**Consumer**: client-side VTE on reattach.

**Contract** (negative — what we explicitly are NOT doing):

- The replay byte stream MUST NOT be changed by this feature.
- Replayed scrollback cells MUST NOT carry OSC 8 hyperlinks (the byte
  stream does not encode them; see `research.md` decision 3).
- *Live* post-reattach hyperlinks MUST work — they ride the normal PTY
  output path.

**Verification**: Manual quickstart US3 scenario 3 (live-only;
replayed-scrollback degradation is documented as expected, not a defect).

## Out of scope (cross-references)

- No `ClientMessage` or `ServerMessage` additions.
- No config schema changes (no new keys in `crates/scribe-common/src/config.rs`).
- No webview/settings page additions.
- No persistence-format changes.
- No new keybinding configurations (the existing Ctrl+click, right-click
  menu, and smart-selection paths route to the new logic; existing
  shortcuts to Esc-dismiss dialogs apply unchanged).
