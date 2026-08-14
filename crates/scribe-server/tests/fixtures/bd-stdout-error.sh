#!/bin/sh
printf '{"error":"envelope %s, ran in %s, args %s"}' "${BD_JSON_ENVELOPE:-unset}" "$(pwd)" "$*"
exit 1
