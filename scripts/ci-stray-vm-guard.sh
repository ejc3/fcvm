#!/usr/bin/env bash
# Report and reap stray microVM process groups on a self-hosted runner.
#
# The guard deliberately reads only the process/thread identity fields exposed by
# `ps`: TGID, TID, PPID, state, and thread name. In particular it never asks procfs
# for argv/cmdline: reading /proc/<pid>/cmdline can block behind a wedged mm and made
# the diagnostic guard itself hang after the ARM KVM failure in August 2026.
#
# Every thread in a matching task group is reported. A zombie leader is harmless
# only when every sibling thread is also a zombie; a D-state vCPU sibling still owns
# live KVM/MM resources and makes the whole group actionable.
#
# Every reported group also gets per-TID evidence (kernel stack, wchan, status)
# captured BEFORE the kill attempt, and SIGKILL survivors get a second capture
# plus reclaim/compaction diagnostics — see scripts/lib/vm-evidence.sh. The one
# time this class struck (a SIGKILLed firecracker stuck non-zombie in D state),
# the stacks were read by hand and lost with the recycled runner.
#
# Usage: ci-stray-vm-guard.sh <pre|post> [--dry-run]
#
#   --dry-run  report only, kill nothing. The default SIGKILL behavior is intended
#              only for dedicated CI runners.
set -uo pipefail

PHASE="${1:-post}"
DRY_RUN=0
[ "${2:-}" = "--dry-run" ] && DRY_RUN=1

SCAN_TIMEOUT_SECONDS="${FCVM_GUARD_SCAN_TIMEOUT_SECONDS:-10}"
LOG_DIR="${FCVM_TEST_LOG_DIR:-/tmp/fcvm-test-logs}"
if ! mkdir -p "$LOG_DIR" 2>/dev/null; then
	LOG_DIR="/tmp/fcvm-test-logs"
	mkdir -p "$LOG_DIR"
fi

# shellcheck source=scripts/lib/vm-evidence.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib/vm-evidence.sh"

REPORT="$LOG_DIR/stray-vm-guard-${PHASE}.log"
THREADS_BEFORE="$LOG_DIR/stray-vm-threads-${PHASE}-before.tsv"
THREADS_AFTER="$LOG_DIR/stray-vm-threads-${PHASE}-after.tsv"
: >"$REPORT"
: >"$THREADS_BEFORE"
: >"$THREADS_AFTER"
exec > >(tee -a "$REPORT") 2>&1

# The thread scan and group selection live in scripts/lib/vm-evidence.sh
# (vm_scan_all_threads / vm_select_vm_groups), shared with the in-job D-state
# watchdog so both report from the identical snapshot discipline.

TEMPORARY_PREFIX="$LOG_DIR/.stray-vm-guard.$$"
# shellcheck disable=SC2317  # reached via `trap ... EXIT`, not fall-through
cleanup_temporary_files() {
	rm -f -- "${TEMPORARY_PREFIX}".*
}
trap cleanup_temporary_files EXIT

new_temporary_file() {
	mktemp "${TEMPORARY_PREFIX}.XXXXXX"
}

print_thread_table() {
	local table=$1
	printf '%8s %8s %8s %-5s %s\n' TGID TID PPID STATE THREAD
	while IFS=$'\t' read -r tgid tid ppid state thread; do
		[ "$tgid" = "TGID" ] && continue
		printf '%8s %8s %8s %-5s %s\n' "$tgid" "$tid" "$ppid" "$state" "$thread"
	done <"$table"
}

scan_vm_groups() {
	local selected=$1
	local live=$2
	local zombies=$3
	local all
	all=$(new_temporary_file) || return 1
	: >"$live"
	: >"$zombies"
	if ! vm_scan_all_threads "$all" "$SCAN_TIMEOUT_SECONDS" "$TEMPORARY_PREFIX"; then
		# Preserve a syntactically useful artifact even when enumeration failed.
		printf 'TGID\tTID\tPPID\tSTATE\tTHREAD\n' >"$selected"
		return 1
	fi
	vm_select_vm_groups "$all" "$selected" "$live" "$zombies"
}

LIVE_BEFORE=$(new_temporary_file)
ZOMBIE_BEFORE=$(new_temporary_file)
if ! scan_vm_groups "$THREADS_BEFORE" "$LIVE_BEFORE" "$ZOMBIE_BEFORE"; then
	echo "=== stray microVM guard (${PHASE}): scan incomplete ==="
	echo "::warning title=Stray microVM scan incomplete (${PHASE})::Process enumeration timed out or failed; no destructive cleanup attempted. Diagnostics were saved to ${LOG_DIR}."
	[ -n "${GITHUB_STEP_SUMMARY:-}" ] &&
		echo "- stray microVM guard (${PHASE}): scan incomplete; diagnostics saved" >>"$GITHUB_STEP_SUMMARY"
	exit 0
fi

mapfile -t STRAY_TGIDS <"$LIVE_BEFORE"
mapfile -t ZOMBIE_TGIDS <"$ZOMBIE_BEFORE"
COUNT=${#STRAY_TGIDS[@]}
ZOMBIES=${#ZOMBIE_TGIDS[@]}

[ "$ZOMBIES" -gt 0 ] &&
	echo "note (${PHASE}): ignoring ${ZOMBIES} zombie-only process group(s); every thread is already dead"

echo "=== stray microVM guard (${PHASE}): ${COUNT} stray process group(s) ==="

if [ "$COUNT" -eq 0 ]; then
	echo "✓ no live fcvm/firecracker/cloud-hypervisor process groups"
	[ -n "${GITHUB_STEP_SUMMARY:-}" ] &&
		echo "- stray microVM guard (${PHASE}): 0; zombie-only groups ${ZOMBIES}" >>"$GITHUB_STEP_SUMMARY"
	exit 0
fi

print_thread_table "$THREADS_BEFORE"

# Evidence first, kill second: SIGKILL destroys exactly the state (stacks,
# wchan, pending-signal masks) that explains a survivor, and the survivor case
# is the one that recycles the runner before anything else can look.
vm_evidence_group "$THREADS_BEFORE" "pre-kill, ${PHASE}" "${STRAY_TGIDS[@]}"

if [ "$DRY_RUN" -eq 1 ]; then
	echo "::warning title=Stray microVMs (${PHASE}, dry-run)::${COUNT} live process group(s) found; NOT killing them (--dry-run)."
	echo "--dry-run: reporting only, killing nothing"
	[ -n "${GITHUB_STEP_SUMMARY:-}" ] &&
		echo "- stray microVM guard (${PHASE}, dry-run): found ${COUNT}, zombie-only groups ${ZOMBIES}" >>"$GITHUB_STEP_SUMMARY"
	exit 0
fi

echo "::warning title=Stray microVMs (${PHASE})::${COUNT} live process group(s) were present; killing each TGID once. A non-zero post count means this job leaked VMs."

for tgid in "${STRAY_TGIDS[@]}"; do
	# `kill` needs only the numeric TGID and never reads the target mm/cmdline.
	timeout --signal=KILL 2 sudo kill -9 -- "$tgid" 2>/dev/null || true
done
sleep 2

LIVE_AFTER=$(new_temporary_file)
ZOMBIE_AFTER=$(new_temporary_file)
if ! scan_vm_groups "$THREADS_AFTER" "$LIVE_AFTER" "$ZOMBIE_AFTER"; then
	echo "post-kill process/thread scan timed out; survivor count is unknown"
	echo "::warning title=Unkillable microVM scan incomplete (${PHASE})::Post-kill enumeration did not complete; runner replacement is required."
	[ -n "${GITHUB_STEP_SUMMARY:-}" ] &&
		echo "- stray microVM guard (${PHASE}): found ${COUNT}, survivor scan incomplete" >>"$GITHUB_STEP_SUMMARY"
	exit 0
fi

mapfile -t SURVIVOR_TGIDS <"$LIVE_AFTER"
echo "killed ${COUNT} process group(s), still live after SIGKILL: ${#SURVIVOR_TGIDS[@]}"
if [ "${#SURVIVOR_TGIDS[@]}" -gt 0 ]; then
	print_thread_table "$THREADS_AFTER"
	# A survivor is a task the kernel could not reap: its status now shows the
	# pending SIGKILL bit on a non-zombie task, and whatever owns it (the ARM
	# case: kcompactd holding a folio lock under do_swap_page) is visible only
	# while the survivor exists. Capture both before this runner disappears.
	vm_evidence_group "$THREADS_AFTER" "post-SIGKILL survivors, ${PHASE}" "${SURVIVOR_TGIDS[@]}"
	vm_evidence_mm_diagnostics 150
	echo "::warning title=Unkillable microVMs (${PHASE})::${#SURVIVOR_TGIDS[@]} process group(s) survived SIGKILL; D-state means this runner needs replacement."
fi

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
	echo "- stray microVM guard (${PHASE}): found ${COUNT}, unkillable ${#SURVIVOR_TGIDS[@]}" >>"$GITHUB_STEP_SUMMARY"
fi

exit 0
