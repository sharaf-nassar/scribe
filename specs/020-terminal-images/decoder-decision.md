# Bounded Decoder Spike Decision

The decoder spike gives a conditional go for production Kitty and Sixel decoder work, with narrow vendored boundaries and Scribe-owned budget enforcement.

## Decision

Proceed only with incremental APIs that expose every allocation and work boundary to Scribe; stock whole-image decode entry points remain outside the trust boundary.

| Path | Decision | Boundary |
| --- | --- | --- |
| RFC 1950 zlib | Go | Use `flate2 1.1.9` low-level `Decompress` only, with caller-owned 4,096-byte output storage, checked input/output work charges, projected-output checks, cancellation, deadline checks, and fallible growth. |
| PNG | Fork required | Vendor the decoder core from `png 0.18.1`, exclude encoder/APNG/text/profile paths, and add a Scribe step hook around compressed input, inflated output, row unfiltering, and pixel conversion. |
| Sixel | Fork required | Vendor decoder-only source from `icy_sixel 0.5.0`, remove encoder, SIMD span fast paths until audited, and `quantette`; add `DecodeLimits`, checked growth, fallible allocation, work/cancel/deadline hooks, and typed failures. |
| `image 0.25.10` generic decode | No-go | Its allocation budget is non-strict and the generic reader admits formats outside PNG; it has no cooperative work/cancellation hook. |
| Stock `png 0.18.1` whole-frame/row decode | No-go | Its documented allocation limit is best effort and internal inflate/unfilter work cannot be interrupted at Scribe's 4,096-unit interval. |
| Stock `icy_sixel 0.5.0` | No-go | Its public decode call accepts no caller dimensions, allocation, work, deadline, or cancellation policy and pulls the encoder's `quantette` dependency. |
| C decoders | No-go | They add an unnecessary unsafe/FFI trust boundary and do not satisfy the required Rust-owned fallible allocation and cooperative cancellation contract. |

The selected crates are pure Rust at the retained boundaries. The production
vendor tasks must pin crates.io source/checksum and upstream URL, retain both
MIT and Apache-2.0 notices, document every fork delta, and assign CVE/update
ownership. This spike does not vendor or implement either production decoder.

## Evidence Schema

Docker writes `test-output/terminal-images/decode-spike-evidence.json` and `decoder-decision.md` through the functional harness.

Evidence schema version 1 contains the contract version; exact copied limits;
an `all_passed` aggregate; the decoder decision and library boundaries; and a
case array. Each case has a stable id, status, typed rejection where relevant,
and boundary-specific measurements such as work units, check interval,
allocation peak, compressed size, dimensions, or byte ceiling.

The executable probe loads `contract.json` instead of duplicating constants.
It allocates one exact 4096-square canonical buffer fallibly, rejects both
axis max-plus-one cases before allocation, injects a deterministic allocation
failure, and exercises exact work max/max-plus-one accounting. Cancellation
and deadline checks occur at 4,096 charged units.

The bomb corpus is generated in-process without network or disk inputs. A
valid RFC 1950 stream expands to `max_inflated_bytes + 1`; both direct zlib and
PNG IDAT preflight stop at the frozen output ceiling. A valid one-pixel RGBA
PNG also passes the stock decoder behind preflight, proving interoperability
without treating that whole-image API as production-safe.

Gradual Sixel growth expands a fallible canvas one column at a time through
the exact 4,096 width limit and rejects column 4,097 before growth. Peak live
allocation remains the last accepted canvas size.

## Source Verification

Dependency decisions use current primary crate documentation and inspected locked source, not assumed APIs.

- [`flate2 1.1.9 Decompress`](https://docs.rs/flate2/1.1.9/flate2/struct.Decompress.html)
  exposes incremental input/output buffers plus `total_in` and `total_out`.
- [`png 0.18.1`](https://docs.rs/png/0.18.1/png/) supports row and streaming
  decode, while its source documents `Limits` as a best-effort allocation
  control that excludes caller buffers.
- [`image 0.25.10 Limits`](https://docs.rs/image/0.25.10/image/struct.Limits.html)
  documents width/height as strict but `max_alloc` as non-strict.
- [`icy_sixel 0.5.0`](https://docs.rs/icy_sixel/0.5.0/icy_sixel/)
  exposes whole-slice decode and depends on `quantette` for its encoder.

## Remaining Production Work

Dependent decoder tasks must turn the proven controls into reusable production types without weakening or duplicating the frozen contract.

The bounded Sixel task owns vendoring, license/checksum inventory, parser
semantics, removal of encoder/quantizer code, typed errors, and adversarial
tests. The bounded Kitty task owns base64/chunk normalization, the zlib wrapper,
the PNG decoder-core fork, exact raw lengths, canonical RGBA conversion, and
format rejection. Both share one `DecodeBudget`/cancellation interface and
must prove no interval exceeds 4,096 work units.

Queue concurrency, generation cancellation, ordered commit, stale completion,
session/process retention, IPC replay, and GPU upload remain server/common
tasks. They must consume completed bounded decode results and must not move
decoder work onto the GPUI paint path.
