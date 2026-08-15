#!/bin/bash
set -euo pipefail

MODE_FILE=/tmp/scribe-beads-write-fault-mode
command_name=
issue_id=
args=("$@")
for ((index = 0; index < ${#args[@]}; index++)); do
    if [ "${args[$index]}" = update ]; then
        command_name=update
        issue_id=${args[$((index + 1))]:-}
        break
    fi
done
mode=$(cat "$MODE_FILE" 2>/dev/null || true)
target=write-fault
if [[ "$mode" == *:* ]]; then
    target=${mode#*:}
    mode=${mode%%:*}
fi

if [ "$command_name" = "update" ] && [ "$issue_id" = "$target" ]; then
    case "$mode" in
        nonzero)
            echo '{"error":"forced nonzero write"}' >&2
            exit 9
            ;;
        timeout)
            sleep 30
            exit 0
            ;;
    esac
fi

real_bd=/usr/local/bin/bd
[ -x /usr/local/bin/bd-real ] && real_bd=/usr/local/bin/bd-real
exec "$real_bd" "$@"
