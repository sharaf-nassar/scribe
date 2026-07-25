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

# Run the feature-015 sharing/control E2E through the wire tap
e2e-visual-share:
    docker run --rm --gpus all -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/share-control.sh

# Run the Subscribe / RequestSnapshot session-tooling E2E through the wire tap
e2e-visual-session-tooling:
    docker run --rm --gpus all -e TEST_TIMEOUT=180 -e SCRIBE_SHARE_TAP=1 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/session-tooling.sh

# Run the update-surface visual E2E pair. The server polls a fake releases API
# inside the container, so it needs a longer budget than the default 60 s.
e2e-visual-update:
    docker run --rm --gpus all -e TEST_TIMEOUT=180 -e SCRIBE_UPDATE_API_URL=http://127.0.0.1:8099/releases/latest -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/update-config.toml)" -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/update-trigger.sh
    docker run --rm --gpus all -e TEST_TIMEOUT=180 -e SCRIBE_UPDATE_API_URL=http://127.0.0.1:8099/releases/latest -e SCRIBE_EXTRA_CONFIG="$(cat tests/e2e/visual/update-config.toml)" -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/update-dismiss.sh

# Run the AI task-label visual E2E. It relaunches the client to adopt the
# harness session before it can assert anything, so it needs more than the
# default 60 s budget.
e2e-visual-ai-task-label:
    docker run --rm --gpus all -e TEST_TIMEOUT=180 -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/visual/ai-task-label.sh

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
