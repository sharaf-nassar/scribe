# Terminal image protocol decision

Scribe will add bounded, in-process Sixel and Kitty graphics support to the
GPUI client, using a shared placement renderer rather than treating images as
text glyphs.

## Decision

The supported protocols are:

| Protocol | Input framing and parser | First supported subset |
| --- | --- | --- |
| Sixel | DCS `q` is captured before the Alacritty VTE consumes it; `icy_sixel` 0.5.0 decodes the complete DCS into RGBA. | Raster DCS images, palette changes, repeat, raster attributes, transparent background, and cursor-relative placement. |
| Kitty graphics | A streaming splitter captures `ESC _ G ... ESC \\` before the Alacritty VTE consumes it; a narrowed parser derived from WezTerm's `wezterm-escape-parser/src/apc.rs` parses the resulting `G<control data>;<payload>` body. | Direct (`t=d`) RGB/RGBA/PNG transmission, zlib/DEFLATE, chunk accumulation, transmit/display, display, delete, and query actions. |

iTerm2/OSC 1337 images, Kitty file/temporary-file/shared-memory transmission,
Unicode placeholders, animations, and frame composition are deliberately not
supported in the first release. They remain safely ignored. This scope covers
the two protocols named in the follow-on register while avoiding host-file and
shared-memory access initiated by untrusted PTY output.

## Why this split

`icy_sixel` is a small, pure-Rust MIT-or-Apache-2.0 crate with a public DCS
decoder that yields RGBA pixels. Version 0.5.0 validates dimensions and limits
its own decoded canvas, but Scribe still applies lower protocol budgets before
calling it because the PTY is untrusted.

Kitty graphics are an APC protocol, but `vte` 0.15.0 routes SOS, PM, and APC
bytes through a discarded string state instead of exposing them to `Perform`.
VTE does expose DCS callbacks, but the `vte::ansi::Processor` used by
`DisplayOnlyTerminal` logs and ignores those callbacks. The client therefore
uses one streaming DCS/APC splitter in front of the existing processor. It
sends ordinary bytes unchanged to Alacritty, sends a complete Sixel DCS to
`icy_sixel`, and strips APC framing before sending the Kitty body to the
dedicated control-data parser. Scanning rendered cells after the fact cannot
recover either protocol.

The Kitty parser is vendored rather than brought in as the full WezTerm parser
crate. Derive a narrowed module from the MIT-licensed
`wezterm-escape-parser/src/apc.rs` at WezTerm commit
`76b606ec597a3c0263fa60321548637451c0a547`, and record the upstream path,
commit, MIT license, and copyright attribution in `third_party/wezterm-apc/`.
Keep only typed parsing for the first-release keys and actions plus adapted
parser tests; the upstream `parse_apc` entry point expects a body beginning
with `G`, not the surrounding APC escape bytes. Scribe owns framing, base64
and zlib processing, chunk accumulation, data loading, image storage,
placement, quotas, and rendering. This avoids importing WezTerm's terminal
state, file/shm loaders, image crate, and unrelated escape protocols.

Sources examined: [icy_sixel 0.5.0](https://docs.rs/icy_sixel/0.5.0/icy_sixel/),
[icy_sixel source](https://github.com/mkrueger/icy_sixel/tree/6472b6be6f5d7b20c17498957e22d2480f2e12a3),
[WezTerm APC source](https://github.com/wezterm/wezterm/blob/76b606ec597a3c0263fa60321548637451c0a547/wezterm-escape-parser/src/apc.rs),
the [WezTerm MIT license](https://github.com/wezterm/wezterm/blob/76b606ec597a3c0263fa60321548637451c0a547/LICENSE.md),
the [`vte` 0.15.0 parser](https://docs.rs/crate/vte/0.15.0/source/src/lib.rs),
the [`vte` ANSI processor](https://docs.rs/crate/vte/0.15.0/source/src/ansi.rs),
and the [Kitty graphics specification](https://sw.kovidgoyal.net/kitty/graphics-protocol/).

## Rendering and state model

Both decoders feed one client-owned `TerminalImageStore`, keyed by image id and
placement id. It stores RGBA textures separately from cursor-relative
placements, so a Kitty image may have multiple placements and a Sixel image
can use the same paint path without pretending to be terminal text.

`TerminalElement` paints image quads after cell backgrounds and before glyphs.
The quads use the terminal grid's current cell pixel metrics, clip to the pane,
scroll with their anchored rows, and respect a Kitty placement's z-index. This
ordering permits normal text above an image while retaining terminal selection
and text copy semantics. Image state belongs to each `DisplayOnlyTerminal` and
is cleared on RIS, ED 2, and alternate-screen entry, matching the Kitty reset
rules; scroll and resize recompute placement geometry from the grid rather than
resampling stored pixels.

Sixel's image becomes one cursor-relative placement at DCS completion. Its
pixel dimensions advance the terminal cursor using the actual current cell
size. Kitty transmission and placement stay separate: upload only stores
pixels, `a=T` uploads and places, `a=p` places stored pixels, and `a=d` removes
the requested placement or data. Query responses are returned to the source
PTY through the existing ordered `IpcSink` key-input path, never written into
the visible terminal stream.

## Safety and resource policy

The decoder boundary is a PTY trust boundary. The first release applies these
limits before allocation or texture upload:

- 16 MiB maximum encoded DCS/APC sequence and direct Kitty upload;
- 16 MiB maximum decoded image; dimensions and RGBA multiplication use checked
  arithmetic;
- 64 MiB total retained image data per pane, evicting oldest unplaced data
  first; and
- 256 active placements per pane, with malformed, over-budget, incomplete, or
  unsupported sequences discarded without disturbing terminal text.

The splitter must bound an unterminated DCS/APC while preserving subsequent
ordinary terminal output. Decoding runs off the GPUI paint path; completed
RGBA textures are uploaded and invalidated on the foreground thread. No image
decoder, decompressor, or base64 work may run while a frame is being painted.

## Delivery criteria

The implementation feature is complete only when it has a byte-split streaming
test corpus for DCS/APC framing, parser tests pinned to the vendored WezTerm
revision, malformed-input and quota tests, and running-client visual tests for
Sixel plus Kitty direct PNG/RGBA placement, scrolling, deletion, text-overlaid
z-order, alternate-screen reset, and resize. A manual check against kitty and
a Sixel-capable terminal application validates protocol replies and cursor
advance. The performance report records decode/upload latency and retained GPU
memory under a 10-image workload; no image protocol may degrade ordinary
text-only output when it is absent.
