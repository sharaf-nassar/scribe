# Plan: terminal-images

## Architecture Approach

Scribe will implement Kitty Graphics and Sixel as server-owned terminal protocols, not as Scribe-specific application APIs. One ordered server pipeline frames graphics escapes before Alacritty consumes or discards them, applies terminal effects once, retains bounded canonical image state, answers application queries exactly once, and fans typed immutable state to capable clients.

```text
PTY bytes
  -> bounded ordered graphics framer
     -> raw bytes -> authoritative Alacritty Term + existing PtyOutput
     -> Kitty/Sixel command -> bounded decode/state -> grid/image delta + reply
  -> atomic session commit -> AttachedSinks
  -> raw output + sequenced typed image state
  -> client ordered pane drain -> terminal + CPU image scene
  -> per-view GPUI RenderImage cache -> layered terminal paint
```

Each live session gains one `SessionTerminal` lock containing the authoritative Alacritty `Term`, graphics framer/chunk state, `TerminalImageState`, Sixel mode state, and a monotonic generation/commit cursor. Text and graphics mutations therefore share one order and one attach snapshot boundary. Existing raw `PtyOutput` remains byte-compatible; sequenced immutable image updates accompany it at explicit output commit boundaries. The server normalizes completed direct uploads to bounded RGBA definitions, drops raw encoded payloads after validation, and retains placements independently. Clients continue feeding raw output through Alacritty while applying canonical image/grid effects once at their committed boundary.

At a graphics boundary, later PTY bytes are staged behind a bounded per-session barrier until framing, decode, terminal effects, state commit, and any required reply finish. No later text, reply, or grid mutation can become visible first. Queue and byte ceilings backpressure the PTY reader; text-only streams never enter the decoder path. Decode runs on a bounded worker pool, but server-side normalization is intentional: clarification 2 assigns authoritative protocol/data state to the server and only GPU resources to each client view, avoiding repeated untrusted decode in every viewer.

The server owns protocol replies. Kitty acknowledgements/errors and Sixel/DA discovery use the existing server PTY write-back seam; clients never synthesize replies through `KeyInput`. Image capability is latched when an image-capable client creates or first enables a session and remains server state across detach. Image-enabled sessions continue parsing and retaining while temporarily viewerless. An incapable client cannot attach to an image-enabled session: it receives a typed update-required/capability-mismatch response instead of silently seeing divergent text. Remote peers continue using exact protocol-version matching. The kill switch disables new claims and clears existing image state before allowing degraded attachment. Capability responses advertise only the enabled implemented subset.

Clients apply text, terminal-grid effects, image definitions, placements, and replay boundaries through the same ordered pane operation queue. `DisplayOnlyTerminal` owns an immutable CPU scene. `TerminalView` owns `(session_id, image_id, generation) -> Arc<RenderImage>` resources for its window and calls `Window::drop_image` on eviction, replay replacement, pane destruction, or session exit.

`TerminalElement` paints multiple image phases required by Kitty semantics:

1. z-index below `-1_073_741_824`, beneath non-default cell backgrounds;
2. cell backgrounds;
3. other negative-z images;
4. box drawing and shaped text;
5. nonnegative-z images;
6. cursor, selection, split-scroll, scrollbar, and Scribe chrome.

Unicode-placeholder placements remain real terminal cells using `U+10EEEE`, diacritics, and color channels as specified by Kitty. The client recognizes them for rendering while copy/selection excludes placeholder markers and retains surrounding text. Classic placements use the shared placement scene. Sixel completions become anonymous cursor/mode-anchored raster definitions; Scribe does not invent Sixel IDs, deletion, or z-index semantics. A Sixel raster occupies the legacy graphics layer above the default terminal background but beneath non-default cell backgrounds and glyphs. Existing and later glyphs remain visible; later non-default backgrounds and erase operations cover affected raster pixels. Scroll, alternate-screen entry, hard reset, history trim, and resize follow the frozen xterm-compatible matrix. Soft reset preserves committed raster content.

Kitty v1 supports direct `t=d`, RGB `f=24`, RGBA `f=32`, PNG `f=100`, bounded RFC 1950 zlib, chunk accumulation, transmit/store/display/query/delete actions, classic placements, and Unicode placeholders. Sixel supports 7-bit and C1 DCS/ST framing, raster attributes, repeat, palette selection and HLS/RGB definitions, transparency/background modes, `$`/`-`, DA1 attribute 4 discovery, and documented xterm-compatible DECSDM/8452 behavior.

Direct bytes are the only payload source. File, temporary-file, shared-memory, URL, and network transports; iTerm2 OSC 1337; animation/frame composition; protocol translation; terminal-brand impersonation; and guaranteed tmux/GNU screen/Zellij passthrough are rejected or deferred.

The frozen v1 matrix is:

| Area | Supported | Explicitly excluded |
| --- | --- | --- |
| Kitty framing | APC `G`, bounded base64 chunks, `m=1/0`, `q=0/1/2` replies | Interleaved transfers and unterminated/unbounded strings |
| Kitty data | Direct `t=d`; RGB `f=24`, RGBA `f=32`, PNG `f=100`; bounded `o=z` | `t=f`, `t=t`, `t=s`, paths, shared memory, URLs, generic format guessing |
| Kitty actions | transmit `t`, transmit/display `T`, put `p`, query `q`, delete `d` | Animation/frame actions and relative-parent placement extensions |
| Kitty placement | Classic crop/scale/offset/cursor rules; `U=1` virtual placements; official placeholder cells/diacritics/colors | Animation composition and image interaction/tooling |
| Kitty lifecycle | ID replacement; supported lower/uppercase delete selectors; chunk abort; scroll/margins; ED2; reset; 1049; full z-order | Protocol translation or invented semantics for undefined overlaps |
| Sixel | 7-bit/C1 DCS/ST; bit raster; repeat; raster attributes; palette/HLS/RGB; P2 modes; `$`/`-`; DA4 | Object IDs, reusable placement commands, z-index, animation |
| Sixel modes | xterm-compatible `?80l/h` and `?8452l/h`, cursor/scroll/crop matrix | Claim of literal DEC parity where DEC/xterm documents disagree |
| Platforms/apps | Linux X11/Wayland, native macOS; direct PTY/SSH; Yazi, Chafa, gnuplot Sixel, owned fixtures | Windows, video/vector formats, guaranteed tmux/screen/Zellij behavior |

Alternatives rejected:

- Client-only parsing breaks dropped-output replay, detach/reattach, server handoff, and exactly-once query replies with multiple viewers.
- Sending raw graphics escapes to clients duplicates parser/state logic and cannot keep the authoritative server cursor/grid synchronized with Sixel effects.
- Stock `icy_sixel` 0.5.0 permits non-configurable dimensions and up to 64 million pixels, lacks cancellation/work limits, and can repeatedly grow/copy buffers; an encoded-length precheck cannot make it safe.
- Full `termwiz` imports indirect transports, animation, unrelated escape parsing, and avoidable payload copies. A narrow typed Kitty parser has a smaller trust surface.
- GPUI `ImageSource::Resource` accepts paths and URLs, violating the direct-inline boundary. Rendering uses decoded `RenderImage` data only.

Primary references are the [Kitty Graphics Protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/), [DEC VT330/VT340 Sixel chapter](https://manx-docs.org/mirror/vt100.net/docs/vt3xx-gp/chapter14.html), [xterm Sixel controls](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h2-Sixel-Graphics), [xterm runtime behavior](https://xterm.dev/manpage-xterm/), and [Yazi image-preview contract](https://yazi-rs.github.io/docs/image-preview/).

## Affected Components

- `crates/scribe-common/src/terminal_images.rs` (new) — typed IDs, definitions, placement variants, grid effects, deltas, replay records, capability flags, limits, and typed rejection reasons.
- `crates/scribe-common/src/protocol.rs` — `Hello`/`Welcome` image capability negotiation; image replay begin/chunk/placement/commit and live-update server messages; protocol compatibility fixtures; remote protocol version bump.
- `crates/scribe-common/src/screen_replay.rs` and `screen.rs` — bind text replay to an image generation/commit without embedding large images into ANSI or `ScreenSnapshot`.
- `crates/scribe-pty/src/` — bounded streaming DCS/APC framer and a graphics-aware terminal handler seam that delegates ordinary VTE behavior while emitting lifecycle events for scroll, margins, erase, reset, alternate screen, and resize.
- `crates/scribe-server/src/ipc_server.rs` — `SessionTerminal`, ordered PTY processing, state commits, exactly-once replies, capable-sink fanout, overflow replay-dirty handling, diagnostics, and kill-switch enforcement.
- `crates/scribe-server/src/attach_flow.rs` — atomic text/image snapshot and chunked replay staging.
- `crates/scribe-server/src/session_manager.rs` — session capability, limits, decoder queue, image lifecycle, launch/restore integration, and truthful DA/query behavior.
- `crates/scribe-server/src/handoff.rs` and handoff tests — backward-compatible image state/generation plus in-flight framer quiescence across upgrade.
- `crates/scribe-client/src/ipc_bridge.rs`, `main.rs`, `sync_frames.rs`, and `session_lifecycle.rs` — ordered image pane operations, atomic replay staging, generation replacement, and cleanup.
- `crates/scribe-client/src/terminal.rs` — immutable definitions/placements and grid-effect application beside terminal `Content`.
- `crates/scribe-client/src/terminal_element.rs` — source cropping/scaling, placeholder resolution, clipping, z-order phases, and GPUI image paint.
- `crates/scribe-client/src/settings/`, `crates/scribe-common/src/config.rs`, and settings docs — default-on master image kill switch with runtime capability changes and payload-free disabled/rejected affordance.
- `third_party/icy-sixel-decoder/` and attribution/license inventory — bounded decoder-only `icy_sixel` 0.5.0 fork with no encoder/quantizer dependency.
- `third_party/image-png-decoder/` and attribution/license inventory — bounded decoder-only `png` 0.18.1 fork with Scribe work/cancellation hooks and no encoder/APNG/ancillary metadata paths.
- `crates/scribe-image-decode/` — shared caller-owned cumulative work, cancellation, monotonic deadline, allocation veto, and peak-allocation accounting consumed by both decoder forks.
- `Cargo.toml`/`Cargo.lock` — direct `base64` 0.22.1 and `flate2` 1.1.9 aligned with already locked versions; no generic `image`, full `termwiz`, `quantette`, or C decoder in server decode paths.
- `crates/scribe-test/` and `tests/e2e/` — typed protocol support, direct PTY/SSH application corpus, functional malformed/quota/replay cases, and visual fixtures.
- `.github/workflows/native-macos-metal.yml`, `tools/run-native-macos-terminal-images.sh`, `justfile`, and repository policy — manual GPU-backed macOS dispatch, runner guard, downstream driver contract, evidence retention, and fail-closed release ownership.
- `lat.md/client.md`, `lat.md/server.md`, `lat.md/pty.md`, `lat.md/rendering.md`, `lat.md/common.md`, and `lat.md/test.md` — architecture and verification contracts.

## Data Model

`TerminalImageCapabilities` records Kitty direct formats/actions, Unicode placeholders, Sixel, and the runtime-enabled bit. It is negotiated in `Hello`/`Welcome` with defaults that mean unsupported for older peers.

`ImageLimits` is the server-owned immutable policy frozen in
[`contract.md`](contract.md): 16 MiB control strings, 4096-byte Kitty chunks,
32,768 chunks, 89,478,488 encoded bytes, 64 MiB decoded/inflated/canonical
bytes, 4096 pixels per axis and 16,777,216 pixels, 128 images, 1024
placements, 128 MiB session CPU, 256 MiB projected view GPU, and 512 MiB
process retention. The process permits two active and eight queued decodes,
charges 128 MiB queued bytes and 134,217,728 work units, waits at most 1000 ms
in queue, and decodes at most 2000 ms with checks every 4096 work units.
Replay/handoff chunks are at most 1 MiB. Native platform evidence may lower,
but never raise, the 4096-pixel v1 ceiling.

`TerminalImageId` and `TerminalPlacementId` are typed identifiers. Kitty preserves specified image/placement numbers; Sixel receives a server-internal monotonically allocated image ID and anonymous placement.

`ImageDefinition` contains ID, generation, width, height, canonical RGBA byte length, alpha/background metadata, and retained bounded RGBA bytes. The definition never stores paths, URLs, shared-memory names, raw PTY payloads, or disk-cache keys.

`ImagePlacement` contains protocol kind, image/placement identity, terminal anchor, source crop, destination cell geometry, pixel offsets, z-index, scroll/margin association, cursor-movement behavior, and placeholder metadata. Unicode placeholder cells stay in terminal content; placement state maps their encoded color/diacritic identity to an image definition.

`TerminalImageState` contains definitions, placements, Kitty chunk accumulator, deterministic oldest-unplaced eviction order, Sixel modes, retained-byte counters, and generation. All arithmetic is checked. Eviction never crosses session boundaries and never removes data still required by a live placement unless a documented hard-pressure policy rejects the new command instead.

`TerminalGridEffect` represents image-driven cursor movement, scroll/crop consequences, and lifecycle changes that both server and client terminal models must apply identically.

`TerminalImageUpdate` is a canonical ordered operation: define chunks, place, replace, delete, erase/reset, grid effect, evict, or diagnostic status. It cannot represent an indirect payload loader.

`TerminalImageSnapshot` is transmitted as replay begin, bounded definition chunks, placement state, and replay commit under one generation. The client stages it off-screen and swaps only on commit; live updates buffer after the snapshot cursor. Output overflow marks the sink replay-dirty and suppresses image/text deltas until a fresh combined replay completes.

No database schema or disk migration is introduced. Handoff state gains defaulted image fields; older states restore an empty scene. CPU and GPU image data never enters logs, telemetry, crash metadata, settings, or disk caches.

## API / Interface Changes

- Extend `ClientMessage::Hello` with defaulted image-render capabilities and `ServerMessage::Welcome` with the effective session subset/kill-switch state.
- Add typed, bounded `ServerMessage` variants for image update and chunked image replay. Large image scenes never occupy one frame; every chunk stays below existing wire/queue caps.
- Bump `REMOTE_PROTOCOL_VERSION` because remote endpoints require exact protocol matching. Local serde defaults permit handshake decoding, but an incapable client is refused when it attempts to attach to an image-enabled session; it is never given a silently degraded renderer claim.
- Extend handoff serialization with defaulted committed image state/generation and bounded partial framer/chunk state. Upgrade pauses PTY reads, waits for a bounded active decode to commit or cancel, then transfers text state plus generation-tagged image metadata/data in bounded begin/chunk/commit frames. The child stages the scene before resuming reads. No retained scene or partial DCS/APC occupies one handoff frame.
- Add a server-internal terminal-reply function distinct from user `KeyInput`. It writes Kitty results and augmented DA replies to the originating PTY exactly once and in PTY-stream order.
- Add `terminal.images.enabled` defaulting true after release gates pass. Turning it off stops new image capability claims, cancels/clears bounded decoder state, releases retained resources, returns no Kitty success probe, and omits Sixel DA attribute 4 while preserving other DA fields.
- Add payload-free diagnostic categories for unsupported protocol/action/transport, malformed framing, quota breach, decode failure, eviction, and renderer unavailability. Messages contain protocol/action, dimensions/counts, and typed reason—not payload bytes.
- Do not change `$TERM` to `xterm-kitty` or spoof `TERM_PROGRAM`. Keep `TERM_PROGRAM=Scribe`. A pinned Yazi release must render through its generic successful Kitty probe; upstream Scribe recognition is a non-blocking follow-up, not an external merge prerequisite.
- Direct protocol bytes and replies traverse SSH PTYs unchanged. No v1 promise is made for tmux, GNU screen, or Zellij passthrough.
- Payload-free diagnostics are operational hints only. They never replace an application's textual fallback and never expose image bytes.

## Testing Strategy

Automated Linux Scribe validation runs through the Docker E2E harness and its
`just` entry points. No developer-host `scribe-server`, `scribe-client`,
`scribe`, or `scribe-test` invocation is allowed. Native macOS runtime is the
sole narrow exception: the manual `Native macOS Metal` GitHub Actions workflow
runs on the GPU-accelerated ARM64 `macos-14-xlarge` hosted runner and enters
through `just native-macos-terminal-images`.

Protocol and state tests:

- Byte-split 7-bit/C1 DCS/APC framing; unterminated, CAN/SUB-interrupted, malformed, over-budget, and recovery streams with adjacent text preserved.
- Kitty RGB/RGBA/PNG, bounded zlib, exact raw lengths, base64/chunk rules, transmit/store/display/query/delete actions, replacement, every supported delete selector/case, crop/scale/offset/cursor, and all z phases.
- Unicode placeholder 8/24/32-bit identities, row/column diacritics and inheritance, underline placement IDs, alpha background, ordinary cell movement/erase, horizontal mismatch, selection, and copy filtering.
- Sixel bit order, repeat, raster declarations smaller/larger than data, palette select/HLS/RGB extrema, transparency modes, `$`/`-`, 7/8-bit framing, DA enable/disable, xterm-compatible DECSDM/8452 matrix, bottom-margin scroll, clipping, reset, clear, and alternate screen.
- Decoder adversaries: gradual Sixel growth, huge repeats, zlib bombs, PNG dimension bombs, allocation failure, queue saturation, cancellation, and max/max-plus-one limits. Every failure is typed and bounded without panic or unbounded allocation.
- Ordered state: delete/reset/resize/screen switch/pane close during decode cannot resurrect stale state; failed replacement preserves protocol-defined prior state; query replies precede DA and occur once with zero/one/multiple viewers and control handoff.
- Atomic replay/handoff: detach, dropped-output resync, late attach, concurrent viewer, client restart, server upgrade, and handoff during framing/decode all converge on one generation without partial scenes.
- GPUI lifecycle: 1-pixel, max, and over-limit resources; first-paint upload; crop mechanism; shared placement reuse; deterministic eviction; final-reference `drop_image`; pane/session destruction; device loss; Linux WGPU and macOS Metal behavior.

Running-client functional/visual corpus:

- Ten owned ASCII-hex fixtures cover the frozen Kitty/Sixel matrix without importing an external test suite; `tests/e2e/fixtures/terminal-images/fixtures.tsv` owns their paths and expected outcomes.
- Yazi `v26.5.6` (`aa526434f00bb44e2e902d9a4ac5f810da1018b9`) renders through its generic successful Kitty probe (`KgpOld`) without terminal spoofing; owned placeholder fixtures separately prove `Kgp` semantics. Upstream Scribe recognition may improve adapter selection but cannot block task closure on an external merge.
- Chafa `1.18.2` is the dual-protocol previewer and gnuplot `6.0.3` with `sixelgd` is the plotting workflow.
- Direct PTY and an actual SSH path inside the authorized harness verify fragmented output and bidirectional replies.
- A network-disabled Docker pass proves core parsing, replay, rendering, settings, and fixtures remain local/offline; only the dedicated in-container SSH case enables its loopback transport.
- Kill switch causes no Kitty probe reply and DA without attribute 4, while ordinary terminal output remains correct.
- Stable visual captures cover alpha, crop/scale, placeholder redraw, z-order around backgrounds/text, scroll/margins, deletion, reset, resize, split panes, replay, and disabled/rejected affordance.

Performance follows clarification 7C through `tests/e2e/functional/terminal-images-performance.sh` and `tests/e2e/visual/terminal-images-frame-stability.sh`, invoked only with `just e2e-func` and `just e2e-visual`. They record text throughput, input latency, CPU, frame stability, decode/upload latency, and retained CPU/GPU measurements for qualitative material-regression review. No numeric performance-regression threshold gates release. Numeric performance goals are explicitly inapplicable for v1 under Constitution principle 4; its named-command measurement requirement still applies. Numeric resource ceilings remain hard security assertions.

macOS parity requires the same named application/protocol corpus before
default-on release. After the workflow is on the default branch and the target
ref contains executable `tests/native-macos/terminal-images-metal.sh`, a
repository maintainer with GitHub write access dispatches
`gh workflow run native-macos-metal.yml --ref <release-candidate-ref>`.
No secrets are required. The run uploads
`test-output/terminal-images/macos/` as
`native-macos-metal-<run-id>` for 14 days, including runner/display metadata
and the corpus log. Downstream work may invoke Scribe only through that driver
and only on this workflow.

The job has a 120-minute timeout and no `continue-on-error`. Any build, corpus,
timeout, missing-driver, or upload failure blocks GPUI platform work and
default-on release. Product failures require a fixing commit before rerun. A
maintainer may rerun an Actions infrastructure failure, but the release
evidence must retain both run URLs and its rationale. Acceptance requires a
green run for the exact candidate SHA plus the retained artifact; Linux Docker
or a successful macOS package build cannot substitute.

## Risks

- **Qualitative performance judgment:** clarification 7C can make release review subjective across hardware. Mitigation: explicitly mark numeric goals inapplicable under Constitution principle 4, keep named repeatable commands and raw measurements, preserve the human decision, and record the reviewer and rationale.
- **Decoder denial of service:** stock Sixel decoder limits are unsuitable. Mitigation: vendor decoder-only code with `DecodeLimits`, fallible allocation, checked growth/work counters, cooperative cancellation, attribution, adversarial corpus, and no encoder/quantizer.
- **Server/client divergence:** graphics affect cursor, scroll, reset, and erase. Mitigation: server produces canonical `TerminalGridEffect` under one session lock; client consumes effects instead of separately interpreting protocol semantics.
- **Slow decode ordering:** asynchronous completion may reorder effects. Mitigation: bounded worker semaphore with per-session ordered commit barrier; cancel/discard stale generations after delete/reset/close.
- **Replay size and backpressure:** existing IPC/output caps cannot hold a large scene. Mitigation: generation-tagged chunked replay with atomic commit and replay-dirty recovery; never retain per-sink duplicate image state.
- **GPUI limitations:** `paint_image` uploads lazily, public atlas accounting/limit queries are weak, and source cropping may be unavailable. Mitigation: early GPUI crop/eviction spike; independent dimension cap; choose a minimal pinned GPUI UV extension or bounded cached crop variants.
- **Memory accounting:** RGBA, BGRA/upload staging, atlas allocation, and multiple viewers amplify bytes. Mitigation: separate session CPU, per-view projected GPU, and process caps; reject before GPUI; measure staging/atlas estimates.
- **Yazi discovery:** generic Kitty query selects Yazi's classic path for unknown terminals, not placeholders. Mitigation: gate v1 image display on a pinned released Yazi using its generic Kitty probe, verify placeholder semantics with owned fixtures, and pursue upstream truthful Scribe detection as a non-blocking P3 follow-up; never impersonate Kitty.
- **Sixel ambiguity:** DEC and xterm disagree about DECSDM semantics. Mitigation: document/test xterm-compatible `?80` and `?8452` behavior as a deliberate compatibility choice.
- **Mixed versions:** capable server plus incapable viewer can produce invisible or cursor-divergent state. Mitigation: refuse incapable attachment to image-enabled sessions with a typed update-required result; use exact-match remote versions; exercise old/new local handshakes, old/new handoff, downgrade config, and maximum-scene rollback fixtures.
- **Native macOS evidence:** Linux Docker and the package-only release matrix
  cannot prove Metal behavior. Mitigation: only the manual GPU-backed workflow
  may run the native corpus, and its exact-SHA artifact remains a blocking gate.
- **Upstream maintenance/license:** vendored parsers/decoder create ownership. Mitigation: pin source revisions, retain MIT/Apache attribution, document delta, audit CVEs/licenses, and keep the fork decode-only.
- **Rollback:** malformed real-world sequences or GPU problems may require rapid disablement. Mitigation: master kill switch changes truthful advertising, releases state/resources, and leaves text path intact.

## Sequencing

The following work items become task beads. Titles intentionally omit numeric prefixes; priorities and explicit edges define order. Every behavior-changing task updates its affected `lat.md/` sections, runs `lat check`, and records Docker evidence under `test-output/terminal-images/`; final documentation consolidates user/support material rather than deferring architecture updates.

| Work item | Priority | Depends on | Verifiable acceptance |
| --- | --- | --- | --- |
| Specify image protocol contract and security limits | P0 | None | Commit the exact matrix, capability lifecycle, Sixel chronology, typed failures, numeric `ImageLimits`, app/version corpus, and owned fixtures in `specs/020-terminal-images/` and `lat.md/`; `just e2e-func terminal-image-contract.sh` passes and writes `test-output/terminal-images/contract.json`. |
| Approve and provision native macOS validation path | P0 | Protocol contract | Manual `.github/workflows/native-macos-metal.yml` uses hosted GPU-backed `macos-14-xlarge`; a write-access maintainer dispatches `gh workflow run native-macos-metal.yml --ref <release-candidate-ref>`, owns triage/review, and retains `native-macos-metal-<run-id>` for 14 days. The Actions-only guard fails before runtime when the downstream driver is absent; every non-success blocks platform GPUI work and release; `lat check` passes. |
| Spike bounded Sixel and Kitty decode | P0 | Protocol contract | `tests/e2e/functional/terminal-image-decode-spike.sh` proves fallible allocation, cooperative cancellation, zlib/PNG ceilings, max/max-plus-one dimensions, gradual-growth defense, and returns a written go/no-go decoder decision through `just e2e-func`. |
| Spike GPUI crop, upload, and eviction | P0 | Protocol contract; macOS path approval | `tests/e2e/visual/terminal-image-gpui-spike.sh` chooses existing API, bounded crop cache, or pinned GPUI UV patch; proves upload reuse, `drop_image`, device loss, and dimension cap on Linux plus defines the native Metal assertion; `just e2e-visual` passes. |
| Define common image types and IPC fixtures | P1 | Protocol contract | Add `scribe-common` image types, capability fields, bounded live/replay messages, sequence boundaries, typed mismatch response, remote version bump, serde/MessagePack fixtures, and `scribe-test` decoding; `just e2e-func terminal-image-ipc.sh` proves old/new local handshakes and remote mismatch. |
| Implement bounded graphics framing and parsers | P1 | Protocol contract | Add split-safe APC/DCS framing, narrow Kitty commands, Sixel/xterm modes, CAN/SUB/unterminated recovery, and raw-byte boundary annotations; `just e2e-func terminal-image-framing.sh` passes every split and malformed corpus without swallowing adjacent text. |
| Vendor bounded Sixel decoder | P1 | Decode spike | Follow [`decoder-decision.md`](decoder-decision.md): pin the `icy_sixel` 0.5.0 source/checksum and MIT/Apache files, document fork delta/CVE owner, remove encoder/`quantette`, add `DecodeLimits` plus fallible checked growth/work/cancel hooks; `just e2e-func terminal-image-sixel-decoder.sh` passes and `cargo tree` evidence excludes `quantette`, full `termwiz`, and C Sixel libraries. |
| Implement bounded Kitty data normalization | P1 | Decode spike | Follow [`decoder-decision.md`](decoder-decision.md): add direct `base64` 0.22.1 and `flate2` 1.1.9 plus a pinned decoder-only `png` 0.18.1 fork with shared work/cancel hooks, exact raw lengths, and bounded inflate/decode; `just e2e-func terminal-image-kitty-decode.sh` rejects bombs, indirect sources, and non-PNG formats before retained allocation. |
| Build authoritative server image state engine | P1 | Parsers; Sixel decoder; Kitty normalization; common model | Add `SessionTerminal`, canonical RGBA definitions/placements, xterm Sixel grid effects, deterministic quotas, ordered decode barrier, cancellation, and viewerless retention; `just e2e-func terminal-image-server-state.sh` proves later bytes/replies never overtake a graphics command. |
| Integrate server fanout, replies, and capability lifecycle | P1 | Server state; IPC fixtures | Preserve raw `PtyOutput`, sequence typed deltas, answer Kitty/DA once through PTY write-back, latch capability across detach, refuse incapable attach, and handle kill-switch transitions; `just e2e-func terminal-image-replies-sharing.sh` proves zero/one/multiple-viewer and controller cases. |
| Build client live image scene | P1 | IPC fixtures | Add ordered pane operations, immutable CPU definitions/placements/grid effects, copy filtering, generation cleanup, and mismatch UI plumbing without requiring replay implementation; `just e2e-func terminal-image-client-scene.sh` passes fixture-driven live updates. |
| Implement combined replay and backpressure recovery | P1 | Server fanout; IPC fixtures | Add generation-tagged begin/chunk/placement/commit replay, bounded live buffering, replay-dirty recovery, and max-scene chunking; `just e2e-func terminal-image-replay.sh` proves atomic late attach, dropped-output recovery, and simultaneous viewers. |
| Stage atomic image replay in the client | P1 | Client live scene; combined replay | Stage definitions/placements off-screen and swap only on commit while buffering later live operations; `just e2e-func terminal-image-client-replay.sh` proves no partial scene or stale-generation resurrection. |
| Persist image state through server handoff | P1 | Server state; combined replay | Pause reads, commit/cancel bounded decode, transfer committed state plus partial framer/chunks in bounded begin/chunk/commit frames, and stage before resume; `just e2e-func terminal-image-handoff.sh` covers partial APC/DCS, chunk accumulation, max scene, old-to-new restore, new-to-old rollback refusal, and downgrade config. |
| Render layered GPUI image placements | P1 | Client live scene; GPUI spike | Add per-view `RenderImage` cache, chosen crop path, classic/placeholder mapping, six paint phases, Sixel chronology, clip/scroll/margin/resize/DPI semantics, eviction, and final-reference `drop_image`; `just e2e-visual terminal-image-renderer.sh` writes stable captures. |
| Add enablement, diagnostics, and settings | P2 | Server fanout; client live scene | Add default-on master switch, truthful runtime changes, payload-free typed diagnostics/placeholder, localization, renderer-failure cleanup, and offline behavior; `just e2e-func terminal-image-settings.sh` proves disable/re-enable and no payload persistence/logging. |
| Pin truthful application compatibility corpus | P1 | Server replies; renderer | Pin Yazi using generic Kitty probe, Chafa, and gnuplot Sixel versions; add direct PTY and in-container SSH scripts without terminal spoofing; `just e2e-visual terminal-image-apps.sh` proves image output while owned fixtures prove Unicode placeholders. Upstream Yazi Scribe recognition is optional P3 follow-up, not a blocker. |
| Verify protocol safety and session continuity | P1 | Parsers; decoders; server fanout; replay; client replay; handoff; settings | `just e2e-func terminal-images-functional.sh` passes quotas, recovery, ordering, replies, viewerless output, overflow, attach, SSH, renderer failure, kill switch, upgrade/rollback, and a network-disabled local-only pass. |
| Verify running-client image behavior | P1 | Renderer; app corpus; replay; settings; functional verification | `just e2e-visual terminal-images-visual.sh` passes RGB/RGBA/PNG/Sixel, placeholders, z-order, Sixel chronology, crop, scroll/reset/delete/resize/splits/replay, disabled/rejected affordance, and application captures. |
| Run native macOS Metal parity corpus | P1 | MacOS path approval; renderer; shared functional/visual fixtures | Execute the sanctioned native command on the same subset and pinned apps, recording Metal scale/texture/upload/eviction evidence and same capability matrix; max-plus-one rejects before GPUI. This blocks default-on release. |
| Record qualitative performance and resource review | P2 | Functional verification; visual verification | Run the named functional/visual performance scripts, record text/input/CPU/frame/decode/upload/CPU-GPU data, and publish a human qualitative material-regression conclusion with no invented threshold; preserve hard security ceilings and the principle-4 inapplicability rationale. |
| Consolidate compatibility, operations, and support docs | P2 | Stable implementation; verification results | Consolidate already-updated `lat.md`, settings/help, exact matrix, limits, SSH/multiplexer policy, payload privacy, vendored attribution/CVE owner, diagnostics, kill-switch rollback, and evidence paths; `lat check` passes. |
| Run terminal-images release gate | P0 | Functional; visual; native macOS; qualitative review; docs; dependency audit | Produce `test-output/terminal-images/release-manifest.json` linking every spec criterion to passing evidence, confirm no P4 work, run Docker quality gates and `lat check`, and enable default-on only when safety, session continuity, app, rollback, and supported-platform gates are green. |

Parallelism after the contract frontier is explicit: decoder and GPUI spikes run beside common IPC work once the macOS path is approved; Kitty/Sixel decoders and framing run in parallel; client live-scene work starts from IPC fixtures while server state proceeds; renderer work starts from the live scene and GPUI decision while replay/handoff proceed; settings and application corpus proceed beside replay after replies/rendering exist.

## Backlog Refinement

None. No open P4 backlog source entered this molecule. Closed `scribe-38e.105` remains prior research only and is covered by the new protocol-contract, decoder, architecture, renderer, and verification work without reopening it.

## Target Epic

Create a new `terminal image protocol support` feature epic. All work items above become P0-P2 child tasks with explicit blocking edges; no P4 task is permitted.

## Alignment fixes applied

- Restored existing raw `PtyOutput` compatibility and added sequenced typed image state at explicit commit boundaries instead of replacing the terminal byte stream (alignment A, must-fix).
- Defined server-side bounded normalization as deliberate clarification-2 ownership, plus a bounded ordering barrier so later PTY bytes cannot overtake graphics decode/state effects (alignment A/B, must-fix).
- Added an exact Kitty/Sixel/platform/application matrix, explicit Sixel text chronology, direct-only exclusions, Windows/video exclusions, and payload-free diagnostics that never replace application fallback (alignment A, must-fix/should-fix).
- Pinned direct dependency versions and added vendored revision, license, fork-delta, CVE ownership, and dependency-tree evidence (alignment A/B, must-fix).
- Named Yazi, Chafa, and gnuplot behavior without making task completion depend on an external upstream merge; added an offline validation pass (alignment A/B, must-fix/should-fix).
- Split macOS validation into early path approval and later native Metal parity execution, making missing sanctioned infrastructure an early P0 blocker (alignment B, must-fix).
- Rebuilt sequencing around common IPC fixtures, server core/fanout, client live/replay, and renderer dependencies to remove circular and false serialization (alignment B, must-fix).
- Added observable acceptance, artifact paths, Docker `just` commands, output evidence, per-task `lat.md` updates, and a final evidence manifest to every work item (alignment B, must-fix).
- Chose bounded handoff begin/chunk/commit transfer with partial framer state and bounded decode commit/cancel instead of leaving quiesce-versus-serialize unresolved (alignment B, must-fix).
- Defined incapable-viewer refusal and capability behavior across detach, viewerless output, rollback, and kill-switch transitions instead of silently degrading image-enabled sessions (alignment A/B, must-fix).
- Preserved clarification 7C unchanged. Numeric performance goals are explicitly inapplicable under Constitution principle 4, while named commands and recorded measurements remain mandatory; the alignment pass did not override the human decision (alignment A/B, resolved interpretation tension).
