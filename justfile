# Scribe — GPU-accelerated terminal emulator

_default:
    @just --list -u

# ==================== Build ====================

# Debug build (all crates)
build:
    cargo build

# Release build (all crates)
build-release:
    cargo build --release

# Type-check without building (faster feedback)
check:
    cargo check

# ==================== Quality ====================

# Block new Rust lint suppression attributes in local diffs.
lint-suppressions:
    tools/check-no-new-lint-suppressions.sh --working-tree

# Ratchet how much of the GPUI client the running binary can actually reach.
reachability:
    tools/check-reachability.sh --working-tree

# Re-derive the 016 launch gate's reachable-row count from parity-inventory.md.
parity-inventory:
    tools/check-parity-inventory.sh --working-tree

# Score the 016 launch gate's go threshold: every user-facing row reachable.
# Fails while any row is unreachable, so it is not part of `just ready`.
parity-gate:
    tools/check-parity-inventory.sh --gate

# Lint (strict clippy config)
clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Format
fmt:
    cargo fmt --all

# Run all tests
test:
    cargo test --workspace

# Pre-commit gate: fmt, lint, test
ready:
    just lint-suppressions
    just reachability
    just parity-inventory
    just fmt
    just clippy
    just test

# ==================== Run ====================

# Run the server
server:
    cargo run --bin scribe-server

# Run the GPU client
client:
    cargo run --bin scribe-client

# Hot-reload the server with the current debug binary
restart-server:
    target/debug/scribe-server --upgrade

# Hot-reload the server with the current release binary
restart-server-release:
    target/release/scribe-server --upgrade

# ==================== Package ====================

# Build release .deb (full workspace so scribe-client is included)
deb:
    cargo build --release
    cargo deb -p scribe-server --no-build

# Build release .deb for the isolated scribe-dev install flavor
deb-dev:
    cargo build --release
    cargo deb -p scribe-server --no-build --variant dev

# Exercise Debian maintainer-script guards without touching a live install.
test-install:
    bash tests/install/postinst-regressions.sh

# Run the Vulkan-less upgrade guard in a disposable Debian userspace.
test-install-vulkan-guard:
    docker run --rm --tmpfs /run -v "$(pwd):/repo:ro" debian:bookworm-slim bash /repo/tests/install/postinst-regressions.sh

# Build and install .deb
install: deb
    sudo dpkg -i $(find target/debian -name 'scribe_*.deb' -print -quit)

# Build and install isolated scribe-dev .deb
install-dev: deb-dev
    sudo dpkg -i $(find target/debian -name 'scribe-dev_*.deb' -print -quit)

# Build macOS .app bundle and .dmg installer
dmg:
    bash dist/macos/build-dmg.sh

# Build macOS .dmg (skip cargo build, use existing release binaries)
dmg-quick:
    bash dist/macos/build-dmg.sh --skip-build

# Set up Claude Code AI indicator hooks (run after installing Claude Code)
setup-claude:
    bash dist/setup-claude-hooks.sh --hook-source dist

# Set up Codex Code AI indicator hooks (run after installing Codex)
setup-codex:
    bash dist/setup-codex-hooks.sh --hook-source dist

# ==================== E2E Testing ====================

gpu_flags := if env("SCRIBE_E2E_GPUS", "") == "" { "" } else { "--gpus " + env("SCRIBE_E2E_GPUS") }
hardened_e2e_flags := "--network none --read-only --cap-drop ALL"

# Rebuild functional test container from release or debug binaries
docker-func profile="release":
    #!/usr/bin/env bash
    set -euo pipefail
    # A binary is stale when it is absent or older than the newest commit touching crates/.
    requested="{{ profile }}"
    profile="${requested#profile=}"
    case "$profile" in
        release) image="scribe-test-func" ;;
        debug) image="scribe-test-func-debug" ;;
        *) printf 'ERROR: invalid profile %q; expected release or debug.\n' "$requested" >&2; exit 2 ;;
    esac
    tools/e2e-stage.sh "$profile" scribe-server scribe-test scribe-hook-helper scribe-cli
    docker build --build-arg "BIN_DIR=target/e2e-stage/$profile" -f docker/Dockerfile.func -t "$image" .

# Rebuild visual test container from release or debug binaries
docker-visual profile="release":
    #!/usr/bin/env bash
    set -euo pipefail
    # A binary is stale when it is absent or older than the newest commit touching crates/.
    requested="{{ profile }}"
    profile="${requested#profile=}"
    case "$profile" in
        release) image="scribe-test-visual" ;;
        debug) image="scribe-test-visual-debug" ;;
        *) printf 'ERROR: invalid profile %q; expected release or debug.\n' "$requested" >&2; exit 2 ;;
    esac
    tools/e2e-stage.sh "$profile" scribe-server scribe-client scribe-test scribe-hook-helper
    docker build --build-arg "BIN_DIR=target/e2e-stage/$profile" -f docker/Dockerfile.visual -t "$image" .

# Run a functional E2E test (e.g. just e2e-func func/smoke.sh)
e2e-func script image="scribe-test-func" runtime_profile="default":
    #!/usr/bin/env bash
    set -euo pipefail
    image="{{ image }}"
    image="${image#image=}"
    requested="{{ runtime_profile }}"
    runtime_profile="${requested#runtime_profile=}"
    case "$runtime_profile" in
        default) runtime_flags=() ;;
        hardened)
            runtime_uid=$(id -u)
            runtime_gid=$(id -g)
            runtime_flags=(
                {{ hardened_e2e_flags }}
                --user "$runtime_uid:$runtime_gid"
                --env USER=scribe-e2e
                --env HOME=/root
                --env SHELL=/bin/bash
                --tmpfs "/run:rw,nosuid,nodev,mode=755,uid=$runtime_uid,gid=$runtime_gid"
                --tmpfs "/tmp:rw,nosuid,nodev,mode=1777,uid=$runtime_uid,gid=$runtime_gid"
                --tmpfs "/root:rw,nosuid,nodev,mode=700,uid=$runtime_uid,gid=$runtime_gid"
            )
            ;;
        *) printf 'ERROR: invalid runtime profile %q; expected default or hardened.\n' "$requested" >&2; exit 2 ;;
    esac
    docker run --rm "${runtime_flags[@]}" -e TEST_TIMEOUT -e RUST_LOG -e SCRIBE_KEYRING -v ./tests/e2e:/tests:ro -v ./test-output:/output "$image" /tests/{{script}}

# Run a functional E2E test under the hardened Docker runtime profile.
e2e-func-hardened script image="scribe-test-func":
    just e2e-func "{{ script }}" "{{ image }}" runtime_profile=hardened

# Run a visual E2E test. Set SCRIBE_E2E_GPUS to opt into GPU passthrough.
e2e-visual script image="scribe-test-visual" runtime_profile="default":
    #!/usr/bin/env bash
    set -euo pipefail
    image="{{ image }}"
    image="${image#image=}"
    requested="{{ runtime_profile }}"
    runtime_profile="${requested#runtime_profile=}"
    case "$runtime_profile" in
        default) runtime_flags=() ;;
        hardened)
            runtime_uid=$(id -u)
            runtime_gid=$(id -g)
            runtime_flags=(
                {{ hardened_e2e_flags }}
                --user "$runtime_uid:$runtime_gid"
                --env USER=scribe-e2e
                --env HOME=/root
                --env SHELL=/bin/bash
                --tmpfs "/run:rw,nosuid,nodev,mode=755,uid=$runtime_uid,gid=$runtime_gid"
                --tmpfs "/tmp:rw,nosuid,nodev,mode=1777,uid=$runtime_uid,gid=$runtime_gid"
                --tmpfs "/root:rw,nosuid,nodev,mode=700,uid=$runtime_uid,gid=$runtime_gid"
            )
            ;;
        *) printf 'ERROR: invalid runtime profile %q; expected default or hardened.\n' "$requested" >&2; exit 2 ;;
    esac
    docker run --rm "${runtime_flags[@]}" {{gpu_flags}} -e TEST_TIMEOUT -e RUST_LOG -e SCRIBE_KEYRING -v ./tests/e2e:/tests:ro -v ./test-output:/output "$image" /tests/{{script}}

# Run a visual E2E test under the hardened Docker runtime profile.
e2e-visual-hardened script image="scribe-test-visual":
    just e2e-visual "{{ script }}" "{{ image }}" runtime_profile=hardened

# Run a visual E2E test that needs a live pane BOTH the GPUI client and
# scribe-test can see (SCRIBE_SHARED_PANE=1). The entrypoint creates the session
# first and hands the client the daemon's window id, so the client joins that
# window's share additively instead of opening an empty window of its own — the
# only arrangement in which a pixel assertion and `scribe-test wait-output` are
# talking about the same pane.
e2e-visual-shared script:
    docker run --rm {{gpu_flags}} -e SCRIBE_SHARED_PANE=1 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/{{script}}

# Run the feature-015 sharing/control E2E through the wire tap
e2e-visual-share:
    docker run --rm {{gpu_flags}} -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/share-control.sh

# Run the Subscribe / RequestSnapshot session-tooling E2E through the wire tap
e2e-visual-session-tooling:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=180 -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/session-tooling.sh

# Run the feature-014 settings trust/preflight E2E. Drives the real settings
# window (`--settings`) against the real server through the wire tap, with one
# trusted network and one approved device seeded into the server's stores.
e2e-visual-settings-trust:
    docker run --rm {{gpu_flags}} -e SCRIBE_VISUAL_APP=settings -e SCRIBE_SHARE_TAP=1 -e SCRIBE_SEED_TRUST=1 -e TEST_TIMEOUT=180 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/settings-trust.sh

# Run the in-app settings entry-point E2E. Drives the running terminal window
# with the settings chord, the palette row, and the titlebar gear, and asserts
# the "Scribe Settings" window maps exactly once.
e2e-visual-settings-entry:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=180 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/settings-entry.sh

# Run the live tab-switching E2E through the shared-pane rig and the wire tap.
# The client creates its own second tab, then keyboard and titlebar selection
# changes are asserted on the recorded `AttachSessions` frames.
e2e-visual-tab-switching:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=180 -e SCRIBE_SHARED_PANE=1 -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/tab-switching.sh

# Run the window-lifecycle E2E through the wire tap. The tap only records here
# (nothing is injected); the seeded config turns the client's window-list poll
# on, and the run relaunches the client twice so it needs a longer budget.
e2e-visual-window-lifecycle:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=180 -e SCRIBE_SHARE_TAP=1 -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/window-lifecycle-config.toml)" -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/window-lifecycle.sh

# Run the cold-restart restore E2E against the real client. No wire tap: the run
# restarts the real server, and the tap renames the socket `scribe-test server`
# addresses. It kills the client, cold-restarts the server, and relaunches, so
# it needs a longer budget.
e2e-visual-cold-restart:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=240 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/cold-restart.sh

# Run the find-overlay E2E. It needs the shared pane (so the harness can put
# the searched text on the real PTY the client renders) AND the wire tap (so
# SearchRequest leaving the client and SearchResults coming back can both be
# asserted as real frames).
e2e-visual-find:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=180 -e SCRIBE_SHARED_PANE=1 -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/find-overlay.sh

# Run the feature-014 LAN approval + dial visual E2E. The wire tap records the
# Unix socket (the approval decision leaves on it) and SCRIBE_KEYRING=1 starts a
# session keyring so the server can seal a LAN device identity for the dial half.
e2e-visual-lan-approval:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=240 -e SCRIBE_SHARE_TAP=1 -e SCRIBE_KEYRING=1 -e SCRIBE_SEED_TRUST=1 -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/lan-approval-config.toml)" -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/lan-approval.sh

# Run the feature-013 tailnet remote-control visual E2E. The wire tap records
# the Unix socket (the startup remote probe and the reclaim leave on it) and
# injects the takeover / severance notices a second machine would have caused;
# `scribe-test remote-peer` terminates the TCP dial for the handshake half.
e2e-visual-remote-control:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=300 -e SCRIBE_SHARE_TAP=1 -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/remote-control-config.toml)" -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/remote-control.sh

# Run either update-surface visual E2E, or both when no script is named. The
# optional selector lets the aggregate report each script independently while
# this recipe remains the source of truth for their shared environment.
e2e-visual-update script="":
    #!/usr/bin/env bash
    set -euo pipefail
    requested="{{ script }}"
    requested="${requested#script=}"
    case "$requested" in
        "") scripts=(visual/update-trigger.sh visual/update-dismiss.sh) ;;
        visual/update-trigger.sh|visual/update-dismiss.sh) scripts=("$requested") ;;
        *) printf 'ERROR: unknown update E2E script %q.\n' "$requested" >&2; exit 2 ;;
    esac
    for script in "${scripts[@]}"; do
        docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=180 -e SCRIBE_UPDATE_API_URL=http://127.0.0.1:8099/releases/latest -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/update-config.toml)" -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual "/tests/$script"
    done

# Run the window-chrome band visual E2E: the derived window size, the whole
# terminal grid, and the prompt/status bands all on screen. Uses the shared-pane
# rig so `scribe-test send` fills the very pane being measured, and the AI hook
# channel to raise a real prompt strip.
e2e-visual-chrome-bands:
    docker run --rm {{gpu_flags}} -e SCRIBE_SHARED_PANE=1 -e TEST_TIMEOUT=180 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/window-chrome-bands.sh

# Run the terminal-viewport E2E: scrollback paging, zoom, vi mode, split-scroll,
# and the smart-selection context menu, all against the real window. Needs the
# shared-pane rig (so `scribe-test` and the client see one pane), the
# `scroll_pin` opt-in, and a longer budget than the default 60 s for its phases.
e2e-visual-terminal-viewport:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=240 -e SCRIBE_SHARED_PANE=1 -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/terminal-viewport-config.toml)" -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/terminal-viewport.sh

# Run the terminal-zoom E2E: the three zoom chords against the real window, with
# the wire tap recording the `Resize` each font rescale republishes. Needs the
# shared-pane rig (so `scribe-test` seeds the very pane being measured) and
# SCRIBE_SHARE_TAP=1 for the on-the-wire half.
e2e-visual-terminal-zoom:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=180 -e SCRIBE_SHARED_PANE=1 -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/terminal-zoom.sh

# Run the window-resize E2E: the window manager resizes the real window and the
# wire tap records the `Resize` each new grid band republishes, with `stty size`
# inside the PTY as the end-to-end oracle. Three further phases seed marker
# rows and compare the window against the server's own screen row for row,
# across a stepped drag, so a pane that publishes perfect geometry and renders
# nothing still fails. Needs the shared-pane rig (so `scribe-test` owns the very
# pane being measured) and SCRIBE_SHARE_TAP=1.
e2e-visual-window-resize:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=240 -e SCRIBE_SHARED_PANE=1 -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/window-resize.sh

# Run the mouse-reporting E2E: the wheel over the grid, and the X10 / SGR-1006
# reports a mouse-tracking application receives. Needs the shared-pane rig (a
# real `cat -v` behind the DEC modes echoes the reports back), the wire tap
# (the recorded `KeyInput` bytes are the byte-identical oracle), and a longer
# budget than the default 60 s for its ten phases.
e2e-visual-mouse-reporting:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=300 -e SCRIBE_SHARED_PANE=1 -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/mouse-reporting.sh

# Run the prompt-mark E2E: OSC 133 ingestion plus prompt_jump_up /
# prompt_jump_down / jump_to_failure and the server's ScrollBottom snap, all
# against the real window. Needs the shared-pane rig (so `scribe-test` writes
# the OSC 133 bytes into the very pane the client renders) and a longer budget
# than the default 60 s for its six phases.
e2e-visual-prompt-marks:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=300 -e SCRIBE_SHARED_PANE=1 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/prompt-marks.sh

# Run the command-mark scrollbar E2E: the overlay thumb, its success/failure
# ticks, the shift a server scrollback trim causes, and the idle fade — all
# asserted as pixels in the pane's right-edge strip. Needs the shared-pane rig
# (so `scribe-test` writes the OSC 133 bytes into the very pane the client
# paints) and a longer budget than the default 60 s for its six phases.
e2e-visual-scrollbar:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=300 -e SCRIBE_SHARED_PANE=1 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/scrollbar.sh

# Run the AI task-label visual E2E. It relaunches the client to adopt the
# harness session before it can assert anything, so it needs more than the
# default 60 s budget.
e2e-visual-ai-task-label:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=180 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/ai-task-label.sh

# Run the workspace-IPC visual E2E: CreateWorkspace / ReportWorkspaceTree /
# MoveSession / CloseWorkspace leaving the client on the wire, and an injected
# WorkspaceInfo repainting the status bar. Needs the wire tap (frames are the
# evidence) and a longer budget than the default 60 s because it relaunches the
# client to adopt the harness session first.
e2e-visual-workspace-ipc:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=240 -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/workspace-ipc.sh
# Run the clipboard / OSC 52 visual E2E. The wire tap records the prompt
# response and the bridge read reply leaving the client, and the seeded config
# puts both OSC 52 policy axes in prompt mode so the modal is exercised. The run
# relaunches the client to adopt the harness session, so it needs a longer
# budget than the default 60 s.
e2e-visual-clipboard:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=240 -e SCRIBE_SHARE_TAP=1 -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/clipboard-config.toml)" -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/clipboard-osc52.sh

# Run the terminal-bell visual E2E. It needs the shared-pane rig so a real shell
# can write the BEL byte into the very pane the client renders, and it minimizes
# and restores the window between phases, so it needs more than the default 60 s.
e2e-visual-bell:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=180 -e SCRIBE_SHARED_PANE=1 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/bell.sh

# Run the desktop-notification visual E2E. SCRIBE_NOTIFY=1 stands a real
# `org.freedesktop.Notifications` service on a session bus so the client's zbus
# dispatcher has something to deliver to and something to be clicked from; the
# shared-pane rig is what makes the AI hook events reach the client's own window.
e2e-visual-notifications:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=240 -e SCRIBE_NOTIFY=1 -e SCRIBE_SHARED_PANE=1 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/notifications.sh

# Run the drag-and-drop visual E2E. A real XDND drag source on the same X server
# hands the client a file; the shared-pane rig is what lets `scribe-test` read
# the quoted path back off the very PTY the client typed it into.
e2e-visual-drag-drop:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=180 -e SCRIBE_SHARED_PANE=1 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/drag-drop.sh

# Run the server-lifecycle visual E2E: a stale socket diagnosed by name, and an
# autostart that ends in a live initial shell. The run stops the server and
# relaunches the client twice, so it needs a longer budget than the default 60 s.
e2e-visual-server-lifecycle:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=240 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/server-lifecycle.sh

# Run the live server-upgrade reattach oracle. It keeps the original GPUI
# process alive through `scribe-server --upgrade` and asserts that it rebuilds
# its session topology before accepting post-handoff output.
e2e-visual-server-upgrade-reattach:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=180 -e SCRIBE_SHARED_PANE=1 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/server-upgrade-reattach.sh

# Run the IME/preedit visual E2E. SCRIBE_IME=1 starts ibus with an XIM server
# and exports XMODIFIERS before the client launches, so a real input method
# owns the keyboard; the shared-pane rig is what lets `scribe-test` prove the
# raw composition keys never reached the PTY.
e2e-visual-ime:
    docker run --rm {{gpu_flags}} -e TEST_TIMEOUT=240 -e SCRIBE_IME=1 -e SCRIBE_SHARED_PANE=1 -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/ime-preedit.sh

# Run the pane/workspace layout E2E with its openbox-safe keybinding.
e2e-visual-pane-workspace-layout:
    docker run --rm {{gpu_flags}} -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/pane-workspace-layout-config.toml)" -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/pane-workspace-layout.sh

# Run paste confirmation with its opt-in policy enabled.
e2e-visual-paste-confirmation:
    docker run --rm {{gpu_flags}} -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/paste-confirmation-config.toml)" -v ./tests/e2e:/tests:ro -v ./test-output:/output scribe-test-visual /tests/visual/paste-confirmation.sh

# Full functional E2E suite: build, containerise, run all tests
e2e: build-release docker-func
    #!/usr/bin/env bash
    set -euo pipefail
    scripts=(
        func/ai-context-thresholds.sh
        func/ai-launch-smoke.sh
        func/ai-shell-env-bash.sh
        func/ai-shell-env-fish.sh
        func/ai-shell-env-zsh.sh
        func/ai-state-indicator.sh
        func/attach-lossless.sh
        func/cli-smoke.sh
        func/cold-restart.sh
        func/env-persistence.sh
        func/failure-server-down.sh
        func/failure-socket-loss.sh
        func/fresh-create-geometry.sh
        func/handoff-truecolor.sh
        func/hot-reload.sh
        func/keybindings-validation.sh
        func/multi-window.sh
        func/reconnect.sh
        func/resize-coalescing.sh
        func/session-exit-status.sh
        func/shell-integration.sh
        func/smoke.sh
        func/terminal-shortcuts.sh
        func/viewport-debounce.sh
        func/workspace-split.sh
    )
    mapfile -t inventory < <(find tests/e2e/func -maxdepth 1 -type f -name '*.sh' -perm -u+x -printf 'func/%f\n' | sort)
    mapfile -t mapped < <(printf '%s\n' "${scripts[@]}" | sort)
    if ! diff -u <(printf '%s\n' "${inventory[@]}") <(printf '%s\n' "${mapped[@]}"); then
        echo 'ERROR: functional E2E recipe does not match executable script inventory.' >&2
        exit 2
    fi
    for script in "${scripts[@]}"; do
        if [[ "$script" == func/env-persistence.sh ]]; then
            SCRIBE_KEYRING=1 just e2e-func "$script"
        else
            just e2e-func "$script"
        fi
    done

# Full visual E2E suite: build once, delegate each executable test to its recipe,
# collect every result, and leave a crash-tolerant machine-readable summary.
e2e-all-visual: build-release docker-visual
    #!/usr/bin/env bash
    set -uo pipefail
    mappings=(
        'visual/ai-indicator.sh|e2e-visual-shared'
        'visual/ai-task-label.sh|e2e-visual-ai-task-label'
        'visual/bell.sh|e2e-visual-bell'
        'visual/clipboard-osc52.sh|e2e-visual-clipboard'
        'visual/cold-restart.sh|e2e-visual-cold-restart'
        'visual/color-emoji.sh|e2e-visual-shared'
        'visual/config-reload.sh|e2e-visual'
        'visual/dialogs.sh|e2e-visual'
        'visual/drag-drop.sh|e2e-visual-drag-drop'
        'visual/find-overlay.sh|e2e-visual-find'
        'visual/ime-preedit.sh|e2e-visual-ime'
        'visual/lan-approval.sh|e2e-visual-lan-approval'
        'visual/mouse-reporting.sh|e2e-visual-mouse-reporting'
        'visual/notifications.sh|e2e-visual-notifications'
        'visual/overlay-actions.sh|e2e-visual-shared'
        'visual/overlays.sh|e2e-visual'
        'visual/pane-workspace-layout.sh|e2e-visual-pane-workspace-layout'
        'visual/paste-confirmation.sh|e2e-visual-paste-confirmation'
        'visual/prompt-marks.sh|e2e-visual-prompt-marks'
        'visual/reconnect.sh|e2e-visual'
        'visual/remote-control.sh|e2e-visual-remote-control'
        'visual/scrollbar.sh|e2e-visual-scrollbar'
        'visual/server-lifecycle.sh|e2e-visual-server-lifecycle'
        'visual/server-upgrade-reattach.sh|e2e-visual-server-upgrade-reattach'
        'visual/session-tooling.sh|e2e-visual-session-tooling'
        'visual/settings-entry.sh|e2e-visual-settings-entry'
        'visual/settings-trust.sh|e2e-visual-settings-trust'
        'visual/share-control.sh|e2e-visual-share'
        'visual/tab-switching.sh|e2e-visual-tab-switching'
        'visual/tab-window-chords.sh|e2e-visual'
        'visual/terminal-viewport.sh|e2e-visual-terminal-viewport'
        'visual/terminal-zoom.sh|e2e-visual-terminal-zoom'
        'visual/titlebar.sh|e2e-visual'
        'visual/update-dismiss.sh|e2e-visual-update'
        'visual/update-trigger.sh|e2e-visual-update'
        'visual/window-chrome-bands.sh|e2e-visual-chrome-bands'
        'visual/window-lifecycle.sh|e2e-visual-window-lifecycle'
        'visual/window-resize.sh|e2e-visual-window-resize'
        'visual/workspace-ipc.sh|e2e-visual-workspace-ipc'
        'visual/workspace-split.sh|e2e-visual'
        'visual/x11-focus-guard.sh|e2e-visual'
    )
    mapfile -t inventory < <(find tests/e2e/visual -maxdepth 1 -type f -name '*.sh' -perm -u+x -printf 'visual/%f\n' | sort)
    mapfile -t mapped < <(printf '%s\n' "${mappings[@]}" | cut -d'|' -f1 | sort)
    if ! diff -u <(printf '%s\n' "${inventory[@]}") <(printf '%s\n' "${mapped[@]}"); then
        echo 'ERROR: visual E2E mapping does not match executable script inventory.' >&2
        exit 2
    fi
    mkdir -p test-output
    summary=test-output/e2e-visual-summary.jsonl
    : >"$summary"
    failures=0
    for mapping in "${mappings[@]}"; do
        IFS='|' read -r script recipe <<<"$mapping"
        started=$(date +%s)
        case "$recipe" in
            e2e-visual|e2e-visual-shared|e2e-visual-update)
                just "$recipe" "$script"
                exit_code=$?
                ;;
            *)
                just "$recipe"
                exit_code=$?
                ;;
        esac
        duration_s=$(($(date +%s) - started))
        if ((exit_code == 0)); then
            status=pass
        else
            status=fail
            failures=$((failures + 1))
        fi
        printf '{"script":"%s","recipe":"%s","status":"%s","exit_code":%d,"duration_s":%d}\n' \
            "$script" "$recipe" "$status" "$exit_code" "$duration_s" >>"$summary"
    done
    if ((failures > 0)); then
        printf 'ERROR: %d visual E2E script(s) failed; see %s.\n' "$failures" "$summary" >&2
        exit 1
    fi
