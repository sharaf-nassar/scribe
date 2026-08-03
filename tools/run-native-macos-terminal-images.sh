#!/usr/bin/env bash
set -euo pipefail

if [[ "${GITHUB_ACTIONS:-}" != "true" || "${RUNNER_OS:-}" != "macOS" ]]; then
    printf '%s\n' \
        'ERROR: native macOS validation is authorized only in GitHub Actions.' >&2
    exit 2
fi

if [[ "${RUNNER_ARCH:-}" != "ARM64" || \
      "${SCRIBE_NATIVE_MACOS_RUNNER:-}" != "github-actions-macos-14-xlarge" ]]; then
    printf '%s\n' \
        'ERROR: native macOS validation requires the sanctioned Metal runner.' >&2
    exit 2
fi

repo_root="$(git rev-parse --show-toplevel)"
driver="$repo_root/tests/native-macos/terminal-images-metal.sh"
output_dir="$repo_root/test-output/terminal-images/macos"
mkdir -p "$output_dir"

if [[ ! -f "$driver" || ! -x "$driver" ]]; then
    printf 'ERROR: native corpus driver is missing or not executable: %s\n' \
        "$driver" >&2
    printf '%s\n' \
        'Downstream native-corpus work must add it before runtime validation.' >&2
    exit 2
fi

export SCRIBE_NATIVE_MACOS_OUTPUT_DIR="$output_dir"
"$driver" 2>&1 | tee "$output_dir/run.log"
