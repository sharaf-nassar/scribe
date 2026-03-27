#!/bin/bash
set -euo pipefail

VERSION="${1:?Usage: inject-version.sh <version>}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# Portable in-place sed: macOS requires -i '' while GNU sed requires -i
sedi() {
    if sed --version 2>/dev/null | grep -q GNU; then
        sed -i "$@"
    else
        sed -i '' "$@"
    fi
}

echo "==> Injecting version ${VERSION}..."

# Replace version placeholder in workspace Cargo.toml (line 6)
sedi "s/^version = \"0\.0\.0-dev\"/version = \"${VERSION}\"/" "${REPO_ROOT}/Cargo.toml"

# Replace CFBundleShortVersionString in Info.plist
sedi \
    "/<key>CFBundleShortVersionString<\/key>/{n; s|<string>0\.0\.0-dev<\/string>|<string>${VERSION}</string>|;}" \
    "${REPO_ROOT}/dist/macos/Info.plist"

echo "==> Version ${VERSION} injected."
