#!/usr/bin/env python3
"""Generate the committed A2/A3 machine contract manifest.

`beads-board-directions.html` is the approved mock for the Beads board.
Only its A2 (Ledger + rail) and A3 (Flow) sections are normative --
`specs/028-beads-board-contract.md` is the canonical prose contract, and this
script is the machine oracle that keeps the two from drifting apart. It reads
the mock's own CSS and markup -- never a second, hand-copied set of numbers --
and writes `a2a3-contract.json`: a small, deterministic manifest of the
geometry, typography, color roles, named states, and named interactions that
`check-contract.py`, Rust unit tests, and E2E shell/Python helpers can all
read instead of re-deriving or re-transcribing.

Regenerate after any edit to the mock:

    python3 .impeccable/mocks/gen-contract.py

`check-contract.py` fails if the committed manifest no longer matches a fresh
run of this script, so a mock edit with no regeneration is caught rather than
silently drifting.

Manifest shape (top-level keys):
  schema_version   -- bump on an incompatible shape change.
  source           -- mock path + sha256, for humans skimming the file.
  provenance       -- the Quill session recorded as decision provenance.
  sections         -- the normative vs. reference-only case headings, taken
                       from the mock's own <h2> text.
  colors           -- the mock theme's named color-role custom properties
                       (`--backlog`, `--p0`, ... without the leading `--`).
  fonts            -- the `--sans` / `--mono` stacks.
  states           -- one entry per required named state
                       ({section, slug, label}); label is the mock's own
                       verbatim `.state` div text.
  interactions     -- one entry per required named interaction
                       ({section, slug, trigger, evidence}).
  interactions_excluded -- controls deliberately NOT named as interactions,
                       with the closed decision that excludes them.
  geometry.a2/.a3  -- flat numeric facts (pixels, plus the drag state's own
                       source-row opacity), pre-parsed from the mock CSS,
                       markup and formula prose so consumers never parse CSS
                       shorthand themselves.
  typography.a2/.a3 -- per-role font metrics ({font_size, line_height, ...}),
                       keyed by a short descriptive role name.
"""
from __future__ import annotations

import hashlib
import json
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
MOCK_PATH = HERE / "beads-board-directions.html"
MANIFEST_PATH = HERE / "a2a3-contract.json"
SPEC_PATH = REPO_ROOT / "specs" / "028-beads-board-contract.md"
QUILL_SESSION = "01a01227-1e60-78a5-ac9d-58a63aef7ead"

# The mock's own section boundary comments, in file order. Slicing on these
# exact strings -- rather than on class names shared with normative markup --
# is what excludes CURRENT and standalone A "by construction": normative
# extraction only ever reads bytes from the a2/a3 slices below.
SECTION_MARKERS = [
    ("current", "<!-- =============================== CURRENT =============================== -->"),
    ("a", "<!-- =============================== D1 LEDGER =============================== -->"),
    ("a2", "<!-- =============================== A2 LEDGER + RAIL =============================== -->"),
    ("a3", "<!-- =============================== A3 FLOW =============================== -->"),
]
NORMATIVE_KEYS = ("a2", "a3")
REFERENCE_KEYS = ("current", "a")

STATE_PATTERN = re.compile(r'<div class="state">(.*?)</div>', re.S)

# Required named states, keyed by the section they belong to. Each candidate
# slug's pattern is matched against the mock's own verbatim `.state` label, so
# the mapping is read from the mock's prose rather than invented.
REQUIRED_STATES = {
    "A2": [
        ("collapsed", re.compile(r"\bcollapsed\b", re.I)),
        ("hover", re.compile(r"\bhover(ing)?\b", re.I)),
        ("pinned", re.compile(r"\bpinned\b", re.I)),
        ("drag", re.compile(r"\bdrag(ging)?\b", re.I)),
    ],
    "A3": [
        ("opened", re.compile(r"\bopened\b", re.I)),
        ("traced", re.compile(r"\btrac(e|es|ed|ing)\b", re.I)),
        ("deep", re.compile(r"\bdeep(er)?\b", re.I)),
        ("scrolled", re.compile(r"\bwheel(ed|s|ing)?\b", re.I)),
    ],
}

# Required named interactions. `evidence` is a literal substring asserted
# present in that section's sliced HTML -- both the proof the interaction is
# actually depicted in the mock, and the change detector if it stops being so.
#
# `.fl .switch` (the epic chevron) is deliberately absent even though its CSS
# carries `cursor:pointer`: the canonical contract closes it as inert chrome
# (no pointer cursor, hover, focus stop, or action in production), so this
# manifest must not report it as an interaction despite the literal mock CSS.
INTERACTION_SPECS = [
    ("A2", "hover-drawer", "pointer-hover|keyboard-focus", "hover to open, click to pin"),
    ("A2", "pin-drawer", "click|enter|space", "click to pin"),
    ("A2", "unpin-drawer", "click", 'class="unpin"'),
    ("A2", "drag-card", "pointer-drag", 'class="ghost"'),
    ("A3", "open-epic", "click|enter|space", "Clicking a row in Lanes"),
    ("A3", "trace-node", "pointer-hover|keyboard-focus", 'class="board fl trace"'),
    ("A3", "wheel-scroll", "wheel", "transform:translateX(-193px)"),
]

EXCLUDED_INTERACTIONS = [
    {
        "section": "A3",
        "selector": ".fl .switch",
        "reason": (
            "closed decision: epic chevron is inert chrome -- no pointer "
            "cursor, hover, focus stop, or action -- despite cursor:pointer "
            "in the mock CSS"
        ),
    },
]

FONT_SHORTHAND = re.compile(r"^(?:(\d+)\s+)?(\d+(?:\.\d+)?)px(?:/(\d+(?:\.\d+)?)(px)?)?\s+(.+)$")


class ContractError(Exception):
    """The mock no longer has the shape this generator assumes."""


def strip_css_comments(css_text: str) -> str:
    return re.sub(r"/\*.*?\*/", "", css_text, flags=re.S)


def expand_font_shorthand(value: str) -> dict[str, str]:
    """Expand a `font: [weight] size[/line-height] family` shorthand into
    discrete longhand keys, so a later rule that overrides just one longhand
    (as `.dr .pri { line-height:19px; }` does over `.d1 .pri`'s shorthand)
    merges correctly by plain dict update instead of losing to the shorthand
    it is meant to partially override."""
    m = FONT_SHORTHAND.match(value.strip())
    if not m:
        raise ContractError(f"unrecognised font shorthand: {value!r}")
    weight, size, lh, lh_px, family = m.groups()
    out = {"font-size": f"{size}px", "font-family": family}
    if weight:
        out["font-weight"] = weight
    if lh:
        out["line-height"] = f"{lh}px" if lh_px else lh
    return out


def parse_css(css_text: str) -> dict[str, dict[str, str]]:
    """Parse a flat (no nesting, no @media) stylesheet into selector ->
    declarations. A selector list (`a, b { ... }`) fans out to one entry per
    selector. A selector redeclared later in the file overlays its own
    properties onto the earlier entry, matching the cascade for two rules of
    equal specificity. `font` shorthand is expanded to longhands so that
    overlay is correct even when a later rule overrides only one longhand."""
    rules: dict[str, dict[str, str]] = {}
    for chunk in strip_css_comments(css_text).split("}"):
        chunk = chunk.strip()
        if not chunk:
            continue
        if "{" not in chunk:
            raise ContractError(f"unbalanced CSS near: {chunk[:80]!r}")
        selector_list, body = chunk.split("{", 1)
        decls: dict[str, str] = {}
        for decl in body.split(";"):
            decl = decl.strip()
            if not decl:
                continue
            if ":" not in decl:
                raise ContractError(f"malformed declaration {decl!r} in {selector_list!r}")
            prop, _, value = decl.partition(":")
            prop, value = prop.strip(), value.strip()
            if prop == "font":
                decls.update(expand_font_shorthand(value))
            else:
                decls[prop] = value
        for selector in selector_list.split(","):
            selector = selector.strip()
            rules.setdefault(selector, {}).update(decls)
    return rules


def css_of(rules: dict, selector: str) -> dict[str, str]:
    try:
        return rules[selector]
    except KeyError as exc:
        raise ContractError(f"expected CSS selector is no longer in the mock: {selector!r}") from exc


def merged(rules: dict, *selectors: str) -> dict[str, str]:
    out: dict[str, str] = {}
    for selector in selectors:
        out.update(css_of(rules, selector))
    return out


def px(value: str) -> float:
    value = value.strip()
    if value == "0":
        return 0
    m = re.match(r"^(-?\d+(?:\.\d+)?)px$", value)
    if not m:
        raise ContractError(f"expected a px length, got {value!r}")
    n = float(m.group(1))
    return int(n) if n.is_integer() else n


def px_list(value: str) -> list[float]:
    return [px(part) for part in value.split()]


def font_size_and_line_height(decls: dict[str, str]) -> tuple[float, float | None]:
    """A unitless line-height (e.g. shorthand `/1`) is not a pixel
    measurement, so it is reported as None rather than mis-parsed."""
    if "font-size" not in decls:
        raise ContractError(f"no font-size (or font shorthand) in {decls!r}")
    size = px(decls["font-size"])
    lh_raw = decls.get("line-height")
    lh = px(lh_raw) if lh_raw and lh_raw.endswith("px") else None
    return size, lh


def type_metrics(decls: dict[str, str]) -> dict:
    size, lh = font_size_and_line_height(decls)
    out: dict = {"font_size": size}
    if lh is not None:
        out["line_height"] = lh
    if "font-weight" in decls:
        out["font_weight"] = int(decls["font-weight"])
    if "letter-spacing" in decls:
        out["letter_spacing"] = decls["letter-spacing"]
    if "text-transform" in decls:
        out["text_transform"] = decls["text-transform"]
    return out


def slice_sections(html: str) -> dict[str, tuple[str, str]]:
    """Return key -> (h2 label, section text) for the four case sections,
    slicing strictly between consecutive marker comments (or EOF for the
    last). Asserts the exact marker set the mock is expected to carry, so a
    renamed/added/removed section fails loudly instead of mis-scoping."""
    offsets = []
    for key, marker in SECTION_MARKERS:
        idx = html.find(marker)
        if idx == -1:
            raise ContractError(f"expected section marker not found: {marker!r}")
        offsets.append((key, idx))
    bounds = [o for _, o in offsets] + [len(html)]
    out = {}
    for i, (key, start) in enumerate(offsets):
        text = html[start:bounds[i + 1]]
        h2 = re.search(r"<h2>([^<]*)(?:<span>)?", text)
        if not h2:
            raise ContractError(f"section {key!r} has no <h2> label")
        out[key] = (h2.group(1).strip(), text)
    return out


def require_evidence(text: str, evidence: str, where: str) -> None:
    if evidence not in text:
        raise ContractError(f"missing interaction evidence in {where}: {evidence!r}")


def extract_states(label_prefix: str, text: str) -> list[dict]:
    labels = STATE_PATTERN.findall(text)
    required = REQUIRED_STATES[label_prefix]
    entries = []
    seen_slugs: set[str] = set()
    for i, label in enumerate(labels):
        # Every state label follows the mock's own "headline \u2014 detail"
        # convention; classify against the headline alone so a detail clause
        # (e.g. "...the collapsed Done tab..." inside the drag state) cannot
        # spuriously match a second slug's keyword.
        headline = label.split("\u2014", 1)[0]
        matches = [slug for slug, pattern in required if pattern.search(headline)]
        if not matches:
            raise ContractError(f"{label_prefix} state {i} has no recognised slug: {label!r}")
        if len(matches) > 1:
            raise ContractError(f"{label_prefix} state {i} matches multiple slugs {matches}: {label!r}")
        slug = matches[0]
        if slug in seen_slugs:
            raise ContractError(f"{label_prefix} duplicate state slug {slug!r}")
        seen_slugs.add(slug)
        entries.append({"section": label_prefix, "slug": slug, "label": label})
    missing = {slug for slug, _ in required} - seen_slugs
    if missing:
        raise ContractError(f"{label_prefix} missing required state(s): {sorted(missing)}")
    if len(labels) != len(required):
        raise ContractError(f"{label_prefix} expected {len(required)} states, found {len(labels)}")
    order = {slug: i for i, (slug, _) in enumerate(required)}
    entries.sort(key=lambda e: order[e["slug"]])
    return entries


def build_interactions(section_text: dict[str, str]) -> list[dict]:
    out = []
    for section, slug, trigger, evidence in INTERACTION_SPECS:
        require_evidence(section_text[section], evidence, f"{section}/{slug}")
        out.append({"section": section, "slug": slug, "trigger": trigger, "evidence": evidence})
    return out


def a3_geometry(rules: dict, a3_text: str) -> dict:
    node = css_of(rules, ".fl .node")
    dot = css_of(rules, ".fl .node .dot")
    band = css_of(rules, ".fl .band")
    ruler = css_of(rules, ".fl .rank-ruler")
    graph = css_of(rules, ".fl .graph")
    hbar = css_of(rules, ".fl .hbar")
    floor = css_of(rules, ".fl .floor")
    floor_after = css_of(rules, ".fl .floor::after")
    prog = css_of(rules, ".fl .prog")
    fade = css_of(rules, ".fl .fade")
    chip = css_of(rules, ".fl .unlocks")
    board = css_of(rules, ".board")

    node_w, node_h = px(node["width"]), px(node["height"])
    graph_h = px(graph["height"])
    band_h = px(band["height"])
    ruler_h = px(ruler["height"])
    hbar_h = px(hbar["height"])
    floor_h = px(floor["height"])
    strip_h = px(board["height"])

    # Rank/row pitch and the gutter/row-gap they derive from are prose-only in
    # the mock (there is no literal `.gutter` CSS rule) -- read from the
    # `<p class="formula">` block the same way check-flow.py already proves
    # the prose matches the geometry, so this manifest and that checker can
    # never silently disagree about where these numbers come from.
    formula = re.search(
        r"node width (\d+) \+ gutter (\d+) = <b>(\d+)px</b>.*?"
        r"node height (\d+) \+ row gap (\d+) = <b>(\d+)px</b>",
        a3_text,
        re.S,
    )
    if not formula:
        raise ContractError("A3 formula paragraph (rank pitch / row pitch) not found")
    f_node_w, gutter, rank_pitch, f_node_h, row_gap, row_pitch = (int(g) for g in formula.groups())
    if f_node_w != node_w or f_node_h != node_h:
        raise ContractError("A3 formula prose node size disagrees with .fl .node CSS")
    if rank_pitch != node_w + gutter or row_pitch != node_h + row_gap:
        raise ContractError("A3 formula prose pitch does not equal its own stated sum")

    budget = re.search(
        r"band (\d+) \+ ruler (\d+) \+ graph (\d+) \+ hbar (\d+) \+ gap\s+(\d+) \+\s*floor (\d+)",
        a3_text,
    )
    if not budget:
        raise ContractError("A3 strip budget prose not found")
    b_band, b_ruler, b_graph, b_hbar, gap, b_floor = (int(g) for g in budget.groups())
    if (b_band, b_ruler, b_graph, b_hbar, b_floor) != (band_h, ruler_h, graph_h, hbar_h, floor_h):
        raise ContractError("A3 strip budget prose disagrees with the CSS band/ruler/graph/hbar/floor heights")
    if band_h + ruler_h + graph_h + hbar_h + gap + floor_h != strip_h:
        raise ContractError("A3 strip budget does not sum to the board height")

    left_lefts = [int(x) for x in re.findall(r'class="node[^"]*" style="left:(\d+)px', a3_text)]
    if not left_lefts:
        raise ContractError("no A3 node left offsets found to derive graph left padding")
    left_pad = min(left_lefts)

    row_capacity = {}
    for scale in (0.8, 1.0, 1.6):
        row_capacity[str(scale)] = int((graph_h + row_gap * scale) // (node_h * scale + row_gap * scale))

    band_padding = px_list(band["padding"])
    return {
        "strip_h": strip_h,
        "band_h": band_h,
        "ruler_h": ruler_h,
        "graph_h": graph_h,
        "hbar_h": hbar_h,
        "gap": gap,
        "floor_h": floor_h,
        "node_w": node_w,
        "node_h": node_h,
        "dot_size": px(dot["width"]),
        "gutter": gutter,
        "row_gap": row_gap,
        "rank_pitch": rank_pitch,
        "row_pitch": row_pitch,
        "left_pad": left_pad,
        "row_capacity": row_capacity,
        "progress_w": px(prog["width"]),
        "progress_h": px(prog["height"]),
        "fade_w": px(fade["width"]),
        "floor_grip_w": px(floor_after["width"]),
        "floor_grip_h": px(floor_after["height"]),
        "floor_grip_top": px(floor_after["top"]),
        "chip_pad_v": px_list(chip["padding"])[0],
        "chip_pad_h": px_list(chip["padding"])[1],
        "chip_radius": px(chip["border-radius"]),
        "band_pad_left": band_padding[3],
        "band_pad_right": band_padding[1],
        "band_gap": px(band["gap"]),
        "hbar_top": px(hbar["top"]),
        "graph_top": px(graph["top"]),
    }


def a2_geometry(rules: dict, a2_text: str) -> dict:
    lanes = css_of(rules, ".dr .lanes")
    zoom = css_of(rules, ".dr .zoom")
    zoom_i = css_of(rules, ".dr .zoom i")
    headband = css_of(rules, ".dr .headband")
    head = css_of(rules, ".dr .head")
    bar = css_of(rules, ".d1 .bar")
    row = merged(rules, ".d1 .row", ".dr .row")
    rows = css_of(rules, ".dr .rows")
    tab = css_of(rules, ".dr .tab")
    spine = css_of(rules, ".dr .tab .spine i")
    drawer = css_of(rules, ".dr .drawer")
    chev = css_of(rules, ".dr .chev")
    floor = css_of(rules, ".dr .floor")
    floor_after = css_of(rules, ".dr .floor::after")
    ghost = css_of(rules, ".dr .ghost")
    epic = css_of(rules, ".dr .epic")
    lane_epic = css_of(rules, ".dr .lane-epic")
    qcount = merged(rules, ".d1 .qcount", ".dr .qcount")
    board = css_of(rules, ".board")
    sub = css_of(rules, ".dr .sub")

    lanes_padding = px_list(lanes["padding"])
    row_rows = px_list(row["grid-template-rows"])
    body_h = px(rows["height"])
    row_h = px(row["height"])
    body_rows = body_h / row_h
    if body_rows != int(body_rows):
        raise ContractError(f"A2 body height {body_h} is not a whole multiple of row height {row_h}")
    _, spine_lh = font_size_and_line_height(spine)
    drawer_pad = px_list(drawer["padding"])
    ghost_pad = px_list(ghost["padding"])

    # The lifted row's own dim is an inline style on the drag state's source
    # row, not a CSS rule, so it is read from that section's markup.
    lifted = re.findall(r"opacity:(\d*\.\d+|\d+)", a2_text)
    if len(lifted) != 1:
        raise ContractError(
            f"expected exactly one dimmed A2 drag source row, found {len(lifted)}"
        )

    return {
        "strip_h": px(board["height"]),
        "lanes_padding_top": lanes_padding[0],
        "lanes_padding_right": lanes_padding[1],
        "lanes_padding_bottom": lanes_padding[2],
        "lanes_padding_left": lanes_padding[3],
        "track_gap": px(lanes["column-gap"]),
        "headband_h": px(headband["height"]),
        "head_h": px(head["height"]),
        "seam_h": px(bar["height"]),
        "row_h": row_h,
        "row_title_h": row_rows[0],
        "row_sub_h": row_rows[1],
        "row_interline_gap": px(row["row-gap"]),
        "row_priority_w": px(row["grid-template-columns"].split()[0]),
        "row_priority_gap": px(row["column-gap"]),
        "body_h": body_h,
        "body_rows": int(body_rows),
        "zoom_left": px(zoom["left"]),
        "zoom_top": px(zoom["top"]),
        "zoom_gap": px(zoom["gap"]),
        "zoom_glyph_w": px(zoom_i["width"]),
        "zoom_glyph_h": px(zoom_i["height"]),
        "tab_w": px(tab["width"]),
        "tab_spine_line_h": spine_lh,
        "drawer_top": px(drawer["top"]),
        "drawer_bottom": px(drawer["bottom"]),
        "drawer_right": px(drawer["right"]),
        "drawer_w": px(drawer["width"]),
        "drawer_pad_h": drawer_pad[-1],
        "drawer_border_w": px(drawer["border"].split()[0]),
        "drawer_radius": px(drawer["border-radius"]),
        "ghost_w": px(ghost["width"]),
        "ghost_h": px(ghost["height"]),
        "ghost_pad_right": ghost_pad[1],
        "ghost_pad_left": ghost_pad[3],
        "ghost_radius": px(ghost["border-radius"]),
        "drag_source_opacity": float(lifted[0]),
        "chev_size": font_size_and_line_height(chev)[0],
        "chev_right": px(chev["right"]),
        "chev_bottom": px(chev["bottom"]),
        "floor_h": px(floor["height"]),
        "floor_grip_w": px(floor_after["width"]),
        "floor_grip_h": px(floor_after["height"]),
        "floor_grip_top": px(floor_after["top"]),
        "epic_separation_min": px(epic["margin-left"]),
        "qcount_margin_left": qcount.get("margin-left"),
        "lane_epic_margin_left": lane_epic.get("margin-left"),
        "sub_columns": sub["grid-template-columns"],
    }


TYPOGRAPHY_ROLES = {
    "a3": [
        ("node_title", (".fl .node .title",)),
        ("node_pri", (".fl .node .pri",)),
        ("node_id", (".fl .node .id",)),
        ("epic", (".fl .epic",)),
        ("tally", (".fl .tally",)),
        ("modes", (".fl .modes span",)),
        ("back", (".fl .back",)),
        ("chip", (".fl .unlocks",)),
        ("rank_label", (".fl .rank-ruler b",)),
        ("epic_chevron", (".fl .switch",)),
    ],
    "a2": [
        ("row_title", (".d1 .title", ".dr .title")),
        ("row_pri", (".d1 .pri", ".dr .pri")),
        ("row_id", (".d1 .id",)),
        ("row_age", (".d1 .age",)),
        ("row_epic", (".d1 .epic",)),
        ("qname", (".d1 .qname",)),
        ("qcount", (".d1 .qcount", ".dr .qcount")),
        ("tab_spine", (".dr .tab .spine i",)),
        ("tab_count", (".dr .tab .tcount",)),
        ("zoom_glyph", (".dr .zoom i",)),
    ],
}


def build_typography(rules: dict) -> dict:
    return {
        section: {role: type_metrics(merged(rules, *selectors)) for role, selectors in roles}
        for section, roles in TYPOGRAPHY_ROLES.items()
    }


def build_manifest(html_path: Path = MOCK_PATH, spec_path: Path = SPEC_PATH) -> dict:
    html = html_path.read_text(encoding="utf-8")
    style_m = re.search(r"<style>(.*?)</style>", html, re.S)
    if not style_m:
        raise ContractError("no <style> block found in the mock")
    rules = parse_css(style_m.group(1))

    sections = slice_sections(html)
    section_text = {"A2": sections["a2"][1], "A3": sections["a3"][1]}

    root = css_of(rules, ":root")
    colors = {k[2:]: v for k, v in root.items() if k.startswith("--") and k not in ("--sans", "--mono")}
    fonts = {"sans": root["--sans"], "mono": root["--mono"]}

    states = extract_states("A2", sections["a2"][1]) + extract_states("A3", sections["a3"][1])
    interactions = build_interactions(section_text)

    spec_text = spec_path.read_text(encoding="utf-8")
    if QUILL_SESSION not in spec_text:
        raise ContractError(f"provenance session {QUILL_SESSION} is not recorded in {spec_path}")

    return {
        "schema_version": 1,
        "generated_by": "gen-contract.py",
        "source": {
            "path": ".impeccable/mocks/beads-board-directions.html",
            "sha256": hashlib.sha256(html.encode("utf-8")).hexdigest(),
        },
        "provenance": {
            "quill_session": QUILL_SESSION,
            "contract_spec": "specs/028-beads-board-contract.md",
        },
        "sections": {
            "normative": [sections[k][0] for k in NORMATIVE_KEYS],
            "reference_only": [sections[k][0] for k in REFERENCE_KEYS],
        },
        "colors": colors,
        "fonts": fonts,
        "states": states,
        "interactions": interactions,
        "interactions_excluded": EXCLUDED_INTERACTIONS,
        "geometry": {
            "a2": a2_geometry(rules, sections["a2"][1]),
            "a3": a3_geometry(rules, sections["a3"][1]),
        },
        "typography": build_typography(rules),
    }


def main() -> int:
    try:
        manifest = build_manifest()
    except ContractError as exc:
        print(f"gen-contract.py: {exc}", file=sys.stderr)
        return 1
    text = json.dumps(manifest, indent=2, sort_keys=True, ensure_ascii=False) + "\n"
    MANIFEST_PATH.write_text(text, encoding="utf-8")
    print(f"wrote {MANIFEST_PATH.relative_to(REPO_ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
