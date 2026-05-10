# Implementation Plan: Releases Page

**Branch**: `001-releases-page` | **Date**: 2026-05-09 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/001-releases-page/spec.md`

## Summary

Add a new "Releases" page to the Scribe settings window that lets the user browse the project's GitHub release history and read each release's notes as rendered markdown, and replace the hardcoded `Scribe v0.1.0` in the sidebar footer with the actual running version.

Technical approach: extend the existing in-process updater on `scribe-server` to also fetch and cache the multi-release list from the GitHub `/releases` endpoint and to pre-render each release's markdown body to sanitized HTML; expose this through two new `ClientMessage`/`ServerMessage` variants on the existing msgpack-framed protocol; have the `scribe-settings` binary call them via the same one-shot Unix-socket pattern used today for `CheckForUpdates` (`server_action.rs`); render the page from inlined HTML/CSS/JS in `crates/scribe-settings/src/assets/` with no new JS framework; source the sidebar footer's version string at build time from `env!("CARGO_PKG_VERSION")` (the workspace version that all crates share via `version.workspace = true`).

## Technical Context

**Language/Version**: Rust (workspace `rust-version` from `Cargo.toml`); HTML/CSS/vanilla JavaScript for the settings webview UI (no framework).
**Primary Dependencies**: existing — `wry` (webview), `gtk 0.18` (Linux), `tao` (non-Linux), `rmp-serde` (msgpack framing), `reqwest` (HTTP, already used by the updater), `serde`, `tokio` (server only). New — `pulldown-cmark` for server-side CommonMark+GFM rendering, `ammonia` for HTML sanitization of rendered release notes. No new JS dependencies.
**Storage**: in-memory only. The release list and rendered notes live in a per-process cache on `scribe-server` for the lifetime of the running session. No on-disk persistence, no database.
**Testing**: `cargo test` for unit tests; integration-style tests exercise the new protocol round-trip on a loopback Unix socket (mirroring patterns used elsewhere in the codebase). Webview JS continues to be exercised manually as today (the project has no JS test runner).
**Target Platform**: same as Scribe today — Linux (GTK + wry) and macOS/non-Linux (tao + wry). Windows is not currently a Scribe target and is not addressed by this feature.
**Project Type**: desktop app (multi-crate Rust workspace; `scribe-server` background process + `scribe-settings` GUI process + shared `scribe-common`; `scribe-client` is the GPU terminal frontend, not touched by this feature).
**Performance Goals**: opening the Releases page renders the loading state immediately and the first release notes within 5 s on broadband (SC-001); switching between already-fetched releases updates the content area in under 200 ms with no network call (SC-002); offline / failure state appears within 5 s (SC-003).
**Constraints**: the `scribe-settings` process has no Tokio runtime — all GitHub I/O must happen on `scribe-server` and reach settings over the existing sync Unix-socket protocol. The settings webview cannot pull in a JS framework or a third-party UI library. Rendered release-note HTML must be sanitized server-side before it crosses the IPC boundary; the webview must treat it as read-only display content. External links in release notes must be opened via the existing host IPC, not via the embedded webview navigating away.
**Scale/Scope**: list size bounded to the 30 most recent non-draft releases (see FR-018 and research R3). Per-release body size is the GitHub release-body size (typically a few KB; some can be tens of KB). Total release-cache footprint is on the order of a few hundred KB.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

The project constitution at `.specify/memory/constitution.md` is currently the unfilled Spec Kit template: every principle slot still contains `[PRINCIPLE_N_NAME]` / `[PRINCIPLE_N_DESCRIPTION]` placeholders, and no concrete gates have been ratified. There are therefore no project-specific constitutional constraints to evaluate against.

Until the constitution is filled in (via `/speckit-constitution`), this plan applies the project's de-facto standing rules from `CLAUDE.md` and the existing codebase conventions as advisory gates:

- **No new lint suppressions** (project hook). The plan does not require `#[allow(...)]` escape hatches; any lints triggered by new code are to be fixed at the call site, not silenced.
- **`lat.md/` stays in sync** (`CLAUDE.md` post-task checklist). The plan adds explicit `lat.md/` updates for every new architectural concept introduced (settings panel, protocol message, server cache, version source).
- **Reuse existing infrastructure** (user-stated spec constraint, FR-010). The plan reuses the existing `updater.rs` HTTP client, the existing `ClientMessage`/`ServerMessage` framing, and the existing `server_action.rs` sync IPC pattern.
- **No JS framework introduction** (user-stated spec constraint, FR-017). The plan keeps the settings webview on inlined vanilla HTML/CSS/JS and pushes complex rendering (markdown → sanitized HTML) to the Rust server.

No violations to track. The Complexity Tracking table is intentionally empty.

## Project Structure

### Documentation (this feature)

```text
specs/001-releases-page/
├── plan.md              # This file (/speckit-plan command output)
├── spec.md              # Feature specification (already created by /speckit-specify)
├── research.md          # Phase 0 output (this command)
├── data-model.md        # Phase 1 output (this command)
├── quickstart.md        # Phase 1 output (this command)
├── contracts/           # Phase 1 output (this command)
│   └── releases-protocol.md
├── checklists/
│   └── requirements.md  # already created by /speckit-specify
└── tasks.md             # Phase 2 output (/speckit-tasks command — NOT created here)
```

### Source Code (repository root)

The project is an existing single-workspace Rust monorepo. This feature spreads across three existing crates plus the inlined webview assets. No new crate is introduced.

```text
crates/
├── scribe-common/
│   └── src/
│       └── protocol.rs           # MODIFIED: add ClientMessage::ListReleases, ServerMessage::ReleaseList { state: ReleaseListResultState }, plus the Release struct and ReleaseListResultState enum
├── scribe-server/
│   └── src/
│       ├── updater.rs            # MODIFIED: extract reusable GitHub HTTP client + reusable GhRelease deserialiser; keep existing fetch_latest_release intact
│       └── releases.rs           # NEW: ReleaseCatalog cache (recent releases, fetch + render + sanitize + TTL), handler that converts ClientMessage::ListReleases to ServerMessage::ReleaseList
└── scribe-settings/
    ├── Cargo.toml                # MODIFIED: no new deps for the settings binary itself (rendering happens server-side); only the workspace version inheritance is exercised
    └── src/
        ├── server_action.rs      # MODIFIED: add request_release_list(timeout) using the same sync write_frame/read_frame pattern as request_update_check
        ├── lib.rs                # MODIFIED: when bootstrapping the webview, inject scribe_version (env!("CARGO_PKG_VERSION")) into the initial state passed to JS, plus a host handler for "request_releases" / "open_external_url" IPC messages
        └── assets/
            ├── settings.html     # MODIFIED: add the "Releases" sidebar nav item and the empty <section id="releases-panel"> shell containing a header row with [Older] / version-picker <select> / [Newer] navigation controls plus a single content area for rendered release notes; replace the hardcoded "Scribe v0.1.0" footer with a placeholder element populated from JS state (e.g. <div class="sidebar-footer" id="sidebar-footer"></div>)
            ├── settings.css      # MODIFIED: styles for the dropdown + Newer/Older header row (sharp corners, reused settings-button tokens, disabled boundary state) and the rendered-notes typography in the single content area; reusing existing CSS variables (no new colors, no rounded cards)
            └── settings.js       # MODIFIED: tab-handler for "releases" that posts a request_releases IPC, renders the version-picker options, owns selectedReleaseVersion state shared by both the picker and the Newer/Older buttons, enforces the boundary-disable rules (FR-019/FR-020), intercepts internal-link clicks to forward to the host as open_external_url, and writes the sidebar-footer text from injected scribe_version

lat.md/
├── settings.md                   # MODIFIED: new "Releases" panel section, "Sidebar Footer" section
├── server.md                     # MODIFIED: new "Releases" / "Release Catalog" section under Server
├── protocol.md                   # MODIFIED: new "List Releases" / "Release List" entries under Client / Server messages
└── architecture.md               # MODIFIED: cross-link the new sections from the relevant Crate Map entries
```

**Structure Decision**: This is a desktop application split across multiple existing Rust crates in a single workspace, with the UI living in inlined webview assets inside `crates/scribe-settings/src/assets/`. The plan extends the three crates that already own the relevant responsibilities (protocol, server-side network I/O, settings UI) rather than introducing a new crate. Markdown rendering and sanitization live on the server side of the IPC boundary so the webview never touches untrusted HTML or pulls in a JS markdown library, satisfying FR-010 and FR-017.

## Complexity Tracking

> **Fill ONLY if Constitution Check has violations that must be justified**

No constitutional violations to track. The constitution is the unfilled template and the project's de-facto rules (see Constitution Check section) are all satisfied by reusing existing infrastructure.
