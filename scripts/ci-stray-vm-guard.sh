#!/usr/bin/env bash
# Report and reap stray microVM processes on a self-hosted runner.
#
# Self-hosted runners are reused forever, so anything a job leaves behind is inherited by
# every later job on that box. In August 2026 two ARM runners wedged for 21 hours with ~498
# live `firecracker` processes (load average 523, 103 tasks in D-state, 420 zombies) that had
# accumulated silently across runs — nothing in any job log ever mentioned them.
#
# This is defense in depth, NOT the fix: the leak itself is fixed by the kernel-enforced
# PR_SET_PDEATHSIG chain (scripts/root-test-runner.sh, src/utils.rs::install_namespace_pre_exec).
# It also acts only at job BOUNDARIES, so it cannot stop a host that wedges mid-job — that
# remediation belongs on the AWS side (terminate and replace the instance).
# Run this at the start of a job (what the previous job left) and at the end (what this job
# left). Either way the count lands in the job log and, when non-zero, in a GitHub annotation.
#
# Usage: ci-stray-vm-guard.sh <pre|post> [--dry-run]
#
#   --dry-run  report only, kill nothing. The default mode SIGKILLs every match, which is
#              correct on a dedicated CI runner and destructive anywhere else — use this to
#              exercise the script on a shared dev box, where the matches are someone's
#              running VMs, not garbage.
set -uo pipefail

PHASE="${1:-post}"
DRY_RUN=0
[ "${2:-}" = "--dry-run" ] && DRY_RUN=1

# `comm` is the executable basename truncated to 15 chars, so the content-addressed
# `firecracker-default-<sha>.bin` shows up as `firecracker-def`. Matching on comm (not the
# full argv) means this can never match a grep/ps/pgrep of its own patterns.
STRAY_RE='^(firecracker|cloud-hypervisor|fcvm)'

# Zombies are EXCLUDED ($4 is stat; Z may carry modifiers like `Z+`). A zombie is already
# dead — it holds no memory, no KVM fd and no vCPU thread, only a process-table slot until
# its parent reaps it. It is therefore not a leaked microVM, and more importantly it can
# never be killed: SIGKILL to a zombie is a no-op, so counting them would put them in
# SURVIVORS forever and fire the "survived SIGKILL, this runner needs a reboot" annotation
# on every run. The 2026-08-06 incident had 420 zombies alongside the real leak, so this is
# the difference between a signal and a permanent false alarm.
list_strays() {
	ps -eo pid=,ppid=,etimes=,stat=,comm=,args= |
		awk -v re="$STRAY_RE" '$5 ~ re && $4 !~ /^Z/ { print }'
}

# Counted and reported, never killed: a high zombie count is a symptom worth seeing (it means
# something is not reaping its children) but it is not actionable by this script.
count_zombies() {
	ps -eo stat=,comm= | awk -v re="$STRAY_RE" '$2 ~ re && $1 ~ /^Z/' | wc -l
}

mapfile -t STRAYS < <(list_strays)
COUNT=${#STRAYS[@]}
ZOMBIES=$(count_zombies)
[ "$ZOMBIES" -gt 0 ] && echo "note: ${ZOMBIES} unreaped ${PHASE} zombie(s) ignored (already dead, hold no resources)"

echo "=== stray microVM guard (${PHASE}): ${COUNT} stray process(es) ==="

if [ "$COUNT" -eq 0 ]; then
	echo "✓ no stray fcvm/firecracker/cloud-hypervisor processes"
	[ -n "${GITHUB_STEP_SUMMARY:-}" ] &&
		echo "- stray microVM guard (${PHASE}): 0" >>"$GITHUB_STEP_SUMMARY"
	exit 0
fi

printf '%8s %8s %9s %-5s %s\n' PID PPID ELAPSED STAT COMMAND
for row in "${STRAYS[@]}"; do
	# shellcheck disable=SC2086 # deliberate word splitting into positional fields
	set -- $row
	printf '%8s %8s %8ss %-5s %s\n' "$1" "$2" "$3" "$4" "${*:5:12}"
done

# A GitHub annotation surfaces on the run page, not just buried in the step log.
echo "::warning title=Stray microVMs (${PHASE})::${COUNT} fcvm/firecracker process(es) were alive on this runner; killing them. A non-zero 'post' count means this job leaked VMs."

if [ "$DRY_RUN" -eq 1 ]; then
	echo "--dry-run: reporting only, killing nothing"
	[ -n "${GITHUB_STEP_SUMMARY:-}" ] &&
		echo "- stray microVM guard (${PHASE}, dry-run): found ${COUNT}, zombies ${ZOMBIES}" >>"$GITHUB_STEP_SUMMARY"
	exit 0
fi

for row in "${STRAYS[@]}"; do
	# shellcheck disable=SC2086
	set -- $row
	sudo kill -9 "$1" 2>/dev/null || true
done
sleep 2

mapfile -t SURVIVORS < <(list_strays)
echo "killed ${COUNT}, still present after SIGKILL: ${#SURVIVORS[@]}"
if [ "${#SURVIVORS[@]}" -gt 0 ]; then
	# SIGKILL is not delivered to a task blocked in uninterruptible (D) state — that is the
	# shape of the 21-hour wedge, and it needs a reboot, not another kill.
	printf '%s\n' "${SURVIVORS[@]}"
	echo "::warning title=Unkillable microVMs (${PHASE})::${#SURVIVORS[@]} process(es) survived SIGKILL (likely D-state); this runner needs a reboot."
fi

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
	echo "- stray microVM guard (${PHASE}): found ${COUNT}, unkillable ${#SURVIVORS[@]}" >>"$GITHUB_STEP_SUMMARY"
fi

exit 0
