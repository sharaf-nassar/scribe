#!/bin/sh
printf 'child=%s' "${XDG_DATA_DIRS-}" > "$1"
