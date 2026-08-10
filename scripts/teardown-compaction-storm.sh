#!/usr/bin/env bash
# Bounded compaction-storm probe for the SIGKILL-survivor teardown class.
#
# The wild ARM failure had kcompactd holding a folio lock while a SIGKILLed
# firecracker waited on it (softleaf_entry_wait_on_locked -> migration_entry_wait
# -> do_swap_page). scripts/teardown-folio-lock-red.sh freezes that owner
# deterministically with dmsetup; this probe instead recreates the ORIGINAL
# race shape: clones faulting in pages from a memory server while manual
# compaction hammers page migration, and SIGKILL lands mid-fault.
#
#   loop N times (bounded, argument, default 10):
#     spawn a clone, start touching its cold pages, SIGKILL its VMM + fcvm
#     while `echo 1 > /proc/sys/vm/compact_memory` runs in a tight background
#     loop; sample kcompactd/kswapd stacks each iteration; a VMM still
#     non-zombie after the window (FCVM_RED_WINDOW_SECONDS, default 20s) is a
#     reproduction and gets the full evidence dump.
#
# Exit codes:
#   0  probe completed and the host is clean (summary says reproduced or not —
#      this race is probabilistic, so a no-show is not a failure)
#   4  a survivor never reaped even after the storm stopped; host wedged
#   other  harness/setup failure
#
# Root required (compact_memory, kernel stacks).
set -euo pipefail

ITERATIONS="${1:-10}"
FCVM_BIN="${FCVM_BIN:-./target/release/fcvm}"
IMAGE="${FCVM_TEST_IMAGE:-nginx:alpine}"
RED_WINDOW="${FCVM_RED_WINDOW_SECONDS:-20}"
EVIDENCE_DIR="${FCVM_TEARDOWN_EVIDENCE_DIR:-/tmp/fcvm-teardown-evidence}"

RUN_ID="$$-$(date +%s)"
VM_NAME="storm-base-${RUN_ID}"
TAG="storm-${RUN_ID}"

# shellcheck source=scripts/lib/vm-evidence.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib/vm-evidence.sh"

mkdir -p "$EVIDENCE_DIR"
exec > >(tee -a "${EVIDENCE_DIR}/compaction-storm.log") 2>&1

[ "$(id -u)" -eq 0 ] || { echo "FATAL: must run as root"; exit 1; }
[ -x "$FCVM_BIN" ] || { echo "FATAL: fcvm binary not found at $FCVM_BIN (run make build)"; exit 1; }
[ -w /proc/sys/vm/compact_memory ] || { echo "FATAL: /proc/sys/vm/compact_memory not writable"; exit 1; }

proc_state() {
	awk '/^State:/{print $2}' "/proc/$1/status" 2>/dev/null
}

proc_ppid() {
	local stat
	stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
	stat=${stat##*') '}
	read -r _ ppid _ <<<"$stat"
	echo "$ppid"
}

is_descendant_of() {
	local pid=$1 ancestor=$2 hop
	for hop in $(seq 1 32); do
		: "$hop"
		[ "$pid" = "$ancestor" ] && return 0
		[ "$pid" -le 1 ] && return 1
		pid=$(proc_ppid "$pid") || return 1
	done
	return 1
}

find_firecracker_descendant() {
	local root=$1 entry pid comm
	for entry in /proc/[0-9]*; do
		pid=${entry#/proc/}
		comm=$(cat "$entry/comm" 2>/dev/null) || continue
		case $comm in firecracker*) ;; *) continue ;; esac
		if is_descendant_of "$pid" "$root"; then
			echo "$pid"
			return 0
		fi
	done
	return 1
}

wait_exec_ready() {
	local fcvm_pid=$1 deadline=$((SECONDS + 240))
	until timeout 15 "$FCVM_BIN" exec --pid "$fcvm_pid" --vm -- true >/dev/null 2>&1; do
		kill -0 "$fcvm_pid" 2>/dev/null || return 1
		[ "$SECONDS" -lt "$deadline" ] || return 1
		sleep 2
	done
}

sample_kcompactd() {
	local label=$1 pid
	echo "-- kcompactd/kswapd sample (${label}) at $(date -u '+%Y-%m-%dT%H:%M:%S.%3NZ')"
	while IFS= read -r pid; do
		[ -n "$pid" ] || continue
		echo "[$(cat "/proc/${pid}/comm" 2>/dev/null || echo '?')] pid=${pid}"
		timeout 2 head -n 60 "/proc/${pid}/stack" 2>/dev/null || echo "stack: <unreadable>"
	done < <(pgrep '^(kcompactd|kswapd)' || true)
}

dump_survivor_evidence() {
	local pid=$1 label=$2 task
	echo "===== survivor evidence (${label}) ====="
	for task in "/proc/${pid}/task"/*; do
		[ -d "$task" ] || continue
		vm_evidence_tid "$pid" "${task##*/}"
	done
	vm_evidence_mm_diagnostics 150
	echo "===== end survivor evidence (${label}) ====="
}

# Kill and reap one fcvm process, bounded.
reap_fcvm() {
	local pid=$1 deadline
	[ -n "$pid" ] || return 0
	kill -TERM "$pid" 2>/dev/null || true
	deadline=$((SECONDS + 30))
	while kill -0 "$pid" 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do
		sleep 1
	done
	kill -9 "$pid" 2>/dev/null || true
}

STORM_PID=""
BASE_PID=""
SERVE_PID=""
CLONE_PID=""
WEDGED=0

# shellcheck disable=SC2317  # reached via `trap cleanup EXIT`, not fall-through
cleanup() {
	local rc=$?
	set +e
	echo "=== cleanup (script exiting with ${rc}) ==="
	[ -n "$STORM_PID" ] && kill -9 "$STORM_PID" 2>/dev/null
	reap_fcvm "$CLONE_PID"
	reap_fcvm "$SERVE_PID"
	reap_fcvm "$BASE_PID"
	wait 2>/dev/null
	"$FCVM_BIN" snapshots delete "$TAG" >/dev/null 2>&1
	echo "=== cleanup done ==="
	[ "$WEDGED" = 1 ] && exit 4
}
trap cleanup EXIT

echo "=== fcvm teardown compaction-storm probe (${RUN_ID}) ==="
echo "iterations=${ITERATIONS} window=${RED_WINDOW}s image=${IMAGE}"

echo "--- setup: baseline VM, snapshot, memory server"
"$FCVM_BIN" podman run --name "$VM_NAME" --network rootless "$IMAGE" \
	>"${EVIDENCE_DIR}/storm-base-${RUN_ID}.log" 2>&1 &
BASE_PID=$!
wait_exec_ready "$BASE_PID" || { echo "FATAL: baseline never became exec-ready"; exit 1; }
# 64 MiB pattern = fault surface the clones must pull from the server.
timeout 120 "$FCVM_BIN" exec --pid "$BASE_PID" --vm -- \
	sh -c "yes STORMPAGE | head -c 67108864 > /dev/shm/storm && sync" ||
	{ echo "FATAL: could not write fault-surface pattern"; exit 1; }
"$FCVM_BIN" snapshot create --pid "$BASE_PID" --tag "$TAG" ||
	{ echo "FATAL: snapshot create failed"; exit 1; }
"$FCVM_BIN" snapshot serve "$TAG" >"${EVIDENCE_DIR}/storm-serve-${RUN_ID}.log" 2>&1 &
SERVE_PID=$!
DEADLINE=$((SECONDS + 60))
until find /mnt/fcvm-btrfs -maxdepth 2 -name "uffd-${TAG}-${SERVE_PID}.sock" 2>/dev/null | grep -q .; do
	kill -0 "$SERVE_PID" 2>/dev/null || { echo "FATAL: serve exited; see its log"; exit 1; }
	[ "$SECONDS" -lt "$DEADLINE" ] || { echo "FATAL: serve socket never appeared"; exit 1; }
	sleep 1
done
echo "serve PID ${SERVE_PID} ready"

echo "--- storm: manual compaction in a tight loop"
(
	while :; do
		echo 1 >/proc/sys/vm/compact_memory 2>/dev/null || true
		sleep 0.2
	done
) &
STORM_PID=$!

REPRODUCED=0
for i in $(seq 1 "$ITERATIONS"); do
	echo "--- iteration ${i}/${ITERATIONS}"
	sample_kcompactd "iteration ${i}"
	"$FCVM_BIN" snapshot run --pid "$SERVE_PID" --name "storm-clone-${RUN_ID}-${i}" \
		>"${EVIDENCE_DIR}/storm-clone-${RUN_ID}-${i}.log" 2>&1 &
	CLONE_PID=$!
	if ! wait_exec_ready "$CLONE_PID"; then
		echo "WARN: clone ${i} never became exec-ready; skipping iteration"
		reap_fcvm "$CLONE_PID"
		CLONE_PID=""
		continue
	fi
	FC_PID=$(find_firecracker_descendant "$CLONE_PID") ||
		{ echo "WARN: no VMM under clone ${i}"; reap_fcvm "$CLONE_PID"; CLONE_PID=""; continue; }

	# Touch the cold pattern in the background so faults are in flight, give
	# it a moment to start pulling pages, then kill mid-fault.
	timeout 60 "$FCVM_BIN" exec --pid "$CLONE_PID" --vm -- \
		sh -c "md5sum /dev/shm/storm" >/dev/null 2>&1 &
	TOUCH_PID=$!
	sleep 1

	kill -9 "$FC_PID" 2>/dev/null || true
	kill -TERM "$CLONE_PID" 2>/dev/null || true
	KILLED_AT=$SECONDS
	SURVIVED=1
	while [ $((SECONDS - KILLED_AT)) -lt "$RED_WINDOW" ]; do
		STATE=$(proc_state "$FC_PID")
		if [ -z "$STATE" ] || [ "$STATE" = "Z" ]; then
			SURVIVED=0
			break
		fi
		sleep 0.5
	done
	kill -9 "$TOUCH_PID" 2>/dev/null || true
	if [ "$SURVIVED" = 1 ]; then
		echo "REPRODUCED at iteration ${i}: VMM ${FC_PID} state=$(proc_state "$FC_PID") ${RED_WINDOW}s after SIGKILL"
		awk '/^(SigPnd|ShdPnd):/{print}' "/proc/${FC_PID}/status" 2>/dev/null || true
		dump_survivor_evidence "$FC_PID" "compaction-storm iteration ${i}"
		REPRODUCED=1
		# Stop the storm and confirm the survivor eventually reaps — leaving a
		# wedge behind is the one outcome this probe must never produce.
		kill -9 "$STORM_PID" 2>/dev/null || true
		STORM_PID=""
		DEADLINE=$((SECONDS + 60))
		while [ "$SECONDS" -lt "$DEADLINE" ]; do
			STATE=$(proc_state "$FC_PID")
			{ [ -z "$STATE" ] || [ "$STATE" = "Z" ]; } && break
			kill -9 "$FC_PID" 2>/dev/null
			sleep 1
		done
		STATE=$(proc_state "$FC_PID")
		if [ -n "$STATE" ] && [ "$STATE" != "Z" ]; then
			echo "FATAL: survivor ${FC_PID} still ${STATE} 60s after the storm stopped"
			dump_survivor_evidence "$FC_PID" "post-storm, unrecovered"
			WEDGED=1
		fi
		reap_fcvm "$CLONE_PID"
		CLONE_PID=""
		break
	fi
	echo "iteration ${i}: VMM reaped within the window"
	reap_fcvm "$CLONE_PID"
	CLONE_PID=""
done

echo "=== RESULT: reproduced=$([ "$REPRODUCED" = 1 ] && echo yes || echo no) after ${ITERATIONS} bounded iteration(s) ==="
exit 0
