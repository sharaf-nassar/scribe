#!/bin/sh
set -eu

for arg in "$@"; do
    if [ "$arg" = show ]; then
        status=$(cat status)
        assignee=$(cat assignee)
        if [ -n "$assignee" ]; then
            assignee_json="\"$assignee\""
        else
            assignee_json=null
        fi
        printf '{"data":[{"id":"issue","title":"Issue","status":"%s","priority":2,"issue_type":"task","assignee":%s}],"schema_version":1}\n' "$status" "$assignee_json"
        exit 0
    fi
done

printf '%s\n' "$*" >> writes
if [ "$(cat mode 2>/dev/null || true)" = timeout ]; then
    sleep 30
fi
printf '{"data":{"id":"issue"},"schema_version":1}\n'
