# beads-flow-view

## Status

**Superseded.**

This draft is retired in full by the approved canonical
[Beads board A2/A3 production contract](028-beads-board-contract.md). It is not
an alternative source of production scope, behavior, geometry, outcomes, or
tests.

Decision provenance is Quill session
`01a01227-1e60-78a5-ac9d-58a63aef7ead`, recorded without reconstructed or
fabricated dialogue. The approved artifact produced by that decision process is
`.impeccable/mocks/beads-board-directions.html`.

## Supersession ledger

| Stale statement in this former draft | Canonical resolution |
| --- | --- |
| Status is Draft / awaiting clarification. | `specs/028-beads-board-contract.md` is Approved and canonical. |
| Every mock surface is normative, or A3 alone is the normative board. | Only mock sections **A2 · Ledger + rail** and **A3 · Flow** are normative. Page scaffolding, CURRENT, and standalone A are reference-only. |
| Lanes must remain visually unchanged. | A2 replaces the legacy five-lane/card rendering. Existing guarded write and detail behavior remains, but its presentation and collapsed-lane geometry must reach A2 parity. |
| A3 nodes are 262×40px on a 286px rank pitch and 56px row pitch. | The approved revision is 214×24px, 28px gutter, 242px rank pitch, 10px row gap, and 34px row pitch in a 139px graph band. |
| “Mock revision required”; current geometry is temporarily non-normative. | The revision landed in the approved mock. `mock-revision-required` is closed and has no remaining exception. |
| Cycle members may be parked at `max_rank + 1`. | Cycles are typed admission refusals. No cycle parking or degenerate renderer path exists. |
| Graph reply may be optional or truncated. | Outcomes are mandatory and typed: complete `Graph`, `NoGraph { reason }`, or `Unavailable { message }`. `truncated` does not exist; over-bound graphs are refused whole. |
| Flow questions remain unanswered. | Narrow allocation, exclusive lane pinning and lifetime, drawer/drag keyboard equivalents, non-live In-progress/Backlog paint, graph lifetime, return controls, overflow, admission, and chevron behavior are closed in the canonical contract. |
| Back, mode pair, and chevron may all remain static sample chrome. | `← LANES` and `LANES` are actionable pointer/keyboard/AccessKit controls; active `FLOW` is a selected no-op. Only the epic chevron remains inert and non-interactive. |
| A card open may replace the detail panel. | A2 activation preserves the panel and conditionally swaps only the board strip to A3. |
| Flow may poll/re-rank live or retain optional stale state. | A successful graph is frozen per open. Polling continues underneath; node activation retargets only the cursor/panel. Exit and reopen requests a fresh graph. |
| Backlog and non-live In-progress nodes have no defined paint. | Backlog uses a filled backlog/muted dot; non-live In progress uses a filled progress dot. Both use ordinary title treatment and no halo/agent line. |
| A graph too wide/tall may grow the strip or gain vertical scroll. | Width scrolls with fades and a 2px position bar. Height never scrolls or grows; rank overflow remains/returns to A2. |
| Test planning may leave states or outcomes unowned. | The canonical coverage table assigns every normative row to one of the supplied `scribe-zwtv.*` owner beads and names its planned oracle. |

## Historical note

The former draft described the path by which Flow was designed and partially
implemented. Git history remains the historical record. Keeping its obsolete
requirements inline would re-create the contradiction this supersession is
intended to remove, so only this explicit retirement notice remains.
