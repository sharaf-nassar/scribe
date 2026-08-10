#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual E2E: an AI provider's task label is the fallback tab label, while a
# native OSC 0 title owns the tab until the application resets it.
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
NATIVE_TITLE="Native application title"
WINDOW_TITLE="Window title via BEL"
ICON_TITLE="Icon title via ST"
LATEST_WINDOW_TITLE="Latest window title via BEL"
UNIFIED_TITLE="Unified;title via OSC 0"

# Minimum differing pixels in the tab band for a label to count as rendered.
# Replacing a four-character shell name with a multi-word label repaints far
# more than this; a dropped notice leaves the band byte-identical.
LABEL_DELTA_MIN="${LABEL_DELTA_MIN:-40}"
# Slack allowed when the band must have returned to its pre-label state. With
# SCRIBE_DISABLE_ANIMATIONS=1 consecutive frames are byte-identical, so this is
# noise tolerance only.
CLEARED_DELTA_MAX="${CLEARED_DELTA_MAX:-2}"

# The openbox decoration is 20 px tall and the client's own titlebar is 34 px.
# Stop exactly at their shared bottom edge so the terminal cursor cannot blink
# into a comparison that is meant to cover only the tab strip.
TAB_BAND_HEIGHT=54

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
# or the window controls on the right. TAB_BAND_HEIGHT includes openbox's own
# decoration and the whole client titlebar, but no pixels from the terminal
# grid below it.
crop_tab_band() {
    local src="$1" dest="$2" w h ox oy
    read -r w h ox oy <<<"$(window_bbox "$src")"
    convert "$src" -crop "$((w / 2))x${TAB_BAND_HEIGHT}+${ox}+${oy}" +repage "$dest"
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

# Prove an application title supersedes an already-visible AI fallback. Clear
# the hidden task label first, then reset OSC 0, so the final source is the
# shell basename and every transition has an unambiguous pixel oracle.
run_native_title_precedence() {
    local baseline delta

    shot /output/03-title-00-shell.png
    crop_tab_band /output/03-title-00-shell.png /output/03-title-band-shell.png

    baseline=$(count_log "tab task label updated")
    hook --provider=claude_code --event=task_label_changed --label="$CLAUDE_LABEL"
    if ! wait_for_log_growth "tab task label updated" "$baseline"; then
        fail "FAIL (title): the client never took the fallback task label"
    fi
    shot /output/03-title-01-task.png
    crop_tab_band /output/03-title-01-task.png /output/03-title-band-task.png

    focus
    xdotool type --clearmodifiers --delay 20 \
        "printf '\\033]0;${NATIVE_TITLE}\\a'; read"
    xdotool key --clearmodifiers Return
    shot /output/03-title-02-native.png
    crop_tab_band /output/03-title-02-native.png /output/03-title-band-native.png
    delta=$(band_delta /output/03-title-band-task.png /output/03-title-band-native.png)
    echo "  tab band delta after OSC 0 title: $delta"
    if [ "${delta:-0}" -lt "$LABEL_DELTA_MIN" ]; then
        fail "FAIL (title): OSC 0 did not replace the AI fallback label"
    fi
    echo "PASS (title): native OSC 0 title owns the tab"

    baseline=$(count_log "tab task label updated")
    hook --provider=claude_code --event=task_label_cleared
    if ! wait_for_log_growth "tab task label updated" "$baseline"; then
        fail "FAIL (title): the client never cleared the hidden task label"
    fi
    shot /output/03-title-03-task-cleared.png
    crop_tab_band /output/03-title-03-task-cleared.png \
        /output/03-title-band-task-cleared.png
    delta=$(band_delta /output/03-title-band-native.png \
        /output/03-title-band-task-cleared.png)
    if [ "${delta:-0}" -gt "$CLEARED_DELTA_MAX" ]; then
        fail "FAIL (title): clearing hidden AI metadata changed the native title"
    fi
    echo "PASS (title): clearing hidden task metadata leaves OSC 0 visible"

    xdotool key --clearmodifiers ctrl+c
    focus
    xdotool type --clearmodifiers --delay 20 "printf '\\033]0;\\a'; read"
    xdotool key --clearmodifiers Return
    shot /output/03-title-04-reset.png
    crop_tab_band /output/03-title-04-reset.png /output/03-title-band-reset.png
    delta=$(band_delta /output/03-title-band-shell.png /output/03-title-band-reset.png)
    if [ "${delta:-0}" -gt "$CLEARED_DELTA_MAX" ]; then
        fail "FAIL (title): blank OSC 0 did not restore the shell fallback"
    fi
    xdotool key --clearmodifiers ctrl+c
    echo "PASS (title): blank OSC 0 restores the shell fallback"
}

# Prove the standard xterm title-source precedence through both OSC
# terminators. OSC 1 owns the visible tab while set; clearing it reveals the
# independently retained OSC 2 title. OSC 0 then updates both sources, so
# clearing only OSC 1 leaves the same label visible before blank OSC 0 resets
# both to the shell fallback.
run_osc_title_sources() {
    local delta

    shot /output/04-title-00-shell.png
    crop_tab_band /output/04-title-00-shell.png /output/04-title-band-shell.png

    focus
    xdotool type --clearmodifiers --delay 20 \
        "printf '\\033]2;${WINDOW_TITLE}\\a'; read"
    xdotool key --clearmodifiers Return
    shot /output/04-title-01-window-bel.png
    crop_tab_band /output/04-title-01-window-bel.png /output/04-title-band-window.png

    xdotool key --clearmodifiers ctrl+c
    focus
    xdotool type --clearmodifiers --delay 20 \
        "printf '\\033]1;${ICON_TITLE}\\033\\\\'; read"
    xdotool key --clearmodifiers Return
    shot /output/04-title-02-icon-st.png
    crop_tab_band /output/04-title-02-icon-st.png /output/04-title-band-icon.png
    delta=$(band_delta /output/04-title-band-window.png /output/04-title-band-icon.png)
    if [ "${delta:-0}" -lt "$LABEL_DELTA_MIN" ]; then
        fail "FAIL (OSC 1 ST): icon title did not override the OSC 2 tab title"
    fi
    echo "PASS (OSC 1 ST): icon title owns the visible tab"

    xdotool key --clearmodifiers ctrl+c
    focus
    xdotool type --clearmodifiers --delay 20 \
        "printf '\\033]2;${LATEST_WINDOW_TITLE}\\a'; read"
    xdotool key --clearmodifiers Return
    shot /output/04-title-03-window-hidden-bel.png
    crop_tab_band /output/04-title-03-window-hidden-bel.png \
        /output/04-title-band-window-hidden.png
    delta=$(band_delta /output/04-title-band-icon.png \
        /output/04-title-band-window-hidden.png)
    if [ "${delta:-0}" -gt "$CLEARED_DELTA_MAX" ]; then
        fail "FAIL (OSC 2 BEL): newer window title displaced active OSC 1"
    fi
    echo "PASS (OSC 2 BEL): newer window title remains hidden behind OSC 1"

    xdotool key --clearmodifiers ctrl+c
    focus
    xdotool type --clearmodifiers --delay 20 "printf '\\033]1;\\033\\\\'; read"
    xdotool key --clearmodifiers Return
    shot /output/04-title-04-icon-reset-st.png
    crop_tab_band /output/04-title-04-icon-reset-st.png \
        /output/04-title-band-icon-reset.png
    delta=$(band_delta /output/04-title-band-icon.png /output/04-title-band-icon-reset.png)
    if [ "${delta:-0}" -lt "$LABEL_DELTA_MIN" ]; then
        fail "FAIL (OSC 1 ST): blank icon title did not reveal latest OSC 2"
    fi
    echo "PASS (OSC 1 ST): blank icon title reveals the latest OSC 2 title"

    xdotool key --clearmodifiers ctrl+c
    focus
    xdotool type --clearmodifiers --delay 20 \
        "printf '\\033]0;${UNIFIED_TITLE}\\a'; read"
    xdotool key --clearmodifiers Return
    shot /output/04-title-05-unified-bel.png
    crop_tab_band /output/04-title-05-unified-bel.png /output/04-title-band-unified.png

    xdotool key --clearmodifiers ctrl+c
    focus
    xdotool type --clearmodifiers --delay 20 "printf '\\033]1;\\033\\\\'; read"
    xdotool key --clearmodifiers Return
    shot /output/04-title-06-unified-icon-reset-st.png
    crop_tab_band /output/04-title-06-unified-icon-reset-st.png \
        /output/04-title-band-unified-icon-reset.png
    delta=$(band_delta /output/04-title-band-unified.png \
        /output/04-title-band-unified-icon-reset.png)
    if [ "${delta:-0}" -gt "$CLEARED_DELTA_MAX" ]; then
        fail "FAIL (OSC 0 BEL): OSC 0 did not update both title sources"
    fi
    echo "PASS (OSC 0 BEL): OSC 0 updates icon and window title sources"

    xdotool key --clearmodifiers ctrl+c
    focus
    xdotool type --clearmodifiers --delay 20 "printf '\\033]0;\\033\\\\'; read"
    xdotool key --clearmodifiers Return
    shot /output/04-title-07-unified-reset-st.png
    crop_tab_band /output/04-title-07-unified-reset-st.png \
        /output/04-title-band-unified-reset.png
    delta=$(band_delta /output/04-title-band-shell.png \
        /output/04-title-band-unified-reset.png)
    if [ "${delta:-0}" -gt "$CLEARED_DELTA_MAX" ]; then
        fail "FAIL (OSC 0 ST): blank OSC 0 did not clear both title sources"
    fi
    xdotool key --clearmodifiers ctrl+c
    echo "PASS (OSC 0 ST): blank OSC 0 restores the shell fallback"
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

# ── Phase 3: application titles own the primary automatic label ───
run_native_title_precedence
echo "PHASE 3 PASS: OSC 0 owns the tab and blank OSC 0 restores the shell"

# ── Phase 4: OSC 0/1/2 keep independent standard title sources ───
run_osc_title_sources
echo "PHASE 4 PASS: OSC 0/1/2 title precedence survives BEL and ST"

echo ""
echo "PASS: native titles own AI fallback labels and reset to the shell"
echo "  Inspect screenshots in test-output/:"
echo "    01-claude-00-before.png    — tab strip before the Claude label"
echo "    01-claude-01-labelled.png  — tab renamed to \"$CLAUDE_LABEL\""
echo "    01-claude-02-cleared.png   — tab back to its shell title"
echo "    02-codex-01-labelled.png   — tab renamed to \"$CODEX_LABEL\""
echo "    02-codex-02-cleared.png    — tab back to its shell title"
echo "    03-title-02-native.png     — OSC 0 title overrides an active AI label"
echo "    03-title-04-reset.png      — blank OSC 0 restores the shell title"
echo "    04-title-02-icon-st.png    — OSC 1 overrides OSC 2"
echo "    04-title-04-icon-reset-st.png — blank OSC 1 reveals latest OSC 2"
echo "    04-title-07-unified-reset-st.png — blank OSC 0 clears both sources"
