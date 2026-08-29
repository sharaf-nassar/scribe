#!/usr/bin/env python3
"""Check the approved Beads detail mock inventory against a real window crop."""

import argparse
import collections
import json
import re
import subprocess
from pathlib import Path


def pixels(image: Path, width: int, height: int, x: int, y: int):
    dump = subprocess.check_output(
        ["convert", str(image), "-crop", f"{width}x{height}+{x}+{y}", "+repage", "txt:-"],
        text=True,
    )
    parsed = {}
    for line in dump.splitlines():
        match = re.match(r"(\d+),(\d+): \((\d+),(\d+),(\d+)", line)
        if match:
            px, py, red, green, blue = map(int, match.groups())
            parsed[px, py] = red, green, blue
    if len(parsed) != width * height:
        raise AssertionError(f"read {len(parsed)} of {width * height} panel pixels")
    return parsed


def distance(left, right):
    return sum(abs(a - b) for a, b in zip(left, right))


def luminance(color):
    return sum(color) / 3


def ink_groups(panel, x, y, width, height):
    rows = []
    for py in range(y, y + height):
        values = [panel[px, py] for px in range(x, x + width)]
        ground = collections.Counter(values).most_common(1)[0][0]
        if sum(distance(value, ground) > 18 for value in values) > 2:
            rows.append(py)
    groups = []
    for row in rows:
        if not groups or row > groups[-1][-1] + 1:
            groups.append([row])
        else:
            groups[-1].append(row)
    return [(group[0], group[-1]) for group in groups]


def longest_run(values, ground, threshold=8):
    best = None
    start = None
    for index, value in enumerate(values + [ground]):
        if distance(value, ground) > threshold:
            start = index if start is None else start
        elif start is not None:
            run = start, index - 1
            if best is None or run[1] - run[0] > best[1] - best[0]:
                best = run
            start = None
    return best


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--mock", type=Path, required=True)
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument("--long-design-image", type=Path)
    parser.add_argument("--panel", nargs=4, type=int, metavar=("X", "Y", "W", "H"), required=True)
    parser.add_argument("--scale", type=float, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    mock = args.mock.read_text()
    width_match = re.search(r"\.detail\s*\{.*?width:(\d+)px", mock, re.S)
    if not width_match:
        raise AssertionError(f"{args.mock} declares no detail width")
    mock_width = int(width_match.group(1))
    for selector in [".spine-node", ".runin", ".d-rail", ".fepic", ".prio", ".cmt p"]:
        if selector not in mock:
            raise AssertionError(f"approved mock omitted {selector}")

    panel_x, panel_y, panel_width, panel_height = args.panel
    if panel_width != mock_width:
        raise AssertionError(f"panel width {panel_width} != mock width {mock_width}")
    if args.scale != 1.0:
        raise AssertionError(f"inventory requires text scale 1.0, got {args.scale}")
    panel = pixels(args.image, panel_width, panel_height, panel_x, panel_y)

    node = luminance(panel[17, 72])
    halo = luminance(panel[13, 72])
    node_ground = luminance(panel[23, 72])
    if node < node_ground + 60 or halo < node_ground + 4:
        raise AssertionError(
            f"spine node/halo lost: node={node:.1f} halo={halo:.1f} ground={node_ground:.1f}"
        )

    runin_colors = []
    for top in (110, 135, 160):
        runin_colors.append(len({panel[x, y] for x in range(38, 118) for y in range(top, top + 14)}))
    if min(runin_colors) < 30:
        raise AssertionError(f"run-in heads lost ink: unique colors {runin_colors}")

    rail_y = 320
    rail_row = [panel[x, rail_y] for x in range(panel_width)]
    rail_ground = collections.Counter(rail_row).most_common(1)[0][0]
    rail = longest_run(rail_row, rail_ground)
    if rail is None or rail[1] - rail[0] < 180 or rail[1] > 400:
        raise AssertionError(f"status rail run is {rail}, expected a break before x=400")
    next_ink = next(
        (x for x in range(rail[1] + 1, panel_width) if distance(rail_row[x], rail_ground) > 8),
        panel_width,
    )
    if next_ink - rail[1] < 40:
        raise AssertionError(f"status rail leaves only {next_ink - rail[1]}px before actions")

    separator_rows = []
    # The hairline renders at row 211 (measured: dominant (64,64,66) vs panel
    # ground (44,44,46)); sample the ground one row above it.
    field_ground = panel[250, 210]
    for y in range(200, 225):
        row = [panel[x, y] for x in range(33, 553)]
        dominant, count = collections.Counter(row).most_common(1)[0]
        if count > 480 and distance(dominant, field_ground) > 10:
            separator_rows.append(y)
    if separator_rows != [211]:
        raise AssertionError(f"empty fields moved the comment separator to {separator_rows}")

    priority = panel[18, 23]
    if not (priority[0] > priority[1] + 30 and priority[1] > priority[2] + 80):
        raise AssertionError(f"priority ink lost its heat hue: {priority}")
    epic = panel[448, 23]
    if not (epic[2] > epic[0] > epic[1]):
        raise AssertionError(f"epic lost its distinct hue: {epic}")

    comment_groups = ink_groups(panel, 33, 215, 520, 85)
    if len(comment_groups) != 5:
        raise AssertionError(
            f"collapsed comments painted {len(comment_groups)} text rows {comment_groups}; expected 2+1 bodies"
        )

    long_design_evidence = None
    if args.long_design_image:
        long_design = pixels(args.long_design_image, panel_width, panel_height, panel_x, panel_y)
        header_changed = sum(
            distance(panel[x, y], long_design[x, y]) > 18
            for x in range(panel_width)
            for y in range(64)
        )
        if header_changed > 25:
            raise AssertionError(
                f"long Design changed {header_changed}px in the bounded head/identity region"
            )
        body_changed = sum(
            distance(panel[x, y], long_design[x, y]) > 18
            for x in range(panel_width)
            for y in range(70, panel_height)
        )
        if body_changed < 500:
            raise AssertionError(
                f"long Design changed only {body_changed}px below the queue row"
            )
        long_design_evidence = {
            "header_changed_pixels": header_changed,
            "body_changed_pixels": body_changed,
        }

    evidence = {
        "mock": str(args.mock),
        "scale": args.scale,
        "panel": {"x": panel_x, "y": panel_y, "width": panel_width, "height": panel_height},
        "spine": {"node": node, "halo": halo, "ground": node_ground},
        "runin_unique_colors": runin_colors,
        "status_rail": {"run": rail, "next_ink": next_ink},
        "comment_separator_rows": separator_rows,
        "priority_rgb": priority,
        "epic_rgb": epic,
        "comment_text_rows": comment_groups,
        "long_design": long_design_evidence,
    }
    args.output.write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps(evidence, separators=(",", ":")))


if __name__ == "__main__":
    main()
