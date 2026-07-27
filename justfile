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

# Rebuild functional test container (after cargo build --release)
docker-func:
    docker build -f docker/Dockerfile.func -t scribe-test-func .

# Rebuild visual test container (after cargo build --release)
docker-visual:
    docker build -f docker/Dockerfile.visual -t scribe-test-visual .

# Run a functional E2E test (e.g. just e2e-func func/smoke.sh)
e2e-func script:
    docker run --rm -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-func /tests/{{script}}

# Run a visual E2E test (requires --gpus all)
e2e-visual script:
    docker run --rm --gpus all -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/{{script}}

# Run a visual E2E test that needs a live pane BOTH the GPUI client and
# scribe-test can see (SCRIBE_SHARED_PANE=1). The entrypoint creates the session
# first and hands the client the daemon's window id, so the client joins that
# window's share additively instead of opening an empty window of its own — the
# only arrangement in which a pixel assertion and `scribe-test wait-output` are
# talking about the same pane.
e2e-visual-shared script:
    docker run --rm --gpus all -e SCRIBE_SHARED_PANE=1 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/{{script}}

# Run the feature-015 sharing/control E2E through the wire tap
e2e-visual-share:
    docker run --rm --gpus all -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/share-control.sh

# Run the Subscribe / RequestSnapshot session-tooling E2E through the wire tap
e2e-visual-session-tooling:
    docker run --rm --gpus all -e TEST_TIMEOUT=180 -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/session-tooling.sh

# Run the feature-014 settings trust/preflight E2E. Drives the real settings
# window (`--settings`) against the real server through the wire tap, with one
# trusted network and one approved device seeded into the server's stores.
e2e-visual-settings-trust:
    docker run --rm --gpus all -e SCRIBE_VISUAL_APP=settings -e SCRIBE_SHARE_TAP=1 -e SCRIBE_SEED_TRUST=1 -e TEST_TIMEOUT=180 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/settings-trust.sh

# Run the in-app settings entry-point E2E. Drives the running terminal window
# with the settings chord, the palette row, and the titlebar gear, and asserts
# the "Scribe Settings" window maps exactly once.
e2e-visual-settings-entry:
    docker run --rm --gpus all -e TEST_TIMEOUT=180 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/settings-entry.sh

# Run the window-lifecycle E2E through the wire tap. The tap only records here
# (nothing is injected); the seeded config turns the client's window-list poll
# on, and the run relaunches the client twice so it needs a longer budget.
e2e-visual-window-lifecycle:
    docker run --rm --gpus all -e TEST_TIMEOUT=180 -e SCRIBE_SHARE_TAP=1 -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/window-lifecycle-config.toml)" -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/window-lifecycle.sh

# Run the cold-restart restore E2E against the real client. No wire tap: the run
# restarts the real server, and the tap renames the socket `scribe-test server`
# addresses. It kills the client, cold-restarts the server, and relaunches, so
# it needs a longer budget.
e2e-visual-cold-restart:
    docker run --rm --gpus all -e TEST_TIMEOUT=240 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/cold-restart.sh

# Run the find-overlay E2E. It needs the shared pane (so the harness can put
# the searched text on the real PTY the client renders) AND the wire tap (so
# SearchRequest leaving the client and SearchResults coming back can both be
# asserted as real frames).
e2e-visual-find:
    docker run --rm --gpus all -e TEST_TIMEOUT=180 -e SCRIBE_SHARED_PANE=1 -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/find-overlay.sh

# Run the feature-014 LAN approval + dial visual E2E. The wire tap records the
# Unix socket (the approval decision leaves on it) and SCRIBE_KEYRING=1 starts a
# session keyring so the server can seal a LAN device identity for the dial half.
e2e-visual-lan-approval:
    docker run --rm --gpus all -e TEST_TIMEOUT=240 -e SCRIBE_SHARE_TAP=1 -e SCRIBE_KEYRING=1 -e SCRIBE_SEED_TRUST=1 -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/lan-approval-config.toml)" -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/lan-approval.sh

# Run the feature-013 tailnet remote-control visual E2E. The wire tap records
# the Unix socket (the startup remote probe and the reclaim leave on it) and
# injects the takeover / severance notices a second machine would have caused;
# `scribe-test remote-peer` terminates the TCP dial for the handshake half.
e2e-visual-remote-control:
    docker run --rm --gpus all -e TEST_TIMEOUT=300 -e SCRIBE_SHARE_TAP=1 -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/remote-control-config.toml)" -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/remote-control.sh

# Run the update-surface visual E2E pair. The server polls a fake releases API
# inside the container, so it needs a longer budget than the default 60 s.
e2e-visual-update:
    docker run --rm --gpus all -e TEST_TIMEOUT=180 -e SCRIBE_UPDATE_API_URL=http://127.0.0.1:8099/releases/latest -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/update-config.toml)" -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/update-trigger.sh
    docker run --rm --gpus all -e TEST_TIMEOUT=180 -e SCRIBE_UPDATE_API_URL=http://127.0.0.1:8099/releases/latest -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/update-config.toml)" -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/update-dismiss.sh

# Run the window-chrome band visual E2E: the derived window size, the whole
# terminal grid, and the prompt/status bands all on screen. Uses the shared-pane
# rig so `scribe-test send` fills the very pane being measured, and the AI hook
# channel to raise a real prompt strip.
e2e-visual-chrome-bands:
    docker run --rm --gpus all -e SCRIBE_SHARED_PANE=1 -e TEST_TIMEOUT=180 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/window-chrome-bands.sh

# Run the terminal-viewport E2E: scrollback paging, zoom, vi mode, split-scroll,
# and the smart-selection context menu, all against the real window. Needs the
# shared-pane rig (so `scribe-test` and the client see one pane), the
# `scroll_pin` opt-in, and a longer budget than the default 60 s for its phases.
e2e-visual-terminal-viewport:
    docker run --rm --gpus all -e TEST_TIMEOUT=240 -e SCRIBE_SHARED_PANE=1 -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/terminal-viewport-config.toml)" -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/terminal-viewport.sh

# Run the terminal-zoom E2E: the three zoom chords against the real window, with
# the wire tap recording the `Resize` each font rescale republishes. Needs the
# shared-pane rig (so `scribe-test` seeds the very pane being measured) and
# SCRIBE_SHARE_TAP=1 for the on-the-wire half.
e2e-visual-terminal-zoom:
    docker run --rm --gpus all -e TEST_TIMEOUT=180 -e SCRIBE_SHARED_PANE=1 -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/terminal-zoom.sh

# Run the mouse-reporting E2E: the wheel over the grid, and the X10 / SGR-1006
# reports a mouse-tracking application receives. Needs the shared-pane rig (a
# real `cat -v` behind the DEC modes echoes the reports back), the wire tap
# (the recorded `KeyInput` bytes are the byte-identical oracle), and a longer
# budget than the default 60 s for its ten phases.
e2e-visual-mouse-reporting:
    docker run --rm --gpus all -e TEST_TIMEOUT=300 -e SCRIBE_SHARED_PANE=1 -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/mouse-reporting.sh

# Run the prompt-mark E2E: OSC 133 ingestion plus prompt_jump_up /
# prompt_jump_down / jump_to_failure and the server's ScrollBottom snap, all
# against the real window. Needs the shared-pane rig (so `scribe-test` writes
# the OSC 133 bytes into the very pane the client renders) and a longer budget
# than the default 60 s for its six phases.
e2e-visual-prompt-marks:
    docker run --rm --gpus all -e TEST_TIMEOUT=300 -e SCRIBE_SHARED_PANE=1 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/prompt-marks.sh

# Run the AI task-label visual E2E. It relaunches the client to adopt the
# harness session before it can assert anything, so it needs more than the
# default 60 s budget.
e2e-visual-ai-task-label:
    docker run --rm --gpus all -e TEST_TIMEOUT=180 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/ai-task-label.sh

# Run the workspace-IPC visual E2E: CreateWorkspace / ReportWorkspaceTree /
# MoveSession / CloseWorkspace leaving the client on the wire, and an injected
# WorkspaceInfo repainting the status bar. Needs the wire tap (frames are the
# evidence) and a longer budget than the default 60 s because it relaunches the
# client to adopt the harness session first.
e2e-visual-workspace-ipc:
    docker run --rm --gpus all -e TEST_TIMEOUT=240 -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/workspace-ipc.sh
# Run the workspace-notes visual E2E: the modal opening on the SERVER's own
# workspace id, WorkspaceNotesGet / WorkspaceNotesMutate leaving the client, and
# the WorkspaceNotesSnapshot / WorkspaceNotesChanged answers rendering into the
# modal. Needs the wire tap (frames are the evidence, and the last phase injects
# a change the client never asked for) and a longer budget than the default 60 s
# because the run relaunches the client to adopt the harness session first.
e2e-visual-workspace-notes:
    docker run --rm --gpus all -e TEST_TIMEOUT=240 -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/workspace-notes.sh

# Run the clipboard / OSC 52 visual E2E. The wire tap records the prompt
# response and the bridge read reply leaving the client, and the seeded config
# puts both OSC 52 policy axes in prompt mode so the modal is exercised. The run
# relaunches the client to adopt the harness session, so it needs a longer
# budget than the default 60 s.
e2e-visual-clipboard:
    docker run --rm --gpus all -e TEST_TIMEOUT=240 -e SCRIBE_SHARE_TAP=1 -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/clipboard-config.toml)" -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/clipboard-osc52.sh

# Run the terminal-bell visual E2E. It needs the shared-pane rig so a real shell
# can write the BEL byte into the very pane the client renders, and it minimizes
# and restores the window between phases, so it needs more than the default 60 s.
e2e-visual-bell:
    docker run --rm --gpus all -e TEST_TIMEOUT=180 -e SCRIBE_SHARED_PANE=1 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/bell.sh

# Run the IME/preedit visual E2E. SCRIBE_IME=1 starts ibus with an XIM server
# and exports XMODIFIERS before the client launches, so a real input method
# owns the keyboard; the shared-pane rig is what lets `scribe-test` prove the
# raw composition keys never reached the PTY.
e2e-visual-ime:
    docker run --rm --gpus all -e TEST_TIMEOUT=240 -e SCRIBE_IME=1 -e SCRIBE_SHARED_PANE=1 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/ime-preedit.sh

# Full functional E2E suite: build, containerise, run all tests
e2e: build-release docker-func
    just e2e-func func/smoke.sh
    just e2e-func func/reconnect.sh
    just e2e-func func/workspace-split.sh
    just e2e-func func/shell-integration.sh
    just e2e-func func/hot-reload.sh
    just e2e-func func/cold-restart.sh
    just e2e-func func/failure-server-down.sh
    just e2e-func func/failure-socket-loss.sh
    just e2e-func func/ai-state-indicator.sh
    just e2e-func func/ai-context-thresholds.sh
