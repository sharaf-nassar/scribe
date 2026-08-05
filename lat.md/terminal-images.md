# Terminal Images

Terminal images are server-owned, bounded Kitty and Sixel state derived from untrusted PTY bytes and rendered from immutable typed client state.

The frozen source contract is `specs/020-terminal-images/contract.md`; its
machine-readable values and owned fixtures live under
`tests/e2e/fixtures/terminal-images/`.

## V1 Protocol Contract

V1 accepts direct-inline Kitty RGB, RGBA, PNG and xterm-compatible Sixel without exposing host resource loaders.

Kitty uses 7-bit APC `G`/ST, direct `t=d`, formats `f=24/32/100`, RFC 1950
`o=z`, official 4096-byte `m=1/0` chunks, and actions `t/T/p/q/d`. Classic
crop, cell span, pixel offset, cursor movement, signed z-order, and Unicode
`U=1` placeholders are supported. Placeholder identity uses `U+10EEEE`,
official diacritics, foreground image id, underline placement id, and
specified left-cell inheritance. The IPC placeholder background byte is
reserved compatibility data, not a second image-opacity channel.

Kitty delete selectors are exactly `a/A`, `i/I`, `p/P`, `x/X`, `y/Y`, and
`z/Z`. Any delete aborts an incomplete transfer. `q=0` permits success and
failure replies, `q=1` suppresses success, and `q=2` suppresses all replies.
Retransmitting an image id replaces its data and placements; reusing an
image/placement pair replaces that placement.

Physical Kitty and Sixel row, column, and cell deletes select against the
placement's effective clipped cell extent, not only its anchor. Virtual
`U=1` placements ignore all, placement, cell, row, column, and z selectors;
only an image selector carrying their explicit image id removes them. A hard
delete frees only definitions selected by placements it actually removed,
once no placement references them. An explicit image-id delete can free an
unplaced target. Unrelated unplaced definitions and data used by a protected
virtual placement survive every other hard scope.

Kitty images below z `-1073741824` paint beneath non-default backgrounds;
other negative images paint above backgrounds and below glyphs; nonnegative
images paint above glyphs. ED2 clears visible placements, RIS clears all image
state, 1049 owns an empty alternate-screen image set, and other text erases do
not affect Kitty graphics.

Sixel accepts 7-bit and C1 DCS/ST, `P1;P2;P3 q`, sixel bits, repeat, raster
attributes, 256 palette selection and HLS/RGB definitions, `$`, and `-`.
`P2=0/2` paints zero bits with the background; `P2=1` preserves them. Runtime
DA1 adds attribute 4 exactly once.

V1 excludes Kitty indirect transports, C1 APC, file/temp/shared-memory/URL or
network loading, animations, frame composition, relative-parent placement,
image-number allocation, iTerm2, ReGIS, vector/video, protocol translation,
Windows, and guaranteed tmux/GNU screen/Zellij passthrough.

### Support matrix

One table fixes what v1 accepts and what it refuses, so a support question is answered by protocol, transport, format, action, and platform instead of by reading the contract prose above.

| Axis | Supported | Excluded |
| --- | --- | --- |
| Protocol | Kitty graphics over 7-bit APC `G`/ST; Sixel over 7-bit and C1 DCS/ST | iTerm2 OSC 1337, ReGIS, vector and video protocols, protocol translation |
| Kitty transport | direct inline `t=d` | `t=f`, `t=t`, `t=s`, URL and network loading, 8-bit C1 APC |
| Kitty format | `f=24` RGB, `f=32` RGBA, `f=100` PNG, RFC 1950 `o=z` compression | every other `f=` value, APNG and animation, frame composition |
| Kitty action | `t`, `T`, `p`, `q`, `d` | `I`, `N`, `P`, `Q`, image-number allocation, relative-parent placement |
| Kitty placement | classic placements and Unicode `U=1` placeholders | — |
| Sixel | `P1;P2;P3 q`, sixel bits, repeat, raster attributes, 256-entry palette with HLS and RGB definitions, `$`, `-` | — |
| Platform | Linux X11 and Wayland, native macOS | Windows |
| Session transport | direct PTY, SSH | guaranteed tmux, GNU screen, or Zellij passthrough |
| Applications | Yazi, Chafa, and gnuplot at the pinned corpus versions | — |

Rejection is typed only where Scribe recognizes the framing. An 8-bit Kitty APC
or an unsupported `f=`, `t=`, or action produces a payload-free diagnostic
category, but an iTerm2 `OSC 1337;File=` sequence is not intercepted at all: it
passes through as ordinary bytes, renders nothing, and produces no notice.

## Trust Boundary and Limits

ImageLimits rejects untrusted work before allocation, retention, IPC replay, or GPU upload with checked arithmetic and per-session isolation.

| Field | Exact v1 ceiling |
| --- | ---: |
| control string | 16,777,216 bytes |
| Kitty chunk payload | 4,096 encoded bytes |
| chunks per transfer | 32,768 |
| accumulated encoded transfer | 89,478,488 bytes |
| base64-decoded transfer | 67,108,864 bytes |
| inflated output | 67,108,864 bytes |
| width / height | 4,096 pixels each |
| pixels | 16,777,216 |
| canonical RGBA definition | 67,108,864 bytes |
| definitions per session | 128 |
| placements per session | 1,024 |
| retained session CPU data | 134,217,728 bytes |
| projected GPU data per view | 268,435,456 bytes |
| retained image data per process | 536,870,912 bytes |
| concurrent decodes per process | 2 |
| queued decodes per process | 8 |
| queued encoded data per process | 134,217,728 bytes |
| work per command | 134,217,728 units |
| queue wait | 1,000 monotonic ms |
| decode | 2,000 monotonic ms |
| replay/handoff chunk | 1,048,576 bytes |
| deadline check interval | 4,096 work units |

One work unit is one input byte examined, output byte emitted, or pixel write,
charged cumulatively. Deadline checks occur during work, not only afterward.
The 64 MiB image ceiling is one 4096-square RGBA definition; larger session,
view, and process caps account separately for copies and concurrency.

Framing charges bytes before append; transfer/base64/inflate checks guard each
buffer growth; dimension and pixel checks precede allocation; session/process
retention checks precede commit; view projection precedes IPC/upload. Queue
depth and bytes are charged before enqueue, then queue and decode deadlines use
monotonic time.

Three ceilings sit outside `ImageLimits` and bound seams rather than the decode
path: the IPC and replay chunk carrier
[[crates/scribe-common/src/terminal_images.rs#BoundedImageBytes]] at 1,048,576
bytes, the client's staged live buffer at
[[crates/scribe-client/src/terminal_image_scene.rs#MAX_BUFFERED_LIVE_RECORDS]]
records, and
[[crates/scribe-server/src/terminal_image_handoff.rs#MAX_HANDOFF_IMAGE_BYTES]]
at 134,217,728 bytes — half of the 268,435,456-byte `HandoffState` ceiling every
session's upgrade payload shares.

## SSH and Multiplexer Policy

Image bytes are ordinary PTY bytes: nothing in Scribe detects, wraps, or unwraps a transport, so SSH works by construction and multiplexers are out of scope rather than handled.

No SSH-aware, tmux-aware, GNU-screen-aware, or Zellij-aware code path exists in
the image seam. The framer runs on whatever arrives on the local PTY, so a
remote application's Kitty APC and Sixel DCS sequences and Scribe's own PTY
replies traverse an SSH hop unchanged and unattributed. `tests/e2e/terminal-images-functional.sh`
proves that against a real loopback `sshd` and `ssh -tt` inside the container and
writes `functional-probe-ssh.txt`. The evidence is Linux-only: the native macOS
manifest lists `ssh_transport` in `not_covered_natively`, because a hop is a
transport fact rather than a platform one.

Multiplexers get no promise and no code. Scribe emits no `ESC Ptmux;` wrapper,
strips none, and does not chunk `ESC P` for GNU screen, so whether an image
survives depends entirely on the multiplexer's own passthrough configuration and
is untested. Running an image application directly in a Scribe pane is the
supported arrangement; under a multiplexer that drops or mangles a sequence, the
application's textual fallback is what remains.

## Payload Privacy

Image bytes and decoded pixels live in process memory and in the local IPC and handoff sockets only; they never reach logs, telemetry, crash metadata, settings, or a disk cache.

No image module writes a file. The framer's retained buffers, the server's staged
and owned storage, the client's pending definitions, and the GPUI `RenderImage`
cache are all in memory, and the only cross-process copies are the local IPC
socket and the hot-upgrade handoff socket — never a temporary file, a cache
directory, or the config file, which persists nothing about images beyond the
`terminal.images.enabled` boolean.

Nothing on the payload path logs. The image log sites that do exist carry scalars
and enums only: the master-switch transition, and the client's per-placement
warning naming a session id, an image id, and a typed rejection category. The
guarantee is structural rather than a review rule.
[[crates/scribe-common/src/terminal_images.rs#TerminalImageRejection]] has no
string or byte field at all, `Debug` for the payload carriers prints lengths and
capacities instead of contents, and Scribe ships no telemetry, analytics, or
panic-hook path that could serialize an image.

## Capability Lifecycle

Capability is latched server session state, so viewer churn cannot alter protocol replies or authoritative image behavior.

A session starts text-only and unlatches. A capable creator or first explicit
capable enable latches exact v1. A latched viewerless session keeps parsing,
replying, and retaining bounded state. Every Kitty result and DA1 response is
written once by the server to the originating PTY in input order.

An incapable viewer gets typed `capability_mismatch` instead of a degraded
attach. Effective advertising intersects compile support, decoder health,
viewer capability, and the master switch. Disabling cancels queued/active work,
clears partial and committed state, removes DA attribute 4, emits no Kitty
discovery reply, then unlatches. Re-enable needs a new capable latch.

Capable handoff preserves latch, generation, committed state, and bounded
partial framing. Unsupported downgrade fails typed; detach, replay, reconnect,
handoff, and multiple viewers never synthesize duplicate PTY replies.

## Typed IPC Contract

Shared image types keep local compatibility while giving remote peers one exact bounded wire contract.

[[crates/scribe-common/src/terminal_images.rs#ImageLimits]] freezes every v1
security ceiling in code. Canonical definitions carry typed IDs, generation,
checked RGBA dimensions, and byte length. Actual bytes travel only through a
custom-deserialized `BoundedImageBytes` chunk capped at 1,048,576 bytes; the
contract cannot represent paths, URLs, shared-memory names, or other indirect
loaders.

Placements carry protocol, typed image and placement identity, generation,
cell anchor, source crop, destination cell extent, pixel offsets, z-index,
scroll/cursor behavior, an optional exclusive logical-cell clip, and bounded
placeholder metadata. The clip defaults absent for replay compatibility and
preserves original source mapping through repeated scroll and resize. Typed
grid effects bind image-driven cursor, scroll, erase, resize, screen, and reset
consequences to the same ordered client operation stream.

Common placement validation rejects empty geometry, inconsistent
protocol/kind/placeholder combinations, and invalid clips before replay or
client ingestion. A clip is nonempty, bounded by exclusive cell coordinate
65,536, forbidden on virtual placeholders, and contained by the placement's
logical envelope. That envelope adds one right or bottom cell when the
corresponding pixel offset is nonzero.

Live IPC uses generation-tagged begin, update, and commit records under one
monotonic output sequence. Replay uses begin metadata, definition metadata,
bounded definition chunks, placements, and commit; every record repeats the
generation so a client can reject mixed snapshots before exposing partial
state. Replay begin validates definition, placement, and retained-byte totals.

Local `Hello` and `Welcome` image capabilities default every missing field to
false, preserving old/new MessagePack decoding. An incapable attach has a
typed capability mismatch with required and offered sets. Remote protocol v5
uses exact matching and a typed mismatch naming client/server versions plus
which endpoint must update.

## Sixel Chronology

Sixel mode behavior deliberately follows current xterm polarity instead of DEC's contradictory DECSDM description.

At DCS start the server captures cursor, margins, screen, `?80`, and `?8452`;
later PTY bytes wait for atomic commit/rejection. Under default `?80l`, the
raster anchors at the cursor and scrolls vertically at the bottom margin. At
ST, `?8452l` advances to the next complete row at the original column, while
`?8452h` advances immediately right of the raster.

Under `?80h`, the raster anchors at the page origin, crops without scrolling,
and leaves the cursor unchanged; `?8452` has no cursor effect. Rasters paint
above default background and below non-default backgrounds and glyphs. Grid
erase, scroll, trim, resize clipping, alternate-screen destruction, and RIS
apply in byte chronology. Resize clips without resampling; DECSTR preserves
committed raster and resets modes.

## Bounded Framing and Parsing

The graphics framer recognizes the frozen image subset across arbitrary PTY reads while preserving exact byte order for the existing terminal path.

`GraphicsFramer` runs inside the server image-state seam and holds at most one
bounded APC/DCS. A speculative DCS candidate retains raw header
bytes up to the control-string ceiling because a non-`q` final byte must pass
through exactly; its parallel Sixel parameter scanner uses three fixed slots,
one checked numeric accumulator, and field/status metadata. Seven-bit Kitty APC
and seven-bit/C1 Sixel DCS accept split introducers and terminators. CAN or SUB
cancels a recognized image string; EOF reports a typed truncated sequence;
over-budget input is discarded without retaining more payload until ST restores
the ordinary byte stream. A numeric Sixel-looking DCS header that crosses the
ceiling is charged through its offending digit or separator, converted to the
same failed discard state, and never returned as raw terminal bytes.

Every public framer constructor requires a session/process storage budget.
Candidate, active, and completed payload buffers always carry a move-only real
lease. Storage rejection abandons the affected candidate or active string and
returns `GraphicsStorageRejection` instead of fabricating a protocol failure or
silently reporting image parsing as successful raw fallback.

Recovery is overlap-safe: a repeated ESC can begin the real seven-bit ST, an
ESC followed by C1 ST ends with typed malformed framing, and an ESC/C1 control
after an abandoned introducer candidate is reprocessed from ground. Sixel's
three introducer parameters use constant scanner metadata and mark a fourth
field at its separator or numeric overflow at its offending digit. Once `q`
confirms Sixel, a malformed header seeds the active typed failure, so subsequent
body bytes are counted for termination recovery but never copied into the body
buffer. ST, CAN, SUB, and EOF extend the failure's exact source range without
replacing an earlier malformed or quota category; cancellation is malformed
framing only when no earlier failure exists.

Each result carries a half-open absolute raw-byte range. `Raw` events and
recognized `?80`/`?8452` mode annotations expose their exact terminal bytes for
one downstream feed; Kitty/Sixel commands and failures expose no terminal
payload. This lets the later ordered server integration suppress image strings,
forward every unrelated byte once, and preserve an auditable commit boundary.

Kitty parsing accepts only direct `t=d`, `f=24/32/100`, `o=z`, actions
`t/T/p/q/d`, v1 placement/delete controls, quiet/chunk flags, and encoded chunks
up to 4,096 bytes. Sixel parsing accepts the three DCS parameters, sixel data,
repeat, raster attributes, palette selection/HLS/RGB, graphics CR/newline, and
the exact xterm private modes. Unsupported or malformed controls return the
frozen typed category with safe limit identity only, never payload bytes.

## Server-Owned Session State Seam

One production `SessionTerminal` owns terminal-image ordering and state so later hardening extends one path instead of creating probe-only engines.

Each production PTY reader owns exactly one seam through
`PtyTerminalImageState`. The seam owns `GraphicsFramer`, generation and
sequence cursors, active screen, definition and placement maps, and
payload-free pending-transfer metadata. Sessions share one immutable v1
process policy.

Placement identity is keyed by screen plus the complete protocol identity
`(image_id, placement_id)`, matching client canonical state. Sequence
admission uses a checked upper bound of one non-raw boundary per input byte.
Normal reads parse directly, avoiding copies of retained transfer payloads;
only reads near sequence exhaustion use speculative rollback framing.

Each production PTY read advances the seam before delivering the same effective
bytes to the client once. The seam returns typed `Raw` or sequenced `Image`
boundaries in source order. Recognized Sixel modes also produce an image-side
boundary. No image fanout or PTY reply write-back is connected yet.

The existing Alacritty ANSI processor feeds every byte to the real production
`Term` exactly once. It may split one `advance` call at completed graphics
boundaries so later bytes in the same read cannot contaminate an image-time
observation; ordered boundary ends are consumed and deduplicated linearly, and
the normal no-image read remains one call. Split controls create no effects
until Alacritty changes state or the framer completes a boundary. If image
framing rejects a read, the same bytes still cross the delegating handler once
as one full span because no committed image cuts exist. The live reader and
Docker probe share one ingress orchestration seam, so client delivery, `Term`
mutation, typed rejection, and payload-free logging each keep one production
occurrence.

One delegating `Handler` observes that same `Term`; no replay parser or
image-only cursor engine exists. Payload-free snapshots retain active screen,
primary and alternate dimensions, each grid's current and saved cursor plus
deferred-wrap flag, margins, origin/wrap modes, and cell pixel metrics. Typed
effects use half-open scroll and erase bounds and cover ED2, reset, screen
switch, and both-grid resize. Image cursor movement delegates to Alacritty's
`goto`, clearing deferred wrap in the terminal and observer together.

Callback decisions use live post-delegation Alacritty modes. A same-span mode 7
change therefore controls pending-wrap scrolling immediately, while DECCOLM
set and unset reset the active grid and margins and emit a full-display erase.
ED1 mirrors pinned `0.26.0-rc1`: row index 1 does not clear row 0, while rows
above index 1 clear preceding rows plus the cursor-row left portion.

Printable-input effects follow pinned Alacritty character widths: combining
characters return before a pending wrap, while a width-two glyph that does not
fit calls `wrapline` and may scroll without a pre-existing pending wrap.

VTE synchronized-update expiry flushes buffered callbacks through the same
delegating handler. Timeout expiry consumes no new source bytes, so its result
is a state/effect observation without a fabricated input range or replay.

The observer handle is session-owned and shared with live, attach-time, paced,
and shared-window resize paths. `Term::resize` runs first, resizing active and
inactive grids and resetting margins; one internal resize effect then records
both dimensions. Only the active grid's cursor and saved cursor are publicly
readable afterward; inactive cursor facts are marked unavailable rather than
clamped or synthesized. Activating that grid refreshes them from the real
`Term`. Observations remain internal and carry no cells or payloads.

Canonical Kitty and Sixel replacements, image outputs, and sequence are staged
for the complete read while all pre-read owners remain live. Only a read whose
every canonical allocation succeeds swaps slots and sequence. A later storage
error drops staged/event leases and preserves exact pre-read canonical bytes,
ownership, and sequence while ordinary ingress sinks still run once.

Payload-free work counters distinguish direct reads from speculative clones.
Transactional rejection preserves framer offsets, pending metadata, sequence,
screen, definitions, and placements. Ownership current/peak counters roll back
with canonical state, while reservation, allocator, and reconciliation attempt
telemetry remains monotonic for the rejected work.

## Exact Requested Storage Accounting

Session and process ledgers cover every retained production image buffer before allocation while reporting requested live storage separately from allocator-observed capacity.

Each `SessionTerminal` owns an independent session ledger and a handle to the
process ledger frozen in its shared policy. A move-only reservation precomputes
complete process and session snapshots under one fixed-order critical section.
It validates every counter, health, and limit transition before committing both
scopes ahead of `Vec::try_reserve_exact`, then atomically reconciles any extra
observed capacity before retention. Drop precomputes both exact releases first.
Paired preflight always inspects both snapshots: counter or invariant failures
from either ledger outrank capacity pressure, then process capacity maps to
`ProcessLimit` and session capacity remains `SessionLimit`. Neither ledger
commits when either side rejects.

Framer candidate and body growth uses fallible replacement buffers. The old
buffer and reservation remain live until the requested replacement and its
observed capacity are covered, so failed growth preserves canonical bytes.
Kitty parsing moves one opaque, non-cloneable bytes-plus-lease owner into its
command instead of copying payload. Commands expose only borrowed slices, and
the owner moves through graphics events and ordered session boundaries. Sixel
and retained raw fallback bytes follow the same path.

Each PTY read creates zero-byte leased event and output vectors, then grows them
geometrically under the same paired ledger before allocation. Consuming such a
vector moves its lease into the consuming iterator, so ownership is released
only after the backing allocation is freed rather than when iteration starts. Ordinary short
raw text stays inline; retained fallback bytes remain charged until their event
drops. Candidate, active, and EOF transactions preserve exact buffer length,
capacity, digest, offset, state, and owner when metadata or payload growth
fails, so retry publishes each event once.

Pending Kitty transfer bytes, Sixel bodies, and Kitty or Sixel decoded buffers
use the same paired owner. `DecodeStorage` is a concrete required type
by the Kitty, PNG, and Sixel decoder APIs; it returns an opaque move-only
`DecodeBuffer` whose lease reserves before the real `Vec` allocation and
reconciles its capacity before decoder mutation. Base64, zlib, PNG inflate,
canonical RGBA, and Sixel canvas growth are geometric: copy work is charged,
the replacement lease and observed capacity are covered while the old owner is
live, then one copy and swap releases the superseded owner. No production
decoder has an unaccounted convenience entry point or forgeable storage trait.
Kitty transfer ingestion owns encoded and compressed-input work once; zlib
inflation owns produced-output and geometric-copy work. Work admission always
precedes the work it admits: buffer initialization, canvas fills, and pixel
copies are charged before the bytes are touched, so a refused decode never
reserves or initializes the buffer it was refused for. When ceilings overlap,
the first bound actually reached wins rather than relabeling work as storage.

`SessionTerminal` feeds each Kitty chunk exactly once into one real transfer;
it does not concatenate and re-feed prior payload. Protocol-significant
controls come from the first chunk, and the published final boundary of a split
transfer carries that first command's controls and control presence rather than
the last chunk's defaults; only payload, chunk state, and range stay local to
the final chunk. Equal explicit repeats are accepted,
conflicts fail without merging, queries validate without publishing retained
image state, and the final RGBA lease moves into retained state without an
encoded duplicate. Sixel decoding still occurs at its complete DCS boundary.
Sixel DCS settings pass from the production framer into the vendored decoder.
Replacement reserves the simultaneous old-plus-requested-new peak; only a
successful allocation swaps state. Session/process rejection, counter overflow,
allocation failure, and internal invariant failure remain distinct
`SessionTerminalError::Storage` outcomes and never enter the frozen protocol
failure taxonomy.

Session transactions serialize every process peak-increasing reservation and
reconciliation. Current ownership is always the sum of live leases and is never
restored wholesale: failed work drops provisional leases, while unrelated
concurrent releases remain applied. Rollback restores only commit-visible peaks
to the transaction checkpoint. Attempt telemetry stays monotonic, preserving
proof that reservation preceded each allocator or reconcile stage without
publishing a rejected peak.

Counters expose requested current/peak, observed current/peak, reservation and
allocator attempts, successful pre-allocation reservations, and
observed-capacity reconciliations. An immutable validation observer forces
extra reported capacity through framer and canonical allocation paths. These
are storage-accounting facts, not allocator metadata or process RSS.
Grid span observations and their typed effect vectors are reserved from the
same paired ledger before allocation and travel with the commit that owns them.
The already-fed terminal is never rewound, so storage pressure truncates that
payload-free list and returns a typed rejection with the commit instead of
allocating outside the ledger.

Validation rejection targets name candidate, active, event metadata, output
metadata, canonical Sixel, grid observation, and Kitty/Sixel decoded allocation
classes. There is
no canonical Kitty class because the live transfer/decoded owner is retained
directly. Class-local occurrence counts cannot be
satisfied by unrelated framing or decoding work.

## Mandatory Decode Scheduling

Every image decode is admitted by one process-owned scheduler, so no caller can charge decode work outside the frozen concurrency, queue, and byte ceilings.

[[crates/scribe-image-decode/src/scheduler.rs#DecodeScheduler]] owns the
immutable [[crates/scribe-image-decode/src/scheduler.rs#DecodeCeilings]]
derived from the frozen limits: two concurrent decodes, an eight-deep queue,
134,217,728 queued bytes, and a 1,000 ms queue wait. One
[[crates/scribe-image-decode/src/scheduler.rs#DecodeTicket]] is issued per
request and consumed by admission, which returns the only
[[crates/scribe-image-decode/src/scheduler.rs#DecodePermit]] that can exist -
the type has no public constructor.

The permit, not the caller, owns the session storage handle, so
[[crates/scribe-image-decode/src/lib.rs#DecodeBudget]] - the type both the
Kitty and Sixel decoders charge - cannot be built without one. That is what
makes admission mandatory rather than advisory: a decode entry point that skips
scheduling does not compile.

Each ticket binds an issuer, session, generation, target, requested byte count,
and storage budget.
[[crates/scribe-image-decode/src/scheduler.rs#DecodePermit#authorize]] re-checks
all five where the work is about to run and returns a typed
[[crates/scribe-image-decode/src/scheduler.rs#DecodeAdmissionError]] first, so a
permit minted for one session, generation, or target cannot be replayed against
another. [[crates/scribe-server/src/terminal_image_state.rs#SessionTerminal]]
issues, admits, and authorizes one request per Kitty and Sixel decode boundary;
a refused admission becomes a typed quota failure, and a foreign capability is
an internal invariant because the seam is presenting a ticket it just issued.

Admission is strict FIFO by issue order: a waiter is eligible only at the head
of the queue, so a later request can never barge. Issue and admission are
therefore a pair - the seam admits on the thread that issued - and a ticket
dropped without admission removes itself and wakes the queue, which is what
keeps an abandoned ticket from parking the line.

Cancellation is per session target.
[[crates/scribe-image-decode/src/scheduler.rs#DecodeScheduler#cancel_target]]
flags queued and in-flight entries for exactly one transfer or target image,
wakes every waiter, and leaves unrelated targets and sessions untouched. An
in-flight cancellation is observed by the shared budget's cooperative check, so
decoding stops at the next observation boundary instead of running to
completion. A waiter that outlives its queue wait retires itself the same way
and wakes its successor. Ownership is released exactly once by `Drop` on the
ticket or the permit, never by a caller.

Queue metadata is bounded by construction: at most one queue-depth of waiters
and one concurrency ceiling of active entries, each payload-free. A request over
the byte ceiling, or one arriving at a full queue, is refused at issue before
any storage is reserved, and an unrelated session keeps its own slot.

## Incomplete Transfer Retirement

A transfer that never completes is discarded with a typed boundary, never as an image: stream end, reset, close, cancellation, and queue-wait expiry all release its storage and decode admission exactly once.

[[crates/scribe-server/src/terminal_image_state.rs#SessionTerminal#retire_transfers]]
takes one
[[crates/scribe-server/src/terminal_image_state.rs#TransferRetirement]] reason
and returns an ordinary commit, so retirement shares the session's output
sequence rather than a side channel. That is what preserves chronology: a Kitty
query reply already owed keeps its earlier sequence, and the retirement boundary
follows it.

`StreamEnd` is EOF. It drains the framer through
[[crates/scribe-pty/src/graphics_framing.rs#GraphicsFramer#finish]], so an
unclassified candidate is still emitted as ordinary raw text while an
unterminated APC or DCS string becomes a `TruncatedSequence` failure carrying its
protocol. `Reset` and `Close` instead call
[[crates/scribe-pty/src/graphics_framing.rs#GraphicsFramer#discard]]: a parser or
session reset destroys the terminal context those bytes belonged to, so no raw
text and no failure boundary is owed for them.

Kitty chunks are the case the framer cannot see. Each chunk is a complete APC
string, so an abandoned multi-chunk transfer leaves buffered payload with the
framer already in ground state. Retirement takes that pending transfer first -
releasing its charged bytes even if the boundary below cannot be recorded - and
reports the span from its first chunk through its last as one truncated
sequence. Nothing incomplete ever reaches a definition, a placement, or a
generation.

`Close` additionally cancels the session's outstanding admissions through
[[crates/scribe-image-decode/src/scheduler.rs#DecodeScheduler#cancel_session]]
and releases every retained buffer, so a decode queued behind another session's
slot cannot outlive the seam that issued it. Cancellation and deadline refusals
arriving during an ordinary read retire the same way: the refused admission
becomes a typed quota boundary and the pending transfer is dropped with it - see
[[terminal-images#Terminal Images#Mandatory Decode Scheduling]].

Repetition is safe by construction. A retired session has no framing state and no
pending transfer, so a second reset or close produces an empty commit and touches
no counter, which is what keeps a double close from underflowing the ledger.

## Transactional Image Mutations

Canonical definitions and placements change all-or-nothing: a read either commits every mutation it implies or leaves the prior state, ownership, and counters exactly as they were.

Framing runs before the terminal does, so a completed boundary cannot know its
own cursor yet. Each boundary therefore carries payload-free decoded canonical
facts, and a second phase replays the read's grid effects and image boundaries
in original byte order against a clone of canonical state. The clone is swapped
in only after the whole read succeeds. Out-of-band spans — resize and
synchronized-update flush — commit through the same boundary.

Ordered mutations are reserved from the paired session/process ledger before
they are retained, because one grid effect can republish every live placement.
The definition and placement maps themselves stay inside the frozen
`max_images_per_session` and `max_placements_per_session` ceilings.

A compound transmit-and-display validates both halves before committing either,
so an invalid source rectangle or destination extent leaves no definition
behind. Placements are keyed by screen plus the complete protocol identity
`(image_id, placement_id)`. Server-assigned identifiers for unnamed images
start above the Kitty `i=` range and can never collide with an application's
choice. Reaching a session ceiling evicts the oldest committed entry by a
monotonic tick, not by identifier value, and publishes that removal before the
mutation that displaced it.

Delete operands keep their own presence. An omitted `i=`, `p=`, `x=`, `y=`, or
`z=` matches everything in scope; an explicit `0` stays a literal value.
Identity and placement scopes reach both screens, matching Kitty's image-global
delete semantics, while every geometric scope stays on the active screen. Cell
operands are converted from Kitty's 1-based coordinates to canonical 0-based
anchors, and the uppercase selector polarity is what frees canonical data.

Protocol lifecycles stay distinct. ED2, a hard reset, and alternate-screen
creation clear visible graphics; every other text erase leaves Kitty placements
alone and removes only the Sixel placements sharing those cells. Erase
rectangles, scroll margins, and resize viewports are half-open on every edge,
matching the observer that produced them, and the client applies the same
shared placement geometry so both sides converge.

## Retained Canonical Pixels

A committed definition keeps the pixels that created it, because a scene the server can describe but not re-send is a scene no viewer and no successor can ever be given.

[[crates/scribe-server/src/terminal_image_state.rs#SessionTerminal#retain_committed_rgba]]
runs inside the same transaction that commits a read's mutations. The decoded
bytes are *shared*, not copied: the decode slot and the retention map hold the
same `Arc<DecodeBuffer>`, so the bytes keep the one lease the decode already
paid for and retention costs the session nothing it had not already been
charged. The lease is released when the last owner drops it, which is what
makes eviction, delete, erase, reset, and the master switch free the pixels
without a second accounting path.

Pairing is per image, not per read. A read can transmit several images in one
uninterrupted write, so the decode slots — which only ever hold the newest
decode of each protocol — cannot say which bytes belong to which definition.
Every decode a read produces is therefore kept under the output sequence of the
image boundary that produced it, and
[[crates/scribe-server/src/terminal_image_state.rs#apply_image_boundary]] reads
the definitions each boundary created straight off the mutation log, so a
boundary that defines nothing claims no pixels. Output sequences are monotonic
and never reused, which is what keeps an earlier read's bytes out of a later
read's definition. An exact canonical-length check still guards the attachment,
so a definition is left unbacked rather than backed by another image's bytes —
unbacked is withdrawn wherever a scene is stated, which is recoverable, while
mismatched would be a silently wrong picture. Once paired, the read's decodes
are released and every commit drops the pixels of images canonical state no
longer holds.

[[crates/scribe-server/src/terminal_image_state.rs#SessionTerminal#canonical_rgba]]
is what the definition-payload seam reads, so every place a scene is stated —
the live burst one committed read publishes, the replay burst a late or shed
sink is given, and the handoff export — states a real scene instead of
withdrawing every definition in it. A restored scene is retained the same way,
which is what lets a session survive a second upgrade.

The store is what backs the seam by default rather than what the caller has to
remember to pass:
[[crates/scribe-server/src/terminal_image_state.rs#SessionTerminal#publish_committed]]
consults the caller's provider first and falls through to the retained pixels
when it declines, so the production commit path supplies no payload at all and a
caller stating a scene from bytes the session never decoded can still override
it.

## Client Convergence and Counter Safety

Canonical server state and the connected client's scene stay identical because the server publishes the exact deltas one committed read produced, and refuses to publish at all once a counter is exhausted.

Publication is the only place the two models meet. One committed mutation log
becomes generation- and sequence-tagged live records: a definition plus its
bounded canonical chunks, a placement, an exact placement removal, a freed
image, or a typed rejection. The seam owns no image payload, so the caller that
owns decoded pixels supplies the bytes for each published definition.

Every placement record names its owning screen, so resize clipping, eviction,
and alternate-screen creation cannot land in the wrong client bucket. The field
is omitted at its legacy default — whichever screen is active — and existing
encoded records keep their exact bytes. A screen switch is published last, once
every screen-scoped record has landed. Because a completed definition replaces
the client's image data and drops the placements bound to the previous bytes,
the burst that redefined an image ends by restating the placements the server
still holds.

A hard reset invalidates the whole scene and therefore opens the next
generation. The client binds every record in one burst to a single generation,
so a read that resets and then defines publishes two bursts, each with its own
sequence. Clients reject a lower generation or a repeated sequence, which makes
a duplicated, delayed, or reordered burst inert.

Generation and sequence headroom are checked before anything mutates. An
exhausted counter returns `GenerationExhausted` or `SequenceExhausted` and
leaves the last committed definitions, placements, screen, generation, and
published scene exactly as they were. Nothing partial is emitted, and no
exhausted counter is reused.

## Authoritative Image State Assembly

The independently verified image invariants compose into one server-owned engine whose combined behavior is certified by a versioned payload-free evidence manifest.

Assembly adds no engine. Every session is one
[[crates/scribe-server/src/terminal_image_state.rs#PtyTerminalImageState]] over
the shared immutable process policy, so framing order, storage reservation,
decode admission, transfer retirement, Alacritty-derived observation,
transactional commit, and client publication are the same code paths the child
invariants certified. Sessions are independent in canonical state and decode
identity while sharing one process storage ledger and one decode scheduler, so
neither session can spend the other's image state or bypass the process
ceilings that bound them both.

The manifest is the objective artifact that closes the epic and that downstream
live-fanout work reads. It is versioned by `schema_version`, names the engine it
came from, and carries the frozen `ImageLimits`, exact per-session and process
storage counters, scheduler admission counters, typed outcomes for every
rejection and retirement the scenario produced, and a canonical convergence
digest pair per session. It records no image payload: definitions and
placements are metadata only, and the digests are folded from that metadata
rather than from pixels.

Every specification acceptance criterion maps to the assembly case that
exercised it and to the child functional gate that certifies it independently,
so a reviewer can trace any criterion to evidence without rerunning the epic.

## Session Capability Latch

Image capability is server session state latched once by a capable viewer, so viewer count, detach, controller changes, and reattach cannot change what a running application was already told.

[[crates/scribe-server/src/terminal_image_sharing.rs#SessionImageSharing]] holds
the latched subset and the master switch. A session begins
`text-only-unlatched` and admits anyone. The first capable viewer latches the
intersection of its advertised renderer capability with Scribe's compile-time v1
subset; a later viewer joins that latch instead of widening or narrowing it, so
the advertised subset is stable for the life of the session. A latched session
keeps parsing, replying, and retaining bounded state with zero viewers.

Disabling the master switch is the only thing that clears a latch. The
transition is reported once as `disabled_cleared_latch`, a repeated write is
`unchanged`, and re-enabling never restores the old latch — a capable viewer
must claim again. While disabled the session advertises nothing, so discovery
cannot mistake a policy-disabled Scribe for an enabled implementation.

## Incapable Viewer Refusal

A viewer that cannot render what a session latched is refused for that session with a typed mismatch rather than attached to a screen whose graphics it would silently drop.

[[crates/scribe-server/src/ipc_server.rs#admit_image_capable_sessions]] runs
between the window-ownership filter and the attach itself. For each requested
session it either latches an unlatched session to the attaching viewer or, when
the viewer does not support the latched subset, drops that session from the
batch and answers `TerminalImageCapabilityMismatch` naming the required and
offered capability. The rest of the batch still attaches, and nothing here
clears a latch, so a refusal cannot be used to downgrade a session.

## Exactly-Once PTY Replies

The server owns every image protocol reply and writes it to the originating PTY once, in PTY byte order, ahead of the terminal's own replies for the same read.

[[crates/scribe-server/src/terminal_image_sharing.rs#plan_pty_replies]] turns one
committed read into the ordered replies it owes: an APC `G` result echoing `i`
and `p` for each completed Kitty command, and a stable error code for each Kitty
failure. A continuation chunk replies only when its final chunk lands, and a
session whose capability is not live owes nothing at all. The planner is pure,
so replanning the same commit — what a reattach or a replay would do — cannot
add a second reply.

Both reply kinds honor the command's own quiet level. `q=1` suppresses success
only, so an error is still answered; `q=2` suppresses the failure reply as
well. Every failure therefore carries the level it must honor:
[[crates/scribe-pty/src/graphics_framing.rs#GraphicsFailure]] reads it from the
controls of a body that framed to its terminator, and the seam propagates the
transfer's first-chunk level to decode-time and truncation failures, because a
continuation chunk may omit every control.

A failure whose quiet level was never readable — malformed framing, a string
that never terminated, a `q=` operand that is not a defined level — carries `0`
and still replies. Silence has to be requested in a sequence we could actually
read, otherwise a hostile stream could mute its own diagnostics by malforming
them.

[[crates/scribe-server/src/ipc_server.rs#deliver_image_commit]] is the single
production caller. It runs immediately after the reader's ingress seam and
before `process_metadata_events` drains the terminal's own event queue, which is
what puts a Kitty result ahead of the DA1 reply an application requests directly
behind its capability probe. Clients never synthesize a reply through
`KeyInput`.

## Sixel DA1 Advertisement

Sixel discovery is attribute `4` appended to the terminal's own primary device-attributes reply, exactly once and only while the capability is live.

[[crates/scribe-server/src/terminal_image_sharing.rs#augment_device_attributes]]
rewrites the `PtyWrite` event the authoritative `Term` raised for DA1, preserving
every attribute the terminal already reported and skipping a reply that already
carries `4`. Any other terminal reply — the secondary DA, DSR, DECRPM — passes
through untouched, and a disabled or unlatched session gains nothing, so
discovery stays truthful in both directions. `$TERM` and `TERM_PROGRAM` are
never spoofed.

## Capable-Sink Image Fanout

Typed image records reach the attached sinks that can render them and no others, so one incapable connection cannot suppress or corrupt a capable viewer's convergence.

[[crates/scribe-server/src/ipc_server.rs#AttachedSinks#fan_out_images]] walks the
per-session sink set and skips any sink whose `Hello` capability does not support
the session's latched subset. Each connection's advertised capability lives on
its lock-free output-queue handle rather than behind the connection mutex,
because the fan-out runs inside the per-session sink lock where nothing may
await.

A live record is a session-scoped droppable frame, exactly like the raw
`PtyOutput` it accompanies: a saturated link sheds both together and the sink
owes a fresh combined replay, which is cheaper and more truthful than growing
the queue until the connection dies. A sink that owes a replay receives no live
delta at all, because an increment applied to an unknown scene is what produces
a divergent viewer.

The returned count is the viewer count the zero/one/multiple-viewer contract is
written against. Zero viewers is a normal outcome, not an error: the session
still commits canonical state and still answers the PTY. A shared-mode join adds
a sink, a `SingleController` re-point replaces the set, and a disconnect removes
one — none of which touches the latch.

## Image Master Switch

Terminal graphics ship on and turn off from one place, so a bad decoder, a hostile stream, or a GPU problem is a settings change rather than a downgrade.

[[crates/scribe-common/src/config.rs#TerminalImagesConfig]] holds the single
`terminal.images.enabled` boolean, defaulted on so an existing config keeps
today's behavior. The server mirrors it into the process-wide switch at startup
and on every `ConfigReloaded`, and
[[crates/scribe-server/src/ipc_server.rs#apply_image_master_switch]] writes the
same value into every live session's latch. Disabling therefore stops
advertising, PTY replies, and fan-out at once, including for a session nobody is
watching. Re-enabling restores nothing: a capable viewer must latch again, so an
application is never told images came back without a renderer behind them.

Retained bytes belong to each session's PTY reader, which owns the image seam
alone. [[crates/scribe-server/src/ipc_server.rs#release_images_if_disabled]]
runs on the reader when the reload broadcast wakes it and calls
[[crates/scribe-server/src/terminal_image_state.rs#SessionTerminal#release_for_policy_disable]],
which retires the session exactly as a close does — decode admissions
cancelled, partial framing discarded, retained buffers dropped — and then
performs the same canonical reset a hard terminal reset performs, so no
committed scene survives to be replayed to a later viewer. The seam skips a
session holding nothing, so the reload costs one predicate per text-only
session, and a second release is a no-op.

Text is never part of this. The release opens a retirement boundary whose raw
outputs are the same bytes the terminal already showed, and the grid, its
scrollback, and the application's own textual fallback are untouched.

### Rollback procedure

Turning terminal images off is a live configuration change with no restart, no downgrade, and no package action, which is what makes it the first response to a decoder, stream, or GPU incident.

Set `enabled = false` under `[terminal.images]` in
`~/.config/scribe/config.toml`, either by editing the file or by clearing the
"Terminal images" toggle in the settings editor. The config file watcher raises
`ConfigReloaded`, the server mirrors the new value into the process switch and
into every live session's latch, and each PTY reader releases its retained image
state on its next wake. Confirm from any pane: a Kitty capability probe goes
unanswered, and a DA1 reply no longer carries attribute 4.

Rolling back never touches text. Scrollback, the grid, and the application's own
textual fallback survive, and a hot upgrade taken while the switch is off
declares the pre-image handoff version so an older server accepts the payload.
Setting the key back to `true` restores advertising for sessions that latch
again; it restores no scene the disable released.

## Localized Image Diagnostics

Scribe's own image messages come from one static catalog keyed by the frozen rejection taxonomy, so a diagnostic cannot carry a byte an application produced.

[[crates/scribe-common/src/terminal_images.rs#TerminalImageRejectionReason#localized_message]]
maps each category to one `&'static str`. There is no interpolation and no
placeholder, which is what makes the payload-free guarantee structural rather
than a review rule: there is nowhere for pixels, base64, identifiers, or paths
to go. Translating Scribe means translating exactly this table.

A pane reads its notice through
[[crates/scribe-client/src/terminal_image_scene.rs#CommittedImageScene#diagnostic_notice]],
which renders the scene's last typed rejection. The notice is additive: it never
replaces the application's textual fallback, and the published definitions and
placements beside it are unchanged.

Suppression is structural, not temporal. The scene holds exactly one
`last_rejection` slot, so a storm of rejections overwrites that slot and still
renders one notice. There is no timer, cooldown, or burst counter to tune, and
no rejection is ever queued or replayed as a backlog of notices. The notice
names a category and nothing else: it is the string to quote in a bug report,
and it exposes no image id, no dimensions, no path, and no application bytes.

## Renderer Failure Cleanup

A failed GPU operation releases that session's sources once and marks the view unavailable, instead of retrying an upload that cannot work on every frame.

[[crates/scribe-client/src/gpui_image_lifecycle.rs#GpuiImageError#is_renderer_failure]]
separates the two kinds of failure. A bounded rejection — a view limit, a bad
crop, an inconsistent definition — is Scribe refusing one image and says nothing
about the GPU path, so the rest of the scene keeps painting.
[[crates/scribe-client/src/gpui_image_lifecycle.rs#GpuiImageError#rejection_reason]]
gives each failure its payload-free diagnostic category, `renderer_unavailable`
for the window operations and a bounded category otherwise.

Only a window failure reaches
[[crates/scribe-client/src/gpui_image_lifecycle.rs#GpuiImageCache#note_renderer_failure]],
which drops the session's cached sources and latches the unavailable flag until
a later source builds successfully. Glyph painting is a separate pass, so the
pane keeps its text through the failure and through the cleanup.

## Combined Image Replay

A viewer with no knowable scene is caught up by one bounded generation-tagged burst rather than by incremental deltas, so a late attach and a shed backlog share one recovery.

[[crates/scribe-server/src/terminal_image_replay.rs#plan_replay]] turns a
canonical snapshot into `Begin`, every surviving definition followed by its RGBA
split into wire-sized chunks, every surviving placement tagged with its owning
screen, and `Commit`. Every record carries the same generation and the
snapshot's output cursor, so a receiver stages the whole burst and swaps at
`Commit`; a partial scene is never observable. An empty scene is still a
truthful two-record burst, which is what converges a viewer holding stale
placements.

Chunks are capped at the frozen `max_replay_chunk_bytes`, so the largest scene
v1 admits — 128 MiB of canonical RGBA — becomes 128 bounded records rather than
one oversized frame. Records are charged their real payload size in the output
queue, because a flat nominal charge would let a large scene outgrow the queue's
byte ceiling without the ceiling noticing.

Canonical pixels arrive through the same payload seam the live publication path
uses, backed by [[terminal-images#Terminal Images#Retained Canonical Pixels]].
A definition the session could not retain is withdrawn together with every
placement naming it: an unbacked definition would leave the receiver staging a
scene it can never complete.

### Replay debt

A sink owes a replay in exactly two situations: it just attached, or its queued
output was shed.

Either way what the viewer has seen no longer describes the session.
[[crates/scribe-server/src/ipc_server.rs#AttachedSinks#fan_out_images]] detects
the shed case at the moment the queue refuses a frame, which is synchronous and
race-free, and stops sending deltas to that sink.

Each way of accruing debt has a drain that fires when it happens.
[[crates/scribe-server/src/ipc_server.rs#drain_image_replay_debt]] pays a fresh
sink at the end of its attach, from canonical state and with no PTY read to ride
on, because an application that has gone quiet would otherwise leave that viewer
in front of an imageless pane indefinitely; the committed-read path drains the
shed case on the session's next commit. An unlatched or disabled session skips
the drain entirely, so a scene the master switch retired never reaches a viewer
that joins afterwards.

[[crates/scribe-server/src/ipc_server.rs#AttachedSinks#fan_out_image_replay]]
delivers one planned burst to every sink that owes one and clears their debt
together. Replay records are non-droppable: the burst *is* the recovery, so
shedding it under the policy that triggered the recovery would loop. The plan is
built once from canonical state however many sinks receive it, so the server
never retains a per-sink copy of the scene and recovery cost does not grow with
viewer count.

## Staged Client Image Replay

A viewer assembles a whole snapshot off-screen and swaps its published scene once, at the burst's commit, so no frame can ever show half a replay.

[[crates/scribe-client/src/terminal_image_scene.rs#LiveImageScene#apply_replay]]
builds an empty scene rather than cloning the published one — a replay is a
whole snapshot, not a delta — and runs every record through the same quota,
contiguity, placement, and screen-ownership checks a live burst uses. The
published `Arc` is replaced only by `Commit`, and only after the burst carried
exactly the definitions, placements, and canonical bytes its `Begin` declared.

Any failure abandons the whole snapshot and leaves the published scene exactly
as it was. That is deliberate: the pane keeps showing the last scene it could
prove, and the server's existing replay-debt path is what corrects it. A
half-applied snapshot would instead be a wrong picture the client believes.

### Live records behind a snapshot

Live records that arrive while a snapshot stages are buffered in arrival order
and applied after the swap, never before it.

A delta applied to a scene the client is about to replace is meaningless, and
one applied afterwards must still land in the order the server emitted it. Both
streams share the client's ordered pane FIFO, so "later" is a defined property
rather than a race. A buffered record whose generation or output cursor the
snapshot already covers is dropped at drain: replaying it would resurrect
definitions and placements the snapshot deliberately replaced.

The buffer is bounded by
[[crates/scribe-client/src/terminal_image_scene.rs#MAX_BUFFERED_LIVE_RECORDS]]
records and by the session's retained-CPU ceiling. The server suppresses live
deltas to a sink that owes a replay, so in practice the buffer only absorbs the
boundary between the two streams; overflow abandons the staged snapshot instead
of growing without bound or applying part of a stream it can no longer order.

## Image State Across Handoff

A zero-downtime upgrade pauses PTY reads mid-stream, so the successor inherits three things or the application notices: the committed scene, the prefix of a control string that has not terminated, and any chunked transfer still accumulating.

[[crates/scribe-server/src/terminal_image_state.rs#SessionTerminal#export_handoff]]
captures all three. The scene travels as the same bounded burst
[[crates/scribe-server/src/terminal_image_replay.rs#plan_replay|the replay
planner]] builds for a late attacher, so there is exactly one way to stage a
scene rather than a second handoff-only format, and the max-scene chunk ceiling
is inherited for free. Reads are already paused when this runs, so no decode is
in flight: quiescence is the pause, not a separate barrier.

[[crates/scribe-server/src/terminal_image_state.rs#SessionTerminal#restore_handoff]]
validates and reassembles the whole burst before any field on the session
moves. A burst whose `Begin` counts disagree with its records, whose chunks are
non-contiguous, or that places an image it never carried is refused outright —
the successor keeps an empty session rather than a half-restored one, because a
partial scene is worse than no scene.

### Paused framing

[[crates/scribe-pty/src/graphics_framing.rs#GraphicsFramer#export_partial]]
captures the framer's stream cursor and whichever control string it held;
[[crates/scribe-pty/src/graphics_framing.rs#GraphicsFramer#restore_partial]]
reinstates both on the successor, re-reserving the retained prefix through its
own storage budget so the ledger accounts for it exactly as the sender's did.

Structured state travels rather than replayed bytes because the raw prefix is
not recoverable: promoting a candidate to an active string consumes the
introducer, keeping only the parsed kind and a control-byte count. A successor
that started in Ground would print the remainder of a half-sent APC or DCS as
visible text.

Chunk accumulation has the opposite problem — it spans many *complete* commands,
so no raw prefix survives at all. [[crates/scribe-common/src/kitty_decode.rs#KittyTransfer#export]]
therefore carries the normalized bytes plus the counters that bound what may
still follow, and never the base64 text those chunks arrived as.

### Live upgrade wiring

The canonical scene is mutated only by the PTY reader, but the handoff runs on the shutdown path with every reader parked on a `read()` that will never answer, so the seam hangs off the registry entry instead of the reader task.

[[crates/scribe-server/src/ipc_server.rs#SessionImageState]] is that shared
handle: one tokio-mutexed seam owned by the reader and reachable from
`LiveSession`. [[crates/scribe-server/src/ipc_server.rs#serialize_live_for_handoff]]
opens one [[crates/scribe-server/src/terminal_image_handoff.rs#HandoffImageExport]]
per payload and exports every session through it, so the shared image-byte
ceiling is charged across the whole payload rather than per session. Reads are
already paused when this runs, so the lock is uncontended and the scene it
reads is whatever the last read committed.

Restore happens before the successor's reader consumes a byte:
[[crates/scribe-server/src/session_manager.rs#ManagedSession]] carries the
exported state through
[[crates/scribe-server/src/session_manager.rs#restored_managed_session]], and
[[crates/scribe-server/src/ipc_server.rs#new_session_image_seam]] stages it onto
the fresh seam. A refused payload is logged and dropped — the seam it was
refused on is still empty, so the session starts imageless rather than
half-restored, which is the same bounded degradation an over-ceiling export
produces.

### Payload ceiling

`HandoffState` is capped at 256 MiB for every session's text replay put
together, while one session may legitimately retain 128 MiB of canonical
pixels. [[crates/scribe-server/src/terminal_image_handoff.rs#HandoffImageExport]]
charges each session's scene against
[[crates/scribe-server/src/terminal_image_handoff.rs#MAX_HANDOFF_IMAGE_BYTES|a
128 MiB image ceiling]] shared across the payload.

A scene that does not fit is exported *empty*, never truncated, and the session
still carries its generation, output cursor, and framing. That session shows no
images after the upgrade while its text is untouched — a bounded, visible
degradation instead of a scene the successor can never complete.

### Version gating and rollback

Image state is an additive `#[serde(default)]` field, so old-to-new is free: a
payload from a server that predates it restores as an empty scene.

New-to-old is not free, because a v6 server would ignore the field and land
every session's text with its images silently gone.

So [[crates/scribe-server/src/handoff.rs#handoff_state_version]] declares v7
exactly when the payload carries image state, and
[[crates/scribe-server/src/handoff.rs#handoff_version_accepted]] refuses N+1 —
an older server cold-restarts instead of dropping images. An image-free payload
declares v6 and omits the key entirely, so its bytes are the bytes a pre-image
server produced. Turning the master image switch off is therefore the rollback
path: the next upgrade payload is v6 again and an older server accepts it.

## Typed Failures

Every rejection has a stable category and payload-free metadata suitable for diagnostics without leaking PTY image content.

Categories are exactly `policy_disabled`, `unsupported_protocol`,
`unsupported_action`, `unsupported_transport`, `malformed_framing`,
`malformed_control`, `malformed_payload`, `truncated_sequence`,
`chunk_mismatch`, `invalid_dimensions`, `quota_exceeded`,
`work_budget_exceeded`, `decode_deadline_exceeded`, `decode_cancelled`,
`decode_failed`, `image_not_found`, `capability_mismatch`,
`renderer_unavailable`, and `evicted`.

Kitty maps unsupported/malformed to `ENOSYS`/`EINVAL`, bounds to
`E2BIG`/`ENOSPC`, deadlines/cancellation to `ETIMEDOUT`/`ECANCELED`, decode to
`EIO`, and missing image to `ENOENT`, subject to quiet mode. Diagnostics carry
only protocol/action, safe dimensions/counts, and limit name; applications own
text fallback.

## Compatibility Corpus

Pinned applications and owned protocol fixtures prevent release claims from drifting with package repositories or external test suites.

Yazi is `v26.5.6` at
`aa526434f00bb44e2e902d9a4ac5f810da1018b9`: it probes an unknown terminal with a
generic Kitty query, which Scribe answers, and then draws through Sixel because
Scribe also advertises Sixel in DA1 and this release prefers it when both are
offered. No terminal spoofing is involved either way. Chafa
is `1.18.2`, exercised with `--format kitty --probe off` and
`--format sixels --probe off`; its Kitty form is the corpus's real-application
classic placement. gnuplot is `6.0.3`, exercised through
`set terminal sixelgd`. Each application runs through direct PTY and SSH.

Ten owned ASCII-hex fixtures cover Kitty query ordering, RGB, chunked zlib
RGBA, PNG, Unicode placeholders, deletion; 7-bit and C1 Sixel; xterm mode/text
chronology; and CAN/SUB malformed recovery. `fixtures.tsv` and `contract.json`
freeze every path and expected outcome.

## Pinned Application Corpus

Real released applications are run inside a real pane so protocol choice is observed rather than assumed, and one server evidence line makes that choice assertable.

The corpus is built into the visual image from checksum-pinned upstream
artifacts, never from a distribution package: Debian ships older Chafa and
gnuplot builds, and no Yazi at all. A capable viewer must latch a session
before any of it works, so the harness announces the renderer subset with
`SCRIBE_TERMINAL_IMAGES=1`
([[crates/scribe-client/src/main.rs#advertised_terminal_image_capabilities]]);
a session created by that client latches at creation, because a created session
is attached by its creator and never through `AttachSessions`.

Each committed read whose observed boundaries or live placements changed emits
one summary naming cumulative PTY replies, Kitty commands, completed Kitty
transfers, decoded Sixel images and typed failures, plus the live canonical
placement count per kind. Canonical pixels do not reach a live viewer yet, so
this line — not a rendered frame — is what proves an application's bytes became
canonical image state.

Two defects the corpus surfaced are fixed at their root: a Kitty transfer opened
by a data-less control command (Chafa's shape, and legal to Kitty) no longer
fails its first chunk, and CR/LF inside a Sixel payload (gnuplot's `sixelgd`
shape, ignored by DEC and xterm) no longer fails validation.

## Evidence Index

Every corpus writes reviewable JSON under `test-output/terminal-images/`, so a release review reads one directory instead of re-running the harness.

The Docker corpora land in the top level of that directory. Each gate below runs
as `just e2e-func <name>` after `just docker-func`, or as `just e2e-visual
<name>` for the `visual/` entries after `just docker-visual`. Both images need
`just build-release` first, and `test-output/` is written by the container as
root.

| Gate | Evidence |
| --- | --- |
| `terminal-image-contract.sh` | `contract.json` |
| `terminal-image-framing.sh` | `framing.json` |
| `terminal-image-ipc.sh` | `ipc.json` |
| `terminal-image-sixel-decoder.sh` | `sixel-decoder-evidence.json` |
| `terminal-image-kitty-decode.sh` | `kitty-decode-evidence.json` |
| `functional/terminal-image-decode-spike.sh` | `decode-spike-evidence.json` |
| `terminal-image-state-seam.sh` | `state-seam.json`, `state-seam-ipc.json` |
| `terminal-image-accounting.sh` | `accounting.json` |
| `terminal-image-scheduler.sh` | `scheduler.json` |
| `terminal-image-transfer-lifecycle.sh` | `transfer-lifecycle.json` |
| `terminal-image-mutations.sh` | `mutations.json` |
| `terminal-image-convergence.sh` | `convergence.json` |
| `terminal-image-observer-parity.sh` | `observer-parity.json` |
| `terminal-image-server-state.sh` | `server-state-manifest.json` |
| `terminal-image-replay.sh` | `replay.json` |
| `terminal-image-replies-sharing.sh` | `replies-sharing.json` |
| `terminal-image-handoff.sh` | `handoff.json` |
| `terminal-image-client-scene.sh` | `client-scene.json` |
| `terminal-image-client-replay.sh` | `client-replay.json` |
| `terminal-image-settings.sh` | `settings.json`, `settings-run.log` |
| `terminal-images-functional.sh` | `functional.json` and its probe transcripts |
| `terminal-images-performance.sh` | `performance.json` |
| `visual/terminal-image-gpui-spike.sh` | `linux/gpui-spike.json` |
| `visual/terminal-image-renderer.sh` | `linux/renderer/renderer.json` and captures |
| `visual/terminal-image-apps.sh` | `linux/apps/apps.json` |
| `visual/terminal-images-visual.sh` | `linux/client/client.json` |
| `visual/terminal-images-frame-stability.sh` | `linux/client/frame-stability.json` |

`server-state-manifest.json` is not independent: it refuses to write unless the
state-seam, accounting, scheduler, transfer-lifecycle, observer-parity,
mutations, and convergence evidence is already green in the same directory.

The native macOS corpus writes `macos/` in the same directory and is the only
evidence that does not come from Docker; it is produced on a hosted runner and
retrieved as a workflow artifact rather than run locally.

## Contract Verification

Docker verification proves the frozen limits, matrix markers, app pins, and owned fixture inventory and emits reviewable contract evidence.

`tests/e2e/terminal-image-contract.sh` runs only under `SCRIBE_E2E_SANDBOX`,
checks exact security values and fixture ownership/hex integrity, then copies
the canonical JSON unchanged to
`test-output/terminal-images/contract.json`. Invoke it with
`just e2e-func terminal-image-contract.sh` after building the functional image.

## Framing Verification

Docker verification proves framing is invariant across PTY read boundaries and that recovery never consumes adjacent terminal text.

`tests/e2e/terminal-image-framing.sh` runs the production `scribe-pty` framer
through `scribe-test image-framing`. Every owned fixture is tried whole, at
every two-chunk byte split, and one byte per read. Every feed verifies the
fixture's complete parsed command expectation and contiguous raw-range tiling.
For every forwarded event, it also proves range length equals byte length and
the bytes equal the corresponding source-input slice; suppressed commands and
failures retain ranges without terminal bytes.
Adversarial cases cover both Sixel forms, split and overlapping ESC/ST and C1
controls, candidate resynchronization, CAN/SUB, malformed and unsupported
controls, fourth-field/overflow classification before a later body quota,
malformed-header recovery, mixed terminators, EOF truncation,
exact/over-budget strings, Sixel numeric-header max-plus-one discard, first-error
preservation across CAN/SUB, Kitty chunk limits, and bounded non-image DCS
passthrough. Boundary and cancellation cases cover every split, both Sixel
forms, exact failure ranges, adjacent text, and absence of raw payload leakage.
After every assertion passes, the probe atomically publishes schema-versioned,
payload-free audit evidence to `test-output/terminal-images/framing.json`, with
owned-fixture semantic/tiling counts and stable markers for each adversarial
case family.

## Bounded Decode Decision

Production decoding is a conditional go through narrow decoder-only forks and Scribe-owned limits; generic or stock whole-image entry points are outside the trust boundary.

The decode spike selects `flate2 1.1.9` low-level `Decompress` behind a
4,096-work-unit Scribe loop. The loop charges input and output, checks the
monotonic deadline and cancellation, rejects projected inflation, and grows
output only through fallible allocation.

Sixel vendors only `icy_sixel 0.5.0` decoder source and adds caller-owned
dimension, pixel, allocation, work, deadline, and cancellation limits. Its
encoder, `quantette`, and unaudited SIMD span paths are excluded. PNG vendors
only `png 0.18.1` decoder core and adds the same hook at compressed-input,
inflated-output, unfilter, and pixel-conversion boundaries; encoder, APNG, and
ancillary text/profile paths are excluded.

Generic `image 0.25.10`, stock `png`, stock `icy_sixel`, and C decoders are
no-go. The first two expose no cooperative hook at the frozen interval and
their allocation controls cannot alone enforce every Scribe trust budget.
`specs/020-terminal-images/decoder-decision.md` records source evidence, fork
ownership, evidence schema, and remaining production work.

## Bounded Sixel Decoder

The vendored decoder makes Sixel rasterization interruptible and fallible at every untrusted growth boundary without exposing partial images.

[[third_party/icy-sixel-decoder/src/lib.rs#decode_sixel]] vendors the decoder
concepts from `icy_sixel 0.5.0`, crates.io SHA-256
`85518b9086bf01117761b90e7691c0ef3236fa8adfb1fb44dd248fe5f87215d5`,
at upstream revision `998cbb2c6d8ed5272f9cc4702a4660778972bf3f`.
`README.md`, `LICENSE-MIT`, and `LICENSE-APACHE` in that directory retain the
source URL, exact license texts, fork delta, excluded code, and Scribe
maintainer ownership for CVE and upstream-release review.

[[crates/scribe-image-decode/src/lib.rs#DecodeBudget|DecodeBudget]] carries the
shared frozen 4096-axis, 16,777,216-pixel, 67,108,864-byte
RGBA, 134,217,728-work-unit, absolute monotonic deadline, and 4096-unit check
interval boundaries. Caller hooks provide cooperative cancellation and an
allocation-accounting veto. Every dimension/add/multiply and canvas offset is
checked; each canvas allocation uses `try_reserve_exact` before mutation.

Input examinations, emitted RGBA bytes, and pixel writes charge cumulative
work. Cancellation and deadline checks happen at entry, at most every 4096
charged units, and before returning. Failure drops the private canvas and
returns a payload-free typed category, so no stale or partial result can
escape. Complete 7-bit/C1 DCS and already-framed payload entry points share
the same budget implementation.

The fork preserves raster attributes, repeat, `$`/`-`, 256 private palette
registers, RGB/HLS definitions, least-significant-bit sixel rows, and opaque
`P2=0/2` versus transparent `P2=1` backgrounds. Encoder and quantizer source,
`quantette`, unsafe/SIMD fills, full terminal parsers such as termwiz, and C
Sixel libraries are absent. `DEPENDENCY-TREE.txt` records the shared
Scribe-owned budget dependency and its `scribe-test` reverse edge.

## Bounded Sixel Decoder Verification

Docker verification exercises owned fixtures and adversarial allocation, arithmetic, work, cancellation, deadline, palette, raster, repeat, and malformed-input boundaries.

`tests/e2e/terminal-image-sixel-decoder.sh` invokes the production vendored
crate through the `scribe-test sixel-decoder` command. It covers 7-bit and C1
owned fixtures, background/palette/repeat/raster semantics, exact and
max-plus-one dimensions and growth, palette 255/256, deterministic allocation
denial, immediate and interval cancellation, expired deadline, truncated and
overflowed controls, plus exact and max-plus-one work accounting.

The run atomically writes
`test-output/terminal-images/sixel-decoder-evidence.json` with the contract
version, source revision/checksum/licenses, explicit exclusions, frozen limits,
typed outcomes, and an `all_passed` aggregate. Invoke it only through
`just e2e-func terminal-image-sixel-decoder.sh` after rebuilding the functional
Docker image.

## Shared Decode Budget

Kitty and Sixel charge one caller-owned cooperative budget, preventing either decoder from weakening work, cancellation, deadline, or allocation policy.

[[crates/scribe-image-decode/src/lib.rs#DecodeBudget]] owns cumulative work,
the next 4096-unit observation boundary, an absolute monotonic deadline,
caller cancellation and allocation hooks, and peak live-allocation evidence. It
is built from a scheduler permit rather than a raw storage handle, so the budget
also observes scheduler cancellation at every check - see
[[terminal-images#Terminal Images#Mandatory Decode Scheduling]].
Checked work overflow, hook denial, cancellation, and deadline expiry return
typed payload-free errors. The Sixel and PNG forks use this exact type rather
than independently approximating the frozen interval.

## Bounded Kitty Normalization

Kitty direct payloads become canonical RGBA only after exact chunk, base64, transport, compression, dimension, and format validation.

[[crates/scribe-common/src/kitty_decode.rs#KittyTransfer]] rejects every
indirect transport before allocation, caps each 4096-byte chunk and cumulative
chunk/encoded/decoded counts, requires exact RFC 4648 padding boundaries, and
drops encoded bytes after each chunk. Optional RFC 1950 data uses low-level
`flate2 1.1.9` with 4096-byte caller storage, counter-derived input/output work,
projected-output checks, fallible growth, and no trailing compressed bytes.

Raw `f=24` and `f=32` require exact checked `s*v*channels` length before the
canonical allocation; RGB conversion writes opaque alpha. `f=100` accepts only
a PNG signature and delegates to the bounded decoder fork. Completed results
retain width, height, canonical RGBA, alpha metadata, and safe counts only;
base64, compressed data, paths, URLs, resource names, and partial results never
escape normalization.

## Bounded Kitty PNG Decoder

The decoder-only PNG fork retains static PNG semantics while exposing every untrusted allocation and work boundary to Scribe.

[[third_party/image-png-decoder/src/lib.rs#decode_png]] is pinned to `png
0.18.1`, crates.io SHA-256
`60769b8b31b2a9f263dae2776c37b1b28ae246943cf719eb6946a1db05128a61`,
at upstream revision `2a3f980245e3ae38b82ade96533e7b450e8477bb`.
The adjacent README, exact MIT/Apache licenses, and dependency tree record
provenance, fork delta, exclusions, pure-Rust dependencies, and Scribe CVE and
upstream-update ownership.

The fork validates signature, chunk order/length/CRC, dimensions, color/depth,
IDAT completion, filters, transparency, and Adam7 with checked arithmetic. It
supports every legal static PNG color/depth combination and converts to RGBA.
Unknown ancillary chunks are CRC-checked then skipped without allocation;
unknown critical chunks, APNG, text/profile retention, encoders, generic image
selection, and indirect resource loaders are excluded.

## Bounded Kitty Decoder Verification

Docker verification covers supported normalization plus adversarial payload, resource, allocation, work, deadline, and decompression boundaries.

`tests/e2e/terminal-image-kitty-decode.sh` invokes the production normalizer
through `scribe-test kitty-decode`. Stable cases cover RGB, RGBA, exact chunk
accumulation, RFC 1950 zlib, PNG, malformed base64, padded non-final chunks,
raw-length mismatch, truncated/non-PNG input, every indirect source class with
zero allocations, deterministic allocation denial, cancellation, deadline,
and a valid stream expanding one byte beyond the frozen ceiling.

The run atomically writes schema-versioned
`test-output/terminal-images/kitty-decode-evidence.json` with exact dependency
versions, revisions, checksums, licenses, fork exclusions, frozen limits,
typed case outcomes, and `all_passed`. Run only through
`just e2e-func terminal-image-kitty-decode.sh` after rebuilding the functional
Docker image.

## Decode Spike Verification

Docker verification exercises exact decode limits and emits a reviewable decision plus structured evidence without implementing production decoders.

`tests/e2e/functional/terminal-image-decode-spike.sh` runs the harness-only
probe through `just e2e-func`. It loads frozen `contract.json` values and covers
exact max/max-plus-one dimensions, a real fallible maximum allocation,
deterministic allocation denial, cumulative work max-plus-one, cooperative
cancellation, deadline checks, zlib and PNG bombs, valid PNG decode, and
gradual Sixel canvas growth.

The run writes schema-versioned
`test-output/terminal-images/decode-spike-evidence.json` and
`decoder-decision.md`. Evidence contains only limits, typed outcomes, counts,
dimensions, allocation peaks, and compressed sizes; it never records image
payloads or decoded pixels.

## IPC Contract Verification

Docker verification freezes MessagePack bytes, bounded decode behavior, local handshake defaults, and both remote version directions.

`tests/e2e/fixtures/terminal-images/ipc.json` stores stable named MessagePack
hex for legacy/current local handshakes, live chunks, every replay phase,
capability refusal, and client-older/server-older remote mismatches.
`scribe-test terminal-image-ipc` decodes them through the production shared
types, checks maximum and maximum-plus-one bounds, round-trips a named clipped
placement, proves absent legacy clips remain omitted/defaulted, rejects
reversed, empty, out-of-range, placeholder, and protocol-mismatched placements, and writes
`test-output/terminal-images/ipc.json`. Invoke it with
`just e2e-func terminal-image-ipc.sh`.

## Client Live Scene Verification

Docker verification proves ordered atomic live state, cleanup, text filtering, and truthful mismatch plumbing before image painting exists.

`tests/e2e/fixtures/terminal-images/client-scene.json` drives production
[[crates/scribe-client/src/terminal_image_scene.rs#LiveImageScene]] records
through definitions, bounded chunks, placements, replacement, delete, scroll,
erase, hard reset, interrupted generation replacement, and stale rejection.
It also freezes placeholder-copy input and typed capability mismatch data.

`tests/e2e/terminal-image-client-scene.sh` invokes
`scribe-test terminal-image-client-scene`, writes
`test-output/terminal-images/client-scene.json`, and requires every evidence
field to pass. This is CPU-scene evidence only; it makes no renderer, replay,
server fanout, or settings claim.

## Staged Client Replay Verification

Docker verification proves that a client applying the server's own planned burst never publishes a partial scene, never resurrects a superseded generation, and recovers from a corrupt burst.

`tests/e2e/terminal-image-client-replay.sh` invokes
`scribe-test terminal-image-client-replay` and writes
`test-output/terminal-images/client-replay.json`. The probe drives real PTY
bytes through the production seam, plans the burst with
[[crates/scribe-server/src/terminal_image_replay.rs#plan_replay]], and applies
every record through
[[crates/scribe-client/src/terminal_image_scene.rs#LiveImageScene#apply_replay]],
so atomicity is an observation of the published `Arc` identity rather than an
inference.

The gate pins one publication per burst, zero partial observations, an
order-preserving live drain compared against the same records applied without
buffering, typed refusal of an older generation as both a snapshot and a
buffered delta, a typed error for each of six corruptions built by permuting
real planned records, and the released pixels, retained-byte total, and buffer
ceiling that make cleanup observable.

## GPUI Lifecycle Verification

The isolated visual spike proves the selected shared-source crop and lifecycle path on Linux WGPU without implementing terminal image placement rendering.

`tests/e2e/visual/terminal-image-gpui-spike.sh` launches the guarded
`--gpui-image-spike` surface inside the visual Docker harness. The release
binary records GPUI's selected Linux WGPU adapter/backend, paints a
four-quadrant source twice through one `RenderImage`, and checks GPUI's source
identity across atlas invalidation. It uses
[[crates/scribe-client/src/gpui_image_lifecycle.rs#paint_cropped_image]],
requiring the cropped destination to remain green before and after atlas
invalidation and final-reference eviction.

[[crates/scribe-client/src/gpui_image_lifecycle.rs#GpuiImageCache]] rejects
4097-by-1 metadata before constructing a `RenderImage`, uploads 1-by-1 and
4096-by-1 sources, shares one source identity across full and cropped
placements, and calls `Window::drop_image` for each final cache reference.
Evidence lands in `test-output/terminal-images/linux/gpui-spike.json` beside
the three compared captures and sanitized log. The chosen path and pinned
source audit prove that the shared identity is one atlas key, `drop_image`
deallocates it, and device recovery clears then lazily rebuilds that key. Those
facts are frozen in
`specs/020-terminal-images/gpui-lifecycle-decision.md`.

## Layered GPUI Renderer Verification

The Docker visual corpus verifies production placement paint, phase ordering, geometry, placeholder mapping, and final-reference cleanup on Linux WGPU.

`tests/e2e/visual/terminal-image-renderer.sh` launches the guarded
`--terminal-image-renderer-probe` surface at a real 2x GPUI X11 scale and
captures crop, scale, alpha, every paint phase, 8-bit inherited and 32-bit
placeholder mapping, Sixel text chronology, production scroll/margin effects,
cache pressure, eviction, pane close, and atlas recovery. It invokes the same
[[crates/scribe-client/src/terminal_element.rs#TerminalElement]] and shared
[[crates/scribe-client/src/gpui_image_lifecycle.rs#GpuiImageCache]] used by live
panes.

Evidence lands under `test-output/terminal-images/linux/renderer/`.
`renderer.json` records device-space coordinates plus observed and expected RGB
for every phase/geometry assertion, the platform scale factor, and pixel
comparisons. Pressure must reject the extra live source without an atlas drop
or pixel change at the hard cache bound. Distinct overlapping Sixels prove
later completion wins below text; a selected current match proves find
precedence above images.
Eviction and Linux atlas-invalidation repaint must each change zero pixels;
pane close must leave zero projected GPU bytes after `Window::drop_image`.

The probe applies typed scroll effects through
[[crates/scribe-client/src/terminal_image_scene.rs#CommittedImageScene#apply_grid_effect]],
then hands the resulting committed scene to the renderer. This exercises the
production common-placement logical envelope and clip for partially visible
Kitty and Sixel rasters. First and repeated scroll captures keep the original
source crop, destination extent, and nonzero Y offset while the stored clip
moves through the margins, proving no proportional recrop or rounding drift.
An off-margin placement keeps its resized envelope, X/Y offsets, mapping, and
sampled pixels through an unrelated margin scroll. Production-scene delete
counts prove physical effective-extent selectors, virtual-placement immunity,
hard-scope definition selection, unrelated unplaced-definition survival, and
explicit deletion of an unplaced image. Linux atlas
invalidation remains a device-recovery proxy, not a physical-device-loss claim.

## Image Settings Verification

`just e2e-func terminal-image-settings.sh` proves the master switch, its resource release, its truthful advertising, the diagnostic catalog, and the renderer-failure taxonomy against shipped code.

The gate writes `test-output/terminal-images/settings.json` and its own run log,
then refuses either artifact if the pinned fixture's image payload appears in
it. The probe drives the settings model, the settings write path, the server
latch and reply planner, the session image seam, the client scene, and the GPUI
error taxonomy; see [[test#Test Harness#Image Settings and Diagnostics]] for the
case-by-case description.

## Native macOS Metal Validation

Native Metal evidence runs only on the sanctioned GitHub-hosted Apple-silicon runner and never on a developer workstation.

`.github/workflows/native-macos-metal.yml` is a manual `workflow_dispatch` job
on ARM64 `macos-14`, GitHub's standard Apple-silicon macOS runner, which is
free for public repositories. Whether that runner exposes a usable Metal
device is what this corpus verifies; a run that cannot reach Metal fails
closed rather than degrading silently. The workflow has read-only permission,
requires no secrets, builds release binaries, and invokes
`just native-macos-terminal-images`. The guarded recipe refuses every context
except GitHub Actions on the named macOS ARM64 runner, then requires executable
`tests/native-macos/terminal-images-metal.sh` before any Scribe runtime call.

A repository maintainer with GitHub write access owns dispatch, triage,
evidence review, and the release verdict. After the workflow is on the default
branch and the driver exists at the target ref, invoke:

```bash
gh workflow run native-macos-metal.yml --ref <release-candidate-ref>
```

The job records candidate SHA, ref, runner identity, OS, and architecture under
`test-output/terminal-images/macos/`: `runner.txt` and `display.txt` from the
workflow, `metal.json` as the machine-readable manifest, `run.log` as the
driver's merged stdout and stderr, and `protocol/`, `apps/`, and `gpui/` for the
per-phase transcripts. The driver receives that directory in
`SCRIBE_NATIVE_MACOS_OUTPUT_DIR`. `actions/upload-artifact` archives it as
`native-macos-metal-<run-id>` for 14 days, even after a corpus failure. Download
it with `gh run download <run-id> --dir <destination>`.

The runner identity is asserted in three places that must agree: the workflow
sets `SCRIBE_NATIVE_MACOS_RUNNER: github-actions-macos-14`, and both
`tools/run-native-macos-terminal-images.sh` and the driver refuse to run unless
they see that exact marker alongside `GITHUB_ACTIONS`, `RUNNER_OS=macOS`, and
`RUNNER_ARCH=ARM64`.

The 120-minute job has no soft-failure path. Build, corpus, timeout,
missing-driver, and artifact-upload failures block platform-dependent GPUI work
and default-on release. Product failures require a fixing commit. A maintainer
may retry an Actions infrastructure failure only when both run URLs and the
rationale remain in release evidence. A green exact-candidate run and retained
artifact are both required; Linux Docker and package-only macOS jobs do not
substitute. See [[test#Sandbox limits#Host-only hardware and platforms]].

### Required Metal lifecycle assertions

`tests/native-macos/terminal-images-metal.sh` runs the same contract, protocol, and pinned-application corpus the Linux harness runs, then the Metal facts Docker cannot produce.

The driver repeats the wrapper's context guard so a direct invocation cannot
reach a runtime call either, then records the frozen contract digest, runs
every in-process protocol probe against the owned fixtures on ARM64 macOS,
provisions the pinned Yazi/Chafa/gnuplot versions from the same checksums
`docker/Dockerfile.visual` pins, and drives them through a live native server
and a capable harness viewer.

Its Metal phase requires the running window to report the `metal` renderer.
`gpui_macos` has only a Metal renderer, but unlike `gpui_wgpu` it logs no
selected adapter and returns `None` from `Window::gpu_specs`, so the backend
name is the whole of the assertion. The manifest's `device` and
`device_metal_support` fields are best-effort `system_profiler` scrapes and are
empty on the hosted runner by design; nothing gates on them. The phase then requires one `RenderImage` per
definition, one reuse across the full and cropped placements, 1-by-1 and
4096-by-1 uploads, 4097-by-1 rejection with zero `RenderImage` objects created,
atlas recovery that preserves source identities, and three final-reference
drops. Stages advance from the render pass under
`SCRIBE_GPUI_IMAGE_SPIKE_AUTO=1`, because a hosted runner cannot synthesize key
events without an interactive accessibility grant; the Linux spike proves that
same unattended path.

Three Linux assertions have no native counterpart and the manifest names them
in `not_covered_natively`: the SSH hop (a transport, not a platform, fact), the
compared pixel captures, and an induced device loss. Window capture and key
synthesis both need a TCC grant no hosted runner has, so the native run asserts
protocol and lifecycle effects instead of pixels. In GPUI revision
`f96212f2c50f54d93712fa130d6226b1ce7d76b5` device-loss handling exists only in
`gpui_wgpu`, is set only from wgpu's own callback, and has no macOS
counterpart, so a genuine recoverable device loss cannot be induced from an
application without forking GPUI. That
decision is tracked separately and still blocks default-on release; the Linux
atlas-clear proxy must not satisfy it.

### Recorded green run

The gate has passed once against the shipped epic, and that run is the evidence a release review cites.

Run `31040886874` against candidate SHA `209da99` on runner
`github-actions-macos-14` retained artifact `native-macos-metal-31040886874`.
Its manifest records `backend=metal`, a 4096-pixel maximum and 1-pixel minimum
upload, a 4097-pixel rejection refused before GPUI, three `RenderImage` objects
created, one cache reuse, and three final-reference drops, with `ssh_transport`,
`pixel_captures`, and `induced_metal_device_loss` listed in
`not_covered_natively`. The run also produced a fix rather than only a verdict:
the harness wrote the server PID file before anything created the runtime
directory, which fails only on a fresh macOS host.
