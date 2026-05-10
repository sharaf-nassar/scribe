---

description: "Tasks for implementing the Releases Page feature"
---

# Tasks: Releases Page

**Input**: Design documents from `/specs/001-releases-page/`
**Prerequisites**: [plan.md](./plan.md), [spec.md](./spec.md), [research.md](./research.md), [data-model.md](./data-model.md), [contracts/releases-protocol.md](./contracts/releases-protocol.md), [quickstart.md](./quickstart.md)

**Tests**: TDD tasks are included as a deliberate part of this feature's design — [research.md](./research.md) §R10 and [contracts/releases-protocol.md](./contracts/releases-protocol.md) §3 enumerate explicit test obligations for every layer (serde round-trips, cache state machine, render-and-sanitize pipeline, transport-failure mapping, scheme validation, bootstrap snapshot, stale-while-revalidate). Each test task lands next to its target code as `#[cfg(test)] mod tests` per existing project convention. Tests are written first, observed to FAIL, then the corresponding implementation task makes them pass.

**Organization**: Tasks are grouped by user story to enable independent implementation and testing of each story.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies on incomplete tasks)
- **[Story]**: Which user story this task belongs to (US1 = Browse releases, US2 = Footer version, US3 = Offline / failure resilience)
- Every task includes its exact file path

## Path Conventions

This is an existing multi-crate Rust workspace. All file paths below are repository-root-relative. The feature spreads across:

- `crates/scribe-common/src/protocol.rs` (wire types)
- `crates/scribe-server/src/{updater.rs,releases.rs,ipc_server.rs}` (server-side data + dispatch)
- `crates/scribe-server/Cargo.toml` (deps)
- `crates/scribe-settings/src/{lib.rs,server_action.rs}` (settings host process)
- `crates/scribe-settings/src/assets/{settings.html,settings.css,settings.js}` (webview)
- `lat.md/{settings.md,server.md,protocol.md,architecture.md}` (documentation)

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Add new external dependencies the feature needs.

- [X] T001 Add `pulldown-cmark` (CommonMark + GFM features) and `ammonia` dependencies in `crates/scribe-server/Cargo.toml`. Use `find-docs` / Context7 to confirm the current major versions and feature flags before pinning.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Cross-cutting infrastructure used by both the existing updater and the new releases module.

**⚠️ CRITICAL**: User Story 1 (P1) and User Story 3 (P3) both depend on this phase. User Story 2 (P2) does not depend on this phase and may proceed in parallel.

- [X] T002 Extract a shared `pub(crate) fn http_client() -> &'static reqwest::Client` (or equivalent `OnceCell`-backed accessor) helper in `crates/scribe-server/src/updater.rs`, re-using the User-Agent / TLS configuration `fetch_latest_release` already builds today, so both `updater.rs` and the new `releases.rs` consume the same configured `reqwest::Client`. Keep `fetch_latest_release`'s observable behavior unchanged.

**Checkpoint**: HTTP client is shared. User stories may now begin.

---

## Phase 3: User Story 1 - Browse Scribe release notes from inside the app (Priority: P1) 🎯 MVP

**Goal**: Add the "Releases" sidebar entry, a settings panel containing a version picker + Older/Newer navigation buttons + rendered notes content area, the server-side cache and GitHub fetcher, the protocol wiring, and the IPC handlers — all working on the happy path. Failure-state UI is split out into User Story 3.

**Independent Test**: Open settings online → click Releases → newest release notes render in the content area and the version picker is pre-set to that release. Pick an older release from the dropdown → notes update within 200 ms with no extra network call. Click the Older / Newer navigation buttons → picker and content stay in sync; buttons disable visibly at the boundaries. Click a link in the rendered notes → it opens in the OS browser, not inside the webview.

### Tests for User Story 1 ⚠️

> **NOTE: Write these tests FIRST, see them FAIL, then implement the corresponding tasks below to make them pass.**

- [X] T003 [P] [US1] Add a `#[cfg(test)] mod tests` (or extend the existing one) in `crates/scribe-common/src/protocol.rs` with msgpack-named round-trip tests for `Release`, `ReleaseListResultState::{Fresh, Stale, Failed}`, `ClientMessage::ListReleases`, and `ServerMessage::ReleaseList { state }`, asserting field names match the wire examples in `contracts/releases-protocol.md` §1.2.
- [X] T004 [P] [US1] Add a `#[cfg(test)] mod tests` in `crates/scribe-server/src/releases.rs` (created in T009) covering the markdown → `pulldown-cmark` → `ammonia::clean` pipeline against fixtures: plain text, fenced code block, list, link, table, and an attempted `<script>` injection that the sanitizer must strip. Pre-create the file as a stub so the test compiles before T009 lands the implementation.
- [X] T005 [P] [US1] Add cache state machine tests for `ReleaseCatalog` in `crates/scribe-server/src/releases.rs` test module: (a) returns `Fresh` while within TTL, (b) returns `Stale { releases, .. }` past TTL when a refresh is inflight, (c) returns `Failed` when no cache exists and the synchronous fetch errors, (d) does not produce a thundering herd when multiple concurrent calls arrive while a refresh is inflight.
- [X] T006 [P] [US1] Add a transport-failure mapping test for `request_release_list` in `crates/scribe-settings/src/server_action.rs` test module: simulate a closed-peer-mid-frame condition on a temp Unix socket and assert the helper returns `ReleaseListResultState::Failed { reason }` (no panic).
- [X] T007 [P] [US1] Add a scheme-validation test for the host-side `open_external_url` handler in `crates/scribe-settings/src/lib.rs` test module: assert that `javascript:`, `file:`, `data:`, and other non-http(s) schemes are dropped (and `tracing::warn!`-logged) without invoking the platform opener; assert `http://` and `https://` are accepted.

### Implementation for User Story 1

- [X] T008 [US1] Add `pub struct Release { version, name, published_at, body_html, prerelease, html_url }`, `pub enum ReleaseListResultState { Fresh { releases }, Stale { releases, reason }, Failed { reason } }`, `ClientMessage::ListReleases`, and `ServerMessage::ReleaseList { state: ReleaseListResultState }` to `crates/scribe-common/src/protocol.rs`. Match the existing `UpdateCheckResultState`'s serde attributes (`#[serde(tag = "type", content = "data", rename_all = "snake_case")]` or whatever the file's existing convention is) line-for-line so the wire format is consistent. Makes T003 pass.
- [X] T009 [US1] Create `crates/scribe-server/src/releases.rs`. Implement `pub struct ReleaseCatalog { last_fetched_at: Option<Instant>, last_fetch_was_success: bool, value: Option<Vec<Release>>, ttl: Duration, inflight_refresh: <flag/handle> }`. Implement an async `fetch_releases(client) -> Result<Vec<Release>, ScribeError>` that calls `https://api.github.com/repos/sharaf-nassar/scribe/releases?per_page=30`, drops drafts, keeps pre-releases, and runs each release's `body` through `pulldown-cmark` (CommonMark + GFM) → `ammonia::clean` to produce `Release.body_html`. Implement `pub async fn handle_list_releases(catalog: &Arc<Mutex<ReleaseCatalog>>) -> ReleaseListResultState` realizing the Fresh / Stale / Failed transitions in `data-model.md`. Declare the new module in BOTH parent files to match the existing `updater` pattern: add `pub mod releases;` next to the existing `pub mod updater;` at `crates/scribe-server/src/lib.rs:15`, and add `mod releases;` next to the existing `mod updater;` at `crates/scribe-server/src/main.rs:20`. Makes T004 and T005 pass.
- [X] T010 [US1] Wire `ClientMessage::ListReleases` into the existing dispatcher in `crates/scribe-server/src/ipc_server.rs`. Add a parallel arm to each of the three existing `ClientMessage::CheckForUpdates` sites (around lines 511, 721, and 844 — verify line numbers at edit time): the new arm calls `releases::handle_list_releases(&catalog)` and replies with `ServerMessage::ReleaseList { state }`. Pass / construct a `ReleaseCatalog` shared handle alongside the existing updater state.
- [X] T011 [P] [US1] Add `pub fn request_release_list(timeout: Duration) -> ReleaseListResultState` in `crates/scribe-settings/src/server_action.rs`, mirroring `request_update_check`: open a fresh `UnixStream` to `server_socket_path()`, set read/write timeouts, send `ClientMessage::ListReleases` via the existing `write_frame`, read one `ServerMessage::ReleaseList { state }` via `read_frame`, return `state`. Convert any transport error or unexpected message into `ReleaseListResultState::Failed { reason }`. Makes T006 pass.
- [X] T012 [P] [US1] In `crates/scribe-settings/src/assets/settings.html`, add a `<div class="nav-item" data-tab="releases">` entry between the existing "Updates" and "Notifications" entries (reuse the existing nav-item icon styling). Add the empty `<section id="releases-panel" class="panel">` shell with a header row containing `<button id="releases-older" class="releases-nav-btn" disabled>‹ Older</button>`, `<select id="releases-picker" class="releases-picker">`, `<button id="releases-newer" class="releases-nav-btn" disabled>Newer ›</button>`, and `<a id="releases-external" class="releases-external" data-external href="#">View on GitHub</a>` (right-aligned). Add the content area `<article id="release-notes" class="release-notes" aria-live="polite"></article>` and the status banner `<div id="releases-status" class="releases-status" hidden></div>`.
- [X] T013 [P] [US1] In `crates/scribe-settings/src/assets/settings.css`, add styles for `.releases-nav` (flex row, tight gap, vertically centered), `.releases-nav-btn` (sharp corners, reuse existing settings-button color tokens, `:disabled` rule that lowers contrast and disables cursor), `.releases-picker` (native `<select>` styled to match other settings inputs — height, border, background), `.releases-external` (right-aligned panel-header link), `.release-notes` (typography reusing existing settings-panel font sizes for `<h1>`–`<h3>`, `<code>`, `<pre>`, links — do NOT redefine sizes), `.releases-status` (base layout — variant classes are added in T022), and `.pre-release-badge` (small inline label inside the rendered notes header). Use only existing CSS variables; corners ≤ 6px; no glassmorphism per FR-017.
- [X] T014 [US1] In `crates/scribe-settings/src/lib.rs`, extend the existing `window.ipc.postMessage` dispatch with two new arms. (1) `"request_releases"` — `std::thread::spawn` a worker that calls `request_release_list(Duration::from_secs(7))` and on completion runs `webview.evaluate_script(format!("window.SCRIBE_ON_RELEASE_LIST({});", serde_json::to_string(&payload)?))` where `payload` carries `state` ("fresh"/"stale"/"failed"), `releases` (only for fresh/stale), `reason` (only for stale/failed), and `fetched_at` (only for fresh/stale). (2) `"open_external_url"` — extract `url`, validate it starts with `http://` or `https://`, dispatch via `Command::new(if cfg!(target_os = "linux") { "xdg-open" } else { "open" }).arg(url).spawn()`; non-http(s) schemes are dropped with `tracing::warn!`. Makes T007 pass.

  Note on the 7 s timeout: the success target in SC-001 / SC-003 is **5 s** on a typical broadband connection. The 7 s timeout here is the upper bound for failure detection — it provides headroom over the 5 s SLA so that a slightly slow but ultimately successful fetch still completes; only requests genuinely stuck past 7 s flip the panel into the `Failed` state.
- [X] T015 [US1] In `crates/scribe-settings/src/assets/settings.js`, add the Releases-panel logic. Define `window.SCRIBE_ON_RELEASE_LIST = function(payload) { ... }` near the top of the file (before any tab-activation can fire) that updates the JS state (`releases`, `selectedReleaseVersion`, `releaseListState`, `releaseLastFetchedAt`) and re-renders. Implement `renderRelease()` that finds the `Release` matching `selectedReleaseVersion` and writes its `body_html` to `#release-notes` via `innerHTML`, sets the picker's selected option, and updates the `[data-external]` link's `href` to that release's `html_url`. Implement `populatePicker(releases)` that fills `#releases-picker` with one `<option>` per release labeled `vX.Y.Z — YYYY-MM-DD` with a `[PRE] ` prefix when `prerelease` is true. Implement `updateNavBoundaries()` that toggles the `disabled` attribute on `#releases-newer` (when at index 0) and `#releases-older` (when at index `releases.length - 1`) — single source of truth for FR-019 / FR-020. Wire the picker `change` handler and both buttons' `click` handlers to a single state-update path that calls `renderRelease()` and `updateNavBoundaries()`. Delegate clicks on any `<a>` inside `#release-notes` and on `[data-external]` to call `event.preventDefault()` and `sendIpc({ type: "open_external_url", url: e.currentTarget.href })`. In the existing tab-activation handler, when activating `data-tab="releases"` with empty `releases` state, post `sendIpc({ type: "request_releases" })`.

**Checkpoint**: User Story 1 should be fully functional and testable on the happy path. Failure UX still falls through to a generic "could not load" until US3 lands.

---

## Phase 4: User Story 2 - See the accurate running Scribe version in the sidebar footer (Priority: P2)

**Goal**: Replace the hardcoded `Scribe v0.1.0` in the sidebar footer with a value sourced from the actual workspace build, so version bumps propagate without source-edits to the settings UI.

**Independent Test**: Build at the current workspace version and confirm the footer matches. Bump `Cargo.toml`'s workspace `version` to a synthetic value (e.g. `9.9.9`), rebuild only `scribe-settings`, launch, confirm the footer reads `Scribe v9.9.9`. Revert.

**Note**: This story is independent of US1 and US3 — it does not touch any of the protocol, server, cache, or IPC paths. It can be implemented before, after, or in parallel with US1 by a different developer; the only friction is shared-file edits in `lib.rs`, `settings.html`, and `settings.js`.

### Tests for User Story 2 ⚠️

- [X] T016 [P] [US2] Add a snapshot/substring test in `crates/scribe-settings/src/lib.rs` test module that asserts the constructed pre-page-load bootstrap script literal contains a `version: "<expected>"` entry equal to `env!("CARGO_PKG_VERSION")` and a JSON-safe `platform` value. Use a small helper (e.g. `fn bootstrap_script(version: &str, platform: &str) -> String`) that the test can call directly without running the full webview.

### Implementation for User Story 2

- [X] T017 [P] [US2] In `crates/scribe-settings/src/assets/settings.html`, replace `<div class="sidebar-footer">Scribe v0.1.0</div>` with `<div class="sidebar-footer" id="sidebar-footer"></div>`.
- [X] T018 [US2] In `crates/scribe-settings/src/lib.rs`, add the pre-page-load script-injection step (use the wry / GTK pre-page-load script hook the platform exposes) that defines `window.SCRIBE_BOOTSTRAP = { version: "<env!(\"CARGO_PKG_VERSION\")>", platform: "<linux|macos|other>" };`. JSON-escape both string values; do not interpolate raw. Extract the script-building into a small pure helper so T016's test can target it directly. Makes T016 pass.
- [X] T019 [P] [US2] In `crates/scribe-settings/src/assets/settings.js`, add a `DOMContentLoaded` listener that reads `window.SCRIBE_BOOTSTRAP?.version`, writes `Scribe v${version}` (or just `Scribe` when the value is falsy or missing) into `#sidebar-footer`. Run this once on initial load; do not re-run on tab activation.

**Checkpoint**: Both User Story 1 (browse releases, happy path) and User Story 2 (accurate footer version) work independently.

---

## Phase 5: User Story 3 - Use the Releases page when GitHub is slow, rate-limited, or unreachable (Priority: P3)

**Goal**: Polish the failure UX of the Releases page: distinct loading / failed / stale states with retry controls, and a unit-test that pins down the stale-while-revalidate behavior so the handler never silently downgrades to `Failed` when a cached value is available (FR-013).

**Independent Test**: With the Releases page previously loaded, disconnect the network and re-open the Releases tab → previously cached releases render along with a visible "may be stale" indicator. With network still disconnected, click Refresh → status updates with the failure reason, page remains usable. Click Retry → request is re-issued and the success state restores once connectivity returns.

### Tests for User Story 3 ⚠️

- [X] T020 [P] [US3] Add a stale-while-revalidate unit test for `handle_list_releases` in `crates/scribe-server/src/releases.rs` test module: pre-populate `ReleaseCatalog.value` with a fixture vector and `last_fetched_at` past TTL; inject a fetch implementation that fails; assert the handler returns `ReleaseListResultState::Stale { releases, reason }` (NOT `Failed`) and the `releases` payload equals the fixture. Verifies FR-013.

### Implementation for User Story 3

- [X] T021 [US3] In `crates/scribe-settings/src/assets/settings.js`, extend `window.SCRIBE_ON_RELEASE_LIST` and the panel render path to handle the loading, stale, and failed states distinctly. Loading: while waiting for the IPC response, show a non-blocking "Loading releases…" message in `#releases-status` (class `is-loading`) without disabling the rest of the settings window. Stale: render the cached releases plus a visible "may be stale (last refreshed at <fetched_at>) — reason: <reason>" indicator (class `is-stale`) and a "Refresh" button that re-posts `request_releases`. Failed: render a plain-language message with the `reason` from the payload (class `is-error`) and a "Retry" button that re-posts `request_releases`. Reuse the `releaseListState` JS state key from US1 to drive which sub-view is shown.
- [X] T022 [P] [US3] In `crates/scribe-settings/src/assets/settings.css`, add styles for the three `.releases-status` variant classes — `.releases-status.is-loading`, `.releases-status.is-error`, and `.releases-status.is-stale` — distinguishing them by accent color and the in-banner action button styling, all reusing existing CSS variables. The Retry / Refresh button reuses `.releases-nav-btn` styling for consistency.

**Checkpoint**: All three user stories are independently functional. Failure modes are explicit; data is preserved across transient outages.

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: Update the project's `lat.md/` knowledge graph, run the full validation walkthrough, and make sure local hooks pass before the work is closed.

- [X] T023 [P] Update `lat.md/settings.md` with new sections: a "Releases" panel section under Settings (leading paragraph ≤ 250 chars summarizing what the panel does and how it is wired), and a "Sidebar Footer" section documenting the bootstrap-injected version source. Cross-link via `[[…]]` source-code refs to `crates/scribe-settings/src/lib.rs`, `crates/scribe-settings/src/server_action.rs#request_release_list`, and the assets.
- [X] T024 [P] Update `lat.md/server.md` with a new "Releases / Release Catalog" section under Server (leading paragraph ≤ 250 chars), documenting the cache state machine (`Fresh` / `Stale` / `Failed`), TTL, the GitHub `/releases?per_page=30` endpoint usage, and the markdown render+sanitize pipeline. Cross-link to `crates/scribe-server/src/releases.rs#ReleaseCatalog` and `#handle_list_releases`.
- [X] T025 [P] Update `lat.md/protocol.md` with new entries: "List Releases" under Client Messages (one paragraph) and "Release List" under Server Messages (one paragraph including the three result variants). Cross-link to `crates/scribe-common/src/protocol.rs#ClientMessage` and `#ServerMessage`.
- [X] T026 [P] Update `lat.md/architecture.md` to cross-link the new sections from the relevant Crate Map entries (`scribe-server`, `scribe-settings`, `scribe-common`). Do not introduce new top-level sections; add `[[…]]` references to the existing entries.
- [X] T027 Run `lat check` from the repository root. Resolve any reported broken refs, missing leading paragraphs, or over-long leading paragraphs reported in `lat.md/` files touched in T023–T026.
- [X] T028 Run `cargo fmt --all`, `cargo clippy --workspace -- -D warnings`, and `cargo test -p scribe-common -p scribe-server -p scribe-settings`. Resolve any failures by fixing root causes (no `#[allow(...)]` escape hatches per the project's no-new-lint-suppressions hook).
- [X] T029 Walk the validation checklist in `specs/001-releases-page/quickstart.md` (every checkbox: SC-001 through SC-005, FR-005, FR-006, FR-014, FR-019, FR-020, the four `cargo`/`lat` clean-runs, and the no-new-lint-suppressions check). Document the results inline in this file by ticking each checkbox; if any item fails, file a follow-up before marking the feature complete.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1, T001)** — no dependencies; can start immediately.
- **Foundational (Phase 2, T002)** — depends on T001 (the new `pulldown-cmark` / `ammonia` deps need to be present so the rest of the work compiles when added).
- **User Story 1 (Phase 3, T003–T015)** — depends on Foundational (T002) for the shared HTTP client. Tests T003–T007 should be written first to fail (TDD), then implementations T008–T015 land in dependency order.
- **User Story 2 (Phase 4, T016–T019)** — does **NOT** depend on Foundational (T002). May start immediately after Setup. Independent of User Story 1; only friction is the shared files (`settings.html`, `settings.js`, `lib.rs`).
- **User Story 3 (Phase 5, T020–T022)** — depends on User Story 1 implementation (T009 for the cache state machine, T015 for the JS render path being in place to extend).
- **Polish (Phase 6, T023–T029)** — depends on at least User Story 1 and User Story 2 being complete. T023–T026 may begin once their target feature areas are in place; T027–T029 are end-of-feature gates.

### User Story Dependencies

- **US1 (P1) — Browse releases**: depends on T001, T002. No dependency on US2 or US3.
- **US2 (P2) — Footer version**: depends on T001 only (it does not touch the protocol, server, or cache; it only needs the workspace to compile). No dependency on US1 or US3.
- **US3 (P3) — Failure UX**: depends on US1's server-side cache state machine (T009) and the US1 JS render path (T015). Cannot start until US1 is at least mid-implementation.

### Within Each User Story

- Tests are written first; ensure they FAIL before running the corresponding implementation tasks.
- For US1: wire types (T008) → server cache (T009) → server dispatch (T010) → settings IPC (T011) and HTML/CSS (T012, T013) in parallel → settings host IPC handlers (T014) → JS render (T015).
- For US2: HTML placeholder (T017) and JS handler (T019) can run in parallel with each other and with the test (T016); the lib.rs implementation (T018) is the single sequencing point.
- For US3: test (T020) first, then JS render extension (T021), then CSS variants (T022).

### Parallel Opportunities

- T003, T004, T005, T006, T007 — all five US1 tests are independent files and can be authored in parallel.
- T011 (server_action.rs), T012 (settings.html), T013 (settings.css) — can be authored in parallel after T008 lands.
- T016 (lib.rs test), T017 (settings.html), T019 (settings.js) — all three US2 tasks across three different files can be authored in parallel; T018 then lands lib.rs in sequence with T016.
- T023, T024, T025, T026 — all four `lat.md/` updates are different files and can be authored in parallel.

---

## Parallel Example: User Story 1 (after T002 lands)

```bash
# Author all US1 tests in parallel:
Task: "T003: serde round-trip tests in crates/scribe-common/src/protocol.rs test module"
Task: "T004: markdown→sanitize pipeline tests in crates/scribe-server/src/releases.rs test module"
Task: "T005: ReleaseCatalog state-machine tests in crates/scribe-server/src/releases.rs test module"
Task: "T006: request_release_list transport-failure test in crates/scribe-settings/src/server_action.rs test module"
Task: "T007: open_external_url scheme-validation test in crates/scribe-settings/src/lib.rs test module"

# After T008 (wire types) lands, author these in parallel:
Task: "T011: request_release_list in crates/scribe-settings/src/server_action.rs"
Task: "T012: settings.html sidebar nav + panel shell"
Task: "T013: settings.css styles for the panel"

# Then T009 → T010 → T014 → T015 in sequence.
```

## Parallel Example: User Story 2 (independent of US1)

```bash
# Author all three US2 tasks in parallel (different files, independent):
Task: "T016: bootstrap snapshot test in crates/scribe-settings/src/lib.rs test module"
Task: "T017: footer placeholder swap in crates/scribe-settings/src/assets/settings.html"
Task: "T019: DOMContentLoaded handler in crates/scribe-settings/src/assets/settings.js"

# Then T018: pre-page-load script injection in crates/scribe-settings/src/lib.rs (sequential after T016).
```

## Parallel Example: Phase 6 Polish

```bash
# All four lat.md updates in parallel (different files):
Task: "T023: lat.md/settings.md updates"
Task: "T024: lat.md/server.md updates"
Task: "T025: lat.md/protocol.md updates"
Task: "T026: lat.md/architecture.md updates"

# Then T027 (lat check), T028 (cargo gates), T029 (validation walk) in sequence.
```

---

## Implementation Strategy

### Suggested order (minimizes shared-file friction)

1. **Phase 1 — Setup (T001)**: dep additions only.
2. **Phase 2 — Foundational (T002)**: shared HTTP-client extraction.
3. **Phase 4 — User Story 2 first (T016 → T017 / T019 in parallel → T018)**: smallest scope, immediate user-visible win, touches only three files. Ship and validate before tackling US1's larger surface.
4. **Phase 3 — User Story 1**: implement in the order dictated by the dependency graph above. Tests first (T003–T007) → wire types (T008) → server side (T009 → T010) → settings host (T011 → T014) → webview (T012 / T013 / T015).
5. **Phase 5 — User Story 3 (T020 → T021 → T022)**: bolt the failure-state UX onto the working US1 panel; the cache-state-machine code already exists from T009.
6. **Phase 6 — Polish (T023–T029)**: knowledge-graph updates and the full gate sweep.

### MVP Scope

User Story 1 alone is the MVP if we want a single-shot release with the core feature. User Story 2 is small enough that it usually ships in the same milestone with no impact on US1.

### Parallel Team Strategy

If multiple developers are available:

- Developer A: Setup + Foundational, then User Story 1 server side (T002, T008, T009, T010).
- Developer B: User Story 2 end-to-end (T016–T019).
- Developer C: User Story 1 webview side (T012, T013, T015) once T008 has landed (the JS needs the wire types stable for the IPC payload shape).
- All three converge on Polish (T023–T029).

---

## Notes

- **Tests origin**: every test task above is justified by a specific test obligation enumerated in `research.md` §R10 or `contracts/releases-protocol.md` §3. None are speculative.
- **No new lint suppressions**: the project's pre-commit hook denies new `#[allow(...)]` attributes. If a new lint trips, fix the underlying code rather than silencing it.
- **No premature features**: do not implement pagination, on-disk caching, channel-based filtering, or anything else listed in the spec's Assumptions / out-of-scope. They are deliberately deferred.
- **Restart discipline**: Per `CLAUDE.md`, do NOT restart `scribe-server` (e.g. `just restart-server`, `scribe-server --upgrade`) without explicit user approval. Iterate against a running server, or build into a separate test binary, until you have approval.
- **Commits**: commit after each task or each cohesive group (e.g. all of US1's tests, then T008, then T009+T010 together). Do not batch the entire feature into a single commit.
- **Stop checkpoints**: at the checkpoint at the end of each phase, validate the story works on its own before moving forward.
