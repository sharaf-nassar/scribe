# Phase 0 Research — OSC 8 Explicit Hyperlinks

**Branch**: `009-osc8-hyperlinks`
**Date**: 2026-05-21
**Method**: Targeted code reads against `crates/`, `Cargo.toml`, the pinned
`alacritty_terminal-0.26.0-rc1` source, and adjacent shipped specs (005, 007,
008). No build, no dynamic verification — facts only.

## Headline finding

`alacritty_terminal` 0.26 already parses OSC 8 and stores the hyperlink on
each affected cell — **no upstream feature flag, no missing API**. This
collapses what the spec assumed was a meaningful implementation choice (a)
"re-enable upstream cell-attribute feature" vs. (b) "build a parallel span
layer" into a clear winner: **use the upstream feature**. Scribe's existing
`alacritty_terminal` dependency (`Cargo.toml:40`, `default-features = false`)
already pulls the relevant code in; there is nothing to enable.

## Decisions

### 1. Cell-attribute carriage: upstream `alacritty_terminal` hyperlink field

- **Decision**: Use `alacritty_terminal::term::cell::Cell::hyperlink()` /
  `set_hyperlink()` directly. Read OSC 8 URIs back from the live `Term` grid
  at render/hit-test time the same way Scribe reads colors and styles today.
- **Rationale**: The upstream `Cell` struct already has
  `extra: Option<Arc<CellExtra>>`, and `CellExtra` holds
  `hyperlink: Option<Hyperlink>` with public accessor methods on `Cell`
  (`alacritty_terminal-0.26.0-rc1/src/term/cell.rs:128,202,219`). The OSC 8
  open/close protocol is parsed by alacritty's VTE Perform impl
  unconditionally. Scribe's `default-features = false` does not gate this
  out; there is no upstream feature flag involved. Building a parallel span
  layer would duplicate parser state already maintained correctly upstream.
- **Alternatives considered**:
  - **Build a Scribe-owned parallel hyperlink span layer** (the spec's
    option (b)). Rejected: re-implements parsing already done upstream;
    requires its own `id`-merging logic; risks divergence from VTE state.
  - **Server-side OSC 8 parsing with new IPC messages**. Rejected: see
    decision 7 — server already forwards raw bytes; client-side VTE handles
    OSC 8 in the same byte stream.
- **Implication for FR-002 (cells carry URI)**: Satisfied by reading
  `cell.hyperlink()` at hit-test and render time. No new per-cell field on
  Scribe's side; the upstream Arc-shared `CellExtra` already implements the
  *spirit* of FR-016 (interning) at the cell-extra level (multiple cells in
  the same span reference one `CellExtra`).

### 2. FR-016 URI interning: leveraged via upstream `Arc<CellExtra>` sharing

- **Decision**: Treat FR-016's "per-pane URI table" as **logically
  satisfied** by alacritty_terminal's existing `Arc<CellExtra>` sharing.
  Scribe does not add a separate intern table; it relies on the upstream
  Arc-sharing semantics that already exist for cell extras (which include
  the hyperlink). On cells that share an OSC 8 open/close span, the
  upstream code shares a single `Arc<CellExtra>`. The "URI table" entity in
  the spec's data model becomes a *conceptual* one (the de facto Arc'd
  hyperlink storage), not a new Scribe data structure.
- **Rationale**: Avoids two parallel hyperlink stores. Memory bound is
  satisfied: the URI string lives once per `Arc<CellExtra>` reference graph,
  and cells trimmed from scrollback drop their `Arc`, naturally decrementing
  the refcount. Pathological "every-cell-unique-URI" emitters do allocate one
  `CellExtra` per cell, but the cap is the scrollback line cap × cols — the
  same backstop the spec named.
- **Alternatives considered**:
  - **Separate Scribe-owned URI intern table on Pane**. Rejected: duplicates
    upstream Arc sharing; adds an invalidation seam (when does Scribe evict?
    upstream already handles refcount).
  - **Per-cell verbatim URI string**. Rejected: violates PR-001's
    distinct-URI memory budget.
- **Implication**: Data-model.md describes the *conceptual* per-pane URI
  table (handle = `Arc<Hyperlink>` borrowed via `cell.hyperlink()`); no new
  struct in Scribe code.

### 3. FR-012 replayed scrollback fidelity: **documented limitation (b)**

- **Decision**: After hot reattach (zero-downtime upgrade) and cold-restart
  restore, OSC 8 hyperlinks present in the *replayed scrollback* are LOST
  by design. *Live* (post-reattach) hyperlinks emitted by the PTY after
  reattach work unchanged. This was the planning-time decision the spec
  deferred under FR-012.
- **Rationale**: `crates/scribe-common/src/screen_replay.rs#snapshot_to_ansi`
  reconstructs the replay byte stream from the snapshotted `Term` grid by
  emitting character + SGR-flag bytes (`screen_replay.rs:137-215, 240-252`)
  — it iterates cells and re-encodes color/style, but it **does not re-emit
  OSC 8 open/close sequences** because the snapshot has no concept of where
  a hyperlink began/ended. The cells carry the hyperlink in
  `Arc<CellExtra>`, but the byte stream that gets replayed through VTE on
  the receiving side has no way to reconstruct it.
- **Alternatives considered**:
  - **Extend `snapshot_to_ansi` to emit OSC 8 open/close around hyperlinked
    cell runs**. Rejected for this spec: requires snapshot-format changes
    that ripple through cold-restart persistence (would change the on-disk
    byte format and require a migration); the spec's Constitution Check
    rules out unjustified protocol/persistence changes. Worth a follow-up
    spec if there's user demand.
  - **Carry hyperlinks as a side-channel in `SessionReplay`** (e.g., a
    JSON of per-cell hyperlink references). Rejected: requires new
    `SessionReplay` field, a versioning bump, and a migration path —
    larger blast radius than the spec's scope.
- **Implication for SC-007**: *Live* (post-reattach) hyperlinks MUST work
  with 0 regressions (verifiable). Replayed-scrollback hyperlinks are
  documented as a known limitation; the cells visually appear unhyperlinked
  after reattach, but resume working as soon as the PTY emits new ones.
  This must be stated in `lat.md` and the spec's Assumptions when the
  feature ships.

### 4. Tooltip API: directly usable, App owns dwell + visibility

- **Decision**: Reuse `crates/scribe-client/src/tooltip.rs#render_tooltip`
  unchanged. Add App-level state for: (a) hover dwell timer (~300 ms after
  the cursor settles on an OSC 8 cell), (b) anchor Rect computed from the
  hovered cell's pixel position, (c) URI truncation for display (cap at the
  pane width or a reasonable column count; full URI still flows to
  activation).
- **Rationale**: The tooltip is a stateless renderer taking `(text,
  anchor_rect, position)` and emitting `CellInstance` quads
  (`tooltip.rs:43-98`). It already follows Scribe's GPU-overlay pattern.
  Multiline is not supported; URI truncation is acceptable for the *display*
  surface — the underlying activation always uses the full URI from
  `cell.hyperlink()`.
- **Alternatives considered**:
  - **Status-bar segment instead of tooltip**. Rejected during clarify
    (Q1) — tooltip anchors at the cell, status bar crowds other segments.
  - **Multiline tooltip**. Rejected: requires changes to a shared renderer
    used outside this feature; truncation is simpler and matches kitty/VTE
    behavior.
- **Implication for PR-001**: Dwell timer is App-side polling-free (driven
  by mouse-move events); the tooltip itself renders one frame per visible
  hover, well within the existing render budget.

### 5. Confirmation dialog (FR-015): factory-style copy from update_dialog.rs

- **Decision**: Create a new
  `crates/scribe-client/src/disallowed_scheme_dialog.rs` modelled after
  `update_dialog.rs` / `close_dialog.rs`. Use the same `DialogLayout`,
  `DialogRenderer`, and `DialogColors` patterns. Buttons: **Open Anyway**
  (proceeds with the URI through the existing `url_detect::open_url`-style
  path) and **Cancel** (default focus, dismisses).
- **Rationale**: Both shipped dialogs (`close_dialog.rs`,
  `update_dialog.rs`) follow an identical stateful struct + `build_instances`
  + per-button hit-test/click pattern with no shared base type. Adding a
  third dialog as a sibling file matches the established structure exactly
  and is the cleanest factory pattern in the current codebase.
- **Alternatives considered**:
  - **Refactor close_dialog + update_dialog into a shared `Dialog<T>`
    base**. Rejected for this spec: shared base is worth extracting only
    after a third user (this is that third user, so the *next* dialog would
    justify it — keep the refactor as a follow-up).
  - **Reuse the context menu surface for the prompt**. Rejected:
    context menu doesn't support a per-item action that opens another
    surface and waits for confirmation; the dialog is purpose-built for
    this kind of gate.
- **Implication for FR-015**: A ~150-line dialog file plus a routing point
  in `main.rs` where activation discriminates allowed vs disallowed schemes.

### 6. OSC 8 URI reassembly across split params: handled upstream

- **Decision**: Leave URI reassembly to alacritty_terminal's VTE Perform
  impl. Scribe's `MetadataParser::process_osc` does NOT need to learn
  OSC 8 — the upstream parser already handles VTE's `;`-split params and
  reassembles URIs that contain raw semicolons.
- **Rationale**: Scribe's pty/metadata layer extracts OSC 0/2/7/133/1337
  for *its own metadata events* (titles, CWD, prompt marks, AI hooks). OSC
  8 doesn't produce a Scribe metadata event — its effect is per-cell
  hyperlink state, which alacritty_terminal already wires up. The risk
  ("VTE splits on `;` but URIs contain `;`") is real but lives in the
  upstream parser; if upstream gets it wrong, that's an upstream bug.
  Spot-check during implementation: a URI like
  `https://example.com/x?a=1;b=2` activated via Ctrl+click should reach the
  OS handler with the query string intact.
- **Alternatives considered**:
  - **Add OSC 8 handling to Scribe's `MetadataParser`** alongside the
    existing dispatch. Rejected: would duplicate parsing already happening
    upstream; would not have any consumer (no metadata event is needed).
- **Implication for FR-001**: Satisfied transparently. Verification is
  observational — confirm `cell.hyperlink()` returns the expected URI for
  a known emit.

### 7. Server/client split: pure client-side feature, no new IPC

- **Decision**: No protocol additions. `ServerMessage::PtyOutput`
  (`crates/scribe-common/src/protocol.rs:296-300`) already carries raw
  bytes from the PTY to the client, and the client feeds them into its own
  VTE which populates the hyperlink field on cells. The server's
  `OscInterceptor` observes OSC sequences for metadata but does not
  consume them — bytes pass through to the replay buffer and to the client.
- **Rationale**: Mirrors the IME (spec 008) decision: "pure client-local,
  no IPC/protocol/persistence change." Same blast radius, same operational
  safety profile.
- **Alternatives considered**:
  - **Server-side OSC 8 parsing emitting a `HyperlinkSeen` event to the
    client**. Rejected: client-side VTE already does the work; an extra
    event is wasted IPC.
  - **Encode hyperlinks in `SessionReplay`** to fix the replay limitation.
    Rejected for this spec (see decision 3); it would require a new
    protocol field and a migration.
- **Implication for Constitution Check (Operational Safety)**: No server
  restart, no IPC change, no persistence migration. Same posture as 008.

## Resolved risks

- ✅ Upstream alacritty_terminal availability of hyperlink parsing
  (decision 1).
- ✅ Memory ceiling for many distinct URIs (decision 2 — via upstream
  `Arc<CellExtra>` sharing).
- ✅ Replay-time fidelity binary decision (decision 3 — documented
  limitation, scoped to the planning-time follow-up).
- ✅ Tooltip surface adequacy (decision 4).
- ✅ Dialog surface adequacy (decision 5).
- ✅ URI semicolon-reassembly responsibility (decision 6 — upstream).
- ✅ Protocol/IPC scope (decision 7 — none).

## Outstanding planning risks (handed to /speckit-tasks)

- **Upstream URI-with-semicolons correctness (low risk, observational)**:
  verify by emitting `OSC 8 ;; https://example.com/x?a=1;b=2 ST` during
  US1 quickstart that `cell.hyperlink().uri()` returns the full URI. If
  upstream truncates, the fallback is to add a Scribe-owned reassembler in
  `metadata.rs`, sized at ~30 lines.
- **Dwell-timer tuning (UX)**: 300 ms is a sensible default; finalise
  during quickstart pass — if too short, hovers fire during cursor
  transit; if too long, users feel laggy.
- **Replayed-scrollback limitation messaging (lat.md + spec callout)**:
  ship a clear note in `lat.md/client.md#URL Detection` or a new
  `lat.md/client.md#Hyperlinks` subsection so future readers don't expect
  replay to preserve OSC 8.
