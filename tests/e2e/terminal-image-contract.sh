#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[terminal-images#Terminal Images#Contract Verification]]
set -euo pipefail

CONTRACT=/tests/fixtures/terminal-images/contract.json
ROOT=/tests
OUTPUT=/output/terminal-images/contract.json

python3 - "$CONTRACT" "$ROOT" "$OUTPUT" <<'PY'
import json
import os
import pathlib
import re
import sys

contract_path = pathlib.Path(sys.argv[1])
root = pathlib.Path(sys.argv[2]).resolve()
output = pathlib.Path(sys.argv[3])

with contract_path.open(encoding="utf-8") as handle:
    contract = json.load(handle)

if contract.get("schema_version") != 1:
    raise SystemExit("FAIL: terminal-image contract schema version drifted")
if contract.get("contract_version") != "terminal-images-v1":
    raise SystemExit("FAIL: unsupported terminal-image contract version")

fixtures = contract.get("fixtures")
if not isinstance(fixtures, list) or not fixtures:
    raise SystemExit("FAIL: terminal-image contract has no fixture registry")

owned = (root / "fixtures/terminal-images").resolve()
ids = set()
paths = set()
for fixture in fixtures:
    fixture_id = fixture.get("id")
    relative = fixture.get("path")
    expectation = fixture.get("expect")
    if not isinstance(fixture_id, str) or not fixture_id or fixture_id in ids:
        raise SystemExit(f"FAIL: duplicate or empty fixture id {fixture_id!r}")
    if not isinstance(relative, str) or relative in paths:
        raise SystemExit(f"FAIL: duplicate or invalid fixture path {relative!r}")
    if fixture.get("encoding") != "hex":
        raise SystemExit(f"FAIL: fixture {fixture_id} is not ASCII hex")
    if not isinstance(expectation, str) or not expectation:
        raise SystemExit(f"FAIL: fixture {fixture_id} has no expectation")

    path = (root / relative).resolve()
    if path.parent != owned or path.suffix != ".hex":
        raise SystemExit(f"FAIL: fixture escapes owned directory: {relative}")
    try:
        text = path.read_text(encoding="ascii").strip()
    except OSError as error:
        raise SystemExit(f"FAIL: cannot read fixture {relative}: {error}") from error
    if not text or len(text) % 2 or re.fullmatch(r"[0-9a-f]+", text) is None:
        raise SystemExit(f"FAIL: fixture is not lowercase even-length ASCII hex: {relative}")

    ids.add(fixture_id)
    paths.add(relative)

output.parent.mkdir(parents=True, exist_ok=True)
temporary = output.with_name(output.name + ".tmp")
temporary.write_bytes(contract_path.read_bytes())
os.replace(temporary, output)
PY

echo "PASS: terminal image contract and fixture registry published at $OUTPUT"
