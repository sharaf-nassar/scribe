#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
output="${1:-${script_dir}/../../target/release/scribe-beads-board}"
mkdir -p "$(dirname "$output")"

cd "$script_dir"
CGO_ENABLED=1 go build -mod=readonly -trimpath -tags gms_pure_go -ldflags="-s -w" -o "$output" .
