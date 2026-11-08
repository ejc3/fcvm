#!/bin/bash
# A stand-in for fcvm in the Minecraft mode agent tests (tests/test_fleet_minecraft.rs):
#
#   podman run --name N ...                 a VM that runs until SIGTERM; its tile's bots have
#                                           joined once ($FAKE_FCVM_DIR/sessions-N is 1)
#   snapshot create --pid P --tag T [--stop]  writes $FAKE_SNAPSHOT_DIR/T/config.json and the
#                                           sessions count of P's VM beside it, and stops P
#                                           with --stop only
#   snapshot run --snapshot T --name N      a VM of T: its bots join again, one past T's count
#
#   FAKE_UP_SECS        time before a VM's tile answers (default 0.1)
#   FAKE_SNAPSHOT_SECS  a snapshot create takes this long, then logs snapshot_done
#   FAKE_FAIL_FORK      a VM restored from a mc-fork-* snapshot exits with status 3
#   FAKE_STALL_FORK     a VM restored from a mc-fork-* snapshot holds that snapshot's generation
#                       lock shared, as fcvm snapshot run does while it restores, and never comes up
#
# The test's fake tiles read sessions-<name> (absent while the VM is down). It logs `<event>
# <name> <pid> <tag or -> <ns>` lines to $FAKE_FCVM_DIR/events.log and each VM's arguments, one
# per line, to $FAKE_FCVM_DIR/args-<name>.
set -u
dir="${FAKE_FCVM_DIR:?}"
snaps="${FAKE_SNAPSHOT_DIR:?}"
log="$dir/events.log"
if [ "${1:-}" = snapshot ] && [ "${2:-}" = create ]; then
    pid="" tag="" stop=""
    shift 2
    while [ $# -gt 0 ]; do
        case "$1" in
            --pid) pid="$2"; shift ;;
            --tag) tag="$2"; shift ;;
            --stop) stop=1 ;;
        esac
        shift
    done
    name=$(cat "$dir/pid-$pid")
    echo "snapshot_create $name $pid $tag${stop:+ stop} $(date +%s%N)" >> "$log"
    # FAKE_SNAPSHOT_SECS: a slow save; the end is logged so a test sees it ran to completion.
    if [ -n "${FAKE_SNAPSHOT_SECS:-}" ]; then
        sleep "$FAKE_SNAPSHOT_SECS"
        echo "snapshot_done $name $pid $tag $(date +%s%N)" >> "$log"
    fi
    mkdir -p "$snaps/$tag"
    echo '{}' > "$snaps/$tag/config.json"
    cp "$dir/sessions-$name" "$snaps/$tag/sessions"
    [ -n "$stop" ] && kill -TERM "$pid"
    exit 0
fi
kind="${1:-}_${2:-}"
argv=("$@")
name="" tag=""
while [ $# -gt 0 ]; do
    case "$1" in
        --name) name="$2"; shift ;;
        --snapshot) tag="$2"; shift ;;
    esac
    shift
done
printf '%s\n' "${argv[@]}" > "$dir/args-$name"
echo "$name" > "$dir/pid-$$"
echo "$kind $name $$ ${tag:--} $(date +%s%N)" >> "$log"
stop() {
    rm -f "$dir/sessions-$name"
    echo "exit $name $$ - $(date +%s%N)" >> "$log"
    exit 0
}
trap stop TERM INT
if [ -n "${FAKE_STALL_FORK:-}" ] && [ "${tag#mc-fork-}" != "$tag" ]; then
    exec 9>"$snaps/$tag.lock"
    flock -s 9
    echo "stalled $name $$ $tag $(date +%s%N)" >> "$log"
    while true; do sleep 1 & wait $!; done
fi
sleep "${FAKE_UP_SECS:-0.1}" & wait $!
# FAKE_FAIL_FORK: a restore of a fork's snapshot exits at once, as a copy that cannot run.
if [ -n "${FAKE_FAIL_FORK:-}" ] && [ "${tag#mc-fork-}" != "$tag" ]; then
    echo "died $name $$ $tag $(date +%s%N)" >> "$log"
    exit 3
fi
if [ -n "$tag" ]; then
    echo $(( $(cat "$snaps/$tag/sessions") + 1 )) > "$dir/sessions-$name"
else
    echo 1 > "$dir/sessions-$name"
fi
while true; do sleep 1 & wait $!; done
