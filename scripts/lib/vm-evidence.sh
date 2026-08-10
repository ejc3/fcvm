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

# Linux comm is limited to 15 bytes. `cloud-hypervisor` is therefore observed as
# `cloud-hypervis`; the prefix intentionally covers both that and the full spelling.
VM_SCAN_RE='^(firecracker|cloud-hypervis|fcvm)'

# vm_scan_all_threads <destination> <timeout_seconds> <tmp_prefix>
#
# One process-table snapshot behind a hard deadline, normalized to a
# tab-separated TGID/TID/PPID/STATE/THREAD artifact. We do not wait for a
# timed-out reader after SIGKILL: if the kernel has already parked it in D
# state, waiting would recreate the guard hang this scan exists to diagnose.
# Diagnostics go to stdout; callers that must stay silent capture them.
vm_scan_all_threads() {
	local destination=$1 scan_timeout=$2 tmp_prefix=$3
	local raw scan_stderr scan_pid deadline rc line
	raw=$(mktemp "${tmp_prefix}.XXXXXX") || return 1
	scan_stderr=$(mktemp "${tmp_prefix}.XXXXXX") || return 1
	printf 'TGID\tTID\tPPID\tSTATE\tTHREAD\n' >"$destination"

	# Do not let the scanner inherit the caller's stdout/stderr pipes. If ps
	# wedges in D state, SIGKILL remains pending and every inherited pipe
	# writer stays open, making a workflow caller wait forever for EOF even
	# after the calling script exits.
	ps -eL -o pid= -o lwp= -o ppid= -o state= -o comm= >"$raw" 2>"$scan_stderr" &
	scan_pid=$!
	deadline=$((SECONDS + scan_timeout))
	while kill -0 "$scan_pid" 2>/dev/null; do
		if [ "$SECONDS" -ge "$deadline" ]; then
			kill -9 "$scan_pid" 2>/dev/null || true
			disown "$scan_pid" 2>/dev/null || true
			while IFS= read -r line || [ -n "$line" ]; do
				printf 'process/thread scanner stderr: %s\n' "$line"
			done <"$scan_stderr"
			echo "process/thread scan timed out after ${scan_timeout}s"
			return 124
		fi
		sleep 0.05
	done

	wait "$scan_pid"
	rc=$?
	while IFS= read -r line || [ -n "$line" ]; do
		printf 'process/thread scanner stderr: %s\n' "$line"
	done <"$scan_stderr"
	if [ "$rc" -ne 0 ]; then
		echo "process/thread scan failed with status ${rc}"
		return "$rc"
	fi

	# Normalize the variable-width comm column to a tab-separated artifact. Thread
	# names can contain spaces, so fields 5..NF are joined back together.
	awk '
		BEGIN { OFS = "\t" }
		NF >= 5 {
			thread = $5
			for (i = 6; i <= NF; i++)
				thread = thread " " $i
			print $1, $2, $3, $4, thread
		}
	' "$raw" >>"$destination"
}

# vm_select_vm_groups <all_threads> <selected_threads> <live_groups> <zombie_groups>
#
# Select complete task groups whose leader name is a VM process. Outputs one TGID
# per live group and one per zombie-only group; the thread artifact always contains
# every sibling, including the zombie leader itself.
vm_select_vm_groups() {
	local all_threads=$1
	local selected_threads=$2
	local live_groups=$3
	local zombie_groups=$4

	awk -F '\t' -v re="$VM_SCAN_RE" \
		-v selected="$selected_threads" \
		-v live_out="$live_groups" \
		-v zombie_out="$zombie_groups" '
		BEGIN { OFS = "\t" }
		NR == 1 { header = $0; next }
		{
			rows[++count] = $0
			tgid[count] = $1
			if ($1 == $2 && $5 ~ re)
				target[$1] = 1
			if ($4 !~ /^Z/)
				live[$1] = 1
		}
		END {
			print header > selected
			for (i = 1; i <= count; i++)
				if (tgid[i] in target)
					print rows[i] >> selected
			for (id in target) {
				if (live[id])
					print id > live_out
				else
					print id > zombie_out
			}
		}
	' "$all_threads"
}

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
