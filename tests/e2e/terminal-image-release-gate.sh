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

# Every criterion in specs/020-terminal-images/spec.md, in document order, with
# the evidence that proves it. `just e2e-release-gate` re-derives the criterion
# count from the spec and refuses a table that has drifted from it, so a new
# acceptance criterion cannot land unmapped.
CRITERIA=(
    "US1.1|Kitty RGB/RGBA/PNG, chunking, bounded zlib, classic placements, placeholders, and Sixel implemented against primary specifications|contract.json framing.json kitty-decode-evidence.json sixel-decoder-evidence.json"
    "US1.2|The compatibility contract documents actions, encodings, placement, reset, numeric limits, and deliberate exclusions|contract.json"
    "US1.3|Running-client fixtures display both protocols with stable visible output over direct PTY and SSH|linux/client/client.json functional.json"
    "US1.4|Yazi placeholders, a dual-protocol previewer, a plotting workflow, and protocol fixtures form the interoperability corpus|linux/apps/apps.json contract.json"
    "US1.5|Unsupported, malformed, truncated, or over-budget sequences never corrupt adjacent text or crash the client|framing.json transfer-lifecycle.json functional.json"
    "US2.1|Required query replies use the selected protocol's specified framing and values|contract.json replies-sharing.json"
    "US2.2|Each protocol reply is emitted exactly once, in byte order, to the originating PTY|replies-sharing.json"
    "US2.3|Typed IPC carries bounded replay and render state without exposing a Scribe-specific application protocol|ipc.json state-seam-ipc.json"
    "US2.4|Capability claims expose only implemented subsets; excluded transports and actions are never advertised|contract.json replies-sharing.json"
    "US2.5|Runtime policy including the master kill switch controls truthful advertising|settings.json functional.json"
    "US2.6|Split, reconnect, and concurrent-pane validation proves replies reach only the originating session in order|replies-sharing.json observer-parity.json"
    "US3.1|Placement geometry derives from cells and pane metrics, clips to the viewport, and follows z-order rules|client-scene.json linux/renderer/renderer.json"
    "US3.2|Scroll, erase, delete, reset, alternate screen, resize, and pane destruction update image state|mutations.json linux/renderer/renderer.json"
    "US3.3|Text stays selectable and copyable without image data leaking into copied text|client-scene.json linux/client/client.json"
    "US3.4|Surrounding text survives a disabled or rejected image beside a non-payload diagnostic affordance|settings.json linux/client/client.json"
    "US3.5|Protocol state is isolated per server session and GPU resources are isolated per pane view|convergence.json server-state-manifest.json linux/renderer/renderer.json"
    "US4.1|Lengths, decoded bytes, dimensions, multiplication, accumulation, retention, placements, and budgets are checked|accounting.json kitty-decode-evidence.json sixel-decoder-evidence.json"
    "US4.2|Decode and decompression run outside the GPUI paint path; only bounded finished resources reach upload|scheduler.json linux/gpui-spike.json"
    "US4.3|Per-pane eviction is deterministic, protocol-correct, and never evicts another pane's content|server-state-manifest.json linux/renderer/renderer.json"
    "US4.4|File, temporary-file, shared-memory, URL, network, and every other indirect transport is refused|contract.json kitty-decode-evidence.json"
    "US4.5|Decoder selection may patch, vendor, or replace upstream to enforce caller-controlled limits|kitty-decode-evidence.json sixel-decoder-evidence.json"
    "US4.6|Corpus-based malformed-input validation covers framing, chunking, decompression, dimensions, and deletion|framing.json kitty-decode-evidence.json sixel-decoder-evidence.json mutations.json"
    "US5.1|Named measurements compare text-only throughput, input latency, CPU use, and frame stability|performance.json linux/client/frame-stability.json"
    "US5.2|Named measurements record decode latency, upload latency, peak retained memory, and eviction|performance.json linux/gpui-spike.json"
    "US5.3|Release review records whether the measurements show a material regression, without invented thresholds|performance.json"
    "US5.4|Exact numeric security limits stay mandatory and distinct from qualitative performance review|contract.json accounting.json performance.json"
    "US5.5|Bounded server state survives detach, reattach, replay, client restart, and simultaneous viewers|replay.json client-replay.json handoff.json"
    "US5.6|Closing or replacing a pane releases every image resource that view held|mutations.json linux/renderer/renderer.json"
    "US6.1|Linux X11 and Wayland behavior is verified only through the Docker E2E harness|linux/client/client.json linux/renderer/renderer.json"
    "US6.2|Native macOS build and runtime verification completes before default-on release|macos/metal.json"
    "US6.3|Linux and macOS advertise the same verified protocol subset at release|contract.json macos/metal.json"
    "US6.4|Platform texture formats, scale factors, and GPU limits do not change protocol-visible semantics|linux/gpui-spike.json macos/metal.json"
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
    rest=${row#*|}
    files=${rest#*|}
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
        rest=${row#*|}
        text=${rest%%|*}
        files=${rest#*|}
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
        printf '    "%s": { "criterion": "%s", "evidence": [' "$id" "$text"
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
