#!/bin/sh
printf '%s\0%s\0%s\0%s\0' \
  "${SCRIBE_PROBE_QUOTE-!unset}" \
  "${SCRIBE_PROBE_MULTI-!unset}" \
  "${SCRIBE_PROBE_BS-!unset}" \
  "${SCRIBE_PROBE_STALE-!unset}" > "$1"
