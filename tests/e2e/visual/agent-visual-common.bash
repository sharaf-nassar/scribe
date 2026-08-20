# Shared window/focus/capture/failure helpers for the agent consent, agent
# indicator, and AI indicator visual E2Es. All three drive one real Scribe
# window on the shared-pane rig (SCRIBE_SHARED_PANE=1) and diff screenshots
# taken of it.

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"

fail() {
    echo "FAIL: $*" >&2
    tail -50 "$CLIENT_LOG" 2>/dev/null >&2 || true
    exit 1
}

find_scribe_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -n "$wid" ] || wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

focus_scribe_window() {
    xdotool windowactivate --sync "$1" 2>/dev/null \
        || xdotool windowfocus --sync "$1" 2>/dev/null || true
}

# Captures the Scribe window named by $WID, which every caller sets as a
# global before calling this.
shot() {
    import -window "$WID" +repage "$1"
}

delta() {
    local changed
    changed=$(compare -metric AE "$1" "$2" null: 2>&1 || true)
    printf '%s' "${changed%%.*}"
}
