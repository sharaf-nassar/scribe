#!/bin/bash
set -e

# Attach is only lossless if the sink is installed BEFORE the replay snapshot.
# The pre-fix order snapshotted first and installed afterwards, so every byte
# the PTY emitted in between reached a sink-less no-op send and vanished; the
# naive inverse (install first, snapshot later) duplicates instead. Neither
# defect is observable unless output is genuinely in flight across the attach,
# so this test reattaches a session mid-burst and compares the client's
# replayed view against the server's own screen AND scrollback.
#
# End markers are spelled `"FOO""-BAR"` throughout: the session echoes every
# command it is sent, so a marker written literally would match its own echo
# and every wait below would return before the output existed.

SAVED_SESSION="$SESSION"

# Bursts rather than one flat-out run: unthrottled bash outpaces a reattach by
# an order of magnitude and would be over before the attach started, while a
# per-line sleep costs a fork per line and drops the rate below the
# few-millisecond replay-build window. 20 bursts of 300 lines keeps output
# arriving for a few hundred milliseconds at a rate that window cannot miss,
# and caps each round's scrollback at 6000 rows — under the 10 000-row cap, so
# a row lost or duplicated still changes the row COUNT rather than sliding
# under a saturated history, and under the 64 MiB frame cap a full history
# blows past.
BURST='(n=0; while [ $n -lt 20 ]; do n=$((n+1)); i=0; while [ $i -lt 300 ]; do i=$((i+1)); echo "stream-$n-$i"; done; sleep 0.01; done; echo "STREAM""-DONE") &\n'

LIVE_ROUNDS=0

for round in 1 2 3; do
    scribe-test send "$SAVED_SESSION" "$BURST"

    scribe-test daemon stop
    scribe-test daemon start
    scribe-test session attach "$SAVED_SESSION" > /dev/null

    # A round whose burst finished before the attach proves nothing. Machine
    # speed decides that, so it is reported rather than failed here — the run
    # only fails if EVERY round came up empty, which means the test has gone
    # vacuous and needs its burst retuned.
    STATUS=$(scribe-test replay status "$SAVED_SESSION" --min-frames 1 --timeout 5000)
    case "$STATUS" in
        *live-after=0) echo "  round $round: no output in flight across the attach" ;;
        *) LIVE_ROUNDS=$((LIVE_ROUNDS + 1)) ;;
    esac

    # The replayed view is this round's replay plus every PtyOutput byte after
    # it. A chunk lost in the attach window leaves its history permanently
    # short of the server's; a duplicated flush leaves it long. A plain
    # `RequestSnapshot` stays green for both, which is why this compares the
    # replay path.
    scribe-test wait-output "$SAVED_SESSION" "STREAM-DONE" --timeout 20000
    scribe-test wait-idle "$SAVED_SESSION" --ms 500
    scribe-test replay assert-matches "$SAVED_SESSION"
    echo "PHASE $round PASS: mid-burst attach lost and duplicated nothing"

    # Drop the history so the next round starts from an empty scrollback and
    # stays under the cap. `ESC [ 3 J` travels the same output stream both
    # sides consume, so it cannot itself desynchronise them.
    scribe-test send "$SAVED_SESSION" "printf '\e[3J'; echo \"CLEAR\"\"ED\"\n"
    scribe-test wait-output "$SAVED_SESSION" "CLEARED" --timeout 10000
    scribe-test wait-idle "$SAVED_SESSION" --ms 300
done

if [ "$LIVE_ROUNDS" -eq 0 ]; then
    echo "FAIL: no round had output in flight across its attach; retune the burst" >&2
    exit 1
fi

echo "PASS: attach-lossless test completed ($LIVE_ROUNDS/3 rounds with output in flight)"
