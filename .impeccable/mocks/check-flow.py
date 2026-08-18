#!/usr/bin/env python3
"""Check the A3 Flow section of beads-board-directions.html against its contract.

Asserts the acceptance criteria for the compact-node revision directly against
the emitted markup, so the geometry is proved rather than eyeballed.
"""
import re
import sys

NODE_W, NODE_H = 214, 24
RANK_PITCH, ROW_PITCH = 242, 34
GRAPH_TOP, GRAPH_H = 49, 139
STRIP = 197
DOT_DY = 12

src = open(sys.argv[1] if len(sys.argv) > 1 else "beads-board-directions.html").read()
a3 = src[src.index("<!-- =============================== A3 FLOW"):]
boards = re.findall(r'<div class="board fl.*?<i class="floor">', a3, re.S)
assert boards, "no Flow boards found"
fail = []


def check(cond, msg):
    print(("  ok   " if cond else "  FAIL ") + msg)
    if not cond:
        fail.append(msg)


# --- 1. the strip budget adds up, and the formula in the prose matches it ----
print("strip budget")
budget = re.search(r"band (\d+) \+ ruler (\d+) \+ graph (\d+) \+ hbar (\d+) \+ gap\s+(\d+) \+\s*floor (\d+)", a3)
total = sum(int(g) for g in budget.groups()) if budget else -1
check(total == STRIP, f"band+ruler+graph+hbar+gap+floor = {total} == {STRIP}")
check(f"node width {NODE_W} + gutter 28 = <b>{RANK_PITCH}px</b>" in a3,
      f"prose states rank pitch = {NODE_W} + 28 = {RANK_PITCH}")
check(f"node height {NODE_H} + row gap 10 = <b>{ROW_PITCH}px</b>" in a3,
      f"prose states row pitch = {NODE_H} + 10 = {ROW_PITCH}")
check(f"floor(({GRAPH_H} + 10) / {ROW_PITCH}) = <b>4</b>" in a3,
      "prose states rows-per-rank as a derivation")
scale_rows = {0.8: 5, 1.0: 4, 1.6: 2}
for s_, want in scale_rows.items():
    got = int((GRAPH_H + 10 * s_) // (NODE_H * s_ + 10 * s_))
    check(got == want, f"row budget at text scale {s_} is {got} (prose says {want})")
check("floor((139 + 10s) / 34s)" in a3, "prose states the scale-dependent row budget")
check(f"width:{NODE_W}px; height:{NODE_H}px" in src, f"node css is {NODE_W}x{NODE_H}")

# --- 2. no state can show a vertical scrollbar -------------------------------
print("vertical scrolling")
check("overflow:hidden" in re.search(r"\.fl \.graph \{[^}]*\}", src).group(0),
      ".fl .graph clips its overflow")
check(not re.search(r"\.fl [^{]*\{[^}]*overflow-y:\s*(auto|scroll)", src),
      "no overflow-y:auto/scroll anywhere under .fl")
check(not re.search(r"\.fl [^{]*\{[^}]*overflow:\s*(auto|scroll)", src),
      "no overflow:auto/scroll anywhere under .fl")

# --- 3. per-board geometry ---------------------------------------------------
max_rows_seen = 0
for bi, b in enumerate(boards, 1):
    print(f"board {bi}")
    nodes = re.findall(r'class="node ([^"]*)" style="left:(\d+)px;top:(\d+)px', b)
    check(bool(nodes), "has nodes")
    xs = sorted({int(x) for _, x, _ in nodes})
    ys = sorted({int(y) for _, _, y in nodes})

    # ranks sit on the pitch, rows sit on the pitch
    check(all((x - xs[0]) % RANK_PITCH == 0 for x in xs),
          f"every rank x is on the {RANK_PITCH}px pitch: {xs}")
    if len(ys) > 1:
        deltas = {b_ - a_ for a_, b_ in zip(ys, ys[1:])}
        check(all(d % ROW_PITCH == 0 or d == 17 for d in deltas),
              f"row offsets land on the {ROW_PITCH}px pitch: {ys}")

    # the tallest rank, and that it fits the band
    per_rank = {}
    for _, x, y in nodes:
        per_rank.setdefault(int(x), []).append(int(y))
    tallest = max(len(v) for v in per_rank.values())
    max_rows_seen = max(max_rows_seen, tallest)
    lo, hi = min(ys), max(ys) + NODE_H
    check(lo >= 0 and hi <= GRAPH_H,
          f"tallest rank is {tallest} row(s); rows span {lo}..{hi} inside the {GRAPH_H}px band")

    # every wire endpoint lands on a dot centre
    dots = {int(y) + DOT_DY for _, _, y in nodes}
    wires = [tuple(map(int, m)) for m in re.findall(
        r'<i[^>]*style="left:(\d+)px;top:(\d+)px;width:(\d+)px;height:(\d+)px"', b)]
    check(bool(wires), f"has {len(wires)} wire rects")
    horiz = [w for w in wires if w[3] == 1]
    off = [w for w in horiz if w[1] not in dots]
    lanes = {35, 69, 103, 135}
    off_real = [w for w in off if w[1] not in lanes]
    check(not off_real,
          f"every horizontal wire sits on a dot centre or a long-haul lane "
          f"({len(horiz)} rects, {len(off)} in lanes)")

    # verticals must connect two real y values (dot centres or lanes)
    vert = [w for w in wires if w[2] == 1]
    bad = [w for w in vert if w[1] not in dots | lanes or (w[1] + w[3]) not in dots | lanes]
    check(not bad, f"every vertical wire joins two anchored rows ({len(vert)} rects){' ' + str(bad) if bad else ''}")

    # wires terminate at a node's left edge / leave from its right edge
    node_l = {int(x) for _, x, _ in nodes}
    node_r = {int(x) + NODE_W for _, x, _ in nodes}
    ends = {w[0] + w[2] for w in horiz}
    starts = {w[0] for w in horiz}
    check(bool(ends & node_l), "some wire terminates on a node's left edge")
    check(bool(starts & node_r), "some wire departs from a node's right edge")

    # scroll affordance only where the graph actually overflows
    gw = max(int(x) for _, x, _ in nodes) + NODE_W
    overflows = gw > 1552
    check(('class="hbar"' in b) == overflows,
          f"graph is {gw}px wide; hbar {'present' if overflows else 'absent'} as required")
    if overflows:
        shifted = re.search(r'canvas" style="transform:translateX\((-?\d+)px\)', b)
        off_x = int(shifted.group(1)) if shifted else 0
        check(('class="fade l"' in b) == (off_x < 0),
              f"left fade {'present' if off_x < 0 else 'absent'} at scroll {off_x}")
        check('class="fade r"' in b, "right fade present")

print("summary")
check(max_rows_seen >= 3, f"at least one rank is {max_rows_seen} rows deep (>= 3)")

print()
if fail:
    print(f"FAILED {len(fail)} check(s)")
    for f in fail:
        print("  - " + f)
    sys.exit(1)
print("all checks passed")
