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
NOTARY_SUBMIT_LOG="${TMPDIR:-/tmp}/scribe-notary-submit-$$.log"
SUBMISSION_ID=""

cleanup() {
    security delete-keychain "${KEYCHAIN_PATH}" 2>/dev/null || true
    rm -f "${P12_FILE}" "${NOTARY_SUBMIT_LOG}"
}
trap cleanup EXIT

sign_code() {
    codesign \
        --force \
        --options runtime \
        --timestamp \
        --sign "${MACOS_SIGNING_IDENTITY}" \
        --keychain "${KEYCHAIN_PATH}" \
        "$1"
}

print_notary_log() {
    if [ -z "${SUBMISSION_ID}" ]; then
        echo "ERROR: No notarization submission ID was found."
        return
    fi

    echo "==> Fetching notarization log for ${SUBMISSION_ID}..."
    xcrun notarytool log "${SUBMISSION_ID}" \
        --apple-id "${APPLE_ID}" \
        --team-id "${APPLE_TEAM_ID}" \
        --password "${APPLE_APP_PASSWORD}" \
        || true
}

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

MAIN_EXECUTABLE=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "${APP_BUNDLE}/Contents/Info.plist")
if [ -z "${MAIN_EXECUTABLE}" ]; then
    echo "ERROR: CFBundleExecutable is missing from ${APP_BUNDLE}/Contents/Info.plist"
    exit 1
fi

echo "==> Signing nested executables..."
for executable in "${APP_BUNDLE}/Contents/MacOS/"*; do
    if [ -f "${executable}" ] && [ -x "${executable}" ]; then
        if [ "$(basename "${executable}")" = "${MAIN_EXECUTABLE}" ]; then
            continue
        fi
        sign_code "${executable}"
    fi
done

echo "==> Signing .app bundle..."
sign_code "${APP_BUNDLE}"

echo "==> Verifying .app signature..."
codesign --verify --deep --strict --verbose=2 "${APP_BUNDLE}"

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
set +e
xcrun notarytool submit "${DMG_OUTPUT}" \
    --apple-id "${APPLE_ID}" \
    --team-id "${APPLE_TEAM_ID}" \
    --password "${APPLE_APP_PASSWORD}" \
    --wait 2>&1 | tee "${NOTARY_SUBMIT_LOG}"
NOTARY_STATUS=${PIPESTATUS[0]}
set -e

SUBMISSION_ID=$(awk '/^[[:space:]]*id: / { print $2; exit }' "${NOTARY_SUBMIT_LOG}")
if [ "${NOTARY_STATUS}" -ne 0 ]; then
    print_notary_log
    exit "${NOTARY_STATUS}"
fi

if ! grep -q '^[[:space:]]*status: Accepted[[:space:]]*$' "${NOTARY_SUBMIT_LOG}"; then
    echo "ERROR: Notarization was not accepted."
    print_notary_log
    exit 1
fi

echo "==> Stapling notarization ticket..."
xcrun stapler staple "${DMG_OUTPUT}"

echo "==> Signing and notarization complete."
