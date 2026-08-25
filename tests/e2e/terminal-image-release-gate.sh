#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-release-gate)." >&2
    exit 99
}
# @lat: [[test#Test Harness#Terminal Image Release Gate]]
#
# Assembles test-output/terminal-images/release-manifest.json: the
# machine-readable counterpart of the human Evidence Index. It runs no Scribe
# runtime of its own — every claim it publishes is read back out of evidence a
# sibling gate already wrote into the same output directory, so the manifest
# can only be green when those gates ran and passed together.
set -euo pipefail

OUT=/output/terminal-images
MANIFEST=$OUT/release-manifest.json
CANDIDATE=${SCRIBE_RELEASE_CANDIDATE_SHA:?the release gate needs the candidate SHA}
EXPECTED=${SCRIBE_RELEASE_CRITERIA:?the release gate needs the spec criterion count}

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

[ -d "$OUT" ] || fail "no evidence directory at $OUT; run the terminal-image gates first"

# Evidence mapping only. Specification prose stays solely in spec.md;
# `just e2e-release-gate` re-derives its criterion count so a new criterion
# still cannot land unmapped.
CRITERIA=(
    "US1.1|contract.json framing.json kitty-decode-evidence.json sixel-decoder-evidence.json"
    "US1.2|contract.json"
    "US1.3|linux/client/client.json functional.json"
    "US1.4|linux/apps/apps.json contract.json"
    "US1.5|framing.json transfer-lifecycle.json functional.json"
    "US2.1|contract.json replies-sharing.json"
    "US2.2|replies-sharing.json"
    "US2.3|ipc.json state-seam-ipc.json"
    "US2.4|contract.json replies-sharing.json"
    "US2.5|settings.json functional.json"
    "US2.6|replies-sharing.json observer-parity.json"
    "US3.1|client-scene.json linux/renderer/renderer.json"
    "US3.2|mutations.json linux/renderer/renderer.json"
    "US3.3|client-scene.json linux/client/client.json"
    "US3.4|settings.json linux/client/client.json"
    "US3.5|convergence.json server-state-manifest.json linux/renderer/renderer.json"
    "US4.1|accounting.json kitty-decode-evidence.json sixel-decoder-evidence.json"
    "US4.2|scheduler.json linux/renderer/renderer.json"
    "US4.3|mutations.json server-state-manifest.json linux/renderer/renderer.json"
    "US4.4|contract.json kitty-decode-evidence.json"
    "US4.5|kitty-decode-evidence.json sixel-decoder-evidence.json"
    "US4.6|framing.json kitty-decode-evidence.json sixel-decoder-evidence.json mutations.json"
    "US5.1|performance.json linux/client/frame-stability.json"
    "US5.2|performance.json linux/renderer/renderer.json"
    "US5.3|performance.json"
    "US5.4|contract.json accounting.json performance.json"
    "US5.5|replay.json client-replay.json handoff.json"
    "US5.6|mutations.json linux/renderer/renderer.json"
    "US6.1|linux/client/client.json linux/renderer/renderer.json"
    "US6.2|macos/metal.json"
    "US6.3|contract.json macos/metal.json"
    "US6.4|linux/renderer/renderer.json macos/metal.json"
)

# The literal that certifies an artifact. Most gates publish a typed status;
# the rest are keyed on the load-bearing fact the gate exists to record. The
# native manifest is keyed on the candidate SHA, so a green run against an
# older tree can never satisfy this gate.
marker_for() {
    case "$1" in
        contract.json) printf '"contract_version": "terminal-images-v1"' ;;
        client-scene.json) printf '"typed_quota_error": true' ;;
        performance.json) printf '"status": "measured"' ;;
        linux/gpui-spike.json) printf '"render_image_reuse": true' ;;
        linux/renderer/renderer.json) printf '"renderer_boundary": "production-committed-image-scene"' ;;
        linux/apps/apps.json) printf '"terminal_spoofing": false' ;;
        linux/client/client.json) printf '"surface": "running_client"' ;;
        linux/client/frame-stability.json) printf '"scene_present_in_every_idle_frame": true' ;;
        macos/metal.json) printf '"candidate_sha": "%s"' "$CANDIDATE" ;;
        *) printf '"status": "pass"' ;;
    esac
}

declare -A GREEN=()
declare -A REASON=()

certify() {
    local file="$1" path="$OUT/$1" marker
    [ -n "${GREEN[$file]+set}" ] && return 0
    marker=$(marker_for "$file")
    if [ ! -s "$path" ]; then
        GREEN[$file]=0
        REASON[$file]="missing"
    elif ! grep -Fq "$marker" "$path"; then
        GREEN[$file]=0
        # Markers are JSON fragments and carry quotes; escape them or the
        # failure path publishes a manifest no parser can read — exactly when
        # a reviewer needs to read it.
        REASON[$file]="did not record ${marker//\"/\\\"}"
    else
        GREEN[$file]=1
        REASON[$file]="ok"
    fi
}

# ---------------------------------------------------------------------------
# Evaluate before writing anything, so the manifest reports one settled verdict.
# ---------------------------------------------------------------------------
declare -A SEEN=()
unproven=()
status=pass

[ "${#CRITERIA[@]}" = "$EXPECTED" ] \
    || fail "the criteria table maps ${#CRITERIA[@]} criteria but the spec states $EXPECTED"

for row in "${CRITERIA[@]}"; do
    id=${row%%|*}
    files=${row#*|}
    [[ "$id" =~ ^US[0-9]+\.[0-9]+$ ]] || fail "criterion id $id is malformed"
    [ -z "${SEEN[$id]+set}" ] || fail "criterion $id is mapped twice"
    SEEN[$id]=1
    [ -n "$files" ] || fail "criterion $id maps to no evidence"
    for file in $files; do
        certify "$file"
        [ "${GREEN[$file]}" = "1" ] || { unproven+=("$id"); status=fail; break; }
    done
done

# ---------------------------------------------------------------------------
# Publish. A failed verdict still writes the manifest: a reviewer needs to see
# which criterion is unproven, not an absent file.
# ---------------------------------------------------------------------------
{
    printf '{\n'
    printf '  "schema_version": 1,\n'
    printf '  "gate": "terminal images release gate",\n'
    printf '  "spec": "specs/020-terminal-images/spec.md",\n'
    printf '  "candidate_sha": "%s",\n' "$CANDIDATE"
    printf '  "criteria_expected": %s,\n' "$EXPECTED"
    printf '  "criteria_mapped": %s,\n' "${#CRITERIA[@]}"
    printf '  "default_on_setting": "terminal.images.enabled",\n'
    printf '  "evidence": {\n'
    first=1
    for file in $(printf '%s\n' "${!GREEN[@]}" | sort); do
        [ "$first" = 1 ] || printf ',\n'
        first=0
        printf '    "%s": { "green": %s, "detail": "%s" }' \
            "$file" \
            "$([ "${GREEN[$file]}" = 1 ] && echo true || echo false)" \
            "${REASON[$file]}"
    done
    printf '\n  },\n'
    printf '  "criteria": {\n'
    first=1
    for row in "${CRITERIA[@]}"; do
        id=${row%%|*}
        files=${row#*|}
        verdict=pass
        detail=""
        for file in $files; do
            if [ "${GREEN[$file]}" != "1" ]; then
                verdict=fail
                detail="$file ${REASON[$file]}"
                break
            fi
        done
        [ "$first" = 1 ] || printf ',\n'
        first=0
        printf '    "%s": { "evidence": [' "$id"
        sep=""
        for file in $files; do
            printf '%s"%s"' "$sep" "$file"
            sep=", "
        done
        printf '], "status": "%s", "detail": "%s" }' "$verdict" "$detail"
    done
    printf '\n  },\n'
    printf '  "unproven": ['
    sep=""
    for id in ${unproven[@]+"${unproven[@]}"}; do
        printf '%s"%s"' "$sep" "$id"
        sep=", "
    done
    printf '],\n'
    printf '  "status": "%s"\n' "$status"
    printf '}\n'
} >"$MANIFEST"

if [ "$status" != pass ]; then
    printf 'FAIL: %s criteria are unproven: %s\n' \
        "${#unproven[@]}" "${unproven[*]}" >&2
    printf 'The manifest at %s records why.\n' "$MANIFEST" >&2
    exit 1
fi

echo "PASS: every specification criterion is proven; manifest at $MANIFEST"
