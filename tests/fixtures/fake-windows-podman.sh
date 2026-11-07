#!/bin/bash
# A stand-in for podman in the Windows mode agent tests, modelling a container apart from the
# `podman run` client, as conmon keeps it: killing the client does not stop the container.
#
#   run ... windows-bridge logon   writes the desktop reference where DESKTOP_REF_OUT says
#                                  (through the state volume) and exits FAKE_LOGON_EXIT (0)
#   run ... windows-bridge serve   starts the container, which marks its bridge up by writing
#                                  $FAKE_FCVM_DIR/bridge-<published HTTP port>.up with the time
#                                  it came up after FAKE_BRIDGE_SECS (default 0.2); a fake bridge
#                                  on that port answers while the file exists. The client
#                                  passes SIGTERM on to the container and exits with it.
#                                  FAKE_BRIDGE_STUCK=1: the container ignores SIGTERM.
#                                  FAKE_BRIDGE_CLIENT_DIES=1: the client exits 3 at once and
#                                  the container runs on, never serving.
#   stop -t N <name>               SIGTERM to the container, SIGKILL after N s
#   rm -f <name>                   SIGKILL to the container, and its state removed
#
# Events and arguments are logged as the fake fcvm logs them; the container's pid is in
# $FAKE_FCVM_DIR/container-<name>.pid while it runs.
set -u
dir="${FAKE_FCVM_DIR:?}"
log="$dir/events.log"
gone() { ! kill -0 "$1" 2>/dev/null || grep -q '^State:.*Z' "/proc/$1/status" 2>/dev/null; }
case "${1:-}" in
    stop)
        name="${*: -1}" secs="$3"
        echo "podman_stop $name $$ $(date +%s%N)" >> "$log"
        pid=$(cat "$dir/container-$name.pid" 2>/dev/null) || exit 0
        kill -TERM "$pid" 2>/dev/null
        for _ in $(seq $((secs * 10))); do gone "$pid" && exit 0; sleep 0.1; done
        kill -KILL "$pid" 2>/dev/null
        rm -f "$(cat "$dir/container-$name.port" 2>/dev/null)"
        exit 0 ;;
    rm)
        name="${*: -1}"
        echo "podman_rm $name $$ $(date +%s%N)" >> "$log"
        pid=$(cat "$dir/container-$name.pid" 2>/dev/null) && kill -KILL "$pid" 2>/dev/null
        rm -f "$(cat "$dir/container-$name.port" 2>/dev/null)" "$dir/container-$name.pid" "$dir/container-$name.port"
        exit 0 ;;
    run) ;;
    *) exit 2 ;;
esac
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
    [ "${FAKE_LOGON_EXIT:-0}" = 0 ] || { echo "[fake podman] the logon failed" >&2; exit "$FAKE_LOGON_EXIT"; }
    printf 'P6\n' > "$state/$(basename "$ref_out")"
    exit 0
fi
marker="$dir/bridge-$port.up"
echo "$marker" > "$dir/container-$name.port"
(
    if [ -n "${FAKE_BRIDGE_STUCK:-}" ]; then trap '' TERM; else trap 'rm -f "$marker"; exit 0' TERM; fi
    sleep "${FAKE_BRIDGE_SECS:-0.2}"
    [ -n "${FAKE_BRIDGE_CLIENT_DIES:-}" ] || date +%s%3N > "$marker"
    # The test's directory going away ends a container nothing stopped.
    while [ -d "$dir" ]; do sleep 0.2; done
) &
cpid=$!
echo "$cpid" > "$dir/container-$name.pid"
trap 'kill -TERM "$cpid" 2>/dev/null' TERM INT
if [ -n "${FAKE_BRIDGE_CLIENT_DIES:-}" ]; then
    sleep 0.3
    echo "died $name $$ $(date +%s%N)" >> "$log"
    exit 3
fi
while ! gone "$cpid"; do wait "$cpid"; done
rm -f "$dir/container-$name.pid"
echo "exit $name $$ $(date +%s%N)" >> "$log"
exit 0
