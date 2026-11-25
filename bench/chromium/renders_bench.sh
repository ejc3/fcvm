#!/bin/bash
# Pages mode's host module (`fcvm fleet renders-bench`) as a systemd unit.
#
#   renders_bench.sh run [module arguments]     start the module and wait for its end
#   renders_bench.sh start [module arguments]   start it and return
#   renders_bench.sh teardown-check [STAGES]    end a module at each stage and count what is
#                                               left (renders_teardown_check.py). The stage
#                                               `golden` exists only while the snapshot is
#                                               made, so name it with a TAG that has none.
#
# The module puts each side in a cpuset of its own, so it runs in a subgroup of a delegated
# unit: a transient system service of the calling user with Delegate=yes and
# DelegateSubgroup=. The unit keeps systemd's default KillMode (control-group). With
# KillMode=mixed systemd sends SIGKILL to every process left the moment the main process
# is gone, the module's reaper included, and the clones' state files and the run directory
# then stay until the next start's sweep.
#
# Environment:
#   OUT          where the records go (required)
#   TAG          the snapshot every clone is restored from (default renders-golden)
#   KITESURF     a Kitesurf build: src and tools/node-*/bin (required unless MAX_ENGINES=0)
#   DIST         the engine bundle (default $KITESURF/dist)
#   LEVELS       F,K: hold these renders in flight and run no search
#   SECONDS_RUN  stop after this many seconds of rendering (default 0: until a signal)
#   MAX_FCVM, MAX_ENGINES, RECYCLE_MIB, REPLAY_PROCS, VIEWPORT, CPUS_FCVM, CPUS_ENGINES,
#   CPUS_HARNESS, TIMEOUT_S   the module's flags of those names, when set
#   IMAGE, CPU, MEM   the snapshot is made from IMAGE with CPU vCPUs and MEM MiB when it is
#                absent (the module's defaults: the bench image, 4, 1024, on 2 MiB pages)
#   UNIT, RUN_DIR, STATUS, FCVM, URLS, CORPUS
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd -P)"
mode="${1:-}"; [ $# -gt 0 ] && shift

for tool in systemd-run systemctl python3 sudo; do
    command -v "$tool" >/dev/null 2>&1 || { echo "BLOCKED: '$tool' missing" >&2; exit 2; }
done
: "${OUT:?OUT=DIR is where the records go}"
TAG="${TAG:-renders-golden}"
UNIT="${UNIT:-fcvm-renders}"
RUN_DIR="${RUN_DIR:-${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/fcvm-renders}"
STATUS="${STATUS:-$OUT.status.json}"
FCVM="${FCVM:-$REPO/target/release/fcvm}"
CORPUS="${CORPUS:-$HERE/corpus-live}"
URLS="${URLS:-$(sed -n 's/^URLS="\(https[^"]*\)".*/\1/p' "$HERE/corpus_campaign.sh" | head -1)}"
[ -n "$URLS" ] || { echo "BLOCKED: no page list (URLS)" >&2; exit 2; }
[ -x "$FCVM" ] || { echo "BLOCKED: no fcvm binary at $FCVM (make build)" >&2; exit 2; }
module_args() {
    printf '%s\0' fleet renders-bench --out "$OUT" --run-dir "$RUN_DIR" --status-file "$STATUS" \
        --bench "$HERE" --corpus "$CORPUS" --urls "$URLS" --snapshot-tag "$TAG"
    [ -z "${IMAGE:-}" ] || printf '%s\0' --image "$IMAGE"
    [ -z "${CPU:-}" ] || printf '%s\0' --vcpus "$CPU"
    [ -z "${MEM:-}" ] || printf '%s\0' --mem-mib "$MEM"
    [ -z "${KITESURF:-}" ] || printf '%s\0' --kitesurf "$KITESURF"
    [ -z "${DIST:-}" ] || printf '%s\0' --kitesurf-dist "$DIST"
    [ -z "${LEVELS:-}" ] || printf '%s\0' --levels "$LEVELS"
    [ -z "${SECONDS_RUN:-}" ] || printf '%s\0' --seconds "$SECONDS_RUN"
    [ -z "${MAX_FCVM:-}" ] || printf '%s\0' --max-fcvm "$MAX_FCVM"
    [ -z "${MAX_ENGINES:-}" ] || printf '%s\0' --max-engines "$MAX_ENGINES"
    [ -z "${RECYCLE_MIB:-}" ] || printf '%s\0' --recycle-mib "$RECYCLE_MIB"
    [ -z "${REPLAY_PROCS:-}" ] || printf '%s\0' --replay-procs "$REPLAY_PROCS"
    [ -z "${VIEWPORT:-}" ] || printf '%s\0' --viewport "$VIEWPORT"
    [ -z "${CPUS_FCVM:-}" ] || printf '%s\0' --cpus-fcvm "$CPUS_FCVM"
    [ -z "${CPUS_ENGINES:-}" ] || printf '%s\0' --cpus-engines "$CPUS_ENGINES"
    [ -z "${CPUS_HARNESS:-}" ] || printf '%s\0' --cpus-harness "$CPUS_HARNESS"
    [ -z "${TIMEOUT_S:-}" ] || printf '%s\0' --timeout-s "$TIMEOUT_S"
}

start_unit() {  # wait|no-block, then the module's further arguments
    local how=$1; shift
    local args=()
    while IFS= read -r -d '' a; do args+=("$a"); done < <(module_args)
    local wait=(--wait)
    [ "$how" = wait ] || wait=()
    mkdir -p "$(dirname "$OUT")"
    sudo systemd-run --unit "$UNIT" --collect --quiet "${wait[@]}" \
        -p Delegate=yes -p DelegateSubgroup=agent -p "User=$(id -un)" \
        -p LimitNOFILE=1048576 -p TasksMax=infinity \
        --setenv=HOME="$HOME" --setenv=XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" \
        --setenv=PATH="$PATH" --setenv=RUST_LOG="${RUST_LOG:-info}" \
        "$FCVM" "${args[@]}" "$@"
}

case "$mode" in
    run) start_unit wait "$@" ;;
    start) start_unit no-block "$@" ;;
    teardown-check)
        stages="${1:-start,cgroups,replays,serve-starting,serve,engines-starting,engines,workers,running,running:TERM}"
        data="${FCVM_DATA_DIR:-/mnt/fcvm-btrfs}"
        ports=()
        for a in 127.0.0.1 127.0.0.3; do for p in 80 443; do ports+=(--port "$a:$p"); done; done
        ports+=(--port 127.0.0.1:53)
        for n in $(seq 0 $(( ${MAX_ENGINES:-48} - 1 ))); do ports+=(--port "127.0.0.1:$((28800 + n))"); done
        python3 "$HERE/renders_teardown_check.py" --unit "$UNIT" --status "$STATUS" \
            --teardown "$OUT/teardown.json" --run-dir "$RUN_DIR" --stages "$stages" \
            --state-dir "$data/state" --vm-dir "$data/vm-disks" --data-dir "$data" --snapshot-tag "$TAG" \
            --after-s "${AFTER_S:-20}" --timeout-s "${STAGE_TIMEOUT_S:-1800}" --out "$OUT.teardown-check.json" \
            "${ports[@]}" -- "$0" start
        ;;
    *) echo "usage: OUT=DIR $0 run|start|teardown-check [...]" >&2; exit 2 ;;
esac
