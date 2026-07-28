#!/bin/bash
# Visual + scripted E2E test: the trusted-device, trusted-network, and
# env-preflight controls of the GPUI settings window (fix unit FU-18).
#
# Runs the REAL settings window (`scribe-client --settings`, main.rs
# `run_settings`) against the REAL scribe-server, with the wire tap
# (`scribe-test share-tap`, SCRIBE_SHARE_TAP=1) interposed on the server socket.
# The settings window never registers a client connection — every server action
# it performs is a one-shot transient socket — so the tap's JSONL record is the
# only place those frames can be observed leaving the process.
#
# A green unit test over settings/server_action.rs proves nothing here: those
# helpers already passed with zero callers. This script therefore asserts, for
# each control, that the corresponding `ClientMessage` appears ON THE WIRE after
# a real pointer click, and that the server's reply is rendered back into the
# page (screenshots plus the window's status line).
#
# The container seeds one trusted network and one approved device before the
# server starts (SCRIBE_SEED_TRUST=1, see docker/entrypoint-visual.sh): a single
# machine has no peer to approve and no fingerprintable Wi-Fi, so without the
# seed the Remove/Revoke rows would never exist to click.
#
# Requires: visual container with SCRIBE_VISUAL_APP=settings, SCRIBE_SHARE_TAP=1,
# SCRIBE_SEED_TRUST=1, xdotool, scrot, python3.
set -e

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
SEEDED_NETWORK_ID="seeded-network-1"
SEEDED_DEVICE_ID="1f2e3d4c5b6a798877665544332211000f1e2d3c4b5a69788796a5b4c3d2e1f0"

# ── Window geometry ───────────────────────────────────────────────────────────
# settings/window.rs lays each page out as fixed-height flex rows, so every
# control has a stable offset from the client area's top-left corner. The
# heights below are the GPUI box heights at the default 16px rem, a 23px
# text_sm line box, and a 28px text_lg line box — each verified against a
# captured frame:
#   nav item      py_2*2 + 23                       = 39
#   page header   py_3*2 + 28                       = 52
#   control row   py_2*2 + pill(py_1*2 + 23) + 1px border = 48
#   note row      py_2*2 + 23 + 1px border          = 40
#   section head  pt_4 + pb_1 + 23                  = 43
NAV_ITEM_H=39
PAGE_HEADER_H=52
CONTROL_ROW_H=48
NOTE_ROW_H=40
SECTION_HEAD_H=43

NAV_X=100
# Nav order: Appearance, Colors, AI, Terminal, Environment, Keybindings,
# Workspaces, Updates, Releases, Notifications, Remote.
NAV_ENVIRONMENT_INDEX=4
NAV_REMOTE_INDEX=10

CLIENT_X=0
CLIENT_Y=0
CLIENT_RIGHT=0

find_window() {
    local wid
    wid=$(xdotool search --name 'Scribe Settings' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

raise_window() {
    local wid
    wid=$(find_window)
    if [ -z "$wid" ]; then
        echo "FAIL: no Scribe Settings window found" >&2
        exit 1
    fi
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.3
}

# Locate the GPUI client area inside the openbox frame from a screenshot.
#
# `xdotool getwindowgeometry` reports pre-reparenting coordinates under openbox
# and disagrees with what was actually painted (the same reason
# tests/e2e/visual/update-common.sh trims the root away), and the frame carries a
# 1px border plus a ~26px titlebar that must not be counted as page content. So:
# trim the black root to find the frame, then walk down the frame's left edge
# until the light-grey titlebar gives way to the dark sidebar.
calibrate() {
    local shot="$1" fw fh fx fy offset right
    read -r fw fh fx fy <<<"$(convert "$shot" -bordercolor black -fuzz 1% -trim \
        -format "%w %h %X %Y" info: | tr -d '+')"
    CLIENT_X=$(( fx + 1 ))
    # x is taken well inside the sidebar: openbox draws a dark iconify button in
    # the titlebar's left corner, and three consecutive dark rows are required so
    # a single dark decoration pixel cannot be mistaken for the client area.
    offset=$(convert "$shot" -crop "1x120+$(( fx + 60 ))+$fy" +repage txt: \
        | awk -F'[,:(]' 'NR>1 { v=$4+0; if (v < 60 && v > 5) { run++ } else { run=0 }
                               if (run == 3) { print $2 - 2; exit } }')
    if [ -z "$offset" ]; then
        echo "FAIL: could not find the settings client area in $shot" >&2
        exit 1
    fi
    CLIENT_Y=$(( fy + offset ))
    right=$(( fx + fw - 2 ))
    CLIENT_RIGHT=$right
    echo "client area at ${CLIENT_X},${CLIENT_Y}, right edge ${CLIENT_RIGHT}"
}

shot() {
    raise_window
    sleep 0.3
    scrot -o "$1"
    echo "captured $1"
}

# Click a client-area-relative point through XTEST.
click_at() {
    local x="$1" y="$2"
    raise_window
    xdotool mousemove "$(( CLIENT_X + x ))" "$(( CLIENT_Y + y ))"
    sleep 0.2
    xdotool click 1
    # The settings window performs its server round-trips synchronously on the UI
    # thread, so a click can take up to the 3s SERVER_ACTION_TIMEOUT to settle.
    sleep 1.5
}

# Click the sidebar entry for a page index.
click_nav() {
    click_at "$NAV_X" "$(( NAV_ITEM_H * $1 + NAV_ITEM_H / 2 ))"
}

# Click the right-hand control of a content row whose vertical centre (measured
# from the client-area top) is $1. Every interactive widget is right-aligned
# inside the row's 16px padding, so a point 30px inside the client area's right
# edge lands on the pill.
click_row_control() {
    click_at "$(( CLIENT_RIGHT - CLIENT_X - 30 ))" "$1"
}

# Count recorded client frames of a given message type.
count_client() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
path, wanted = sys.argv[1], sys.argv[2]
total = 0
try:
    fh = open(path)
except OSError:
    print(0)
    sys.exit(0)
with fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") == "client" and row.get("message", {}).get("type") == wanted:
            total += 1
print(total)
PY
}

# Count recorded server frames of a given message type.
count_server() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
path, wanted = sys.argv[1], sys.argv[2]
total = 0
try:
    fh = open(path)
except OSError:
    print(0)
    sys.exit(0)
with fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") == "server" and row.get("message", {}).get("type") == wanted:
            total += 1
print(total)
PY
}

# Assert a recorded client frame of $2 matches every key=value pair after it.
assert_client_frame() {
    python3 - "$RECORD" "$@" <<'PY'
import json, sys
path, wanted = sys.argv[1], sys.argv[2]
def norm(value):
    try:
        return json.loads(value)
    except ValueError:
        return value

pairs = [(k, norm(v)) for k, v in (p.split("=", 1) for p in sys.argv[3:])]
with open(path) as fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != "client":
            continue
        msg = row.get("message", {})
        if msg.get("type") != wanted:
            continue
        if all(msg.get(k) == v for k, v in pairs):
            print(json.dumps(msg))
            sys.exit(0)
sys.exit(1)
PY
}

# Fail unless the count of $1 frames in direction $2 grew past $3.
assert_grew() {
    local kind="$1" dir="$2" before="$3" now
    if [ "$dir" = "client" ]; then
        now=$(count_client "$kind")
    else
        now=$(count_server "$kind")
    fi
    if [ "$now" -le "$before" ]; then
        echo "FAIL: no new $dir $kind frame on the wire (was $before, now $now)" >&2
        exit 1
    fi
}

sleep 1.5
raise_window
scrot -o /output/01-settings-open.png
calibrate /output/01-settings-open.png
echo "captured /output/01-settings-open.png"

# ── Phase 1: opening Remote queries the whole trust surface ───────────────────
LAN_BEFORE=$(count_client GetLanEnv)
NETS_BEFORE=$(count_client ListTrustedNetworks)
DEVS_BEFORE=$(count_client ListTrustedDevices)
NET_LIST_BEFORE=$(count_server TrustedNetworkList)
DEV_LIST_BEFORE=$(count_server TrustedDeviceList)

click_nav "$NAV_REMOTE_INDEX"
shot /output/02-remote-trust.png

assert_grew GetLanEnv client "$LAN_BEFORE"
assert_grew ListTrustedNetworks client "$NETS_BEFORE"
assert_grew ListTrustedDevices client "$DEVS_BEFORE"
assert_grew TrustedNetworkList server "$NET_LIST_BEFORE"
assert_grew TrustedDeviceList server "$DEV_LIST_BEFORE"
echo "PHASE 1 PASS: opening Remote puts ListTrustedNetworks + ListTrustedDevices"
echo "              (and GetLanEnv) on the wire and renders both replies"

# Row tops on the loaded Remote page, in tenths of a pixel from the client top:
# header, "Local network" heading, the two trust actions, three status notes,
# the "Trusted networks" heading and its one seeded row, then the "Approved
# devices" heading and its one seeded row.
REFRESH_ROW=$(( PAGE_HEADER_H + SECTION_HEAD_H ))
TRUST_NETWORK_ROW=$(( REFRESH_ROW + CONTROL_ROW_H ))
NETWORKS_HEAD=$(( TRUST_NETWORK_ROW + CONTROL_ROW_H + NOTE_ROW_H * 3 ))
NETWORK_ROW=$(( NETWORKS_HEAD + SECTION_HEAD_H ))
DEVICE_ROW=$(( NETWORK_ROW + CONTROL_ROW_H + SECTION_HEAD_H ))

mid() { echo $(( $1 + CONTROL_ROW_H / 2 )); }

# ── Phase 2: the explicit refresh action re-queries ─────────────────────────────
NETS_BEFORE=$(count_client ListTrustedNetworks)
click_row_control "$(mid "$REFRESH_ROW")"
assert_grew ListTrustedNetworks client "$NETS_BEFORE"
shot /output/03-trust-refreshed.png
echo "PHASE 2 PASS: the Refresh control re-issues the trust queries"

# ── Phase 3: removing the seeded trusted network ─────────────────────────
REMOVE_BEFORE=$(count_client RemoveTrustedNetwork)
click_row_control "$(mid "$NETWORK_ROW")"
assert_grew RemoveTrustedNetwork client "$REMOVE_BEFORE"
assert_client_frame RemoveTrustedNetwork "id=$SEEDED_NETWORK_ID" >/dev/null
shot /output/04-network-removed.png
echo "PHASE 3 PASS: the seeded network's Remove button sends"
echo "              RemoveTrustedNetwork{id=$SEEDED_NETWORK_ID}"

# ── Phase 4: revoking the seeded approved device ─────────────────────────
# Removing the only trusted network replaces its row with the "No trusted
# networks yet." note, so the device list moves up by that height difference.
DEVICE_ROW_AFTER_REMOVE=$(( DEVICE_ROW - CONTROL_ROW_H + NOTE_ROW_H ))
REVOKE_BEFORE=$(count_client RevokeTrustedDevice)
click_row_control "$(mid "$DEVICE_ROW_AFTER_REMOVE")"
assert_grew RevokeTrustedDevice client "$REVOKE_BEFORE"
assert_client_frame RevokeTrustedDevice "device_id=$SEEDED_DEVICE_ID" >/dev/null
shot /output/05-device-revoked.png
echo "PHASE 4 PASS: the approved device's Revoke button sends"
echo "              RevokeTrustedDevice{device_id=$SEEDED_DEVICE_ID}"

# ── Phase 5: trusting the current network ─────────────────────────────────
# Last of the Remote-page phases on purpose: if the container's network happens
# to be fingerprintable the server adds a row, and no later click may depend on
# the list length.
ADD_BEFORE=$(count_client AddCurrentNetworkTrusted)
click_row_control "$(mid "$TRUST_NETWORK_ROW")"
assert_grew AddCurrentNetworkTrusted client "$ADD_BEFORE"
shot /output/06-add-current-network.png
echo "PHASE 5 PASS: AddCurrentNetworkTrusted leaves the settings window"

# ── Phase 6: the Environment page's keystore probe ────────────────────────────
click_nav "$NAV_ENVIRONMENT_INDEX"
shot /output/07-environment-page.png

ENV_TOGGLE_ROW=$PAGE_HEADER_H
ENV_ACTION_ROW=$(( ENV_TOGGLE_ROW + CONTROL_ROW_H ))

PREFLIGHT_BEFORE=$(count_client EnvPreflight)
RESULT_BEFORE=$(count_server EnvPreflightResult)
click_row_control "$(mid "$ENV_ACTION_ROW")"
assert_grew EnvPreflight client "$PREFLIGHT_BEFORE"
assert_grew EnvPreflightResult server "$RESULT_BEFORE"
shot /output/08-env-preflight.png
echo "PHASE 6 PASS: the keystore-availability action sends EnvPreflight and the"
echo "              server's EnvPreflightResult renders in the status line"

# ── Phase 7: the toggle's ON transition is gated on the same probe ────────────
PREFLIGHT_BEFORE=$(count_client EnvPreflight)
click_row_control "$(mid "$ENV_TOGGLE_ROW")"
assert_grew EnvPreflight client "$PREFLIGHT_BEFORE"
shot /output/09-env-toggle-gated.png
echo "PHASE 7 PASS: enabling env persistence runs the EnvPreflight gate first"

echo ""
echo "PASS: visual settings trust/preflight test"
echo "  Inspect screenshots in test-output/:"
echo "    01-settings-open.png     — settings window on its default page"
echo "    02-remote-trust.png      — Remote page with both server-fed lists"
echo "    03-trust-refreshed.png   — explicit refresh"
echo "    04-network-removed.png   — trusted network removed"
echo "    05-device-revoked.png    — approved device revoked"
echo "    06-add-current-network.png — trust-current-network result"
echo "    07-environment-page.png  — Environment page"
echo "    08-env-preflight.png     — manual keystore probe result"
echo "    09-env-toggle-gated.png  — gated env-persistence toggle"
echo "  Wire record: test-output/share-wire.jsonl"
