# Quickstart: Implementing the Releases Page

**Feature**: 001-releases-page
**Date**: 2026-05-09
**Audience**: a developer (or future agent) picking up the implementation work for this feature.

This document is a sequenced walkthrough. Following it top-to-bottom produces a working Releases page in the settings window plus an accurate sidebar-footer version, in roughly the same order tasks.md will land things. Read [spec.md](./spec.md), [plan.md](./plan.md), [research.md](./research.md), and [contracts/releases-protocol.md](./contracts/releases-protocol.md) first; this file does not re-derive their decisions, it consumes them.

## Goal recap

When done:

1. The settings sidebar has a new "Releases" entry (alongside the existing Updates entry).
2. Clicking it shows a two-pane layout: a left list of recent Scribe releases (newest first, pre-releases badged) and a right pane of the selected release's notes rendered as readable formatted text.
3. The page degrades gracefully on offline / rate-limited / parse-error scenarios.
4. The sidebar footer reads `Scribe v<actual workspace version>` in every build, with no source edit needed when the version is bumped.

## Prerequisites

- `cargo build` succeeds on `main` before you start (sanity).
- You can run `just restart-server` only after explicit user approval (see project `CLAUDE.md`). Do not restart the running server unprompted while iterating.
- You have a checkout of the `001-releases-page` branch (created by `/speckit-git-feature`).
- `lat search` and `lat check` work in your shell.

## Phase A — Server-side data path

This phase puts the data-producing infrastructure on `scribe-server` and on the wire.

### A1. Add the wire types in `scribe-common`

In `crates/scribe-common/src/protocol.rs`, add the new types alongside the existing enums (no removals, no reorderings of existing variants — order matters for human readability and grep hits, not for serde-named, but we keep the file tidy):

- `pub struct Release { … }` matching the data-model spec exactly. Use `#[serde(rename_all = "snake_case")]` if the rest of the file does (verify), otherwise plain `Serialize, Deserialize`.
- `pub enum ReleaseListResultState { Fresh { releases: Vec<Release> }, Stale { releases: Vec<Release>, reason: String }, Failed { reason: String } }`. Match the existing `UpdateCheckResultState`'s serde attributes line-for-line so the wire format is consistent.
- Append `ListReleases` to `ClientMessage` and `ReleaseList { state: ReleaseListResultState }` to `ServerMessage` exactly where similar items currently sit.

Write serde round-trip tests now (one per type and one for each enum variant), in line with research R10. Drive tests with msgpack-named encoding to match the running framing.

### A2. Implement the `ReleaseCatalog` cache and handler

Create `crates/scribe-server/src/releases.rs`. Wire it into the server module tree (`mod releases;` in the appropriate parent).

The module owns:

- A `ReleaseCatalog` struct as defined in `data-model.md`.
- An async fetcher that calls the GitHub list endpoint (`/releases?per_page=30`), reuses the shared `reqwest::Client` from `updater.rs` (extract a small helper there as called out in research R7), and converts the GitHub response into `Vec<Release>`. Inline a single per-release rendering pipeline: `body` → `pulldown-cmark` (CommonMark + GFM features) → `ammonia::clean(...)` → `Release.body_html`. Drop drafts. Keep pre-releases. Cap at 30.
- A handler `pub async fn handle_list_releases(catalog: &Arc<Mutex<ReleaseCatalog>>) -> ReleaseListResultState` implementing the cache state machine in `data-model.md` (Fresh / Stale / Failed).
- Hook the handler into the existing client-message dispatch pathway alongside `CheckForUpdates` (find the dispatcher that routes `ClientMessage::CheckForUpdates` and add the new arm next to it).

Tests in this phase (research R10):

- Unit tests for the cache state machine: `Fresh` while in TTL, transitions to `Stale` past TTL with intact `releases`, transitions to `Failed` only when no cache exists, no thundering-herd on concurrent calls.
- Unit tests for the markdown-render-and-sanitize pipeline against fixtures: plain text, fenced code, list, link, table, and a `<script>` injection attempt that must be stripped.

### A3. Wire the new dependencies

In `crates/scribe-server/Cargo.toml`, add:

- `pulldown-cmark = "<latest 0.x as of plan date>"` with the appropriate features (CommonMark + GFM extensions).
- `ammonia = "<latest 4.x as of plan date>"`.

Use `find-docs` / Context7 to confirm the current versions and feature flags before committing.

## Phase B — Settings binary

### B1. Add `request_release_list` to `server_action.rs`

In `crates/scribe-settings/src/server_action.rs`, add a sibling of `request_update_check`:

```rust
pub fn request_release_list(timeout: Duration) -> ReleaseListResultState { /* … */ }
```

It must:

- Open a new one-shot `UnixStream` (do not reuse the update-check connection).
- Apply read/write timeouts identically to `request_update_check`.
- Send `ClientMessage::ListReleases` via `write_frame`.
- Read one `ServerMessage::ReleaseList { state }` via `read_frame` and return `state`.
- Convert any transport or unexpected-message error into `ReleaseListResultState::Failed { reason }` so the UI has a single shape to handle.

Add a test that simulates a closed-peer-mid-frame condition and asserts the `Failed` mapping.

### B2. Bootstrap `window.SCRIBE_BOOTSTRAP` and add the host-side IPC handler

In `crates/scribe-settings/src/lib.rs`:

- At webview construction, add a pre-page-load script that defines `window.SCRIBE_BOOTSTRAP = { version: "<env!(\"CARGO_PKG_VERSION\")>", platform: "<linux|macos|other>" }`. Use a JSON-safe escape; do not interpolate raw strings.
- In the existing `window.ipc.postMessage` dispatch, add two arms:
  - `"request_releases"` → spawn a thread (the settings binary has no Tokio; use a plain `std::thread::spawn`) that calls `request_release_list(...)` and on completion calls the webview's `evaluate_script("window.SCRIBE_ON_RELEASE_LIST(<json>)")` with the result. Pick a sensible timeout (e.g. 7 s) so the JS-side loading state never hangs.
  - `"open_external_url"` → validate the `url` field starts with `http://` or `https://`, then `Command::new("xdg-open" | "open").arg(url).spawn()` per platform. Discard non-http(s) URLs silently (and `tracing::warn!` for observability).

## Phase C — Webview UI

### C1. HTML

In `crates/scribe-settings/src/assets/settings.html`:

- Insert the new sidebar entry between the existing "Updates" and "Notifications" entries:

  ```html
  <div class="nav-item" data-tab="releases">
    <span class="nav-icon">…</span>
    <span class="nav-label">Releases</span>
  </div>
  ```

  Use an icon consistent with the rest of the sidebar (the existing entries follow a pattern; reuse it rather than introducing a new icon style).

- Add an empty `<section id="releases-panel" class="panel">` after the existing panels with the dropdown + Newer/Older + single-content-area skeleton (research R11):

  ```html
  <section id="releases-panel" class="panel">
    <header class="panel-header">
      <div class="releases-nav">
        <button type="button" class="releases-nav-btn" id="releases-older"
                aria-label="Older release" disabled>‹ Older</button>
        <select id="releases-picker" class="releases-picker"
                aria-label="Select Scribe release"></select>
        <button type="button" class="releases-nav-btn" id="releases-newer"
                aria-label="Newer release" disabled>Newer ›</button>
      </div>
      <a class="releases-external" id="releases-external" href="#"
         data-external>View on GitHub</a>
    </header>
    <article class="release-notes" id="release-notes" aria-live="polite"></article>
    <div class="releases-status" id="releases-status" hidden></div>
  </section>
  ```

  Notes:
  - The two `<button>` elements both start `disabled`; `settings.js` flips the `disabled` attribute as the selection moves.
  - The `<select>` is intentionally a native control (no custom dropdown component) per FR-017's no-third-party-UI constraint.
  - `data-external` on the GitHub link is the same delegation hook the JS uses to forward link clicks to `open_external_url` (see C3 below).

- Replace the hardcoded footer:

  ```html
  <!-- before -->
  <div class="sidebar-footer">Scribe v0.1.0</div>
  <!-- after -->
  <div class="sidebar-footer" id="sidebar-footer"></div>
  ```

### C2. CSS

In `crates/scribe-settings/src/assets/settings.css`, add styles for:

- `.releases-nav` — the flex row that holds `[Older]` `[picker]` `[Newer]`. Tight gap, vertically centered, aligned to the panel-header start.
- `.releases-nav-btn` — sharp-cornered button reusing the existing settings-button color tokens. A `:disabled` rule that lowers contrast (no separate disabled color token; reuse the existing one elsewhere in `settings.css`) and disables the cursor.
- `.releases-picker` — native `<select>` styled to match other settings inputs (matching height, border, background). Include an option-level affordance for the pre-release marker (e.g. an inline `[PRE]` text prefix in the option text — research R11 explicitly avoids icon-in-option since native `<select>` cannot render arbitrary HTML).
- `.releases-external` — the "View on GitHub" link, right-aligned in the header.
- `.release-notes` — the content area. Reuse existing settings-panel font sizes for headings (`<h1>`–`<h3>`), code (`<code>`/`<pre>`), and links — do not redefine them.
- `.releases-status` — the loading/error/stale banner at the bottom of the panel. Three variants distinguished by a class (`is-loading`, `is-error`, `is-stale`).
- `.pre-release-badge` — only used inside `.release-notes` header to mark a pre-release release with a small label.

Pull every color and spacing token from the existing CSS variables — do not introduce new ones. Keep corners sharp (≤ 6px radius) and avoid glassmorphism per FR-017.

### C3. JS

In `crates/scribe-settings/src/assets/settings.js`:

- Read `window.SCRIBE_BOOTSTRAP.version` on `DOMContentLoaded` and write `Scribe v${version}` into `#sidebar-footer`. If `version` is missing or falsy, write just `Scribe`.
- In the existing tab-activation handler, when the user activates `data-tab="releases"`:
  - If `releases` JS state is empty, post `{ "type": "request_releases" }` and switch the panel into `loading` UI.
  - Otherwise, simply re-render from existing state (no IPC).
- Define `window.SCRIBE_ON_RELEASE_LIST = function(payload) { … }` early (before tab activation can fire) to receive the host's response and update state.
- Maintain a single `selectedReleaseVersion` JS state key. Both the `<select>` `change` handler and the Newer/Older button `click` handlers write to this key and call a single `renderRelease()` helper. The first item is auto-selected on initial population (FR-008).
- Implement `updateNavBoundaries()` that runs every time `selectedReleaseVersion` changes:
  - `#releases-newer` — set `disabled = (currentIndex === 0)`.
  - `#releases-older` — set `disabled = (currentIndex === releases.length - 1)`.
  - This single function satisfies both FR-019 (boundary disable) and FR-020 (controls stay in sync), because it is the only place that mutates the buttons' state and the `<select>`'s value is always set to the same `selectedReleaseVersion`.
- Wire the Newer/Older buttons:
  - `#releases-older` `click` → if not `disabled`, set `selectedReleaseVersion = releases[currentIndex + 1].version` and re-render.
  - `#releases-newer` `click` → if not `disabled`, set `selectedReleaseVersion = releases[currentIndex - 1].version` and re-render.
  - The `disabled` check is defense-in-depth; the attribute already prevents click events on disabled buttons in standards-compliant browsers, but explicitly checking keeps the behavior correct under any unusual focus/event-dispatch path.
- Capture link clicks inside `.release-notes a` (event delegation on the panel root). Call `event.preventDefault()` and post `{ "type": "open_external_url", "url": e.target.href }`. Apply the same delegation to `[data-external]` (the panel-header GitHub link) using the release's `html_url`.

## Phase D — Sidebar footer correctness check

The footer is technically already wired in C1 + C3, but it deserves a separate validation pass:

- Build at the current workspace version and confirm the footer matches.
- Bump `Cargo.toml`'s workspace version locally to a synthetic value (e.g. `9.9.9`), rebuild only `scribe-settings`, launch, and confirm the footer reads `Scribe v9.9.9`. Revert the version bump.
- Confirm SC-004's three-bump test passes mentally: any future legitimate version bump propagates without an HTML edit.

## Phase E — `lat.md/` updates

Per `CLAUDE.md`'s post-task checklist, the project's lat.md graph must reflect the new architecture. Touch these files (already enumerated in the plan's project structure):

- `lat.md/settings.md`: add a "Releases" panel section and a "Sidebar Footer" section, both with leading paragraphs (≤ 250 chars). Cross-link to the source files via `[[…]]` source-code refs.
- `lat.md/server.md`: add a "Releases / Release Catalog" section under Server documenting the cache state machine and the GitHub list-endpoint usage.
- `lat.md/protocol.md`: add entries for `ListReleases` and `ReleaseList { state }` under Client Messages and Server Messages, mirroring the existing Update Control / Update Notifications subsections.
- `lat.md/architecture.md`: cross-link from the existing crate-map entries.

Run `lat check` and confirm zero broken refs before considering the work done.

## Validation checklist (gate to closing the feature)

A feature gate equivalent to the spec's success criteria. Walk it before requesting review.

- [ ] **SC-001**: open settings → click Releases → first release notes render within 5 s on broadband. <!-- needs manual GUI check; code paths in place: T012/T015 wire the tab activation to `request_releases`, T009 fetcher caps at 30 releases per_page so first paint is bounded; T014 uses a 7 s upper-bound timeout for failure detection so successful fetches comfortably hit the 5 s SC -->
- [ ] **SC-002**: changing the displayed release through either the version picker or the Newer / Older buttons updates the content area in under 200 ms with no network call (verify in `tcpdump` / browser-style network tab equivalent, or by toggling network mid-session). <!-- needs manual GUI check; T015 picker change and button click handlers call `renderRelease()` synchronously off the in-memory `releases` JS state — no IPC is posted on selection change -->
- [ ] **FR-019 / FR-020**: at the boundaries the corresponding nav button is visibly `disabled` and clicking it does nothing; selecting a release through the picker visibly updates the buttons' disabled state, and stepping through with the buttons updates the picker's selected option. <!-- needs manual GUI check; T015 `updateNavBoundaries()` is the single source of truth for both the disabled attribute and picker selection -->
- [ ] **SC-003**: with network disabled, the panel reaches a non-blocking state (Stale-with-data or Failed-with-retry) within 5 s; rest of settings remains usable. <!-- needs manual GUI check; T009 + T020 cover Stale-while-revalidate, T021 + T022 render the failed/stale variants and Retry/Refresh buttons -->
- [ ] **SC-004**: footer reads the running workspace version; toggling a synthetic version bump updates the footer with no HTML edit. <!-- needs manual GUI check; T016/T018/T019 wire `env!("CARGO_PKG_VERSION")` → `bootstrap_script` → `#sidebar-footer`; bootstrap tests in `crates/scribe-settings/src/lib.rs` pin the substring -->
- [ ] **SC-005**: side-by-side comparison with the Updates panel shows visual consistency (same fonts, spacing, sharp corners). <!-- needs manual GUI check; T013/T022 CSS reuses existing settings tokens, sharp corners ≤6px, no glassmorphism per FR-017 -->
- [ ] **FR-005**: pre-releases visibly badged. <!-- needs manual GUI check; T015 populates picker options with `[PRE] ` prefix and renders `.pre-release-badge` inside `#release-notes` header when `prerelease=true` -->
- [ ] **FR-006**: drafts never appear. <!-- needs manual verification; `GithubReleaseFetcher::fetch_releases` filters with `!r.draft` in `crates/scribe-server/src/releases.rs` and is covered by the cache-state tests -->
- [ ] **FR-014**: clicking a link in rendered notes opens the OS default browser; webview does not navigate away from settings. <!-- needs manual GUI check; T015 delegates `<a>` clicks inside `#release-notes` to `open_external_url`; T007 unit test pins http(s) scheme validation and rejection of `javascript:` / `file:` / `data:` -->
- [X] `cargo test -p scribe-common -p scribe-server -p scribe-settings` passes. <!-- PASS via T028 — full workspace test run green; releases:: cache, render, transport, scheme, bootstrap suites all pass -->
- [X] `cargo clippy -- -D warnings` clean. <!-- PASS via T028 — `cargo clippy --workspace -- -D warnings` finished with no warnings -->
- [X] `lat check` clean. <!-- PASS via T027 — all wiki links, source-code refs, and leading-paragraph length checks pass -->
- [X] No new entries appear in any `#[allow(...)]` lint suppressions (the project hook enforces this). <!-- PASS — `tools/check-no-new-lint-suppressions.sh --working-tree` exit 0 -->

### T029 walkthrough summary

- PASS-by-gate: `cargo test`, `cargo clippy`, `lat check`, no-new-lint-suppressions (full workspace runs are green; suppression baseline unchanged).
- Needs manual user GUI check: SC-001, SC-002, SC-003, SC-004, SC-005, FR-005, FR-006 (FR-006 is also indirectly covered by the `GithubReleaseFetcher` `!r.draft` filter and the cache-state tests), FR-014, FR-019 / FR-020. All underlying code paths are wired up by tasks T008–T022; the manual gate is the user observing the running GUI behavior end-to-end.

## What `/speckit-tasks` will produce next

Running `/speckit-tasks` after this plan will fan these phases out into individual ordered, dependency-aware tasks. Expect tasks roughly in this order:

1. Add new wire types and serde tests (A1).
2. Add `ReleaseCatalog`, fetcher, render-sanitize pipeline, and unit tests (A2).
3. Wire new server-side dispatch arm + integration test (A2 cont.).
4. Add `pulldown-cmark`, `ammonia` deps (A3).
5. Add `request_release_list` + transport-failure test (B1).
6. Add bootstrap injection + scheme-validated `open_external_url` handler (B2).
7. HTML/CSS/JS for the panel + footer (C1–C3, D).
8. `lat.md/` updates + `lat check` (E).
9. Validation walk-through (checklist above).

Do not pre-empt `/speckit-tasks`; this list is a sanity-check on completeness, not a substitute.
