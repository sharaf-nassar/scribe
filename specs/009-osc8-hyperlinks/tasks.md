---
description: "Task list for OSC 8 Explicit Hyperlinks"
---

# Tasks: OSC 8 Explicit Hyperlinks

**Input**: Design documents from `/specs/009-osc8-hyperlinks/`
**Prerequisites**: `plan.md`, `spec.md`, `research.md`, `data-model.md`,
`contracts/internal-osc8-pipeline.md`, `quickstart.md`

**Tests**: No automated test tasks. Spec QR-002 and the project's
test-only-on-explicit-request rule mean verification is manual quickstart
(Constitution II compliant). The OSC 8 conformance matrix (open/close,
`id=` reconnect, malformed payloads, scheme allowlist) is a recommended
*future* automated suite (would extend the existing `cargo
test --workspace` harness) requiring explicit approval — intentionally
NOT tasked here.

**Organization**: Grouped by user story. All work is client-side; no
IPC/protocol/persistence change (research-verified — see
`research.md` decision 7). No live Scribe server restart required.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no incomplete
  dependency).
- **[Story]**: US1 / US2 / US3 (Setup, Foundational, Polish carry no
  story label).
- Exact file paths included.

## Path Conventions

Existing Rust workspace. Touched paths:
`crates/scribe-client/src/`, `lat.md/`, `README.md`. No new crates, no
new modules outside `scribe-client`.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Establish the regression baseline and confirm a clean
starting point. No functional change.

- [ ] T001 [P] **[USER-OWNED — requires running GPU client; cannot run headless]** Capture the heuristic-URL baseline for SC-004 non-regression: in a session with no OSC 8 emit, save the observed Ctrl+click behaviour for several representative URL forms (`https://`, `http://`, `ftp://`, `file://`, `mailto:`, `ssh:`, `telnet:`) plus path-like spans, save the reference under `specs/009-osc8-hyperlinks/baseline-heuristic-urls.txt`
- [X] T002 [P] Confirm clean baseline: `cargo build -p scribe-client` → exit 0, `cargo test --workspace` → all green (no code change in this task)
- [X] T003 [P] Re-verify `alacritty_terminal 0.26.0-rc1` `Cell::hyperlink()` against the pinned crate source at `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/alacritty_terminal-0.26.0-rc1/src/term/cell.rs`: (a) confirm the public accessors `hyperlink()` and `set_hyperlink()` still exist at lines 128, 202, 219 and that no upstream feature flag gates them; (b) inspect upstream's OSC 8 URI handling for any length cap (search `src/term/mod.rs` or `src/event_loop.rs` for OSC 8 dispatch; check whether upstream truncates, rejects, or accepts arbitrary-length URIs). Record both findings (API surface + cap behavior) as a comment block at the top of the OSC 8 cell-walk pass added in T005, and use them to drive the FR-010 cap enforcement in T005 (apply the 2 KiB Scribe-side cap only if upstream is uncapped or applies a larger cap; if upstream cap is smaller, inherit it unchanged).

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: URL-detect cache extension shared by US1 and US2. US3
builds on it observationally. This is the *only* foundational phase
because every other piece is story-specific.

- [X] T004 Extend `SpanKind` enum with a new `Osc8Hyperlink` variant in `crates/scribe-client/src/url_detect.rs` — additive, no rename of existing variants (`Url`, `FilePath` retained)
- [X] T005 Add OSC 8 cell-walk pass to `scan_visible_urls` in `crates/scribe-client/src/url_detect.rs`: iterate the visible grid via the existing alacritty display iterator, read `cell.hyperlink()`, and for each contiguous run of cells sharing the same `Arc<CellExtra>` (so same `Hyperlink`) emit an `UrlSpan { kind: SpanKind::Osc8Hyperlink, url: hyperlink.uri().to_string(), start_row/col/end_row/col: <run bounds> }`. Run this pass BEFORE the existing heuristic pass. **FR-010 cap enforcement:** if `hyperlink.uri().len() > 2048` AND upstream did not already truncate/reject (per T003 findings), skip emitting any `UrlSpan` for that hyperlink — the cell carries no OSC 8 URI in the cache. Document the chosen branch (upstream-enforced vs. Scribe-enforced) in the same comment block T003 writes (depends on T004, T003)
- [X] T006 Make the existing heuristic pass in `scan_visible_urls` in `crates/scribe-client/src/url_detect.rs` skip any cell already covered by an `Osc8Hyperlink` span (FR-004 precedence). The heuristic pass MUST continue to function unchanged for cells outside OSC 8 spans (FR-014, SC-004) (depends on T005)
- [X] T007 Update `PaneUrlCache::url_at(row, col)` in `crates/scribe-client/src/url_detect.rs` so that when multiple spans cover the same cell, `Osc8Hyperlink` is returned before `Url` and `FilePath` (linear scan preserved; only the ordering changes) (depends on T006)

**Checkpoint**: URL-detect cache now carries OSC 8 spans with precedence. US1 and US2 can both begin immediately, in parallel.

---

## Phase 3: User Story 1 - Tool-emitted hyperlinks reach their real destination (Priority: P1) 🎯 MVP

**Goal**: When a tool emits OSC 8 hyperlinks, Ctrl+click, right-click → Open URL, and smart-selection activation all open the OSC 8 URI (not the heuristic-detected substring). Disallowed-scheme URIs route through the FR-015 confirmation dialog.

**Independent Test**: `quickstart.md` US1 scenarios 1–6 — `ls --hyperlink=auto` activation, precedence over heuristic, close clears, `id=` wrap reconnect, disallowed-scheme dialog (Cancel and Open Anyway both verified), multi-pane isolation.

### Implementation for User Story 1

- [X] T008 [US1] Plumb OSC 8 URI through Ctrl+click activation in `crates/scribe-client/src/main.rs`: at the existing Ctrl+click → `open_url` call site, ask `PaneUrlCache::url_at` for the span at the clicked cell and pass `span.url` directly (no heuristic re-scan of displayed text). This satisfies FR-003 for the Ctrl+click path (depends on T007)
- [X] T009 [US1] Plumb OSC 8 URI through right-click context-menu Open URL in `crates/scribe-client/src/context_menu.rs` and `crates/scribe-client/src/main.rs`: add `osc8_uri: Option<String>` field to `ContextMenuRequest`; in `crates/scribe-client/src/main.rs` right-click handler, populate `osc8_uri = Some(uri)` when the hit-tested cell has an OSC 8 span; in the menu builder use `osc8_uri` (if `Some`) in place of the heuristic `url` for the "Open URL" item's `ContextMenuAction::OpenUrl(uri)` payload (depends on T007)
- [X] T010 [US1] Plumb OSC 8 URI through smart-selection actions that open a URL in `crates/scribe-client/src/main.rs`: at the smart-selection action entry that today calls the open-URL path, prefer `PaneUrlCache::url_at(row, col).url` when the selection origin cell carries an `Osc8Hyperlink` (depends on T007)
- [X] T011 [US1] Add a scheme-allowlist check at the activation entry in `crates/scribe-client/src/main.rs` (single helper, e.g. `fn route_activation(uri: &str)`): extract the scheme (substring up to the first `:`), match against the existing allowlist (`https`, `http`, `ftp`, `file`, `mailto`, `ssh`, `telnet`) defined in `crates/scribe-client/src/url_detect.rs#PREFIXES`. Allowed-scheme URIs route directly to the existing `open_url` path; disallowed schemes route to `DisallowedSchemeDialog::show(uri)` (depends on T008, T009, T010)
- [X] T012 [US1] Create `crates/scribe-client/src/disallowed_scheme_dialog.rs` (NEW file) modeled after `crates/scribe-client/src/update_dialog.rs`: `DisallowedSchemeDialog` struct with `pending_uri: String`, `scheme: String`, `focused_button: enum { Cancel, OpenAnyway }` defaulting to `Cancel`, `hovered_button: Option<_>`; `DialogLayout`/`DialogRenderer`/`DialogColors` patterns copied verbatim from `update_dialog.rs`; two buttons (Cancel default focus, Open Anyway); Esc dismisses (returns `Cancel`), Enter activates focused button, Tab cycles focus, mouse click activates the clicked button. Dialog body: "Scheme `<scheme>:` is normally blocked. Open `<pending_uri>` anyway?" — single-line warning + the URI (truncated to fit dialog width if needed)
- [X] T013 [US1] Add `mod disallowed_scheme_dialog;` to `crates/scribe-client/src/main.rs` and instantiate one per window alongside `close_dialog` and `update_dialog`. Add render call to the existing render loop (one branch per dialog, mutually exclusive) and event-routing branches so Esc/Enter/Tab/click events reach the dialog while it is visible. **Open Anyway** routes to the existing `url_detect::open_url`-style call; **Cancel** dismisses and does nothing (depends on T011, T012)
- [ ] T014 [US1] **[USER-OWNED — manual quickstart, needs GPU client]** Run quickstart.md US1 scenarios 1–6 (`ls --hyperlink` activation, precedence over heuristic, close clears, `id=` wrap reconnect, disallowed-scheme dialog Cancel+OpenAnyway, multi-pane isolation) PLUS scenario 7 (malformed-OSC-8 emit verifies FR-013 no-crash + no-dangling-hyperlink behavior) PLUS the oversized-URI sub-check verifying the FR-010 cap. Record observations; any deviation from the documented Pass criterion blocks US1 close-out (depends on T013)

**Checkpoint**: US1 fully functional and independently testable (MVP slice).

---

## Phase 4: User Story 2 - Real destination visible before activation (Priority: P1)

**Goal**: Users see the true OSC 8 URI before clicking — via hover tooltip and via the right-click context menu — and gain a "Copy hyperlink address" entry distinct from the existing "Copy" path.

**Independent Test**: `quickstart.md` US2 scenarios 1–5 — tooltip dwell with verbatim URI, context-menu Open URL shows real URI, Copy hyperlink address writes URI to clipboard, selection-copy semantics unchanged, disallowed-scheme hover still shows URI.

### Implementation for User Story 2

- [X] T015 [P] [US2] Add HoverState fields to `App` in `crates/scribe-client/src/main.rs`: `hover_cell: Option<(PaneId, i32, usize)>`, `hover_started_at: Option<Instant>`, `hover_tooltip_visible: bool`, `hover_tooltip_uri: Option<String>`. Initialize to `None`/`false` in `App::new`. (Different file region from T016/T017 but same file — sequential within main.rs; the [P] marker is against the `context_menu.rs` track T018–T020 which can run in parallel)
- [X] T016 [US2] Update the existing mouse-move handler in `crates/scribe-client/src/main.rs`: on cursor entering a new cell, reset `hover_started_at = Some(Instant::now())`, set `hover_cell` (or `None` outside terminal panes), clear `hover_tooltip_visible` and `hover_tooltip_uri`. Cells without an OSC 8 hyperlink MUST NOT trigger the dwell path (read `PaneUrlCache::url_at` and gate on `kind == Osc8Hyperlink`) (depends on T015, T007)
- [X] T017 [US2] In the App render loop in `crates/scribe-client/src/main.rs`: when `hover_started_at` has elapsed ≥300 ms and the current `hover_cell` still carries an OSC 8 URI, set `hover_tooltip_visible = true` and cache the URI in `hover_tooltip_uri`. While visible, compute the anchor `Rect` from the hovered cell's pixel coordinates (use the existing `cell_to_pixel`-style helper) and call `tooltip::render_tooltip(&display_uri, anchor, Position::Below)`, falling back to `Position::Above` when the cell is in the bottom row. Truncate `display_uri` to the pane's column width; full URI stays in `hover_tooltip_uri` for activation (depends on T016)
- [X] T018 [P] [US2] Add `ContextMenuAction::CopyHyperlinkAddress(String)` variant in `crates/scribe-client/src/context_menu.rs` (no behaviour change yet — the action is dispatched in T021). Different file from T015–T017 so this can run in parallel with the main.rs track
- [X] T019 [US2] In `crates/scribe-client/src/context_menu.rs`, extend the menu builder so it appends a "Copy hyperlink address" item with `ContextMenuAction::CopyHyperlinkAddress(uri)` when `ContextMenuRequest.osc8_uri` is `Some(uri)`. Item placement: after "Open File" (consistent with the existing pattern of appending context-dependent items) (depends on T018, T009)
- [X] T020 [US2] In `crates/scribe-client/src/main.rs`, dispatch `ContextMenuAction::CopyHyperlinkAddress(uri)` by routing through the same clipboard-write path that `ContextMenuAction::Copy` uses — write `uri` verbatim to the system clipboard via the existing client-side clipboard write (the heuristic path's existing infra) (depends on T018)
- [ ] T021 [US2] **[USER-OWNED — manual quickstart, needs GPU client]** Run quickstart.md US2 scenarios 1–5 (tooltip dwell shows verbatim URI, context-menu Open URL shows real URI, Copy hyperlink address writes URI to clipboard, selection-copy unchanged, disallowed-scheme hover still shows URI). Record observations and tune the 300 ms dwell threshold if it feels laggy or fires during cursor transit; document the chosen value if it differs from 300 ms (depends on T017, T019, T020)

**Checkpoint**: US1 AND US2 both functional. Either alone is a shippable MVP slice (both P1).

---

## Phase 5: User Story 3 - Hyperlinks survive scrollback, wrapping, and reattach (Priority: P2)

**Goal**: OSC 8 spans remain navigable across line wraps, scrollback survival within the configured cap, and after server hot reattach / cold restart — with the documented replay-scrollback limitation acknowledged.

**Independent Test**: `quickstart.md` US3 scenarios 1–5 — wrapped span end-to-end, scrollback survival, live post-reattach (firm MUST), scrollback trim consistency, cross-pane isolation under load.

**Dependency**: requires US1 cache work (T007) and US2 hover plumbing (T017) to manually verify hover + activation through scrollback / reattach.

### Implementation for User Story 3

- [X] T022 [US3] Audit `PaneUrlCache::mark_dirty` call sites in `crates/scribe-client/src/main.rs` and `crates/scribe-client/src/pane.rs` to confirm the cache invalidates on scrollback trim and on PTY-output (`content_dirty`) events. If any trim path does not currently mark the URL cache dirty, add the call here so OSC 8 spans whose cells are trimmed drop from the cache (preventing dangling `Osc8Hyperlink` URIs). Most likely a 1-3 line change; if no change is needed, record that observation in the task notes (depends on T005)
- [ ] T023 [US3] **[USER-OWNED — manual quickstart, needs GPU client + a hot-reattach / cold-restart capable build]** Run quickstart.md US3 scenarios 1–5 (wrapped end-to-end, scrollback survival, live post-reattach firm MUST, scrollback trim consistency, cross-pane isolation) PLUS scenario 6 (`id=` anti-merge: emit two separately-opened OSC 8 spans reusing the same `id` with different URIs and confirm each activates to its own destination per FR-005 per-open-sequence scope). The Known Limitation for replay-scrollback hyperlinks (cells from before reattach do NOT carry OSC 8) is expected behaviour per `research.md` decision 3 — do not treat as a defect (depends on T022, T014, T021)

**Checkpoint**: All three user stories independently functional.

---

## Phase 6: Polish & Cross-Cutting Concerns

- [X] T024 [P] Update `lat.md/client.md`: extend the URL Detection subsection to note OSC 8 precedence over heuristic detection, and add a new short Hyperlinks subsection covering (a) tooltip-on-dwell surface, (b) context-menu "Copy hyperlink address" entry, (c) DisallowedSchemeDialog confirmation gate, and (d) the **replay-scrollback limitation** (cells reconstructed from `SessionReplay` do NOT carry OSC 8 — live post-reattach hyperlinks do work) (depends on T014, T021, T023)
- [X] T025 [P] Update `README.md` if and only if a user-visible feature-list section already exists in it that calls out clickable URLs / hyperlinks; add a one-line note that Scribe now honours OSC 8 explicit hyperlinks (file: `README.md`). Skip this task if no such section exists — no new section is required by this spec
- [X] T026 Run `lat check` from repo root; must report `All checks passed`. If it errors, fix the lat.md issues from T024 and re-run (depends on T024)
- [ ] T027 [P] **[USER-OWNED — manual measurement, needs GPU client]** Performance verification (PR-001, SC-005): time Ctrl+click activation manually for an allowed-scheme OSC 8 hyperlink vs a plain `https://` heuristic URL across at least 10 trials each. Record observations; no perceptible difference is the pass criterion (depends on T021)
- [ ] T028 [P] **[USER-OWNED — needs macOS host]** Cross-platform spot-check on macOS for representative US1 and US2 scenarios (Ctrl+click `file://` URI activation, tooltip-on-dwell, DisallowedSchemeDialog Cancel/OpenAnyway, Copy hyperlink address). Record observations (depends on T014, T021)
- [ ] T029 Final completion report: assemble a summary naming the verification commands run (T002, T014, T021, T023, T026, T027, T028), the residual risks (upstream URI-with-semicolons correctness — observational in T014; dwell-timer value if tuned in T021), and the documented replay-scrollback limitation. File the report inline in the merge commit's body or under `specs/009-osc8-hyperlinks/completion-report.md` if a separate doc is preferred (depends on T024, T025, T026, T027, T028)

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: no dependencies; T001, T002, T003 all `[P]`.
- **Foundational (Phase 2)**: depends on Setup; T004 → T005 → T006 → T007 (sequential, all in `url_detect.rs`).
- **US1 (Phase 3)** and **US2 (Phase 4)**: both start after Foundational; can run **in parallel** by different developers. Caution: both edit `crates/scribe-client/src/main.rs` and `crates/scribe-client/src/context_menu.rs` — coordinate or serialize merges.
- **US3 (Phase 5)**: depends on US1 (T014) and US2 (T021) for the manual verification path; T022 (cache-dirty audit) may start anytime after T005.
- **Polish (Phase 6)**: after the targeted stories complete; T024–T028 mostly parallel, T029 is the assembling finish.

### User Story Dependencies

- **US1 (P1)**: foundational only. MVP-capable alone.
- **US2 (P1)**: foundational only. MVP-capable alone. Equal priority with US1 — pick either as the first shippable increment.
- **US3 (P2)**: independently *testable* once US1 and US2 manual verifications pass; T022 may start in parallel with US1/US2.

### Within-Story Order

- **US1**: T008 / T009 / T010 are parallel-safe in principle (different fragments of `main.rs` / `context_menu.rs`); in practice serialise main.rs edits to avoid merge headaches. T011 after T008+T009+T010. T012 is a NEW file — fully parallel-safe. T013 after T011+T012. T014 after T013.
- **US2**: T015 → T016 → T017 (sequential in main.rs). T018 → T019 → T020 (T018 is independent of US1, T019 depends on T018+T009 which lands in US1, T020 dispatches the new action). T021 after T017 and T020 and T019.
- **US3**: T022 after T005; T023 after T022 + T014 + T021.

### Parallel Opportunities

- Setup: T001, T002, T003 together.
- Foundational: sequential (same file).
- Cross-track: entire US1 and US2 in parallel by different developers; coordinate `main.rs` and `context_menu.rs` merges. `[P]` within stories across files: T012 (NEW dialog file) ∥ T008/T009/T010; T015 (main.rs HoverState) ∥ T018 (context_menu.rs action). Polish: T024, T025, T027, T028 all `[P]`.

### Parallel Example (cross-track, after Foundational)

```bash
# Developer A — US1 (main.rs activation routing + NEW disallowed_scheme_dialog.rs)
Task: "T008 Ctrl+click OSC 8 URI plumbing in crates/scribe-client/src/main.rs"
# Developer B — US2 (main.rs HoverState + context_menu.rs CopyHyperlinkAddress)
Task: "T015 HoverState fields on App in crates/scribe-client/src/main.rs"
```

---

## Implementation Strategy

### MVP First

Minimal MVP = **Setup + Foundational + US1** OR **Setup + Foundational + US2** — both are P1 and independently shippable. Either alone delivers user-visible value (activation correctness, or trust signalling). Pick US1 first if optimising for tool-emitted hyperlink correctness, US2 first if optimising for the security/trust posture.

### Incremental Delivery

1. Setup → baseline captured.
2. Foundational → URL-detect cache extended.
3. US1 → verify quickstart US1 → ship (MVP).  *(or US2 first — both P1, independent.)*
4. US2 → verify quickstart US2 → ship.
5. US3 → verify quickstart US3 → ship.
6. Polish → lat.md update, lat check, performance + macOS spot-check, completion report.

### Parallel Team Strategy

After Foundational: Dev A takes US1, Dev B takes US2 (coordinate `main.rs` and `context_menu.rs` merges). Either dev can pick up US3 after their story closes since US3 is mostly observational verification.

---

## Notes

- No automated test tasks (QR-002 + project test-on-request rule); every story has a USER-OWNED manual quickstart verification task (Constitution II compliant).
- `[P]` = different files, no incomplete dependency. `[Story]` maps to spec.md user stories.
- No IPC/protocol/persistence change; no live server restart. No config keys added. No webview/settings page changes.
- Update `lat.md/client.md` (T024) when behavior changes; run `lat check` (T026) to gate completion.
- Replay-scrollback hyperlink loss is a **known, documented limitation** (research.md decision 3, contract C6) — do not treat as a defect.
- Commit after each task or logical group. Stop at any checkpoint to validate independently.
