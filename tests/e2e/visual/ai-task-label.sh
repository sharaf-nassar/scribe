#!/bin/bash
# Visual E2E: an AI provider's task label becomes the tab label, and clearing
# it puts the shell title back.
#
# The four task-label notices (`TaskLabelChanged`, `TaskLabelCleared`,
# `CodexTaskLabelChanged`, `CodexTaskLabelCleared`) used to fall into the live
# reader's unhandled-message counter, so an AI tab in the GPUI client never
# renamed itself. This test proves the whole chain end to end with nothing
# stubbed: the real `scribe-hook-helper` posts a hook event to the real
# `scribe-server`, the server translates it and broadcasts the notice to the
# window attached to that session, and the running client's tab strip repaints.
#
# Both wire spellings are exercised. `--provider=claude_code` produces the
# provider-tagged `TaskLabelChanged` / `TaskLabelCleared` pair;
# `--provider=codex_code` produces the legacy `CodexTaskLabelChanged` /
# `CodexTaskLabelCleared` pair, because the server splits Codex back out for
# backward compatibility.
#
# Phase 0 mirrors `overlay-actions.sh`: the entrypoint creates $SESSION through
# `scribe-test` *after* launching the client, and the server both sends
# `SessionCreated` only to the connection that asked and hides another window's
# sessions from `ListSessions`, so the running client never learns the session
# exists. Stopping the test daemon releases that ownership; a relaunched client
# then picks the session up through the normal `ListSessions` path and shows a
# tab for it. `scribe-hook-helper` needs no daemon — it addresses the server
# socket directly with the session id in its environment — so the hook channel
# still works after the daemon is gone.
#
# Requires: visual container (see docker/entrypoint-visual.sh), which exports
# SESSION, SCRIBE_CLIENT_PID and SCRIBE_CLIENT_LOG.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
HOOK_SOCK="${SCRIBE_RUNTIME_DIR:-/run/user/$(id -u)/scribe}/server.sock"

CLAUDE_LABEL="Ship the tab labels"
CODEX_LABEL="Rewrite the parser"

# Minimum differing pixels in the tab band for a label to count as rendered.
# Replacing a four-character shell name with a multi-word label repaints far
# more than this; a dropped notice leaves the band byte-identical.
LABEL_DELTA_MIN="${LABEL_DELTA_MIN:-40}"
# Slack allowed when the band must have returned to its pre-label state. With
# SCRIBE_DISABLE_ANIMATIONS=1 consecutive frames are byte-identical, so this is
# noise tolerance only.
CLEARED_DELTA_MAX="${CLEARED_DELTA_MAX:-2}"

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

focus() {
    local wid
    wid=$(find_window)
    if [ -z "$wid" ]; then
        echo "FAIL: no Scribe window found"
        exit 1
    fi
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.3
}

shot() {
    focus
    sleep 0.3
    scrot -o "$1"
    echo "captured $1"
}

# Echo "W H OX OY" for the Scribe window frame as it appears in a capture,
# found by trimming the black Xvfb root away. `xdotool getwindowgeometry`
# reports pre-reparenting coordinates under openbox and does not agree with the
# screenshot, so the screenshot itself is the source of truth.
window_bbox() {
    convert "$1" -bordercolor black -fuzz 1% -trim -format "%w %h %X %Y" info: | tr -d '+'
}

# Crop the tab strip out of a full screenshot. The client's titlebar is
# TITLEBAR_HEIGHT (34 px) tall and lays its tabs out from the left edge, so the
# left half of the window's top band is all tab and holds none of the gear icon
# or the window controls on the right. The band is 60 px so it still contains
# the whole tab row underneath openbox's own decoration, whose pixels are
# identical in every capture and so contribute nothing to a diff.
crop_tab_band() {
    local src="$1" dest="$2" w h ox oy
    read -r w h ox oy <<<"$(window_bbox "$src")"
    convert "$src" -crop "$((w / 2))x60+${ox}+${oy}" +repage "$dest"
}

# Count lit pixels inside the Scribe window of a full-screen capture. Rendered
# text is near-white on a near-black background, so a plain luminance threshold
# separates ink from an empty window cleanly.
window_ink() {
    local w h ox oy value
    read -r w h ox oy <<<"$(window_bbox "$1")"
    value=$(convert "$1" -crop "${w}x${h}+${ox}+${oy}" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

# Count differing pixels between two tab-band crops.
band_delta() {
    local diff
    diff=$(compare -metric AE "$1" "$2" null: 2>&1 || true)
    echo "${diff%%.*}"
}

# Post one hook event for $SESSION straight to the server socket, exactly as a
# provider's hook would. No daemon involved.
hook() {
    SCRIBE_HOOK_SOCK="$HOOK_SOCK" SCRIBE_SESSION_ID="$SESSION" scribe-hook-helper "$@"
    sleep 1.0
}

# Count matching lines in the client log (0 when the log does not exist yet).
count_log() {
    grep -acF "$1" "$CLIENT_LOG" 2>/dev/null || true
}

# Wait until the client log holds more copies of a pattern than `baseline`.
wait_for_log_growth() {
    local pattern="$1" baseline="$2" timeout_secs="${3:-15}" started now
    started=$(date +%s)
    while true; do
        now=$(count_log "$pattern")
        if [ "$now" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(("$(date +%s)" - started)) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

fail() {
    echo "$1"
    echo "--- client log tail ---"
    tail -40 "$CLIENT_LOG" 2>/dev/null || true
    echo "--- server log tail ---"
    tail -20 "${SCRIBE_SERVER_LOG:-/output/server.log}" 2>/dev/null || true
    exit 1
}

# Drive one provider through set → clear and assert the tab band both times.
# $1 provider id, $2 label text, $3 output-file prefix.
run_label_cycle() {
    local provider="$1" label="$2" prefix="$3" delta baseline

    shot "/output/${prefix}-00-before.png"
    crop_tab_band "/output/${prefix}-00-before.png" "/output/${prefix}-band-before.png"

    baseline=$(count_log "tab task label updated")
    hook --provider="$provider" --event=task_label_changed --label="$label"
    if ! wait_for_log_growth "tab task label updated" "$baseline"; then
        fail "FAIL ($provider): the client never took a task-label notice"
    fi
    if ! grep -qF -- "$label" "$CLIENT_LOG"; then
        fail "FAIL ($provider): the label text never reached the client"
    fi
    shot "/output/${prefix}-01-labelled.png"
    crop_tab_band "/output/${prefix}-01-labelled.png" "/output/${prefix}-band-labelled.png"
    delta=$(band_delta "/output/${prefix}-band-before.png" "/output/${prefix}-band-labelled.png")
    echo "  tab band delta after $provider label: $delta"
    if [ "${delta:-0}" -lt "$LABEL_DELTA_MIN" ]; then
        fail "FAIL ($provider): the tab band did not repaint for the task label"
    fi
    echo "PASS ($provider): task label rendered into the tab strip"

    baseline=$(count_log "tab task label updated")
    hook --provider="$provider" --event=task_label_cleared
    if ! wait_for_log_growth "tab task label updated" "$baseline"; then
        fail "FAIL ($provider): the client never took the task-label clear"
    fi
    shot "/output/${prefix}-02-cleared.png"
    crop_tab_band "/output/${prefix}-02-cleared.png" "/output/${prefix}-band-cleared.png"
    delta=$(band_delta "/output/${prefix}-band-before.png" "/output/${prefix}-band-cleared.png")
    echo "  tab band delta after $provider clear: $delta"
    if [ "${delta:-0}" -gt "$CLEARED_DELTA_MAX" ]; then
        fail "FAIL ($provider): the tab band did not return to its shell title"
    fi
    echo "PASS ($provider): clearing the task label restored the shell title"
}

# ── Phase 0: hand the client a live pane so it has a tab at all ────
sleep 1.0
kill "$SCRIBE_CLIENT_PID" 2>/dev/null || true
for _ in $(seq 1 40); do
    pgrep -f 'scribe-client' >/dev/null 2>&1 || break
    sleep 0.25
done
if pgrep -f 'scribe-client' >/dev/null 2>&1; then
    fail "PHASE 0 FAIL: the original client did not exit"
fi
scribe-test daemon stop >/dev/null 2>&1 || true
sleep 1.0
scribe-client >>"$CLIENT_LOG" 2>&1 &
xdotool search --sync --name "Scribe" >/dev/null 2>&1 || true
sleep 2

# Attaching is a full Hello / ListSessions / AttachSessions / SessionReplay
# round trip, so poll for rendered content rather than guessing a delay. The
# attach itself leaves no client log line at `info`, and the tab strip is the
# thing under test, so the window's own ink is the readiness signal.
BASE_INK=0
for _ in $(seq 1 40); do
    shot /output/00-attached.png >/dev/null
    BASE_INK=$(window_ink /output/00-attached.png)
    [ "${BASE_INK:-0}" -ge 20 ] && break
    sleep 0.5
done
if [ "${BASE_INK:-0}" -lt 20 ]; then
    fail "PHASE 0 FAIL: the client rendered no pane content (ink $BASE_INK)"
fi
echo "PHASE 0 PASS: client attached to session $SESSION (window ink $BASE_INK)"

# ── Phase 1: the provider-tagged pair (TaskLabelChanged/Cleared) ───
run_label_cycle claude_code "$CLAUDE_LABEL" 01-claude
echo "PHASE 1 PASS: TaskLabelChanged / TaskLabelCleared drive the tab label"

# ── Phase 2: the legacy Codex pair (CodexTaskLabelChanged/Cleared) ─
run_label_cycle codex_code "$CODEX_LABEL" 02-codex
echo "PHASE 2 PASS: CodexTaskLabelChanged / CodexTaskLabelCleared drive it too"

echo ""
echo "PASS: AI task labels rename the tab and clear back to the shell title"
echo "  Inspect screenshots in test-output/:"
echo "    01-claude-00-before.png    — tab strip before the Claude label"
echo "    01-claude-01-labelled.png  — tab renamed to \"$CLAUDE_LABEL\""
echo "    01-claude-02-cleared.png   — tab back to its shell title"
echo "    02-codex-01-labelled.png   — tab renamed to \"$CODEX_LABEL\""
echo "    02-codex-02-cleared.png    — tab back to its shell title"
