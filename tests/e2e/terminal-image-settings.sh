#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Image Settings and Diagnostics#Docker Evidence Entry Point]]
set -euo pipefail

FIXTURES=/tests/fixtures/terminal-images
EVIDENCE=/output/terminal-images/settings.json
RUN_LOG=/output/terminal-images/settings-run.log

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

# The probe writes the master switch through the shipped settings path, so it
# gets its own config root. Nothing else in the container shares it.
CONFIG_ROOT="$(mktemp -d)"
trap 'rm -rf "$CONFIG_ROOT"' EXIT

mkdir -p "$(dirname "$EVIDENCE")"
XDG_CONFIG_HOME="$CONFIG_ROOT" scribe-test terminal-image-settings \
    --fixtures "$FIXTURES" --evidence "$EVIDENCE" >"$RUN_LOG" 2>&1 \
    || {
        cat "$RUN_LOG" >&2
        fail "settings probe exited non-zero"
    }

[ -s "$EVIDENCE" ] || fail "settings probe did not write evidence"
grep -Fq '"schema_version": 1' "$EVIDENCE" || fail "evidence version drifted"
grep -Fq '"status": "pass"' "$EVIDENCE" || fail "settings evidence does not pass"
grep -Fq '"engine": "scribe image master switch, diagnostics, and settings"' "$EVIDENCE" \
    || fail "the probe did not run through the production settings seam"
grep -Fq '"payload_free": true' "$EVIDENCE" || fail "evidence claims retained payloads"

# Every acceptance behavior maps to a named case.
for case in default_on settings_toggle_present disable_then_reenable no_payload_on_disk \
    resource_release text_fallback_preserved no_false_kitty_claim no_false_da_claim \
    renderer_failure_cleanup localized_diagnostics payload_free_diagnostics; do
    grep -Fq "\"$case\": \"pass\"" "$EVIDENCE" || fail "case $case did not pass"
done

# The switch is default-on, lives behind a labelled toggle, and survives a
# disable/re-enable round trip through the shipped config writer.
grep -Fq '"control_key": "terminal.images.enabled"' "$EVIDENCE" \
    || fail "the settings key drifted"
grep -Fq '"control_kind": "toggle"' "$EVIDENCE" || fail "the master switch is not a toggle"
grep -Fq '"control_label": "Terminal images"' "$EVIDENCE" \
    || fail "the master switch has no localized label"
grep -Fq '"switch_round_trip": "default_on,disabled_off,reenabled_on"' "$EVIDENCE" \
    || fail "the master switch did not round-trip through the settings write path"
grep -Fq '"disabled_toml_line": "enabled = false"' "$EVIDENCE" \
    || fail "the disabled config line is not a plain boolean"
grep -Fq '"reenabled_toml_line": "enabled = true"' "$EVIDENCE" \
    || fail "the re-enabled config line is not a plain boolean"

# Disabling frees the session: decode admissions, retained buffers, and the
# committed scene all go, and a second release is a no-op.
grep -Fq '"transitions": "disabled_cleared_latch,unchanged,enabled"' "$EVIDENCE" \
    || fail "kill-switch transitions are wrong or repeated"
grep -Fq '"releases_state": true' "$EVIDENCE" \
    || fail "a latched disable did not report owed resource release"
grep -Fq '"definitions_after": 0' "$EVIDENCE" || fail "disabling kept committed definitions"
grep -Fq '"placements_after": 0' "$EVIDENCE" || fail "disabling kept committed placements"
grep -Fq '"session_requested_bytes_after": 0' "$EVIDENCE" \
    || fail "disabling left requested image storage charged"
grep -Fq '"session_observed_bytes_after": 0' "$EVIDENCE" \
    || fail "disabling left observed image storage charged"
grep -Fq '"pending_transfer_after": false' "$EVIDENCE" \
    || fail "disabling left a partial transfer behind"
grep -Fq '"second_release": "no_op"' "$EVIDENCE" \
    || fail "a second release after disable was not a no-op"

# The application's own text is untouched by the release.
grep -Fq '"text_outcome": "preserved,fallback_visible"' "$EVIDENCE" \
    || fail "releasing image state changed the terminal's text or its fallback"

# A disabled Scribe advertises nothing: no Kitty answer, no DA1 attribute 4,
# an empty connection subset, and no latch until it is turned back on.
grep -Fq '"disabled_kitty_replies": 0' "$EVIDENCE" \
    || fail "a disabled session answered a Kitty probe"
grep -Fq '"disabled_device_attributes": "\u001b[?6c"' "$EVIDENCE" \
    || fail "disabled DA1 still advertises Sixel"
grep -Fq '"enabled_device_attributes": "\u001b[?6;4c"' "$EVIDENCE" \
    || fail "enabled DA1 lacks Sixel attribute 4"
grep -Fq '"disabled_connection_subset_features": 0' "$EVIDENCE" \
    || fail "a disabled server advertised image features"
grep -Fq '"disabled_advertising": "no_runtime,no_features,no_latch"' "$EVIDENCE" \
    || fail "a disabled server advertised or latched an image capability"
grep -Fq '"relatched_after_reenable": true' "$EVIDENCE" \
    || fail "a capable viewer could not latch after re-enable"

# A failed window operation is a renderer failure; a bounded rejection is not.
grep -Fq '"renderer_failures": "paint,drop"' "$EVIDENCE" \
    || fail "a failed window operation was not treated as renderer failure"
grep -Fq '"bounded_rejection": "not_renderer_failure"' "$EVIDENCE" \
    || fail "a bounded rejection was mistaken for renderer failure"
grep -Fq '"paint_failure_reason": "RendererUnavailable"' "$EVIDENCE" \
    || fail "a renderer failure did not map to renderer_unavailable"
grep -Fq '"limit_rejection_reason": "QuotaExceeded"' "$EVIDENCE" \
    || fail "a view-limit rejection did not map to quota_exceeded"

# The localized catalog is exhaustive, distinct, and cannot interpolate.
grep -Fq '"reason_count": 19' "$EVIDENCE" || fail "the rejection taxonomy changed size"
grep -Fq '"distinct_messages": 19' "$EVIDENCE" \
    || fail "two rejection categories share one message"
grep -Fq '"unsafe_messages": 0' "$EVIDENCE" \
    || fail "a diagnostic message can interpolate runtime data"
grep -Fq '"policy_disabled_message": "Terminal images are turned off in Scribe settings."' \
    "$EVIDENCE" || fail "the disabled affordance has no localized message"
grep -Fq '"scene_notice": "Images are unavailable because the renderer failed."' "$EVIDENCE" \
    || fail "the renderer-failure affordance has no localized message"
grep -Fq '"placements_with_notice": 2' "$EVIDENCE" \
    || fail "the renderer-failure notice replaced the pane's scene"

# No image payload may reach the evidence, the run log, or the config file the
# settings path wrote. The fixture's only payload is a base64 red pixel.
for artifact in "$EVIDENCE" "$RUN_LOG"; do
    if grep -Fq '/wAA' "$artifact"; then
        fail "$artifact leaked the fixture image payload"
    fi
    if grep -qE '"(payload|bytes|data|rgba|pixels)": *\[' "$artifact"; then
        fail "$artifact embedded image payload data"
    fi
done
saved_config="$(find "$CONFIG_ROOT" -name config.toml -type f | head -n 1)"
[ -n "$saved_config" ] || fail "the settings path wrote no config file"
grep -Fq '[terminal.images]' "$saved_config" || fail "the switch table is not on disk"
if grep -Fq '/wAA' "$saved_config"; then
    fail "the saved config leaked image payload"
fi
if grep -q $'\033_G' "$saved_config"; then
    fail "the saved config leaked a graphics control string"
fi

echo "PASS: terminal image settings and diagnostics at $EVIDENCE"
