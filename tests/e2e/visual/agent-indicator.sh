#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-visual-agent-indicator)." >&2; exit 99; }
# @lat: [[test#Test Harness#Visual E2E Tests]]
set -euo pipefail

# shellcheck source=tests/e2e/visual/agent-visual-common.bash
. /tests/visual/agent-visual-common.bash

[ "${SCRIBE_SHARED_PANE:-0}" = "1" ] \
    || fail "agent indicator requires the shared-pane visual harness"
command -v scribe >/dev/null 2>&1 || fail "scribe CLI is absent from the visual harness"

WID=$(find_scribe_window)
[ -n "$WID" ] || fail "no Scribe window"
focus_scribe_window "$WID"
sleep 1

capture_tab() {
    local name="$1"
    shot "/output/agent-indicator-$name.png"
    convert "/output/agent-indicator-$name.png" -crop 400x34+0+0 +repage \
        "/output/agent-indicator-tab-$name.png"
}

capture_tab before
SCRIBE_SESSION_ID="$SESSION" RUST_LOG=off \
    scribe agent --agent agent-indicator-e2e read "$SESSION" \
    >/output/agent-indicator-read.json 2>/output/agent-indicator-read.stderr
ACTIVE_DELTA=0
for _ in {1..20}; do
    capture_tab active
    ACTIVE_DELTA=$(delta \
        /output/agent-indicator-tab-before.png \
        /output/agent-indicator-tab-active.png)
    [ "${ACTIVE_DELTA:-0}" -ge 8 ] && break
    sleep 0.05
done
sleep 2
capture_tab cleared

CLEAR_DELTA=$(delta /output/agent-indicator-tab-before.png /output/agent-indicator-tab-cleared.png)
[ "${ACTIVE_DELTA:-0}" -ge 8 ] \
    || fail "agent activity did not paint the leading tab glyph ($ACTIVE_DELTA changed pixels)"
[ "${CLEAR_DELTA:-0}" -lt 8 ] \
    || fail "agent activity glyph did not clear after dwell ($CLEAR_DELTA residual pixels)"
grep -q '"ok":true' /output/agent-indicator-read.json \
    || fail "the read driving the indicator failed"

echo "PASS: tab agent icon painted ($ACTIVE_DELTA px), cleared ($CLEAR_DELTA px)"
echo "  Screenshot: /output/agent-indicator-active.png"
