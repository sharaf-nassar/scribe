#!/usr/bin/env bash
set -euo pipefail

usage() {
    printf 'Usage: %s <release|debug> <binary>...\n' "$0" >&2
}

if (( $# < 2 )); then
    usage
    exit 2
fi

profile="$1"
shift

case "$profile" in
    release)
        build_command="just build-release"
        ;;
    debug)
        build_command="just build"
        ;;
    *)
        printf 'ERROR: invalid profile %q; expected release or debug.\n' "$profile" >&2
        exit 2
        ;;
esac

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

# Let cargo decide what is current. It owns the dependency graph and the
# fingerprints, so it rebuilds exactly what changed and returns in well under a
# second when nothing did.
#
# This replaces an mtime-versus-commit-timestamp heuristic that could not work:
# binaries are built BEFORE the commit that lands them, so every commit made
# every binary look stale, and the remedy it suggested — rerun the build — was
# a no-op precisely because cargo already knew they were current. Comparing
# against source mtimes fails the same way across crates: editing one crate
# does not stale another crate's binary, but any tree-wide comparison says it
# does.
printf 'Ensuring %s binaries are current...\n' "$profile" >&2
$build_command >&2

source_dir="target/$profile"
for binary in "$@"; do
    source_path="$source_dir/$binary"
    if [[ ! -f "$source_path" || ! -x "$source_path" ]]; then
        printf 'ERROR: required staging source %s is missing or not executable.\n' "$source_path" >&2
        printf 'The build reported success, so this binary is not one it produces.\n' >&2
        exit 2
    fi
done

stage_root="target/e2e-stage"
stage_dir="$stage_root/$profile"
mkdir -p "$stage_root"
temporary_dir="$(mktemp -d "$stage_root/.${profile}.XXXXXX")"

cleanup() {
    if [[ -n "${temporary_dir:-}" && -d "$temporary_dir" ]]; then
        rm -rf -- "$temporary_dir"
    fi
}
trap cleanup EXIT

for binary in "$@"; do
    cp -p -- "$source_dir/$binary" "$temporary_dir/$binary"
done

rm -rf -- "$stage_dir"
mv -- "$temporary_dir" "$stage_dir"
temporary_dir=""
trap - EXIT

printf 'Staged %s binaries in %s.\n' "$profile" "$stage_dir"
