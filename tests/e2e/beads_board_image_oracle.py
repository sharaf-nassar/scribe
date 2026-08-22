#!/usr/bin/env python3
"""Shared pixel measurements for the Beads board E2E suites.

The generated contract supplies fixed geometry. This oracle reads the geometry
that the running client actually painted: the A2 seam tracks and widest marks
inside bounded bands. Its CLI output is shell-safe for the two E2E suites.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from collections.abc import Iterable
from pathlib import Path

INK_TOLERANCE = 10


class Image:
    """An ImageMagick RGBA capture with clamped pixel lookup."""

    def __init__(self, path: str):
        self.path = path
        size = subprocess.check_output(
            ["identify", "-format", "%w %h", path], text=True
        ).split()
        self.width, self.height = (int(value) for value in size)
        self.data = subprocess.check_output(["convert", path, "rgba:-"])

    def pixel(self, x: float, y: float) -> tuple[int, int, int]:
        x = min(self.width - 1, max(0, round(x)))
        y = min(self.height - 1, max(0, round(y)))
        offset = 4 * (y * self.width + x)
        return tuple(self.data[offset : offset + 3])


def delta(left: tuple[int, int, int], right: tuple[int, int, int]) -> int:
    return sum(abs(a - b) for a, b in zip(left, right))


def differs(left: tuple[int, int, int], right: tuple[int, int, int], tolerance: int = INK_TOLERANCE) -> bool:
    return max(abs(a - b) for a, b in zip(left, right)) > tolerance


def contract(path: str) -> dict:
    return json.loads(Path(path).read_text(encoding="utf-8"))


def runs(flags: Iterable[bool], minimum: int = 1, merge: int = 0) -> list[tuple[int, int]]:
    """Return (start, width) runs, joining false gaps up to ``merge`` pixels."""
    found: list[tuple[int, int]] = []
    start: int | None = None
    end: int | None = None
    for index, enabled in enumerate(flags):
        if not enabled:
            continue
        if start is None:
            start = index
        elif index - end > merge + 1:
            if end - start + 1 >= minimum:
                found.append((start, end - start + 1))
            start = index
        end = index
    if start is not None and end - start + 1 >= minimum:
        found.append((start, end - start + 1))
    return found


def seam_tracks(image: Image, board_top: int, data: dict, left: int = 0, width: int | None = None) -> list[tuple[int, int]]:
    """Read the five A2 tracks from their painted state-seam row."""
    geometry = data["geometry"]["a2"]
    width = width or image.width
    seam_y = board_top + geometry["lanes_padding_top"] + geometry["head_h"]
    ground = image.pixel(left + 2, board_top + geometry["headband_h"] + 8)
    found = [
        (left + start, length)
        for start, length in runs(
            (delta(image.pixel(x, seam_y), ground) > 8 for x in range(left, left + width)),
            3,
        )
    ]
    tail = 2 * geometry["tab_w"] + geometry["track_gap"]
    expected_tail_x = left + width - geometry["lanes_padding_right"] - tail
    if (
        len(found) == 4
        and close(found[-1][0], expected_tail_x)
        and tail <= found[-1][1] <= tail + geometry["lanes_padding_right"] + 2
    ):
        found[-1:] = [
            (round(expected_tail_x), round(geometry["tab_w"])),
            (round(expected_tail_x + geometry["tab_w"] + geometry["track_gap"]), round(geometry["tab_w"])),
        ]
    return found


def close(actual: float, expected: float, tolerance: float = 1.1) -> bool:
    return abs(actual - expected) <= tolerance


def rail_search(args: argparse.Namespace) -> int:
    data = contract(args.contract)
    geometry = data["geometry"]["a2"]
    pad_left = int(geometry["lanes_padding_left"])
    pad_right = int(geometry["lanes_padding_right"])
    gap = int(geometry["track_gap"])
    seam_offset = int(geometry["lanes_padding_top"]) + int(geometry["head_h"])
    image = Image(args.shot)

    for y in range(args.search_top, args.search_top + args.search_height):
        ground = image.pixel(0, y)
        found = runs(
            (differs(image.pixel(x, y), ground) for x in range(args.width)),
            3,
            gap - 4,
        )
        if len(found) < 3 or found[0][0] != pad_left:
            continue
        print(y - seam_offset)
        for index, (left, run_width) in enumerate(found):
            width = (
                found[index + 1][0] - gap - left
                if index + 1 < len(found)
                else min(run_width, args.width - pad_right - left)
            )
            print(left, width)
        return 0
    print("no painted A2 seam row in the searched band", file=sys.stderr)
    return 1


def widest_run(args: argparse.Namespace) -> int:
    image = Image(args.shot)
    colors: dict[tuple[int, int, int], int] = {}
    for y in range(args.y, args.y + args.height):
        for x in range(args.width):
            pixel = image.pixel(x, y)
            colors[pixel] = colors.get(pixel, 0) + 1
    ground = max(colors, key=colors.get)
    widest: tuple[int, int] | None = None
    for y in range(args.y, args.y + args.height):
        for left, width in runs(
            differs(image.pixel(x, y), ground) for x in range(args.width)
        ):
            if width >= args.min_width and (widest is None or width > widest[1]):
                widest = (left, width)
    if widest is None:
        print("no painted run of that width in the searched band", file=sys.stderr)
        return 1
    print(*widest)
    return 0


def contract_env(args: argparse.Namespace) -> int:
    data = contract(args.contract)
    for section in ("a2", "a3"):
        for key, value in data["geometry"][section].items():
            if isinstance(value, (int, float)):
                print(f"{section.upper()}_{key.upper()}={value}")
    slugs = [f"{entry['section'].lower()}:{entry['slug']}" for entry in data["states"]]
    print("CONTRACT_STATE_SLUGS=" + ",".join(slugs))
    print("CONTRACT_SOURCE_SHA=" + data["source"]["sha256"])
    return 0


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    sub = root.add_subparsers(dest="command", required=True)

    rail = sub.add_parser("rail-search", help="painted A2 strip top and track bounds")
    rail.add_argument("--contract", required=True)
    rail.add_argument("--shot", required=True)
    rail.add_argument("--width", type=int, required=True)
    rail.add_argument("--search-top", type=int, default=0)
    rail.add_argument("--search-height", type=int, default=120)
    rail.set_defaults(func=rail_search)

    widest = sub.add_parser("widest-run", help="widest painted run in a band of rows")
    widest.add_argument("--shot", required=True)
    widest.add_argument("--width", type=int, required=True)
    widest.add_argument("--y", type=int, required=True)
    widest.add_argument("--height", type=int, default=1)
    widest.add_argument("--min-width", type=int, default=1)
    widest.set_defaults(func=widest_run)

    env = sub.add_parser("contract-env", help="emit contract fields as shell assignments")
    env.add_argument("contract")
    env.set_defaults(func=contract_env)
    return root


def main() -> int:
    args = parser().parse_args()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
