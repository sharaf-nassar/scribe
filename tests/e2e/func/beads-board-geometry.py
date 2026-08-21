#!/usr/bin/env python3
"""Measure the A2 rail and the A3 position bar out of a real screenshot.

Every constant this reads comes from `.impeccable/mocks/a2a3-contract.json`,
the generated machine contract; nothing here re-transcribes the mock, and
nothing assumes five equal lanes, a fixed row pitch, or which of Blocked and
Done is currently a 36px tab. Where the tracks actually landed is read back
out of the pixels the shipped client painted:

  `rail`  finds the strip's own top by looking for the one row where every
          visible track -- full lane and collapsed tab alike -- paints its
          queue-hued 2px state seam at `lanes_padding_top + head_h`, then
          reports each painted track's left edge and width.
  `run`   reports the widest painted run inside one band of rows -- the A3
          position bar's thumb in its own 2px track, or the selected `FLOW`
          chip in the band -- which is how a wheel gesture's travel and the
          mode pair's own left edge are observed rather than assumed.

Both print plain lines a shell `read` can consume.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from collections import Counter

PIXEL = re.compile(r"^(\d+),(\d+): \((\d+),(\d+),(\d+)")
# A seam or thumb is ink over ground; anti-aliasing and the seam's own fade to
# 12% of its hue stay well inside this tolerance at both ends.
INK_TOLERANCE = 10


def read_band(shot: str, x: int, y: int, width: int, height: int) -> dict:
    """One cropped band's pixels, keyed by absolute (x, y)."""
    out = subprocess.run(
        [
            "convert", shot, "-crop", f"{width}x{height}+{x}+{y}", "+repage",
            "-depth", "8", "txt:-",
        ],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    band: dict[tuple[int, int], tuple[int, int, int]] = {}
    for line in out.splitlines():
        found = PIXEL.match(line)
        if found:
            band[(int(found.group(1)) + x, int(found.group(2)) + y)] = (
                int(found.group(3)),
                int(found.group(4)),
                int(found.group(5)),
            )
    return band


def differs(left: tuple, right: tuple, tolerance: int) -> bool:
    return max(abs(a - b) for a, b in zip(left, right)) > tolerance


def ink_runs(row: dict, ground: tuple, merge: int) -> list[tuple[int, int]]:
    """Contiguous runs of non-ground pixels, joined across `merge` px of gap.

    `merge` counts ground pixels tolerated *inside* one run, so `0` joins only
    genuinely adjacent pixels. The tolerance is what keeps a lane's own faded
    seam tail one run instead of several, while still separating two tracks
    the renderer put a real inter-track gap between.
    """
    runs: list[list[int]] = []
    for x in sorted(row):
        if not differs(row[x], ground, INK_TOLERANCE):
            continue
        if runs and x - runs[-1][1] <= merge + 1:
            runs[-1][1] = x
        else:
            runs.append([x, x])
    return [(left, right) for left, right in runs if right - left >= 2]


def rail(args: argparse.Namespace) -> int:
    a2 = json.load(open(args.contract, encoding="utf-8"))["geometry"]["a2"]
    pad_left = int(a2["lanes_padding_left"])
    pad_right = int(a2["lanes_padding_right"])
    gap = int(a2["track_gap"])
    seam_offset = int(a2["lanes_padding_top"]) + int(a2["head_h"])

    band = read_band(args.shot, 0, args.search_top, args.width, args.search_height)
    for y in range(args.search_top, args.search_top + args.search_height):
        row = {x: colour for ((x, seen), colour) in band.items() if seen == y}
        if not row:
            continue
        found = ink_runs(row, row[min(row)], gap - 4)
        # The seam row is the only row whose ink starts exactly at the lane
        # padding and repeats per track: the text-size gutter starts at
        # `zoom_left`, and the headband hairline spans the whole strip.
        if len(found) >= 3 and found[0][0] == pad_left:
            print(y - seam_offset)
            for index, (left, right) in enumerate(found):
                width = (
                    found[index + 1][0] - gap - left
                    if index + 1 < len(found)
                    else min(right - left + 1, args.width - pad_right - left)
                )
                print(left, width)
            return 0
    print("no painted A2 seam row in the searched band", file=sys.stderr)
    return 1


def widest_run(args: argparse.Namespace) -> int:
    band = read_band(args.shot, 0, args.y, args.width, args.height)
    ground = Counter(band.values()).most_common(1)[0][0]
    widest: tuple[int, int] | None = None
    for y in range(args.y, args.y + args.height):
        row = {x: colour for ((x, seen), colour) in band.items() if seen == y}
        for left, right in ink_runs(row, ground, 0):
            if right - left + 1 >= args.min_width and (
                widest is None or right - left > widest[1] - widest[0]
            ):
                widest = (left, right)
    if widest is None:
        print("no painted run of that width in the searched band", file=sys.stderr)
        return 1
    print(widest[0], widest[1] - widest[0] + 1)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    rail_parser = sub.add_parser("rail", help="painted A2 strip top and track bounds")
    rail_parser.add_argument("--contract", required=True)
    rail_parser.add_argument("--shot", required=True)
    rail_parser.add_argument("--width", type=int, required=True)
    rail_parser.add_argument("--search-top", type=int, default=0)
    rail_parser.add_argument("--search-height", type=int, default=120)
    rail_parser.set_defaults(run=rail)

    run_parser = sub.add_parser("run", help="widest painted run in a band of rows")
    run_parser.add_argument("--shot", required=True)
    run_parser.add_argument("--width", type=int, required=True)
    run_parser.add_argument("--y", type=int, required=True)
    run_parser.add_argument("--height", type=int, default=1)
    run_parser.add_argument("--min-width", type=int, default=1)
    run_parser.set_defaults(run=widest_run)

    args = parser.parse_args()
    return args.run(args)


if __name__ == "__main__":
    raise SystemExit(main())
