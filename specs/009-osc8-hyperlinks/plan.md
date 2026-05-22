# Implementation Plan: OSC 8 Explicit Hyperlinks

**Branch**: `009-osc8-hyperlinks` | **Date**: 2026-05-21 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/009-osc8-hyperlinks/spec.md`

## Summary

Surface OSC 8 hyperlinks on activation, hover, and the right-click context
menu, with a confirmation dialog for disallowed-scheme URIs. The work is
**purely client-local**: no IPC, protocol, server, settings, or config
schema changes. The bulk of the implementation rides existing upstream
infrastructure — `alacritty_terminal-0.26.0-rc1` already parses OSC 8 and
stores the URI on each affected cell via `Cell::hyperlink()` (no feature
flag, no upstream patch required). Scribe's only new client-side surfaces
are: a second pass in `url_detect::scan_visible_urls` that lifts the
cell-level hyperlink into the URL-span cache with precedence over
heuristic detection, a hover-dwell tooltip via the existing
`tooltip.rs` renderer, a "Copy hyperlink address" context-menu entry, and
a new sibling dialog file modeled on `update_dialog.rs` for the
disallowed-scheme confirmation. Replayed scrollback intentionally does
NOT carry OSC 8 hyperlinks (documented limitation — `snapshot_to_ansi`
emits chars + SGR only, no OSC 8 reconstruction); live post-reattach
hyperlinks work unchanged.

See [research.md](./research.md) for the seven decisions that drove this
shape, [data-model.md](./data-model.md) for the cell-attribute carriage
and HoverState design, [contracts/internal-osc8-pipeline.md](./contracts/internal-osc8-pipeline.md)
for the six internal contracts, and [quickstart.md](./quickstart.md) for
per-user-story manual verification.

## Technical Context

**Language/Version**: Rust (workspace edition per `Cargo.toml`)
**Primary Dependencies**:
  - `alacritty_terminal` 0.26.0-rc1 — `Cell::hyperlink()` /
    `set_hyperlink()` already exist publicly and are populated by
    upstream's VTE Perform impl unconditionally.
  - `wgpu` — existing instanced-quad pipeline reused for tooltip and
    dialog rendering.
  - `cosmic-text` — existing glyph shaping/atlas reused for tooltip text
    and dialog label rendering.
  - `winit` — existing mouse/move events drive the hover dwell timer.
  - No new crate dependencies.
**Storage**: N/A — hyperlinks live on cells (upstream
`Arc<CellExtra>`); HoverState and DisallowedSchemeDialog state are
transient on `App`.
**Testing**: `cargo test --workspace` baseline must remain green. No new
automated tests requested in spec; manual quickstart covers all three
user stories (Constitution II compliant — see Quality/UX requirements
section in spec.md QR-002 and the manual-quickstart precedent set by
specs 005, 007, 008).
**Target Platform**: macOS, Linux X11/Wayland, Windows — same scope as
the rest of `scribe-client`. OS handler invocation (`xdg-open`/`open`)
is identical across platforms; the dialog and tooltip overlays are
platform-neutral GPU rendering.
**Project Type**: Desktop app, Rust workspace (`crates/scribe-{client,
server,pty,renderer,common,cli,settings,test}`).
**Performance Goals**:
  - Activation latency for allowed-scheme OSC 8 URIs ≤16 ms (one 60 Hz
    frame) over the existing heuristic-URL activation path. No
    `open_url` path change; the dispatch is a single hyperlink-presence
    check.
  - Tooltip render at one frame after dwell threshold elapses (~300 ms);
    visible within one render cycle of any cursor stillness on a
    hyperlinked cell.
  - URL-detect cache rebuild adds at most one O(cells_in_viewport) pass
    on dirty; the OSC 8 pass and heuristic pass together MUST keep the
    overall rebuild within the existing budget (no measurable per-frame
    regression).
  - Memory: per-pane URI storage is bounded by upstream
    `Arc<CellExtra>` sharing and the scrollback line cap (no
    Scribe-side intern table; see `data-model.md` E1/E2).
**Constraints**:
  - MUST preserve the URL-detect cache invalidation seam (`mark_dirty`)
    — the OSC 8 pass piggybacks on it.
  - MUST preserve the activation routing through the existing
    `url_detect::open_url`-style path for allowed schemes (no parallel
    activation surface for the common case).
  - MUST NOT introduce a new IPC message, config key, persistence
    field, or webview surface.
  - MUST keep heuristic URL detection working unchanged on cells that
    carry no OSC 8 URI (FR-014, SC-004).
**Scale/Scope**: One DisallowedSchemeDialog and one HoverState per
window. Hyperlink storage per pane scales with distinct URIs in live
scrollback (bounded by upstream Arc-sharing + scrollback line cap).

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

### Pre-research check

- **Code Quality**: **PASS**. Change is confined to
  `crates/scribe-client/` with no cross-cutting helpers. Uses existing
  typed surfaces (`Cell::hyperlink()` upstream, `UrlSpan` cache,
  `ContextMenuAction`, the dialog pattern shared by `close_dialog.rs`
  and `update_dialog.rs`). No new crate dependencies. The
  `MetadataParser` in `scribe-pty` is intentionally NOT touched —
  OSC 8 reaches cells via the upstream VTE path that already runs in
  the client.
- **Testing Strategy**: **PASS**. Each user story has an independent
  manual quickstart path documented in `quickstart.md` and referenced
  in `spec.md`. No new automated tests are requested; the existing
  `cargo test --workspace` baseline must remain green. Constitution
  II permits this when the spec documents the manual verification with
  rationale — the rationale here is that OSC 8 behavior is inherently
  OS-handler-mediated (Ctrl+click hands a URI to `xdg-open`/`open` —
  outcome is not in-process testable) and an OSC 8 emit harness is
  straightforward to script by hand per quickstart.
- **User Experience Consistency**: **PASS**. The tooltip overlay,
  context-menu items, and confirmation dialog follow existing Scribe
  surfaces verbatim. No new visual paradigm. Activation routing for
  allowed schemes is unchanged from today's heuristic-URL path. The
  confirmation dialog matches the established Close/Update dialog
  conventions (Esc cancels, Enter activates focused button, Cancel is
  default focus). No new keybindings, no new settings page entries.
- **Performance**: **PASS**. Measurable budgets stated above. Hot
  paths (heuristic URL detection, frame render, `open_url`) are
  preserved verbatim for the common case. The new OSC 8 cache pass is
  bounded to viewport-sized work on `mark_dirty`. Tooltip dwell is
  event-driven (mouse moves), not polled.
- **Operational Safety**: **PASS**. No server restart, no IPC
  protocol change, no config migration, no persistence change.
  `lat.md` updates scoped to `client.md` (URL Detection + new
  Hyperlinks subsection or extension thereof, Context Menu, Tooltip,
  Dialogs). `lat check` will be run before completion.

No constitution violations. Complexity Tracking remains empty.

### Post-Phase 1 re-check

- **Code Quality**: **PASS**. Phase 1 design touches five files in
  `crates/scribe-client/` (`url_detect.rs`, `context_menu.rs`,
  `main.rs`, `tooltip.rs` use-site only, new
  `disallowed_scheme_dialog.rs`) with no cross-crate ripple. The
  `MetadataParser`/`OscInterceptor` in `scribe-pty` are deliberately
  NOT touched (research decision 6) — preserves the protocol layer.
  The data model adds one `SpanKind` variant and one
  `ContextMenuAction` variant — both additive, no rename or removal.
- **Testing Strategy**: **PASS**. Quickstart covers US1 (6 scenarios),
  US2 (5 scenarios), US3 (5 scenarios), plus a performance spot-check
  and a replay-limitation note. Independent verification path for
  each story is preserved.
- **User Experience Consistency**: **PASS**. Disallowed-scheme dialog
  matches existing dialog conventions verbatim. "Copy hyperlink
  address" item joins the existing context menu in alphabetical-ish
  position (after "Open File", consistent with the pattern of
  appending context-dependent items). Tooltip uses the existing
  `tooltip::render_tooltip` API unchanged.
- **Performance**: **PASS**. The Phase 1 contracts state that
  `url_at` precedence resolution stays O(spans) (linear, same as
  today), the OSC 8 pass is O(viewport_cells), and the dwell timer is
  event-driven. No design-stage hot-path regression introduced.
- **Operational Safety**: **PASS**. Replay-scrollback limitation is
  documented in `data-model.md` E1 notes, the contract C6, and
  `quickstart.md` US3 Known Limitation block. `lat.md` will gain a
  short note under URL detection / a new Hyperlinks subsection
  describing the limitation explicitly, so future readers know it is
  intentional and where the follow-up improvement lives.

No constitution violations after Phase 1. Complexity Tracking remains
empty.

## Project Structure

### Documentation (this feature)

```text
specs/009-osc8-hyperlinks/
├── plan.md              # This file
├── research.md          # Phase 0 output — 7 decisions, upstream finding
├── data-model.md        # Phase 1 output — E1..E6 entities
├── quickstart.md        # Phase 1 output — manual verification per US
├── contracts/
│   └── internal-osc8-pipeline.md   # Phase 1 output — C1..C6 contracts
├── checklists/
│   └── requirements.md  # Spec quality checklist (already created)
├── spec.md              # Feature spec with clarifications
└── tasks.md             # Phase 2 output (created by /speckit-tasks — not here)
```

### Source Code (repository root)

Touched files (Rust workspace; layout already established in
`lat.md/architecture.md`):

```text
crates/scribe-client/src/
├── url_detect.rs                   # Extend SpanKind with Osc8Hyperlink;
│                                   # add OSC 8 cell-walk pass in
│                                   # scan_visible_urls; precedence in
│                                   # url_at.
├── context_menu.rs                 # New
│                                   # ContextMenuAction::CopyHyperlinkAddress;
│                                   # osc8_uri field on
│                                   # ContextMenuRequest; menu builder
│                                   # appends "Copy hyperlink address"
│                                   # item; Open URL item carries OSC 8
│                                   # URI when present.
├── main.rs                         # New HoverState fields on App;
│                                   # mouse-move handler updates dwell
│                                   # timer; tooltip render call when
│                                   # threshold elapses; activation
│                                   # router that branches on scheme
│                                   # allowlist; dialog instantiation +
│                                   # render + event routing; wire
│                                   # CopyHyperlinkAddress action.
├── tooltip.rs                      # USED AS-IS — no source change.
├── disallowed_scheme_dialog.rs     # NEW — modelled after
│                                   # update_dialog.rs (DialogLayout,
│                                   # DialogRenderer, DialogColors).
│                                   # Two buttons: Cancel (default),
│                                   # Open Anyway.
└── (no other client files change)

crates/scribe-server/                # NO CHANGES (decision 7).
crates/scribe-common/                # NO CHANGES.
crates/scribe-pty/                   # NO CHANGES (decision 6).
crates/scribe-renderer/              # NO CHANGES (tooltip + dialog reuse
                                     # existing chrome / atlas helpers).
crates/scribe-settings/              # NO CHANGES (no new config keys).

lat.md/
├── client.md                       # Extend URL Detection section to
│                                   # note OSC 8 precedence; new short
│                                   # Hyperlinks subsection covering
│                                   # tooltip surface, context-menu
│                                   # entry, dialog, and the
│                                   # replay-scrollback limitation.
└── (other lat.md files unchanged)
```

**Structure Decision**: Pure additive change to `scribe-client` only,
mirroring the IME (spec 008) blast radius. No new crates, no new modules
outside `scribe-client`, no cross-crate ripple. The change preserves the
server/client split — server forwards PtyOutput bytes verbatim as it
does today; client-side VTE (which already runs in `scribe-client`)
populates cell hyperlinks unchanged from upstream behavior.

## Complexity Tracking

> No constitution violations recorded. This section intentionally empty.
