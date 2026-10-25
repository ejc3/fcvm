#!/usr/bin/env bash
# Read root podman's image layers once, early in a host-root job, so the suite does not.
#
# A runner's root volume is an EBS volume restored from the AMI's snapshot, and EBS
# fetches a snapshot block from S3 the first time it is read. The x64 runner AMI carries
# root-store image layers (localhost/pjdfstest at 434 MB, nginx:alpine, debian and others,
# 778 MB of layer data), and the localhost image tests build on them. In a cold suite run
# the pjdfstest export is the first read of its layers: it reads them at about 5 MB/s
# (10 ms per read, the root volume 90 to 98% busy) while holding the layer store's lock,
# and every other root podman call waits behind it. Measured on Host-Root-x64 jobs
# 2026-10-22 to 10-24: that export took 107 s at the median (108 jobs; 3.9 s on arm64),
# every other export ended within 2 s of it, and tests with a 120 s budget failed their
# first try. Reading the layers here, while the job builds, moves the wait out of the suite.
#
# Usage:
#   ci-warm-root-container-store.sh start   # detach a reader, record its pid in $RUNNER_TEMP
#   ci-warm-root-container-store.sh wait    # wait (bounded) for it and print what it read
#   ci-warm-root-container-store.sh run DIR...  # read every regular file under DIR (foreground)
#
# Waiting never fails the job: the read only moves latency. A reader that did not finish,
# could not start, exited nonzero or ended without reporting its status says so in the
# step log and as a warning annotation.
set -uo pipefail

STATE_DIR="${RUNNER_TEMP:-/tmp}"
LOG="$STATE_DIR/fcvm-store-warm.log"
PID_FILE="$STATE_DIR/fcvm-store-warm.pid"
# The reader writes its exit status here as its last act. `wait` runs in a later step, so
# the reader is not its child and the shell's `wait` cannot collect the status.
STATUS_FILE="$STATE_DIR/fcvm-store-warm.status"
WAIT_SECONDS="${FCVM_STORE_WARM_WAIT_SECONDS:-600}"
PARALLEL="${FCVM_STORE_WARM_PARALLEL:-16}"

# The directories containers/storage owns under the graphroot for this driver. The
# graphroot itself can be shared with other data (fcvm's own data dir on some hosts),
# so it is never read whole.
store_dirs() {
	local info root driver
	info=$(sudo podman info --format '{{.Store.GraphRoot}} {{.Store.GraphDriverName}}') || return 1
	read -r root driver <<<"$info"
	[ -n "$root" ] && [ -n "$driver" ] || return 1
	for d in "$root/$driver" "$root/$driver-images" "$root/$driver-layers"; do
		sudo test -d "$d" && printf '%s\n' "$d"
	done
	return 0
}

run() {
	local began sizes rc elapsed
	began=$(date +%s.%N)
	sizes=$(sudo find "$@" -xdev -type f -printf '%s\n')
	rc=$?
	# The status is reported, not acted on: a failed read only leaves latency in the suite.
	sudo find "$@" -xdev -type f -print0 | sudo xargs -0 -r -P "$PARALLEL" -n 64 cat >/dev/null
	[ "$rc" -ne 0 ] || rc=$?
	elapsed=$(awk -v a="$began" -v b="$(date +%s.%N)" 'BEGIN {printf "%.1f", b - a}')
	echo "$sizes" | awk -v dirs="$*" -v s="$elapsed" -v rc="$rc" \
		'NF {n++; b += $1} END {printf "read %d files, %d MiB under %s in %s s (exit %s)\n", n, b / 1048576, dirs, s, rc}'
	echo "$rc" >"$STATUS_FILE"
}

case "${1:-}" in
start)
	if ! dirs=$(store_dirs) || [ -z "$dirs" ]; then
		echo "::warning::container store warm-up not started: podman did not name a graphroot with image layers"
		exit 0
	fi
	echo "warming: $(echo "$dirs" | tr '\n' ' ')"
	rm -f "$STATUS_FILE"
	# shellcheck disable=SC2086  # one directory per line, none contain spaces
	setsid nohup "$0" run $dirs </dev/null >"$LOG" 2>&1 &
	echo $! >"$PID_FILE"
	;;
wait)
	if [ ! -s "$PID_FILE" ]; then
		echo "::warning::container store warm-up was not started; the suite reads cold image layers"
		exit 0
	fi
	pid=$(cat "$PID_FILE")
	waited=0
	while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt "$WAIT_SECONDS" ]; do
		sleep 1
		waited=$((waited + 1))
	done
	echo "waited ${waited}s for the warm-up"
	cat "$LOG" 2>/dev/null || echo "::warning::container store warm-up wrote no log"
	if kill -0 "$pid" 2>/dev/null; then
		echo "::warning::container store warm-up still reading after ${WAIT_SECONDS}s of waiting; continuing"
	elif ! status=$(cat "$STATUS_FILE" 2>/dev/null) || [ -z "$status" ]; then
		echo "::warning::container store warm-up reader ended without reporting a status; it was killed or failed before its summary"
	elif [ "$status" != 0 ]; then
		echo "::warning::container store warm-up reader exited $status; the suite may read cold image layers (log above)"
	fi
	;;
run)
	shift
	run "$@"
	;;
*)
	echo "usage: $0 start|wait|run DIR..." >&2
	exit 2
	;;
esac
