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

newest_crates_commit="$(git log -1 --format=%ct -- crates/)"
if [[ -z "$newest_crates_commit" ]]; then
    printf 'ERROR: cannot determine the newest commit touching crates/.\n' >&2
    exit 2
fi

file_mtime() {
    local path="$1"
    if stat -f '%m' "$path" >/dev/null 2>&1; then
        stat -f '%m' "$path"
        return
    fi
    stat -c '%Y' "$path"
}

source_dir="target/$profile"
for binary in "$@"; do
    source_path="$source_dir/$binary"
    if [[ ! -f "$source_path" || ! -x "$source_path" ]]; then
        printf 'ERROR: required staging source %s is missing or not executable.\n' "$source_path" >&2
        printf 'Build it with `%s`, then retry.\n' "$build_command" >&2
        exit 2
    fi

    binary_mtime="$(file_mtime "$source_path")"
    if (( binary_mtime < newest_crates_commit )); then
        printf 'ERROR: required staging source %s is stale.\n' "$source_path" >&2
        printf 'It is older than the newest commit touching crates/.\n' >&2
        printf 'Rebuild it with `%s`, then retry.\n' "$build_command" >&2
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
