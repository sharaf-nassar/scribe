#!/bin/bash
# Verify that the built scribe-dev package and an installed dev flavor are the
# exact release bytes that passed the A2/A3 source contracts. This is read-only:
# it only extracts the package into a temporary directory.
set -u
set -o pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
package=""
installed_root="/"
package_only=0

usage() {
    cat <<'EOF'
Usage: tests/install/dev-package-smoke.sh [options]

Options:
  --package PATH         scribe-dev .deb to inspect (default: target/debian/scribe-dev_*.deb)
  --installed-root PATH  root containing an installed scribe-dev payload (default: /)
  --package-only         verify source-to-package parity without an installed payload
  -h, --help             show this help
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --package)
            [ "$#" -ge 2 ] || { usage >&2; exit 2; }
            package="$2"
            shift 2
            ;;
        --installed-root)
            [ "$#" -ge 2 ] || { usage >&2; exit 2; }
            installed_root="$2"
            shift 2
            ;;
        --package-only)
            package_only=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            exit 2
            ;;
    esac
done

if [ -z "$package" ]; then
    shopt -s nullglob
    packages=("$repo_root"/target/debian/scribe-dev_*.deb)
    shopt -u nullglob
    if [ "${#packages[@]}" -ne 1 ]; then
        echo "FAIL: expected exactly one target/debian/scribe-dev_*.deb; use --package" >&2
        exit 2
    fi
    package="${packages[0]}"
fi

for command in cmp dpkg-deb grep mktemp sha256sum; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "FAIL: required command is unavailable: $command" >&2
        exit 2
    fi
done

if [ ! -r "$package" ]; then
    echo "FAIL: package is unreadable: $package" >&2
    exit 2
fi
if [ "$package_only" -eq 0 ] && [ ! -d "$installed_root" ]; then
    echo "FAIL: installed root is not a directory: $installed_root" >&2
    exit 2
fi

failures=0
temp_dir="$(mktemp -d)"
trap 'rm -rf "$temp_dir"' EXIT
package_root="$temp_dir/payload"
control_root="$temp_dir/control"

fail() {
    echo "FAIL: $*" >&2
    failures=$((failures + 1))
}

pass() {
    echo "PASS: $*"
}

sha256() {
    sha256sum "$1" | cut -d' ' -f1
}

compare_bytes() {
    local expected="$1"
    local actual="$2"
    local label="$3"

    if [ ! -f "$expected" ]; then
        fail "$label source is missing: $expected"
    elif [ ! -f "$actual" ]; then
        fail "$label is missing: $actual"
    elif cmp -s "$expected" "$actual"; then
        pass "$label ($(sha256 "$actual"))"
    else
        fail "$label differs: expected $(sha256 "$expected"), got $(sha256 "$actual")"
    fi
}

package_name="$(dpkg-deb -f "$package" Package 2>/dev/null)"
if [ "$package_name" = "scribe-dev" ]; then
    pass "package identity is scribe-dev"
else
    fail "package identity is ${package_name:-unreadable}, expected scribe-dev"
fi

if ! dpkg-deb -x "$package" "$package_root"; then
    fail "could not extract $package"
fi
if ! dpkg-deb -e "$package" "$control_root"; then
    fail "could not extract control data from $package"
fi

# This list deliberately names every dev-flavor payload asset. The package
# manifest is the producer; this independent consumer catches omitted or stale
# assets before a human installs the package.
assets=(
    "target/release/scribe-client|usr/bin/scribe-dev"
    "target/release/scribe-server|usr/bin/scribe-dev-server"
    "target/release/scribe-cli|usr/bin/scribe-dev-cli"
    "dist/scribe-dev-server.service|usr/lib/systemd/user/scribe-dev-server.service"
    "dist/scribe-dev.desktop|usr/share/applications/scribe-dev.desktop"
    "dist/scribe-icon-48.png|usr/share/icons/hicolor/48x48/apps/scribe-dev.png"
    "dist/scribe-icon-128.png|usr/share/icons/hicolor/128x128/apps/scribe-dev.png"
    "dist/scribe-icon-256.png|usr/share/icons/hicolor/256x256/apps/scribe-dev.png"
    "dist/scribe-icon-512.png|usr/share/icons/hicolor/512x512/apps/scribe-dev.png"
    "target/release/scribe-hook-helper|usr/share/scribe-dev/scribe-hook-helper"
    "dist/ai-hook-claude.sh|usr/share/scribe-dev/ai-hook-claude.sh"
    "dist/ai-hook-statusline.sh|usr/share/scribe-dev/ai-hook-statusline.sh"
    "dist/setup-claude-hooks.sh|usr/share/scribe-dev/setup-claude-hooks.sh"
    "dist/ai-hook-codex.sh|usr/share/scribe-dev/ai-hook-codex.sh"
    "dist/setup-codex-hooks.sh|usr/share/scribe-dev/setup-codex-hooks.sh"
    "dist/pi-extension.ts|usr/share/scribe-dev/pi-extension.ts"
    "dist/setup-pi-extension.sh|usr/share/scribe-dev/setup-pi-extension.sh"
    "dist/shell-integration/bash/scribe.bash|usr/share/scribe-dev/shell-integration/bash/scribe.bash"
    "dist/shell-integration/zsh/.zshenv|usr/share/scribe-dev/shell-integration/zsh/.zshenv"
    "dist/shell-integration/zsh/scribe.zsh|usr/share/scribe-dev/shell-integration/zsh/scribe.zsh"
    "dist/shell-integration/fish/vendor_conf.d/scribe.fish|usr/share/scribe-dev/shell-integration/fish/vendor_conf.d/scribe.fish"
    "dist/shell-integration/nushell/vendor/autoload/scribe.nu|usr/share/scribe-dev/shell-integration/nushell/vendor/autoload/scribe.nu"
    "dist/shell-integration/powershell/scribe.ps1|usr/share/scribe-dev/shell-integration/powershell/scribe.ps1"
)

for asset in "${assets[@]}"; do
    source_path="${asset%%|*}"
    payload_path="${asset#*|}"
    compare_bytes "$repo_root/$source_path" "$package_root/$payload_path" \
        "package asset $payload_path"
    if [ "$package_only" -eq 0 ]; then
        compare_bytes "$package_root/$payload_path" "$installed_root/$payload_path" \
            "installed asset $payload_path"
    fi
done

for script in preinst postinst prerm postrm; do
    compare_bytes "$repo_root/dist/debian/$script" "$control_root/$script" \
        "package maintainer script $script"
done

for stable_path in usr/bin/scribe-client usr/bin/scribe-server usr/share/scribe; do
    if [ -e "$package_root/$stable_path" ]; then
        fail "dev package leaks stable path $stable_path"
    else
        pass "dev package excludes stable path $stable_path"
    fi
done

if [ "$failures" -gt 0 ]; then
    echo "$failures dev package smoke check(s) failed." >&2
    exit 1
fi

if [ "$package_only" -eq 1 ]; then
    echo "Dev package smoke passed: source and package payload match."
else
    echo "Dev package smoke passed: source, package, and installed payload match."
fi
