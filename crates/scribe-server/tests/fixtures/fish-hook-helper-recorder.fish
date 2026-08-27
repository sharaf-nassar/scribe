#!/usr/bin/env fish
if not set -q SCRIBE_RECORD_PATH
    echo "SCRIBE_RECORD_PATH is required" >&2
    exit 1
end

set -l payload (cat | string collect --allow-empty)
string join0 -- CALL $argv STDIN $payload >> "$SCRIBE_RECORD_PATH"
