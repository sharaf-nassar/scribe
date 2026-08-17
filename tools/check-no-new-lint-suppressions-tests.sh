#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
check_script="$script_dir/check-no-new-lint-suppressions.sh"
fixture="$(mktemp -d)"
trap "rm -rf \"$fixture\"" EXIT

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

repo="$fixture/repo"
mkdir -p "$repo/tools" "$repo/crates/fake"
git -C "$repo" init -q
touch "$repo/tools/lint-suppressions-allowlist.txt"

printf 'pub fn ok() {}\n' >"$repo/crates/fake/lib.rs"
git -C "$repo" add -A
git -C "$repo" -c user.email="test@example.com" -c user.name="test" commit -q -m init

git -C "$repo" worktree add -q -b child ".worktrees/child" >/dev/null
printf '#[allow(clippy::too_many_lines)]\npub fn ok() {}\n' >"$repo/.worktrees/child/crates/fake/lib.rs"

if ! (cd "$repo" && "$check_script" --working-tree); then
    fail "--working-tree rejected a suppression that exists only in a linked worktree"
fi

printf '#[allow(clippy::too_many_lines)]\npub fn ok() {}\n' >"$repo/crates/fake/lib.rs"

if (cd "$repo" && "$check_script" --working-tree); then
    fail "--working-tree accepted a new suppression added to tracked source"
fi
