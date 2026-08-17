#!/usr/bin/env bash
set -euo pipefail

# Focused regression coverage for check-no-new-lint-suppressions.sh's
# --working-tree worktree pruning: a linked worktree nested inside the repo
# (this repo's own .worktrees/<task> layout) must not leak its suppressions
# into the scan, while a real new suppression in tracked source must still
# be rejected.

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
check_script="$script_dir/check-no-new-lint-suppressions.sh"

fixture=""
cleanup() {
    if [[ -n "$fixture" && -d "$fixture" ]]; then
        rm -rf "$fixture"
    fi
}
trap cleanup EXIT

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

fixture="$(mktemp -d)"
repo="$fixture/repo"
mkdir -p "$repo"
git -C "$repo" init -q
git -C "$repo" config user.email "test@example.com"
git -C "$repo" config user.name "test"

mkdir -p "$repo/tools" "$repo/crates/fake"
: >"$repo/tools/lint-suppressions-allowlist.txt"
cat >"$repo/crates/fake/lib.rs" <<'EOF'
pub fn ok() {}
EOF
git -C "$repo" add -A
git -C "$repo" commit -q -m init

# A linked worktree nested under .worktrees/, mirroring this repo's own
# implement-ready task-worktree layout.
git -C "$repo" worktree add -q -b child ".worktrees/child" >/dev/null
cat >"$repo/.worktrees/child/crates/fake/lib.rs" <<'EOF'
#[allow(clippy::too_many_lines)]
pub fn ok() {}
EOF

if ! (cd "$repo" && "$check_script" --working-tree); then
    fail "--working-tree rejected a suppression that exists only in a linked worktree"
fi
echo "PASS: linked worktree suppressions are ignored"

cat >"$repo/crates/fake/lib.rs" <<'EOF'
#[allow(clippy::too_many_lines)]
pub fn ok() {}
EOF

if (cd "$repo" && "$check_script" --working-tree); then
    fail "--working-tree accepted a new suppression added to tracked source"
fi
echo "PASS: a new suppression in tracked source is still rejected"

echo "OK: check-no-new-lint-suppressions.sh worktree regression coverage passed"
