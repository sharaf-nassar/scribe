#!/bin/bash
set -euo pipefail
#
# Scribe — Pi AI extension setup
#
# Installs Scribe's Pi lifecycle extension at Pi's documented global
# auto-discovery path so a fresh Pi process picks up AI state, prompt, task,
# and context tracking automatically. Never touches a project-local
# `.pi/extensions/` directory or Pi's own `settings.json` — a single global
# registration keeps Scribe from tripping the duplicate-extension failure Pi
# raises when the same tool name is registered at both scopes.
#
# The installed file is content-identical between the stable and development
# packages: it resolves the active hook helper at runtime from
# `SCRIBE_HOOK_HELPER`, so either flavor may run this script safely and a
# second flavor's run of this script is a no-op once the first has installed
# it.
#
# Idempotent: re-running with identical source content performs no write.
# Refuses to overwrite a target that exists but carries no Scribe ownership
# marker, leaving it untouched and reporting the collision instead of
# clobbering unrelated user or third-party extension code.
#
# Usage:
#   setup-pi-extension.sh --extension-source <dir>
#
# <dir>/pi-extension.ts is installed as
# ~/.pi/agent/extensions/scribe-ai-integration.ts.

MARKER="// SCRIBE-MANAGED-PI-EXTENSION"
EXTENSION_SOURCE=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --extension-source)
            EXTENSION_SOURCE="${2:-}"
            shift 2
            ;;
        *)
            echo "setup-pi-extension.sh: unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

if [[ -z "$EXTENSION_SOURCE" ]]; then
    echo "setup-pi-extension.sh: --extension-source <dir> is required" >&2
    exit 1
fi

SOURCE_FILE="${EXTENSION_SOURCE}/pi-extension.ts"
if [[ ! -f "$SOURCE_FILE" ]]; then
    echo "setup-pi-extension.sh: source file not found: ${SOURCE_FILE}" >&2
    exit 1
fi

if ! head -n 1 "$SOURCE_FILE" | grep -qF "$MARKER"; then
    echo "setup-pi-extension.sh: source file is missing the Scribe ownership marker: ${SOURCE_FILE}" >&2
    exit 1
fi

EXTENSIONS_DIR="${HOME}/.pi/agent/extensions"
TARGET_FILE="${EXTENSIONS_DIR}/scribe-ai-integration.ts"

mkdir -p "$EXTENSIONS_DIR"

if [[ -e "$TARGET_FILE" || -L "$TARGET_FILE" ]]; then
    if [[ -L "$TARGET_FILE" || ! -f "$TARGET_FILE" ]]; then
        echo "setup-pi-extension.sh: refusing to replace a non-regular file at ${TARGET_FILE}" >&2
        exit 1
    fi
    if ! head -n 1 "$TARGET_FILE" | grep -qF "$MARKER"; then
        echo "setup-pi-extension.sh: an existing extension at ${TARGET_FILE} was not installed by Scribe; leaving it in place. Remove or rename it, then retry Pi integration setup." >&2
        exit 1
    fi
    if cmp -s "$SOURCE_FILE" "$TARGET_FILE"; then
        echo "  ${TARGET_FILE} already up to date"
        exit 0
    fi
fi

TMP_FILE="$(mktemp "${EXTENSIONS_DIR}/.scribe-ai-integration.ts.XXXXXX")"
trap 'rm -f "$TMP_FILE"' EXIT
cp "$SOURCE_FILE" "$TMP_FILE"
chmod 0644 "$TMP_FILE"
mv -f "$TMP_FILE" "$TARGET_FILE"

echo "  Installed Pi extension at ${TARGET_FILE}"
