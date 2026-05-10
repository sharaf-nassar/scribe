# Phase 0 Research: Releases Page

**Feature**: 001-releases-page
**Date**: 2026-05-09
**Spec**: [spec.md](./spec.md) | **Plan**: [plan.md](./plan.md)

This phase resolves every "NEEDS CLARIFICATION" / open technical question in the plan's Technical Context by recording a concrete decision, the rationale, and the alternatives that were considered and rejected. The plan is built directly on these decisions.

## R1. Markdown rendering for release notes

- **Decision**: Server-side rendering with `pulldown-cmark` (with the `html` feature) for CommonMark + GFM tables/strikethrough/tasklists/footnotes, then sanitized through `ammonia` before crossing the IPC boundary to the settings webview.
- **Rationale**:
  - The `scribe-settings` webview is constrained by FR-017 to inlined vanilla HTML/CSS/JS with no JS framework or third-party UI library. A JS markdown library is exactly the kind of dependency that constraint is meant to keep out.
  - `pulldown-cmark` is the de-facto Rust CommonMark parser, has no async runtime requirement, is a single fast pull-style parser, and is already a transitive dependency in many Rust ecosystems (small marginal cost).
  - GitHub release notes use GFM-flavored markdown (tables, task lists, fenced code, autolinks). `pulldown-cmark` covers the parts that matter for release notes; missing GitHub-specific extensions (e.g. issue-number autolinks, @mentions) are nice-to-haves explicitly listed as out-of-scope in the spec's Assumptions.
  - Pre-rendering on the server keeps the webview free of untrusted markdown parsing logic and lets us sanitize once in Rust rather than relying on browser-side defenses.
  - `ammonia` is the standard Rust HTML sanitizer (whitelist-based), maintained, and explicitly designed for the "render markdown to HTML, then sanitize" pipeline.
- **Alternatives considered**:
  - `comrak`: feature-richer (full GFM, including GitHub-style autolinking and footnotes) but pulls in more dependencies and a heavier API surface than this feature needs. Reject — `pulldown-cmark` is sufficient.
  - JS markdown library (`marked`, `markdown-it`) inlined into the webview: would violate FR-017 (no third-party UI library); also pushes XSS-relevant logic to the JS side. Reject.
  - Hand-rolled minimal markdown renderer: avoids dependencies but is high-risk for security (escape correctness) and behavior (matching GitHub flavor). Reject.

## R2. Source of truth for the Scribe version shown in the sidebar footer

- **Decision**: At settings-binary build time, read `env!("CARGO_PKG_VERSION")` and inject it into the webview's initial state as `scribe_version`. The JS sets the `#sidebar-footer` text content to `Scribe v${scribe_version}` on load.
- **Rationale**:
  - All Scribe crates already use `version.workspace = true` in their `Cargo.toml` (verified in `crates/scribe-settings/Cargo.toml`), so `CARGO_PKG_VERSION` in the settings binary is automatically the workspace version. Bumping the workspace version updates the settings binary's footer with no source edits to the UI.
  - Settings binary and server are released in lockstep (spec Assumption); using the settings binary's compile-time version is therefore equivalent to "the running build's version" from the user's perspective.
  - Compile-time injection satisfies FR-016: there is no runtime path that can fail to determine the version, so no fallback branch is needed in practice (the fallback exists only as defense-in-depth).
- **Alternatives considered**:
  - Query the running `scribe-server` for its version over the existing IPC: introduces a synchronous network round-trip to render static chrome, and a failure mode (server not yet ready) for what is fundamentally a static value. Reject.
  - Replace the literal `v0.1.0` in `settings.html` with a build-script string substitution: works, but conflates build wiring with HTML, fragile if assets get regenerated. Reject in favor of a single JS-side write from the injected initial state.
  - Read the running scribe-server binary's version off disk: brittle and adds a filesystem read to the settings startup path. Reject.

## R3. GitHub API endpoint and release-list bound

- **Decision**: Fetch from `https://api.github.com/repos/sharaf-nassar/scribe/releases?per_page=30` (the multi-release list endpoint). Filter out drafts. Keep pre-releases. Cap displayed count at 30 (matches GitHub's default page size; satisfies FR-018's "30 most recent non-draft releases" requirement).
- **Rationale**:
  - The existing updater hits `/releases/latest` (verified at `crates/scribe-server/src/updater.rs:24`); the "browse history" use case requires the list endpoint.
  - `per_page=30` is GitHub's default and is sufficient for the project's release cadence; pagination beyond the first page is out of scope (spec assumption: deep historical archaeology is not required).
  - Drafts are repository-internal; filtering them out matches the existing updater logic and avoids leaking unpublished work.
  - Pre-releases are kept (the user explicitly confirmed this scope) and are distinguished by the `prerelease: true` flag carried through to the UI.
- **Alternatives considered**:
  - Authenticated requests for higher rate limits: requires credential storage, key management, user setup. Reject for v1; the unauthenticated 60/hr rate limit is adequate for a desktop app that opens the page occasionally and benefits from the in-memory cache.
  - Pagination / "load more" UX: out of scope for v1. Reject.
  - Per-channel filtering on the server side: rejected by user during spec scoping (channel filtering applies only to auto-updater).

## R4. Release-list cache strategy

- **Decision**: In-memory cache on `scribe-server` keyed by `()` (single global), holding `(timestamp_of_last_successful_fetch, list_of_renderables, freshness_indicator)`. TTL 1 hour. On each `ClientMessage::ListReleases`:
  1. If a cached value exists and is younger than the TTL, return it immediately tagged `Fresh`.
  2. If a cached value exists but is older, kick off a background refresh and return the cached value tagged `Stale`. The next `ListReleases` will pick up the refreshed data if the refresh succeeded.
  3. If no cached value exists, perform a synchronous fetch with a sensible timeout (5 s, matching SC-003); on success return `Fresh`; on failure return `Failed { reason }` and do not populate the cache.
- **Rationale**:
  - One hour balances "GitHub rate limit safety" (60 unauth req/hr is plenty if cache holds) against "user opens settings after a release" responsiveness. Most users will see fresh data without ever hitting GitHub for already-running sessions.
  - Stale-while-revalidate gives FR-013 the right behavior automatically: if a refresh fails the previous data stays visible with the `Stale` indicator the UI can surface.
  - In-memory only matches the spec's "out of scope: persisting cache across restarts."
  - A single global cache (not per-client) is correct because the data is identical for everyone and `scribe-server` is a singleton process per user.
- **Alternatives considered**:
  - On-disk JSON cache: would help cold-start offline scenarios but adds storage management, schema migration, and stale-on-disk concerns. Out of scope per spec. Reject.
  - Push-on-change subscription: requires server-initiated messages and a longer-lived connection. The settings binary already uses a strict request/response one-shot pattern. Reject.
  - No cache (always refetch): would blow the unauthenticated rate limit and slow every settings panel open. Reject.

## R5. IPC shape for releases

- **Decision**: Two new variants on the existing protocol enums:
  - `ClientMessage::ListReleases` — no payload; the server returns whatever its current cache holds.
  - `ServerMessage::ReleaseList { state: ReleaseListResultState }` — where `ReleaseListResultState` is an enum mirroring `UpdateCheckResultState`'s shape (`Fresh { releases: Vec<Release> }`, `Stale { releases: Vec<Release>, reason: String }`, `Failed { reason: String }`).
  - Each `Release` carries the version string, name, publish date, the rendered+sanitized HTML body, the prerelease flag, and the canonical GitHub URL.
- **Rationale**:
  - This is the exact shape of the existing manual update-check IPC (`request_update_check` in `server_action.rs`), so there is one obvious pattern to follow and no new framing primitives to design.
  - Returning rendered HTML (not raw markdown) over the wire keeps the webview free of markdown logic and lets us put the trust boundary at the server.
  - Carrying `releases: Vec<Release>` even in the `Stale` state is what FR-013 requires (show previous data plus a stale indicator).
- **Alternatives considered**:
  - Two separate `ClientMessage` variants for "list" vs "fetch single release body": rejected — doubles round-trips and complicates caching for a feature where the entire list+bodies is naturally fetched together.
  - Returning raw markdown bodies and rendering in the webview: rejected as it conflicts with FR-017 and the markdown-renderer decision in R1.

## R6. Webview ↔ host (settings binary) IPC for opening external links

- **Decision**: When a user clicks a link inside rendered release notes, the webview's JS captures the click and posts `{ "type": "open_external_url", "url": "<absolute URL>" }` over the existing `window.ipc.postMessage` channel. The settings host platform-dispatches: on Linux, `xdg-open <url>` via `std::process::Command`; on macOS / non-Linux (`tao` build), `open <url>`.
- **Rationale**:
  - The existing webview already uses `window.ipc.postMessage(JSON.stringify(message))` for setting changes (verified in `crates/scribe-settings/src/assets/settings.js:6-8`). Extending it with one more message type matches the existing pattern.
  - Capturing on the JS side prevents the embedded webview from navigating off the settings page (which would break the panel until the user closes and reopens settings).
  - Using `xdg-open` / `open` matches platform convention and reuses the OS's chosen browser.
- **Alternatives considered**:
  - Setting `<a target="_blank">` and relying on the webview to delegate: behavior across `wry` on GTK vs `tao` is not guaranteed to open externally; the webview may either swallow it or open a popup that we cannot easily close. Reject.
  - Using the `webbrowser` crate: an extra dependency for a one-line `xdg-open` / `open` invocation. Reject.

## R7. Bringing GitHub HTTP client to a shared place

- **Decision**: Refactor `crates/scribe-server/src/updater.rs` minimally: extract a small helper that returns a configured `reqwest::Client` (currently the updater builds one internally for `fetch_latest_release`) into a shared function in the same crate, and reuse it from the new `crates/scribe-server/src/releases.rs`. Do not change the existing `fetch_latest_release` signature or behavior.
- **Rationale**:
  - Avoids two divergent HTTP-client configurations in the same process (User-Agent, timeouts, TLS settings).
  - Keeps the diff focused: the updater module's existing logic and message handling stays put. The new module is purely additive.
  - Stays within `scribe-server`, so the workspace shape doesn't change.
- **Alternatives considered**:
  - Promote the HTTP client to `scribe-common`: tempting for symmetry, but `scribe-common` does not currently depend on `reqwest` (verified — settings binary uses sync sockets specifically because `scribe-common` is runtime-agnostic). Pulling `reqwest` into `scribe-common` would force the settings crate to compile a Tokio-flavored HTTP client into a no-Tokio binary. Reject.
  - Keep two separate `reqwest::Client` instances (one in the updater, one in the new releases module): minor duplication, but configuration drift becomes a real risk over time. Reject.

## R8. Webview asset bootstrap: passing the version into JS

- **Decision**: At `lib.rs` webview initialization, build a small JS bootstrap snippet of the form `window.SCRIBE_BOOTSTRAP = { version: "x.y.z", platform: "linux" };` using `env!("CARGO_PKG_VERSION")` and the existing platform string, and inject it via the webview's pre-page-load script hook. `settings.js` reads `window.SCRIBE_BOOTSTRAP.version` once on `DOMContentLoaded` and writes it into `#sidebar-footer`.
- **Rationale**:
  - `wry` and the GTK fallback both expose a "run this script before page loads" mechanism, which is the right place to drop a single `window.X = {...}` literal. This is the standard pattern for handing static config from a Rust webview host to inlined JS.
  - Keeps version handling out of the IPC path entirely (no round-trip needed for chrome).
  - The same bootstrap can carry future static config without inventing a new mechanism.
- **Alternatives considered**:
  - HTML string substitution at build time: works but couples the build pipeline to the asset, requires a `build.rs`, and introduces a place where assets diverge from on-disk source. Reject.
  - Asset preprocessing at runtime (read the HTML, replace a token, serve the result): same downsides as above, plus a startup cost. Reject.
  - Sending the version via `setting_init` IPC at startup: works, but requires an IPC round-trip for static chrome. Reject.

## R9. Date and version formatting

- **Decision**: Render publish dates as `YYYY-MM-DD` (UTC). Render version strings exactly as the GitHub `tag_name` minus a leading `v`, falling back to the raw tag if it does not start with `v`. The existing updater already uses `tag_name.trim_start_matches('v')` for parsing (verified in `crates/scribe-server/src/updater.rs:447`), and we adopt the same convention for display.
- **Rationale**:
  - `YYYY-MM-DD` is unambiguous and locale-free; the settings UI does not currently localize dates anywhere.
  - Stripping the `v` keeps the displayed string consistent with how the existing updater talks about versions in user-visible messages.
- **Alternatives considered**:
  - "X days ago" relative time: requires periodic re-rendering, locale considerations, and a date library on the JS side. Reject.
  - Locale-aware date formatting: out of scope; nothing else in the settings UI is localized today. Reject.

## R10. Testing strategy

- **Decision**:
  - Unit tests in `crates/scribe-server/src/releases.rs` for the cache state machine (Fresh / Stale / Failed transitions, TTL boundary, missing-cache fallback).
  - Unit tests for markdown→HTML→sanitized-HTML pipeline against a small fixture of representative release-note bodies (plain text, code blocks, lists, links, tables, an attempted `<script>` injection that the sanitizer must strip).
  - Unit tests for `Release` and `ReleaseListResultState` serde round-trips through msgpack to catch protocol-shape regressions (matches existing patterns for `UpdateCheckResultState`).
  - One integration-style test that wires `request_release_list` against an in-process server instance over a temp Unix socket, asserting the success and failure response shapes — mirrors how the manual update-check is exercised today.
  - Manual UX validation of the rendered page (side-by-side with existing panels for visual consistency, offline behavior, click-through to external browser) — matches how the rest of the settings UI is verified today.
- **Rationale**: covers every layer where the new code introduces a state machine or a trust boundary; respects the project's existing testing depth (no JS test runner exists).
- **Alternatives considered**:
  - Pulling in a JS test runner to cover the webview JS: mismatch with the rest of the codebase, additional toolchain. Reject for v1.
  - End-to-end test that drives the actual webview: high cost to set up; the rest of the codebase does not do this. Reject for v1.

## R11. Releases panel layout

- **Decision**: Single-column content area driven by two coordinated controls in a panel header row: a **version picker** (a native `<select>`-style dropdown listing each available release as `vX.Y.Z — YYYY-MM-DD` with an inline "PRE" badge for pre-releases) and a pair of **Newer / Older navigation buttons** flanking it. The buttons step the displayed release one position newer or older respectively; both controls write to the same `selectedReleaseVersion` state and re-render the content area. Buttons disable visibly at the boundaries (oldest/newest in the available list). No master-detail two-pane and no accordion.
- **Rationale**:
  - User explicitly requested this pattern at the end of `/speckit-plan`. Captured here so future readers see the decision and not just the implementation.
  - Compact: gives the maximum vertical space to the rendered release notes, which is the content the user came to read.
  - Browseable: the Newer / Older buttons let users walk through versions linearly without re-opening the picker every time, addressing the main weakness of a dropdown-only design.
  - Both controls can share a single source of truth (`selectedReleaseVersion`); keeping them synchronized is a one-state-write update on each interaction. No complex coordination logic.
  - Disabled boundary states are standard, learnable, and avoid the "what does Next do at the end?" ambiguity that wrap-around would introduce.
- **Specifics**:
  - Header row layout (left to right): `[<- Older]` `[ Version picker ▾ ]` `[Newer ->]` on the left of the panel header; `[View on GitHub]` aligned to the right edge.
  - Button labels use direction words tied to release recency, not list position, to avoid the `Prev` vs `Next` ambiguity (Prev-in-list-order vs Prev-in-version-order can confuse users). Concretely: the left button steps to the **older** release, the right button steps to the **newer** release. Icons paired with text labels (or accessible names) keep the meaning explicit. Final wording is implementation-detail and may be tuned during UI build, as long as direction is unambiguous.
  - The buttons reuse the existing settings-button styles (sharp corners, dark-theme tokens) and gain a `disabled` style that is also reused from existing settings controls — no new visual primitives.
  - Boundary disable rules:
    - "Newer" disabled iff `selectedReleaseVersion` equals the first item in the available list.
    - "Older" disabled iff `selectedReleaseVersion` equals the last item in the available list.
  - Wrap-around is explicitly disallowed.
- **Alternatives considered**:
  - Two-pane master-detail (left list + right notes): more browseable for very long lists, but spends ~30–40% of horizontal real estate on a list that the user mostly doesn't need to keep visible once they've picked. Rejected by user.
  - Vertical accordion (single column, click to expand): poor for the "I want to read these notes thoroughly" use case because only one item is fully visible at a time and the act of expanding moves content out of view. Rejected by user.
  - Wrap-around at boundaries (clicking Newer at the newest jumps to the oldest, and vice versa): would let the user round-trip with one click, but introduces "did I jump or step?" ambiguity and turns boundary clicks into a surprise. Rejected.
  - Keyboard arrow shortcuts that step the selection (Left/Right or `[`/`]`) when the panel is focused: nice-to-have, easy to add later if the buttons prove popular. Out of scope for v1 to keep the surface area small.

## Summary

All technical context fields in the plan are now backed by a concrete decision recorded above. There are no remaining "NEEDS CLARIFICATION" markers. The layout pattern was confirmed by the user at the end of `/speckit-plan` (see R11). The plan is cleared to proceed to `/speckit-tasks`.
