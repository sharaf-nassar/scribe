#!/bin/bash
set -euo pipefail

# Configuration
APP_NAME="Scribe"
BUNDLE_NAME="${APP_NAME}.app"
DMG_NAME="${APP_NAME}.dmg"
VOLUME_NAME="${APP_NAME}"

# Paths (relative to repo root)
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
BUILD_DIR="${REPO_ROOT}/target/release"
DIST_DIR="${REPO_ROOT}/dist"
STAGING_DIR="${REPO_ROOT}/target/macos-staging"

# Parse arguments
SKIP_BUILD=false
VERSION=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-build)
            SKIP_BUILD=true
            shift
            ;;
        --version)
            VERSION="${2:?--version requires an argument}"
            shift 2
            ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

# Clean previous staging
rm -rf "${STAGING_DIR}"
mkdir -p "${STAGING_DIR}"

# --- Step 1: Build release binaries (unless --skip-build) ---
if [[ "$SKIP_BUILD" != "true" ]]; then
    echo "==> Building release binaries..."
    (cd "${REPO_ROOT}" && cargo build --release)
fi

# Verify binaries exist
for bin in scribe-client scribe-server scribe-settings scribe-driver; do
    if [[ ! -f "${BUILD_DIR}/${bin}" ]]; then
        echo "ERROR: ${BUILD_DIR}/${bin} not found. Run 'cargo build --release' first."
        exit 1
    fi
done

# --- Step 2: Create .icns from PNGs ---
echo "==> Creating app icon..."
ICONSET_DIR="${STAGING_DIR}/Scribe.iconset"
mkdir -p "${ICONSET_DIR}"

# Available source icons and their actual pixel dimensions:
#   scribe-icon-48.png   =   48x48
#   scribe-icon-64.png   =   64x64
#   scribe-icon-128.png  =  128x128
#   scribe-icon-256.png  =  256x256
#   scribe-icon-512.png  = 1024x1024
#
# macOS iconset required filenames and pixel dimensions:
#   icon_16x16.png       =   16x16
#   icon_16x16@2x.png    =   32x32
#   icon_32x32.png       =   32x32
#   icon_32x32@2x.png    =   64x64
#   icon_128x128.png     =  128x128
#   icon_128x128@2x.png  =  256x256
#   icon_256x256.png     =  256x256
#   icon_256x256@2x.png  =  512x512
#   icon_512x512.png     =  512x512
#   icon_512x512@2x.png  = 1024x1024

# Generate missing sizes via sips
sips -z 16 16 "${DIST_DIR}/scribe-icon-48.png" --out "${ICONSET_DIR}/icon_16x16.png" > /dev/null
sips -z 32 32 "${DIST_DIR}/scribe-icon-48.png" --out "${ICONSET_DIR}/icon_16x16@2x.png" > /dev/null
sips -z 32 32 "${DIST_DIR}/scribe-icon-48.png" --out "${ICONSET_DIR}/icon_32x32.png" > /dev/null
sips -z 512 512 "${DIST_DIR}/scribe-icon-512.png" --out "${ICONSET_DIR}/icon_256x256@2x.png" > /dev/null
sips -z 512 512 "${DIST_DIR}/scribe-icon-512.png" --out "${ICONSET_DIR}/icon_512x512.png" > /dev/null

# Copy exact-match sizes
cp "${DIST_DIR}/scribe-icon-64.png"  "${ICONSET_DIR}/icon_32x32@2x.png"
cp "${DIST_DIR}/scribe-icon-128.png" "${ICONSET_DIR}/icon_128x128.png"
cp "${DIST_DIR}/scribe-icon-256.png" "${ICONSET_DIR}/icon_128x128@2x.png"
cp "${DIST_DIR}/scribe-icon-256.png" "${ICONSET_DIR}/icon_256x256.png"
cp "${DIST_DIR}/scribe-icon-512.png" "${ICONSET_DIR}/icon_512x512@2x.png"

iconutil -c icns "${ICONSET_DIR}" -o "${STAGING_DIR}/Scribe.icns"

# --- Step 3: Assemble .app bundle ---
echo "==> Assembling ${BUNDLE_NAME}..."
APP_DIR="${STAGING_DIR}/${BUNDLE_NAME}"
CONTENTS="${APP_DIR}/Contents"
MACOS_DIR="${CONTENTS}/MacOS"
RESOURCES_DIR="${CONTENTS}/Resources"

mkdir -p "${MACOS_DIR}" "${RESOURCES_DIR}"

# Copy Info.plist
cp "${SCRIPT_DIR}/Info.plist" "${CONTENTS}/Info.plist"

# Inject version into staged Info.plist if --version was provided
if [[ -n "$VERSION" ]]; then
    sed -i '' \
        "/<key>CFBundleShortVersionString<\/key>/{n; s|<string>[^<]*</string>|<string>${VERSION}</string>|;}" \
        "${CONTENTS}/Info.plist"
fi

# Copy binaries
cp "${BUILD_DIR}/scribe-client"   "${MACOS_DIR}/"
cp "${BUILD_DIR}/scribe-server"   "${MACOS_DIR}/"
cp "${BUILD_DIR}/scribe-settings" "${MACOS_DIR}/"
cp "${BUILD_DIR}/scribe-driver"   "${MACOS_DIR}/"

# Copy icon
cp "${STAGING_DIR}/Scribe.icns" "${RESOURCES_DIR}/"

# Copy Claude Code hook integration scripts
cp "${DIST_DIR}/setup-claude-hooks.sh" "${RESOURCES_DIR}/"
cp "${DIST_DIR}/detect-claude-question.sh" "${RESOURCES_DIR}/"

echo "==> ${BUNDLE_NAME} assembled at ${APP_DIR}"

# --- Step 4: Create DMG ---
echo "==> Creating DMG..."

DMG_STAGING="${STAGING_DIR}/dmg-contents"
mkdir -p "${DMG_STAGING}"

# Copy .app to DMG staging
cp -R "${APP_DIR}" "${DMG_STAGING}/"

# Create Applications symlink for drag-to-install
ln -s /Applications "${DMG_STAGING}/Applications"

# Remove any existing DMG
DMG_OUTPUT="${REPO_ROOT}/target/${DMG_NAME}"
rm -f "${DMG_OUTPUT}"

# Create DMG
hdiutil create \
    -volname "${VOLUME_NAME}" \
    -srcfolder "${DMG_STAGING}" \
    -ov \
    -format UDZO \
    "${DMG_OUTPUT}"

echo ""
echo "==> Done! DMG created at: ${DMG_OUTPUT}"
echo "    Size: $(du -h "${DMG_OUTPUT}" | cut -f1)"
