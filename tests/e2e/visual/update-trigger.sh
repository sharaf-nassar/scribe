#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual + scripted E2E: the terminal window learns about an available update
# from a real server, offers it in the centred status-bar CTA, and sends
# `ClientMessage::TriggerUpdate` when the user confirms — after which the
# server's own download/verify progress drives the same CTA.
#
# Nothing about the update is faked on the client side: `scribe-server` polls
# the fake releases API (`fake-update-api.py`), decides on its own that a newer
# version exists, broadcasts `UpdateAvailable`, and later broadcasts
# `UpdateProgress` for each install step. The proof that `TriggerUpdate`
# actually crossed the wire is the server's own log: it only starts a download
# when it receives that message.
#
# Run with:
#   SCRIBE_E2E_GPUS=all just e2e-visual-update
set -e

# shellcheck source=/dev/null
. /tests/visual/update-common.sh

run_update_banner_phases update-trigger

# Default focus is the accent "Update Now" button, so Enter confirms.
send_keys Return
# The server logs the window id it received TriggerUpdate on, so this line is
# proof the message crossed the IPC socket from this client's window.
wait_for_log "$SERVER_LOG" "client triggered update" 20
wait_for_log "$SERVER_LOG" "user triggered update" 20
echo "PHASE 5 PASS: TriggerUpdate reached the server — it started a download"

# The server broadcasts UpdateProgress::Downloading before it fetches the asset;
# the fake API streams it slowly so the label is on screen to be captured.
wait_for_log "$CLIENT_LOG" "Downloading" 30
shot /output/update-trigger-03-downloading.png
echo "PHASE 6 PASS: UpdateProgress::Downloading reached the client and re-labelled the CTA"

# Verification of the deliberately-invalid signature fails, so the terminal
# state is UpdateProgress::Failed rather than an install inside the container.
wait_for_log "$CLIENT_LOG" "Failed" 60
sleep 0.5
shot /output/update-trigger-04-failed.png
echo "PHASE 7 PASS: UpdateProgress::Failed reached the client and re-labelled the CTA"

echo ""
echo "PASS: visual update-trigger test"
echo "  Inspect screenshots in test-output/:"
echo "    update-trigger-00-before-banner.png — status bar with no update"
echo "    update-trigger-01-update-banner.png — centred '↑ Update to vN' CTA"
echo "    update-trigger-02-update-dialog.png — confirmation modal after the CTA click"
echo "    update-trigger-03-downloading.png   — CTA showing 'Downloading...'"
echo "    update-trigger-04-failed.png        — CTA showing 'Update failed'"
