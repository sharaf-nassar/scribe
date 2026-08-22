# A2/A3 final parity audit

## Audit identity

- Task: `scribe-zwtv.18`
- Audit date: 2026-08-22
- Audited commit: `c721a7ef8ea10ae5e2596468062a60c8dc515742`
- Reviewer role: independent final reviewer; no A2/A3 implementation changes
- Authority: `specs/028-beads-board-contract.md`
- Approved artifact: `.impeccable/mocks/beads-board-directions.html`
- Generated contract: `.impeccable/mocks/a2a3-contract.json`
- Decision provenance: Quill session `01a01227-1e60-78a5-ac9d-58a63aef7ead`

## Method

The verdict is derived from artifact coverage, not child-bead status. Every
normative coverage row is checked against production code, Rust or machine
proof, visual proof, functional proof, documentation, and installed-package
provenance. Installed UI captures are produced independently for this audit;
`test-output/zwtv17/` is excluded as verdict evidence.

No finding is fixed during this audit. Any gap makes the verdict fail and must
be filed as a blocking child before this bead or epic can close.


## Evidence index

### Machine, code, and Rust

- **M-contract** — `.impeccable/mocks/check-contract.py`: fresh manifest,
  exact A2/A3 section allowlist, all named states/interactions, geometry
  formulas, 94 owned coverage rows, oracle tiers, production-drift rejection,
  and `check-flow.py`; passed.
- **M-manifest** — `.impeccable/mocks/a2a3-contract.json`, generated from the
  approved HTML with source SHA-256
  `67f1af1e918c47018a9c21e312a5321eb343c0116f8131116a561f26992124ae`.
- **C-A2-layout** — `crates/scribe-client/src/beads_board_a2.rs` (`layout`,
  `visible_row_count`, `rail_widths`, `common_epic`, `compact_relative_age`,
  `queue_at`).
- **C-A2-paint** — `crates/scribe-client/src/beads_board.rs` (`headband`,
  `lane_head`, `lane_seam`, `ledger_row`, `row_sub_line`, `void_copy`,
  `overflow_chevron`, `floor`, `text_size_controls`).
- **C-A2-drawer / C-A2-pin / C-A2-a11y** — `collapsed_tab`, `lane_drawer`,
  `tab_interactivity`, `pin_lane`, `unpin_lane`, `tab_accessible_label`, and
  `unpin_accessible_label` in `beads_board.rs`.
- **C-A2-drag / C-A2-key / C-write** — `card_drop_verb`, drag/key-move state,
  `apply_card_drop`, and the guarded panel/server write path in
  `beads_board.rs`, `beads_panel.rs`, and server `beads_board.rs`.
- **C-A3-layout / C-A3-render / C-A3-trace / C-A3-a11y** —
  `crates/scribe-client/src/beads_flow.rs`: graph ranking, wire union, render,
  trace, controls, node semantics, fades, and scrollbar.
- **C-Flow-state** — `BeadsBoards` Flow entry, frozen graph, cursor, scroll,
  cleanup, relayout, region isolation, and round-trip state in
  `crates/scribe-client/src/beads_board.rs`.
- **C-theme** — `BeadsBoardColors::from_theme`, contrast solving, and named
  A2/A3 color slots in `beads_board.rs`.
- **C-server-board / C-server-graph** — server cache, classification, graph
  assembly/admission, generation fence, and typed refusals in
  `crates/scribe-server/src/beads_board.rs`.
- **C-protocol** — named MessagePack board/write/Flow types and additive
  defaults in `crates/scribe-common/src/protocol.rs`.
- **R-manifest** — `beads_board_a2::tests::manifest::constants_match_the_generated_contract`.
- **R-A2-layout** — sparse/pinned/overflow/whole-row/narrow/starvation/hit-test
  matrix in `beads_board_a2.rs`.
- **R-A2-drawer / R-A2-drag / R-A2-key** — drawer/pin/lifetime, drag target,
  and keyboard-move tests in `beads_board.rs`.
- **R-A3-layout / R-A3-trace / R-A3-live / R-A3-controls / R-A3-a11y** —
  ranking, row budget, wire, trace, liveness, return-control, and accessible
  presentation tests in `beads_flow.rs` and `beads_board.rs`.
- **R-Flow-state** — Flow entry/cursor/scroll/capability/region/round-trip tests
  in `beads_board.rs`.
- **R-contrast** — `every_text_colour_clears_the_contrast_floor`, theme-slot,
  and panel contrast tests.
- **R-server-board / R-server-graph / R-protocol** — server cache/write/graph
  tests and named MessagePack round trips.

### Rebuilt visual and real-`bd` functional evidence

- **V-inventory** — manifest-backed inventory with all eight named states and
  31 captures (`test-output/beads-a2a3-contract-evidence.json`).
- **V-A2-collapsed / hover / pinned / drag / empty / overflow / row / scale /
  resize / narrow / geometry / states** — matching `a2-*.png` captures from
  `just e2e-visual-beads-board`; the suite measured tracks, drawers, 51px rows,
  the 320×36 ghost, whole-row resize, scale, and narrow splits.
- **V-A3-open / trace / deep / overflow / live / controls / keyboard** —
  matching `a3-*.png` captures; the suite measured Flow chrome, state dots,
  trace, live halo, 4-row frontier, fades/hbar, focus, and both return controls.
- **V-theme** — `a2-theme-{before,after}.png` and
  `a3-theme-{before,after}.png`; 7 A2 and 6 A3 semantic samples moved without
  geometry drift.
- **F-board / F-admission / F-geometry / F-drag / F-drawer / F-keymove /
  F-flow / F-live / F-resize / F-isolation / F-restart / F-write** — named
  phases in `tests/e2e/func/beads-board.sh`; all final PASS lines were observed
  in the clean-tree run. `beads-flow-graph.json`, wire records, real `bd show`
  snapshots, and phase screenshots remain under `test-output/`.
- **F-suite** — full rebuilt visual plus real-`bd` functional suites.

### Documentation and installed package

- **D-sync** — canonical contract plus `lat.md/client.md`, `lat.md/server.md`,
  `lat.md/protocol.md`, and `lat.md/test.md`; `lat check` passed. The retired
  `specs/026-beads-flow-view.md` is only a supersession pointer. Stale-design
  grep found no authoritative legacy contract.
- **I-PKG** — canonical-checkout `tests/install/dev-package-smoke.sh`: exit 0,
  78 PASS checks. Source, `.deb`, and installed client/server bytes match;
  installed binaries contain board/write/Flow protocol fields.
- **I-A2** — independent installed `/usr/bin/scribe-dev` capture
  `01-test-beads-terminal.png` / `05-a2-installed-back-return.png`; 1552×739
  client, board top 34, 197px strip, tracks
  `44:454, 514:454, 984:454, 1454:36, 1506:36`.
- **I-DRAWER** — installed `06-a2-installed-blocked-hover.png` measured
  `452x188+1004+39`; `08-a2-installed-blocked-pinned.png` measured tracks
  `44:363, 423:363, 802:363, 1181:309, 1506:36`; `09` proves unpin.
- **I-ROW** — installed `13-a2-installed-row-hover.png`; existing oracle passed
  `row=454x51+44+58 changed=23055`.
- **I-A3** — installed `03-a3-installed-opened.png`: 197px budget
  `34+15+139+2+4+3`, progress track 150px, 214×24 cursor node at `(30,140)`,
  rank lefts `30,272,514` (242px pitch), no hbar for fitting graph.
- **I-FLOW** — installed row activation opened panel and Flow (`03`),
  `← LANES` returned to A2 (`05`), and mode-pair `LANES` returned (`11`).
- **I-TRACE** — installed `12-a3-installed-trace.png` over admissible epic
  `test-beads-8fj`.

The installed server was PID 785594. Before capture,
`/proc/785594/exe` resolved to `/usr/bin/scribe-dev-server` with no
`(deleted)` suffix and inode `109185926`, equal to the installed file. The
installed client PID 1727541 similarly matched inode `109185922`. Stable PIDs
13871 and 687754 remained `/usr/bin/scribe-server` and
`/usr/bin/scribe-client`; neither was signalled.

## Zero-gap coverage matrix

Every canonical row appears once. `PASS` means its required proof tier is
present and the installed-byte bridge is intact; child status was not used as
a verdict input.

| ID | Normative requirement | Code / Rust | Visual | Functional | Docs | Installed | Verdict |
| --- | --- | --- | --- | --- | --- | --- | --- |
| SCOPE-1 | A2 and A3 are the only normative production sections; page scaffolding, CURRENT, and standalone A are reference-only. | M-contract | V-inventory | F-suite | D-sync; `scribe-zwtv.2` | I-PKG; I-A2; I-A3 | PASS |
| SCOPE-2 | Session `01a01227-1e60-78a5-ac9d-58a63aef7ead` is recorded as provenance; no fabricated dialogue is quoted. | M-contract | V-inventory | F-suite | D-sync; `scribe-zwtv.2` | I-PKG; I-A2; I-A3 | PASS |
| SCOPE-3 | Every normative mock selector/state must map to an implementation, visual/functional oracle, and owner before materialization. | M-contract | V-inventory | F-suite | D-sync; `scribe-zwtv.6` | I-PKG; I-A2; I-A3 | PASS |
| SCOPE-4 | Literal hex values are mock-theme references; production uses live-theme roles and contrast solving, never hardcoded board colors. | C-theme; R-contrast | V-theme | F-suite | D-sync; `scribe-zwtv.13` | I-PKG; I-A2; I-A3 | PASS |
| SCOPE-5 | Final specs and `lat.md/` describe landed A2/A3 behavior. | D-sync | V-inventory | F-suite | D-sync; `scribe-zwtv.16` | I-PKG; I-A2; I-A3 | PASS |
| A2-S1 | **Collapsed — real state:** Backlog and Ready may be empty; In progress receives the work width; Blocked and Done remain full-strength 36px tabs with counts. | C-A2-layout; R-A2-layout | V-A2-collapsed | F-geometry | D-sync; `scribe-zwtv.7` | I-PKG; I-A2 | PASS |
| A2-S2 | **Hovering the Blocked tab:** drawer overlays lanes without changing their bounds; tab becomes hot and drawer says `click to pin`. | C-A2-drawer; R-A2-drawer | V-A2-hover | F-drawer | D-sync; `scribe-zwtv.8` | I-PKG; I-DRAWER | PASS |
| A2-S3 | **Blocked pinned — busy state, Done still a tab:** four visible lanes use three equal active shares plus a `0.85` pinned share; `×` unpins. | C-A2-layout; C-A2-pin; R-A2-layout | V-A2-pinned | F-drawer | D-sync; `scribe-zwtv.8` | I-PKG; I-DRAWER | PASS |
| A2-S4 | **Dragging a card:** source dims, 320×36 ghost follows the pointer, collapsed Done becomes the accepted close target, and collapsed Blocked remains a rejected target. | C-A2-drag; R-A2-drag | V-A2-drag | F-drag | D-sync; `scribe-zwtv.9` | I-PKG; I-A2 | PASS |
| A2-S5 | Empty lane: seam and count dim, copy is queue-specific, and no empty card outline appears. Ready may add the subordinate blocked count shown by the mock. | C-A2-layout; R-A2-layout | V-A2-empty | F-geometry | D-sync; `scribe-zwtv.7` | I-PKG; I-A2 | PASS |
| A2-S6 | Overflow lane: only whole rows show and `⌄` marks hidden rows; no clipped partial row is visible. | C-A2-layout; R-A2-layout | V-A2-overflow | F-geometry | D-sync; `scribe-zwtv.7` | I-PKG; I-A2 | PASS |
| A2-S7 | Row hover/focus: background lifts and a 2px lane-hue underline replaces the lower separator without doubling the next separator. | C-A2-paint; R-A2-layout | V-A2-row | F-geometry | D-sync; `scribe-zwtv.7` | I-PKG; I-ROW | PASS |
| A2-S8 | One-epic lane/drawer: when every visible issue shares one epic, show it once in the head and omit it from rows; mixed lanes keep per-row epic text. | C-A2-layout; R-A2-layout | V-A2-collapsed | F-geometry | D-sync; `scribe-zwtv.4` | I-PKG; I-A2 | PASS |
| A2-G1 | Default board height is 197px. Lanes use `5px 10px 7px 44px` padding and 16px track gaps; the bottom 3px floor is the resize grip. | C-A2-layout; R-manifest | V-A2-geometry | F-geometry | D-sync; `scribe-zwtv.4` | I-PKG; I-A2 | PASS |
| A2-G2 | Text controls sit at left 8px/top 5px as borderless 12×17px `+` and `−` glyphs with a 1px gap. | C-A2-paint; R-manifest | V-A2-scale | F-geometry | D-sync; `scribe-zwtv.7` | I-PKG; I-A2 | PASS |
| A2-G3 | Header grouping is a 24px hairline band; each lane head is 17px high and its state seam is 2px. | C-A2-paint; R-manifest | V-A2-geometry | F-geometry | D-sync; `scribe-zwtv.7` | I-PKG; I-A2 | PASS |
| A2-G4 | A row is 51px: 19px title line, 15px subline, 4px interline gap. Default body is 153px and therefore exactly three rows. | C-A2-layout; R-manifest | V-A2-geometry | F-geometry | D-sync; `scribe-zwtv.4` | I-PKG; I-A2 | PASS |
| A2-G5 | Row grid is 20px priority + 6px gap + title. Subline is three columns: ID left, age at the true center, epic right with at least 12px separation. | C-A2-paint; R-manifest | V-A2-geometry | F-geometry | D-sync; `scribe-zwtv.7` | I-PKG; I-A2 | PASS |
| A2-G6 | Lane count sits beside its name, not at the far edge. A common epic, when present, is the right-aligned head item. | C-A2-paint; R-A2-layout | V-A2-geometry | F-geometry | D-sync; `scribe-zwtv.7` | I-PKG; I-A2 | PASS |
| A2-G7 | Blocked and Done rail tabs are 36px wide. Labels are one glyph per 10.5px line, never rotated text; count/head seam/cue occupy the mock order. | C-A2-drawer; R-manifest | V-A2-geometry | F-geometry | D-sync; `scribe-zwtv.8` | I-PKG; I-A2 | PASS |
| A2-G8 | Drawer bounds are top 5px, bottom 4px, right 96px, width 452px, 13px horizontal padding, 1px border, and 3px radius. | C-A2-drawer; R-manifest | V-A2-hover | F-geometry | D-sync; `scribe-zwtv.8` | I-PKG; I-DRAWER | PASS |
| A2-G9 | Overflow chevron is 10px at right 1px/bottom 0. Floor is 3px with a centered 34×1px grip at top 1px. | C-A2-paint; R-manifest | V-A2-geometry | F-geometry | D-sync; `scribe-zwtv.7` | I-PKG; I-A2 | PASS |
| A2-G10 | At non-default board heights, compute the largest whole 51px row count that fits after head and floor; leave remainder as ground and never show a partial row. | C-A2-layout; R-A2-layout | V-A2-resize | F-geometry | D-sync; `scribe-zwtv.10` | I-PKG; I-A2 | PASS |
| A2-C1 | Ground is the tab-bar chrome slot; hairline and strong hairline are the theme-derived structural rules; title, muted, and quiet are distinct text roles. | C-theme; R-contrast | V-theme; V-A2-states | F-geometry | D-sync; `scribe-zwtv.7` | I-PKG; I-A2 | PASS |
| A2-C2 | Queue roles are Backlog, Ready, In progress, Blocked, Done. Header labels mix 40% queue hue toward chrome ink; empty labels use the 32% muted treatment. | C-theme; C-A2-paint; R-contrast | V-theme; V-A2-states | F-geometry | D-sync; `scribe-zwtv.7` | I-PKG; I-A2 | PASS |
| A2-C3 | Priority roles are P0 red, P1 amber, P2 yellow, P3 neutral-high, P4 neutral-low; only the priority glyph is saturated row ink. | C-theme; C-A2-paint; R-contrast | V-theme; V-A2-states | F-geometry | D-sync; `scribe-zwtv.7` | I-PKG; I-A2 | PASS |
| A2-C4 | Normal/empty counts use the mock's `#cdd3dd` / `#767d8a` roles; empty tab count uses `#5c636f`. IDs use `#7a828f`, ages `#6b7280`, epics `#767d8a`. | C-theme; C-A2-paint; R-contrast | V-theme; V-A2-states | F-geometry | D-sync; `scribe-zwtv.7` | I-PKG; I-A2 | PASS |
| A2-C5 | Lane seam runs queue hue to 12% of it; empty seam runs 34% to 9%. Row hover is a subtle lift plus lane-hue underline. | C-theme; C-A2-paint; R-contrast | V-theme; V-A2-states | F-geometry | D-sync; `scribe-zwtv.7` | I-PKG; I-A2 | PASS |
| A2-C6 | Hot tab uses lifted ground, bright cue/spine, and a 1px queue-hue inner edge; nonempty collapsed counts/hues remain full strength. | C-theme; C-A2-drawer; R-contrast | V-theme; V-A2-states | F-geometry | D-sync; `scribe-zwtv.8` | I-PKG; I-DRAWER | PASS |
| A2-C7 | Drawer uses raised ground, strong hairline, and left shadow; drag ghost uses chip ground, stronger hairline, and shadow. Accepted Done target uses done-hue wash and lifted text. | C-theme; C-A2-drag; R-contrast | V-theme; V-A2-states | F-geometry | D-sync; `scribe-zwtv.8` | I-PKG; I-A2 | PASS |
| A2-C8 | Floor and horizontal grip use the subtle-lift and grip roles; zoom glyph is quiet and lifts to title ink on hover/focus. | C-theme; C-A2-paint; R-contrast | V-theme; V-A2-states | F-geometry | D-sync; `scribe-zwtv.7` | I-PKG; I-A2 | PASS |
| A2-C9 | All text clears 4.5:1 on its actual ground and all state marks/controls clear 3:1; already-compliant theme values remain unchanged. | C-theme; R-contrast | V-theme | F-geometry | D-sync; `scribe-zwtv.10` | I-PKG; I-A2 | PASS |
| A2-I1 | Hover/focus opens one transient collapsed-lane drawer over the lanes; leaving both tab and drawer closes it after the existing board hover grace. | C-A2-drawer; R-A2-drawer | V-A2-hover | F-drawer | D-sync; `scribe-zwtv.5` | I-PKG; I-DRAWER | PASS |
| A2-I2 | Click or Enter/Space pins one drawer; pinning the other replaces it; `×` or reactivation unpins. | C-A2-pin; R-A2-drawer | V-A2-pinned; V-A2-row | F-drawer | D-sync; `scribe-zwtv.5` | I-PKG; I-DRAWER | PASS |
| A2-I3 | Drawer/tab accessible names include lane name, count, collapsed/pinned state, and “focus opens; activate pins/unpins.” Visible keyboard focus matches hot-state prominence. | C-A2-a11y; R-A2-drawer | V-A2-pinned; V-A2-row | F-drawer | D-sync; `scribe-zwtv.10` | I-PKG; I-DRAWER | PASS |
| A2-I4 | Row pointer click, Enter, or AccessKit Click opens detail; epic-backed rows also request Flow. Full title remains available by tooltip and accessible name. | C-A2-a11y; C-Flow-state; R-Flow-state | V-A2-states | F-flow | D-sync; `scribe-zwtv.14` | I-PKG; I-FLOW | PASS |
| A2-I5 | Pointer drag keeps the existing >2px threshold, eligible source lanes, five target semantics, native ghost, PTY isolation, guarded writes, optimistic overlay, and authoritative settlement. | C-A2-drag; C-write; R-A2-drag | V-A2-drag | F-drag | D-sync; `scribe-zwtv.9` | I-PKG; I-A2 | PASS |
| A2-I6 | Keyboard move uses Space grab, Left/Right named targets, Enter/Space drop, and Escape cancel through the same guard/write functions as pointer drag. | C-A2-key; C-write; R-A2-key | V-A2-states | F-keymove | D-sync; `scribe-zwtv.9` | I-PKG; I-A2 | PASS |
| A2-R1 | Per-strip allocation follows the closed narrow policy above; tabs and controls keep fixed geometry, text ellipsizes, and A2 never scrolls horizontally. | C-A2-layout; R-A2-layout | V-A2-narrow | F-resize | D-sync; `scribe-zwtv.4` | I-PKG; I-A2 | PASS |
| A2-R2 | If a pinned lane would starve the three active lanes, it auto-collapses without deleting its persisted preference; it restores when the region again fits. | C-A2-layout; R-A2-layout | V-A2-narrow | F-resize | D-sync; `scribe-zwtv.10` | I-PKG; I-A2 | PASS |
| A2-R3 | Text scale remains 0.8–1.6 per window; track allocation and whole-row count recompute without changing the stored board height. | C-A2-layout; C-Flow-state; R-A2-layout | V-A2-scale | F-resize | D-sync; `scribe-zwtv.10` | I-PKG; I-A2 | PASS |
| A2-L1 | Hover/focus drawer state is per workspace and transient. Pinned collapsed lane is per workspace, persisted, exclusive, and cleared only by the lifetime rules above. | C-A2-pin; R-A2-drawer | V-A2-states | F-drawer | D-sync; `scribe-zwtv.5` | I-PKG; I-DRAWER | PASS |
| A2-L2 | Board pin, board height, and lane pin survive A2→A3→A2 unchanged; separate regions never share them. | C-A2-pin; C-Flow-state; R-Flow-state | V-A2-states | F-isolation; F-restart | D-sync; `scribe-zwtv.10` | I-PKG; I-FLOW | PASS |
| A2-BD1 | `Ready` board snapshot renders A2, omits epic records as rows, retains parent epic metadata, counts non-epic issues, and preserves authoritative newest-created-first order. | C-server-board; R-server-board | V-A2-states | F-board | D-sync; `scribe-zwtv.14` | I-PKG; I-A2 | PASS |
| A2-BD2 | Ordinary board items carry the tracker timestamp needed for compact relative age; every A2 row, including standalone issues, renders that age without a new `bd` command. | C-protocol; C-A2-layout; R-protocol | V-A2-states | F-board | D-sync; `scribe-zwtv.3` | I-PKG; I-A2 | PASS |
| A2-BD3 | `Loading` without last-good data does not invent rows; a refresh with last-good data keeps it until `Ready`. | C-server-board; C-Flow-state; R-server-board | V-A2-states | F-board | D-sync; `scribe-zwtv.14` | I-PKG; I-A2 | PASS |
| A2-BD4 | `NotDetected` removes that workspace's board, drawer, lane pin, drag, and Flow state without affecting another region. | C-Flow-state; R-Flow-state | V-A2-states | F-isolation | D-sync; `scribe-zwtv.14` | I-PKG; I-A2 | PASS |
| A2-BD5 | `Unavailable` preserves last-good board/pin state and remains retryable; it is not treated as `NotDetected`. | C-server-board; C-Flow-state; R-server-board | V-A2-states | F-write; F-board | D-sync; `scribe-zwtv.14` | I-PKG; I-A2 | PASS |
| A2-BD6 | Ready drop sends guarded open with defer clear; In-progress sends Claim; Done sends CloseIssue; Backlog, Blocked, source, and no target send no write. | C-write; C-A2-drag; R-A2-drag | V-A2-states | F-drag | D-sync; `scribe-zwtv.9` | I-PKG; I-A2 | PASS |
| A2-BD7 | Applied settles from authoritative refresh; precondition failure reports conflict and refreshes; failure rolls back; timeout/reconnect block duplicates until reconciliation; classifier-selected lane wins. | C-write; C-Flow-state; R-server-board | V-A2-states | F-write; F-board | D-sync; `scribe-zwtv.14` | I-PKG; I-A2 | PASS |
| A3-S1 | **Opened issue:** A2 row click opens panel and A3; cursor is unique; band shows return control, epic, inert chevron, closed/total tally, progress, opened id, and mode pair. | C-A3-render; C-Flow-state; R-A3-layout | V-A3-open | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-S2 | **Hover/focus trace:** ancestor and descendant closure stays full opacity, other nodes dim to 0.24, on-path wire intervals brighten, other intervals dim, and chip states transitive counts. | C-A3-trace; R-A3-trace | V-A3-trace | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-TRACE | PASS |
| A3-S3 | **Deeper epic at origin:** four-row frontier fits, right edge fades, and horizontal position bar appears because content exceeds the strip. | C-A3-layout; R-A3-layout | V-A3-deep | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-S4 | **Wheeled into the graph:** rank ruler and canvas move together, both clipped edges fade, position thumb moves, and no vertical scrollbar appears. | C-A3-layout; C-Flow-state; R-A3-layout | V-A3-overflow | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-S5 | Done node recedes; Ready and Blocked are hollow; ordinary In progress and Backlog are filled; live treatment overrides queue paint; cursor and trace are independent overlays. | C-A3-render; R-A3-live | V-A3-open; V-A3-live | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-G1 | Default 197px budget is exactly band 34 + ruler 15 + graph 139 + hbar 2 + gap 4 + floor 3. | C-A3-layout; R-manifest | V-A3-open | F-flow | D-sync; `scribe-zwtv.2` | I-PKG; I-A3 | PASS |
| A3-G2 | Node is 214×24px with 6px horizontal padding/gap, 8px dot, 9.5px mono priority/id, and 12px ellipsized title on one line. | C-A3-render; R-manifest | V-A3-open | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-G3 | Gutter is 28px, rank pitch 242px, row gap 10px, row pitch 34px, and graph left padding 30px. | C-A3-layout; R-A3-layout | V-A3-open | F-flow | D-sync; `scribe-zwtv.2` | I-PKG; I-A3 | PASS |
| A3-G4 | Row capacity is 5 at scale 0.8, 4 at 1.0, and 2 at 1.6 in the fixed 139px graph band. | C-A3-layout; R-A3-layout | V-A3-deep; V-A2-scale | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-G5 | Adjacent wires are orthogonal half-gutter stubs/dogleg; skip edges use intermediate lanes; every endpoint lands on the node dot center. Shared translucent intervals paint once. | C-A3-layout; R-A3-layout | V-A3-open | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-G6 | Band padding is 14px left/10px right with 10px gaps. Progress is 150×2px. Rank ruler begins at y=34 and graph at y=49. | C-A3-render; R-manifest | V-A3-open | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-G7 | Chip uses 3px vertical/7px horizontal padding, 2px radius, and remains anchored to its node while scrolled. | C-A3-trace; R-manifest | V-A3-trace; V-A3-overflow | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-G8 | Clipped-edge fades are 48px over the graph band. Hbar is 2px at y=188. Floor is 3px with the same centered 34×1px grip as A2. | C-A3-render; C-A3-layout; R-manifest | V-A3-overflow | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-G9 | A3 never changes stored board height. Below the 197px minimum it stays in A2; above it, the normative A3 module remains top-anchored and surplus area is board ground above the bottom floor. | C-A3-render; C-Flow-state; R-Flow-state | V-A3-open; V-A2-resize | F-resize; F-restart | D-sync; `scribe-zwtv.10` | I-PKG; I-A3 | PASS |
| A3-C1 | Band is subtle lifted ground with strong lower hairline; epic/title, chevron/muted, tally/title, total/muted, progress track, and done-hue fill are separate roles. | C-theme; C-A3-render; R-contrast | V-theme; V-A3-open | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-C2 | Mode inactive/active/background roles remain distinct; `FLOW` is selected, `LANES` is actionable, and `← LANES` uses muted control ink. | C-theme; C-A3-render; R-contrast | V-theme; V-A3-open | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-C3 | Rank label, base wire, dim wire, and traced wire are four roles; traced wire and cursor keyline use title ink. | C-theme; C-A3-layout; R-contrast | V-theme; V-A3-trace | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-C4 | Ordinary node title/id/hover are distinct roles. Done uses filled done dot, muted title, and 0.6 priority; Ready/Blocked use hollow state rings, with Blocked title lifted. | C-theme; C-A3-render; R-A3-live | V-theme; V-A3-open | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-C5 | Non-live In progress uses filled progress dot; Backlog uses filled backlog/muted dot. Both retain ordinary title and never show an agent line or halo from assignment alone. | C-theme; C-A3-render; R-A3-live | V-theme; V-A3-open | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-C6 | Live uses filled progress dot, 3px 20%-strength progress halo, lifted 650 title, agent ink, and 4px progress status dot. Missing assignee suppresses only the agent text. | C-theme; C-A3-render; R-A3-live | V-A3-live | F-live | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-C7 | Cursor uses subtle fill plus 2px title-ink left keyline. Trace dims off-path nodes to 0.24 without altering geometry. | C-theme; C-A3-trace; R-A3-trace | V-A3-trace | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-TRACE | PASS |
| A3-C8 | Chip uses lifted ground, strong hairline, and body ink. Edge fade resolves into live ground; hbar track/thumb and floor/grip use distinct lift roles. | C-theme; C-A3-render; R-contrast | V-theme; V-A3-overflow | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-C9 | Text and marks meet the same 4.5:1 / 3:1 floors as A2 across live themes. | C-theme; R-contrast | V-theme; V-A3-open | F-flow | D-sync; `scribe-zwtv.13` | I-PKG; I-A3 | PASS |
| A3-I1 | Successful epic-backed A2 activation opens panel and Flow together; only the clicked workspace changes mode. | C-Flow-state; R-Flow-state | V-A3-open | F-flow | D-sync; `scribe-zwtv.14` | I-PKG; I-FLOW | PASS |
| A3-I2 | Node pointer click, Enter/Space, or AccessKit Click moves cursor and retargets an open panel without fetching/re-ranking the graph; cursor reactivation is a no-op. | C-A3-a11y; C-Flow-state; R-Flow-state | V-A3-open | F-flow | D-sync; `scribe-zwtv.14` | I-PKG; I-A3 | PASS |
| A3-I3 | Hover and keyboard focus apply the same path trace. Leaving/blur restores all nodes and wires in one frame; reduced motion lands on the same frame. | C-A3-trace; R-A3-trace | V-A3-trace | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-TRACE | PASS |
| A3-I4 | `← LANES` and `LANES` are Buttons with visible focus and Enter/Space activation. `FLOW` exposes selected/current state but no action. Epic chevron is hidden from interaction/accessibility. | C-A3-a11y; R-A3-controls | V-A3-controls | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-FLOW | PASS |
| A3-I5 | Nodes are Buttons and Tab stops ordered rank-left-to-right then top-to-bottom. Name is `<id> <title>, <state>` plus liveness; description names blockers/dependents and trace counts. | C-A3-a11y; R-A3-a11y | V-A3-open | F-flow | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-I6 | Tab/Shift+Tab auto-scrolls the focused node into view. No arrow-key graph traversal, zoom, pan, node dragging, or dependency editing exists. | C-Flow-state; R-Flow-state | V-A3-keyboard | F-flow | D-sync; `scribe-zwtv.10` | I-PKG; I-A3 | PASS |
| A3-I7 | Wheel over Flow claims the gesture and maps either axis to clamped horizontal scroll; no wheel outside that workspace changes it. | C-Flow-state; R-Flow-state | V-A3-overflow | F-flow | D-sync; `scribe-zwtv.14` | I-PKG; I-A3 | PASS |
| A3-R1 | Flow is per workspace/region. A second region keeps its own mode, graph, cursor, scroll, trace, panel, and pin state. | C-Flow-state; R-Flow-state | V-A3-open | F-isolation | D-sync; `scribe-zwtv.10` | I-PKG; I-A3 | PASS |
| A3-R2 | Text-scale relayout preserves the graph only while every rank fits; failure exits to A2 without changing scale or stored board height. | C-A3-layout; C-Flow-state; R-A3-layout | V-A2-scale; V-A3-open | F-resize; F-restart | D-sync; `scribe-zwtv.10` | I-PKG; I-A3 | PASS |
| A3-L1 | Pending entry exists only until matching reply, exit, workspace loss, `NotDetected`, capability loss, or window close; late replies cannot reopen Flow. | C-Flow-state; R-Flow-state | V-A3-open | F-flow | D-sync; `scribe-zwtv.14` | I-PKG; I-A3 | PASS |
| A3-L2 | Graph, layout, cursor, scroll, and trace live only for the current window/open. Exit clears them; reopening requests a fresh complete graph. | C-Flow-state; R-Flow-state | V-A3-open | F-flow | D-sync; `scribe-zwtv.14` | I-PKG; I-A3 | PASS |
| A3-L3 | Liveness is session-scoped and window-local. It clears when focus changes, state clears, session ends, or owner disconnects; two live sessions on one issue keep the halo until both clear. | C-A3-render; C-Flow-state; R-A3-live | V-A3-live | F-live | D-sync; `scribe-zwtv.11` | I-PKG; I-A3 | PASS |
| A3-L4 | Board pin, board height, A2 lane pin, and text scale are outside Flow and survive its round trip. Flow mode itself does not survive window restart. | C-A2-pin; C-Flow-state; R-Flow-state | V-A3-open | F-resize; F-restart | D-sync; `scribe-zwtv.10` | I-PKG; I-FLOW | PASS |
| A3-BD1 | `Graph` is complete and epic-scoped, includes closed members omitted by capped Done lanes, and includes satisfied `blocks` edges omitted by `bd blocked`. | C-server-graph; R-server-graph | V-A3-open | F-admission | D-sync; `scribe-zwtv.14` | I-PKG; I-A3 | PASS |
| A3-BD2 | `NoEpic` (non-epic id, vanished epic, or wrong workspace root) leaves A2 and panel usable and paints no empty Flow frame. | C-server-graph; C-Flow-state; R-server-graph | V-A3-open | F-admission | D-sync; `scribe-zwtv.14` | I-PKG; I-A3 | PASS |
| A3-BD3 | `Disconnected` and `ExternalBlocker` are real-`bd` refusal fixtures; both remain A2 and are logged diagnostically. | C-server-graph; C-Flow-state; R-server-graph | V-A3-open | F-admission | D-sync; `scribe-zwtv.14` | I-PKG; I-A3 | PASS |
| A3-BD4 | `Cycle` is a typed refusal proven in memory because official `bd` rejects cycle creation; it has no renderer layout or fake E2E fixture. | C-server-graph; C-A3-layout; R-server-graph | V-A3-open | F-admission | D-sync; `scribe-zwtv.14` | I-PKG; I-A3 | PASS |
| A3-BD5 | `TooLarge` returns only its whole-graph refusal; it never exposes nodes or a `truncated` flag. | C-server-graph; C-protocol; R-server-graph | V-A3-open | F-admission | D-sync; `scribe-zwtv.14` | I-PKG; I-A3 | PASS |
| A3-BD6 | `Unavailable { message }` leaves A2 usable, clears the pending entry, and may be retried by reopening; it never replaces a frozen graph already on screen. | C-server-graph; C-Flow-state; R-server-graph | V-A3-open | F-flow | D-sync; `scribe-zwtv.14` | I-PKG; I-A3 | PASS |
| A3-BD7 | Missing/false `beads_flow`, remote/shared ownership, protocol mismatch, or capability loss never enters Flow and never weakens A2/detail gating. | C-protocol; C-Flow-state; R-protocol | V-A3-open | F-flow; F-isolation | D-sync; `scribe-zwtv.14` | I-PKG; I-A3 | PASS |
| A3-BD8 | Graph assembly reuses the server-owned full board read and generation fence; it adds no client `bd` process and no second tracker invocation. | C-server-graph; R-server-graph | V-A3-open | F-admission | D-sync; `scribe-zwtv.14` | I-PKG; I-A3 | PASS |

## Clean-tree command results

| Command | Result | Direct result |
| --- | --- | --- |
| `just ready` | PASS | Ratchets, format, clippy, and workspace tests passed. |
| `lat check` | PASS | All checks passed. |
| `python3 .impeccable/mocks/check-contract.py` | PASS | Fresh manifest, 94 ownership rows, state/interaction inventories, drift checks, and `check-flow.py` passed. |
| `pre-commit run --all-files` | PASS | All 12 hooks passed. |
| `just docker-visual` | PASS | Rebuilt release binaries and `scribe-test-visual`; this rebuild preceded the trusted visual run. |
| `just e2e-visual-beads-board` | PASS | 31 captures; all eight named states; geometry/theme/focus/overflow inventory passed. |
| `just e2e-func-beads-board` | PASS | Rebuilt its derived image, then all real-`bd` board, write, Flow, liveness, resize, isolation, and restart phases passed. |
| `cd /home/mamba/work/scribe && bash tests/install/dev-package-smoke.sh` | PASS | Exit 0; 78 PASS checks; source/package/installed payload match. |
| Installed `/usr/bin/scribe-dev` inspection | PASS | Fresh independent A2/A3/drawer/pin/row/trace/control captures and geometry measurements passed. |

The visual run above was rebuild-preceded. The functional recipe also rebuilt
its derived official-`bd` image before execution. No result from
`test-output/zwtv17/` was used.

## Installed evidence hashes

| Artifact | SHA-256 |
| --- | --- |
| `01-test-beads-terminal.png` | `6ed086564ac1860f86051b22ebf2936e71184815977e82be55d3aa58bd5640f4` |
| `03-a3-installed-opened.png` | `ba6224acbe1647b734c060592d01737b0558bd2321a3786da850958d35adb2ce` |
| `05-a2-installed-back-return.png` | `4462ac88ff51cf96228a27f0592722884d3a9a1e4fb91d86743fd147ea4358e3` |
| `06-a2-installed-blocked-hover.png` | `d2a5c01792fc7d77a93a3e41c87b8c330bc18319aad5794da3721e793d3f90b4` |
| `08-a2-installed-blocked-pinned.png` | `34c98afe7233dd8e27e76c30cfaf42c79b01b78b5698e7d0e3efaa2cbc5c1f14` |
| `09-a2-installed-blocked-unpinned.png` | `c01dc71a6c02644c1a87b82d786d9e8cf6419e4115701fdc990bc7598eeca6b1` |
| `10-a3-installed-before-lanes-control.png` | `318845ada8ea9f97e20d370926df068fc3310daca4ed6f8b2674244f2c2c596c` |
| `11-a2-installed-lanes-control-return.png` | `4f405fc099d4f9546c56e7f01744f7c02d71ba4bd152ce3579903c72c216a5ec` |
| `12-a3-installed-trace.png` | `e7ae18ae894fcce91acea7061a4be687b68799a5e4db3c51b54192416a37876f` |
| `13-a2-installed-row-hover.png` | `8f8cdd36795a82a1770099e7225722ecc0912e42a96b9723363c24d552f17d7a` |
| `installed-geometry.json` | `956d58aca51b60c840fe80fe038b7b803056f822c9e896e3e7dd33040f7b4cca` |
| `package-smoke.log` | `1eed275fd21940536a0df4a7cf3fbc0871369ed40bc1dac19570fb5e6a9b34be` |

## Obsolete-source review

- Production drift checker found no CURRENT/standalone-A markers, raised-ledger
  paint restoration, five-equal-track division, or shell geometry restatement.
- `specs/026-beads-flow-view.md` is explicitly superseded and contains no
  searchable retired requirements.
- `lat.md/` describes the adaptive A2 rail, no A2 scroll axis, complete typed
  A3 outcomes, active return controls, fixed-height Flow, and rebuilt evidence.
- References to “raised card” or “five equal” remaining in source/tests are
  explanatory prevention comments, not authoritative behavior.
- Example issue title “Preserve lane scroll” remains inside the approved mock's
  fixture content; it is card copy, not a design requirement. No contract or
  implementation grants A2 a scroll axis.

## Residual risk

- Quill session `01a01227-1e60-78a5-ac9d-58a63aef7ead` is inaccessible in this
  environment. This is not treated as a gap: the approved HTML is the artifact
  produced by those decisions, the canonical closed-decision table records the
  non-visual policy, and no dialogue is reconstructed. Audit compared those
  artifacts directly and found no unsupported sibling of the removed A2 lane
  scroll contradiction.
- Installed manual capture used Linux/X11, the live dark theme, and the
  admissible three-node `test-beads-8fj` epic. Wider/deeper/theme/negative and
  lifecycle matrices ran in freshly rebuilt Docker images, while the 78-check
  package smoke proves the installed client/server are byte-identical to the
  source/package binaries those suites exercise. Residual platform/display
  variance is bounded; no unproved A2/A3 contract row remains.
- Capture files live under ignored `test-output/zwtv18-installed/`; hashes and
  measured geometry are recorded above in this durable report.

## Verdict

**PASS — zero gaps.** All 94 normative rows have owned implementation and direct
machine/Rust, visual, functional, documentation, and installed-package
traceability. All required clean-tree gates passed. Installed A2/A3 geometry
matches the generated canonical contract within the existing visual-oracle
tolerance. No blocker was found, so no blocking child is required.
