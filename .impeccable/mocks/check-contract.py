#!/usr/bin/env python3
"""Check beads-board-directions.html against the committed A2/A3 contract.

Regenerates the manifest from the current mock (see `gen-contract.py`) and
fails if:

  - the committed `a2a3-contract.json` is stale (does not byte-for-byte match
    a fresh generation -- catches a mock edit with no regeneration, and any
    normative geometry change along with it);
  - a required named A2/A3 state or interaction is missing;
  - a reference-only section (CURRENT, standalone A) leaks into the
    normative set;
  - the A3 geometry formulas (rank/row pitch, strip budget, row capacity per
    text scale) no longer hold;
  - the existing `check-flow.py` suite regresses.

Usage: python3 .impeccable/mocks/check-contract.py
"""
import importlib.util
import json
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
GEN_PATH = HERE / "gen-contract.py"
MANIFEST_PATH = HERE / "a2a3-contract.json"
CHECK_FLOW_PATH = HERE / "check-flow.py"
HTML_PATH = HERE / "beads-board-directions.html"


def load_generator():
    spec = importlib.util.spec_from_file_location("gen_contract", GEN_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def report(ok: bool, msg: str, failures: list[str]) -> None:
    print(("ok    " if ok else "FAIL  ") + msg)
    if not ok:
        failures.append(msg)


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
        committed = None
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
        got = {s["slug"] for s in fresh["states"] if s["section"] == section}
        report(got == want, f"{section} states present: {sorted(got)}", failures)

    print("named interactions")
    required_interactions = {
        "A2": {"hover-drawer", "pin-drawer", "unpin-drawer", "drag-card"},
        "A3": {"open-epic", "trace-node", "wheel-scroll"},
    }
    for section, want in required_interactions.items():
        got = {i["slug"] for i in fresh["interactions"] if i["section"] == section}
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
        for f in failures:
            print("  - " + f)
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
