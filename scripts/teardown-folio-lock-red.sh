#!/usr/bin/env bash
# Deterministic reproduction (RED) of the SIGKILL-survivor teardown class.
#
# The class: during concurrent clone teardown on an ARM runner, a SIGKILLed
# firecracker stayed non-zombie in D state (stacks:
# softleaf_entry_wait_on_locked -> migration_entry_wait -> do_swap_page) and
# wedged the runner. SIGKILL cannot reap a task waiting uninterruptibly on a
# folio another kernel path holds locked. That owner was transient in the wild;
# this harness makes it stand still by suspending the block device under the
# folio's backing store:
#
#   1. swapfile on a dm-linear device (so it can be suspended), swapon at
#      priority 100 so it takes the traffic even when the host already swaps
#   2. one fcvm VM; its firecracker moved into a cgroup-v2 group with
#      memory.max far below guest RAM, then the guest dirties enough anonymous
#      memory that the host swaps guest pages out to the dm device
#   3. dmsetup suspend — all swap I/O now blocks indefinitely
#   4. an exec in the guest touches the swapped pages: vCPU threads enter
#      do_swap_page and wait on folios locked under the suspended I/O
#   5. SIGKILL the firecracker PID
#   6. REPRODUCTION = the PID stays NON-zombie for the whole window (default
#      20s, FCVM_RED_WINDOW_SECONDS) with the SIGKILL bit pending in its
#      status — the exact signature the wild failure showed. Evidence
#      (per-TID stack/wchan/status + reclaim/compaction diagnostics) is
#      captured while the survivor exists.
#   7. RECOVERY always runs from the EXIT trap: dmsetup resume lets the I/O
#      complete, the pending SIGKILL lands, and the harness asserts the
#      process reaps promptly, then removes swap/dm/loop/cgroup — the box is
#      never left wedged.
#
# Exit codes:
#   0  reproduced AND recovered AND cleaned up
#   3  the class did not manifest here (SIGKILL reaped the VMM inside the
#      window, or no uninterruptible fault could be staged) — meaningful
#      negative result for this kernel
#   4  the survivor did NOT reap after resume; the host needs attention
#   other  harness/setup failure
#
# Root required (dmsetup, swapon, cgroup writes). Never run on a box whose
# existing swap you care about: it adds and removes its own device only, but
# the memory pressure it creates is real.
set -euo pipefail

FCVM_BIN="${FCVM_BIN:-./target/release/fcvm}"
IMAGE="${FCVM_TEST_IMAGE:-nginx:alpine}"
RED_WINDOW="${FCVM_RED_WINDOW_SECONDS:-20}"
EVIDENCE_DIR="${FCVM_TEARDOWN_EVIDENCE_DIR:-/tmp/fcvm-teardown-evidence}"
WORK_DIR="${FCVM_TEARDOWN_WORK_DIR:-/var/tmp}"
SWAP_SIZE_MIB=2048
CGROUP_LIMIT="${FCVM_TEARDOWN_CGROUP_LIMIT:-256M}"
DIRTY_BYTES=400000000 # ~400 MB dirtied in-guest, >> the 256M cgroup limit

RUN_ID="$$-$(date +%s)"
DM_NAME="fcvm-teardown-red-${RUN_ID}"
SWAPFILE="${WORK_DIR}/fcvm-teardown-red-${RUN_ID}.swap"
CG="/sys/fs/cgroup/fcvm-teardown-red-${RUN_ID}"
VM_NAME="teardown-red-${RUN_ID}"

# shellcheck source=scripts/lib/vm-evidence.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib/vm-evidence.sh"

mkdir -p "$EVIDENCE_DIR"
exec > >(tee -a "${EVIDENCE_DIR}/folio-lock-red.log") 2>&1

[ "$(id -u)" -eq 0 ] || { echo "FATAL: must run as root"; exit 1; }
for tool in dmsetup losetup mkswap swapon blockdev; do
	command -v "$tool" >/dev/null || { echo "FATAL: $tool not installed"; exit 1; }
done
[ -x "$FCVM_BIN" ] || { echo "FATAL: fcvm binary not found at $FCVM_BIN (run make build)"; exit 1; }
grep -qw memory /sys/fs/cgroup/cgroup.controllers 2>/dev/null ||
	{ echo "FATAL: cgroup v2 memory controller unavailable"; exit 1; }

proc_state() {
	# Empty output = process gone.
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

SUSPENDED=0
SWAP_ON=0
WEDGED=0
FC_PID=""
FCVM_PID=""
TOUCH_PID=""
LOOPDEV=""

# shellcheck disable=SC2317  # reached via `trap cleanup EXIT`, not fall-through
cleanup() {
	local rc=$? deadline state
	set +e
	echo "=== cleanup (script exiting with ${rc}) ==="

	# Order matters: resume FIRST. Every later step (swapoff, dmsetup remove)
	# performs I/O that would hang forever against a suspended device.
	if [ "$SUSPENDED" = 1 ]; then
		dmsetup resume "$DM_NAME" && SUSPENDED=0
		echo "dm device resumed"
	fi

	# With I/O flowing again the pending SIGKILL must land. This is the
	# recovery half of the reproduction: a survivor here means the host is
	# genuinely wedged and a human (or runner replacement) is needed.
	if [ -n "$FC_PID" ]; then
		deadline=$((SECONDS + 30))
		while [ "$SECONDS" -lt "$deadline" ]; do
			state=$(proc_state "$FC_PID")
			[ -z "$state" ] || [ "$state" = "Z" ] && break
			kill -9 "$FC_PID" 2>/dev/null
			sleep 1
		done
		state=$(proc_state "$FC_PID")
		if [ -n "$state" ] && [ "$state" != "Z" ]; then
			echo "FATAL: firecracker ${FC_PID} still ${state} 30s after resume — host wedged"
			dump_survivor_evidence "$FC_PID" "post-resume, unrecovered"
			WEDGED=1
		else
			echo "firecracker ${FC_PID} reaped after resume"
		fi
	fi

	[ -n "$TOUCH_PID" ] && kill -9 "$TOUCH_PID" 2>/dev/null
	if [ -n "$FCVM_PID" ] && kill -0 "$FCVM_PID" 2>/dev/null; then
		kill -TERM "$FCVM_PID" 2>/dev/null
		deadline=$((SECONDS + 30))
		while kill -0 "$FCVM_PID" 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do
			sleep 1
		done
		kill -9 "$FCVM_PID" 2>/dev/null
	fi
	wait 2>/dev/null

	[ "$SWAP_ON" = 1 ] && { timeout 60 swapoff "/dev/mapper/${DM_NAME}" || echo "WARN: swapoff failed"; }
	if dmsetup info "$DM_NAME" >/dev/null 2>&1; then
		timeout 30 dmsetup remove "$DM_NAME" ||
			{ sleep 2; timeout 30 dmsetup remove "$DM_NAME" || echo "WARN: dmsetup remove failed"; }
	fi
	[ -n "$LOOPDEV" ] && { losetup -d "$LOOPDEV" 2>/dev/null || echo "WARN: losetup -d failed"; }
	rm -f "$SWAPFILE"
	[ -d "$CG" ] && { rmdir "$CG" 2>/dev/null || echo "WARN: cgroup ${CG} not removable (procs left?)"; }
	echo "=== cleanup done ==="
	[ "$WEDGED" = 1 ] && exit 4
}
trap cleanup EXIT

echo "=== fcvm teardown folio-lock RED harness (${RUN_ID}) ==="
echo "window=${RED_WINDOW}s cgroup-limit=${CGROUP_LIMIT} image=${IMAGE}"

echo "--- step 1: swapfile on a suspendable dm-linear device"
dd if=/dev/zero of="$SWAPFILE" bs=1M count="$SWAP_SIZE_MIB" status=none
chmod 600 "$SWAPFILE"
LOOPDEV=$(losetup --find --show "$SWAPFILE")
dmsetup create "$DM_NAME" --table "0 $(blockdev --getsz "$LOOPDEV") linear ${LOOPDEV} 0"
mkswap "/dev/mapper/${DM_NAME}" >/dev/null
swapon --priority 100 "/dev/mapper/${DM_NAME}"
SWAP_ON=1
echo "swap device /dev/mapper/${DM_NAME} on ${LOOPDEV} (${SWAP_SIZE_MIB} MiB, prio 100)"

echo "--- step 2: VM under a ${CGROUP_LIMIT} memory ceiling"
mkdir "$CG"
[ -f "${CG}/memory.max" ] || { echo "FATAL: ${CG} has no memory controller"; exit 1; }
"$FCVM_BIN" podman run --name "$VM_NAME" --network rootless --mem 1024 "$IMAGE" \
	>"${EVIDENCE_DIR}/vm-${RUN_ID}.log" 2>&1 &
FCVM_PID=$!
echo "fcvm PID ${FCVM_PID}; waiting for guest exec"
DEADLINE=$((SECONDS + 240))
until timeout 15 "$FCVM_BIN" exec --pid "$FCVM_PID" --vm -- true >/dev/null 2>&1; do
	kill -0 "$FCVM_PID" 2>/dev/null || { echo "FATAL: fcvm exited during boot; see vm-${RUN_ID}.log"; exit 1; }
	[ "$SECONDS" -lt "$DEADLINE" ] || { echo "FATAL: guest never became exec-ready"; exit 1; }
	sleep 2
done
FC_PID=$(find_firecracker_descendant "$FCVM_PID") ||
	{ echo "FATAL: no firecracker under fcvm ${FCVM_PID}"; exit 1; }
echo "firecracker PID ${FC_PID}"
# Constrain only the VMM (guest RAM is its anonymous memory). Boot ran
# unconstrained so the pressure starts exactly when the experiment does.
echo "$FC_PID" >"${CG}/cgroup.procs"
echo "$CGROUP_LIMIT" >"${CG}/memory.max"

echo "--- step 3: dirty ${DIRTY_BYTES} bytes in-guest so host pages swap out"
timeout 300 "$FCVM_BIN" exec --pid "$FCVM_PID" --vm -- \
	sh -c "yes SWAPME | head -c ${DIRTY_BYTES} > /dev/shm/swapme && sync" ||
	{ echo "FATAL: could not dirty guest memory"; exit 1; }
DEADLINE=$((SECONDS + 120))
SWAPPED=0
while [ "$SECONDS" -lt "$DEADLINE" ]; do
	SWAPPED=$(cat "${CG}/memory.swap.current" 2>/dev/null || echo 0)
	[ "$SWAPPED" -ge 104857600 ] && break
	sleep 1
done
if [ "$SWAPPED" -lt 104857600 ]; then
	echo "FATAL: only $((SWAPPED / 1048576)) MiB swapped; cannot stage the class here"
	exit 3
fi
echo "cgroup swapped $((SWAPPED / 1048576)) MiB; device usage: $(awk -v d="/dev/mapper/${DM_NAME}" '$1==d{print $4" KiB"}' /proc/swaps)"

echo "--- step 4: suspend the swap device; touch the swapped pages"
dmsetup suspend "$DM_NAME"
SUSPENDED=1
timeout 180 "$FCVM_BIN" exec --pid "$FCVM_PID" --vm -- \
	sh -c "md5sum /dev/shm/swapme" >"${EVIDENCE_DIR}/touch-${RUN_ID}.log" 2>&1 &
TOUCH_PID=$!
DEADLINE=$((SECONDS + 60))
PARKED=""
while [ -z "$PARKED" ] && [ "$SECONDS" -lt "$DEADLINE" ]; do
	for task in "/proc/${FC_PID}/task"/*; do
		[ -d "$task" ] || continue
		if [ "$(awk '/^State:/{print $2}' "${task}/status" 2>/dev/null)" = "D" ]; then
			PARKED="${task##*/}"
			echo "task ${PARKED} uninterruptible (wchan: $(cat "${task}/wchan" 2>/dev/null))"
			break
		fi
	done
	[ -n "$PARKED" ] || sleep 0.5
done
if [ -z "$PARKED" ]; then
	echo "FATAL: no vCPU entered D state under suspended swap; nothing to reproduce against"
	exit 3
fi

echo "--- step 5: SIGKILL firecracker ${FC_PID} while the fault is wedged"
kill -9 "$FC_PID"
KILLED_AT=$SECONDS

echo "--- step 6: watch the window (${RED_WINDOW}s): reproduced = still non-zombie at the end"
REPRODUCED=1
while [ $((SECONDS - KILLED_AT)) -lt "$RED_WINDOW" ]; do
	STATE=$(proc_state "$FC_PID")
	if [ -z "$STATE" ] || [ "$STATE" = "Z" ]; then
		echo "firecracker reaped ${STATE:+(zombie) }after $((SECONDS - KILLED_AT))s — class did NOT manifest"
		REPRODUCED=0
		break
	fi
	sleep 1
done

if [ "$REPRODUCED" = 1 ]; then
	STATE=$(proc_state "$FC_PID")
	PENDING=$(awk '/^(SigPnd|ShdPnd):/{print}' "/proc/${FC_PID}/status" 2>/dev/null)
	echo "REPRODUCED: firecracker ${FC_PID} state=${STATE} ${RED_WINDOW}s after SIGKILL"
	printf '%s\n' "$PENDING"
	# SIGKILL is signal 9 -> bit 0x100 of the pending masks.
	KILL_PENDING=0
	for mask in $(printf '%s\n' "$PENDING" | awk '{print $2}'); do
		[ $((16#$mask & 16#100)) -ne 0 ] && KILL_PENDING=1
	done
	if [ "$KILL_PENDING" != 1 ]; then
		echo "WARN: survivor exists but no pending SIGKILL bit found — inspect the status dump"
	fi
	dump_survivor_evidence "$FC_PID" "SIGKILL survivor, ${RED_WINDOW}s window"
	echo "=== RESULT: reproduced (recovery follows in cleanup) ==="
	exit 0
fi

echo "=== RESULT: not reproduced on this kernel/host ==="
exit 3
