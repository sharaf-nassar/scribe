#!/usr/bin/env python3
"""Check A2/A3 mock evidence, coverage ownership, and production drift.

Regenerates the manifest from the current mock (see `gen-contract.py`) and
fails if its committed evidence is stale, a normative state/interaction lacks
an owner oracle, production restates contract geometry, or A2 returns to the
retired raised-card/five-equal-track grammar.

Usage: python3 .impeccable/mocks/check-contract.py
"""
import importlib.util
import json
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
GEN_PATH = HERE / "gen-contract.py"
MANIFEST_PATH = HERE / "a2a3-contract.json"
CHECK_FLOW_PATH = HERE / "check-flow.py"
HTML_PATH = HERE / "beads-board-directions.html"
SPEC_PATH = REPO_ROOT / "specs" / "028-beads-board-contract.md"
JUSTFILE_PATH = REPO_ROOT / "justfile"
VISUAL_ORACLE_PATH = REPO_ROOT / "tests" / "e2e" / "visual" / "beads-board-oracle.py"
VISUAL_SCRIPT_PATH = REPO_ROOT / "tests" / "e2e" / "visual" / "beads-board.sh"
FUNCTIONAL_SCRIPT_PATH = REPO_ROOT / "tests" / "e2e" / "func" / "beads-board.sh"
BOARD_SOURCE_PATH = REPO_ROOT / "crates" / "scribe-client" / "src" / "beads_board.rs"
FLOW_SOURCE_PATH = REPO_ROOT / "crates" / "scribe-client" / "src" / "beads_flow.rs"
A2_SOURCE_PATH = REPO_ROOT / "crates" / "scribe-client" / "src" / "beads_board_a2.rs"

COVERAGE_ID = re.compile(r"(?:SCOPE-\d+|A[23]-[A-Z]+\d+)")
OWNER = re.compile(r"`scribe-[a-z0-9]+(?:\.\d+)?`")


def load_generator():
    spec = importlib.util.spec_from_file_location("gen_contract", GEN_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def report(ok: bool, msg: str, failures: list[str]) -> None:
    print(("ok    " if ok else "FAIL  ") + msg)
    if not ok:
        failures.append(msg)


def read(path: Path, failures: list[str]) -> str:
    if not path.is_file():
        report(False, f"required oracle source is missing: {path.relative_to(REPO_ROOT)}", failures)
        return ""
    return path.read_text(encoding="utf-8")


def coverage_rows(spec: str) -> dict[str, tuple[str, str, str]]:
    rows = {}
    for line in spec.splitlines():
        if not line.startswith("| ") or line.startswith("| ---"):
            continue
        columns = [column.strip() for column in line.strip().strip("|").split("|")]
        if len(columns) != 4 or not COVERAGE_ID.fullmatch(columns[0]):
            continue
        rows[columns[0]] = (columns[1], columns[2], columns[3])
    return rows


def check_coverage(fresh: dict, failures: list[str]) -> dict[str, tuple[str, str, str]]:
    print("coverage ownership")
    rows = coverage_rows(read(SPEC_PATH, failures))
    report(bool(rows), "028 coverage map contains normative rows", failures)
    missing_owners = [row_id for row_id, (_, owner, _) in rows.items() if not OWNER.fullmatch(owner)]
    report(
        not missing_owners,
        "coverage rows name one owner bead" + (f": {sorted(missing_owners)}" if missing_owners else ""),
        failures,
    )
    missing_oracles = [row_id for row_id, (_, _, oracle) in rows.items() if not oracle]
    report(
        not missing_oracles,
        "coverage rows name an owner oracle" + (f": {sorted(missing_oracles)}" if missing_oracles else ""),
        failures,
    )

    for section in ("A2", "A3"):
        states = [state for state in fresh["states"] if state["section"] == section]
        state_rows = [f"{section}-S{index}" for index in range(1, len(states) + 1)]
        for state, row_id in zip(states, state_rows, strict=True):
            oracle = rows.get(row_id, ("", "", ""))[2]
            report(
                "visual" in oracle.lower(),
                f"{section}:{state['slug']} is owned by visual oracle {row_id}",
                failures,
            )

    for interaction in fresh["interactions"]:
        section = interaction["section"]
        token = interaction["slug"].split("-", 1)[0]
        matches = [
            row_id
            for row_id, (requirement, _, oracle) in rows.items()
            if row_id.startswith(f"{section}-I") and token in f"{requirement} {oracle}".lower()
        ]
        report(
            bool(matches),
            f"{section}:{interaction['slug']} is owned by interaction oracle"
            + (f" {', '.join(matches)}" if matches else ""),
            failures,
        )
    return rows


def oracle_tiers(row_id: str, oracle: str) -> set[str]:
    lower = oracle.lower()
    tiers = set()
    # Row families define the minimum proof tier; prose may add another.
    # This is derived from the canonical coverage IDs, not a second mapping.
    if re.fullmatch(r"A[23]-(?:S|C)\d+", row_id) or "visual" in lower:
        tiers.add("visual")
    if re.fullmatch(r"A[23]-(?:I|L|R|BD)\d+", row_id) or "functional" in lower or "real-`bd`" in lower:
        tiers.add("functional")
    if "machine" in lower or "check-flow.py" in lower or "stale-contract" in lower:
        tiers.add("machine")
    if any(token in lower for token in ("pure", "unit", "headless", "protocol", "layout", "contrast", "accesskit", "test", "fixture", "matrix")):
        tiers.add("rust")
    if "lat check" in lower:
        tiers.add("docs")
    return tiers


def check_owner_tiers(rows: dict[str, tuple[str, str, str]], failures: list[str]) -> None:
    """Require each planned oracle tier without a second coverage checklist."""
    print("coverage oracle tiers")
    ready = {
        "visual": (
            contains(VISUAL_SCRIPT_PATH, 'python3 "$ORACLE" inventory')
            and contains(VISUAL_ORACLE_PATH, "def command_inventory"),
            "tests/e2e/visual/beads-board.sh capture inventory",
        ),
        "functional": (
            contains(FUNCTIONAL_SCRIPT_PATH, "SCRIBE_A2A3_CONTRACT", '"$IMAGE_ORACLE" contract-env'),
            "tests/e2e/func/beads-board.sh real-bd interaction inventory",
        ),
        "rust": (
            contains(
                A2_SOURCE_PATH,
                'include_str!("../../../.impeccable/mocks/a2a3-contract.json")',
                "fn constants_match_the_generated_contract()",
            ),
            "crates/scribe-client/src/beads_board_a2.rs generated-contract test",
        ),
        "machine": (CHECK_FLOW_PATH.is_file() and GEN_PATH.is_file(), "machine contract checker"),
        "docs": ((REPO_ROOT / "lat.md" / "test.md").is_file(), "lat.md test contract"),
    }
    unknown = []
    for row_id, (_, _, oracle) in rows.items():
        tiers = oracle_tiers(row_id, oracle)
        if not tiers:
            unknown.append(row_id)
            continue
        for tier in sorted(tiers):
            if not ready[tier][0]:
                report(False, f"{row_id}: expected {ready[tier][1]} for planned oracle", failures)
    report(not unknown, f"coverage rows have known oracle tiers: {unknown}", failures)


def contains(path: Path, *needles: str) -> bool:
    return path.is_file() and all(needle in path.read_text(encoding="utf-8") for needle in needles)


def check_e2e_inventory(fresh: dict, failures: list[str]) -> None:
    print("E2E ownership")
    visual_oracle = read(VISUAL_ORACLE_PATH, failures)
    visual_script = read(VISUAL_SCRIPT_PATH, failures)
    functional_script = read(FUNCTIONAL_SCRIPT_PATH, failures)
    justfile = read(JUSTFILE_PATH, failures)
    report(
        "def command_inventory" in visual_oracle and "required == set(mapping)" in visual_oracle,
        "visual inventory derives required named states from the manifest",
        failures,
    )
    report(
        'python3 "$ORACLE" inventory' in visual_script,
        "visual suite validates its capture inventory",
        failures,
    )
    report(
        "SCRIBE_A2A3_CONTRACT" in functional_script and '"$IMAGE_ORACLE" contract-env' in functional_script,
        "functional suite retains its manifest-backed interaction inventory",
        failures,
    )
    report(
        "e2e-visual-beads-board:" in justfile and "e2e-func-beads-board:" in justfile,
        "both A2/A3 E2E recipes remain registered",
        failures,
    )


def rust_without_comments(path: Path, failures: list[str]) -> str:
    source = read(path, failures)
    source = re.sub(r"/\*.*?\*/", "", source, flags=re.S)
    return re.sub(r"//.*", "", source)


def check_drift(failures: list[str]) -> None:
    print("production drift")
    board = rust_without_comments(BOARD_SOURCE_PATH, failures)
    flow = rust_without_comments(FLOW_SOURCE_PATH, failures)
    shell = read(VISUAL_SCRIPT_PATH, failures) + "\n" + read(FUNCTIONAL_SCRIPT_PATH, failures)

    report(
        not re.search(r"\bBEADS_BOARD_HEIGHT\s*:\s*f32\s*=\s*\d", board),
        "board height is imported from the manifest-backed Rust bridge",
        failures,
    )
    flow_metric_literals = re.search(
        r"\b(node_width|node_height|gutter|row_gap|graph_height|left_padding)\s*:\s*\d",
        flow,
    )
    report(
        flow_metric_literals is None,
        "Flow layout geometry is imported from the manifest-backed Rust bridge",
        failures,
    )
    shell_constant = re.search(r"(?m)^\s*A[23]_[A-Z0-9_]+\s*=\s*\d", shell)
    report(
        shell_constant is None,
        "A2/A3 shell geometry is read from the generated manifest",
        failures,
    )

    reference_markers = ("CURRENT", "A · Ledger", "D1 LEDGER")
    a2 = rust_without_comments(A2_SOURCE_PATH, failures)
    leaked_reference = [marker for marker in reference_markers if marker in board or marker in flow or marker in a2]
    report(
        not leaked_reference,
        "reference-only CURRENT and standalone A markers stay out of production"
        + (f": {leaked_reference}" if leaked_reference else ""),
        failures,
    )
    raised_card = re.search(r"\.bg\(colors\.card\)|border_color\(colors\.card_border\)", board)
    report(
        raised_card is None,
        "ledger rows do not restore raised-card paint markers",
        failures,
    )
    equal_tracks = re.search(r"/\s*5(?:\.0)?\b|\*\s*0\.2\b", board)
    report(
        equal_tracks is None,
        "ledger production code does not divide the board into five equal tracks",
        failures,
    )


def main() -> int:
    failures: list[str] = []
    gen = load_generator()

    print("regeneration")
    try:
        fresh = gen.build_manifest()
    except gen.ContractError as exc:
        report(False, f"contract extraction: {exc}", failures)
        print()
        print(f"FAILED {len(failures)} check(s)")
        return 1

    if not MANIFEST_PATH.exists():
        report(False, f"{MANIFEST_PATH} is missing; run gen-contract.py", failures)
    else:
        committed = json.loads(MANIFEST_PATH.read_text(encoding="utf-8"))
        report(
            fresh == committed,
            "a2a3-contract.json matches a fresh generation (not stale)",
            failures,
        )
        if fresh != committed:
            for key in sorted(set(fresh) | set(committed)):
                if fresh.get(key) != committed.get(key):
                    print(f"      - {key!r} differs from the committed manifest")

    print("section scope")
    normative = set(fresh["sections"]["normative"])
    reference = set(fresh["sections"]["reference_only"])
    report(
        normative == {"A2 · Ledger + rail", "A3 · Flow"},
        f"normative sections are exactly A2 and A3: {sorted(normative)}",
        failures,
    )
    report(
        reference == {"Current", "A · Ledger"} and not (reference & normative),
        f"reference-only sections excluded: {sorted(reference)}",
        failures,
    )

    print("named states")
    required_states = {
        "A2": {"collapsed", "hover", "pinned", "drag"},
        "A3": {"opened", "traced", "deep", "scrolled"},
    }
    for section, want in required_states.items():
        got = {state["slug"] for state in fresh["states"] if state["section"] == section}
        report(got == want, f"{section} states present: {sorted(got)}", failures)

    print("named interactions")
    required_interactions = {
        "A2": {"hover-drawer", "pin-drawer", "unpin-drawer", "drag-card"},
        "A3": {"open-epic", "trace-node", "wheel-scroll"},
    }
    for section, want in required_interactions.items():
        got = {item["slug"] for item in fresh["interactions"] if item["section"] == section}
        report(want <= got, f"{section} interactions present: {sorted(got)}", failures)

    print("geometry formulas")
    a3 = fresh["geometry"]["a3"]
    report(a3["rank_pitch"] == a3["node_w"] + a3["gutter"], "A3 rank pitch = node width + gutter", failures)
    report(a3["row_pitch"] == a3["node_h"] + a3["row_gap"], "A3 row pitch = node height + row gap", failures)
    report(
        a3["band_h"] + a3["ruler_h"] + a3["graph_h"] + a3["hbar_h"] + a3["gap"] + a3["floor_h"] == a3["strip_h"],
        "A3 strip budget sums to the 197px board height",
        failures,
    )
    report(
        a3["row_capacity"] == {"0.8": 5, "1.0": 4, "1.6": 2},
        f"A3 row capacity per text scale: {a3['row_capacity']}",
        failures,
    )
    a2 = fresh["geometry"]["a2"]
    report(a2["body_h"] == a2["row_h"] * a2["body_rows"], "A2 body height = row height * row count", failures)
    report(a2["body_rows"] == 3, "A2 default body is exactly three rows", failures)
    report(a2["pinned_lane_share"] == 0.85, "A2 pinned lane uses the mock's 0.85 share", failures)
    report(a3["viewport_w"] == 1552, "A3 mock viewport is the 1552px strip", failures)
    report(
        a3["node_pad_h"] == 6 and a3["node_gap"] == 6,
        "A3 node horizontal padding and gap are 6px",
        failures,
    )
    report(
        a3["chip_offset_x"] == 14 and a3["chip_gap_y"] == 6,
        "A3 trace chip keeps its 14px/6px node anchor",
        failures,
    )

    rows = check_coverage(fresh, failures)
    check_owner_tiers(rows, failures)
    check_e2e_inventory(fresh, failures)
    check_drift(failures)

    print("existing check-flow.py suite")
    result = subprocess.run(
        [sys.executable, str(CHECK_FLOW_PATH), str(HTML_PATH)],
        capture_output=True,
        text=True,
        check=False,
    )
    for line in result.stdout.splitlines():
        print("      " + line)
    if result.returncode != 0:
        for line in result.stderr.splitlines():
            print("      " + line)
    report(result.returncode == 0, "check-flow.py passes", failures)

    print()
    if failures:
        print(f"FAILED {len(failures)} check(s)")
        for failure in failures:
            print("  - " + failure)
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
