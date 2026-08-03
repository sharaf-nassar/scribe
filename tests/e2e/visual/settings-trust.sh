#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual + scripted E2E test: the grouped navigation, search, trusted-device,
# trusted-network, and env-preflight surfaces of the redesigned GPUI settings
# window.
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
# a real gesture, and that the server's reply is rendered back into the page
# (screenshots plus the window's status line).
#
# EVERY control is reached through the window's own semantic targets — the
# search field (Ctrl+K) and the keyboard focus ring — never through a pixel
# offset into a page. `SettingsWindow::focus_targets` defines one stable
# traversal order (visible nav pages, then the Remote page's live trust
# actions, then the selected page's actionable controls), so a phase says "the
# second focus target while the search reads `remote`" instead of "48px below
# the page header". The redesigned window is free to move any of it: grouped
# nav, compact geometry, custom client chrome and the search bar can all be
# re-laid-out without touching this file, and no production layout
# constant exists to keep a click landing here.
#
# The one pointer gesture is a click on the empty sidebar background below the
# last nav item. It is deliberately inert — it hands the GPUI root its focus
# handle (nothing is focused when the window opens) and resets `focus_index` to
# 0 through `clear_keyboard_navigation`, so every phase counts its Down presses
# from a known origin. Phase 0 asserts it puts nothing on the wire.
#
# The container seeds one trusted network and one approved device before the
# server starts (SCRIBE_SEED_TRUST=1, see docker/entrypoint-visual.sh): a single
# machine has no peer to approve and no fingerprintable Wi-Fi, so without the
# seed the Remove/Revoke rows would never exist to reach.
#
# Requires: visual container with SCRIBE_VISUAL_APP=settings, SCRIBE_SHARE_TAP=1,
# SCRIBE_SEED_TRUST=1, xdotool, scrot, python3.
set -e

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
SEEDED_NETWORK_ID="seeded-network-1"
SEEDED_DEVICE_ID="1f2e3d4c5b6a798877665544332211000f1e2d3c4b5a69788796a5b4c3d2e1f0"

# ── Focus-order targets ───────────────────────────────────────────────────────
# Offsets into `SettingsWindow::focus_targets`, i.e. how many Down presses to
# make from a freshly reset focus. They are page-model positions, not pixels.
#
# `settings_nav_pages()` order: Appearance, Colors, Terminal, Keybindings, AI,
# Environment, Workspaces, Updates, Releases, Notifications, Remote.
NAV_STEPS_TO_REMOTE=10
# With the search reading `remote` exactly one nav page survives the filter, so
# the Remote page's live trust actions follow it immediately:
#   0 Remote (nav)  1 Refresh trust state  2 Trust this network
#   3 Remove <trusted network>  4 Revoke <approved device>
REMOTE_REFRESH_STEPS=1
REMOTE_ADD_NETWORK_STEPS=2
REMOTE_REMOVE_NETWORK_STEPS=3
REMOTE_REVOKE_DEVICE_STEPS=4
# A search that names one control leaves that control as the only target after
# its page, so the first Down always lands on it.
FIRST_CONTROL_STEPS=1

WIN=""
SEED_X=0
SEED_Y=0

find_window() {
    local wid
    wid=$(xdotool search --name 'Scribe Settings' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

raise_window() {
    if [ -z "$WIN" ]; then
        WIN=$(find_window)
    fi
    if [ -z "$WIN" ]; then
        echo "FAIL: no Scribe Settings window found" >&2
        exit 1
    fi
    xdotool windowactivate --sync "$WIN" 2>/dev/null \
        || xdotool windowfocus --sync "$WIN" 2>/dev/null || true
    sleep 0.3
}

# The settings window is the app here, so its WM title is also the assertion
# that the right process is under the camera.
assert_titled() {
    local name
    name=$(xdotool getwindowname "$WIN")
    if [ "$name" != "Scribe Settings" ]; then
        echo "FAIL: window $WIN is titled '$name', not 'Scribe Settings'" >&2
        exit 1
    fi
}

# Pick the inert sidebar point used to seed focus. The sidebar is the window's
# left column and its nav list is top-aligned, so a point near the bottom of the
# left edge is background under any plausible grouping — the list would have to
# more than double in height before it reached this far.
locate_seed_point() {
    local X=0 Y=0 WIDTH=0 HEIGHT=0
    eval "$(xdotool getwindowgeometry --shell "$WIN")"
    SEED_X=$(( X + 60 ))
    SEED_Y=$(( Y + HEIGHT - 100 ))
    echo "window ${WIDTH}x${HEIGHT} at ${X},${Y}; focus seed at ${SEED_X},${SEED_Y}"
}

# Resolve the Docker bridge's real default-gateway MAC before the first
# `GetLanEnv`. A fresh network namespace has an empty neighbour table, so
# netdev otherwise reports the gateway MAC as zero and the production settings
# model correctly omits the unavailable "Trust it" focus target. That shifts
# every later semantic target by one and makes four Down presses pass Revoke.
# A refused TCP connect is sufficient: ARP resolution happens before the port
# result, and the `/proc/net/arp` check makes the fixture precondition explicit.
prime_gateway_neighbor() {
    python3 - <<'PY'
import socket
import time

gateway = None
with open("/proc/net/route") as routes:
    next(routes, None)
    for line in routes:
        fields = line.split()
        if len(fields) >= 3 and fields[1] == "00000000":
            gateway = socket.inet_ntoa(bytes.fromhex(fields[2])[::-1])
            break

if gateway is None:
    raise SystemExit("FAIL: Docker E2E has no default gateway to fingerprint")

for _ in range(10):
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.settimeout(0.2)
        try:
            probe.connect((gateway, 9))
        except OSError:
            pass
    with open("/proc/net/arp") as neighbors:
        for line in neighbors:
            fields = line.split()
            if len(fields) >= 4 and fields[0] == gateway and fields[3] != "00:00:00:00:00:00":
                print(f"primed Docker gateway neighbor {gateway} at {fields[3]}")
                raise SystemExit(0)
    time.sleep(0.1)

raise SystemExit(f"FAIL: Docker gateway {gateway} did not resolve to a MAC")
PY
}

shot() {
    raise_window
    sleep 0.3
    scrot -o "$1"
    echo "captured $1"
}

# Give the GPUI root its focus handle and reset the keyboard traversal to its
# first target. Nothing else is ever clicked in this script.
reset_focus() {
    raise_window
    xdotool mousemove "$SEED_X" "$SEED_Y"
    sleep 0.2
    xdotool click 1
    sleep 0.4
}

key() {
    xdotool key --clearmodifiers "$@"
    sleep 0.3
}

# Focus the search field through its own shortcut and replace the query. Escape
# is the field's deliberate clear-query key and returns focus to the root, so
# focus it again before typing the replacement.
search_for() {
    raise_window
    key ctrl+k
    key Escape
    key ctrl+k
    xdotool type --clearmodifiers --delay 40 "$1"
    sleep 0.8
    echo "search reads '$1'"
}

# Walk $1 focus targets forward and activate the one we land on. Each activation
# runs a synchronous server round-trip on the UI thread, so allow the full 3s
# SERVER_ACTION_TIMEOUT plus render time to settle.
activate_target() {
    local steps="$1" i
    for (( i = 0; i < steps; i++ )); do
        xdotool key --clearmodifiers Down
        sleep 0.15
    done
    sleep 0.3
    xdotool key --clearmodifiers Return
    sleep 1.7
}

# Count recorded client frames of a given message type.
count_client() {
    python3 - "$RECORD" "$@" <<'PY'
import json, sys
path, wanted = sys.argv[1], sys.argv[2]
def norm(value):
    try:
        return json.loads(value)
    except ValueError:
        return value

pairs = [(k, norm(v)) for k, v in (p.split("=", 1) for p in sys.argv[3:])]
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
            msg = row.get("message", {})
            if all(msg.get(k) == v for k, v in pairs):
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

# Total recorded client frames, whatever their type.
count_client_total() {
    python3 - "$RECORD" <<'PY'
import json, sys
total = 0
try:
    fh = open(sys.argv[1])
except OSError:
    print(0)
    sys.exit(0)
with fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") == "client":
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

# Fail unless exactly one matching client frame was added after the gesture.
assert_client_grew_once() {
    local kind="$1" before="$2" now expected
    shift 2
    now=$(count_client "$kind" "$@")
    expected=$(( before + 1 ))
    if [ "$now" -ne "$expected" ]; then
        echo "FAIL: expected exactly one new client $kind frame on the wire" >&2
        echo "      (was $before, now $now; filters: ${*:-none})" >&2
        exit 1
    fi
}

prime_gateway_neighbor
sleep 1.5
raise_window
assert_titled
locate_seed_point
scrot -o /output/01-settings-open.png
echo "captured /output/01-settings-open.png"

# ── Phase 0: the focus seed is inert ──────────────────────────────────────────
# Everything downstream counts Down presses from this click, so it has to be
# proven to do nothing but hand over focus: no server action, no page change.
TOTAL_BEFORE=$(count_client_total)
reset_focus
shot /output/02-focus-seeded.png
TOTAL_AFTER=$(count_client_total)
if [ "$TOTAL_AFTER" -ne "$TOTAL_BEFORE" ]; then
    echo "FAIL: the sidebar focus seed sent $(( TOTAL_AFTER - TOTAL_BEFORE )) frame(s)" >&2
    exit 1
fi
echo "PHASE 0 PASS: the sidebar focus seed is inert (no frames on the wire)"

# ── Phase 1: grouped navigation reaches Remote by keyboard ────────────────────
# Walking the whole nav list proves the grouped sidebar is one continuous
# traversal (group headings are not focus stops) and that landing on Remote
# pulls the trust surface exactly as the old webview's load-time inject did.
LAN_BEFORE=$(count_client GetLanEnv)
NETS_BEFORE=$(count_client ListTrustedNetworks)
DEVS_BEFORE=$(count_client ListTrustedDevices)
NET_LIST_BEFORE=$(count_server TrustedNetworkList)
DEV_LIST_BEFORE=$(count_server TrustedDeviceList)

activate_target "$NAV_STEPS_TO_REMOTE"
shot /output/03-remote-trust.png

assert_grew GetLanEnv client "$LAN_BEFORE"
assert_grew ListTrustedNetworks client "$NETS_BEFORE"
assert_grew ListTrustedDevices client "$DEVS_BEFORE"
assert_grew TrustedNetworkList server "$NET_LIST_BEFORE"
assert_grew TrustedDeviceList server "$DEV_LIST_BEFORE"
echo "PHASE 1 PASS: keyboard-traversing the grouped nav to Remote puts"
echo "              ListTrustedNetworks + ListTrustedDevices (and GetLanEnv)"
echo "              on the wire and renders both replies"

# ── Phase 2: the search field reaches the same page and its Refresh action ────
# Ctrl+K focuses the search field from anywhere in the window, the query narrows
# the nav to the matching page, and the first focus target after that page is
# the trust section's Refresh control. Re-querying proves both.
search_for "remote"
shot /output/04-search-remote.png
NETS_BEFORE=$(count_client ListTrustedNetworks)
DEVS_BEFORE=$(count_client ListTrustedDevices)
reset_focus
activate_target "$REMOTE_REFRESH_STEPS"
assert_grew ListTrustedNetworks client "$NETS_BEFORE"
assert_grew ListTrustedDevices client "$DEVS_BEFORE"
shot /output/05-trust-refreshed.png
echo "PHASE 2 PASS: Ctrl+K search narrows the nav to Remote and its Refresh"
echo "              control re-issues the trust queries"

# ── Phase 3: revoking the seeded approved device ──────────────────────────────
# Revoked before the network is removed: the device rows follow the network rows
# in the traversal, so shrinking the network list first would move them.
REVOKE_BEFORE=$(count_client RevokeTrustedDevice)
REVOKE_MATCH_BEFORE=$(count_client RevokeTrustedDevice "device_id=$SEEDED_DEVICE_ID")
reset_focus
activate_target "$REMOTE_REVOKE_DEVICE_STEPS"
assert_client_grew_once RevokeTrustedDevice "$REVOKE_BEFORE"
assert_client_grew_once RevokeTrustedDevice "$REVOKE_MATCH_BEFORE" \
    "device_id=$SEEDED_DEVICE_ID"
assert_client_frame RevokeTrustedDevice "device_id=$SEEDED_DEVICE_ID" >/dev/null
shot /output/06-device-revoked.png
echo "PHASE 3 PASS: the approved device's Revoke control sends"
echo "              RevokeTrustedDevice{device_id=$SEEDED_DEVICE_ID}"

# ── Phase 4: removing the seeded trusted network ──────────────────────────────
REMOVE_BEFORE=$(count_client RemoveTrustedNetwork)
reset_focus
activate_target "$REMOTE_REMOVE_NETWORK_STEPS"
assert_grew RemoveTrustedNetwork client "$REMOVE_BEFORE"
assert_client_frame RemoveTrustedNetwork "id=$SEEDED_NETWORK_ID" >/dev/null
shot /output/07-network-removed.png
echo "PHASE 4 PASS: the seeded network's Remove control sends"
echo "              RemoveTrustedNetwork{id=$SEEDED_NETWORK_ID}"

# ── Phase 5: trusting the current network ─────────────────────────────────────
# Last of the Remote-page phases on purpose: if the container's network happens
# to be fingerprintable the server adds a row, and no later phase may depend on
# the length of either trust list.
ADD_BEFORE=$(count_client AddCurrentNetworkTrusted)
reset_focus
activate_target "$REMOTE_ADD_NETWORK_STEPS"
assert_grew AddCurrentNetworkTrusted client "$ADD_BEFORE"
shot /output/08-add-current-network.png
echo "PHASE 5 PASS: AddCurrentNetworkTrusted leaves the settings window"

# ── Phase 6: the Environment page's keystore probe ────────────────────────────
# Searching for the control's own label crosses the grouped nav to a different
# section and leaves that control as the only target after its page.
search_for "keystore"
PREFLIGHT_BEFORE=$(count_client EnvPreflight)
RESULT_BEFORE=$(count_server EnvPreflightResult)
reset_focus
activate_target "$FIRST_CONTROL_STEPS"
assert_grew EnvPreflight client "$PREFLIGHT_BEFORE"
assert_grew EnvPreflightResult server "$RESULT_BEFORE"
shot /output/09-env-preflight.png
echo "PHASE 6 PASS: the keystore-availability action sends EnvPreflight and the"
echo "              server's EnvPreflightResult renders in the status line"

# ── Phase 7: the toggle's ON transition is gated on the same probe ────────────
search_for "persist environment"
PREFLIGHT_BEFORE=$(count_client EnvPreflight)
reset_focus
activate_target "$FIRST_CONTROL_STEPS"
assert_grew EnvPreflight client "$PREFLIGHT_BEFORE"
shot /output/10-env-toggle-gated.png
echo "PHASE 7 PASS: enabling env persistence runs the EnvPreflight gate first"

echo ""
echo "PASS: visual settings navigation/trust/preflight test"
echo "  Inspect screenshots in test-output/:"
echo "    01-settings-open.png       — settings window on its default page"
echo "    02-focus-seeded.png        — after the inert sidebar focus seed"
echo "    03-remote-trust.png        — Remote page with both server-fed lists"
echo "    04-search-remote.png       — nav narrowed by the Ctrl+K search"
echo "    05-trust-refreshed.png     — explicit refresh"
echo "    06-device-revoked.png      — approved device revoked"
echo "    07-network-removed.png     — trusted network removed"
echo "    08-add-current-network.png — trust-current-network result"
echo "    09-env-preflight.png       — manual keystore probe result"
echo "    10-env-toggle-gated.png    — gated env-persistence toggle"
echo "  Wire record: test-output/share-wire.jsonl"
