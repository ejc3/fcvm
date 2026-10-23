#!/bin/bash
# A stand-in for `fcvm podman run ...` in the fleet agent tests. It prints what fcvm
# prints on a restore (the one-line `restored` record on stdout), marks its "renderer" up
# by writing $FAKE_FCVM_DIR/port-<published port>.up (a fake renderer on that port answers
# while the file exists), and logs start and exit with nanosecond times to
# $FAKE_FCVM_DIR/events.log so a test can check ordering.
#
#   FAKE_RESTORE_SECS   time before the restored record (default 0.2)
#   FAKE_STOP_SECS      time it takes to stop after SIGTERM (default 0.1)
#   FAKE_DIE_AFTER_SECS exit with status 3 this long after coming up (default: never)
#   FAKE_FAIL_START     exit with status 1 before the restored record; "gpu" names the GPU
#                       in the error, anything else does not
#   FAKE_SCRUB_EXIT     exit status of `gpu scrub <sbdf>` (default 0)
set -u
dir="${FAKE_FCVM_DIR:?}"
log="$dir/events.log"
if [ "${1:-}" = gpu ] && [ "${2:-}" = scrub ]; then
    echo "scrub $3 $$ $(date +%s%N)" >> "$log"
    if [ "${FAKE_SCRUB_EXIT:-0}" != 0 ]; then
        echo "[fake fcvm] scrub of $3 left memory nonzero" >&2
    fi
    exit "${FAKE_SCRUB_EXIT:-0}"
fi
name=""
port=""
gpu=""
while [ $# -gt 0 ]; do
    case "$1" in
        --name) name="$2"; shift ;;
        --publish) port="${2#*:}"; port="${port%%:*}"; shift ;;
        --gpu) gpu="$2"; shift ;;
    esac
    shift
done
echo "start $name $$ $(date +%s%N)" >> "$log"
if [ -n "${FAKE_FAIL_START:-}" ]; then
    if [ "$FAKE_FAIL_START" = gpu ]; then
        echo "[fake fcvm] GPU $gpu did not come back from its reset" >&2
    else
        echo "[fake fcvm] the restore failed" >&2
    fi
    echo "exit $name $$ $(date +%s%N)" >> "$log"
    exit 1
fi

stop() {
    sleep "${FAKE_STOP_SECS:-0.1}"
    rm -f "$dir/port-$port.up"
    echo "exit $name $$ $(date +%s%N)" >> "$log"
    exit 0
}
trap stop TERM INT

echo "[fake fcvm] restoring $name" >&2
sleep "${FAKE_RESTORE_SECS:-0.2}" & wait $!
echo "{\"status\":\"restored\",\"name\":\"$name\",\"ack_ms\":300,\"guest_phases\":{\"gpu_ms\":250}}"
echo "$$" > "$dir/port-$port.up"

if [ -n "${FAKE_DIE_AFTER_SECS:-}" ]; then
    sleep "$FAKE_DIE_AFTER_SECS" & wait $!
    rm -f "$dir/port-$port.up"
    echo "died $name $$ $(date +%s%N)" >> "$log"
    echo "[fake fcvm] the VM died" >&2
    exit 3
fi
while :; do
    sleep 1 & wait $!
done
