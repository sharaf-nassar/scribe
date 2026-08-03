# Spec: terminal-images

## Problem Statement

Terminal applications running inside Scribe cannot display inline images because the GPUI client does not parse, retain, place, or paint any terminal image protocol. Applications must fall back to text or assume a different terminal, despite GPUI being able to render image resources.

Scribe needs interoperable image support based on protocols that terminal applications already emit. The implementation must preserve Scribe's server-owned session model, treat all PTY output as untrusted, remain local and offline, and avoid inventing a Scribe-specific application API when standard terminal protocols suffice.

## Goals

- Render bounded direct inline Kitty Graphics RGB, RGBA, and PNG payloads, including chunking, zlib compression, classic placements, and Unicode placeholders, plus Sixel for compatibility.
- Support protocol discovery and replies needed for applications to select a supported path instead of guessing from `$TERM` or terminal brand.
- Decode, retain, place, scroll, clip, resize, layer, delete, and reset image state with behavior consistent with each selected protocol.
- Keep bounded protocol and image data in the server so detach, replay, reattach, client restart, and multiple viewers preserve session behavior; send typed replay/render state to clients while each view owns its GPU cache.
- Route capability replies through ordered server-owned protocol handling so each query produces exactly one reply to its originating PTY.
- Keep text selection, text copy, cursor behavior, alternate-screen behavior, scrollback, splits, reconnects, and long-lived server sessions correct around images.
- Enforce checked dimensions and strict per-sequence, decoded-image, per-pane memory, placement-count, and decode-time budgets before allocation or GPU upload.
- Reject unsafe host access by default, including file paths, temporary files, shared memory, URLs, or other indirect payload sources named by untrusted PTY output.
- Keep ordinary text-only output on the existing hot path with no material throughput, input-latency, or frame-stability regression when no image sequences are present.
- Ship image support enabled by default only after security, interoperability, Linux, and native macOS gates pass, with a master kill switch and runtime-truthful capability advertisement.
- Provide user-reachable evidence for direct PTY and SSH workflows using Yazi, a dual-protocol previewer, a plotting workflow, and protocol fixtures on Linux and macOS.

## Non-Goals

- A Scribe-specific image command, public IPC image frame, or SDK when standard terminal escape protocols can serve terminal applications.
- Kitty file, temporary-file, shared-memory, URL, network, or other indirect payload transports in the first release.
- iTerm2 OSC 1337 image support in the first release.
- Automatic conversion between unrelated image protocols.
- Persisting decoded GPU resources across client restart or sharing GPU resources between viewers; clients rebuild per-view caches from bounded server-owned state.
- Windows support, matching the current GPUI client support matrix.
- Animations, multi-frame composition, or video.
- Guaranteed passthrough or rendering behavior under tmux, GNU screen, or Zellij in the first release.
- Perfect support for every extension or historical terminal image protocol.
- Scribe-generated textual fallbacks for applications; applications remain responsible for their own fallback output.

## Backlog Inputs

None. The closed `scribe-38e.105` task and `specs/016-gpui-client-rebuild/spikes/terminal-image-protocols.md` are prior research inputs, not open P4 backlog sources. Their Sixel plus bounded Kitty proposal must be revalidated rather than treated as an implementation mandate.

## Target Epic

This run will create a new feature epic for terminal image support.

## User Stories

### US1 — Display images from terminal applications

As a Scribe user, I want compatible terminal applications to display images inline, so that previews, plots, documents, and image-aware CLI workflows work without leaving the terminal.

Acceptance Criteria:

- Kitty direct RGB/RGBA/PNG payloads, chunking, bounded zlib, classic placements, Unicode placeholders, and Sixel are implemented and documented against current primary specifications.
- The compatibility contract documents supported actions, encodings, placement rules, reset behavior, numeric safety limits, and deliberate exclusions, including iTerm2, indirect transports, and animation.
- Running-client fixtures display representative images through both protocols and compare stable visible output for direct PTY and SSH paths.
- Yazi's Unicode-placeholder workflow, a dual-protocol previewer, a plotting workflow, and protocol-level fixtures form the release interoperability corpus.
- Unsupported, malformed, truncated, or over-budget sequences do not corrupt adjacent terminal text or crash the client.

### US2 — Let applications detect support

As a terminal application author, I want standards-compatible capability queries and replies, so that my application can select an image path or fall back cleanly.

Acceptance Criteria:

- Required query replies use the selected protocol's specified framing and values.
- The server emits each protocol reply exactly once, in byte order, to the originating PTY; detach, replay, reconnect, and multiple viewers cannot duplicate it.
- Typed IPC carries bounded replay/render state between the server and clients without exposing a Scribe-specific application protocol.
- Capability claims expose only implemented subsets; excluded transports or actions are never advertised.
- Runtime policy, including the master kill switch, controls truthful advertising; disabled or unavailable image support is never claimed.
- Split, reconnect, and concurrent-pane validation proves replies reach only the originating session in order.

### US3 — Preserve terminal semantics around images

As a Scribe user, I want images to behave like terminal content, so that scrolling, resizing, switching screens, splitting panes, and overlaying text remain predictable.

Acceptance Criteria:

- Placement geometry is derived from terminal cells and current pane metrics, clips to the terminal viewport, and follows specified z-order rules.
- Scroll, erase, delete, reset, alternate-screen entry/exit, resize, and pane destruction update image state according to selected protocol semantics.
- Text remains selectable and copyable without image data leaking into copied text.
- Surrounding text remains visible, selectable, and copyable when an image is disabled or rejected; Scribe shows a non-payload diagnostic affordance while applications own textual fallback.
- Protocol/data state is isolated per server session, and GPU resources are isolated per pane view; one application or viewer cannot mutate another's images.

### US4 — Bound untrusted image processing

As a Scribe user, I want image escape data treated as untrusted, so that a program cannot exhaust memory, stall rendering, or read host resources.

Acceptance Criteria:

- Encoded length, decoded byte count, dimensions, multiplication, chunk accumulation, retained data, placement count, and work/time budgets are checked before or during processing.
- Decode and decompression run outside the GPUI paint path; only bounded completed resources reach foreground GPU upload.
- Per-pane eviction is deterministic, protocol-correct where required, and does not evict content belonging to another pane.
- File, temporary-file, shared-memory, URL, network, and all other indirect transports are unconditionally rejected or ignored in v1.
- Decoder selection may patch, vendor, or replace an upstream implementation when needed to enforce caller-controlled dimension, byte, work, time, and cancellation limits.
- Fuzz or corpus-based malformed-input validation covers framing, chunking, decompression, dimensions, and deletion/reset operations.

### US5 — Preserve performance and session continuity

As a Scribe user, I want image support not to degrade normal terminal work or long-lived sessions.

Acceptance Criteria:

- Named measurements compare text-only throughput, input latency, CPU use, and frame stability before and after image support.
- Named measurements record decode latency, upload latency, peak retained CPU/GPU memory, and eviction behavior for a fixed multi-image workload.
- Release review records whether those measurements show any material regression; v1 does not impose invented numeric performance thresholds or freeze measurement-derived performance limits.
- Exact numeric security resource limits remain mandatory for untrusted payload handling and are distinct from qualitative performance-regression review.
- Bounded server-owned image state survives detach, reattach, replay, client restart, and multiple simultaneous viewers; each client recreates its per-view GPU cache.
- Closing or replacing a pane releases all image resources associated with that view.

### US6 — Behave consistently on supported platforms

As a Linux or macOS user, I want the same advertised image subset, so that applications behave consistently across supported Scribe builds.

Acceptance Criteria:

- Linux X11/Wayland behavior is verified only through the Docker E2E harness required by repository policy.
- Native macOS build and runtime verification are completed before default-on release; Linux Docker results are not presented as macOS validation.
- Linux and macOS advertise the same verified protocol subset at release.
- Platform-specific texture formats, scale factors, and GPU limits do not change protocol-visible semantics.

## Constraints

- PTY programs are untrusted. Constitution principles 1, 3, 4, 5, 6, and 7 govern ownership, typed failure, user-reachable verification, budgets, local processing, and compatibility evidence.
- Current `DisplayOnlyTerminal` sends PTY bytes directly to Alacritty's VTE processor; protocols hidden or discarded by that processor need bounded streaming interception before ordinary bytes continue unchanged.
- GPUI owns GPU resources. Protocol parsing, decoding, storage, and placement policy must stay outside frame painting; the paint path consumes immutable ready-to-draw state.
- The server owns bounded parsed protocol/data state and exactly-once PTY replies. Typed replay/render IPC transfers immutable bounded state to clients, which own per-view GPU caches; terminal image escapes are not themselves Scribe IPC.
- Dependency APIs, format support, licenses, maintenance status, and limits must be verified from current primary documentation and source before selection.
- Test code may be planned because this requested feature changes existing coverage, but all Scribe validation and experimentation must use the Docker functional/visual harness. Never invoke host Scribe binaries or restart the live server.
- Native macOS runtime validation is the sole exception: only the manual
  `.github/workflows/native-macos-metal.yml` workflow on GitHub's hosted
  `macos-14-xlarge` runner may invoke the downstream Metal corpus. A repository
  maintainer with write access owns dispatch, evidence review, and failure
  triage; developer workstations remain forbidden.
- Current primary documentation and ecosystem evidence must define the exact Kitty and Sixel semantics. iTerm2 OSC 1337 remains researched prior art but is excluded from v1.
- Image handling must remain fully local and usable offline.
- Exact numeric limits for encoded/chunk data, dimensions, pixels, decompressed/decoded bytes, retained CPU/GPU resources, placements, queues, concurrency, work, and decode time are mandatory security controls.
- Performance acceptance uses named measurements and qualitative no-material-regression review, without numeric regression thresholds or measurement-derived frozen performance limits. Numeric performance goals are explicitly inapplicable for v1 under Constitution principle 4; analysis must still verify the named-command measurement requirement.

## Open Questions

The decoder question is resolved by
[`decoder-decision.md`](decoder-decision.md): use a Scribe-owned bounded
`flate2` loop and narrow decoder-only Sixel/PNG forks with common allocation,
work, deadline, and cancellation controls. Generic and stock whole-image
decode entry points are no-go.

The v1 protocol matrix, xterm-compatible Sixel chronology, typed failures,
numeric security limits, application pins, and fixture ownership are resolved
in [`contract.md`](contract.md). A later native platform spike may lower the
4096-pixel texture ceiling, but cannot raise any v1 trust-boundary limit.

## Clarifications

Human decisions resolve the first specification gate and define the v1 product boundary.

1. **Protocol set — A:** Ship bounded Kitty direct RGB/RGBA/PNG with chunking, zlib, classic placements, and Unicode placeholders, plus Sixel. Exclude iTerm2, indirect transports, animation, and frame composition.
2. **Session continuity — A:** Keep bounded protocol/data state on the server with exactly-once replies and typed replay/render IPC. Keep GPU caches local to each client view.
3. **Connection and application gates — A:** Require direct PTY and SSH evidence with Yazi, a dual-protocol previewer, a plotting workflow, and protocol fixtures. Defer tmux, GNU screen, and Zellij guarantees.
4. **Trust boundary — A:** Accept direct inline payloads only. Reject file, temporary-file, shared-memory, URL, network, and other indirect sources. Patching, vendoring, or replacing a decoder is allowed to enforce bounds.
5. **Rollout — A:** Enable by default only after release gates pass. Keep a master kill switch, truthful runtime advertisement, preserved text/copy, and a payload-free disabled/rejected affordance; applications own textual fallback.
6. **Platform parity — A:** Require the same natively verified Linux and macOS subset before release.
7. **Performance gates — C:** Use named measurements and qualitative no-material-regression review without numeric performance thresholds. Numeric performance goals are explicitly marked inapplicable for v1 under Constitution principle 4, while named-command measurement remains required. Numeric security resource limits remain required because they define the untrusted-payload boundary, not performance regression acceptance.
8. **Frozen contract:** [`contract.md`](contract.md) is the normative v1
   protocol/security contract. Machine-readable values and owned fixture paths
   are validated by `just e2e-func terminal-image-contract.sh`.

## Spec Review

### Critical Questions

The human gate, frozen contract, bounded decoder decision, and sanctioned
GitHub-hosted Metal workflow resolve product scope, protocol boundaries, the
decoder, and the native runner without reopening v1.

### Non-Blocking Observations

- Current architecture needs more than a paint-layer addition: image commands can affect cursor and scroll state, so server and client terminal effects must remain byte-order equivalent.
- Kitty z-order requires multiple image paint phases around cell backgrounds and glyphs; one image pass cannot implement the specified semantics.
- Asynchronous decode needs ordered commit/cancellation so a late completion cannot resurrect an image after delete, reset, screen switch, pane close, or session exit.
- Sixel cursor behavior varies with DECSDM and private mode 8452; the supported mode contract must come from current primary xterm documentation and explicit fixtures.
- `icy_sixel` 0.5.0 has hard-coded limits above Scribe's proposed budgets and can grow buffers during decode, so a preflight encoded-length check alone is insufficient.
- Quotas must count encoded/chunk accumulation, inflated and decoded CPU buffers, pending upload copies, and retained GPU resources separately, including aggregate multi-pane pressure.
- Plan must define payload-free, rate-limited diagnostics; raw image bytes must never enter logs, telemetry, crash metadata, or disk caches.
- Plan should publish a compatibility table naming protocols, actions, formats, queries, limits, applications, connection paths, platforms, and deliberate exclusions.
