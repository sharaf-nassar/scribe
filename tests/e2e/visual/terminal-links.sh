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
    xdotool type --clearmodifiers --delay 40 "$1"
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

# ── Phase 0: the opener stand-in and a grid full of links ─────────
# Installed on PATH rather than injected into the client, because the point is
# to observe the real `Command::new("xdg-open")` spawn. The lookup happens at
# spawn time, so the already-running client picks this up without a relaunch.
cat >/usr/local/bin/xdg-open <<'SHIM'
#!/bin/sh
printf '%s\n' "$1" >>/output/xdg-open.log
SHIM
chmod +x /usr/local/bin/xdg-open
: >"$OPENED"

focus
# Six copies per row so the row is wider than the window at any plausible font
# size; a URL broken by a soft wrap is rejoined by the scanner, so a row that
# does wrap still resolves to the same link.
run_in_pane "clear; for i in \$(seq 1 20); do echo '$URL $URL $URL $URL $URL $URL'; done"
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
run_in_pane "clear; for i in \$(seq 1 20); do echo './linkme.txt ./linkme.txt ./linkme.txt ./linkme.txt ./linkme.txt ./linkme.txt'; done"
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
run_in_pane "clear; for i in \$(seq 1 20); do echo '.impeccable/mocks/linkme.html .impeccable/mocks/linkme.html .impeccable/mocks/linkme.html .impeccable/mocks/linkme.html .impeccable/mocks/linkme.html .impeccable/mocks/linkme.html'; done"
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
run_in_pane "clear; for i in \$(seq 1 20); do echo '.impeccable/mocks/beads-board-signal-theme.html .impeccable/mocks/beads-board-signal-theme.html .impeccable/mocks/beads-board-signal-theme.html'; done"
shot /output/07-repro-grid.png
click_middle ctrl
wait_for_open "$LINK_DIR/.impeccable/mocks/beads-board-signal-theme.html" 10 \
    || fail "PHASE 6: Ctrl+click on beads-board-signal-theme.html did not open $LINK_DIR/.impeccable/mocks/beads-board-signal-theme.html"
shot /output/08-after-repro-click.png
echo "PHASE 6 PASS: the original repro opened as $LINK_DIR/.impeccable/mocks/beads-board-signal-theme.html"

echo ""
echo "PASS: visual terminal-links test"
echo "  Inspect screenshots in test-output/:"
echo "    00-url-grid.png            — the grid filled with one URL per row"
echo "    01-idle-hover.png          — the pointer parked on a link, no modifier"
echo "    01a-ctrl-hover.png         — the same frame with Ctrl held: the rule"
echo "    01b-ctrl-released.png      — the rule gone again after Ctrl came up"
echo "    02-after-ctrl-click.png    — the window after the Ctrl+click"
echo "    03-path-grid.png           — the same grid filled with a relative path"
echo "    04-after-path-click.png    — the window after the path Ctrl+click"
echo "    05-dotpath-grid.png        — the grid filled with a dot-prefixed path"
echo "    06-after-dotpath-click.png — the window after the dot-path Ctrl+click"
echo "    07-repro-grid.png          — the grid filled with the original repro path"
echo "    08-after-repro-click.png   — the window after the repro Ctrl+click"
echo "  Opened targets: test-output/xdg-open.log"
