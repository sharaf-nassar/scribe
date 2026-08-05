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
failure. `q=1` suppresses success, a continuation chunk replies only when its
final chunk lands, and a session whose capability is not live owes nothing at
all. The planner is pure, so replanning the same commit — what a reattach or a
replay would do — cannot add a second reply.

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
uses. A definition the caller cannot pay for is withdrawn together with every
placement naming it: an unbacked definition would leave the receiver staging a
scene it can never complete.

### Replay debt

A sink owes a replay in exactly two situations: it just attached, or its queued
output was shed.

Either way what the viewer has seen no longer describes the session.
[[crates/scribe-server/src/ipc_server.rs#AttachedSinks#fan_out_images]] detects
the shed case at the moment the queue refuses a frame, which is synchronous and
race-free, and stops sending deltas to that sink.

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
([`advertised_terminal_image_capabilities`](../crates/scribe-client/src/main.rs));
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

## Native macOS Metal Validation

Native Metal evidence runs only on the sanctioned GPU-backed GitHub-hosted runner and never on a developer workstation.

`.github/workflows/native-macos-metal.yml` is a manual `workflow_dispatch` job
on ARM64 `macos-14-xlarge`. GitHub documents that runner class as carrying GPU
hardware acceleration. The workflow has read-only repository permission,
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

The job records candidate SHA, ref, runner identity, OS, architecture, and
display/Metal metadata under `test-output/terminal-images/macos/`. The
driver receives that directory in `SCRIBE_NATIVE_MACOS_OUTPUT_DIR`; its merged
stdout/stderr is `run.log`. `actions/upload-artifact` archives the directory as
`native-macos-metal-<run-id>` for 14 days, even after a corpus failure. Download
it with `gh run download <run-id> --dir <destination>`.

The 120-minute job has no soft-failure path. Build, corpus, timeout,
missing-driver, and artifact-upload failures block platform-dependent GPUI work
and default-on release. Product failures require a fixing commit. A maintainer
may retry an Actions infrastructure failure only when both run URLs and the
rationale remain in release evidence. A green exact-candidate run and retained
artifact are both required; Linux Docker and package-only macOS jobs do not
substitute. See [[test#Sandbox limits#Host-only hardware and platforms]].

### Required Metal lifecycle assertions

The downstream native driver must run the same crop and lifecycle corpus on Metal plus a genuine recoverable device-loss phase.

The driver must record a Metal adapter, one upload for shared full/crop
placements, a green cropped quadrant, reusable texture space after
`drop_image`, three final-reference drops, unchanged pixels after recreation,
1-by-1 and 4096-by-1 uploads, and 4097-by-1 rejection with zero new
`RenderImage` objects. It must then induce one recoverable device loss through
a pinned test hook, observe GPUI context and atlas recreation, preserve source
identities, and require a zero-difference repaint.

Terminal placement rendering now exists, but the genuine Metal device-loss
hook and downstream native driver do not. The workflow remains fail-closed
until those native pieces land; the Linux atlas-clear proxy must not satisfy
the Metal device-loss assertion.
