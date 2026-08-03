# GPUI Image Lifecycle Decision

Terminal image rendering will use one bounded GPUI source per image generation and crop it with existing paint bounds plus a content mask.

## Pinned Source Evidence

The decision is based on GPUI revision `f96212f2c50f54d93712fa130d6226b1ce7d76b5` resolved by this checkout's `Cargo.lock`.

The inspected revision establishes these load-bearing facts:

- `crates/gpui/src/assets.rs`: `RenderImage::new` assigns a stable `ImageId`;
  its frames retain CPU image bytes.
- `crates/gpui/src/window.rs`: `Window::paint_image` uses
  `RenderImageParams { image_id, frame_index }` as its atlas key and calls
  `get_or_insert_with`; repeated placements of one `Arc<RenderImage>` do not
  rebuild the tile.
- `crates/gpui/src/window.rs`: image painting has destination bounds and a
  content mask but no source-UV parameter. `Window::with_content_mask` is
  public during paint.
- `crates/gpui_wgpu/src/shaders.wgsl`: the polychrome vertex shader derives
  texture position across the full sprite bounds, then clips against its
  content mask.
- `crates/gpui/src/window.rs`: `Window::drop_image` removes every frame key.
  `crates/gpui_wgpu/src/wgpu_atlas.rs` deallocates the removed tile and frees an
  unreferenced texture slot.
- `crates/gpui_wgpu/src/wgpu_context.rs` and `wgpu_renderer.rs`: non-destroyed
  device loss sets a recovery flag; recovery recreates the context, calls
  `WgpuAtlas::handle_device_lost`, clears tiles/uploads, and lets later paints
  lazily reconstruct them from CPU `RenderImage` data.

No assumed or Zed-current API influenced the choice; all names above exist at
the exact revision compiled by Scribe.

## Selected Crop Path

One `(session_id, image_id, generation)` cache entry owns the full `RenderImage`; every placement in that session references the same identity.

For source rectangle `(sx, sy, sw, sh)`, source dimensions `(iw, ih)`, and
destination `(dx, dy, dw, dh)`, Scribe paints the full image at:

```text
scale = (dw / sw, dh / sh)
origin = (dx - sx * scale.x, dy - sy * scale.y)
size = (iw * scale.x, ih * scale.y)
envelope = conservative_cells(destination, pixel_offsets)
mask = intersection(envelope, logical_cell_clip, viewport)
```

This maps the selected source rectangle exactly onto the destination while the
mask removes every other source pixel. Scroll and resize move/intersect the
common placement's logical cell clip without changing source, destination, or
pixel offsets. The envelope adds one right/bottom cell for nonzero X/Y offsets,
and the renderer converts the effective intersection with current cell metrics. It preserves
one atlas key across full, classic-crop, and placeholder placements. No crop
variant cache, CPU recrop, extra upload, shader patch, or GPUI fork is required.

## Resource and Limit Contract

The per-view cache validates common metadata and byte length before allocating a GPUI object.

Each source charges twice its canonical RGBA length against
`ImageLimits::V1.max_view_projected_gpu_bytes`: one texture plus one
upload/staging estimate. Before a pane queues image primitives, it removes
closed-session and unplaced source generations. Admission never evicts a live
entry: if remaining live entries plus a new source exceed the view ceiling,
the new source is skipped for that frame. This prevents `drop_image` from
reusing an atlas tile that an earlier same-frame primitive still references
and avoids cross-session eviction churn while preserving the hard bound.
Final removal calls `Window::drop_image` before releasing the last cache
reference. Invalid dimensions, including 4097-by-1, return the common typed
`Dimensions` rejection before BGRA conversion, `image::Frame`, or
`RenderImage::new`.

The Linux probe uploads 1-by-1 and 4096-by-1 resources. This proves the frozen
axis maximum without allocating a gratuitous 4096-by-4096 corpus image; the
independent pixel and canonical-byte maxima remain enforced by the same
`TerminalImageDefinition::validate` call and existing contract corpus.

## Linux WGPU Evidence

The Docker visual probe executes the supported GPUI Linux WGPU window path and combines runtime observations with the exact-source atlas audit above.

Run:

```bash
just build-release
just docker-visual
just e2e-visual terminal-image-gpui-spike.sh
```

The probe requires all of the following before writing
`test-output/terminal-images/linux/gpui-spike.json`:

1. GPUI logs its selected llvmpipe adapter and WGPU backend; evidence also
   records the visual image's configured Lavapipe Vulkan ICD rather than
   assuming which backend won selection.
2. Full and cropped placements share one `RenderImage` identity and the cache
   creates exactly one source per definition; pinned `Window::paint_image`
   source proves that identity is the atlas key.
3. The cropped capture is the expected green quadrant.
4. Calling `Window::drop_image` as a recovery invalidation preserves pixels
   and source identities after repaint; pinned WGPU recovery source performs
   the same atlas clear before lazy repaint.
5. Final-reference cache eviction calls `drop_image` for all three sources;
   recreated identities repaint with zero differing pixels.
6. 1-by-1 and 4096-by-1 upload, while 4097-by-1 creates zero GPUI images.

The 2026-08-03 Docker run selected llvmpipe through WGPU `Gl`, while retaining
the configured Lavapipe Vulkan ICD in process state. Crop means were green
`1.0`, red `0.0`, and blue `0.0`; recovery and eviction comparisons each
changed zero pixels, and all three final cache references were dropped.

The runtime invalidation/repaint and pinned-source audit together prove GPUI's
image reupload seam, tile cleanup, and recovery design. They do not claim that
Docker induced a physical device loss or expose unsupported atlas internals.

## Native Metal Assertions

Native Metal remains a distinct fail-closed runtime gate on the sanctioned GitHub-hosted `macos-14-xlarge` runner.

The downstream executable `tests/native-macos/terminal-images-metal.sh` must:

1. verify GitHub Actions, macOS ARM64, the sanctioned runner marker, candidate
   SHA, and a WGPU `Metal` adapter before product assertions;
2. run the shared-source probe at 1-by-1 and 4096-by-1, reject 4097-by-1 before
   `RenderImage` creation, and record adapter texture limits without raising
   frozen `ImageLimits`;
3. require identical full/crop source IDs, one initial upload, a green crop,
   reusable texture space after `drop_image`, three final-reference drops, and
   zero-difference recreation;
4. invoke a pinned one-shot test hook that produces a recoverable, non-destroyed
   Metal device-loss signal after the initial frame;
5. observe GPUI's context-recreation start and completion, atlas recreation,
   preserved CPU source IDs, one lazy reupload per live source, and a
   zero-difference post-recovery capture;
6. write machine-readable results, logs, and all compared captures beneath
   `SCRIBE_NATIVE_MACOS_OUTPUT_DIR`, failing on missing fields or artifacts.

The lifecycle spike intentionally does not create that driver. Terminal image
placement rendering and the genuine device-loss hook land downstream; a driver
created now could test only the isolated surrogate and would incorrectly make
the guarded workflow green. Until both exist, the workflow's missing-driver
check remains the correct release-blocking result.
