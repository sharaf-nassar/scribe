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

# Lint (strict clippy config)
clippy:
    cargo clippy --workspace

# Format
fmt:
    cargo fmt --all

# Run all tests
test:
    cargo test --workspace

# Pre-commit gate: fmt, lint, test
ready:
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

# ==================== Package ====================

# Build release .deb (full workspace so scribe-client is included)
deb:
    cargo build --release
    cargo deb -p scribe-server --no-build

# Build and install .deb
install: deb
    sudo dpkg -i target/debian/scribe_0.1.0-1_amd64.deb

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

# Full functional E2E suite: build, containerise, run all tests
e2e: build-release docker-func
    just e2e-func func/smoke.sh
    just e2e-func func/reconnect.sh
    just e2e-func func/workspace-split.sh
