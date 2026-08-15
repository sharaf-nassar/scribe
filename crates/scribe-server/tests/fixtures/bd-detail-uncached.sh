#!/bin/sh
if [ "$*" = "--readonly --json -C $PWD ready --limit 0" ]; then
  printf '%s' '{"data":[{"id":"issue","title":"Issue","status":"open","priority":2}],"schema_version":1}'
  exit
fi
expected="--readonly --json -C $PWD show issue --include-comments --include-dependents"
[ "$*" = "$expected" ] || { printf '%s' '{"error":"wrong argv"}'; exit 1; }
calls=0
[ ! -f calls ] || calls=$(sed -n '1p' calls)
calls=$((calls + 1))
printf '%s\n' "$calls" > calls
printf '{"data":{"id":"issue","title":"call %s","status":"open","priority":2,"issue_type":"task","created_at":"now","updated_at":"now"},"schema_version":1}' "$calls"
