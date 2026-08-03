# scribe-png-decoder

Bounded decoder-only PNG fork used by Scribe for untrusted Kitty payloads.

## Upstream pin

- Package: `png 0.18.1`
- Published source: <https://crates.io/crates/png/0.18.1>
- Upstream repository: <https://github.com/image-rs/image-png>
- Embedded VCS revision: `2a3f980245e3ae38b82ade96533e7b450e8477bb`
- crates.io `.crate` SHA-256:
  `60769b8b31b2a9f263dae2776c37b1b28ae246943cf719eb6946a1db05128a61`
- License: `MIT OR Apache-2.0`; exact published license files are retained.

The pin was verified from crates.io's published archive, its
`.cargo_vcs_info.json`, Cargo metadata, current docs.rs API/source, and the
upstream repository on 2026-08-03.

## Fork delta and exclusions

Retained decoder concepts from `src/decoder/stream.rs`, `zlib.rs`,
`unfiltering_buffer.rs`, `transform.rs`, and `adam7.rs`: signature/chunk/CRC
validation, strict static-image ordering, RFC 1950 IDAT inflate, all legal PNG
color/bit-depth combinations, filters, Adam7, transparency, and canonical RGBA
conversion.

Scribe replaced upstream public APIs, allocation, inflate, and output ownership
with strict dimensions, checked arithmetic, fallible exact allocations, and the
shared caller-owned `DecodeBudget`. Hooks run at compressed-input, inflated
output, row-unfilter, pixel-conversion, allocation, cancellation, and deadline
boundaries. CRC is a small safe scalar implementation; low-level `flate2 1.1.9`
uses only its pure-Rust `rust_backend`.

Excluded entirely: encoder modules, APNG (`acTL`/`fcTL`/`fdAT`), text chunks,
ICC/profile parsing, EXIF, generic format selection, whole-image stock APIs,
unsafe code, paths/URLs/resources, and C/FFI decoders. Unknown ancillary chunks
are CRC-checked and skipped without allocation; unknown critical chunks reject.

## Security ownership and updates

Scribe maintainers own CVE/RustSec review, upstream-diff review, and emergency
patches for this fork. Review every `png` release and the regular dependency
advisory audit. Compare changes from the pinned revision, port relevant decoder
security fixes, rerun the adversarial Docker corpus, `cargo deny`, and dependency
tree review, then update this pin, checksum, fork delta, and both licenses.

Security reports follow Scribe's repository security policy. Never replace the
fork with stock `png`, generic `image`, a C decoder, or a resource loader without
a new trust-boundary review.
