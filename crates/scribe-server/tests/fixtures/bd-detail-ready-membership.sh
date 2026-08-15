#!/bin/sh
case "$*" in
  "--readonly --json -C $PWD show backlog --include-comments --include-dependents")
    printf '%s' '{"data":{"id":"backlog","title":"Backlog","status":"open","priority":4,"issue_type":"task","created_at":"now","updated_at":"now"},"schema_version":1}' ;;
  "--readonly --json -C $PWD show ready --include-comments --include-dependents")
    printf '%s' '{"data":{"id":"ready","title":"Ready","status":"open","priority":4,"issue_type":"task","created_at":"now","updated_at":"now"},"schema_version":1}' ;;
  "--readonly --json -C $PWD ready --limit 0")
    printf '%s' '{"data":[{"id":"ready","title":"Ready","status":"open","priority":4}],"schema_version":1}' ;;
  *) printf '%s' '{"error":"wrong argv"}'; exit 1 ;;
esac
