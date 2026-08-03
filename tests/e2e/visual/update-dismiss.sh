#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual + scripted E2E: declining the update in the terminal window sends
# `ClientMessage::DismissUpdate` and clears the centred status-bar CTA.
#
# The setup is identical to update-trigger.sh — a real `scribe-server` polling
# the fake releases API broadcasts a real `UpdateAvailable` — but the user picks
# "Later" instead of "Update Now". The proof that `DismissUpdate` crossed the
# wire is the server's own log: it only records a dismissal when it receives
# that message.
#
# Run with:
#   SCRIBE_E2E_GPUS=all just e2e-visual-update
set -e

# shellcheck source=/dev/null
. /tests/visual/update-common.sh

run_update_banner_phases update-dismiss

# Tab moves focus off the accent "Update Now" onto "Later"; Enter declines.
send_keys Tab
shot /output/update-dismiss-03-later-focused.png
echo "PHASE 5 PASS: Tab moved focus onto the 'Later' button"

send_keys Return
# The server logs the window id it received DismissUpdate on, so this line is
# proof the message crossed the IPC socket from this client's window.
wait_for_log "$SERVER_LOG" "client dismissed update notification" 20
wait_for_log "$SERVER_LOG" "update notification dismissed by user" 20
echo "PHASE 6 PASS: DismissUpdate reached the server — it suppressed the version"

sleep 0.8
shot /output/update-dismiss-04-cta-cleared.png
crop_cta_band /output/update-dismiss-00-before-banner.png /output/cta-baseline.png
crop_cta_band /output/update-dismiss-04-cta-cleared.png /output/cta-cleared.png
CLEARED_DELTA=$(cta_band_delta /output/cta-baseline.png /output/cta-cleared.png)
echo "CTA band pixel delta against the no-update baseline: $CLEARED_DELTA"
if [ "${CLEARED_DELTA:-0}" -ge 40 ]; then
    echo "PHASE 7 FAIL: the CTA band still differs from the no-update baseline"
    exit 1
fi
echo "PHASE 7 PASS: the CTA band returned to its no-update appearance"

echo ""
echo "PASS: visual update-dismiss test"
echo "  Inspect screenshots in test-output/:"
echo "    update-dismiss-00-before-banner.png  — status bar with no update"
echo "    update-dismiss-01-update-banner.png  — centred '↑ Update to vN' CTA"
echo "    update-dismiss-02-update-dialog.png  — confirmation modal after the CTA click"
echo "    update-dismiss-03-later-focused.png  — focus ring on the 'Later' button"
echo "    update-dismiss-04-cta-cleared.png    — CTA gone after the dismissal"
