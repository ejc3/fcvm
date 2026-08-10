#!/usr/bin/env bash
# Sourced evidence-capture helpers for microVM teardown diagnostics.
#
# Why this exists: during concurrent clone teardown on an ARM CI runner, a
# SIGKILLed Firecracker stayed non-zombie in D state and wedged the runner. The
# kernel stacks that named the owner (softleaf_entry_wait_on_locked ->
# migration_entry_wait -> do_swap_page, with kcompactd holding the folio) were
# read once by hand and then lost when the runner was recycled. These helpers
# archive that evidence at the moment it exists, in the caller's report stream.
#
# Same procfs discipline as ci-stray-vm-guard.sh: only /proc/<tid>/{status,
# wchan,stack}, /proc/<pid>/comm, /proc/vmstat, /proc/pressure/memory and dmesg
# are read. Those describe scheduler/signal/MM state owned by the kernel and do
# not fault the target's mm the way /proc/<pid>/cmdline does — but every read
# still runs under `timeout` as a belt. Never add a cmdline/argv read here.
#
# This file defines functions only; it never exits and never writes files.

# Test seam: unit tests point this at a synthetic tree. Production never sets it.
VM_EVIDENCE_PROC_ROOT="${FCVM_PROC_ROOT:-/proc}"
# Stack dumps are capped so one wedged group cannot flood a CI log.
VM_EVIDENCE_STACK_LINES=60
# Seconds between reclaim/compaction stack samples (tests set 0).
VM_EVIDENCE_MM_INTERVAL="${FCVM_MM_SAMPLE_INTERVAL_SECONDS:-1}"

# vm_evidence_tid <tgid> <tid>
#
# One thread's kernel-side state: status (State + SigPnd/ShdPnd prove "SIGKILL
# pending on a non-zombie task"), wchan (the function it sleeps in), and the
# kernel stack (root-only; capped). Unreadable files are reported, not fatal —
# the thread may be reaped between the snapshot and this read.
vm_evidence_tid() {
	local tgid=$1 tid=$2
	local task="${VM_EVIDENCE_PROC_ROOT}/${tgid}/task/${tid}"
	echo "-- tgid=${tgid} tid=${tid}"
	timeout 2 grep -E '^(Name|State|SigPnd|ShdPnd|SigBlk):' "${task}/status" 2>/dev/null ||
		echo "status: <unreadable ${task}/status>"
	local wchan
	wchan=$(timeout 2 cat "${task}/wchan" 2>/dev/null) || true
	echo "wchan: ${wchan:-<unreadable>}"
	local stack
	stack=$(timeout 2 head -n "$VM_EVIDENCE_STACK_LINES" "${task}/stack" 2>/dev/null) || true
	if [ -n "$stack" ]; then
		echo "stack (first ${VM_EVIDENCE_STACK_LINES} lines):"
		printf '%s\n' "$stack"
	else
		echo "stack: <unreadable ${task}/stack (kernel stacks need root)>"
	fi
}

# vm_evidence_group <thread-table.tsv> <label> <tgid>...
#
# Per-TID evidence for every thread of the listed task groups. The TID list
# comes from the already-captured ps snapshot (TGID TID PPID STATE THREAD),
# never from a fresh procfs walk, so a wedged task table cannot stall this loop.
vm_evidence_group() {
	local table=$1 label=$2
	shift 2
	[ "$#" -eq 0 ] && return 0
	declare -A vm_evidence_wanted=()
	local tgid
	for tgid in "$@"; do
		vm_evidence_wanted[$tgid]=1
	done
	echo "=== per-thread evidence (${label}) ==="
	local tid
	while IFS=$'\t' read -r tgid tid _ _ _; do
		[ "$tgid" = "TGID" ] && continue
		[ -n "${vm_evidence_wanted[$tgid]:-}" ] || continue
		vm_evidence_tid "$tgid" "$tid"
	done <"$table"
	echo "=== end per-thread evidence (${label}) ==="
}

# vm_evidence_mm_diagnostics <dmesg-tail-lines>
#
# Reclaim/compaction context for a SIGKILL survivor. A task that ignores
# SIGKILL is waiting uninterruptibly on something another kernel path owns; on
# the ARM wedge that owner was kcompactd, visible only in its stack and the
# compaction counters at that moment. Three samples one interval apart show
# whether the owner is stuck or making progress.
vm_evidence_mm_diagnostics() {
	local dmesg_lines=$1
	echo "=== memory-management diagnostics (SIGKILL survivor present) ==="
	local pids
	pids=$(timeout 2 pgrep '^(kcompactd|kswapd)' 2>/dev/null) || true
	if [ -z "$pids" ]; then
		echo "no kcompactd/kswapd tasks matched by pgrep"
	else
		local sample pid comm stack
		for sample in 1 2 3; do
			[ "$sample" -gt 1 ] && sleep "$VM_EVIDENCE_MM_INTERVAL"
			echo "--- reclaim/compaction stack sample ${sample}/3 at $(date -u '+%Y-%m-%dT%H:%M:%S.%3NZ') ---"
			while IFS= read -r pid; do
				[ -n "$pid" ] || continue
				comm=$(timeout 2 cat "${VM_EVIDENCE_PROC_ROOT}/${pid}/comm" 2>/dev/null) || true
				echo "[${comm:-?}] pid=${pid}"
				stack=$(timeout 2 head -n "$VM_EVIDENCE_STACK_LINES" "${VM_EVIDENCE_PROC_ROOT}/${pid}/stack" 2>/dev/null) || true
				if [ -n "$stack" ]; then
					printf '%s\n' "$stack"
				else
					echo "stack: <unreadable>"
				fi
			done <<<"$pids"
		done
	fi
	echo "--- ${VM_EVIDENCE_PROC_ROOT}/vmstat compaction counters ---"
	timeout 2 grep '^compact_' "${VM_EVIDENCE_PROC_ROOT}/vmstat" 2>/dev/null ||
		echo "<unreadable ${VM_EVIDENCE_PROC_ROOT}/vmstat>"
	echo "--- ${VM_EVIDENCE_PROC_ROOT}/pressure/memory ---"
	timeout 2 cat "${VM_EVIDENCE_PROC_ROOT}/pressure/memory" 2>/dev/null ||
		echo "<unreadable ${VM_EVIDENCE_PROC_ROOT}/pressure/memory>"
	echo "--- dmesg tail (last ${dmesg_lines} lines, unfiltered) ---"
	local dmesg_out
	dmesg_out=$(timeout 5 dmesg 2>/dev/null | tail -n "$dmesg_lines") || true
	if [ -z "$dmesg_out" ]; then
		# kernel.dmesg_restrict may deny the unprivileged read; CI runners have
		# passwordless sudo (the guard's kill path already relies on it).
		dmesg_out=$(timeout 5 sudo dmesg 2>/dev/null | tail -n "$dmesg_lines") || true
	fi
	if [ -n "$dmesg_out" ]; then
		printf '%s\n' "$dmesg_out"
	else
		echo "<dmesg unavailable>"
	fi
	echo "=== end memory-management diagnostics ==="
}
