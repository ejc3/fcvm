#!/usr/bin/env bash
# In-job D-state watchdog: stream teardown-wedge evidence into the LIVE job log.
#
# Why stdout and nothing else: CI run 31363886999 (job 93378160536) wedged
# test-root on an EPHEMERAL runner — the log stream stopped at 07:35:56, the
# job failed at 07:47:53, and the instance was terminated. Post-job guard
# steps and artifact uploads structurally cannot capture that class: the
# runner dies with its evidence, every time. Whatever already reached the
# step's log pipe is the only thing that survives, so this watchdog is
# backgrounded INSIDE the test step and prints there.
#
# Detection: a firecracker/cloud-hypervis/fcvm task group with a D-state
# sibling in two CONSECUTIVE samples (~40s at the default 20s interval), or a
# zombie leader with a live sibling for two consecutive samples. One sample is
# deliberately not enough — ordinary I/O passes through D for moments and an
# exiting group is briefly zombie-led; persistence is what separates a wedge
# from a snapshot of normal life. When healthy it prints NOTHING: a green run
# pays zero log lines for this.
#
# On first detection it prints a delimited full dump (per-TID stack/wchan/
# status, kcompactd/kswapd stack samples, vmstat compaction counters, memory
# pressure, dmesg tail), then rate-limits to one dump per 3 minutes while the
# condition persists. Same procfs discipline as ci-stray-vm-guard.sh: the scan
# and the evidence readers never touch cmdline/argv (those reads fault the
# target mm and hang exactly when the target is wedged).
#
# Usage: ci-dstate-watchdog.sh   (background it; SIGTERM stops it)
#   FCVM_WATCHDOG_INTERVAL_SECONDS       sample interval (default 20)
#   FCVM_WATCHDOG_DUMP_INTERVAL_SECONDS  min gap between dumps (default 180)
#   FCVM_GUARD_SCAN_TIMEOUT_SECONDS      ps snapshot deadline (default 10)
set -uo pipefail

INTERVAL="${FCVM_WATCHDOG_INTERVAL_SECONDS:-20}"
DUMP_GAP="${FCVM_WATCHDOG_DUMP_INTERVAL_SECONDS:-180}"
SCAN_TIMEOUT="${FCVM_GUARD_SCAN_TIMEOUT_SECONDS:-10}"

# shellcheck source=scripts/lib/vm-evidence.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib/vm-evidence.sh"

TEMPORARY_PREFIX="${TMPDIR:-/tmp}/.fcvm-dstate-watchdog.$$"
# shellcheck disable=SC2317  # reached via `trap ... EXIT`, not fall-through
cleanup_temporary_files() {
	rm -f -- "${TEMPORARY_PREFIX}".*
}
trap cleanup_temporary_files EXIT
trap 'exit 0' TERM INT

# Suspect groups from a selected-threads artifact, one "tgid<TAB>reason" per
# line. Reasons are part of the report contract, so name them precisely.
suspects_of() {
	awk -F '\t' '
		NR == 1 { next }
		{
			seen[$1] = 1
			if ($4 ~ /^D/) dstate[$1] = 1
			if ($1 == $2 && $4 ~ /^Z/) zleader[$1] = 1
			if ($4 !~ /^Z/) live[$1] = 1
		}
		END {
			for (g in seen) {
				if (dstate[g])
					print g "\tD-state sibling"
				else if (zleader[g] && live[g])
					print g "\tzombie leader with live sibling"
			}
		}
	' "$1"
}

dump_wedge_evidence() {
	local selected=$1 persistent=$2
	local tgids
	mapfile -t tgids < <(printf '%s\n' "$persistent" | cut -f1)
	echo "======== FCVM D-STATE WATCHDOG: wedged microVM task group(s) detected at $(date -u '+%Y-%m-%dT%H:%M:%SZ') ========"
	echo "suspect groups persisting across two consecutive samples (interval ${INTERVAL}s):"
	printf '%s\n' "$persistent"
	echo "thread snapshot (TGID TID PPID STATE THREAD):"
	awk -F '\t' -v list="${tgids[*]}" '
		BEGIN { n = split(list, a, " "); for (i = 1; i <= n; i++) want[a[i]] = 1 }
		NR == 1 || ($1 in want)
	' "$selected"
	vm_evidence_group "$selected" "d-state watchdog" "${tgids[@]}"
	vm_evidence_mm_diagnostics 120
	echo "======== END FCVM D-STATE WATCHDOG DUMP ========"
}

PREV_SUSPECTS=""
LAST_DUMP=-1
LAST_SCAN_NOTE=-1

while :; do
	ALL=$(mktemp "${TEMPORARY_PREFIX}.XXXXXX")
	SELECTED=$(mktemp "${TEMPORARY_PREFIX}.XXXXXX")
	LIVE=$(mktemp "${TEMPORARY_PREFIX}.XXXXXX")
	ZOMBIE=$(mktemp "${TEMPORARY_PREFIX}.XXXXXX")

	SCAN_NOTE=$(vm_scan_all_threads "$ALL" "$SCAN_TIMEOUT" "$TEMPORARY_PREFIX" 2>&1)
	SCAN_RC=$?
	if [ "$SCAN_RC" -ne 0 ]; then
		# A ps that cannot finish is itself the wedge signature (the guard's
		# header documents the D-state reader hang). Say so, rate-limited.
		# The NOTE prefix is deliberately distinct from the dump header so
		# log consumers counting dumps never conflate the two.
		if [ "$LAST_SCAN_NOTE" -lt 0 ] || [ $((SECONDS - LAST_SCAN_NOTE)) -ge "$DUMP_GAP" ]; then
			echo "FCVM D-STATE WATCHDOG NOTE: thread scan failed (rc=${SCAN_RC}): ${SCAN_NOTE}"
			LAST_SCAN_NOTE=$SECONDS
		fi
		# No sample this round: leave PREV_SUSPECTS as-is — persistence is
		# judged across successful scans only.
	else
		vm_select_vm_groups "$ALL" "$SELECTED" "$LIVE" "$ZOMBIE"
		SUSPECTS=$(suspects_of "$SELECTED")
		PERSISTENT=""
		if [ -n "$SUSPECTS" ] && [ -n "$PREV_SUSPECTS" ]; then
			# Persistence is keyed on the TGID; the reason reported is the
			# CURRENT one (a group can move from D-sibling to zombie-led).
			PERSISTENT=$(awk -F '\t' '
				NR == FNR { prev[$1] = 1; next }
				($1 in prev)
			' <(printf '%s\n' "$PREV_SUSPECTS") <(printf '%s\n' "$SUSPECTS"))
		fi
		if [ -n "$PERSISTENT" ]; then
			if [ "$LAST_DUMP" -lt 0 ] || [ $((SECONDS - LAST_DUMP)) -ge "$DUMP_GAP" ]; then
				dump_wedge_evidence "$SELECTED" "$PERSISTENT"
				LAST_DUMP=$SECONDS
			fi
		fi
		PREV_SUSPECTS=$SUSPECTS
	fi

	rm -f -- "$ALL" "$SELECTED" "$LIVE" "$ZOMBIE"
	# Interruptible sleep: SIGTERM must stop the watchdog now, not one full
	# interval later.
	sleep "$INTERVAL" &
	wait $! || true
done
