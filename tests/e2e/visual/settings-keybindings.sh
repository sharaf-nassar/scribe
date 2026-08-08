#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: the Keybindings page records shortcuts from the keyboard.
#
# Every keybinding row was read-only for the whole rebuild: the page listed the
# 60 actions and their combos, and the only way to change one was editing
# `config.toml` by hand. `ControlKind::Keybinding` returned `None` from
# `focus_targets`, so the rows were not even reachable by traversal.
#
# A unit test can prove the combo grammar and the conflict rule (it does — see
# the settings window suite). It cannot prove that a keystroke pressed at a real
# X server reaches `capture_keystroke` instead of the window's own traversal
# handler, that the write lands in `config.toml`, or that the running client
# re-parses its bindings afterwards. So every phase drives the real settings
# window through XTEST and asserts on config state and on the client's log:
#
#   * traversal reaches the first keybinding row, activation puts it in
#     listening state, and Ctrl+Alt+N is written to `keybindings.new_tab` —
#     which also proves the recording row claims Ctrl+K and Tab instead of
#     letting the window's own shortcuts eat them;
#   * recording the same chord on a second action is refused with the conflict
#     on screen and `config.toml` untouched, and Escape leaves it that way;
#   * a bare Backspace unbinds an action to an empty combo list;
#   * the terminal window then answers the NEW chord with "opened a new tab"
#     and ignores `new_tab`'s old default, which is the only proof the live
#     `ConfigReloaded` path re-parsed `Bindings` rather than just writing a file.
#
# Traversal, not pixel coordinates: the settings window's focus order is
# `settings_nav_pages()` (11 pages, Keybindings at index 3) followed by the
# selected page's controls, and `keybinding_actions()` opens with `new_tab`
# then `new_claude_tab`. Tab counts below are derived from exactly that, so a
# reordered nav or action list fails here loudly instead of silently recording
# onto the wrong row.
#
# Input is driven through XTEST (plain `xdotool key`, no `--window`). GPUI reads
# the keyboard through XInput2 and ignores the synthetic events
# `xdotool --window` sends with XSendEvent, so window-targeted input would leave
# the client untouched while the script still "passed".
#
# Requires: visual container (see docker/entrypoint-visual.sh), which exports
# SESSION, SCRIBE_CLIENT_PID and SCRIBE_CLIENT_LOG.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
CONFIG_FILE="${XDG_CONFIG_HOME:?the entrypoint must export XDG_CONFIG_HOME}/scribe/config.toml"
SERVER_RELOAD_PATTERN="config reloaded successfully via client request"

# Lit pixels the settings window must paint, matching settings-entry.sh: a page
# of labelled rows over a dark ground is far more than this, an unpainted or
# blank window far less.
SETTINGS_INK_MIN="${SETTINGS_INK_MIN:-500}"
# A refused capture adds a conflict line above the row and grows the row from
# 46px to 78px. That repaints comfortably more than this; a capture that was
# silently swallowed repaints nothing.
SETTINGS_CHANGE_MIN="${SETTINGS_CHANGE_MIN:-100}"

# Focus-order arithmetic, from crates/scribe-client/src/settings/window.rs.
NAV_PAGE_COUNT=11
KEYBINDINGS_NAV_INDEX=3
# Tabs from a freshly opened window (focus index 0, no visible focus) to the
# Keybindings row of the sidebar.
TABS_TO_KEYBINDINGS_PAGE=$KEYBINDINGS_NAV_INDEX
# Tabs from the selected Keybindings sidebar row to the page's first control.
TABS_TO_FIRST_ACTION=$(( NAV_PAGE_COUNT - KEYBINDINGS_NAV_INDEX ))

# The chord recorded onto `new_tab`. Deliberately not a default of any action,
# so the conflict phase can only fail for the reason it is testing.
NEW_CHORD_XDOTOOL="ctrl+alt+n"
NEW_CHORD_CONFIG="ctrl+alt+n"
# `new_tab`'s Linux default, which must stop working once it is replaced.
OLD_CHORD_XDOTOOL="ctrl+shift+t"

TERM_X=0
TERM_Y=0

# The terminal window only. `--name Scribe` is an unanchored regex and would
# also match "Scribe Settings".
list_terminal_windows() {
    xdotool search --name '^Scribe$' 2>/dev/null || true
}

list_settings_windows() {
    xdotool search --name '^Scribe Settings$' 2>/dev/null || true
}

count_settings_windows() {
    list_settings_windows | grep -c . || true
}

wait_for_settings_windows() {
    local want="$1" timeout_secs="${2:-15}" started
    started=$(date +%s)
    while true; do
        [ "$(count_settings_windows)" -eq "$want" ] && return 0
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

# Focus the terminal window. Re-focusing is load-bearing: the settings window
# takes the X11 focus when it opens, and the client's own active-window guard
# suppresses keystrokes aimed at a window that is not `_NET_ACTIVE_WINDOW`.
focus_terminal() {
    local wid info
    wid=$(list_terminal_windows | tail -1)
    if [ -z "$wid" ]; then
        fail "FAIL: no Scribe terminal window found"
    fi
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.5
    # `xwininfo`, not `xdotool getwindowgeometry`: openbox reparents the window
    # into a decorated frame and xdotool reports that frame's box.
    info=$(xwininfo -id "$wid")
    TERM_X=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    TERM_Y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
}

focus_settings() {
    local wid
    wid=$(list_settings_windows | tail -1)
    if [ -z "$wid" ]; then
        fail "FAIL: no Scribe Settings window found"
    fi
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.5
}

shot() {
    sleep 0.3
    scrot -o "$1"
    echo "captured $1"
}

settings_ink() {
    local wid info x y w h value
    wid=$(list_settings_windows | tail -1)
    [ -z "$wid" ] && { printf '0'; return; }
    info=$(xwininfo -id "$wid")
    x=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
    w=$(printf '%s\n' "$info" | awk '/^  Width:/ { print $2 }')
    h=$(printf '%s\n' "$info" | awk '/^  Height:/ { print $2 }')
    value=$(convert "$1" -crop "${w}x${h}+${x}+${y}" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

settings_changed_pixels() {
    local before="$1" after="$2" wid info x y w h value
    wid=$(list_settings_windows | tail -1)
    [ -z "$wid" ] && { printf '0'; return; }
    info=$(xwininfo -id "$wid")
    x=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
    w=$(printf '%s\n' "$info" | awk '/^  Width:/ { print $2 }')
    h=$(printf '%s\n' "$info" | awk '/^  Height:/ { print $2 }')
    value=$(convert "$before" "$after" -compose difference -composite \
        -crop "${w}x${h}+${x}+${y}" +repage -colorspace Gray -threshold 3% \
        -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.5
}

press_tab() {
    local times="$1" i
    for (( i = 0; i < times; i++ )); do
        xdotool key --clearmodifiers Tab
        sleep 0.25
    done
    sleep 0.3
}

# Assert one action's combo list in the live config.
#
# `--absent` means the key is not written at all, which is how the fixture ships
# before any settings write: the defaults live in `KeybindingsConfig::default`
# and serde fills them in. `--` means the key IS written and bound to nothing,
# which is what a Backspace unbind produces. The two are different states and
# the phases below depend on telling them apart.
assert_binding() {
    python3 - "$CONFIG_FILE" "$@" <<'PY'
import sys
import tomllib

try:
    with open(sys.argv[1], "rb") as config_file:
        config = tomllib.load(config_file)
except FileNotFoundError:
    config = {}

action = sys.argv[2]
expected = sys.argv[3:]
actual = config.get("keybindings", {}).get(action)
if isinstance(actual, str):
    actual = [actual]

if expected == ["--absent"]:
    if actual is not None:
        print(f"{action} should be unwritten, got {actual!r}")
        raise SystemExit(1)
    raise SystemExit(0)

if expected == ["--"]:
    expected = []
if actual is None:
    actual = []
if actual != expected:
    print(f"{action} mismatch: expected {expected!r}, got {actual!r}")
    raise SystemExit(1)
PY
}

count_log() {
    grep -acF "$1" "$CLIENT_LOG" 2>/dev/null || true
}

wait_for_log_growth() {
    local pattern="$1" baseline="$2" timeout_secs="${3:-15}" started
    started=$(date +%s)
    while true; do
        if [ "$(count_log "$pattern")" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

count_server_reloads() {
    grep -acF "$SERVER_RELOAD_PATTERN" "$SERVER_LOG" 2>/dev/null || true
}

wait_for_server_reload_growth() {
    local baseline="$1" timeout_secs="${2:-15}" started
    started=$(date +%s)
    while true; do
        if [ "$(count_server_reloads)" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

fail() {
    echo "$1"
    echo "--- client log tail ---"
    tail -40 "$CLIENT_LOG" || true
    echo "--- server log tail ---"
    tail -40 "$SERVER_LOG" || true
    exit 1
}

# ── Phase 0: the fixture starts on the shipped defaults ───────────
focus_terminal
if [ "$(count_settings_windows)" -ne 0 ]; then
    fail "PHASE 0 FAIL: a settings window was already open"
fi
if ! assert_binding new_tab --absent; then
    fail "PHASE 0 FAIL: the fixture already carries a written new_tab binding"
fi
shot /output/00-terminal-only.png
echo "PHASE 0 PASS: terminal up on default keybindings, no settings window"

# ── Phase 1: the settings chord opens the window ──────────────────
send_keys ctrl+comma
if ! wait_for_settings_windows 1 15; then
    fail "PHASE 1 FAIL: ctrl+, mapped no settings window"
fi
shot /output/01-settings-open.png
INK=$(settings_ink /output/01-settings-open.png)
if [ "$INK" -lt "$SETTINGS_INK_MIN" ]; then
    fail "PHASE 1 FAIL: the settings window painted $INK px (min $SETTINGS_INK_MIN)"
fi
echo "PHASE 1 PASS: ctrl+, opened the settings window (ink $INK)"

# @lat: [[test#Visual E2E Tests#Keybindings record from the keyboard#A recorded chord is written and applied live]]
# ── Phase 2: traversal reaches a row and records a chord ──────────
# Tab three times to the Keybindings sidebar row, Enter to select the page,
# then eight more Tabs to the first control on it — `new_tab`.
focus_settings
press_tab "$TABS_TO_KEYBINDINGS_PAGE"
send_keys Return
shot /output/02-keybindings-page.png
press_tab "$TABS_TO_FIRST_ACTION"
send_keys Return
shot /output/03-listening.png
LISTENING_CHANGED=$(settings_changed_pixels /output/02-keybindings-page.png /output/03-listening.png)
if [ "$LISTENING_CHANGED" -lt "$SETTINGS_CHANGE_MIN" ]; then
    fail "PHASE 2 FAIL: activating the row repainted $LISTENING_CHANGED px — it never entered listening state"
fi
RELOADS_BEFORE=$(count_server_reloads)
send_keys "$NEW_CHORD_XDOTOOL"
if ! wait_for_server_reload_growth "$RELOADS_BEFORE" 15; then
    fail "PHASE 2 FAIL: the recorded chord never reached the config writer"
fi
if ! assert_binding new_tab "$NEW_CHORD_CONFIG"; then
    fail "PHASE 2 FAIL: $NEW_CHORD_CONFIG was not written to keybindings.new_tab"
fi
shot /output/04-recorded.png
echo "PHASE 2 PASS: the first keybinding row recorded $NEW_CHORD_CONFIG"

# @lat: [[test#Visual E2E Tests#Keybindings record from the keyboard#A conflicting chord is refused on screen]]
# ── Phase 3: the same chord is refused on another action ──────────
# One more Tab reaches `new_claude_tab`, the second action in the page's list.
press_tab 1
send_keys Return
shot /output/05-second-listening.png
send_keys "$NEW_CHORD_XDOTOOL"
shot /output/06-conflict.png
CONFLICT_CHANGED=$(settings_changed_pixels /output/05-second-listening.png /output/06-conflict.png)
if [ "$CONFLICT_CHANGED" -lt "$SETTINGS_CHANGE_MIN" ]; then
    fail "PHASE 3 FAIL: the refused capture repainted $CONFLICT_CHANGED px — no conflict was shown"
fi
if ! assert_binding new_claude_tab "ctrl+alt+c"; then
    fail "PHASE 3 FAIL: a conflicting chord was written to keybindings.new_claude_tab anyway"
fi
if ! assert_binding new_tab "$NEW_CHORD_CONFIG"; then
    fail "PHASE 3 FAIL: the refused capture disturbed the action that owns the chord"
fi
send_keys Escape
shot /output/07-cancelled.png
if ! assert_binding new_claude_tab "ctrl+alt+c"; then
    fail "PHASE 3 FAIL: cancelling the recording still changed the binding"
fi
echo "PHASE 3 PASS: the duplicate chord was refused and Escape left the binding alone"

# @lat: [[test#Visual E2E Tests#Keybindings record from the keyboard#Backspace unbinds an action]]
# ── Phase 4: a bare Backspace unbinds ─────────────────────────────
RELOADS_BEFORE=$(count_server_reloads)
send_keys Return
send_keys BackSpace
if ! wait_for_server_reload_growth "$RELOADS_BEFORE" 15; then
    fail "PHASE 4 FAIL: the unbind never reached the config writer"
fi
if ! assert_binding new_claude_tab --; then
    fail "PHASE 4 FAIL: Backspace did not clear keybindings.new_claude_tab"
fi
shot /output/08-unbound.png
echo "PHASE 4 PASS: Backspace unbound new_claude_tab"

# @lat: [[test#Visual E2E Tests#Keybindings record from the keyboard#A recorded chord is written and applied live]]
# ── Phase 5: the running client honours the new chord ─────────────
# The write is only half the feature: the client re-parses `Bindings` on the
# live `ConfigReloaded`, so the terminal window must answer the recorded chord
# and ignore the default it replaced.
focus_terminal
TABS_BEFORE=$(count_log "opened a new tab")
send_keys "$NEW_CHORD_XDOTOOL"
if ! wait_for_log_growth "opened a new tab" "$TABS_BEFORE" 15; then
    fail "PHASE 5 FAIL: the recorded chord opened no tab — the client never re-parsed its bindings"
fi
TABS_AFTER_NEW=$(count_log "opened a new tab")
send_keys "$OLD_CHORD_XDOTOOL"
sleep 3
if [ "$(count_log "opened a new tab")" -gt "$TABS_AFTER_NEW" ]; then
    fail "PHASE 5 FAIL: $OLD_CHORD_XDOTOOL still opens tabs after being replaced"
fi
shot /output/09-live-chord.png
echo "PHASE 5 PASS: the client answers $NEW_CHORD_CONFIG and ignores the replaced default"

echo "ALL PHASES PASS: keybindings record, refuse conflicts, unbind, and apply live"
