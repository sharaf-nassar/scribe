#!/usr/bin/env python3
"""Pixel oracles for the generated Beads A2/A3 visual contract."""

from __future__ import annotations

import argparse
import json
import math
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from beads_board_image_oracle import Image, close, contract, delta, runs, seam_tracks


class Failure(Exception):
    pass


def rgb(value: str) -> tuple[int, int, int]:
    value = value.removeprefix("#")
    return tuple(int(value[index : index + 2], 16) for index in (0, 2, 4))



def require(condition: bool, message: str) -> None:
    if not condition:
        raise Failure(message)



def role_similarity(sample: tuple[int, int, int], ground: tuple[int, int, int], role: str, data: dict) -> float:
    mock_ground = rgb(data["colors"]["ground"])
    mock_role = rgb(data["colors"][role])
    actual = [sample[i] - ground[i] for i in range(3)]
    expected = [mock_role[i] - mock_ground[i] for i in range(3)]
    actual_norm = math.sqrt(sum(value * value for value in actual))
    expected_norm = math.sqrt(sum(value * value for value in expected))
    require(actual_norm >= 12, f"{role} sample {sample} is indistinguishable from ground {ground}")
    return sum(a * b for a, b in zip(actual, expected)) / (actual_norm * expected_norm)


def assert_role(sample: tuple[int, int, int], ground: tuple[int, int, int], role: str, data: dict) -> None:
    similarity = role_similarity(sample, ground, role, data)
    require(similarity >= 0.70, f"{role} sample {sample} points away from mock role (cos={similarity:.2f})")



def assert_board_chrome(image: Image, top: int, left: int, width: int, data: dict) -> None:
    geometry = data["geometry"]["a2"]
    ground = image.pixel(left + 2, top + geometry["headband_h"] + 8)
    head_y = top + geometry["headband_h"]
    require(
        delta(image.pixel(left + geometry["lanes_padding_left"] - 8, head_y), ground) >= 15,
        "A2 header hairline is missing at the contract y",
    )
    expected_floor = top + geometry["strip_h"] - geometry["floor_h"]
    grip_runs = []
    floor_top = expected_floor
    floor = ground
    for candidate in range(round(expected_floor - 1), round(expected_floor + 2)):
        candidate_floor = image.pixel(left + 2, candidate)
        grip_y = candidate + geometry["floor_grip_top"]
        flags = [delta(image.pixel(x, grip_y), candidate_floor) > 8 for x in range(left, left + width)]
        candidate_runs = [(left + start, length) for start, length in runs(flags, 3)]
        if len(candidate_runs) == 1:
            floor_top, floor, grip_runs = candidate, candidate_floor, candidate_runs
            break
    require(delta(floor, ground) >= 8, "A2 floor does not separate from board ground")
    require(len(grip_runs) == 1, f"A2 floor has {len(grip_runs)} grip runs, expected one: {grip_runs}")
    require(abs(floor_top - expected_floor) <= 1, f"A2 floor starts at {floor_top}, expected {expected_floor}")
    grip_x, grip_width = grip_runs[0]
    require(close(grip_width, geometry["floor_grip_w"]), f"A2 grip is {grip_width}px, expected {geometry['floor_grip_w']}px")
    expected_x = left + (width - geometry["floor_grip_w"]) / 2
    require(close(grip_x, expected_x), f"A2 grip starts at {grip_x}, expected centered x={expected_x}")



def command_board_top(args: argparse.Namespace) -> None:
    before, after = Image(args.before), Image(args.after)
    require((before.width, before.height) == (after.width, after.height), "board-top images differ in size")
    strip = contract(args.contract)["geometry"]["a2"]["strip_h"]
    candidates: list[tuple[int, int]] = []
    for x in (2, 4, 6):
        flags = [delta(before.pixel(x, y), after.pixel(x, y)) > 8 for y in range(before.height)]
        candidates.extend(runs(flags, max(8, round(strip * 0.8))))
    require(candidates, "could not find the A2 strip in the before/after capture")
    top, height = min(candidates, key=lambda run: abs(run[1] - strip))
    require(close(height, strip, 1.5), f"board strip is {height}px high, expected {strip}px")
    print(top)


def command_tracks(args: argparse.Namespace) -> None:
    image = Image(args.image)
    data = contract(args.contract)
    tracks = seam_tracks(image, args.top, data, args.left, args.width)
    require(len(tracks) == 5, f"found {len(tracks)} A2 tracks, expected five: {tracks}")
    print(" ".join(f"{x}:{width}" for x, width in tracks))


def assert_priorities(image: Image, top: int, tracks: list[tuple[int, int]], data: dict, scale: float) -> None:
    geometry = data["geometry"]["a2"]
    ground = image.pixel(2, top + geometry["headband_h"] + 8)
    sites = ((0, 0, "p0"), (0, 1, "p1"), (0, 2, "p2"), (1, 0, "p3"), (1, 1, "p4"))
    content_h = (geometry["row_title_h"] + geometry["row_sub_h"] + geometry["row_interline_gap"]) * scale
    slack = (geometry["row_h"] * scale - content_h) / 2
    for track, row, role in sites:
        x = tracks[track][0]
        y = top + geometry["headband_h"] + row * geometry["row_h"] * scale + slack
        samples = [
            image.pixel(px_x, px_y)
            for px_y in range(round(y), round(y + geometry["row_title_h"] * scale))
            for px_x in range(round(x), round(x + geometry["row_priority_w"] * scale))
        ]
        similarities = [
            role_similarity(sample, ground, role, data)
            for sample in samples
            if sum((sample[index] - ground[index]) ** 2 for index in range(3)) >= 144
        ]
        require(similarities and max(similarities) >= 0.70, f"{role} priority glyph does not match its mock role")


def command_a2_layout(args: argparse.Namespace) -> None:
    image = Image(args.image)
    data = contract(args.contract)
    geometry = data["geometry"]["a2"]
    tracks = seam_tracks(image, args.top, data, args.left, args.width)
    require(len(tracks) == 5, f"found {len(tracks)} A2 tracks, expected five: {tracks}")
    require(close(tracks[0][0], args.left + geometry["lanes_padding_left"]), f"first track starts at {tracks[0][0]}")
    require(close(tracks[-1][0] + tracks[-1][1], args.left + args.width - geometry["lanes_padding_right"]), "last track does not end at contract right padding")
    gaps = [tracks[index + 1][0] - (tracks[index][0] + tracks[index][1]) for index in range(4)]
    require(all(close(gap, geometry["track_gap"]) for gap in gaps), f"A2 gaps are {gaps}, expected {geometry['track_gap']}")

    widths = [width for _, width in tracks]
    if args.mode in {"busy", "collapsed", "sparse", "auto-collapsed"}:
        require(close(widths[3], geometry["tab_w"]) and close(widths[4], geometry["tab_w"]), f"rail tabs are {widths[3:]}, expected {geometry['tab_w']}px")
    if args.mode in {"busy", "collapsed"}:
        require(max(widths[:3]) - min(widths[:3]) <= 2, f"busy active tracks are not equal: {widths[:3]}")
    if args.mode == "sparse":
        require(widths[2] > widths[0] * 2 and widths[2] > widths[1] * 2, f"sparse work lane did not receive slack: {widths[:3]}")
    if args.mode == "pinned-blocked":
        require(close(widths[4], geometry["tab_w"]), f"Done tab is {widths[4]}px")
        require(max(widths[:3]) - min(widths[:3]) <= 2, f"pinned active tracks are not equal: {widths[:3]}")
        require(abs(widths[3] / widths[0] - geometry["pinned_lane_share"]) <= 0.02, f"Blocked pin share is {widths[3] / widths[0]:.3f}")
    if args.mode == "pinned-done":
        require(close(widths[3], geometry["tab_w"]), f"Blocked tab is {widths[3]}px")
        require(max(widths[:3]) - min(widths[:3]) <= 2, f"pinned active tracks are not equal: {widths[:3]}")
        require(abs(widths[4] / widths[0] - geometry["pinned_lane_share"]) <= 0.02, f"Done pin share is {widths[4] / widths[0]:.3f}")
    if args.mode == "auto-collapsed":
        require(close(widths[3], geometry["tab_w"]) and close(widths[4], geometry["tab_w"]), "starved pin did not auto-collapse")

    ground = image.pixel(args.left + 2, args.top + geometry["headband_h"] + 8)
    for (x, _), role in zip(tracks, ("backlog", "ready", "progress", "blocked", "done")):
        assert_role(image.pixel(x, args.top + geometry["lanes_padding_top"] + geometry["head_h"]), ground, role, data)
    assert_board_chrome(image, args.top, args.left, args.width, data)
    if args.mode == "busy" and args.left == 0 and abs(args.scale - 1.0) < 0.01:
        assert_priorities(image, args.top, tracks, data, args.scale)
    print("tracks=" + ",".join(str(width) for width in widths))


def command_a2_drawer(args: argparse.Namespace) -> None:
    before, opened = Image(args.before), Image(args.opened)
    data = contract(args.contract)
    geometry = data["geometry"]["a2"]
    before_tracks = seam_tracks(before, args.top, data)
    require(len(before_tracks) == 5, f"drawer baseline has {len(before_tracks)} tracks")
    x = before.width - geometry["drawer_right"] - geometry["drawer_w"]
    stable_changed = diff_count(before, opened, 0, args.top, max(1, round(x - 10)), round(geometry["strip_h"]))
    require(stable_changed <= 100, f"opening the drawer changed {stable_changed}px before its overlay bound")
    y = args.top + geometry["drawer_top"]
    height = geometry["strip_h"] - geometry["drawer_top"] - geometry["drawer_bottom"]
    for label, px_x, px_y in (
        ("top", x + geometry["drawer_w"] / 2, y + 1),
        ("left", x + 1, y + height / 2),
        ("right", x + geometry["drawer_w"] - 2, y + height / 2),
        ("bottom", x + geometry["drawer_w"] / 2, y + height - 2),
    ):
        require(delta(before.pixel(px_x, px_y), opened.pixel(px_x, px_y)) >= 8, f"drawer {label} edge did not appear at contract bounds")
    require(delta(before.pixel(x - 8, y + height / 2), opened.pixel(x - 8, y + height / 2)) <= 8, "drawer paints beyond its left/shadow tolerance")
    print(f"drawer={geometry['drawer_w']}x{height}+{x}+{y}")


def command_a2_drawer_closed(args: argparse.Namespace) -> None:
    baseline, closed = Image(args.baseline), Image(args.closed)
    data = contract(args.contract)
    geometry = data["geometry"]["a2"]
    x = baseline.width - geometry["drawer_right"] - geometry["drawer_w"]
    y = args.top + geometry["drawer_top"]
    height = geometry["strip_h"] - geometry["drawer_top"] - geometry["drawer_bottom"]
    changed = diff_count(baseline, closed, round(x), round(y), round(geometry["drawer_w"]), round(height))
    require(changed <= 100, f"drawer left {changed}px behind after hover grace")
    require(seam_tracks(baseline, args.top, data) == seam_tracks(closed, args.top, data), "closing drawer changed A2 tracks")
    print(f"drawer-closed changed={changed}")


def diff_count(before: Image, after: Image, x: int, y: int, width: int, height: int, threshold: int = 8) -> int:
    return sum(
        delta(before.pixel(px_x, px_y), after.pixel(px_x, px_y)) > threshold
        for px_y in range(y, y + height)
        for px_x in range(x, x + width)
    )


def command_a2_row(args: argparse.Namespace) -> None:
    before, hovered = Image(args.before), Image(args.hovered)
    data = contract(args.contract)
    geometry = data["geometry"]["a2"]
    tracks = seam_tracks(before, args.top, data)
    x, width = tracks[args.track]
    row_top = round(args.top + geometry["headband_h"] + args.row * geometry["row_h"] * args.scale)
    row_height = round(geometry["row_h"] * args.scale)
    changed = diff_count(before, hovered, x, row_top, width, row_height)
    minimum = width * row_height * 0.45 if args.kind == "hover" else width * 3.5
    require(changed >= minimum, f"row {args.kind} changed only {changed}px")
    if args.kind == "hover":
        scan_height = min(before.height - row_top, max(row_height + 12, round(geometry["row_h"] * 1.7)))
        changed_rows = []
        for px_y in range(row_top, row_top + scan_height):
            row_changed = sum(
                delta(before.pixel(px_x, px_y), hovered.pixel(px_x, px_y)) > 8
                for px_x in range(x, x + width)
            )
            changed_rows.append(row_changed > width * 0.40)
        row_runs = runs(changed_rows, 2)
        require(row_runs, "row hover has no full-width vertical run")
        measured_height = row_runs[0][1]
        require(abs(measured_height - row_height) <= 2, f"row hover is {measured_height}px high, expected {row_height}px")
    above = diff_count(before, hovered, x, max(args.top, row_top - 4), width, min(4, row_top - args.top))
    below = diff_count(before, hovered, x, row_top + row_height, width, 4)
    require(above <= width and below <= width * 4, f"row treatment escaped its {row_height}px box ({above}/{below})")
    print(f"row={width}x{row_height}+{x}+{row_top} changed={changed}")


def command_a2_metadata(args: argparse.Namespace) -> None:
    image = Image(args.image)
    data = contract(args.contract)
    geometry = data["geometry"]["a2"]
    tracks = seam_tracks(image, args.top, data)
    x, width = tracks[args.track]
    row_top = args.top + geometry["headband_h"] + args.row * geometry["row_h"]
    slack = (geometry["row_h"] - geometry["row_title_h"] - geometry["row_sub_h"] - geometry["row_interline_gap"]) / 2
    sub_top = round(row_top + slack + geometry["row_title_h"] + geometry["row_interline_gap"])
    ground = image.pixel(args.left + 2, args.top + geometry["headband_h"] + 8)
    columns = {"left": 0, "center": 0, "right": 0}
    for px_x in range(x, x + width):
        ink = any(delta(image.pixel(px_x, px_y), ground) > 12 for px_y in range(sub_top, sub_top + round(geometry["row_sub_h"])))
        if not ink:
            continue
        relative = px_x - x
        if relative < width * 0.35:
            columns["left"] += 1
        if abs(relative - width / 2) <= max(18, width * 0.08):
            columns["center"] += 1
        if relative > width * 0.62:
            columns["right"] += 1
    require(all(value >= 3 for value in columns.values()), f"metadata columns missing ink: {columns}")
    print("metadata=" + json.dumps(columns, sort_keys=True))


def region_contrast(image: Image, x: int, y: int, width: int, height: int) -> int:
    pixels = [image.pixel(px_x, px_y) for px_y in range(y, y + height) for px_x in range(x, x + width)]
    counts: dict[tuple[int, int, int], int] = {}
    for color in pixels:
        counts[color] = counts.get(color, 0) + 1
    background = max(counts, key=counts.get)
    return sum(delta(color, background) for color in pixels)


def command_a2_drag(args: argparse.Namespace) -> None:
    before, dragged = Image(args.before), Image(args.dragged)
    data = contract(args.contract)
    geometry = data["geometry"]["a2"]
    x, y = args.ghost_x, args.ghost_y
    width, height = round(geometry["ghost_w"]), round(geometry["ghost_h"])
    horizontal_runs = []
    for px_y in range(max(0, y - 3), min(before.height, y + height + 3)):
        flags = [delta(before.pixel(px_x, px_y), dragged.pixel(px_x, px_y)) > 7 for px_x in range(max(0, x - 5), min(before.width, x + width + 5))]
        horizontal_runs.extend(runs(flags, max(10, width // 2)))
    require(horizontal_runs, "drag ghost has no horizontal box run")
    measured_width = min((length for _, length in horizontal_runs), key=lambda value: abs(value - width))
    require(abs(measured_width - width) <= 8, f"drag ghost is {measured_width}px wide, expected {width}px")
    vertical_runs = []
    for px_x in range(max(0, x), min(before.width, x + width)):
        flags = [delta(before.pixel(px_x, px_y), dragged.pixel(px_x, px_y)) > 7 for px_y in range(max(0, y - 5), min(before.height, y + height + 5))]
        vertical_runs.extend(runs(flags, max(8, height // 2)))
    require(vertical_runs, "drag ghost has no vertical box run")
    measured_height = min((length for _, length in vertical_runs), key=lambda value: abs(value - height))
    require(abs(measured_height - height) <= 6, f"drag ghost is {measured_height}px high, expected {height}px")

    source_before = region_contrast(before, args.source_x, args.source_y, args.source_w, args.source_h)
    source_after = region_contrast(dragged, args.source_x, args.source_y, args.source_w, args.source_h)
    require(source_after < source_before * 0.82, f"drag source did not dim ({source_before}->{source_after})")
    done_change = diff_count(before, dragged, args.done_x, args.done_y, args.done_w, args.done_h)
    blocked_change = diff_count(before, dragged, args.blocked_x, args.blocked_y, args.blocked_w, args.blocked_h)
    require(done_change > blocked_change * 1.3, f"Done target ({done_change}) did not outrank rejected Blocked ({blocked_change})")
    print(f"ghost={measured_width}x{measured_height} target={done_change}/{blocked_change}")


def floor_grip_y(image: Image, top: int, data: dict) -> int:
    geometry = data["geometry"]["a2"]
    centre = image.width // 2
    half = round(geometry["floor_grip_w"] / 2)
    start = round(top + geometry["strip_h"] - geometry["floor_h"])
    for y in range(start, image.height - 1):
        outside = image.pixel(2, y)
        inside = (centre - half + 2, centre, centre + half - 2)
        outside_grip = (centre - half - 3, centre + half + 3)
        if all(delta(image.pixel(x, y), outside) >= 8 for x in inside) and all(
            delta(image.pixel(x, y), outside) <= 5 for x in outside_grip
        ):
            return y
    raise Failure("could not find A2 floor grip")


def command_a2_resize(args: argparse.Namespace) -> None:
    before, resized = Image(args.before), Image(args.resized)
    data = contract(args.contract)
    geometry = data["geometry"]["a2"]
    before_y = floor_grip_y(before, args.top, data)
    after_y = floor_grip_y(resized, args.top, data)
    moved = after_y - before_y
    require(abs(moved - args.drag) <= 3, f"A2 floor moved {moved}px, drag was {args.drag}px")
    board_height = after_y - args.top - geometry["floor_grip_top"] + geometry["floor_h"]
    chrome = geometry["headband_h"] + geometry["lanes_padding_bottom"] + geometry["floor_h"]
    rows = math.floor((board_height - chrome) / geometry["row_h"])
    require(rows >= geometry["body_rows"] + 1, f"resized A2 fits only {rows} whole rows")
    body_end = round(args.top + geometry["headband_h"] + rows * geometry["row_h"])
    tracks = seam_tracks(resized, args.top, data)
    sample_x = tracks[0][0] + tracks[0][1] // 2
    ground = resized.pixel(2, args.top + geometry["headband_h"] + 8)
    require(delta(resized.pixel(sample_x, body_end + 4), ground) <= 10, "A2 paints a partial row into resize remainder")
    print(f"resize={moved}px rows={rows} remainder={after_y - body_end}")


def centered_tops(count: int, geometry: dict, scale: float = 1.0) -> list[float]:
    node_h = geometry["node_h"] * scale
    gap = geometry["row_gap"] * scale
    total = count * node_h + (count - 1) * gap
    first = (geometry["graph_h"] - total) / 2
    return [first + index * (node_h + gap) for index in range(count)]


def flow_position(top: int, geometry: dict, rank: int, rows: int, row: int) -> tuple[float, float]:
    return (
        geometry["left_pad"] + rank * geometry["rank_pitch"],
        top + geometry["graph_top"] + centered_tops(rows, geometry)[row],
    )


def flow_ground(image: Image, top: int, geometry: dict) -> tuple[int, int, int]:
    return image.pixel(5, top + geometry["graph_top"] + geometry["graph_h"] - 5)


def dot_site(top: int, geometry: dict, rank: int, rows: int, row: int) -> tuple[float, float]:
    x, y = flow_position(top, geometry, rank, rows, row)
    return x + geometry["node_pad_h"] + geometry["dot_size"] / 2, y + geometry["node_h"] / 2


def find_progress_bar(image: Image, top: int, geometry: dict) -> tuple[int, int, int]:
    band = image.pixel(2, top + geometry["band_h"] / 2)
    expected = round(geometry["progress_w"])
    for y in range(top, top + round(geometry["band_h"])):
        flags = [delta(image.pixel(x, y), band) > 5 for x in range(image.width)]
        for start, length in runs(flags, expected - 2):
            if abs(length - expected) <= 2:
                return start, y, length
    raise Failure(f"could not find {expected}px Flow progress bar")


def assert_flow_chrome(image: Image, top: int, data: dict, overflow: bool) -> None:
    geometry = data["geometry"]["a3"]
    ground = flow_ground(image, top, geometry)
    band = image.pixel(2, top + geometry["band_h"] / 2)
    require(delta(band, ground) >= 8, "Flow band does not separate from graph ground")
    bar_x, _, bar_width = find_progress_bar(image, top, geometry)
    require(close(bar_width, geometry["progress_w"]), f"Flow progress is {bar_width}px")
    require(bar_x >= geometry["band_pad_left"], "Flow progress starts outside band padding")
    floor_top = top + geometry["strip_h"] - geometry["floor_h"]
    floor = image.pixel(2, floor_top)
    require(delta(floor, ground) >= 8, "Flow floor does not separate from graph ground")
    hbar = hbar_thumb(image, top, geometry)
    require((hbar is not None) == overflow, f"Flow hbar presence={hbar is not None}, overflow={overflow}")


def hbar_thumb(image: Image, top: int, geometry: dict) -> tuple[int, int] | None:
    y = round(top + geometry["hbar_top"])
    floor_top = round(top + geometry["strip_h"] - geometry["floor_h"] - 1)
    band = image.pixel(2, floor_top)
    grip = image.pixel(image.width / 2, floor_top + geometry["floor_grip_top"])
    row = [image.pixel(x, y) for x in range(image.width)]
    candidates = runs([delta(color, grip) < delta(color, band) for color in row], 30)
    if not candidates:
        return None
    return max(candidates, key=lambda run: run[1])


def assert_dot(image: Image, top: int, data: dict, rank: int, rows: int, row: int, role: str, ring: bool) -> None:
    geometry = data["geometry"]["a3"]
    ground = flow_ground(image, top, geometry)
    x, y = dot_site(top, geometry, rank, rows, row)
    centre = image.pixel(x, y)
    rim = image.pixel(x - geometry["dot_size"] / 2, y)
    if ring:
        require(delta(centre, ground) <= 15, f"{role} ring centre {centre} is filled")
        assert_role(rim, ground, role, data)
    else:
        assert_role(centre, ground, role, data)


def command_flow_opened(args: argparse.Namespace) -> None:
    image = Image(args.image)
    data = contract(args.contract)
    geometry = data["geometry"]["a3"]
    assert_flow_chrome(image, args.top, data, overflow=False)
    progress_x, progress_y, progress_width = find_progress_bar(image, args.top, geometry)
    fill = image.pixel(progress_x, progress_y)
    fill_width = 0
    for px_x in range(progress_x, progress_x + progress_width):
        if delta(image.pixel(px_x, progress_y), fill) > 5:
            break
        fill_width += 1
    expected_fill = progress_width * 2 / 7
    require(abs(fill_width - expected_fill) <= 2, f"Flow progress fill is {fill_width}px, expected {expected_fill:.1f}px")
    for rank, rows, row, role, ring in (
        (0, 1, 0, "done", False),
        (1, 2, 0, "blocked", True),
        (1, 2, 1, "ready", True),
        (2, 2, 0, "progress", False),
        (2, 2, 1, "done", False),
        (3, 1, 0, "ready", True),
        (4, 1, 0, "backlog", False),
    ):
        assert_dot(image, args.top, data, rank, rows, row, role, ring)

    node_x, node_y = flow_position(args.top, geometry, 2, 2, 0)
    ground = flow_ground(image, args.top, geometry)
    require(delta(image.pixel(node_x, node_y + geometry["node_h"] / 2), ground) >= 40, "cursor keyline missing")
    require(delta(image.pixel(node_x + geometry["node_w"] - 10, node_y + geometry["node_h"] / 2), ground) >= 5, "cursor fill missing")
    require(delta(image.pixel(node_x - 2, node_y + geometry["node_h"] / 2), ground) <= 15, "cursor begins before node box")
    require(delta(image.pixel(node_x + geometry["node_w"] + 2, node_y + geometry["node_h"] / 2), ground) <= 20, "cursor extends beyond node box")

    root_x, root_y = flow_position(args.top, geometry, 0, 1, 0)
    wire_y = root_y + geometry["node_h"] / 2
    wire = image.pixel(root_x + geometry["node_w"] + 2, wire_y)
    require(delta(wire, ground) >= 18, "wire does not leave source at node centre")
    print("flow-opened geometry/state roles pass")


def trace_sites(top: int, geometry: dict) -> dict[str, tuple[float, float]]:
    cursor_x, cursor_y = flow_position(top, geometry, 2, 2, 0)
    gutter_x = geometry["left_pad"] + geometry["node_w"] + geometry["gutter"] / 2
    blocked_y = dot_site(top, geometry, 1, 2, 0)[1]
    ready_y = dot_site(top, geometry, 1, 2, 1)[1]
    return {
        "on_wire": (gutter_x, blocked_y + 5),
        "off_wire": (gutter_x, ready_y - 5),
        "off_node": (dot_site(top, geometry, 1, 2, 1)[0] - geometry["dot_size"] / 2, ready_y),
        "chip": (cursor_x + geometry["chip_offset_x"], cursor_y + geometry["node_h"] + geometry["chip_gap_y"]),
    }


def command_flow_trace(args: argparse.Namespace) -> None:
    base, traced = Image(args.base), Image(args.traced)
    data = contract(args.contract)
    geometry = data["geometry"]["a3"]
    ground = flow_ground(base, args.top, geometry)
    sites = trace_sites(args.top, geometry)
    base_on = base.pixel(*sites["on_wire"])
    traced_on = traced.pixel(*sites["on_wire"])
    traced_off = traced.pixel(*sites["off_wire"])
    require(delta(traced_on, base_on) >= 35, f"traced wire did not brighten: {base_on}->{traced_on}")
    require(delta(traced_on, traced_off) >= 45, f"traced/dim wires are not distinct: {traced_on}/{traced_off}")
    base_node = base.pixel(*sites["off_node"])
    traced_node = traced.pixel(*sites["off_node"])
    require(delta(traced_node, ground) < delta(base_node, ground) * 0.55, "off-path node did not dim toward 0.24")
    chip_x, chip_y = sites["chip"]
    require(delta(base.pixel(chip_x + 1, chip_y + 1), traced.pixel(chip_x + 1, chip_y + 1)) >= 8, "trace chip is not anchored at contract offset")
    require(delta(base.pixel(chip_x - 3, chip_y - 3), traced.pixel(chip_x - 3, chip_y - 3)) <= 8, "trace chip starts before contract offset")
    print("flow trace wires/nodes/chip pass")


def command_flow_live(args: argparse.Namespace) -> None:
    base, live = Image(args.base), Image(args.live)
    data = contract(args.contract)
    geometry = data["geometry"]["a3"]
    ground = flow_ground(base, args.top, geometry)
    x, y = dot_site(args.top, geometry, 1, 2, 1)
    require(delta(base.pixel(x, y), ground) <= 15, "idle Ready node is not hollow")
    assert_role(live.pixel(x, y), ground, "progress", data)
    halo = live.pixel(x - geometry["dot_size"] / 2 - 2, y)
    require(delta(halo, ground) >= 8, "live node has no halo outside its dot")
    idle_halo = base.pixel(x - geometry["dot_size"] / 2 - 2, y)
    require(delta(idle_halo, ground) <= 8, "idle node already has a halo")
    print("flow live halo pass")


def fade_present(image: Image, top: int, geometry: dict, side: str) -> bool:
    ground = flow_ground(image, top, geometry)
    width = round(geometry["fade_w"])
    sample_count = 7
    for y in range(round(top + geometry["graph_top"]), round(top + geometry["graph_top"] + geometry["graph_h"])):
        xs = [round(index * (width - 1) / (sample_count - 1)) for index in range(sample_count)]
        if side == "right":
            xs = [image.width - 1 - x for x in xs]
        values = [delta(image.pixel(x, y), ground) for x in xs]
        rises = sum(values[index + 1] >= values[index] + 2 for index in range(sample_count - 1))
        distinct = len({value // 4 for value in values})
        if values[0] <= 8 and values[-1] >= 20 and rises == sample_count - 1 and distinct >= 6:
            return True
    return False


def vertical_edge_run(image: Image, top: int, geometry: dict) -> int:
    ground = flow_ground(image, top, geometry)
    worst = 0
    for x in range(image.width - 6, image.width):
        current = 0
        for y in range(round(top + geometry["graph_top"]), round(top + geometry["graph_top"] + geometry["graph_h"])):
            if delta(image.pixel(x, y), ground) > 10:
                current += 1
                worst = max(worst, current)
            else:
                current = 0
    return worst


def command_flow_overflow(args: argparse.Namespace) -> None:
    image = Image(args.image)
    data = contract(args.contract)
    geometry = data["geometry"]["a3"]
    assert_flow_chrome(image, args.top, data, overflow=True)
    thumb = hbar_thumb(image, args.top, geometry)
    require(thumb is not None, "overflow state has no position thumb")
    thumb_x, thumb_width = thumb
    if args.position == "near":
        require(thumb_x <= 2, f"near thumb starts at {thumb_x}")
    elif args.position == "middle":
        require(thumb_x > 2 and thumb_x + thumb_width < image.width - 2, f"middle thumb is at edge: {thumb}")
    else:
        require(abs(thumb_x + thumb_width - image.width) <= 2, f"far thumb ends at {thumb_x + thumb_width}, width={image.width}")

    left_fade = fade_present(image, args.top, geometry, "left")
    right_fade = fade_present(image, args.top, geometry, "right")
    expected = {
        "near": (False, True),
        "middle": (True, True),
        "far": (True, False),
    }[args.position]
    if not args.skip_fades:
        require((left_fade, right_fade) == expected, f"{args.position} fades are {(left_fade, right_fade)}, expected {expected}")
    require(vertical_edge_run(image, args.top, geometry) <= 40, "Flow grew a vertical scrollbar")

    if args.position == "near":
        for row in range(4):
            x, y = dot_site(args.top, geometry, 0, 4, row)
            ground = flow_ground(image, args.top, geometry)
            mark = max(
                delta(image.pixel(x, y), ground),
                delta(image.pixel(x - geometry["dot_size"] / 2, y), ground),
            )
            require(mark >= 10, f"deep frontier row {row} has no state dot")
    print(f"flow-overflow {args.position} thumb={thumb} fades={(left_fade, right_fade)}")


def command_focus_control(args: argparse.Namespace) -> None:
    before, focused = Image(args.before), Image(args.focused)
    data = contract(args.contract)
    geometry = data["geometry"]["a3"]
    if args.control == "back":
        x, width = geometry["band_pad_left"], 85
    else:
        x, width = before.width - 150, 90
    changed = diff_count(before, focused, x, args.top, width, round(geometry["band_h"]))
    require(changed >= 30, f"{args.control} focus changed only {changed}px")
    print(f"{args.control}-focus changed={changed}")


def command_theme(args: argparse.Namespace) -> None:
    before, after = Image(args.before), Image(args.after)
    data = contract(args.contract)
    if args.surface == "a2":
        geometry = data["geometry"]["a2"]
        before_tracks = seam_tracks(before, args.top, data)
        after_tracks = seam_tracks(after, args.top, data)
        require(before_tracks == after_tracks, "theme change moved A2 geometry")
        sites = [(2, args.top + geometry["headband_h"] + 8)]
        sites += [(x, args.top + geometry["lanes_padding_top"] + geometry["head_h"]) for x, _ in before_tracks]
        sites += [(before.width / 2, args.top + geometry["strip_h"] - geometry["floor_h"])]
    else:
        geometry = data["geometry"]["a3"]
        sites = [
            (5, args.top + geometry["graph_top"] + geometry["graph_h"] - 5),
            (5, args.top + geometry["band_h"] / 2),
            dot_site(args.top, geometry, 0, 1, 0),
            dot_site(args.top, geometry, 1, 2, 0),
            dot_site(args.top, geometry, 1, 2, 1),
            flow_position(args.top, geometry, 2, 2, 0),
        ]
    static = [(x, y, before.pixel(x, y)) for x, y in sites if delta(before.pixel(x, y), after.pixel(x, y)) < 8]
    require(not static, f"theme left semantic samples unchanged: {static}")
    print(f"theme moved {len(sites)} {args.surface} semantic samples")


def command_inventory(args: argparse.Namespace) -> None:
    data = contract(args.contract)
    mapping = {
        ("A2", "collapsed"): ["a2-collapsed-sparse.png", "a2-collapsed-busy.png"],
        ("A2", "hover"): ["a2-hover-blocked.png", "a2-hover-done.png"],
        ("A2", "pinned"): ["a2-pinned-blocked.png", "a2-pinned-done.png"],
        ("A2", "drag"): ["a2-drag-done-target.png"],
        ("A3", "opened"): ["a3-opened.png"],
        ("A3", "traced"): ["a3-traced-pointer.png", "a3-traced-focus.png"],
        ("A3", "deep"): ["a3-deep-near.png"],
        ("A3", "scrolled"): ["a3-deep-middle.png", "a3-deep-far.png"],
    }
    required = {(entry["section"], entry["slug"]) for entry in data["states"]}
    require(required == set(mapping), f"inventory mapping drift: manifest={sorted(required)} mapping={sorted(mapping)}")
    extended = [
        "a2-empty.png",
        "a2-overflow.png",
        "a2-row-hover.png",
        "a2-row-focus.png",
        "a2-scale-0.8.png",
        "a2-scale-1.0.png",
        "a2-scale-1.6.png",
        "a2-resized.png",
        "a2-theme-before.png",
        "a2-theme-after.png",
        "a2-narrow.png",
        "a2-narrow-split.png",
        "a3-live.png",
        "a3-back-focus.png",
        "a3-lanes-focus.png",
        "a3-keyboard-far-focus.png",
        "a3-theme-before.png",
        "a3-theme-after.png",
    ]
    files = sorted({name for names in mapping.values() for name in names} | set(extended))
    missing = [name for name in files if not (Path(args.output) / name).is_file()]
    require(not missing, f"missing normative captures: {missing}")
    evidence = {
        "contract_source_sha256": data["source"]["sha256"],
        "reduced_motion": True,
        "named_states": {f"{section}:{slug}": names for (section, slug), names in mapping.items()},
        "extended_states": extended,
    }
    Path(args.evidence).write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"inventory={len(files)} captures")


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser()
    sub = root.add_subparsers(dest="command", required=True)

    board_top = sub.add_parser("board-top")
    board_top.add_argument("contract")
    board_top.add_argument("before")
    board_top.add_argument("after")
    board_top.set_defaults(func=command_board_top)

    tracks = sub.add_parser("tracks")
    tracks.add_argument("contract")
    tracks.add_argument("image")
    tracks.add_argument("top", type=int)
    tracks.add_argument("--left", type=int, default=0)
    tracks.add_argument("--width", type=int)
    tracks.set_defaults(func=command_tracks)

    layout = sub.add_parser("a2-layout")
    layout.add_argument("contract")
    layout.add_argument("image")
    layout.add_argument("top", type=int)
    layout.add_argument("mode", choices=["busy", "collapsed", "sparse", "pinned-blocked", "pinned-done", "auto-collapsed"])
    layout.add_argument("--left", type=int, default=0)
    layout.add_argument("--width", type=int, required=True)
    layout.add_argument("--scale", type=float, default=1.0)
    layout.set_defaults(func=command_a2_layout)

    drawer = sub.add_parser("a2-drawer")
    drawer.add_argument("contract")
    drawer.add_argument("before")
    drawer.add_argument("opened")
    drawer.add_argument("top", type=int)
    drawer.set_defaults(func=command_a2_drawer)

    drawer_closed = sub.add_parser("a2-drawer-closed")
    drawer_closed.add_argument("contract")
    drawer_closed.add_argument("baseline")
    drawer_closed.add_argument("closed")
    drawer_closed.add_argument("top", type=int)
    drawer_closed.set_defaults(func=command_a2_drawer_closed)

    row = sub.add_parser("a2-row")
    row.add_argument("contract")
    row.add_argument("before")
    row.add_argument("hovered")
    row.add_argument("top", type=int)
    row.add_argument("track", type=int)
    row.add_argument("row", type=int)
    row.add_argument("scale", type=float)
    row.add_argument("--kind", choices=["hover", "focus"], default="hover")
    row.set_defaults(func=command_a2_row)

    metadata = sub.add_parser("a2-metadata")
    metadata.add_argument("contract")
    metadata.add_argument("image")
    metadata.add_argument("top", type=int)
    metadata.add_argument("track", type=int)
    metadata.add_argument("row", type=int)
    metadata.add_argument("--left", type=int, default=0)
    metadata.set_defaults(func=command_a2_metadata)

    drag = sub.add_parser("a2-drag")
    drag.add_argument("contract")
    drag.add_argument("before")
    drag.add_argument("dragged")
    for name in ("ghost_x", "ghost_y", "source_x", "source_y", "source_w", "source_h", "done_x", "done_y", "done_w", "done_h", "blocked_x", "blocked_y", "blocked_w", "blocked_h"):
        drag.add_argument(name, type=int)
    drag.set_defaults(func=command_a2_drag)

    resize = sub.add_parser("a2-resize")
    resize.add_argument("contract")
    resize.add_argument("before")
    resize.add_argument("resized")
    resize.add_argument("top", type=int)
    resize.add_argument("drag", type=int)
    resize.set_defaults(func=command_a2_resize)

    flow_opened = sub.add_parser("flow-opened")
    flow_opened.add_argument("contract")
    flow_opened.add_argument("image")
    flow_opened.add_argument("top", type=int)
    flow_opened.set_defaults(func=command_flow_opened)

    trace = sub.add_parser("flow-trace")
    trace.add_argument("contract")
    trace.add_argument("base")
    trace.add_argument("traced")
    trace.add_argument("top", type=int)
    trace.set_defaults(func=command_flow_trace)

    live = sub.add_parser("flow-live")
    live.add_argument("contract")
    live.add_argument("base")
    live.add_argument("live")
    live.add_argument("top", type=int)
    live.set_defaults(func=command_flow_live)

    overflow = sub.add_parser("flow-overflow")
    overflow.add_argument("contract")
    overflow.add_argument("image")
    overflow.add_argument("top", type=int)
    overflow.add_argument("position", choices=["near", "middle", "far"])
    overflow.add_argument("--skip-fades", action="store_true")
    overflow.set_defaults(func=command_flow_overflow)

    focus = sub.add_parser("focus-control")
    focus.add_argument("contract")
    focus.add_argument("before")
    focus.add_argument("focused")
    focus.add_argument("top", type=int)
    focus.add_argument("control", choices=["back", "lanes"])
    focus.set_defaults(func=command_focus_control)

    theme = sub.add_parser("theme")
    theme.add_argument("contract")
    theme.add_argument("before")
    theme.add_argument("after")
    theme.add_argument("top", type=int)
    theme.add_argument("surface", choices=["a2", "a3"])
    theme.set_defaults(func=command_theme)

    inventory = sub.add_parser("inventory")
    inventory.add_argument("contract")
    inventory.add_argument("output")
    inventory.add_argument("evidence")
    inventory.set_defaults(func=command_inventory)
    return root


def main() -> int:
    args = parser().parse_args()
    try:
        args.func(args)
    except (Failure, subprocess.CalledProcessError, OSError, ValueError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
