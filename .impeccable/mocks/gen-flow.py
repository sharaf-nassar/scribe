#!/usr/bin/env python3
"""Emit the A3 Flow markup for beads-board-directions.html.

The wire rects are generated rather than hand-written so that every edge
provably terminates on its endpoints' dot centres -- the 9px anchoring error
this section already shipped once came from hand-placing them against the node
box centre instead.
"""

NODE_W, NODE_H = 214, 24
GUTTER, ROW_GAP = 28, 10
RANK_PITCH = NODE_W + GUTTER   # 242
ROW_PITCH = NODE_H + ROW_GAP   # 34
GRAPH_H = 139                  # 197 - band 34 - ruler 15 - hbar 2 - gap 4 - floor 3
LEFT = 30
DOT_DY = (NODE_H - 8) // 2 + 4  # dot is 8px, vertically centred -> centre at top+12
STUB = 14                       # half the gutter
SKIP_STUB = 8

def rank_x(r):
    return LEFT + r * RANK_PITCH

def row_tops(n):
    """Tops for n rows, block-centred in the graph band."""
    total = n * NODE_H + (n - 1) * ROW_GAP
    start = (GRAPH_H - total) // 2
    return [start + i * ROW_PITCH for i in range(n)]

# Lane centres: the free horizontal bands between the 4 possible row slots.
FOUR = row_tops(4)
LANES = [FOUR[i] + NODE_H + ROW_GAP // 2 for i in range(3)] + [FOUR[3] + NODE_H + 3]

def wires(nodes, edges):
    """nodes: id -> (x, top). edges: list of (blocker, dependent)."""
    out = []
    for src, dst in edges:
        x1 = nodes[src][0] + NODE_W
        y1 = nodes[src][1] + DOT_DY
        x2 = nodes[dst][0]
        y2 = nodes[dst][1] + DOT_DY
        assert x2 > x1, f"{src}->{dst} is not left-to-right"
        segs = []
        if x2 - x1 == GUTTER:
            if y1 == y2:
                segs.append((x1, y1, GUTTER, 1))
            else:
                mx = x1 + STUB
                segs.append((x1, y1, STUB, 1))
                segs.append((mx, min(y1, y2), 1, abs(y2 - y1)))
                segs.append((mx, y2, STUB, 1))
        else:
            # Skip edge: leave the rank, run a long-haul lane, re-enter.
            lane = min(LANES, key=lambda L: abs(L - (y1 + y2) / 2))
            ax, bx = x1 + SKIP_STUB, x2 - SKIP_STUB
            segs.append((x1, y1, SKIP_STUB, 1))
            segs.append((ax, min(y1, lane), 1, abs(lane - y1)))
            segs.append((ax, lane, bx - ax, 1))
            segs.append((bx, min(y2, lane), 1, abs(lane - y2)))
            segs.append((bx, y2, SKIP_STUB, 1))
        # Prove the polyline starts and ends on the dot centres.
        assert any(s[1] == y1 and s[0] == x1 for s in segs), f"{src}->{dst} tail off-dot"
        assert any(s[1] == y2 and s[0] + s[2] == x2 for s in segs), f"{src}->{dst} head off-dot"
        out.append((src, dst, segs))
    return out

def emit(wire_list, on_paths=()):
    """Union identical segments. The wire ink is semi-transparent, so a stub
    shared by two edges must be painted once or it composites brighter than a
    plain wire and reads as emphasis that is not there."""
    seen = {}
    order = []
    for src, dst, segs in wire_list:
        lit = (src, dst) in on_paths
        for seg in segs:
            if seg not in seen:
                seen[seg] = lit
                order.append(seg)
            else:
                seen[seg] = seen[seg] or lit
    lines = []
    for x, y, w, h in order:
        cls = ' class="on"' if seen[(x, y, w, h)] else ""
        lines.append(f'        <i{cls} style="left:{x}px;top:{y}px;width:{w}px;height:{h}px"></i>')
    return "\n".join(lines)

def node(cls, x, top, pri, title, ident, agent=None):
    extra = f'<span class="agent">{agent}</span>' if agent else ""
    return (f'      <article class="node {cls}" style="left:{x}px;top:{top}px;--pri:var(--{pri.lower()})">'
            f'<i class="dot"></i><span class="pri">{pri}</span>'
            f'<span class="title">{title}</span>{extra}'
            f'<span class="id">{ident}</span></article>')

# ---------------------------------------------------------------- epic A: real
# pi-ai-integration: .1 forks to .2/.3, reconverges on .6, ends at .7
one, two = row_tops(1)[0], row_tops(2)
A = {
    ".1": (rank_x(0), one),
    ".2": (rank_x(1), two[0]), ".3": (rank_x(1), two[1]),
    ".5": (rank_x(2), two[0]), ".4": (rank_x(2), two[1]),
    ".6": (rank_x(3), one),
    ".7": (rank_x(4), one),
}
A_EDGES = [(".1", ".2"), (".1", ".3"), (".2", ".5"), (".3", ".4"),
           (".5", ".6"), (".4", ".6"), (".6", ".7")]
A_TRACE = {(".1", ".3"), (".3", ".4"), (".4", ".6"), (".6", ".7")}

# --------------------------------------------------------------- epic B: real
# scribe-lpi2, the epic this very task belongs to. Eight ranks, and a frontier
# rank four wide -- four things genuinely startable at once.
four = row_tops(4)
B = {
    ".1": (rank_x(0), four[0]), ".2": (rank_x(0), four[1]),
    ".4": (rank_x(0), four[2]), ".5": (rank_x(0), four[3]),
    ".3": (rank_x(1), two[0]), ".6": (rank_x(1), two[1]),
    ".9": (rank_x(2), two[0]), ".7": (rank_x(2), two[1]),
    ".10": (rank_x(3), two[0]), ".8": (rank_x(3), two[1]),
    ".11": (rank_x(4), two[0]), ".13": (rank_x(4), two[1]),
    ".12": (rank_x(5), two[0]), ".16": (rank_x(5), two[1]),
    ".14": (rank_x(6), two[0]), ".15": (rank_x(6), two[1]),
    ".17": (rank_x(7), one),
}
B_EDGES = [(".1", ".3"), (".1", ".6"), (".1", ".7"), (".6", ".7"), (".7", ".8"),
           (".3", ".9"), (".4", ".9"), (".2", ".9"), (".9", ".10"), (".7", ".10"),
           (".10", ".11"), (".11", ".12"), (".1", ".13"), (".8", ".13"),
           (".13", ".14"), (".12", ".14"), (".12", ".15"), (".5", ".15"),
           (".11", ".16"), (".5", ".16"), (".14", ".17"), (".15", ".17"),
           (".16", ".17")]

B_META = {
    ".1": ("ready", "P1", "Flow protocol types", "lpi2.1"),
    ".2": ("live", "P1", "Revise the Flow mock", "lpi2.2"),
    ".4": ("ready", "P1", "Board colour slots", "lpi2.4"),
    ".5": ("ready", "P1", "Multi-rank fixture", "lpi2.5"),
    ".3": ("blocked", "P1", "Flow layout engine", "lpi2.3"),
    ".6": ("blocked", "P1", "Retain parent id", "lpi2.6"),
    ".9": ("blocked", "P1", "Render the Flow view", "lpi2.9"),
    ".7": ("blocked", "P1", "Assemble + admit graph", "lpi2.7"),
    ".10": ("blocked", "P1", "Flow mode and scroll", "lpi2.10"),
    ".8": ("blocked", "P1", "Focused-issue registry", "lpi2.8"),
    ".11": ("blocked", "P2", "Retarget the panel", "lpi2.11"),
    ".13": ("blocked", "P2", "issue_focused hook", "lpi2.13"),
    ".12": ("blocked", "P2", "Hover path tracing", "lpi2.12"),
    ".16": ("blocked", "P1", "Functional contract", "lpi2.16"),
    ".14": ("blocked", "P2", "Live-agent halo", "lpi2.14"),
    ".15": ("blocked", "P1", "Visual contract", "lpi2.15"),
    ".17": ("blocked", "P2", "Synchronise lat.md", "lpi2.17"),
}

A_META = {
    ".1": ("done", "P0", "Negotiate Pi provider compatibility", "2a8z.1"),
    ".2": ("done", "P1", "Promote Pi launch and restore", "2a8z.2"),
    ".3": ("done", "P1", "Build the Pi lifecycle extension", "2a8z.3"),
    ".5": ("done", "P1", "Verify shared AI behavior", "2a8z.5"),
    ".4": ("done", "P1", "Install and package extension", "2a8z.4"),
    ".6": ("live", "P2", "Prove Pi integration end to end", "2a8z.6"),
    ".7": ("blocked", "P2", "Document Pi integration", "2a8z.7"),
}

if __name__ == "__main__":
    import sys
    graph_w = max(x + NODE_W for x, _ in B.values())
    print(f"<!-- rows(4)={four} rows(2)={two} rows(1)={one} lanes={LANES} -->", file=sys.stderr)
    print(f"<!-- epic B graph width={graph_w} -->", file=sys.stderr)

    which = sys.argv[1]
    if which == "a":
        print(emit(wires(A, A_EDGES)))
        print("---NODES---")
        for k, (x, t) in A.items():
            cls, pri, title, ident = A_META[k]
            print(node(cls, x, t, pri, title, ident,
                       "codex" if cls == "live" else None))
    elif which == "atrace":
        print(emit(wires(A, A_EDGES), A_TRACE))
    elif which == "b":
        print(emit(wires(B, B_EDGES)))
        print("---NODES---")
        for k, (x, t) in B.items():
            cls, pri, title, ident = B_META[k]
            print(node(cls, x, t, pri, title, ident,
                       "codex" if cls == "live" else None))
