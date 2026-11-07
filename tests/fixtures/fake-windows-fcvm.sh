#!/bin/bash
# A stand-in for fcvm's raw-VM commands in the Windows mode agent tests:
#
#   vm run --name N ... / snapshot run --snapshot T --name N   a VM that runs until SIGTERM and
#       is listed healthy by `ls --json` after FAKE_HEALTHY_SECS (default 0.2), with an RDP
#       relay socket path of its own
#   snapshot create --pid P --tag T --stop   writes $FAKE_SNAPSHOT_DIR/T/config.json and stops P
#   ls --json                                every VM up, as fcvm lists them
#
# It logs `<event> <name> <pid> <ns>` lines to $FAKE_FCVM_DIR/events.log and each VM's
# arguments, one per line, to $FAKE_FCVM_DIR/args-<name>.
set -u
dir="${FAKE_FCVM_DIR:?}"
log="$dir/events.log"
mkdir -p "$dir/ls"
if [ "${1:-}" = ls ]; then
    shopt -s nullglob
    sep=""
    printf '['
    for f in "$dir"/ls/*.json; do printf '%s' "$sep"; cat "$f"; sep=","; done
    printf ']\n'
    exit 0
fi
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
    echo "snapshot_create $tag $pid $(date +%s%N)" >> "$log"
    mkdir -p "$FAKE_SNAPSHOT_DIR/$tag"
    echo '{}' > "$FAKE_SNAPSHOT_DIR/$tag/config.json"
    [ -n "$stop" ] && kill -TERM "$pid"
    exit 0
fi
kind="${1:-}_${2:-}"
argv=("$@")
name=""
while [ $# -gt 0 ]; do
    case "$1" in --name) name="$2"; shift ;; esac
    shift
done
echo "$kind $name $$ $(date +%s%N)" >> "$log"
printf '%s\n' "${argv[@]}" > "$dir/args-$name"
stop() {
    rm -f "$dir/ls/$$.json"
    echo "exit $name $$ $(date +%s%N)" >> "$log"
    exit 0
}
trap stop TERM INT
sleep "${FAKE_HEALTHY_SECS:-0.2}" & wait $!
printf '{"name":"%s","pid":%s,"health_status":"healthy","config":{"guest":"windows","rdp_socket":"%s"}}' \
    "$name" "$$" "$dir/rdp-$$.sock" > "$dir/ls/$$.json"
while true; do sleep 1 & wait $!; done
