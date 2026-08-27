# link-open-error

## Problem Statement

Ctrl+click on a detected link in the terminal grid can fail with zero feedback.
All three open paths in `crates/scribe-client/src/url_detect.rs` spawn the OS
opener fire-and-forget and never observe the exit status:

- `open_path` (file paths; tries `code --goto` for `:N` suffixes, falls back to
  `xdg-open`/`open`),
- `open_url` (heuristic URLs; scheme allowlist, then `open_uri_unguarded`),
- `open_uri_unguarded` (OSC 8 activations after the allowlist/confirm gate).

A missing opener surfaces only as `tracing::warn!`; a non-zero exit is never
seen at all. The user clicked, nothing happened, and the screen says nothing.
Separately, there is no affordance showing where an OSC 8 label points before
clicking: the hover tooltip geometry in `crates/scribe-client/src/tooltip.rs`
was ported from the winit client but never wired — its only consumer is the
`tooltip_demo` debug toggle in `main.rs`.

This feature builds the failed-link-open feedback system designed in
`.impeccable/mocks/link-open-error-directions.html`: an opener wait thread, a
cause classifier, a one-row in-grid annotation overlay, a status-bar hover
preview for OSC 8 targets, and retirement of the floating tooltip box.

## Goals

- Every failed Ctrl+click open produces a visible, self-explanatory in-grid
  annotation when the opener reports failure (spawn error or non-zero exit).
- The annotation replicates the mock exactly: one row above the clicked run,
  `┌` over the run's head, `─┐` over its tail when head-anchoring would overrun
  the right edge, `└`/`─┘` flip below only when the clicked row is first on
  screen; opaque band whose cells replace what they cover and restore on
  dismiss; italic lowercase message in lightened red; pure ansi-red `✗` mark;
  dimmed-red joinery corners outside the band; 2px red underline on the failed
  run.
- A cause classifier maps failures to exactly the six message forms in the
  mock's specimen list (see Constraints; the sixth, code-less `✗ <cmd>
  failed`, was approved at the clarify gate and added to the mock).
- Documented feedback exceptions (also Non-Goals): a result whose anchor was
  invalidated before delivery (scroll/output/resize/pane close), a wait
  exceeding 60 s, and panes too small to hold the annotation drop the notice
  with a tracing event instead of rendering it.
- Hovering an OSC 8 label shows the target URI in the status bar's left
  segment, head+tail-truncated via `truncate_url`.
- The annotation never captures a keystroke and dismisses on the next
  keypress, click, scroll, or write to the covered rows.
- The floating tooltip box (`tooltip_element` and its demo toggle) retires.

## Non-Goals

- No feedback for successful opens.
- No actions inside the annotation (no retry button, no "choose application").
- No handler-configuration UI (which app opens which scheme).
- No change to link *detection* (`url_detect` span logic) or to the OSC 8
  scheme-allowlist / confirmation-dialog flow.
- No Windows support (repo targets Linux/macOS).
- No new user-facing configuration for this feature in the settings surface.
- No failure feedback for non-Ctrl+click opener callers (context menu, Smart
  Selection, CI status bar, Settings links) — they keep today's
  fire-and-forget behavior; phase 2 may extend.
- No keyboard-reachable path to the hover URI preview (pointer-only in MVP;
  the status-node mirror carries the assistive-technology story).
- No row-accurate output-dismissal tracking: any PTY output to the clicked
  pane dismisses (deliberate simplification of the mock's "write to that
  row").
- No guaranteed notice for invalidated anchors, waits past 60 s, or panes
  smaller than the annotation minimum — these drop silently with a trace
  (documented exceptions to "every failed open").
- Focus/tab switching does not dismiss; the mock's dismissal set (keypress,
  click, scroll, pane output) is exhaustive.

## Source Cards

None.

## Target Epic

None exists. This run will create the feature epic.

## Source Authority

- `.impeccable/mocks/link-open-error-directions.html` — declared authoritative
  by the user for this run ("ensure it gets perfectly replicated").
  - Normative: the component CSS comment block (placement rule, the three
    chrome tells: italic, wash, lifecycle); all five study frames (placement
    grammar: default-above, on-top-of-text, right-anchored clamp, top-edge
    flip, status-bar hover); the specimen list (exact classifier messages and
    trigger conditions); the closing lede (lifecycle, dismissal set,
    implementation deltas, tooltip-box retirement).
  - Reference-only: page h1/ledes prose, hero typography scale, page layout
    (measure, hero at 24px) — presentation for review, not product UI.
  - Unresolved authority: the mock's colors are hardcoded to its demo theme
    (`#0e0e10` bg, `#ef4444` red, band `#201213`, text `#f7706e`, joinery
    `rgba(239,68,68,.6)`). The product derives colors from the active terminal
    theme; the derivation formulas need confirmation (Open Questions).
  - The mock was gitignored-untracked; this run commits it (matching the
    tracked `beads-board-directions.html` precedent) so the authority artifact
    is versioned alongside this spec.

## User Stories

1. As a terminal user whose Ctrl+click did nothing, I want the terminal to
   tell me why, so that I can fix the cause instead of clicking again.
   - AC: with the platform opener absent (`ErrorKind::NotFound` on spawn), the
     annotation reads `✗ xdg-open is not installed` (platform-appropriate
     command name).
   - AC: opener exits non-zero on a scheme-carrying URI → `✗ no application
     handles <scheme> links` (scheme interpolated, e.g. `ssh`, `https`).
   - AC: opener exits non-zero on a `mailto:` target → `✗ no mail client
     configured`.
   - AC: opener exits non-zero on a file/path target and the resolved path
     no longer exists (`stat` fails) → `✗ file no longer exists`; if the
     path still exists, the coded fallback form is used instead.
   - AC: any other non-zero exit → `✗ xdg-open exited <N>` with the real exit
     code and real command name.
   - AC: a spawn error other than NotFound, or an exit with no code (signal)
     → `✗ <cmd> failed`.
   - AC: the annotation appears in the pane where the click happened, styled
     and placed per the mock's grammar, and the clicked run carries a 2px red
     underline while the annotation is visible.

2. As a user, I never lose input to the notice.
   - AC: no keystroke is consumed by the annotation; the first subsequent
     keypress, click, or scroll both reaches its normal target and dismisses
     the annotation.
   - AC: new PTY output overlapping the annotation's rows dismisses it.
   - AC: covered cells are restored exactly after dismissal (the overlay never
     mutates grid state).

3. As a user clicking links near screen edges, the annotation still lands
   legibly.
   - AC: clicked run on the first visible row → annotation flips below with
     `└` (head) / `─┘` (tail) joinery.
   - AC: head-anchoring would overrun the right edge → right-anchored `─┐`
     over the run's tail.
   - AC: the annotation never draws outside the pane's grid area.

4. As a user on a busy screen, the annotation is legible over existing
   content.
   - AC: the band is opaque (theme bg lifted toward ansi red); covered cells
     vanish beneath it and reappear on dismissal; a partially covered row's
     remnant stays visible past the band's edge.

5. As a user hovering an OSC 8 label, I want to see the real target before I
   commit, so that I don't open something unexpected.
   - AC: bare-hovering an OSC 8 hyperlink (no modifier) underlines it and
     shows `→ <uri>` in the status bar's left segment, truncated head+tail
     via `truncate_url` to the available width; the Ctrl-hover underline for
     click-arming is unchanged.
   - AC: the prior left-segment content restores on unhover.
   - AC: the floating tooltip box no longer exists (demo toggle removed);
     `truncate_url` survives wherever it lands.

## Constraints

- The mock `.impeccable/mocks/link-open-error-directions.html` is the visual
  contract. Placement grammar, message voice (italic, lowercase), mark/joinery
  colors, band opacity behavior, underline, dismissal set, and the exact five
  classifier messages must match:
  - `✗ xdg-open is not installed` — spawn → `ErrorKind::NotFound`
  - `✗ no application handles <scheme> links` — exit ≠ 0, target carries a scheme
  - `✗ no mail client configured` — exit ≠ 0, mailto target
  - `✗ file no longer exists` — exit ≠ 0, file or path target
  - `✗ xdg-open exited <N>` — exit ≠ 0, cause not classified
  - `✗ xdg-open failed` — spawn error · no exit code · killed by signal
  (`file no longer exists` is stat-gated; `xdg-open` is the Linux
  instantiation of `<cmd>` — the finally-spawned command is named.)
- Constitution:
  - #1 typed failure: classifier is a typed enum over spawn error kind /
    exit code / target kind — no string-matching on child output.
  - #2 session-safe UX: transient overlay, no keystroke capture, no change to
    server-owned session state.
  - #5 trust boundary: the child's stdout/stderr is untrusted and is NEVER
    echoed into the grid; messages come only from our classifier over exit
    status + target kind.
  - #3 verification: each story needs a user-reachable verification path;
    the Docker e2e visual harness is the expected home (exact tests in plan).
  - #4 performance: zero per-frame cost while no annotation is active; the
    wait thread lives off the render path.
  - #7: `lat.md/` sync for `url_detect`, tooltip retirement, status bar.
- Threading: waiting must not block the GPUI main thread; result delivery via
  the existing background-executor / async-context patterns in `main.rs`.
- `open_path`'s `code --goto` attempt falls back to `xdg-open` on NotFound:
  the wait must track the finally-spawned child, not the first attempt.
- Rendering: `TerminalElement` already layers adornments
  (`with_highlights`, `with_selection`, `with_link_underline`); the annotation
  overlay row and run underline extend that mechanism rather than inventing a
  new surface.
- Colors must derive from the active terminal theme (mock values are the demo
  theme's outputs): band ≈ theme bg lifted 8% toward ansi red; message text ≈
  lightened ansi red (the mock's `--red-text` rationale: small pure-red text
  vibrates on dark); joinery ≈ 60%-alpha ansi red; underline = ansi red.

## Open Questions

All resolved — see Spec Review (technical decisions) and Clarifications.
Remaining plan-level details: per-platform exit-code→cause tables, exact
e2e test names, and the annotation-demo toggle's final shape.

## Spec Review

Six parallel review passes (requirements, gaps, ambiguity, feasibility, scope,
stakeholders) against the spec, the constitution, the mock, and the codebase.
Cross-dimension convergence was near-total; merged and triaged below.

### Critical Questions (answer before planning)

1. Placement contract: all six passes flagged that the mock's component
   comment block still described the retired end-of-line design ("failed
   row's own empty tail... no clamp, no occlusion"), contradicting the five
   frames and the closing lede. Per the user's in-session direction ("always
   show it on top"), the frames are the contract; the stale comment has been
   fixed in the mock. Needs a confirm at the gate. — flagged by: all six.
2. Hover-preview trigger: bare hover (mock, browser idiom) vs Ctrl+hover
   (matches the existing Ctrl-gated underline in `main.rs`) vs legacy 300 ms
   dwell (`lat.md/client.md`). Changes interaction, hit-testing scope
   (focused pane only today), and idle mouse-move cost. — flagged by:
   requirements, gaps, ambiguity, feasibility, scope, stakeholders.
3. Failure-feedback scope: `open_url`/`open_path` are also called from the
   context menu, Smart Selection, the CI status bar, and Settings release
   links — surfaces with (or without) grid anchors. Is the annotation
   Ctrl+click-only for MVP? — flagged by: requirements, gaps, scope,
   stakeholders, ambiguity.
4. Message vocabulary completeness: "every failed open" includes spawn
   errors other than NotFound and signal exits with no code, which the five
   forms cannot express. Approve the generalized fallback surface form
   `✗ <cmd> failed` for code-less failures (sixth form, same voice)?
   — flagged by: requirements, gaps, scope, ambiguity.
5. Dismissal-on-output scope: mock says "write to that row"; row-accurate
   damage tracking is the single most expensive requirement (the PTY drain
   exposes only a redraw boolean today). Accept "any output to the clicked
   pane dismisses" instead? — flagged by: feasibility, ambiguity,
   requirements.
6. Accessibility acceptance: annotation and hover URI mirror into the
   existing AccessKit `Role::Status` node (per `lat.md/architecture.md`
   pane-feedback contract); no keyboard equivalent for hover preview in MVP.
   Accept this scope? — flagged by: gaps, stakeholders.

### Technical Decisions (self-resolved — veto at the gate to override)

- Classifier is an ordered decision table over typed inputs (spawn
  `ErrorKind`, `ExitStatus`, target kind, scheme) — precedence: spawn
  NotFound → "is not installed"; exit≠0 + mailto → mail message; exit≠0 +
  path or `file://` target → `stat` the resolved path, missing → "file no
  longer exists", present → fallback form; exit≠0 + other scheme → "no
  application handles <scheme> links" with scheme lowercased,
  ASCII-validated, length-capped (else fallback); anything else → fallback.
  Child stdout/stderr never read. (constitution #1, #5)
- `<cmd>` in messages is the finally-spawned command (`code`, `xdg-open`,
  `open`); the mock's `xdg-open` strings are the Linux instantiation of a
  template, not literal cross-platform text.
- `code --goto` spawning but exiting non-zero: no second open attempt;
  classify with cmd=`code` (avoids double-open surprises).
- Anchor identity: pane id + scrollback line + column range of the clicked
  segment, captured at click; multi-row wrapped spans underline every
  segment but anchor the annotation to the clicked segment's head/tail.
- Stale-result policy: revalidate the anchor when the opener reports (link
  cache epoch + line resolvability); on scroll/output/resize/close since
  click → drop silently with a tracing event. Latest-click-wins: one pending
  wait and one visible annotation per pane; a new failure replaces.
- Hung opener: wait thread parks ≤60 s, then drops the result (trace only);
  threads are detached and bounded at one per pane by latest-click-wins.
- Geometry limits: message truncates head-preserving with `…` to the pane
  width minus joinery; annotation suppressed (trace only) below a 12-col
  pane or a 1-row pane where no adjacent row exists.
- Colors (sRGB, derived from the active theme): band = per-channel round of
  mix(bg, ansi red, 8%); message text = ansi red in HSL with lightness
  +10 pt, clamped to ≥4.5:1 against the band; joinery = ansi red at 60%
  alpha; underline = ansi red, 2 logical px. The mock now carries the exact
  formula outputs for its demo theme (band `#201214`, text `#f37373`), so
  unit fixtures assert exact reproduction. The `✗` mark is upright pure
  ansi red; only the message text is italic (mock models them separately).
  Italic is requested as `FontStyle::Italic` with the renderer's synthetic
  fallback; wash + tint + lifecycle remain tells regardless. Hover: label
  underline = theme fg at 50% alpha, 1 px; status URI = the bar's primary
  text tone, arrow = its muted glyph tone. The mock's 2 px radius and 5 px
  band bleed are HTML approximations of cells and are NOT replicated;
  cell-rect rendering is the native truth.
- Hover preview replaces the whole left status group while active (per
  mock); width budget = bar width − right group − paddings, fed to
  `truncate_url`; unhover rebuilds the live model (no snapshot restore).
  Preview scope: OSC 8 spans only (heuristic URL/path text is its own
  target).
- Tooltip retirement: delete `tooltip_element` + the `TooltipDemo` chord
  consumer; relocate `truncate_url` (and tests) beside its new consumer;
  replace the Ctrl+Shift+U demo with an annotation-demo toggle so
  `tests/e2e/visual/overlays.sh` and `x11-focus-guard.sh` keep a
  deterministic pixel oracle and the new feature gains a visual-test seam.
- Alt screen: feature active wherever links are clickable; output-dismissal
  naturally shortens annotation life under chatty TUIs.
- Performance: when no annotation/preview is active, no overlay element is
  built (state short-circuit); verification = code-review assertion plus the
  e2e visual suite; no new bench. (constitution #4: documented manual check)
- Messages are intentionally English-only (terminal voice; no i18n).
- lat.md scope for the plan: `client.md` tooltip/hover sections, `test.md`
  x11-focus-guard oracle rows, `specs/016-gpui-client-rebuild/
  parity-inventory.md` tooltip rows, plus new-feature docs.

### Non-Blocking Observations

- `truncate_url` counts Unicode scalars, not display columns; non-ASCII
  URIs can overflow the status budget — fix opportunistically at its new
  home.
- Structured tracing for request identity, typed cause, and stale-result
  drops; never log child output; consider URI redaction in logs.
- Split-scroll panes currently expose no links; replay-restored OSC 8 spans
  lack URI metadata — carry both as documented limitations.
- `SpanTooltip` in `status_bar.rs` is unrelated GPUI tooltip machinery and
  survives.
- Day-after asks to expect: retry/choose-handler affordance, per-scheme
  handler config, feedback for non-grid opens, keyboard-accessible preview,
  failure history.

## Clarifications

**Q1 (placement contract / stale mock comment):** Resolved as a technical fix
under the user's prior in-session direction ("always show it on top"): the
five frames + closing lede are the contract; the mock's component comment was
updated to match. Presented in the gate decision list; no objection.

**Q2 (hover trigger):** A — bare hover. Hovering an OSC 8 label (no
modifier) underlines it and shows the target in the status bar. The existing
Ctrl-hover click-arming underline is unchanged. Preview stays OSC 8-only.

**Q3 (failure-feedback scope):** A — Ctrl+click only for MVP. Context-menu,
Smart-Selection, CI-bar, and Settings opens keep fire-and-forget; recorded in
Non-Goals; phase 2 may extend to grid-anchored surfaces.

**Q4 (fallback message form):** Accepted via decision list — code-less
failures render `✗ <cmd> failed`; coded failures `✗ <cmd> exited <N>`;
`<cmd>` is the finally-spawned command.

**Q5 (dismissal-on-output scope):** A — any PTY output to the clicked pane
dismisses. Deliberate simplification of the mock's "write to that row";
recorded in Non-Goals and Constraints.

**Q6 (accessibility):** A — failure message and hover URI mirror into the
existing AccessKit `Role::Status` node; hover preview remains pointer-only in
MVP.

All technical decisions in the Spec Review stand unobjected (consent by
silence at the gate).

## Architecture Approach

One new pure-logic module plus thin seams in existing files. All grammar,
classification, and color math live in a new
`crates/scribe-client/src/link_feedback.rs` as pure, unit-testable functions
(the `tooltip.rs` pattern this feature retires); GPUI/state wiring stays in
`main.rs`; painting extends `TerminalElement`'s existing adornment family
(`with_highlights` / `with_selection` / `with_link_underline`).

- Observation: Ctrl+click paths call new observed variants in
  `url_detect.rs` returning `Result<(std::process::Child, Cmd),
  (std::io::ErrorKind, Cmd)>` plus a typed `OpenTarget` (kind, scheme,
  resolved path) — spawn failures classify immediately; spawned children
  hand off to one wait worker per click. The worker loops `try_wait` +
  100 ms sleep against a 60 s deadline and a cancel token; on supersede or
  timeout it kills and reaps the child, reports a dropped result (trace
  only), and exits — no unbounded blocked threads. Outcomes deliver to the
  GPUI foreground via the established worker→channel→`cx.spawn` pattern
  (`settings/window.rs:2623-2641`). Existing fire-and-forget functions
  remain for non-Ctrl+click callers (clarified scope). The OSC 8
  confirmation dialog's pending state carries the click anchor and an
  observed/fire-and-forget origin flag, so a Ctrl+click confirmed via
  "Open Anyway" stays observed while context-menu activations stay
  fire-and-forget.
- Classification: a typed, ordered decision table over spawn `ErrorKind`,
  `ExitStatus`, target kind, and validated scheme — never child output
  (constitution #1, #5).
- State: at most one `PendingOpen` and one `LinkAnnotation` per pane
  (latest-click-wins; a new click cancels the prior worker via its token),
  anchored to pane id + absolute grid line + clicked segment columns +
  a per-pane content revision captured at click. `terminal.rs` publishes
  that revision (bumped on parse/scroll/resize) and resolves the absolute
  anchor; a revision mismatch at delivery drops the result with a trace.
- Rendering: `TerminalElement::with_annotation(...)` paints the opaque band
  cells, joinery corners, italic message glyphs, and the 2 px run underline
  from a prelaid `AnnotationLayout` computed in `link_feedback.rs`.
- Hover: bare mouse-move hit-tests OSC 8 spans only (existing
  `PaneUrlCache`), throttled to cell changes; the status bar's left group is
  replaced by `→ <uri>` while hovered; `truncate_url` relocates to
  `status_bar.rs` with a display-column fix.
- Rejected alternatives: GPUI floating element over the grid (the retired
  balloon — re-introduces geometry/occlusion chrome the design removed);
  writing feedback into the PTY grid itself (mutates session state,
  violates restoration guarantee); row-damage-accurate dismissal (clarified
  out — any pane output dismisses).

## Affected Components

- `crates/scribe-client/src/link_feedback.rs` (new): `OpenOutcome`,
  classifier + message table, `AnnotationLayout` placement grammar
  (above/flip/head/tail anchor, truncation, suppression), theme color
  derivation. Unit tests beside it.
- `crates/scribe-client/src/url_detect.rs`: observed open variants
  returning `(Child, &'static str cmd)`-shaped results; scheme
  validation helper; existing fns unchanged for other callers.
- `crates/scribe-client/src/main.rs`: `press_opens_link` wiring, pending
  state, delivery task, dismissal hooks (key/mouse/wheel/output/focus),
  bare-hover OSC 8 tracking, annotation demo toggle (replacing
  `TooltipDemo`), AccessKit status mirror.
- `crates/scribe-client/src/terminal.rs`: per-pane content revision counter
  + absolute-line anchor resolution (new shared primitive for capture and
  stale-drop).
- `crates/scribe-client/src/terminal_element.rs`: annotation adornment +
  fixed-2px link-failure underline painting.
- `.impeccable/mocks/link-open-error-directions.html`: the normative
  artifact itself — force-added and committed by this spec run's landing
  commit; docs-sync verifies it stays tracked.
- `crates/scribe-client/src/status_bar.rs`: hover-URI left-group
  replacement, relocated `truncate_url` (column-aware), accessibility
  label.
- `crates/scribe-client/src/tooltip.rs`: deleted (geometry fns retire with
  their only consumer; `truncate_url` + tests move).
- `crates/scribe-client/src/keybindings.rs`: `TooltipDemo` chord →
  `AnnotationDemo`.
- `tests/e2e/visual/overlays.sh`, `tests/e2e/visual/x11-focus-guard.sh`:
  oracle rewrite onto the annotation demo; new annotation/hover cases.
- `lat.md/client.md`, `lat.md/test.md`,
  `specs/016-gpui-client-rebuild/parity-inventory.md`: sync.

## Data Model

In-memory only; no schema, storage, config, or protocol changes.

- `OpenFailure` enum: `OpenerMissing{cmd}`, `NoHandler{scheme}`,
  `NoMailClient{cmd}`, `FileMissing{cmd}`, `Exited{cmd, code}`,
  `Failed{cmd}` → exactly the mock's five surface forms plus the clarified
  code-less fallback.
- `PendingOpen { pane, generation, anchor, target: OpenTarget, cmd,
  cancel: Arc<AtomicBool>, started }` / `LinkAnnotation { pane, anchor,
  message, layout }` on the terminal view; `anchor` = absolute grid line +
  clicked-segment column range + full-span segments (underline) + captured
  pane revision. `OpenTarget { kind: Url|Path|Osc8, scheme:
  Option<String>, resolved_path: Option<PathBuf> }`.

## API / Interface Changes

- `url_detect::open_url_observed`, `open_path_observed`,
  `open_uri_unguarded_observed` → `Result<(std::process::Child, Cmd),
  (std::io::ErrorKind, Cmd)>` + an `OpenTarget` describing what was opened;
  existing public fns unchanged (no breaking changes).
- `TerminalElement::with_annotation(Option<AnnotationPaint>)` builder.
- `StatusBarData` gains `hover_uri: Option<&str>` and a measured
  `left_budget_cols` (bar width minus right group, centered CTA, and
  paddings, supplied by the render path); `build_left` renders the URI
  exclusively while present, truncated to that budget.
- `tooltip` module removed from `lib.rs` (internal crate surface only).
- Keybinding chord `ctrl+shift+u` keeps its binding, action renamed to the
  annotation demo.

## Testing Strategy

- Unit (in `link_feedback.rs` + `status_bar.rs`): classifier truth table
  (all six outcomes, precedence, scheme validation/capping, stat-gated
  FileMissing); placement grammar (default above, head/tail anchor
  threshold, top-edge flip, truncation, small-pane suppression); color
  derivation vs the mock's demo-theme expectations (`#201213`, `#f7706e`
  reproduction from `#0e0e10`/`#ef4444`); column-aware `truncate_url`.
- E2E visual (`tests/e2e/visual/terminal-links.sh` — the existing live
  link oracle with an opener shim; the func image ships no client/X11, so
  click paths live here): named cases `link-fail-notfound` (real Ctrl+click,
  annotation appears), `link-fail-dismiss-key` / `-click` / `-wheel` /
  `-output` (dismissal + the triggering key still reaches the PTY),
  `link-open-success-silent` (shim exits 0 → no annotation),
  `link-fail-stale-drop` (output before the shim reports → nothing),
  `hover-preview` (bare hover underlines the OSC 8 label + status URI),
  `hover-unhover-restore` (left group returns live).
- E2E visual (demo oracle): the annotation demo chord cycles
  default → busy-row → clamped `─┐` → top-flip `└` on repeat presses;
  `overlays.sh` gains `annotation-demo-<state>` cases and
  `x11-focus-guard.sh` re-pins its pixel oracle to the first demo state.
- Unit/state additionally: repeated clicks against a hung shim (worker
  cancel + kill/reap), `code --goto` non-zero classification, AccessKit
  status-label mirroring, narrow-bar hover truncation budget.
- Manual (documented in the epic): macOS `open` behavior and exit-code
  table; light-theme contrast clamp.
- Performance: assertion that no overlay element is built when state is
  `None` (code review) + unchanged e2e visual timings; no new bench
  (constitution #4: documented manual check).

## Risks

- `main.rs` is very large and hot; mitigate by keeping logic in
  `link_feedback.rs` and touching only the named seams.
- Bare-hover hit-testing cost on mouse move; mitigate with cell-granularity
  memoization (only re-resolve when the hovered cell changes) — same
  discipline the Ctrl-hover path uses.
- Italic rendering: if the configured font lacks an italic face, fall back
  to the regular face — wash + tint + lifecycle remain the chrome tells
  (the mock's own caveat).
- Output-dismissal races (annotation appears while output streams):
  latest-wins state machine makes the worst case a briefly-visible then
  dismissed annotation; acceptable.
- Rollback: additive seams + one deleted module; revert is a clean
  cherry-pick; no persisted state.

## Sequencing

Shared primitives first; consumers branch after they land.

1. **feedback-core** (P1, foundational): `link_feedback.rs` — `OpenOutcome`,
   `OpenTarget`, classifier + six message forms, scheme validation. Blocks:
   annotation-layout (same new file — hard edge), opener-observe,
   annotation-paint, demo-and-retire.
2. **annotation-layout** (P1, foundational): placement grammar +
   `AnnotationLayout` + theme color derivation (exact-fixture formulas) in
   `link_feedback.rs`. Needs feedback-core (same file; parallel workers
   cannot both create it). Blocks: annotation-paint, demo-and-retire.
3. **anchor-revision** (P1, foundational): `terminal.rs` per-pane content
   revision counter + absolute-line anchor resolution. Blocks: click-wiring.
4. **opener-observe** (P1): observed spawn variants returning typed
   `Result` + `OpenTarget`, wait worker (try_wait loop, cancel token,
   kill/reap on supersede/timeout), stat gate, `code --goto` handling in
   `url_detect.rs`. Needs feedback-core. Blocks: click-wiring.
5. **annotation-paint** (P1): `TerminalElement` annotation adornment
   (upright `✗` + italic message modeled separately) + 2 px failure
   underline. Needs annotation-layout. Blocks: click-wiring,
   demo-and-retire.
6. **click-wiring** (P1): `press_opens_link` observed path, OSC 8
   confirmation origin/anchor carry-through, pending/visible state with
   generation + cancel, stale-drop, dismissal hooks (key/mouse/wheel/pane
   output — no focus dismissal), AccessKit mirror. Needs anchor-revision +
   opener-observe + annotation-paint. Blocks: hover-preview (main.rs
   overlap), e2e-suite, docs-sync.
7. **hover-preview** (P2): bare-hover OSC 8 tracking (cell-memoized),
   status-bar left-group URI with measured `left_budget_cols`,
   `truncate_url` relocation + display-column fix, accessibility label.
   Needs click-wiring (file overlap only). Blocks: demo-and-retire,
   e2e-suite.
8. **demo-and-retire** (P2): delete `tooltip.rs` + `TooltipDemo`; add the
   state-cycling annotation demo on the same chord; rewrite `overlays.sh`
   (`annotation-demo-<state>`) + `x11-focus-guard.sh` oracles. Needs
   feedback-core, annotation-paint + hover-preview (truncate_url must move
   before the module deletes). Blocks: e2e-suite.
9. **e2e-suite** (P2): extend `tests/e2e/visual/terminal-links.sh` with the
   named fail/dismiss/success/stale/hover cases (opener shim) + the demo
   state cases. Needs click-wiring, hover-preview, demo-and-retire.
10. **docs-sync** (P3): lat.md client/test sections, parity-inventory rows,
    spec cross-links, verify the mock artifact is tracked. Needs
    click-wiring, hover-preview, demo-and-retire.

## Normative Visual Coverage

| Row / artifact locator | Normative requirement | Goal / Non-Goal alignment | Implementation work item | Verification work item + oracle | Status |
|---|---|---|---|---|---|
| mock: component comment | Placement rule (above, ┌/─┐, top-flip), opaque replace+restore, three chrome tells, no key capture | aligned | annotation-layout, annotation-paint, click-wiring | e2e-suite (func dismissal + visual states); unit grammar tests | full |
| mock: frame "Above the run · default" | Default placement, ┌ over run head, band on empty cells | aligned | annotation-layout, annotation-paint | e2e-suite visual `annotation-default`; unit layout test | full |
| mock: frame "On top of text · busy row" | Opaque band covers cells mid-row; remnant survives; restore on dismiss | aligned | annotation-paint, click-wiring | e2e-suite visual busy-row before/after pixels | full |
| mock: frame "Clamped · right-anchored" | ─┐ over run tail when head-anchor overruns right edge | aligned | annotation-layout | unit threshold test + e2e-suite visual clamped state | full |
| mock: frame "Top edge · flips down" | └/─┘ below when clicked row is viewport row 0 | aligned | annotation-layout | unit flip test + e2e-suite visual top-flip state | full |
| mock: frame "Hover · status bar" | Bare hover; 1 px fg@50% label underline; `→ <uri>` replaces left group (arrow muted, URI primary tone); truncate_url to measured budget | aligned (clarified: bare hover) | hover-preview | e2e-suite `hover-preview` + `hover-unhover-restore`; unit truncate budget test | full |
| mock: specimen list (6 messages + conditions) | Exact message strings & trigger conditions, incl. stat-gated file row and code-less `✗ <cmd> failed` | aligned (mock updated at alignment round) | feedback-core | unit classifier truth table; e2e `link-fail-notfound` | full |
| mock: closing lede lifecycle | Appear on opener report; dismiss on key/click/scroll/pane output; never capture; covered cells return; tooltip box retires | aligned (output scope clarified to pane-wide) | opener-observe, click-wiring, demo-and-retire | e2e-suite `link-fail-dismiss-*` + `link-open-success-silent`; rewritten focus-guard oracle | full |
| mock: run underline (`.run > .edge`) | 2 px ansi-red underline on the failed run while annotation shows | aligned | annotation-paint | e2e-suite visual underline pixels | full |
| mock: colors (`#201214`, `#f37373`, 60% joinery) | Theme-derived formulas whose demo-theme outputs equal the mock constants exactly (mock updated to formula outputs; radius/bleed not replicated) | aligned | annotation-layout | unit color-derivation test asserting exact mock values | full |

## Source Card Refinement

None — no source cards. Target Epic (resolved): no epic existed; the
materialization step creates the feature epic and parents all Sequencing
items under it.

## Alignment fixes applied

- (A, must) Six-form vocabulary + stat-gated file condition written into the
  normative mock specimen list and the spec's Goals/Constraints/ACs — the
  clarified behavior is now the recorded authority, removing the
  five-vs-six contradiction.
- (A, must) "Every failed open" exceptions (stale anchor, 60 s cap, tiny
  panes) made explicit in Goals and Non-Goals.
- (A+B, must) Observed-opener API retyped: `Result<(Child, Cmd),
  (ErrorKind, Cmd)>` + `OpenTarget`; `PendingOpen` gains generation +
  cancel token; wait worker respecified as try_wait loop with kill/reap on
  supersede/timeout (no unbounded blocked threads).
- (A+B, must) OSC 8 confirmation carries click anchor + observed/origin
  flag so "Open Anyway" stays observed without leaking feedback to
  context-menu callers.
- (B, must) New `anchor-revision` foundational item: `terminal.rs`
  publishes a per-pane content revision + absolute-line anchor (primitive
  the stale-drop and dismissal logic needs; file added to Affected
  Components).
- (B, must) Sequencing items 1→2 given a hard dependency edge (same new
  file); item numbering now 10 items.
- (A, must) `✗` mark modeled upright pure red, message-only italic
  (matches mock `.note .x`); focus-change dismissal removed — the mock's
  dismissal set is exhaustive.
- (A, must) Hover frame styling pinned: fg@50% 1 px label underline,
  muted arrow + primary-tone URI, measured `left_budget_cols` accounting
  for right group and centered CTA.
- (A+B, must) Color formulas made exactly reproducible: mock constants
  updated to formula outputs (`#201214`, `#f37373`); unit fixtures assert
  exact values.
- (A+B, must) E2E strategy moved off the client-less func image onto
  `tests/e2e/visual/terminal-links.sh` (existing opener shim) with named
  cases incl. success-silence, all dismissal variants, stale-drop,
  unhover-restore; demo defined as a state-cycling chord with
  `annotation-demo-<state>` cases.
- (A+B, must/should) Mock artifact committal recorded in Affected
  Components; docs-sync verifies tracking.
- (A, should) Plan-level open questions (demo shape, test names) resolved
  in Testing Strategy and Sequencing.

## Constitution Check

- #1 typed failure: `OpenFailure` enum + ordered table; no string matching,
  no child-output parsing. — honored.
- #2 session-safe UX: overlay never mutates grid/session state; no
  keystroke capture; existing shortcuts untouched (demo chord reused, same
  binding). — honored.
- #3 verification: every story has a named path (unit table, e2e func
  real-failure click, visual states, documented macOS manual). — honored.
- #4 performance: zero-idle-cost short-circuit + hover memoization;
  documented manual check, no new bench. — honored, tension noted: no
  automated frame-time gate.
- #5 trust boundary: scheme validated/capped before interpolation; child
  output never rendered or logged. — honored.
- #6 local-first: no network. — honored.
- #7 documented change: lat.md/parity sync is a sequenced work item;
  no server/live-state disruption. — honored.
