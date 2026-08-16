#!/usr/bin/env bash
# Container HEALTHCHECK: read the verdict the resident checker published.
#
# This runs once per second in EVERY clone, forever, so it must be cheap. It is
# a file read and two integer comparisons: no interpreter, no network, no fork
# beyond this shell. The actual CDP round trip is done by cdp_health.py --loop,
# which runs as a resident service started by entry.sh and is therefore paid
# once, at the golden, where its dirtied pages join the snapshot and are SHARED
# by every clone.
#
# Measured in this image before the change: python3 startup 9.1ms, the whole
# healthcheck 43.6ms even when failing fast. Per clone. Per second.
#
# HEALTHY MEANS FRESH AND HEALTHY, both. A stale "healthy" is refused: it says
# the checker was healthy once, not that it is healthy now, and those differ
# exactly when the checker has died.
#
# FAIL CLOSED. A missing file, an unreadable file, an unparsable stamp, or a
# stale verdict is UNHEALTHY. A reader that returned the last known verdict
# after its writer died would report healthy forever, which is the same
# green-by-absence bug this repo keeps finding in gates: silence read as
# success.
#
# Freshness is measured against /proc/uptime, NOT the wall clock and NOT the
# file's mtime. fc-agent steps CLOCK_REALTIME on every restore
# (set_system_clock), so a wall-clock reader would judge every clone's freshly
# written verdict to be hours old and fail the gate on every single clone.
# /proc/uptime is CLOCK_MONOTONIC and is exactly what the writer records.
set -uo pipefail

STATE_FILE="${BENCH_HEALTH_STATE:-/run/bench-health}"
# Budget arithmetic, not a round number. The writer loops every 1s and its CDP
# probe has a 3s timeout (BENCH_CDP_HEALTH_TIMEOUT), so one genuinely slow
# iteration can stretch the gap between writes to about 4s.
#
# Both endpoints are truncated to whole seconds below, and floor(now) -
# floor(stamp) lies in (age-1, age+1): the computed value can be up to a second
# LARGER than the true age as well as smaller. So a budget of 7 enforces a real
# ceiling somewhere in 6s to 8s, and guarantees a genuine 4s gap is never
# refused. An earlier comment here claimed truncation could only shrink the
# figure, and quoted a 10s budget that had already been changed to 6.
MAX_AGE="${BENCH_HEALTH_MAX_AGE:-7}"

if [ ! -r "$STATE_FILE" ]; then
	echo "unhealthy: no verdict at $STATE_FILE (is cdp_health.py --loop running?)" >&2
	exit 1
fi

read -r verdict stamp detail <"$STATE_FILE" || {
	echo "unhealthy: could not read $STATE_FILE" >&2
	exit 1
}

read -r now _ </proc/uptime || {
	echo "unhealthy: could not read /proc/uptime" >&2
	exit 1
}

# Validate BEFORE arithmetic. $(( )) evaluates its operands, so unvalidated file
# content here is both a crash and an evaluation surface. Verified before this
# guard existed: a stamp of "notanumber" died with "line 56: notanumber:
# unbound variable" and an empty line with "operand expected", and the script
# exited 1 only because `set -u` aborted on the unset `age`. That is the right
# exit status for the wrong reason: it depends on a shell option rather than on
# a check, and the accompanying test passed on that accident.
case "$verdict" in
healthy | unhealthy) ;;
*)
	echo "unhealthy: unrecognised verdict ${verdict:-(empty)} in $STATE_FILE" >&2
	exit 1
	;;
esac

case "${stamp:-}" in
'' | *[!0-9.]* | *.*.*)
	echo "unhealthy: unparsable stamp ${stamp:-(empty)} in $STATE_FILE" >&2
	exit 1
	;;
esac

# Bash has no floats; compare whole seconds. Both operands are validated above.
age=$(( ${now%.*} - ${stamp%.*} ))

if [ "$age" -lt 0 ]; then
	# Monotonic time cannot go backwards within one boot, so this means the
	# verdict was written before a REBOOT of this guest and the file survived
	# on a persistent mount. Refuse it rather than trust it.
	echo "unhealthy: verdict stamp is in the future (age ${age}s); stale across a boot" >&2
	exit 1
fi

if [ "$age" -gt "$MAX_AGE" ]; then
	echo "unhealthy: verdict is ${age}s old (max ${MAX_AGE}s): $verdict $detail" >&2
	exit 1
fi

if [ "$verdict" != healthy ]; then
	echo "unhealthy: $detail (age ${age}s)" >&2
	exit 1
fi

echo "healthy (age ${age}s) $detail"
