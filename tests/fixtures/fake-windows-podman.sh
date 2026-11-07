#!/bin/bash
# A stand-in for `podman run ... windows-bridge logon|serve` in the Windows mode agent tests.
# `logon` writes the desktop reference where DESKTOP_REF_OUT says (through the state volume)
# and exits 0. `serve` marks its bridge up by writing $FAKE_FCVM_DIR/bridge-<published HTTP
# port>.up with the time it came up (a fake bridge on that port answers while the file
# exists) after FAKE_BRIDGE_SECS (default 0.2), and runs until SIGTERM. Events and
# arguments are logged as the fake fcvm logs them.
set -u
dir="${FAKE_FCVM_DIR:?}"
log="$dir/events.log"
[ "${1:-}" = run ] || exit 2
argv=("$@")
mode="${*: -1}"
name="" port="" state="" ref_out=""
while [ $# -gt 0 ]; do
    case "$1" in
        --name) name="$2"; shift ;;
        --publish) [ -n "$port" ] || { p="${2%:*}"; port="${p##*:}"; }; shift ;;
        --volume) case "$2" in *:/run/windows/state) state="${2%:/run/windows/state}" ;; esac; shift ;;
        --env) case "$2" in DESKTOP_REF_OUT=*) ref_out="${2#DESKTOP_REF_OUT=}" ;; esac; shift ;;
    esac
    shift
done
echo "podman_$mode $name $$ $(date +%s%N)" >> "$log"
printf '%s\n' "${argv[@]}" > "$dir/args-$name"
if [ "$mode" = logon ]; then
    sleep 0.1
    printf 'P6\n' > "$state/$(basename "$ref_out")"
    exit 0
fi
stop() {
    rm -f "$dir/bridge-$port.up"
    echo "exit $name $$ $(date +%s%N)" >> "$log"
    exit 0
}
trap stop TERM INT
sleep "${FAKE_BRIDGE_SECS:-0.2}" & wait $!
date +%s%3N > "$dir/bridge-$port.up"
while true; do sleep 1 & wait $!; done
