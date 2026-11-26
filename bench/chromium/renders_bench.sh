#!/bin/bash
# Pages mode's host module (`fcvm fleet renders-bench`) as a systemd unit.
#
#   renders_bench.sh run [module arguments]     start the module and wait for its end
#   renders_bench.sh start [module arguments]   start it and return
#   renders_bench.sh teardown-check [STAGES]    end a module at each stage and count what is
#                                               left (renders_teardown_check.py). The stage
#                                               `golden` exists only while the snapshot is
#                                               made, so name it with a TAG that has none.
#   renders_bench.sh agent-check [STAGES]       the same module under `fcvm fleet agent
#                                               --renders`, ordered by this script as the
#                                               coordinator would (renders_agent_check.py):
#                                               hello, renders, an overlay, a stop while
#                                               rendering and while held at HOLD_STAGES, and
#                                               SIGKILL at each of STAGES. CASES=running,held,kill
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
#   MAX_FCVM, MAX_ENGINES, RECYCLE_MIB, REPLAY_PROCS, REPLAY_CPUS, VIEWPORT, CPUS_FCVM, CPUS_ENGINES,
#   CPUS_HARNESS, TIMEOUT_S   the module's flags of those names, when set
#   IMAGE, CPU, MEM   the snapshot is made from IMAGE with CPU vCPUs and MEM MiB when it is
#                absent (the module's defaults: the bench image, 4, 1024, on 2 MiB pages)
#   UNIT, RUN_DIR, STATUS, FCVM, URLS, CORPUS
#   AFTER_S, STAGE_TIMEOUT_S   the checks' seconds of rendering before a kill, and per stage
#   HOLD_STAGES, CASES          agent-check: stages to stop at, and which cases to run
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd -P)"
mode="${1:-}"; [ $# -gt 0 ] && shift

for tool in systemd-run systemctl python3 sudo jq; do
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
# The module's options shared by the benchmark command and the agent (`--renders-arg`).
module_opts() {
    printf '%s\0' --out "$OUT" --run-dir "$RUN_DIR" --status-file "$STATUS" \
        --bench "$HERE" --corpus "$CORPUS" --urls "$URLS" --snapshot-tag "$TAG"
    [ -z "${IMAGE:-}" ] || printf '%s\0' --image "$IMAGE"
    [ -z "${CPU:-}" ] || printf '%s\0' --vcpus "$CPU"
    [ -z "${MEM:-}" ] || printf '%s\0' --mem-mib "$MEM"
    [ -z "${KITESURF:-}" ] || printf '%s\0' --kitesurf "$KITESURF"
    [ -z "${DIST:-}" ] || printf '%s\0' --kitesurf-dist "$DIST"
    [ -z "${MAX_FCVM:-}" ] || printf '%s\0' --max-fcvm "$MAX_FCVM"
    [ -z "${MAX_ENGINES:-}" ] || printf '%s\0' --max-engines "$MAX_ENGINES"
    [ -z "${RECYCLE_MIB:-}" ] || printf '%s\0' --recycle-mib "$RECYCLE_MIB"
    [ -z "${REPLAY_PROCS:-}" ] || printf '%s\0' --replay-procs "$REPLAY_PROCS"
    [ -z "${REPLAY_CPUS:-}" ] || printf '%s\0' --replay-cpus "$REPLAY_CPUS"
    [ -z "${VIEWPORT:-}" ] || printf '%s\0' --viewport "$VIEWPORT"
    [ -z "${CPUS_FCVM:-}" ] || printf '%s\0' --cpus-fcvm "$CPUS_FCVM"
    [ -z "${CPUS_ENGINES:-}" ] || printf '%s\0' --cpus-engines "$CPUS_ENGINES"
    [ -z "${CPUS_HARNESS:-}" ] || printf '%s\0' --cpus-harness "$CPUS_HARNESS"
    [ -z "${TIMEOUT_S:-}" ] || printf '%s\0' --timeout-s "$TIMEOUT_S"
}

module_args() {
    printf '%s\0' fleet renders-bench
    module_opts
    [ -z "${LEVELS:-}" ] || printf '%s\0' --levels "$LEVELS"
    [ -z "${SECONDS_RUN:-}" ] || printf '%s\0' --seconds "$SECONDS_RUN"
}

# The agent, with harmless values for its own required options: a host name, a renderer
# image and a first port that no slot uses (it finds no GPU under a missing sysfs root).
agent_args() {
    printf '%s\0' fleet agent --stdio --host renders-agent-check --image unused \
        --base-port 1 --sysfs-root "$OUT.no-sysfs" --renders
    local o
    while IFS= read -r -d '' o; do printf '%s\0' "--renders-arg=$o"; done < <(module_opts)
}

# The unit's properties, for the benchmark command and the agent alike.
unit_props() {
    printf '%s\0' --unit "$UNIT" --collect --quiet \
        -p Delegate=yes -p DelegateSubgroup=agent -p "User=$(id -un)" \
        -p LimitNOFILE=1048576 -p TasksMax=infinity \
        --setenv=HOME="$HOME" --setenv=XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" \
        --setenv=PATH="$PATH" --setenv=RUST_LOG="${RUST_LOG:-info}"
}

start_unit() {  # wait|no-block, then the module's further arguments
    local how=$1; shift
    local args=() props=()
    while IFS= read -r -d '' a; do args+=("$a"); done < <(module_args)
    while IFS= read -r -d '' a; do props+=("$a"); done < <(unit_props)
    local wait=(--wait)
    [ "$how" = wait ] || wait=()
    mkdir -p "$(dirname "$OUT")"
    sudo systemd-run "${props[@]}" "${wait[@]}" "$FCVM" "${args[@]}" "$@"
}

# fcvm's state and data directories as the unit's fcvm resolves them, asked of that binary
# with the unit's environment (unit_props: this user, HOME, XDG_RUNTIME_DIR, PATH and
# nothing else). The state directory is fcvm's answer, never a join of the data root. Set
# before census_args runs, because a failure inside its process substitution would be lost.
FCVM_STATE_DIR="" FCVM_DATA_ROOT=""
load_fcvm_paths() {
    local paths
    paths=$(env -i HOME="$HOME" XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" \
        PATH="$PATH" "$FCVM" paths --json) \
        || { echo "BLOCKED: '$FCVM paths --json' failed (its stderr is above)" >&2; return 2; }
    FCVM_STATE_DIR=$(jq -er '.state_dir | select(type == "string" and startswith("/"))' \
        <<<"$paths") \
        || { echo "BLOCKED: '$FCVM paths --json' printed no state_dir: $paths" >&2; return 2; }
    FCVM_DATA_ROOT=$(jq -er '.data_dir | select(type == "string" and startswith("/"))' \
        <<<"$paths") \
        || { echo "BLOCKED: '$FCVM paths --json' printed no data_dir: $paths" >&2; return 2; }
}

census_args() {  # what renders_teardown_check.py counts, for both checks
    local a p n
    printf '%s\0' --unit "$UNIT" --status "$STATUS" --teardown "$OUT/teardown.json" \
        --run-dir "$RUN_DIR" --state-dir "$FCVM_STATE_DIR" --vm-dir "$FCVM_DATA_ROOT/vm-disks" \
        --data-dir "$FCVM_DATA_ROOT" --snapshot-tag "$TAG"
    for a in 127.0.0.1 127.0.0.3; do for p in 80 443; do printf '%s\0' --port "$a:$p"; done; done
    printf '%s\0' --port 127.0.0.1:53
    for n in $(seq 0 $(( ${MAX_ENGINES:-48} - 1 ))); do printf '%s\0' --port "127.0.0.1:$((28800 + n))"; done
}

case "$mode" in
    run) start_unit wait "$@" ;;
    start) start_unit no-block "$@" ;;
    teardown-check)
        load_fcvm_paths || exit 2
        stages="${1:-start,cgroups,replays,serve-starting,serve,engines-starting,engines,workers,running,running:TERM}"
        count=()
        while IFS= read -r -d '' a; do count+=("$a"); done < <(census_args)
        python3 "$HERE/renders_teardown_check.py" "${count[@]}" --stages "$stages" \
            --after-s "${AFTER_S:-20}" --timeout-s "${STAGE_TIMEOUT_S:-1800}" --out "$OUT.teardown-check.json" \
            -- "$0" start
        ;;
    agent-check)
        load_fcvm_paths || exit 2
        stages="${1:-start,cgroups,replays,serve-starting,serve,engines-starting,engines,workers,running,running:TERM}"
        count=() props=() agent=()
        while IFS= read -r -d '' a; do count+=("$a"); done < <(census_args)
        while IFS= read -r -d '' a; do props+=("$a"); done < <(unit_props)
        while IFS= read -r -d '' a; do agent+=("$a"); done < <(agent_args)
        engines=(--engines)
        [ "${MAX_ENGINES:-48}" != 0 ] || engines=()
        mkdir -p "$(dirname "$OUT")"
        python3 "$HERE/renders_agent_check.py" "${count[@]}" "${engines[@]}" \
            --pages "$(printf '%s' "$URLS" | tr ',' '\n' | grep -c .)" --stages "$stages" \
            --hold-stages "${HOLD_STAGES:-replays,serve,engines-starting}" --cases "${CASES:-running,held,kill}" \
            --after-s "${AFTER_S:-20}" --timeout-s "${STAGE_TIMEOUT_S:-1800}" \
            --log "$OUT.agent-check.log" --out "$OUT.agent-check.json" \
            -- sudo systemd-run "${props[@]}" --pipe "$FCVM" "${agent[@]}"
        ;;
    *) echo "usage: OUT=DIR $0 run|start|teardown-check|agent-check [...]" >&2; exit 2 ;;
esac
