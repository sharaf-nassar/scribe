#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: Ctrl+clicking a link in the terminal opens it.
#
# The whole feature only exists at the app level. Detection is unit-tested
# against a grid, but "the pointer was over a link, Ctrl was down, and the OS
# handler was asked to open the right thing" spans the pointer path, the focused
# pane's grid, the pane's CWD as the server reports it, and a spawned process —
# none of which a headless test can produce.
#
# The oracle is a stand-in `xdg-open` installed on PATH ahead of the client's
# spawn, which appends its argument to a log. That is the exact string
# `url_detect::open_url` / `open_path` handed the OS, so the assertions are on
# what Scribe actually asked for rather than on a screenshot of a highlight.
#
# Phases:
#   0. install the opener stand-in and fill the grid with a URL on every row;
#   1. a plain click opens nothing — the modifier is the whole gate;
#   2. holding Ctrl rules the link under the pointer, and releasing it clears
#      the rule again;
#   3. Ctrl+click opens the URL under the pointer;
#   4. Ctrl+click on a RELATIVE path opens it resolved against the pane's CWD,
#      not against the client process's own directory.
#
# Every row is filled with the same link so the click needs no cell-accurate
# pixel arithmetic: any point in the middle of the grid is over one. What is
# being asserted is the routing, and a test that also had to solve for glyph
# metrics would fail for reasons that have nothing to do with it.
#
# Requires: visual container; xdotool, scrot.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
OPENED=/output/xdg-open.log
# The shell's directory for phase 3. A relative link resolved against anything
# else — the client's own CWD included — cannot produce this prefix.
LINK_DIR=/tmp/scribe-link-dir
URL="https://example.com/scribe-link"
# Three rows around the known middle of the 36-row visual pane avoid glyph
# measurement without flooding the paced client with a full scrollback.
URL_GRID_COMMAND="clear; printf '\\033[17;1H'; for i in \$(seq 1 3); do printf '%40s%s\\n' '' '$URL'; done"

fail() {
    echo "FAIL: $1" >&2
    echo "--- opened ---" >&2
    cat "$OPENED" >&2 2>/dev/null || echo "(nothing opened)" >&2
    echo "--- client log tail ---" >&2
    tail -40 "$CLIENT_LOG" >&2 || true
    exit 1
}

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

focus() {
    local wid
    wid=$(find_window)
    [ -n "$wid" ] || fail "no Scribe window found"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    # Past the X11 focus guard's 300 ms reactivation debounce.
    sleep 0.8
}

shot() {
    sleep 0.3
    scrot -o "$1"
    echo "captured $1"
}

type_text() {
    # Paste one shell command so fixture setup does not spend the visual
    # container's fixed budget on per-character XTEST events.
    printf '%s' "$1" | xclip -selection clipboard >/dev/null 2>&1
    xdotool key --clearmodifiers ctrl+shift+v
    sleep 0.3
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.5
}

# Run one command in the focused pane's shell and wait for it to finish.
run_in_pane() {
    type_text "$1"
    send_keys Return
    sleep 1.5
}

# The middle of the Scribe window, which the fill below guarantees is a link.
# Absolute screen coordinates, because the click is delivered through XTEST.
click_point() {
    local wid X Y WIDTH HEIGHT
    wid=$(find_window)
    [ -n "$wid" ] || fail "no Scribe window found"
    eval "$(xdotool getwindowgeometry --shell "$wid")"
    printf '%s %s' "$(( X + WIDTH / 2 ))" "$(( Y + HEIGHT / 2 ))"
}

click_middle() {
    local modifier="$1" x y
    read -r x y <<<"$(click_point)"
    xdotool mousemove --sync "$x" "$y"
    sleep 0.3
    if [ -n "$modifier" ]; then
        xdotool keydown "$modifier"
        sleep 0.3
        xdotool click 1
        sleep 0.3
        xdotool keyup "$modifier"
    else
        xdotool click 1
    fi
    sleep 1.0
}

WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0

# Cache the window's on-screen geometry so a capture can be cropped to the grid.
measure_window() {
    local wid X Y WIDTH HEIGHT
    wid=$(find_window)
    [ -n "$wid" ] || fail "no Scribe window found"
    eval "$(xdotool getwindowgeometry --shell "$wid")"
    WIN_X="$X"
    WIN_Y="$Y"
    WIN_W="$WIDTH"
    WIN_H="$HEIGHT"
}

# Crop a full-screen capture to the window minus its bottom band. The band holds
# the status bar, whose sparklines resample on their own clock, and the shell
# prompt row, which is the one row the fill does not own.
crop_body() {
    convert "$1" -crop "${WIN_W}x$(( WIN_H - 120 ))+${WIN_X}+${WIN_Y}" +repage "$2"
}

opened_count() { wc -l <"$OPENED" 2>/dev/null || echo 0; }

# Wait until the opener stand-in has been handed something matching `$1`.
wait_for_open() {
    local pattern="$1" timeout_secs="$2" started
    started=$(date +%s)
    while ! grep -qxF "$pattern" "$OPENED" 2>/dev/null; do
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
    return 0
}

# Install the real command lookup target with a deterministic observed outcome.
# The client resolves PATH per spawn, so rewriting this file is enough for its
# already-running process; no host opener is ever invoked.
install_opener() {
    local mode="$1"
    cat >/usr/local/bin/xdg-open <<SHIM
#!/bin/sh
printf '%s\\n' "\$1" >>"$OPENED"
case "$mode" in
    success) exit 0 ;;
    failure) exit 7 ;;
    delayed-failure) sleep 3; exit 7 ;;
    *) exit 64 ;;
esac
SHIM
    chmod +x /usr/local/bin/xdg-open
}

# Make the Linux opener genuinely absent without assuming the image has no
# `xdg-open`. Every executable named by the client's actual PATH is moved only
# inside this disposable container; later cases reinstall the stand-in above.
disable_opener() {
    local client_pid client_path dir
    client_pid=$(pgrep -o -x scribe-client 2>/dev/null || true)
    [ -n "$client_pid" ] || fail "link-fail-notfound: no client process"
    client_path=$(tr '\0' '\n' <"/proc/$client_pid/environ" | sed -n 's/^PATH=//p')
    [ -n "$client_path" ] || fail "link-fail-notfound: client PATH is empty"
    IFS=: read -r -a opener_dirs <<<"$client_path"
    for dir in "${opener_dirs[@]}"; do
        [ -n "$dir" ] && [ -e "$dir/xdg-open" ] && mv "$dir/xdg-open" "/tmp/$(basename "$dir")-xdg-open"
    done
    for dir in "${opener_dirs[@]}"; do
        [ ! -e "$dir/xdg-open" ] || fail "link-fail-notfound: xdg-open survived in $dir"
    done
}

fill_url_grid() {
    run_in_pane "$URL_GRID_COMMAND"
}

body_shot() {
    local name="$1"
    measure_window
    shot "/output/$name.png"
    crop_body "/output/$name.png" "/output/$name-body.png"
}

status_left_shot() {
    local name="$1"
    measure_window
    shot "/output/$name.png"
    convert "/output/$name.png" -crop "$(( WIN_W / 2 ))x80+$WIN_X+$(( WIN_Y + WIN_H - 80 ))" \
        +repage "/output/$name-status.png"
}

body_diff() {
    compare -metric AE "/output/$1-body.png" "/output/$2-body.png" null: 2>&1 || true
}

assert_body_changed() {
    local before="$1" after="$2" case_name="$3" diff
    diff=$(body_diff "$before" "$after")
    [ "${diff%% *}" -ge 50 ] || fail "$case_name: annotation changed only $diff pixels"
}

assert_body_restored() {
    local before="$1" after="$2" case_name="$3" diff
    diff=$(body_diff "$before" "$after")
    [ "${diff%% *}" = "0" ] || fail "$case_name: annotation did not restore the grid ($diff pixels)"
}

# A dismissal trigger may legitimately repaint another row (for example the
# key's shell echo). Derive the annotation's own changed rectangle, then require
# that rectangle alone to return to the pre-open pixels.
assert_annotation_restored() {
    local before="$1" annotation="$2" after="$3" case_name="$4" bounds diff
    bounds=$(convert "/output/$before-body.png" "/output/$annotation-body.png" \
        -compose difference -composite -trim -format '%wx%h%X%Y' info:)
    convert "/output/$before-body.png" -crop "$bounds" +repage "/output/$case_name-before.png"
    convert "/output/$after-body.png" -crop "$bounds" +repage "/output/$case_name-after.png"
    diff=$(compare -metric AE "/output/$case_name-before.png" "/output/$case_name-after.png" null: 2>&1 || true)
    [ "${diff%% *}" = "0" ] || fail "$case_name: annotation rectangle was not restored ($diff pixels)"
}

show_failed_annotation() {
    local case_name="$1"
    : >"$OPENED"
    body_shot "$case_name-before"
    click_middle ctrl
    wait_for_open "$URL" 10 || fail "$case_name: Ctrl+click never reached the opener stand-in"
    body_shot "$case_name-annotation"
    assert_body_changed "$case_name-before" "$case_name-annotation" "$case_name"
}

# ── Phase 0: the opener stand-in and a grid full of links ─────────
# Installed on PATH rather than injected into the client, because the point is
# to observe the real `Command::new("xdg-open")` spawn. The lookup happens at
# spawn time, so the already-running client picks this up without a relaunch.
install_opener success
: >"$OPENED"

focus
# The fixed-width URL column covers the window midpoint without measured glyph
# geometry, while the three rows cover normal window-centre rounding.
run_in_pane "$URL_GRID_COMMAND"
shot /output/00-url-grid.png
[ "$(opened_count)" -eq 0 ] || fail "PHASE 0: something was opened before any click"
echo "PHASE 0 PASS: the grid is filled with links and nothing has been opened"

# ── Phase 1: a plain click opens nothing ──────────────────────────
# The modifier is the entire gate: without it a click on a link is an ordinary
# selection gesture, and a build that opened links on a bare click would make
# every drag-select over a URL launch a browser.
click_middle ""
sleep 1.0
[ "$(opened_count)" -eq 0 ] \
    || fail "PHASE 1: an unmodified click opened $(cat "$OPENED")"
echo "PHASE 1 PASS: an unmodified click over a link opened nothing"

# ── Phase 2: holding Ctrl rules the link under the pointer ────────
# The rule is the whole discoverability half of the feature: without it nothing
# on screen says a Ctrl+click would do anything. It is painted from the same
# lookup the click uses, so a build that draws nothing here is a build whose
# click is aimed at a link the user was never shown.
measure_window
read -r POINTER_X POINTER_Y <<<"$(click_point)"
xdotool mousemove --sync "$POINTER_X" "$POINTER_Y"
sleep 0.8
shot /output/01-idle-hover.png
crop_body /output/01-idle-hover.png /output/01-body.png
xdotool keydown ctrl
sleep 1.0
shot /output/01a-ctrl-hover.png
crop_body /output/01a-ctrl-hover.png /output/01a-body.png
xdotool keyup ctrl
HOVER_DIFF=$(compare -metric AE /output/01-body.png /output/01a-body.png null: 2>&1 || true)
[ "${HOVER_DIFF%% *}" != "0" ] \
    || fail "PHASE 2: holding Ctrl over a link painted no underline"
# And it must go away again: a rule left behind would sit under text that is no
# longer offering to open anything.
sleep 1.0
shot /output/01b-ctrl-released.png
crop_body /output/01b-ctrl-released.png /output/01b-body.png
RELEASE_DIFF=$(compare -metric AE /output/01-body.png /output/01b-body.png null: 2>&1 || true)
[ "${RELEASE_DIFF%% *}" = "0" ] \
    || fail "PHASE 2: the underline survived Ctrl coming back up ($RELEASE_DIFF px)"
echo "PHASE 2 PASS: Ctrl ruled the link ($HOVER_DIFF px) and releasing it cleared the rule"

# ── Phase 3: Ctrl+click opens the URL ─────────────────────────────
click_middle ctrl
wait_for_open "$URL" 10 || fail "PHASE 3: Ctrl+click did not open $URL"
shot /output/02-after-ctrl-click.png
echo "PHASE 3 PASS: Ctrl+click handed $URL to the OS opener"

# ── Phase 4: a relative path resolves against the pane's CWD ──────
# The shell announces its directory with OSC 7, which is what puts a CWD on the
# pane at all; the assertion is that the *absolute* path came out, because a
# relative link opened without one would reach the OS as `./linkme.txt` and
# resolve against whatever directory the client process happens to be in.
: >"$OPENED"
mkdir -p "$LINK_DIR"
touch "$LINK_DIR/linkme.txt"
run_in_pane "cd $LINK_DIR && printf '\\033]7;file://localhost%s\\033\\\\' \"\$PWD\""
sleep 2.0
run_in_pane "clear; printf '\\033[17;1H'; for i in \$(seq 1 3); do printf '%55s%s\\n' '' './linkme.txt'; done"
shot /output/03-path-grid.png
click_middle ctrl
wait_for_open "$LINK_DIR/linkme.txt" 10 \
    || fail "PHASE 4: Ctrl+click on ./linkme.txt did not open $LINK_DIR/linkme.txt"
shot /output/04-after-path-click.png
echo "PHASE 4 PASS: a relative link opened as $LINK_DIR/linkme.txt"

# ── Phase 5: a dot-prefixed bare relative path keeps its leading '.' ──
# `.impeccable/mocks/linkme.html` starts with `.` immediately followed by more
# than a bare `/`, unlike the explicit `./` form Phase 4 already covers — this
# exercises the bare-relative scanner's start-of-token gate (scribe-gv09): a
# build that only fixed `./` would still truncate this to `impeccable/...`.
: >"$OPENED"
mkdir -p "$LINK_DIR/.impeccable/mocks"
touch "$LINK_DIR/.impeccable/mocks/linkme.html"
run_in_pane "clear; printf '\\033[17;1H'; for i in \$(seq 1 3); do printf '%40s%s\\n' '' '.impeccable/mocks/linkme.html'; done"
shot /output/05-dotpath-grid.png
click_middle ctrl
wait_for_open "$LINK_DIR/.impeccable/mocks/linkme.html" 10 \
    || fail "PHASE 5: Ctrl+click on .impeccable/mocks/linkme.html did not open $LINK_DIR/.impeccable/mocks/linkme.html"
shot /output/06-after-dotpath-click.png
echo "PHASE 5 PASS: a dot-prefixed relative link opened as $LINK_DIR/.impeccable/mocks/linkme.html"

# ── Phase 6: the original beads-board-signal-theme.html repro opens ──
# The exact file from the bug report, so the fix is proven against the repro
# itself and not only against a synthetic linkme.html.
: >"$OPENED"
touch "$LINK_DIR/.impeccable/mocks/beads-board-signal-theme.html"
run_in_pane "clear; printf '\\033[17;1H'; for i in \$(seq 1 3); do printf '%35s%s\\n' '' '.impeccable/mocks/beads-board-signal-theme.html'; done"
shot /output/07-repro-grid.png
click_middle ctrl
wait_for_open "$LINK_DIR/.impeccable/mocks/beads-board-signal-theme.html" 10 \
    || fail "PHASE 6: Ctrl+click on beads-board-signal-theme.html did not open $LINK_DIR/.impeccable/mocks/beads-board-signal-theme.html"
shot /output/08-after-repro-click.png
echo "PHASE 6 PASS: the original repro opened as $LINK_DIR/.impeccable/mocks/beads-board-signal-theme.html"

# ── Link feedback: deterministic observed-opener outcomes ──────────
# Each failure starts from a fixed URL grid and a controlled opener result. The
# visual delta is the annotation itself; lower-level tests own message spelling.

# `link-fail-notfound`: move every client-PATH opener, then Ctrl+click. This is
# a spawn failure, not an exit-status stand-in, so it exercises the NotFound
# classifier branch regardless of packages installed in the visual image.
fill_url_grid
disable_opener
: >"$OPENED"
body_shot link-fail-notfound-before
click_middle ctrl
sleep 0.5
body_shot link-fail-notfound-annotation
[ "$(opened_count)" -eq 0 ] || fail "link-fail-notfound: a disabled opener ran"
assert_body_changed link-fail-notfound-before link-fail-notfound-annotation link-fail-notfound
echo "CASE link-fail-notfound PASS: absent xdg-open painted feedback"

# One shell driver supplies all four PTY receipts in sequence. It costs one
# paste, then the physical dismissal gesture advances it to the next receipt.
install_opener failure
KEY_MARKER=/output/link-fail-dismiss-key
MOUSE_MARKER=/output/link-fail-dismiss-click
WHEEL_MARKER=/output/link-fail-dismiss-wheel
OUTPUT_RELEASE=/output/link-fail-dismiss-output-release
OUTPUT_MARKER=/output/link-fail-dismiss-output
STALE_RELEASE=/output/link-fail-stale-drop-release
STALE_MARKER=/output/link-fail-stale-drop-output
HOVER_RELEASE=/output/hover-preview-release
HOVER_MARKER=/output/hover-preview-grid
OSC8_URI="https://example.com/scribe-osc8-hover-target"
OSC8_LABEL="osc8-hover-label"
rm -f "$KEY_MARKER" "$MOUSE_MARKER" "$WHEEL_MARKER" "$OUTPUT_RELEASE" "$OUTPUT_MARKER" \
    "$STALE_RELEASE" "$STALE_MARKER" "$HOVER_RELEASE" "$HOVER_MARKER"
run_in_pane "IFS= read -r -n 1 key; printf '%s' \"\$key\" >'$KEY_MARKER'; printf '\\033[?1000h\\033[?1006h'; stty -icanon -echo min 1 time 0; IFS= read -r -d M report; printf '%sM' \"\$report\" >'$MOUSE_MARKER'; stty sane; stty -icanon -echo min 1 time 0; IFS= read -r -d M report; printf '%sM' \"\$report\" >'$WHEEL_MARKER'; stty sane; printf '\\033[?1000l\\033[?1006l'; (while [ ! -e '$OUTPUT_RELEASE' ]; do sleep 0.1; done; printf '\\033[0m'; printf sent >'$OUTPUT_MARKER') & (while [ ! -e '$STALE_RELEASE' ]; do sleep 0.1; done; printf '\\033[0m'; printf sent >'$STALE_MARKER') & (while [ ! -e '$HOVER_RELEASE' ]; do sleep 0.1; done; printf '\\033[2J\\033[H\\033[17;1H'; for i in \$(seq 1 3); do printf '%50s\\033]8;;$OSC8_URI\\033\\\\%s\\033]8;;\\033\\\\\\n' '' '$OSC8_LABEL'; done; printf sent >'$HOVER_MARKER') &"

# `link-fail-dismiss-key`: the same `z` that removes the annotation satisfies
# the driver's one-byte read; an overlay that consumed it leaves no marker.
show_failed_annotation link-fail-dismiss-key
[ ! -e "$KEY_MARKER" ] || fail "link-fail-dismiss-key: Ctrl+click leaked to the PTY"
xdotool key --clearmodifiers z
sleep 0.5
[ "$(cat "$KEY_MARKER" 2>/dev/null)" = z ] || fail "link-fail-dismiss-key: dismissal key missed the PTY"
body_shot link-fail-dismiss-key-after
assert_annotation_restored link-fail-dismiss-key-before link-fail-dismiss-key-annotation link-fail-dismiss-key-after link-fail-dismiss-key
echo "CASE link-fail-dismiss-key PASS: z dismissed feedback and reached the PTY"

# The driver now owns ordinary SGR button reports. The Ctrl+click must remain
# unforwarded; the next click advances the driver and records its own report.
show_failed_annotation link-fail-dismiss-click
[ ! -e "$MOUSE_MARKER" ] || fail "link-fail-dismiss-click: Ctrl+click leaked to the PTY"
click_middle ""
[ -s "$MOUSE_MARKER" ] || fail "link-fail-dismiss-click: dismissal click missed the PTY"
grep -Faq $'\033[<0;' "$MOUSE_MARKER" || fail "link-fail-dismiss-click: PTY received no left-click report"
body_shot link-fail-dismiss-click-after
assert_annotation_restored link-fail-dismiss-click-before link-fail-dismiss-click-annotation link-fail-dismiss-click-after link-fail-dismiss-click
echo "CASE link-fail-dismiss-click PASS: click dismissed feedback and reached the PTY"

# The driver re-arms its raw read before this next failure; wheel-up must enter
# it as SGR button 64 while the root listener dismisses without consuming it.
show_failed_annotation link-fail-dismiss-wheel
[ ! -e "$WHEEL_MARKER" ] || fail "link-fail-dismiss-wheel: Ctrl+click leaked to the PTY"
read -r POINTER_X POINTER_Y <<<"$(click_point)"
xdotool mousemove --sync "$POINTER_X" "$POINTER_Y"
xdotool click 4
sleep 0.5
[ -s "$WHEEL_MARKER" ] || fail "link-fail-dismiss-wheel: dismissal wheel missed the PTY"
grep -Faq $'\033[<64;' "$WHEEL_MARKER" || fail "link-fail-dismiss-wheel: PTY received no wheel report"
body_shot link-fail-dismiss-wheel-after
assert_annotation_restored link-fail-dismiss-wheel-before link-fail-dismiss-wheel-annotation link-fail-dismiss-wheel-after link-fail-dismiss-wheel
echo "CASE link-fail-dismiss-wheel PASS: wheel dismissed feedback and reached the PTY"

# The driver's gated ESC[0m is non-empty PTY output but does not repaint the
# body, so the annotation rectangle must restore exactly after this release.
show_failed_annotation link-fail-dismiss-output
touch "$OUTPUT_RELEASE"
for _ in $(seq 1 30); do [ -e "$OUTPUT_MARKER" ] && break; sleep 0.1; done
[ -e "$OUTPUT_MARKER" ] || fail "link-fail-dismiss-output: gated PTY output never ran"
body_shot link-fail-dismiss-output-after
assert_annotation_restored link-fail-dismiss-output-before link-fail-dismiss-output-annotation link-fail-dismiss-output-after link-fail-dismiss-output
echo "CASE link-fail-dismiss-output PASS: PTY output dismissed feedback"

# `link-open-success-silent`: the observed child exits 0, so the opener log is
# present but no annotation can change an otherwise static grid.
install_opener success
: >"$OPENED"
body_shot link-open-success-silent-before
click_middle ctrl
wait_for_open "$URL" 10 || fail "link-open-success-silent: opener was not invoked"
body_shot link-open-success-silent-after
assert_body_restored link-open-success-silent-before link-open-success-silent-after link-open-success-silent
echo "CASE link-open-success-silent PASS: successful observed open stayed silent"

# `link-fail-stale-drop`: delay the controlled failing child, mutate the PTY
# grid before it reports, then require the settled frame to stay unchanged.
install_opener delayed-failure
: >"$OPENED"
click_middle ctrl
wait_for_open "$URL" 10 || fail "link-fail-stale-drop: delayed opener was not invoked"
touch "$STALE_RELEASE"
for _ in $(seq 1 30); do [ -e "$STALE_MARKER" ] && break; sleep 0.1; done
[ -e "$STALE_MARKER" ] || fail "link-fail-stale-drop: gated PTY output never ran"
body_shot link-fail-stale-drop-after-output
sleep 3.5
body_shot link-fail-stale-drop-settled
assert_body_restored link-fail-stale-drop-after-output link-fail-stale-drop-settled link-fail-stale-drop
echo "CASE link-fail-stale-drop PASS: moved anchor suppressed late feedback"

# `hover-preview` / `hover-unhover-restore`: OSC 8 is the only bare-hover
# source. Its underline changes the body while its URI replaces the left status
# group; leaving the client frame restores that group exactly.
touch "$HOVER_RELEASE"
for _ in $(seq 1 30); do [ -e "$HOVER_MARKER" ] && break; sleep 0.1; done
[ -e "$HOVER_MARKER" ] || fail "hover-preview: OSC 8 fixture never reached the PTY"
read -r POINTER_X POINTER_Y <<<"$(click_point)"
xdotool mousemove --sync "$(( WIN_X + 10 ))" "$(( WIN_Y + 10 ))"
sleep 0.5
body_shot hover-preview-before
status_left_shot hover-preview-before
xdotool mousemove --sync "$POINTER_X" "$POINTER_Y"
sleep 0.8
body_shot hover-preview-hovered
status_left_shot hover-preview-hovered
assert_body_changed hover-preview-before hover-preview-hovered hover-preview
STATUS_DIFF=$(compare -metric AE /output/hover-preview-before-status.png /output/hover-preview-hovered-status.png null: 2>&1 || true)
[ "${STATUS_DIFF%% *}" -ge 10 ] || fail "hover-preview: status URI changed only $STATUS_DIFF pixels"
echo "CASE hover-preview PASS: OSC 8 underline and status URI painted"
xdotool mousemove --sync "$(( WIN_X + 10 ))" "$(( WIN_Y + 10 ))"
sleep 0.8
status_left_shot hover-unhover-restore
UNHOVER_DIFF=$(compare -metric AE /output/hover-preview-before-status.png /output/hover-unhover-restore-status.png null: 2>&1 || true)
[ "${UNHOVER_DIFF%% *}" = "0" ] || fail "hover-unhover-restore: prior status changed by $UNHOVER_DIFF pixels"
echo "CASE hover-unhover-restore PASS: live left status group returned"

# The same Ctrl+Shift+U test seam used by overlays.sh must keep all four real
# annotation layouts reachable from this link-focused visual recipe too.
body_shot annotation-demo-fixture
for state in default busy-row clamped top-flip; do
    xdotool key --clearmodifiers ctrl+shift+u
    sleep 0.5
    body_shot "annotation-demo-$state"
    case "$state" in
        default) assert_body_changed annotation-demo-fixture "annotation-demo-$state" "annotation-demo-$state" ;;
        busy-row) assert_body_changed annotation-demo-default "annotation-demo-$state" "annotation-demo-$state" ;;
        clamped) assert_body_changed annotation-demo-busy-row "annotation-demo-$state" "annotation-demo-$state" ;;
        top-flip) assert_body_changed annotation-demo-clamped "annotation-demo-$state" "annotation-demo-$state" ;;
    esac
    echo "CASE annotation-demo-$state PASS: state painted"
done

echo ""
echo "PASS: visual terminal-links test"
echo "  Link feedback screenshots: test-output/link-fail-*.png"
echo "  Hover screenshots: test-output/hover-*.png"
echo "  Annotation demo screenshots: test-output/annotation-demo-*.png"
echo "  Opened targets: test-output/xdg-open.log"
