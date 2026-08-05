# Analysis: terminal-images

## Coverage Table

| User story / requirement | Covered by (plan section) | Status |
| --- | --- | --- |
| US1 — Display images from terminal applications | Architecture Approach protocol matrix; Testing Strategy corpus; Sequencing decoder/parser/renderer/application tasks | full |
| US2 — Let applications detect support | Architecture Approach server reply ownership; API / Interface Changes capability lifecycle; server fanout/reply and application-corpus tasks | full |
| US3 — Preserve terminal semantics around images | Architecture Approach layering and Sixel chronology; Data Model grid effects; renderer/replay/client tasks; functional/visual matrices | full |
| US4 — Bound untrusted image processing | Direct-only architecture; `ImageLimits`; decoder/parser spikes and implementation; adversarial Docker corpus; dependency audit | full |
| US5 — Preserve performance and session continuity | Ordered server state, chunked replay/handoff, cleanup, viewer lifecycle, named qualitative measurement scripts, security ceilings | full |
| US6 — Behave consistently on supported platforms | Early sanctioned-macOS-path task, Linux Docker evidence, native Metal parity corpus, shared capability matrix, release gate | full |
| Kitty v1 subset | Exact Architecture Approach matrix and primary references; parser/normalizer/state/renderer tasks and protocol fixtures | full |
| Sixel compatibility subset | Exact Architecture Approach matrix, xterm DECSDM/8452 decision, Sixel chronology, bounded decoder and DA tests | full |
| Unicode placeholders | Architecture Approach cell model; Data Model placement metadata; copy filtering; owned placeholder fixtures and visual coverage | full |
| Server-owned durable state and exactly-once replies | `SessionTerminal`, capability lifecycle, PTY write-back, combined replay, handoff, viewerless retention, sharing tests | full |
| Direct PTY and SSH application compatibility | Protocol contract, pinned Yazi, Chafa, and gnuplot corpus, in-container SSH path, multiplexer exclusions | full |
| Direct-inline trust boundary | Non-Goals and architecture exclusions; unrepresentable indirect loaders; settings/diagnostics and security corpus | full |
| Default-on rollout with kill switch and fallback affordance | API changes; settings task; runtime capability and rollback tests; user/support docs | full |
| Native macOS release parity | P0 infrastructure approval, native Metal execution task, final release dependency | full |
| Clarification 7C qualitative performance review | Named functional/visual scripts and recorded measurements; numeric goals explicitly inapplicable; no numeric regression threshold | full |
| Offline/local-first operation | No payload persistence/network loaders; network-disabled Docker pass; loopback limited to SSH case | full |
| Non-goals and exclusions | Matrix explicitly excludes iTerm2, indirect transports, animation, video/vector, Windows, translation, spoofing, and multiplexer guarantees | full |
| Backlog inputs | None; closed `scribe-38e.105` remains prior research and is covered without reopening | full |

## Backlog Disposition

| Source P4 id | Plan work item(s) / non-goal | Disposition | Ready to resolve? |
| --- | --- | --- | --- |
| None | No P4 source entered this molecule | not applicable | yes |

## Target Epic

New epic: `terminal image protocol support`.

## Remaining Risks

- Native macOS runtime is sanctioned only through the manual GitHub Actions
  `macos-14` workflow. Platform-dependent GPUI work remains blocked
  until its downstream corpus driver exists and produces passing Metal
  evidence for the candidate commit.
- The bounded Sixel fork creates long-term security and license ownership. The plan pins source/license material, removes encoder-only dependencies, records the fork delta, assigns CVE maintenance, and gates adoption on adversarial evidence.
- GPUI revision `f96212f2c50f54d93712fa130d6226b1ce7d76b5` lacks a source-UV field but exposes translated image bounds and `Window::with_content_mask`. The P0 spike selected that existing API, one bounded source cache, explicit `drop_image`, and no crop variants or GPUI patch; see [`gpui-lifecycle-decision.md`](gpui-lifecycle-decision.md).
- Server-side decode delays later bytes at a graphics boundary. The design bounds queue, work, and deadline, applies backpressure, and proves that later text/replies cannot overtake the command while leaving text-only streams off the decoder path.
- Raw `PtyOutput` plus canonical image state adds wire and memory pressure for capable viewers. The frozen framed, decoded, 128 MiB session, 256 MiB view, 512 MiB process, and 1 MiB replay ceilings plus replay-dirty recovery constrain it.
- Kitty and Sixel standards leave edge behavior undefined or contradictory. The plan freezes xterm-compatible DECSDM/8452 and Sixel chronology, refuses to claim undefined overlap parity, and builds owned fixtures from primary sources.
- Current Yazi uses classic Kitty for an unknown-but-query-capable terminal. V1 gates real Yazi display on that truthful path and tests Unicode placeholders independently; upstream Scribe recognition remains optional P3 work.
- Incapable local clients cannot safely render an image-enabled session. The plan chooses an explicit typed refusal instead of invisible/cursor-divergent degradation and tests old/new/rollback paths.
- Clarification 7C makes material-regression judgment subjective. The plan retains repeatable named commands, raw measurements, reviewer/rationale, and hard numeric security ceilings while marking numeric performance goals inapplicable.

## Unresolved Questions

No unresolved product-scope, protocol-contract, or epic-selection question remains. The following bounded technical decision is intentionally assigned to later P0 work:

- Decoder-fork go/no-go and final vendored revision.

The GPUI crop/UV decision is closed. Linux WGPU accepts the frozen 4096-pixel
axis ceiling, so the spike does not lower it. Actual Metal behavior remains a
platform gate, not an invitation to raise that ceiling.

## Constitution Check

1. **Clear Boundaries and Typed Failure — pass.** Server protocol/data authority, client GPU ownership, typed commands/rejections, direct-only data, and explicit crate boundaries are documented.
2. **Session-Safe, Consistent UX — pass.** Combined replay, handoff, viewerless retention, exactly-once replies, incapable-viewer refusal, copy behavior, and kill-switch lifecycle preserve long-lived sessions explicitly.
3. **Explicit, Risk-Based Verification — pass.** Each story has user-reachable Docker evidence, named applications, malformed/security cases, native-platform evidence, and artifact paths.
4. **Performance Budgets and Measurement — pass with tension.** The principle permits measurable goals to be explicitly marked inapplicable. Clarification 7C does so for numeric regression goals, while the plan retains named repeatable commands and recorded measurements. Numeric security limits remain mandatory and measurable.
5. **Default-Safe Trust Boundaries — pass.** PTY data is untrusted, direct-inline only, fully bounded, payload-free in diagnostics, and disableable without false capability claims.
6. **Local-First Data Locality — pass.** Core operation is offline; image bytes are not loaded from network/host paths, persisted, logged, or transmitted outside already-authorized terminal transports.
7. **Compatible, Documented, Operationally Safe Change — pass.** Primary APIs are verified, mixed versions and rollback are explicit, worktree isolation is active, `lat.md` updates are per-task, live host Scribe remains untouched, and release/publish actions remain gated.

## Recommendation

**GO** — Specification and plan fully cover the clarified v1 boundary, create a new unambiguous epic, contain no P4 backlog input, and convert remaining technical uncertainty into concrete P0 spikes or infrastructure gates with testable outcomes. Proceed to dependency-wired bead creation after human approval. Numeric performance thresholds remain intentionally absent under clarification 7C; principle 4 is satisfied through its explicit-inapplicability clause plus named measurements.
