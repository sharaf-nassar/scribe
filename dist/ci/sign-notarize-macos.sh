#!/bin/bash
set -euo pipefail

: "${MACOS_CERT_P12:?MACOS_CERT_P12 must be set}"
: "${MACOS_CERT_PASSWORD:?MACOS_CERT_PASSWORD must be set}"
: "${APPLE_ID:?APPLE_ID must be set}"
: "${APPLE_TEAM_ID:?APPLE_TEAM_ID must be set}"
: "${APPLE_APP_PASSWORD:?APPLE_APP_PASSWORD must be set}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

STAGING_DIR="${REPO_ROOT}/target/macos-staging"
APP_BUNDLE="${STAGING_DIR}/Scribe.app"
DMG_OUTPUT="${REPO_ROOT}/target/Scribe.dmg"

KEYCHAIN_NAME="scribe-ci-$$"
KEYCHAIN_PATH="${TMPDIR:-/tmp}/${KEYCHAIN_NAME}.keychain-db"
P12_FILE="${TMPDIR:-/tmp}/scribe-ci-$$.p12"

cleanup() {
    security delete-keychain "${KEYCHAIN_PATH}" 2>/dev/null || true
    rm -f "${P12_FILE}"
}
trap cleanup EXIT

echo "==> Setting up signing keychain..."
echo "$MACOS_CERT_P12" | base64 --decode > "${P12_FILE}"

security create-keychain -p "" "${KEYCHAIN_PATH}"
security set-keychain-settings -lut 21600 "${KEYCHAIN_PATH}"
security unlock-keychain -p "" "${KEYCHAIN_PATH}"

security import "${P12_FILE}" \
    -k "${KEYCHAIN_PATH}" \
    -P "${MACOS_CERT_PASSWORD}" \
    -T /usr/bin/codesign \
    -T /usr/bin/productsign

security set-key-partition-list \
    -S "apple-tool:,apple:" \
    -s \
    -k "" \
    "${KEYCHAIN_PATH}"

security list-keychains -d user -s "${KEYCHAIN_PATH}" $(security list-keychains -d user | tr -d '"')

echo "==> Extracting signing identity..."
MACOS_SIGNING_IDENTITY=$(security find-identity -v -p codesigning "${KEYCHAIN_PATH}" | head -1 | awk -F'"' '{print $2}')
if [ -z "${MACOS_SIGNING_IDENTITY}" ]; then
    echo "ERROR: No codesigning identity found in keychain"
    exit 1
fi
echo "    Identity: ${MACOS_SIGNING_IDENTITY}"

echo "==> Signing .app bundle..."
codesign \
    --deep \
    --force \
    --options runtime \
    --sign "${MACOS_SIGNING_IDENTITY}" \
    --keychain "${KEYCHAIN_PATH}" \
    "${APP_BUNDLE}"

echo "==> Re-creating DMG from signed .app..."
DMG_STAGING="${STAGING_DIR}/dmg-contents"
rm -rf "${DMG_STAGING}"
mkdir -p "${DMG_STAGING}"
cp -R "${APP_BUNDLE}" "${DMG_STAGING}/"
ln -s /Applications "${DMG_STAGING}/Applications"

rm -f "${DMG_OUTPUT}"
hdiutil create \
    -volname "Scribe" \
    -srcfolder "${DMG_STAGING}" \
    -ov \
    -format UDZO \
    "${DMG_OUTPUT}"

echo "==> Submitting DMG for notarization..."
xcrun notarytool submit "${DMG_OUTPUT}" \
    --apple-id "${APPLE_ID}" \
    --team-id "${APPLE_TEAM_ID}" \
    --password "${APPLE_APP_PASSWORD}" \
    --wait

echo "==> Stapling notarization ticket..."
xcrun stapler staple "${DMG_OUTPUT}"

echo "==> Signing and notarization complete."
