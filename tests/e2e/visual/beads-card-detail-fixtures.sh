#!/bin/bash
# e2e-timeout: 90
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: Docker E2E only" >&2; exit 99; }
# @lat: [[test#Test Harness#Visual E2E Tests#Beads card-detail fixtures]]
set -euo pipefail

CONTROL="${SHARE_TAP_CONTROL:-$XDG_RUNTIME_DIR/scribe/share-tap.sock}"
RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
MOCK="${BEADS_DETAIL_MOCK:-/mocks/beads-card-detail.html}"

fail() {
    echo "FAIL: $1"
    tail -60 "$CLIENT_LOG" 2>/dev/null || true
    exit 1
}

window_id() {
    xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1
}

inject() {
    scribe-test share-inject --control "$CONTROL" "$1"
    sleep 0.5
}

first_workspace() {
    python3 - "$RECORD" <<'PY'
import json, sys

found = None
with open(sys.argv[1]) as handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        message = row.get("message", {})
        if row.get("dir") == "client" and message.get("type") == "CreateSession":
            found = message["workspace_id"]
if found:
    print(found)
else:
    raise SystemExit(1)
PY
}

detail_request_count() {
    python3 - "$RECORD" "${1:-}" <<'PY'
import json, sys

count = 0
with open(sys.argv[1]) as handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        message = row.get("message", {})
        if (row.get("dir") == "client"
                and message.get("type") == "RequestBeadsIssueDetail"
                and (not sys.argv[2] or message.get("issue_id") == sys.argv[2])):
            count += 1
print(count)
PY
}

sleep 2
[ -r "$MOCK" ] || fail "approved mock is not mounted at $MOCK"
WID=$(window_id)
[ -n "$WID" ] || fail "no Scribe window"
WORKSPACE=$(first_workspace) || fail "no SessionList workspace recorded"
xdotool windowactivate --sync "$WID" 2>/dev/null || xdotool windowfocus --sync "$WID"
inject "{\"type\":\"WorkspaceInfo\",\"workspace_id\":\"$WORKSPACE\",\"name\":\"scribe\",\"accent_color\":\"#a78bfa\",\"split_direction\":null,\"project_root\":null}"
import -window "$WID" /output/beads-detail-before.png

# Each fixture is already a complete ServerMessage. The runtime workspace id is
# the only substitution. Loading sends a board and withholds the detail reply.
DETAIL_MESSAGES=$(python3 - /tests/fixtures/beads-card-detail.json "$WORKSPACE" <<'PY'
import json, sys, time

with open(sys.argv[1]) as handle:
    fixtures = json.load(handle)
expected = ["loading", "closed", "blocked", "comment-clamped", "hidden-count"]
names = [fixture["name"] for fixture in fixtures]
if names != expected:
    raise SystemExit(f"detail fixture names {names!r} != {expected!r}")
for fixture in fixtures:
    message = fixture["message"]
    message["workspace_id"] = sys.argv[2]
    if message["type"] == "BeadsBoard":
        message["state"]["Ready"]["snapshot"]["refreshed_at_epoch_ms"] = int(time.time() * 1000)
    print(f'{fixture["name"]}\t{json.dumps(message, separators=(",", ":"))}')
PY
)

DETAIL_DROPS_BEFORE=$(grep -c \
    'server message not wired into the GPUI client.*BeadsIssueDetail' \
    "$CLIENT_LOG" 2>/dev/null || true)
while IFS=$'\t' read -r variant message; do
    [ -n "$variant" ] && [ -n "$message" ] || fail "empty card-detail fixture row"
    inject "$message"
done <<<"$DETAIL_MESSAGES"
DETAIL_DROPS_AFTER=$(grep -c \
    'server message not wired into the GPUI client.*BeadsIssueDetail' \
    "$CLIENT_LOG" 2>/dev/null || true)
[ "$(( DETAIL_DROPS_AFTER - DETAIL_DROPS_BEFORE ))" -eq 0 ] \
    || fail "client dropped $(( DETAIL_DROPS_AFTER - DETAIL_DROPS_BEFORE )) handled detail fixtures"
if grep -q 'could not decode an injection' /output/share-tap.log; then
    fail "share-tap rejected a card-detail fixture"
fi

# The real no-project reply may race the matrix above, so restore the fixture
# board immediately before the pointer sequence that consumes it.
LOADING_MESSAGE=$(printf '%s\n' "$DETAIL_MESSAGES" | awk -F '\t' '
    $1 == "loading" { sub(/^[^\t]*\t/, ""); print; exit }
')
[ -n "$LOADING_MESSAGE" ] || fail "loading fixture missing"
inject "$LOADING_MESSAGE"
# Hovering its badge must reveal the five fixture cards.
xdotool mousemove --sync --window "$WID" 66 17
sleep 0.5
import -window "$WID" /output/beads-detail-loading.png
LOADING_DIFF=$(compare -metric AE /output/beads-detail-before.png \
    /output/beads-detail-loading.png null: 2>&1 || true)
LOADING_DIFF=${LOADING_DIFF%%.*}
[ "${LOADING_DIFF:-0}" -ge 10000 ] \
    || fail "loading fixture board changed only ${LOADING_DIFF:-0}px"

# Open the first Backlog card, then answer its request with the matching detail.
# The recovered mock fixes the collapsed thread at two lines for the newest
# comment and one line for every older comment; unit coverage pins those line
# limits while this capture proves the panel is actually lowered in the window.
COMMENT_MESSAGE=$(printf '%s\n' "$DETAIL_MESSAGES" | awk -F '\t' '
    $1 == "comment-clamped" { sub(/^[^\t]*\t/, ""); print; exit }
')
[ -n "$COMMENT_MESSAGE" ] || fail "comment-clamped fixture missing"
REQUESTS_BEFORE=$(detail_request_count)
xdotool mousemove --sync --window "$WID" 80 84
xdotool click 1
sleep 0.2
[ "$(detail_request_count)" -eq "$((REQUESTS_BEFORE + 1))" ] \
    || fail "card click did not send its first detail request"
import -window "$WID" /output/beads-detail-loading-panel.png
LOADING_PANEL_DIFF=$(compare -metric AE /output/beads-detail-loading.png \
    /output/beads-detail-loading-panel.png null: 2>&1 || true)
LOADING_PANEL_DIFF=${LOADING_PANEL_DIFF%%.*}
[ "${LOADING_PANEL_DIFF:-0}" -ge 20000 ] \
    || fail "loading head and placeholder changed only ${LOADING_PANEL_DIFF:-0}px"
inject "$COMMENT_MESSAGE"
sleep 0.5
# Keep pointer effects outside the measured panel inventory.
xdotool mousemove --sync --window "$WID" 700 500
sleep 0.2
import -window "$WID" /output/beads-detail-comment-clamped.png
PANEL_DIFF=$(compare -metric AE /output/beads-detail-loading.png \
    /output/beads-detail-comment-clamped.png null: 2>&1 || true)
PANEL_DIFF=${PANEL_DIFF%%.*}
[ "${PANEL_DIFF:-0}" -ge 30000 ] \
    || fail "detail panel changed only ${PANEL_DIFF:-0}px"
read -r PANEL_W PANEL_H PANEL_X PANEL_Y <<<"$(convert \
    /output/beads-detail-loading.png /output/beads-detail-comment-clamped.png \
    -compose difference -composite -crop "600x400+0+231" +repage \
    -threshold 10% -trim -format '%w %h %X %Y' info:)"
[ "$PANEL_W" -eq 560 ] && [ "$PANEL_X" = "+12" ] && [ "$PANEL_Y" = "+4" ] \
    || fail "panel bounds were ${PANEL_W}x${PANEL_H}${PANEL_X}${PANEL_Y}, expected 560px wide at +12+4"
[ "$PANEL_H" -ge 250 ] \
    || fail "panel anatomy collapsed to ${PANEL_H}px high"
python3 /tests/visual/beads-card-detail-inventory.py \
    --mock "$MOCK" \
    --image /output/beads-detail-comment-clamped.png \
    --panel 12 235 "$PANEL_W" "$PANEL_H" \
    --scale 1.0 \
    --output /output/beads-detail-inventory.json \
    || fail "mock inventory did not match the text-scale 1.0 panel"

# Clicking the newest comment expands its folded body. Clicking it again must
# restore the exact two-line/one-line inventory measured above.
convert /output/beads-detail-comment-clamped.png \
    -crop "560x${PANEL_H}+12+235" +repage /tmp/beads-detail-collapsed.png
xdotool mousemove --sync --window "$WID" 200 452
xdotool click 1
sleep 0.3
xdotool mousemove --sync --window "$WID" 700 500
import -window "$WID" /output/beads-detail-comment-expanded.png
convert /output/beads-detail-comment-expanded.png \
    -crop "560x${PANEL_H}+12+235" +repage /tmp/beads-detail-expanded.png
EXPAND_DIFF=$(compare -metric AE /tmp/beads-detail-collapsed.png \
    /tmp/beads-detail-expanded.png null: 2>&1 || true)
EXPAND_DIFF=${EXPAND_DIFF%%.*}
[ "${EXPAND_DIFF:-0}" -ge 500 ] \
    || fail "expanding the newest comment changed only ${EXPAND_DIFF:-0}px"
xdotool mousemove --sync --window "$WID" 200 452
xdotool click 1
sleep 0.3
xdotool mousemove --sync --window "$WID" 700 500
import -window "$WID" /output/beads-detail-comment-recollapsed.png
convert /output/beads-detail-comment-recollapsed.png \
    -crop "560x${PANEL_H}+12+235" +repage /tmp/beads-detail-recollapsed.png
RECOLLAPSE_DIFF=$(compare -metric AE /tmp/beads-detail-collapsed.png \
    /tmp/beads-detail-recollapsed.png null: 2>&1 || true)
RECOLLAPSE_DIFF=${RECOLLAPSE_DIFF%%.*}
[ "${RECOLLAPSE_DIFF:-9999}" -le 25 ] \
    || fail "recollapsed comment left ${RECOLLAPSE_DIFF}px changed"

# Hover reveals the approved copy glyph without moving the identity row. One
# click writes the full id, and replacing the clipboard afterward proves the
# parked intent cannot replay on a later frame.
xdotool mousemove --sync --window "$WID" 90 284
sleep 0.2
import -window "$WID" /output/beads-detail-id-hover.png
ID_HOVER_DIFF=$(compare -metric AE \
    /output/beads-detail-comment-clamped.png \
    /output/beads-detail-id-hover.png null: 2>&1 || true)
ID_HOVER_DIFF=${ID_HOVER_DIFF%%.*}
[ "${ID_HOVER_DIFF:-0}" -ge 10 ] \
    || fail "id hover changed only ${ID_HOVER_DIFF:-0}px"
[ "$ID_HOVER_DIFF" -le 1000 ] \
    || fail "id hover moved more than its copy glyph (${ID_HOVER_DIFF}px)"
xdotool click 1
sleep 0.2
COPIED=$(xclip -o -selection clipboard 2>/dev/null || true)
[ "$COPIED" = "detail-comment-clamped" ] \
    || fail "panel id copied '$COPIED'"
printf '%s' "clipboard-not-replayed" | xclip -selection clipboard >/dev/null 2>&1
sleep 0.4
COPIED=$(xclip -o -selection clipboard 2>/dev/null || true)
[ "$COPIED" = "clipboard-not-replayed" ] \
    || fail "panel id copy replayed after its click"

# Each dismissal is proven by opening the card again. A surviving panel or
# backdrop owns the pointer, so the next card click cannot emit another request.
xdotool key Escape
sleep 0.2
inject "$LOADING_MESSAGE"
xdotool mousemove --sync --window "$WID" 66 17
sleep 0.2
xdotool mousemove --sync --window "$WID" 80 84
xdotool click 1
sleep 0.2
[ "$(detail_request_count)" -eq "$((REQUESTS_BEFORE + 2))" ] \
    || fail "Escape did not dismiss the panel"

xdotool mousemove --sync --window "$WID" 549 258
xdotool click 1
sleep 0.2
inject "$LOADING_MESSAGE"
xdotool mousemove --sync --window "$WID" 66 17
sleep 0.2
xdotool mousemove --sync --window "$WID" 80 84
xdotool click 1
sleep 0.2
[ "$(detail_request_count)" -eq "$((REQUESTS_BEFORE + 3))" ] \
    || fail "close mark did not dismiss the panel"

xdotool mousemove --sync --window "$WID" 700 500
xdotool click 1
sleep 0.2
inject "$LOADING_MESSAGE"
xdotool mousemove --sync --window "$WID" 66 17
sleep 0.2
xdotool mousemove --sync --window "$WID" 80 84
xdotool click 1
sleep 0.2
[ "$(detail_request_count)" -eq "$((REQUESTS_BEFORE + 4))" ] \
    || fail "backdrop did not dismiss the panel"

inject "{\"type\":\"BeadsIssueDetail\",\"workspace_id\":\"$WORKSPACE\",\"issue_id\":\"detail-comment-clamped\",\"detail\":null}"
import -window "$WID" /output/beads-detail-not-found.png
NOT_FOUND_DIFF=$(compare -metric AE /output/beads-detail-loading.png \
    /output/beads-detail-not-found.png null: 2>&1 || true)
NOT_FOUND_DIFF=${NOT_FOUND_DIFF%%.*}
[ "${NOT_FOUND_DIFF:-0}" -ge 1000 ] \
    || fail "not-found close notice changed only ${NOT_FOUND_DIFF:-0}px"

inject "$LOADING_MESSAGE"
xdotool mousemove --sync --window "$WID" 66 17
sleep 0.2
xdotool mousemove --sync --window "$WID" 80 84
xdotool click 1
sleep 0.2
[ "$(detail_request_count)" -eq "$((REQUESTS_BEFORE + 5))" ] \
    || fail "not-found notice did not yield to a fresh open"
inject "{\"type\":\"BeadsBoard\",\"workspace_id\":\"$WORKSPACE\",\"protocol_version\":1,\"state\":\"NotDetected\"}"
import -window "$WID" /output/beads-detail-not-detected.png
NOT_DETECTED_DIFF=$(compare -metric AE /output/beads-detail-loading.png \
    /output/beads-detail-not-detected.png null: 2>&1 || true)
NOT_DETECTED_DIFF=${NOT_DETECTED_DIFF%%.*}
[ "${NOT_DETECTED_DIFF:-0}" -ge 1000 ] \
    || fail "NotDetected close notice changed only ${NOT_DETECTED_DIFF:-0}px"

# Open the Ready card with a short dependent-navigation detail synthesized
# from the checked-in typed fixture. Its dependent uses the existing hidden
# count fixture as the matching response.
HIDDEN_MESSAGE=$(printf '%s\n' "$DETAIL_MESSAGES" | awk -F '\t' '
    $1 == "hidden-count" { sub(/^[^\t]*\t/, ""); print; exit }
')
[ -n "$HIDDEN_MESSAGE" ] || fail "hidden-count fixture missing"
NAV_MESSAGE=$(python3 - "$HIDDEN_MESSAGE" <<'PY'
import json, sys

message = json.loads(sys.argv[1])
message["issue_id"] = "detail-loading"
detail = message["detail"]
detail.update({
    "id": "detail-loading",
    "title": "Load complete issue detail",
    "description": "",
    "acceptance_criteria": "",
    "notes": "",
    "design": "",
    "spec_id": None,
    "status": "open",
    "priority": 1,
    "issue_type": "task",
    "labels": [],
    "assignee": None,
    "owner": None,
    "defer_until": None,
    "due_at": None,
    "estimated_minutes": None,
    "external_ref": None,
    "blockers": [],
    "dependents": [{
        "id": "detail-hidden-count",
        "title": "Show hidden comment count"
    }],
    "comments": [],
    "hidden_comment_count": 0,
    "queue": "ready",
    "queue_basis": "ready_set"
})
print(json.dumps(message, separators=(",", ":")))
PY
)

xdotool key Escape
inject "$LOADING_MESSAGE"
xdotool mousemove --sync --window "$WID" 66 17
sleep 0.2
xdotool mousemove --sync --window "$WID" 280 84
xdotool click 1
sleep 0.2
inject "$NAV_MESSAGE"
TARGET_REQUESTS=$(detail_request_count "detail-hidden-count")
xdotool mousemove --sync --window "$WID" 155 340
xdotool click 1
sleep 0.3
[ "$(detail_request_count "detail-hidden-count")" -eq "$((TARGET_REQUESTS + 1))" ] \
    || fail "dependent chip sent no fresh detail request"

# The source stays visible while its request is in flight. Only the matching
# hidden-count reply retargets the panel.
xdotool mousemove --sync --window "$WID" 90 284
xdotool click 1
sleep 0.2
COPIED=$(xclip -o -selection clipboard 2>/dev/null || true)
[ "$COPIED" = "detail-loading" ] \
    || fail "dependent click swapped before its reply: '$COPIED'"
inject "$HIDDEN_MESSAGE"
xdotool mousemove --sync --window "$WID" 700 500
sleep 0.2
import -window "$WID" /output/beads-detail-hidden-count.png
xdotool mousemove --sync --window "$WID" 90 284
xdotool click 1
sleep 0.2
COPIED=$(xclip -o -selection clipboard 2>/dev/null || true)
[ "$COPIED" = "detail-hidden-count" ] \
    || fail "matching dependent reply did not swap the panel: '$COPIED'"

# Exercise the remaining checked-in settled variants through their real card
# clicks. Each must request its own ID, paint a distinct panel, and expose that
# exact identity through the panel's copy target.
capture_variant() {
    local name="$1" issue="$2" card_x="$3" message="$4"
    local before copied diff
    xdotool key Escape
    inject "$LOADING_MESSAGE"
    xdotool mousemove --sync --window "$WID" 66 17
    sleep 0.2
    before=$(detail_request_count "$issue")
    xdotool mousemove --sync --window "$WID" "$card_x" 84
    xdotool mousedown 1
    xdotool mouseup 1
    sleep 0.2
    [ "$(detail_request_count "$issue")" -eq "$((before + 1))" ] \
        || fail "$name card click sent no detail request"
    inject "$message"
    xdotool mousemove --sync --window "$WID" 700 600
    sleep 0.2
    import -window "$WID" "/output/beads-detail-${name}.png"
    diff=$(compare -metric AE /output/beads-detail-comment-clamped.png \
        "/output/beads-detail-${name}.png" null: 2>&1 || true)
    diff=${diff%%.*}
    [ "${diff:-0}" -ge 1000 ] || fail "$name fixture painted only ${diff:-0} distinct pixels"
    xdotool mousemove --sync --window "$WID" 466 284
    xdotool click 1
    sleep 0.2
    copied=$(xclip -o -selection clipboard 2>/dev/null || true)
    [ "$copied" = "$issue" ] || fail "$name panel copied '$copied' instead of '$issue'"
}

CLOSED_MESSAGE=$(printf '%s\n' "$DETAIL_MESSAGES" | awk -F '\t' '
    $1 == "closed" { sub(/^[^\t]*\t/, ""); print; exit }
')
BLOCKED_MESSAGE=$(printf '%s\n' "$DETAIL_MESSAGES" | awk -F '\t' '
    $1 == "blocked" { sub(/^[^\t]*\t/, ""); print; exit }
')
[ -n "$CLOSED_MESSAGE" ] && [ -n "$BLOCKED_MESSAGE" ] \
    || fail "closed or blocked fixture missing"
capture_variant blocked detail-blocked 700 "$BLOCKED_MESSAGE"
capture_variant closed detail-closed 900 "$CLOSED_MESSAGE"

echo "PASS: mock inventory, detail variants, lifecycle, copy, and navigation rendered"
