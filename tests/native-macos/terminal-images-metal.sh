#!/usr/bin/env bash
# @lat: [[test#Native macOS Metal Parity Corpus]]
#
# The sanctioned native corpus driver. `just native-macos-terminal-images`
# invokes it from `.github/workflows/native-macos-metal.yml` only; the guarded
# wrapper refuses every other context before this file runs. Nothing here may
# be executed on a developer workstation.
#
# It proves parity, not new behavior: the same frozen contract, the same owned
# fixtures, the same in-process protocol probes, and the same pinned
# applications the Linux Docker corpus runs, plus the Metal-only facts Docker
# cannot produce — the Metal renderer, real GPU upload of the maximum allowed
# image, max-plus-one rejection before any GPUI allocation, shared-source
# reuse, atlas recovery, and final-reference eviction.
set -euo pipefail

OUT=${SCRIBE_NATIVE_MACOS_OUTPUT_DIR:?native driver requires SCRIBE_NATIVE_MACOS_OUTPUT_DIR}
ROOT=$(git rev-parse --show-toplevel)
FIXTURES="$ROOT/tests/e2e/fixtures/terminal-images"
CONTRACT="$FIXTURES/contract.json"
PROTOCOL="$OUT/protocol"
APPS="$OUT/apps"
GPUI="$OUT/gpui"
WORK=${TMPDIR:-/tmp}/scribe-native-macos
SERVER_LOG="$APPS/server.log"
STEPS="$APPS/steps.tsv"
SPIKE_LOG="$GPUI/spike.log"

# Pinned exactly as `docker/Dockerfile.visual` pins them, so the two platforms
# assert against the same programs. Chafa and gnuplot build from the identical
# upstream tarballs; only Yazi has a per-architecture release artifact.
YAZI_VERSION=26.5.6
YAZI_SHA256=7abd71725e2fe27bed036becbf6ce79fa17964eb68491d34190011c94b8c7ca8
CHAFA_VERSION=1.18.2
CHAFA_SHA256=0b8d9ba9f347e8b6c0c71878217c9b0e478b4a42aa4babea0bf20840567239c2
GNUPLOT_VERSION=6.0.3
GNUPLOT_SHA256=ec52e3af8c4083d4538152b3f13db47f6d29929a3f6ecec5365c834e77f251ab

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# Phase 0: refuse every unsanctioned context before touching a Scribe binary.
#
# `tools/run-native-macos-terminal-images.sh` already asserts this. Repeating it
# here keeps the guarantee attached to the executable the workflow requires, so
# a direct invocation cannot reach a runtime call either.
# ---------------------------------------------------------------------------
[ "${GITHUB_ACTIONS:-}" = "true" ] || fail "native validation is authorized only in GitHub Actions"
[ "${RUNNER_OS:-}" = "macOS" ] || fail "native validation requires the macOS runner"
[ "${RUNNER_ARCH:-}" = "ARM64" ] || fail "native validation requires the ARM64 runner"
[ "${SCRIBE_NATIVE_MACOS_RUNNER:-}" = "github-actions-macos-14-xlarge" ] \
    || fail "native validation requires the sanctioned Metal runner marker"

mkdir -p "$PROTOCOL" "$APPS" "$GPUI" "$WORK"
: >"$SERVER_LOG"
: >"$STEPS"

RELEASE="$ROOT/target/release"
for binary in scribe-server scribe-client scribe-test; do
    [ -x "$RELEASE/$binary" ] || fail "missing release binary $RELEASE/$binary"
done
export PATH="$RELEASE:$PATH"

# BSD userland: no GNU sed escapes, no `timeout`, no `sha256sum`, no `nproc`.
plain() { perl -pe 's/\e\[[0-9;]*m//g'; }
sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
cpus() { sysctl -n hw.ncpu; }

cleanup() {
    kill "${SPIKE_PID:-}" 2>/dev/null || true
    scribe-test daemon stop >/dev/null 2>&1 || true
    scribe-test server stop >/dev/null 2>&1 || true
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Phase 1: the frozen contract is the same one Linux verified.
#
# `tests/e2e/terminal-image-contract.sh` owns the exhaustive field audit inside
# the container. Re-running that list here would fork it; instead the driver
# records the contract digest and asserts only the values this run itself
# exercises natively.
# ---------------------------------------------------------------------------
[ -f "$CONTRACT" ] || fail "missing frozen contract $CONTRACT"
CONTRACT_SHA=$(sha256 "$CONTRACT")
for needle in \
    '"contract_version": "terminal-images-v1"' \
    '"max_width_pixels": 4096' \
    '"max_height_pixels": 4096' \
    '"formats": [24, 32, 100]' \
    '"transports": ["direct"]' \
    '"placement": ["classic", "unicode_placeholder"]' \
    '"discovery": "da1_attribute_4_when_runtime_enabled"' \
    "\"version\": \"v$YAZI_VERSION\"" \
    "\"version\": \"$CHAFA_VERSION\"" \
    "\"version\": \"$GNUPLOT_VERSION\""
do
    grep -Fq "$needle" "$CONTRACT" || fail "contract does not pin $needle"
done
echo "PASS: frozen contract and pinned application versions ($CONTRACT_SHA)"

# ---------------------------------------------------------------------------
# Phase 2: the same in-process protocol corpus, compiled and run on ARM64
# macOS.
#
# Each probe drives production code and exits non-zero on any assertion
# mismatch, so the loop needs no second oracle. Running them here is what makes
# the supported-platform claim about this architecture rather than about x86-64
# Linux only.
# ---------------------------------------------------------------------------
PROBES=""
run_probe() {
    local name="$1"; shift
    scribe-test "$@" >"$PROTOCOL/$name.log" 2>&1 || {
        tail -20 "$PROTOCOL/$name.log" >&2
        fail "protocol probe $name did not pass natively"
    }
    [ -s "$PROTOCOL/$name.json" ] || fail "protocol probe $name wrote no evidence"
    PROBES="$PROBES $name"
}

run_probe framing image-framing --fixtures "$FIXTURES" --evidence "$PROTOCOL/framing.json"
run_probe ipc terminal-image-ipc --fixtures "$FIXTURES/ipc.json" --output "$PROTOCOL/ipc.json"
run_probe client-scene terminal-image-client-scene \
    --fixtures "$FIXTURES/client-scene.json" --output "$PROTOCOL/client-scene.json"
run_probe client-replay terminal-image-client-replay --evidence "$PROTOCOL/client-replay.json"
run_probe replay terminal-image-replay --fixtures "$FIXTURES" --evidence "$PROTOCOL/replay.json"
run_probe replies-sharing terminal-image-replies-sharing \
    --fixtures "$FIXTURES" --evidence "$PROTOCOL/replies-sharing.json"
run_probe handoff terminal-image-handoff --fixtures "$FIXTURES" --evidence "$PROTOCOL/handoff.json"
run_probe settings terminal-image-settings --fixtures "$FIXTURES" --evidence "$PROTOCOL/settings.json"
run_probe state-seam terminal-image-state-seam --evidence "$PROTOCOL/state-seam.json"
run_probe server-state terminal-image-server-state --evidence "$PROTOCOL/server-state.json"
run_probe accounting terminal-image-accounting --evidence "$PROTOCOL/accounting.json"
run_probe scheduler terminal-image-scheduler --evidence "$PROTOCOL/scheduler.json"
run_probe transfer-lifecycle terminal-image-transfer-lifecycle \
    --evidence "$PROTOCOL/transfer-lifecycle.json"
run_probe mutations terminal-image-mutations --evidence "$PROTOCOL/mutations.json"
run_probe convergence terminal-image-convergence --evidence "$PROTOCOL/convergence.json"
run_probe observer-parity terminal-image-observer-parity --evidence "$PROTOCOL/observer-parity.json"
run_probe kitty-decode kitty-decode --contract "$CONTRACT" --evidence "$PROTOCOL/kitty-decode.json"
run_probe sixel-decoder sixel-decoder --contract "$CONTRACT" --fixtures "$FIXTURES" \
    --evidence "$PROTOCOL/sixel-decoder.json"
echo "PASS: native protocol corpus —$PROBES"

# ---------------------------------------------------------------------------
# Phase 3: the pinned applications, built from the pinned sources.
#
# The runner image carries neither Yazi nor a Sixel-capable gnuplot, so the
# corpus is provisioned here from the same versions and checksums the Docker
# image pins. A silently republished artifact fails the gate instead of
# quietly changing what the corpus proves.
# ---------------------------------------------------------------------------
CORPUS="$WORK/corpus"
PREFIX="$WORK/prefix"
export PATH="$PREFIX/bin:$PATH"
if [ ! -x "$PREFIX/bin/yazi" ]; then
    brew install --quiet glib gd pkgconf >/dev/null
    mkdir -p "$CORPUS" "$PREFIX"
    (
        cd "$CORPUS"
        curl -fsSL -o yazi.zip \
            "https://github.com/sxyazi/yazi/releases/download/v${YAZI_VERSION}/yazi-aarch64-apple-darwin.zip"
        echo "${YAZI_SHA256}  yazi.zip" | shasum -a 256 -c -
        unzip -q -o yazi.zip
        mkdir -p "$PREFIX/bin"
        install -m 0755 yazi-aarch64-apple-darwin/yazi yazi-aarch64-apple-darwin/ya "$PREFIX/bin/"

        curl -fsSL -o chafa.tar.xz \
            "https://github.com/hpjansson/chafa/releases/download/${CHAFA_VERSION}/chafa-${CHAFA_VERSION}.tar.xz"
        echo "${CHAFA_SHA256}  chafa.tar.xz" | shasum -a 256 -c -
        tar xf chafa.tar.xz
        cd "chafa-${CHAFA_VERSION}"
        ./configure --prefix="$PREFIX" --without-imagemagick >/dev/null
        make -j"$(cpus)" >/dev/null
        make install >/dev/null
        cd "$CORPUS"

        curl -fsSL -o gnuplot.tar.gz \
            "https://downloads.sourceforge.net/project/gnuplot/gnuplot/${GNUPLOT_VERSION}/gnuplot-${GNUPLOT_VERSION}.tar.gz"
        echo "${GNUPLOT_SHA256}  gnuplot.tar.gz" | shasum -a 256 -c -
        tar xf gnuplot.tar.gz
        cd "gnuplot-${GNUPLOT_VERSION}"
        ./configure --prefix="$PREFIX" --without-x --without-qt --without-wx \
            --without-latex --with-gd >/dev/null
        make -j"$(cpus)" >/dev/null
        make install >/dev/null
    ) >"$APPS/provision.log" 2>&1 || {
        tail -30 "$APPS/provision.log" >&2
        fail "the pinned application corpus did not build on the runner"
    }
fi

yazi --version | grep -qF "Yazi $YAZI_VERSION" || fail "yazi is not the pinned $YAZI_VERSION"
chafa --version | grep -qF "Chafa version $CHAFA_VERSION" || fail "chafa is not the pinned $CHAFA_VERSION"
gnuplot --version | grep -qF 'gnuplot 6.0 patchlevel 3' || fail "gnuplot is not the pinned $GNUPLOT_VERSION"
gnuplot -e 'set print "-"; print GPVAL_TERMINALS' 2>&1 | grep -qw sixelgd \
    || fail "the pinned gnuplot has no sixelgd terminal"

# A 64x64 red PNG without ImageMagick: the runner has python3 and zlib, and the
# corpus only needs one decodable raster to hand each previewer. Yazi previews a
# directory, so the raster lives in one.
mkdir -p "$WORK/pictures"
python3 - "$WORK/pictures/red.png" <<'PY'
import struct
import sys
import zlib

width = height = 64
raw = b"".join(b"\x00" + b"\xd0\x20\x20" * width for _ in range(height))


def chunk(tag, payload):
    body = tag + payload
    return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body))


with open(sys.argv[1], "wb") as handle:
    handle.write(b"\x89PNG\r\n\x1a\n")
    handle.write(chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)))
    handle.write(chunk(b"IDAT", zlib.compress(raw, 9)))
    handle.write(chunk(b"IEND", b""))
PY

# The owned Unicode-placeholder fixture, not an application: no released
# terminal-image application drives Kitty virtual placements through an
# unrecognised terminal, so placeholder semantics stay pinned to Scribe's own
# frozen bytes on this platform too.
python3 - "$FIXTURES/kitty-unicode-placeholder.hex" "$WORK/placeholder.bin" <<'PY'
import pathlib
import sys

source, destination = (pathlib.Path(argument) for argument in sys.argv[1:])
destination.write_bytes(bytes.fromhex(source.read_text().strip()))
PY

# ---------------------------------------------------------------------------
# Phase 4: a live native server, a capable viewer, and the pinned corpus on a
# real macOS PTY.
#
# Only a capable viewer latches a session, and only a latched session parses
# graphics at all, so the harness daemon announces the renderer subset exactly
# as the Linux corpus does. Evidence is read from the server's own counters:
# the native run asserts protocol effects, not pixels, because window capture
# needs an interactive TCC grant no hosted runner has.
# ---------------------------------------------------------------------------
export SCRIBE_TEST_SERVER_LOG="$SERVER_LOG"
export SCRIBE_TERMINAL_IMAGES=1
scribe-test server start || fail "the native scribe-server did not start"
scribe-test daemon start || fail "the capable harness viewer did not start"
SESSION=$(scribe-test session create)

# Highest value a named evidence field reached in the server log at or after
# line $2. The leading space keeps `classic_placements` from also matching
# `placeholder_placements`.
log_field_max() {
    local field="$1" value
    value=$(tail -n "+${2:-1}" "$SERVER_LOG" | plain \
        | sed -n "s/.* $field=\([0-9][0-9]*\).*/\1/p" | sort -n | tail -1)
    printf '%s' "${value:-0}"
}
log_lines() { wc -l <"$SERVER_LOG" | tr -d ' '; }

wait_field_at_least() {
    local field="$1" want="$2" from="$3" deadline=$((SECONDS + ${4:-20}))
    until [ "$(log_field_max "$field" "$from")" -ge "$want" ]; do
        [ "$SECONDS" -lt "$deadline" ] || return 1
        sleep 0.2
    done
}

# `\x15` clears whatever the terminal wrote back onto the input line first: a
# graphics reply is delivered as input, and an unread one would turn the next
# command into a syntax error.
run_step() {
    local name="$1" body="$2" deadline
    printf 'clear\n%s\ntouch %s/done-%s\n' "$body" "$WORK" "$name" >"$WORK/$name.sh"
    rm -f "$WORK/done-$name"
    scribe-test send "$SESSION" "\x15bash $WORK/$name.sh\n"
    deadline=$((SECONDS + 60))
    until [ -e "$WORK/done-$name" ]; do
        [ "$SECONDS" -lt "$deadline" ] || fail "$name never completed in the native session"
        sleep 0.2
    done
    sleep 1
}

step_transfers=0
step_sixels=0
step_replies=0
step_commands=0
step_failures=0

assert_step() {
    local name="$1" want_kitty="$2" want_sixel="$3" want_replies="$4" want_commands="$5"
    local want_kind="$6" mark="$7"
    local kitty sixel replies commands failures classic placeholder sixel_place kind_count

    wait_field_at_least kitty_transfers $((step_transfers + want_kitty)) "$mark" \
        || fail "$name transferred fewer than $want_kitty Kitty images"
    wait_field_at_least sixel_images $((step_sixels + want_sixel)) "$mark" \
        || fail "$name decoded fewer than $want_sixel Sixel images"
    wait_field_at_least replies $((step_replies + want_replies)) "$mark" \
        || fail "$name drew fewer than $want_replies PTY replies"
    wait_field_at_least kitty_commands $((step_commands + want_commands)) "$mark" \
        || fail "$name sent fewer than $want_commands Kitty commands"

    kitty=$(log_field_max kitty_transfers)
    sixel=$(log_field_max sixel_images)
    replies=$(log_field_max replies)
    commands=$(log_field_max kitty_commands)
    failures=$(log_field_max failures)
    classic=$(log_field_max classic_placements)
    placeholder=$(log_field_max placeholder_placements)
    sixel_place=$(log_field_max sixel_placements)

    [ "$failures" -le "$step_failures" ] \
        || fail "$name raised $((failures - step_failures)) typed graphics failures"

    case "$want_kind" in
        classic) kind_count="$classic" ;;
        placeholder) kind_count="$placeholder" ;;
        sixel) kind_count="$sixel_place" ;;
        *) fail "unknown placement kind $want_kind" ;;
    esac
    [ "$kind_count" -ge 1 ] \
        || fail "$name left no live $want_kind placement (classic=$classic placeholder=$placeholder sixel=$sixel_place)"

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$name" \
        "$((commands - step_commands))" "$((kitty - step_transfers))" \
        "$((sixel - step_sixels))" "$((replies - step_replies))" \
        "$classic" "$placeholder" "$sixel_place" "$want_kind" >>"$STEPS"
    step_transfers="$kitty"
    step_sixels="$sixel"
    step_replies="$replies"
    step_commands="$commands"
}

native_step() {
    local name="$1" body="$2" mark
    shift 2
    mark=$(($(log_lines) + 1))
    run_step "$name" "$body"
    assert_step "$name" "$@" "$mark"
}

# Prove typed input reaches the pane before any protocol claim is made, so a
# broken rig cannot be mistaken for an application that emitted nothing.
run_step ready ':'

#                                                              kitty sixel replies commands kind
native_step placeholder "cat $WORK/placeholder.bin" 1 0 0 1 placeholder
native_step chafa-kitty "chafa --format kitty --probe off --size 12x6 $WORK/pictures/red.png" 1 0 0 1 classic
native_step chafa-sixel "chafa --format sixels --probe off --size 12x6 $WORK/pictures/red.png" 0 1 0 0 sixel
native_step gnuplot-sixel \
    "gnuplot -e 'set terminal sixelgd size 240,180; set output; plot sin(x)'" 0 1 0 0 sixel
# Yazi emits nothing until its generic Kitty query is answered; the command and
# reply increments are that handshake. It then draws through Sixel, because
# Scribe truthfully advertises Sixel in DA1 and this release prefers it when
# both protocols are offered.
native_step yazi \
    "perl -e 'alarm 5; exec @ARGV' yazi $WORK/pictures || true" 0 1 1 1 sixel

tail -n "+1" "$SERVER_LOG" | plain | grep -q 'panic' \
    && fail "the native server panicked during the application corpus"
scribe-test daemon stop || true
scribe-test server stop || true
echo "PASS: pinned application corpus on native macOS"

# ---------------------------------------------------------------------------
# Phase 5: Metal. The only phase Docker cannot stand in for.
#
# The isolated spike window owns the GPUI lifecycle surface: one shared source
# per definition, the maximum allowed axis uploaded for real, max-plus-one
# refused before any RenderImage exists, atlas recovery, and final-reference
# eviction. `SCRIBE_GPUI_IMAGE_SPIKE_AUTO=1` walks those stages from the render
# pass itself, because synthesizing key events on a hosted runner needs an
# interactive accessibility grant it does not have.
# ---------------------------------------------------------------------------
: >"$SPIKE_LOG"
SCRIBE_GPUI_IMAGE_SPIKE_AUTO=1 RUST_LOG="${RUST_LOG:-scribe_client=info}" \
    scribe-client --gpui-image-spike >"$SPIKE_LOG" 2>&1 &
SPIKE_PID=$!

wait_spike_log() {
    local pattern="$1" deadline=$((SECONDS + ${2:-60}))
    until grep -qF "$pattern" "$SPIKE_LOG" 2>/dev/null; do
        [ "$SECONDS" -lt "$deadline" ] || {
            tail -40 "$SPIKE_LOG" >&2 2>/dev/null || true
            fail "the Metal spike never logged: $pattern"
        }
        kill -0 "$SPIKE_PID" 2>/dev/null || {
            tail -40 "$SPIKE_LOG" >&2 2>/dev/null || true
            fail "the Metal spike exited before logging: $pattern"
        }
        sleep 0.3
    done
}

wait_spike_log 'GPUI image max-plus-one rejected before allocation'
wait_spike_log 'GPUI image spike ready'
wait_spike_log 'GPUI image atlas invalidated for recovery'
wait_spike_log 'GPUI image cache reused after atlas invalidation'
wait_spike_log 'GPUI image cache evicted at final reference'
wait_spike_log 'GPUI image cache recreated after final-reference eviction'
kill "$SPIKE_PID" 2>/dev/null || true
wait "$SPIKE_PID" 2>/dev/null || true
SPIKE_PID=""

CLEAN_SPIKE="$GPUI/spike-clean.log"
plain <"$SPIKE_LOG" >"$CLEAN_SPIKE"

# macOS windows are drawn by `gpui_macos`, whose only renderer is Metal. It logs
# no adapter line and returns no `gpu_specs`, unlike the `gpui_wgpu` backend the
# Linux spike reads, so the running window reports the renderer that painted it
# and the host's Metal device is recorded beside it.
grep -Eq 'backend[=: ]+"?metal"?' "$CLEAN_SPIKE" \
    || fail "the running GPUI window did not paint through the Metal renderer"
system_profiler SPDisplaysDataType >"$GPUI/metal.txt" 2>&1 || true
ADAPTER=$(sed -n 's/^ *Chipset Model: *//p' "$GPUI/metal.txt" | head -1)
METAL_SUPPORT=$(sed -n 's/^ *Metal Support: *//p' "$GPUI/metal.txt" | head -1)
grep -Eq 'render_images_created_before[=: ]+0.*render_images_created_after[=: ]+0' "$CLEAN_SPIKE" \
    || fail "max-plus-one reached GPUI allocation on Metal"
grep -Eq 'rejected_width[=: ]+4097' "$CLEAN_SPIKE" \
    || fail "the rejected dimension was not the frozen max-plus-one"
grep -Eq 'render_images_created[=: ]+3' "$CLEAN_SPIKE" \
    || fail "Metal did not upload one source per definition"
grep -Eq 'cache_reuses[=: ]+1' "$CLEAN_SPIKE" \
    || fail "full and cropped placements did not reuse one Metal source"
grep -Eq 'final_reference_drops[=: ]+3' "$CLEAN_SPIKE" \
    || fail "not every Metal cache entry reached its final reference"
PROJECTED=$(sed -n 's/.* projected_gpu_bytes[=: ]*\([0-9][0-9]*\).*/\1/p' "$CLEAN_SPIKE" | head -1)
[ -n "$PROJECTED" ] || fail "the Metal run recorded no projected GPU accounting"
echo "PASS: Metal renderer, scale, upload, reuse, recovery, and eviction"

# ---------------------------------------------------------------------------
# Phase 6: the machine-readable manifest a maintainer reviews.
# ---------------------------------------------------------------------------
python3 - "$OUT/metal.json" "$STEPS" <<PY
import json
import pathlib
import subprocess
import sys

manifest_path, steps_path = (pathlib.Path(argument) for argument in sys.argv[1:])


def version(*command):
    return subprocess.run(command, capture_output=True, text=True, check=True).stdout.splitlines()[0]


steps = {}
for line in steps_path.read_text(encoding="utf-8").splitlines():
    if not line:
        continue
    (name, commands, kitty, sixel, replies, classic, placeholder,
     sixel_place, kind) = line.split("\t")
    steps[name] = {
        "kitty_commands": int(commands),
        "kitty_transfers": int(kitty),
        "sixel_images": int(sixel),
        "pty_replies": int(replies),
        "live_classic_placements": int(classic),
        "live_placeholder_placements": int(placeholder),
        "live_sixel_placements": int(sixel_place),
        "asserted_placement_kind": kind,
    }

manifest = {
    "schema": 1,
    "platform": "macos",
    "contract_version": "terminal-images-v1",
    "contract_sha256": "$CONTRACT_SHA",
    "candidate_sha": "${GITHUB_SHA:-}",
    "runner": "${SCRIBE_NATIVE_MACOS_RUNNER:-}",
    "runner_arch": "${RUNNER_ARCH:-}",
    "terminal_spoofing": False,
    "gpu": {
        "device": "$ADAPTER",
        "device_metal_support": "$METAL_SUPPORT",
        "backend": "metal",
        "max_axis_pixels_uploaded": 4096,
        "min_axis_pixels_uploaded": 1,
        "rejected_width_pixels": 4097,
        "render_images_created": 3,
        "cache_reuses": 1,
        "final_reference_drops": 3,
        "projected_gpu_bytes": int("$PROJECTED"),
    },
    "protocol_probes": "$PROBES".split(),
    "pinned_versions": {
        "yazi": version("yazi", "--version"),
        "chafa": version("chafa", "--version"),
        "gnuplot": version("gnuplot", "--version"),
    },
    "application_steps": steps,
    "not_covered_natively": [
        "ssh_transport",
        "pixel_captures",
        "induced_metal_device_loss",
    ],
}
with manifest_path.open("w", encoding="utf-8") as handle:
    json.dump(manifest, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY

echo "PASS: native macOS Metal terminal-image parity corpus"
