#!/usr/bin/env bash
set -euo pipefail

recipe="$(just --show docker-unit-beads-write)"
case "$recipe" in
    *"--no-cache-filter beads-write-unit"*) ;;
    *)
        printf '%s\n' 'docker-unit-beads-write must invalidate beads-write-unit.' >&2
        exit 1
        ;;
esac

stage="$(sed -n '/^FROM func-base AS beads-write-unit$/,/^# The read-slice proof/p' docker/Dockerfile.func)"
if [[ -z "$stage" ]]; then
    printf '%s\n' 'beads-write-unit must not inherit staged runtime binaries.' >&2
    exit 1
fi

case "$stage" in
    *"target=/workspace/target"*)
        printf '%s\n' 'beads-write-unit must not share /workspace/target.' >&2
        exit 1
        ;;
esac

case "$stage" in
    *"target=/usr/local/cargo/registry"*"target=/usr/local/cargo/git"*) ;;
    *)
        printf '%s\n' 'beads-write-unit must retain Cargo download caches.' >&2
        exit 1
        ;;
esac
