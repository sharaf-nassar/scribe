# Beads board A2/A3 production contract

## Status and authority

**Approved. Canonical production contract.**

Decision provenance is Quill session
`01a01227-1e60-78a5-ac9d-58a63aef7ead`. The session transcript is not
available in this worktree; this contract records the decisions embodied in
its approved artifact, `.impeccable/mocks/beads-board-directions.html`, without
inventing quotations.

Only these mock sections are normative for the production Beads board:

- **A2 · Ledger + rail** — the Lanes rendering.
- **A3 · Flow** — the dependency-graph rendering.

The HTML page scaffolding, explanatory page chrome, terminal peeks, section
**CURRENT**, and standalone **A · Ledger** are reference-only. They are not
fallback designs, acceptance baselines, or sources of production geometry.
`specs/026-beads-flow-view.md` is superseded in full by this contract.

For A2 and A3, the mock governs hierarchy, geometry, visible states, and color
roles. This contract governs section scope, behavior, accessibility,
responsive policy, state lifetime, and real-`bd` outcomes. Literal mock colors
are the reference rendering under the mock theme; production resolves the
same named roles from the live theme and preserves WCAG contrast.

## Closed decisions

| Decision | Contract | Owner bead | Planned oracle |
| --- | --- | --- | --- |
| Narrow-region allocation | Reserve the 44px left control gutter, 10px right padding, 16px inter-track gaps, and each unpinned 36px rail tab first. Give empty visible lanes only their measured legible header width; divide the remaining width equally among nonempty Backlog, Ready, and In progress lanes. A pinned Blocked or Done lane receives `0.85` of an active-lane share. On further narrowing, auto-collapse the pinned lane before shrinking the three drag-source lanes; then keep all three lanes present and ellipsize row text. A2 never gains a horizontal scrollbar. | `scribe-zwtv.4` | Pure width matrix and narrow visual matrix. |
| Number of pinned collapsed lanes | At most one of Blocked and Done is pinned. Pinning one replaces the other. The normative pinned state is therefore “one lane pinned, the other still a 36px tab”; a two-pinned layout does not exist. | `scribe-zwtv.5` | Per-workspace pin state matrix. |
| Lane-pin lifetime | The selected pinned lane is per workspace and persists with the window geometry record, like the board pin. It survives tab switches, Flow round trips, text-scale changes, resize, and window restart. Explicit unpin, workspace removal/`NotDetected`, or region removal clears it. Hover/focus-open drawers are transient and never persist. | `scribe-zwtv.5` | Restart, cleanup, and region-isolation tests. |
| Hover-drawer keyboard equivalent | A collapsed tab is a named Button and Tab stop. Pointer hover or keyboard focus opens the same non-reflowing drawer. Enter/Space pins it; activating the pinned tab or its `×` control unpins it. Escape closes only a transient drawer. | `scribe-zwtv.5` | AccessKit and pointer/focus equivalence tests. |
| Drag keyboard equivalent | A2 rows are keyboard reachable. Enter and the AccessKit Click action open the issue. Space on an eligible Backlog, Ready, or In-progress row grabs it; Left/Right visits the five named lane targets, including collapsed tabs; Enter/Space drops through the same guarded write path; Escape cancels and restores focus. Blocked and Done rows cannot be grabbed. Accepted and rejected targets are announced, and no key reaches the PTY while the move is armed. | `scribe-zwtv.9` | Real-`bd` keyboard move and PTY-isolation matrix. |
| Flow entry and panel | Opening an epic-backed A2 row opens or retargets the existing detail panel **and** requests A3 for that epic. A missing capability, missing epic, refusal, or unavailable graph leaves A2 in place while the panel remains usable. | `scribe-zwtv.14` | Functional dual-request and fallback run. |
| Flow graph lifetime | A3 is frozen per successful open. Board polling continues but never re-ranks or replaces the visible graph. Node activation changes only the cursor and open detail. Leaving and reopening Flow is the refresh action. | `scribe-zwtv.14` | Request-count and exit/reopen tests. |
| Non-live In-progress Flow node | Filled progress-hue dot, normal title weight/ink, no halo, no agent line. Assignment alone never implies liveness. A Backlog node uses a filled backlog/muted dot with the same ordinary title treatment. | `scribe-zwtv.11` | Visual undefined-state fixture. |
| Flow return controls | `← LANES` and the `LANES` member of the mode pair are real pointer, keyboard, and AccessKit controls that return to A2. The active `FLOW` member is a selected-state indicator and a no-op. Escape returns to A2 only after the detail panel declines Escape. | `scribe-zwtv.11` | Pointer, keyboard, AccessKit, and Escape-precedence tests. |
| Epic chevron | Remains inert. It is visual punctuation only: no pointer cursor, hover state, focus stop, AccessKit action, picker, or epic switching. | `scribe-zwtv.11` | Inert-chrome visual and AccessKit assertion. |
| Graph overflow | Width scrolls; height does not. Either wheel axis over A3 moves horizontal position, clamped to content. Keyboard focus auto-scrolls the focused node into view. Edge fades and the 2px position bar appear only when content is clipped. | `scribe-zwtv.11` | Origin/middle overflow visual matrix. |
| Rank overflow | If a rank exceeds the row budget at current text scale, Flow is not painted (or exits on relayout) and A2 remains usable. The board height is never grown to rescue the graph. | `scribe-zwtv.10` | Scale and rank-overflow functional matrix. |
| Cycles and malformed graphs | Never park cycle members. Cycle, disconnected membership, external blockers, and over-bound graphs are typed admission refusals. The renderer accepts only complete admitted DAGs. | `scribe-zwtv.14` | Server/layout refusal tests. |
| Flow result shape | Outcomes are mandatory and typed: complete `Graph`, `NoGraph` with `NoEpic`, `Cycle`, `Disconnected`, `ExternalBlocker`, or `TooLarge`, and `Unavailable { message }`. No optional graph and no `truncated` flag exist. | `scribe-zwtv.14` | Protocol round trip and real-`bd` outcome matrix. |

## Coverage map

Every row below is normative and has one owner bead. “Visual” means the
A2/A3 matrix in `tests/e2e/visual/beads-board.sh`; “functional” means the
real-`bd` path in `tests/e2e/func/beads-board.sh`; “machine” means the generated
mock/contract checker owned by `scribe-zwtv.2`. `scribe-zwtv.15` wires those
oracles into CI, `scribe-zwtv.17` repeats them against packaged `scribe-dev`,
and `scribe-zwtv.18` performs the final independent audit.

### Boundary and source contract

| ID | Normative requirement | Owner bead | Planned oracle |
| --- | --- | --- | --- |
| SCOPE-1 | A2 and A3 are the only normative production sections; page scaffolding, CURRENT, and standalone A are reference-only. | `scribe-zwtv.2` | Machine section allowlist; spec review. |
| SCOPE-2 | Session `01a01227-1e60-78a5-ac9d-58a63aef7ead` is recorded as provenance; no fabricated dialogue is quoted. | `scribe-zwtv.2` | Machine metadata check. |
| SCOPE-3 | Every normative mock selector/state must map to an implementation, visual/functional oracle, and owner before materialization. | `scribe-zwtv.6` | Speckit materialization rejection fixture. |
| SCOPE-4 | Literal hex values are mock-theme references; production uses live-theme roles and contrast solving, never hardcoded board colors. | `scribe-zwtv.13` | Visual theme rewrite moves every sampled role. |
| SCOPE-5 | Final specs and `lat.md/` describe landed A2/A3 behavior without retaining the legacy five-lane contract. | `scribe-zwtv.16` | `lat check` plus stale-contract grep. |

### A2 named states

| ID | Mock state / normative requirement | Owner bead | Planned oracle |
| --- | --- | --- | --- |
| A2-S1 | **Collapsed — real state:** Backlog and Ready may be empty; In progress receives the work width; Blocked and Done remain full-strength 36px tabs with counts. | `scribe-zwtv.7` | Visual sparse real-state screenshot and pixel geometry. |
| A2-S2 | **Hovering the Blocked tab:** drawer overlays lanes without changing their bounds; tab becomes hot and drawer says `click to pin`. | `scribe-zwtv.8` | Visual before/hover bounds diff. |
| A2-S3 | **Blocked pinned — busy state, Done still a tab:** four visible lanes use three equal active shares plus a `0.85` pinned share; `×` unpins. | `scribe-zwtv.8` | Visual pinned matrix; functional pin/unpin state. |
| A2-S4 | **Dragging a card:** source dims, 320×36 ghost follows the pointer, collapsed Done becomes the accepted close target, and collapsed Blocked remains a rejected target. | `scribe-zwtv.9` | Visual ghost/target probes; functional write/no-write assertions. |
| A2-S5 | Empty lane: seam and count dim, copy is queue-specific, and no empty card outline appears. Ready may add the subordinate blocked count shown by the mock. | `scribe-zwtv.7` | Visual all-empty and sparse fixtures. |
| A2-S6 | Overflow lane: only whole rows show and `⌄` marks hidden rows; no clipped partial row is visible. | `scribe-zwtv.7` | Visual row-box/count measurement. |
| A2-S7 | Row hover/focus: background lifts and a 2px lane-hue underline replaces the lower separator without doubling the next separator. | `scribe-zwtv.7` | Visual hover and focus-visible captures. |
| A2-S8 | One-epic lane/drawer: when every visible issue shares one epic, show it once in the head and omit it from rows; mixed lanes keep per-row epic text. | `scribe-zwtv.4` | Pure presentation fixtures plus visual samples. |

### A2 geometry and type

| ID | Normative geometry | Owner bead | Planned oracle |
| --- | --- | --- | --- |
| A2-G1 | Default board height is 197px. Lanes use `5px 10px 7px 44px` padding and 16px track gaps; the bottom 3px floor is the resize grip. | `scribe-zwtv.4` | Pure layout fixtures; visual exact bounds. |
| A2-G2 | Text controls sit at left 8px/top 5px as borderless 12×17px `+` and `−` glyphs with a 1px gap. | `scribe-zwtv.7` | Visual control bounds and click targets. |
| A2-G3 | Header grouping is a 24px hairline band; each lane head is 17px high and its state seam is 2px. | `scribe-zwtv.7` | Visual horizontal-run measurements. |
| A2-G4 | A row is 51px: 19px title line, 15px subline, 4px interline gap. Default body is 153px and therefore exactly three rows. | `scribe-zwtv.4` | Pure row-count model; visual row-top measurements. |
| A2-G5 | Row grid is 20px priority + 6px gap + title. Subline is three columns: ID left, age at the true center, epic right with at least 12px separation. | `scribe-zwtv.7` | Visual alignment probes with and without epic. |
| A2-G6 | Lane count sits beside its name, not at the far edge. A common epic, when present, is the right-aligned head item. | `scribe-zwtv.7` | Visual wide-lane alignment. |
| A2-G7 | Blocked and Done rail tabs are 36px wide. Labels are one glyph per 10.5px line, never rotated text; count/head seam/cue occupy the mock order. | `scribe-zwtv.8` | Visual tab mask and text-line count. |
| A2-G8 | Drawer bounds are top 5px, bottom 4px, right 96px, width 452px, 13px horizontal padding, 1px border, and 3px radius. | `scribe-zwtv.8` | Visual overlay bounds with unchanged lanes. |
| A2-G9 | Overflow chevron is 10px at right 1px/bottom 0. Floor is 3px with a centered 34×1px grip at top 1px. | `scribe-zwtv.7` | Visual line/run measurements. |
| A2-G10 | At non-default board heights, compute the largest whole 51px row count that fits after head and floor; leave remainder as ground and never show a partial row. | `scribe-zwtv.10` | Resize visual matrix and pure height fixtures. |

### A2 color roles

| ID | Normative color-role inventory | Owner bead | Planned oracle |
| --- | --- | --- | --- |
| A2-C1 | Ground is the tab-bar chrome slot; hairline and strong hairline are the theme-derived structural rules; title, muted, and quiet are distinct text roles. | `scribe-zwtv.7` | Visual theme rewrite and contrast samples. |
| A2-C2 | Queue roles are Backlog, Ready, In progress, Blocked, Done. Header labels mix 40% queue hue toward chrome ink; empty labels use the 32% muted treatment. | `scribe-zwtv.7` | Five-label hue/contrast probes. |
| A2-C3 | Priority roles are P0 red, P1 amber, P2 yellow, P3 neutral-high, P4 neutral-low; only the priority glyph is saturated row ink. | `scribe-zwtv.7` | Visual five-priority fixture. |
| A2-C4 | Normal/empty counts use the mock's `#cdd3dd` / `#767d8a` roles; empty tab count uses `#5c636f`. IDs use `#7a828f`, ages `#6b7280`, epics `#767d8a`. | `scribe-zwtv.7` | Visual role sampling under two themes. |
| A2-C5 | Lane seam runs queue hue to 12% of it; empty seam runs 34% to 9%. Row hover is a subtle lift plus lane-hue underline. | `scribe-zwtv.7` | Gradient endpoint and hover probes. |
| A2-C6 | Hot tab uses lifted ground, bright cue/spine, and a 1px queue-hue inner edge; nonempty collapsed counts/hues remain full strength. | `scribe-zwtv.8` | Visual idle/hot/pinned tab matrix. |
| A2-C7 | Drawer uses raised ground, strong hairline, and left shadow; drag ghost uses chip ground, stronger hairline, and shadow. Accepted Done target uses done-hue wash and lifted text. | `scribe-zwtv.8` | Visual drawer/ghost/target samples. |
| A2-C8 | Floor and horizontal grip use the subtle-lift and grip roles; zoom glyph is quiet and lifts to title ink on hover/focus. | `scribe-zwtv.7` | Visual floor/control samples. |
| A2-C9 | All text clears 4.5:1 on its actual ground and all state marks/controls clear 3:1; already-compliant theme values remain unchanged. | `scribe-zwtv.10` | Contrast unit matrix at dark/light/custom themes. |

### A2 interaction, accessibility, responsive behavior, and lifetime

| ID | Normative requirement | Owner bead | Planned oracle |
| --- | --- | --- | --- |
| A2-I1 | Hover/focus opens one transient collapsed-lane drawer over the lanes; leaving both tab and drawer closes it after the existing board hover grace. | `scribe-zwtv.5` | Headless state test and pointer/focus visual run. |
| A2-I2 | Click or Enter/Space pins one drawer; pinning the other replaces it; `×` or reactivation unpins. | `scribe-zwtv.5` | Headless per-workspace state matrix. |
| A2-I3 | Drawer/tab accessible names include lane name, count, collapsed/pinned state, and “focus opens; activate pins/unpins.” Visible keyboard focus matches hot-state prominence. | `scribe-zwtv.10` | AccessKit tree assertions and visual focus capture. |
| A2-I4 | Row pointer click, Enter, or AccessKit Click opens detail; epic-backed rows also request Flow. Full title remains available by tooltip and accessible name. | `scribe-zwtv.14` | Functional pointer/keyboard request count and AccessKit tree. |
| A2-I5 | Pointer drag keeps the existing >2px threshold, eligible source lanes, five target semantics, native ghost, PTY isolation, guarded writes, optimistic overlay, and authoritative settlement. | `scribe-zwtv.9` | Existing and expanded real-`bd` drag corpus. |
| A2-I6 | Keyboard move uses Space grab, Left/Right named targets, Enter/Space drop, and Escape cancel through the same guard/write functions as pointer drag. | `scribe-zwtv.9` | Functional keyboard claim/close/reject/PTY-zero assertions. |
| A2-R1 | Per-strip allocation follows the closed narrow policy above; tabs and controls keep fixed geometry, text ellipsizes, and A2 never scrolls horizontally. | `scribe-zwtv.4` | Pure width matrix from narrow floor through full width. |
| A2-R2 | If a pinned lane would starve the three active lanes, it auto-collapses without deleting its persisted preference; it restores when the region again fits. | `scribe-zwtv.10` | Resize down/up and restart functional matrix. |
| A2-R3 | Text scale remains 0.8–1.6 per window; track allocation and whole-row count recompute without changing the stored board height. | `scribe-zwtv.10` | Visual 0.8/1.0/1.6 matrix. |
| A2-L1 | Hover/focus drawer state is per workspace and transient. Pinned collapsed lane is per workspace, persisted, exclusive, and cleared only by the lifetime rules above. | `scribe-zwtv.5` | State restore/cleanup tests. |
| A2-L2 | Lane scroll, board pin, board height, and lane pin survive A2→A3→A2 unchanged; separate regions never share them. | `scribe-zwtv.10` | Two-region round-trip and restart functional run. |

### A2 real-`bd` outcomes

| ID | Normative outcome | Owner bead | Planned oracle |
| --- | --- | --- | --- |
| A2-BD1 | `Ready` board snapshot renders A2, omits epic records as rows, retains parent epic metadata, counts non-epic issues, and preserves authoritative newest-created-first order. | `scribe-zwtv.14` | Real-`bd` board fixture and direct JSON comparison. |
| A2-BD2 | Ordinary board items carry the tracker timestamp needed for compact relative age; every A2 row, including standalone issues, renders that age without a new `bd` command. | `scribe-zwtv.3` | Protocol round trip, parser fixture, and real-`bd` age comparison. |
| A2-BD3 | `Loading` without last-good data does not invent rows; a refresh with last-good data keeps it until `Ready`. | `scribe-zwtv.14` | Functional delayed-refresh fixture. |
| A2-BD4 | `NotDetected` removes that workspace's board, drawer, lane pin, drag, and Flow state without affecting another region. | `scribe-zwtv.14` | Two-region functional cleanup run. |
| A2-BD5 | `Unavailable` preserves last-good board/pin state and remains retryable; it is not treated as `NotDetected`. | `scribe-zwtv.14` | Forced nonzero/timeout functional fixture. |
| A2-BD6 | Ready drop sends guarded open with defer clear; In-progress sends Claim; Done sends CloseIssue; Backlog, Blocked, source, and no target send no write. | `scribe-zwtv.9` | Real-`bd` pointer and keyboard move matrix. |
| A2-BD7 | Applied settles from authoritative refresh; precondition failure reports conflict and refreshes; failure rolls back; timeout/reconnect block duplicates until reconciliation; classifier-selected lane wins. | `scribe-zwtv.14` | Existing guarded-write failure/reconnect corpus. |

### A3 named states

| ID | Mock state / normative requirement | Owner bead | Planned oracle |
| --- | --- | --- | --- |
| A3-S1 | **Opened issue:** A2 row click opens panel and A3; cursor is unique; band shows return control, epic, inert chevron, closed/total tally, progress, opened id, and mode pair. | `scribe-zwtv.11` | Visual first-open matrix plus functional dual request. |
| A3-S2 | **Hover/focus trace:** ancestor and descendant closure stays full opacity, other nodes dim to 0.24, on-path wire intervals brighten, other intervals dim, and chip states transitive counts. | `scribe-zwtv.11` | Interior-rank visual trace probes and closure unit test. |
| A3-S3 | **Deeper epic at origin:** four-row frontier fits, right edge fades, and horizontal position bar appears because content exceeds the strip. | `scribe-zwtv.11` | Visual overflow-at-origin screenshot; machine geometry check. |
| A3-S4 | **Wheeled into the graph:** rank ruler and canvas move together, both clipped edges fade, position thumb moves, and no vertical scrollbar appears. | `scribe-zwtv.11` | Functional wheel clamp plus visual middle-scroll screenshot. |
| A3-S5 | Done node recedes; Ready and Blocked are hollow; ordinary In progress and Backlog are filled; live treatment overrides queue paint; cursor and trace are independent overlays. | `scribe-zwtv.11` | Visual state matrix with one sibling per state. |

### A3 geometry and type

| ID | Normative geometry | Owner bead | Planned oracle |
| --- | --- | --- | --- |
| A3-G1 | Default 197px budget is exactly band 34 + ruler 15 + graph 139 + hbar 2 + gap 4 + floor 3. | `scribe-zwtv.2` | Existing `.impeccable/mocks/check-flow.py` plus generated contract check. |
| A3-G2 | Node is 214×24px with 6px horizontal padding/gap, 8px dot, 9.5px mono priority/id, and 12px ellipsized title on one line. | `scribe-zwtv.11` | Visual node box/type measurements. |
| A3-G3 | Gutter is 28px, rank pitch 242px, row gap 10px, row pitch 34px, and graph left padding 30px. | `scribe-zwtv.2` | Machine formula assertions and layout unit tests. |
| A3-G4 | Row capacity is 5 at scale 0.8, 4 at 1.0, and 2 at 1.6 in the fixed 139px graph band. | `scribe-zwtv.11` | Pure layout scale matrix. |
| A3-G5 | Adjacent wires are orthogonal half-gutter stubs/dogleg; skip edges use intermediate lanes; every endpoint lands on the node dot center. Shared translucent intervals paint once. | `scribe-zwtv.11` | Machine endpoint checker and interval-union unit tests. |
| A3-G6 | Band padding is 14px left/10px right with 10px gaps. Progress is 150×2px. Rank ruler begins at y=34 and graph at y=49. | `scribe-zwtv.11` | Visual band/ruler bounds. |
| A3-G7 | Chip uses 3px vertical/7px horizontal padding, 2px radius, and remains anchored to its node while scrolled. | `scribe-zwtv.11` | Visual trace before/after scroll. |
| A3-G8 | Clipped-edge fades are 48px over the graph band. Hbar is 2px at y=188. Floor is 3px with the same centered 34×1px grip as A2. | `scribe-zwtv.11` | Visual overflow geometry. |
| A3-G9 | A3 never changes stored board height. Below the 197px minimum it stays in A2; above it, the normative A3 module remains top-anchored and surplus area is board ground above the bottom floor. | `scribe-zwtv.10` | Resize/restore visual and functional matrix. |

### A3 color roles

| ID | Normative color-role inventory | Owner bead | Planned oracle |
| --- | --- | --- | --- |
| A3-C1 | Band is subtle lifted ground with strong lower hairline; epic/title, chevron/muted, tally/title, total/muted, progress track, and done-hue fill are separate roles. | `scribe-zwtv.11` | Visual theme rewrite samples. |
| A3-C2 | Mode inactive/active/background roles remain distinct; `FLOW` is selected, `LANES` is actionable, and `← LANES` uses muted control ink. | `scribe-zwtv.11` | Visual hover/focus/selected matrix. |
| A3-C3 | Rank label, base wire, dim wire, and traced wire are four roles; traced wire and cursor keyline use title ink. | `scribe-zwtv.11` | Interior shared-gutter trace probes. |
| A3-C4 | Ordinary node title/id/hover are distinct roles. Done uses filled done dot, muted title, and 0.6 priority; Ready/Blocked use hollow state rings, with Blocked title lifted. | `scribe-zwtv.11` | Per-state dot center/rim and title samples. |
| A3-C5 | Non-live In progress uses filled progress dot; Backlog uses filled backlog/muted dot. Both retain ordinary title and never show an agent line or halo from assignment alone. | `scribe-zwtv.11` | Visual undefined-state regression fixture. |
| A3-C6 | Live uses filled progress dot, 3px 20%-strength progress halo, lifted 650 title, agent ink, and 4px progress status dot. Missing assignee suppresses only the agent text. | `scribe-zwtv.11` | IssueFocused positive/negative visual matrix. |
| A3-C7 | Cursor uses subtle fill plus 2px title-ink left keyline. Trace dims off-path nodes to 0.24 without altering geometry. | `scribe-zwtv.11` | Cursor uniqueness and trace screenshot diff. |
| A3-C8 | Chip uses raised-card ground, strong hairline, and body ink. Edge fade resolves into live ground; hbar track/thumb and floor/grip use distinct lift roles. | `scribe-zwtv.11` | Theme rewrite and overflow samples. |
| A3-C9 | Text and marks meet the same 4.5:1 / 3:1 floors as A2 across live themes. | `scribe-zwtv.13` | Dark/light/custom contrast matrix. |

### A3 interaction, accessibility, responsive behavior, and lifetime

| ID | Normative requirement | Owner bead | Planned oracle |
| --- | --- | --- | --- |
| A3-I1 | Successful epic-backed A2 activation opens panel and Flow together; only the clicked workspace changes mode. | `scribe-zwtv.14` | Two-region real-`bd` click run. |
| A3-I2 | Node pointer click, Enter/Space, or AccessKit Click moves cursor and retargets an open panel without fetching/re-ranking the graph; cursor reactivation is a no-op. | `scribe-zwtv.14` | Functional request-count and stale-reply assertions. |
| A3-I3 | Hover and keyboard focus apply the same path trace. Leaving/blur restores all nodes and wires in one frame; reduced motion lands on the same frame. | `scribe-zwtv.11` | Visual pointer/focus equivalence and reduced-motion capture. |
| A3-I4 | `← LANES` and `LANES` are Buttons with visible focus and Enter/Space activation. `FLOW` exposes selected/current state but no action. Epic chevron is hidden from interaction/accessibility. | `scribe-zwtv.11` | AccessKit tree, pointer click, and inert-chevron visual assertion. |
| A3-I5 | Nodes are Buttons and Tab stops ordered rank-left-to-right then top-to-bottom. Name is `<id> <title>, <state>` plus liveness; description names blockers/dependents and trace counts. | `scribe-zwtv.11` | AccessKit order/name/description assertions. |
| A3-I6 | Tab/Shift+Tab auto-scrolls the focused node into view. No arrow-key graph traversal, zoom, pan, node dragging, or dependency editing exists. | `scribe-zwtv.10` | Narrow/overflow keyboard functional run. |
| A3-I7 | Wheel over Flow claims the gesture and maps either axis to clamped horizontal scroll; no wheel outside that workspace changes it. | `scribe-zwtv.14` | Two-region wheel and clamp assertions. |
| A3-R1 | Flow is per workspace/region. A second region keeps its own mode, graph, cursor, scroll, trace, panel, and pin state. | `scribe-zwtv.10` | Two-region visual/functional matrix. |
| A3-R2 | Text-scale relayout preserves the graph only while every rank fits; failure exits to A2 without changing scale or stored board height. | `scribe-zwtv.10` | 0.8/1.0/1.6 plus over-wide fixture. |
| A3-L1 | Pending entry exists only until matching reply, exit, workspace loss, `NotDetected`, capability loss, or window close; late replies cannot reopen Flow. | `scribe-zwtv.14` | Out-of-order/reconnect functional tests. |
| A3-L2 | Graph, layout, cursor, scroll, and trace live only for the current window/open. Exit clears them; reopening requests a fresh complete graph. | `scribe-zwtv.14` | Exit/reopen request-count oracle. |
| A3-L3 | Liveness is session-scoped and window-local. It clears when focus changes, state clears, session ends, or owner disconnects; two live sessions on one issue keep the halo until both clear. | `scribe-zwtv.11` | Hook/session lifecycle unit and functional matrix. |
| A3-L4 | Board pin, board height, A2 lane scroll, A2 lane pin, and text scale are outside Flow and survive its round trip. Flow mode itself does not survive window restart. | `scribe-zwtv.10` | Restart and A2→A3→A2 restore run. |

### A3 real-`bd` and protocol outcomes

| ID | Normative outcome | Owner bead | Planned oracle |
| --- | --- | --- | --- |
| A3-BD1 | `Graph` is complete and epic-scoped, includes closed members omitted by capped Done lanes, and includes satisfied `blocks` edges omitted by `bd blocked`. | `scribe-zwtv.14` | Real-`bd` seven-node/eight-edge comparison. |
| A3-BD2 | `NoEpic` (non-epic id, vanished epic, or wrong workspace root) leaves A2 and panel usable and paints no empty Flow frame. | `scribe-zwtv.14` | Real-`bd` non-epic and cross-root requests. |
| A3-BD3 | `Disconnected` and `ExternalBlocker` are real-`bd` refusal fixtures; both remain A2 and are logged diagnostically. | `scribe-zwtv.14` | Real-`bd` refusal fixture. |
| A3-BD4 | `Cycle` is a typed refusal proven in memory because official `bd` rejects cycle creation; it is never a parked layout or fake E2E fixture. | `scribe-zwtv.14` | Server/layout unit tests. |
| A3-BD5 | `TooLarge` refuses the whole graph. No partial nodes, optional graph, or `truncated` flag are allowed. | `scribe-zwtv.14` | Bound unit tests and protocol round trip. |
| A3-BD6 | `Unavailable { message }` leaves A2 usable, clears the pending entry, and may be retried by reopening; it never replaces a frozen graph already on screen. | `scribe-zwtv.14` | Forced graph-read failure/retry functional run. |
| A3-BD7 | Missing/false `beads_flow`, remote/shared ownership, protocol mismatch, or capability loss never enters Flow and never weakens A2/detail gating. | `scribe-zwtv.14` | Capability/share/reconnect matrix. |
| A3-BD8 | Graph assembly reuses the server-owned full board read and generation fence; it adds no client `bd` process and no second tracker invocation. | `scribe-zwtv.14` | Exact argv/count server tests. |

## Completion gate

The contract is complete only when:

1. `scribe-zwtv.2` machine-checks the A2/A3 section boundary and geometry.
2. `scribe-zwtv.13` replaces the obsolete five-lane visual baseline with every
   named A2/A3 state above.
3. `scribe-zwtv.14` proves every interaction and real-`bd` outcome above.
4. `scribe-zwtv.15` makes the machine, visual, functional, and coverage checks
   permanent CI gates.
5. `scribe-zwtv.16` reconciles `specs/`, `lat.md/`, and shipped behavior after
   parity lands.
6. `scribe-zwtv.17` proves the packaged `scribe-dev` artifact, and
   `scribe-zwtv.18` reports no unowned or unproved normative row.

This publication has **zero unowned normative rows**.
