# icy-sixel-decoder

Bounded decoder-only fork used by Scribe for untrusted Sixel payloads.

## Upstream pin

- Package: `icy_sixel 0.5.0`
- Published source: <https://crates.io/crates/icy_sixel/0.5.0>
- Upstream repository: <https://github.com/mkrueger/icy_sixel>
- Embedded VCS revision: `998cbb2c6d8ed5272f9cc4702a4660778972bf3f`
- crates.io `.crate` SHA-256:
  `85518b9086bf01117761b90e7691c0ef3236fa8adfb1fb44dd248fe5f87215d5`
- Upstream path: `crates/icy_sixel`
- License: `MIT OR Apache-2.0`; exact upstream license files are retained.

The pin was verified from crates.io's published archive, its
`.cargo_vcs_info.json`, Cargo metadata, and the reachable upstream revision on
2026-08-03.

## Fork delta and exclusions

Only decoder concepts and palette/raster semantics from upstream
`src/decoder.rs` and metadata types from `src/sixel_image.rs` are retained.
The public API was replaced with caller-owned `DecodeLimits`, an absolute
monotonic deadline, cooperative cancellation/allocation hooks, typed
payload-free errors, checked arithmetic, fallible canvas growth, and bounded
work checks.

Excluded entirely: encoder APIs/source, `quantette`, quantization types,
benchmarks, CLI/image dependencies, deprecated whole-image entry points,
unsafe pointer fills, x86/x86_64 SIMD span paths, and all C/FFI decoders. The
only dependency is Scribe's shared caller-owned decode-budget crate.

## Security ownership and updates

Scribe maintainers own CVE/RustSec review, upstream-diff review, and emergency
patches for this fork. Review on every `icy_sixel` release and during the
regular dependency advisory audit. Compare upstream decoder/parser changes
from the pinned revision, classify security and compatibility fixes, port only
audited decoder changes, rerun the adversarial Docker corpus, `cargo deny`,
and `cargo tree`, then update this pin, checksum, fork delta, and both licenses.

Report a suspected vulnerability in this fork privately through a GitHub
security advisory on the Scribe repository rather than a public issue. Do not replace
this fork with stock `icy_sixel`, a C decoder, or a whole-terminal dependency
without a new trust-boundary review.
