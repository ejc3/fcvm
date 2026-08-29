#!/bin/bash
# Standalone driver for the request-optimized path (CDP direct + fast teardown).
#
# Separate from bench.sh ON PURPOSE, and not merged into it: bash reads a script
# incrementally as it executes, so editing bench.sh while a run is in flight
# corrupts that run. This file is self-contained; bench.sh is untouched.
#
#   ./reqbench.sh golden      # podman prepare: cold build, snapshot at the image health gate
#   ./reqbench.sh verify      # prove all three hops on a RESTORED CLONE (do this first)
#   ./reqbench.sh run         # the three-arm A/B
#   ./reqbench.sh diag        # what holds each page's load event: one traced render per clone
#
# The two changes under test:
#   PART 1  the request path is Chromium's own CDP endpoint, driven from the host
#           over fcvm's port forwarding. Nothing of ours is resident in the guest.
#   PART 2  the response is delivered the instant the image is in hand; teardown
#           is ONE SIGKILL to fcvm, which the kernel fans out to Firecracker and
#           the namespace holder concurrently via PR_SET_PDEATHSIG.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="${REQBENCH_SOURCE_REPO:-$(cd "$HERE/../.." && pwd)}"
FCVM="${FCVM:-$REPO/target/release/fcvm}"
if [ -z "${FC_AGENT:-}" ]; then
    if [ -x "$(dirname "$FCVM")/fc-agent" ]; then
        FC_AGENT="$(dirname "$FCVM")/fc-agent"
    else
        FC_AGENT="$REPO/target/aarch64-unknown-linux-musl/release/fc-agent"
    fi
fi
# rootless needs no sudo. SUDO is kept as a hook only so the same script can be
# pointed at a root-only mode later without rewriting every call site.
SUDO="${SUDO:-}"
# ENGINE picks the render driver and, with it, the image and port defaults.
# webkit: W3C WebDriver classic on 9515 via wddrive.py; the session id is baked
# into the golden (entry-webkit.sh), captured once per run over fcvm exec.
ENGINE="${ENGINE:-chromium}"
case "$ENGINE" in
chromium)
    IMAGE="${IMAGE:-localhost/chromium-bench-req}"
    CONTAINERFILE="Containerfile.chromium-bench"
    ;;
webkit)
    IMAGE="${IMAGE:-localhost/webkit-bench-req}"
    CONTAINERFILE="Containerfile.webkit-bench"
    CDP_PORT="${CDP_PORT:-9515}"
    # cdp-fast is CDP WebSocket prewiring; exec's guest driver is CDP-only.
    ARMS="${ARMS:-cdp,noop}"
    ;;
*)
    echo "unknown ENGINE '$ENGINE' (chromium|webkit)" >&2
    exit 2
    ;;
esac
# 9222 is Chromium's own CDP port. It binds guest loopback ONLY (it ignores
# --remote-debugging-address; measured evidence in entry.sh), but fcvm DNATs each
# eligible published TCP port to guest 127.0.0.1 when setup succeeds, so
# `--publish 9222:9222` reaches it directly.
# The former socat relay from 9223 -> 9222 is gone, removing a per-clone process
# and byte-path hop. The earlier availability A/B was withdrawn because its arms
# were not comparable, so it does not attribute connection failures to that relay.
# See fc-agent/src/network.rs::publish_to_loopback.
CDP_PORT="${CDP_PORT:-9222}"
# rootless: --publish is supported (pasta -t) and clones inherit port_mappings
# from snapshot metadata (src/commands/snapshot.rs:1070). No root needed, and it
# matches the network mode the exec-path baseline was measured on.
NETMODE="${NETMODE:-rootless}"
CPU="${CPU:-2}"
MEM="${MEM:-1024}"
RUNID="${RUNID:-$(tr -d - </proc/sys/kernel/random/uuid)}"
[[ "$RUNID" =~ ^[0-9a-f]{32}$ ]] \
    || { echo "RUNID must be exactly 32 lowercase hexadecimal characters" >&2; exit 2; }
RESULTS="${RESULTS:-$HERE/results/reqbench-$RUNID}"
TAG="${TAG:-cb-req-golden}"
if [ "$TAG" = . ] || [ "$TAG" = .. ] || [ "${#TAG}" -gt 128 ] \
   || [[ ! "$TAG" =~ ^[A-Za-z0-9_.-]+$ ]]; then
    echo "TAG must be 1..128 ASCII letters, digits, '-', '_', or '.', excluding . and .." >&2
    exit 2
fi
FCVM_LOG="${FCVM_LOG:-fcvm=debug}"   # AGENTS.md defect 4: never measure at info
# Fail closed rather than trusting the default: an override that selects info
# drops the per-request fcvm records every phase here reads, and the run still
# produces numbers, so the loss is silent. Guarding the variable covers every
# call site in this file, not just the one a reviewer happened to be looking at.
# Exact comma-separated directives, not a substring test: `notfcvm=debug` is a
# different target and `fcvm=debugging` is a level tracing ignores — both would
# pass a substring gate and still produce none of the records this harness reads.
# Spaces are dropped first because tracing trims around each directive, so
# `warn, fcvm=debug` does select fcvm=debug and must not be refused. No valid
# directive contains a space, which makes deleting them all safe here.
fcvm_log_ok=0
IFS=',' read -ra fcvm_log_parts <<< "${FCVM_LOG// /}"
for fcvm_log_part in "${fcvm_log_parts[@]}"; do
    case "$fcvm_log_part" in
        fcvm=debug|fcvm=trace) fcvm_log_ok=1 ;;
    esac
done
if [ "$fcvm_log_ok" -ne 1 ]; then
    echo "FCVM_LOG must select fcvm=debug or fcvm=trace as an exact directive (got '$FCVM_LOG'): the harness reads fcvm's debug records" >&2
    exit 2
fi
unset fcvm_log_ok fcvm_log_parts fcvm_log_part
URL="${URL:-http://127.0.0.1:8000/medium.html}"

STATE_DIR="${STATE_DIR:-/mnt/fcvm-btrfs/state}"
DATA_ROOT="${DATA_ROOT:-$(dirname "$STATE_DIR")}"
# fcvm resolves snapshots/ (and the generation lock this harness shares with
# it) from FCVM_DATA_DIR. reqbench derives DATA_ROOT independently; export
# the alignment so both processes always lock the SAME files even when the
# caller overrides the paths.
export FCVM_DATA_DIR="$DATA_ROOT"
LOADAVG_FILE="${LOADAVG_FILE:-/proc/loadavg}"
QUIET_LOADAVG1_LIMIT="2.0"
QUIET_GUARD_LOADAVG1=""
QUIET_GUARD_VM_PROCESSES=""

# Execute every measured phase from one private, content-identified runtime
# bundle. A concurrent `make build` or source edit can replace the repository
# paths between repetitions; copying first means the exact fcvm binary and all
# request code used by this invocation are the bytes named by its manifest.
if [ "${BASH_SOURCE[0]}" = "$0" ] && [ "${REQBENCH_STAGED:-0}" != 1 ]; then
    [ -x "$FCVM" ] || { echo "fcvm binary is not executable: $FCVM" >&2; exit 2; }
    [ -x "$FC_AGENT" ] || { echo "fc-agent binary is not executable: $FC_AGENT" >&2; exit 2; }
    source_revision_before=$(git -C "$REPO" rev-parse HEAD)
    mkdir -p "$RESULTS/runtime"
    stage_dir=$(mktemp -d "$RESULTS/runtime/.stage.XXXXXX")
    for source in reqbench.sh reqbench.py reqanalyze.py cdpdrive.py render.py wddrive.py; do
        cp --reflink=auto "$HERE/$source" "$stage_dir/$source"
    done
    cp --reflink=auto "$FC_AGENT" "$stage_dir/fc-agent"
    cp --reflink=auto "$FCVM" "$stage_dir/fcvm"
    chmod 0555 "$stage_dir/fcvm" "$stage_dir/fc-agent" "$stage_dir/reqbench.sh" \
        "$stage_dir/reqbench.py" "$stage_dir/reqanalyze.py" \
        "$stage_dir/cdpdrive.py" "$stage_dir/render.py" "$stage_dir/wddrive.py"
    (
        cd "$stage_dir"
        sha256sum fcvm fc-agent reqbench.sh reqbench.py reqanalyze.py cdpdrive.py render.py wddrive.py \
            > MANIFEST.sha256
    )
    bundle_hash=$(sha256sum "$stage_dir/MANIFEST.sha256" | cut -d' ' -f1)
    bundle_dir="$RESULTS/runtime/$bundle_hash"
    exec {STAGE_LOCK_FD}>"$RESULTS/runtime/.install.lock"
    flock "$STAGE_LOCK_FD"
    if [ -e "$bundle_dir" ]; then
        cmp -s "$stage_dir/MANIFEST.sha256" "$bundle_dir/MANIFEST.sha256" \
            || { echo "runtime bundle hash collision at $bundle_dir" >&2; exit 2; }
        (
            cd "$bundle_dir"
            sha256sum --check --status MANIFEST.sha256
        ) || { echo "existing runtime bundle is corrupt: $bundle_dir" >&2; exit 2; }
        case "$stage_dir" in
            "$RESULTS"/runtime/.stage.*) rm -rf -- "$stage_dir" ;;
            *) echo "refusing to remove unexpected staging path $stage_dir" >&2; exit 2 ;;
        esac
    else
        mv --no-target-directory "$stage_dir" "$bundle_dir"
    fi
    flock -u "$STAGE_LOCK_FD"
    source_revision=$(git -C "$REPO" rev-parse HEAD)
    [ "$source_revision" = "$source_revision_before" ] \
        || { echo "repository revision changed while staging runtime" >&2; exit 2; }
    exec env \
        REQBENCH_STAGED=1 \
        REQBENCH_SOURCE_REPO="$REPO" \
        REQBENCH_SOURCE_REVISION="$source_revision" \
        REQBENCH_RUNTIME_BUNDLE="$bundle_dir" \
        FCVM="$bundle_dir/fcvm" \
        FC_AGENT="$bundle_dir/fc-agent" \
        SUDO="$SUDO" \
        IMAGE="$IMAGE" \
        ENGINE="$ENGINE" \
        CDP_PORT="$CDP_PORT" \
        NETMODE="$NETMODE" \
        CPU="$CPU" \
        MEM="$MEM" \
        RUNID="$RUNID" \
        RESULTS="$RESULTS" \
        TAG="$TAG" \
        FCVM_LOG="$FCVM_LOG" \
        URL="$URL" \
        STATE_DIR="$STATE_DIR" \
        DATA_ROOT="$DATA_ROOT" \
        LOADAVG_FILE="$LOADAVG_FILE" \
        bash "$bundle_dir/reqbench.sh" "$@"
fi

mkdir -p "$RESULTS/logs"
log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >&2; }

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    mkdir -p "$DATA_ROOT/reqbench-locks"
    exec {TAG_LOCK_FD}>"$DATA_ROOT/reqbench-locks/$TAG.lock"
    flock -n "$TAG_LOCK_FD" \
        || { log "another reqbench invocation owns snapshot tag $TAG"; exit 3; }
fi

verify_runtime_bundle() {
    [ -n "${REQBENCH_RUNTIME_BUNDLE:-}" ] || return 0
    (
        cd "$REQBENCH_RUNTIME_BUNDLE"
        sha256sum --check --status MANIFEST.sha256
    ) || { log "FATAL: staged runtime bundle changed during the run"; return 1; }
}

# ---------------------------------------------------------------------------
# TEARDOWN. Every background fcvm this script starts is registered here the
# instant it exists, and the trap fires on EXIT/INT/TERM.
#
# Without it, `set -euo pipefail` turns every error path into a leak: an
# unguarded failure exits the shell BEFORE the matching kill, and the VM or
# serve it started keeps running. Two of the three phases did not even capture
# `$!`, so on those paths there was no handle to kill at all. bench.sh in this
# same directory has had this shape all along; this file did not have a single
# `trap`. AGENTS.md: contention silently inflates every number, and a leaked
# serve holds the snapshot mapping into the NEXT run.
CLEANUP_PIDS=()
ACTIVE_DRIVER_BG=""

process_identity() {
    local pid="$1" raw rest state fields
    [ -r "/proc/$pid/stat" ] || return 1
    raw=$(<"/proc/$pid/stat") || return 1
    rest=${raw##*) }
    fields=($rest)
    state=${fields[0]:-}
    [ "$state" != Z ] && [ "$state" != X ] && [ "$state" != x ] || return 1
    printf '%s\n' "${fields[19]:-}"
}

track() {
    local pid="${1:-}" existing start kept=()
    [ -n "$pid" ] || return 0
    start=$(process_identity "$pid") || return 0
    for existing in "${CLEANUP_PIDS[@]}"; do
        [ -n "$existing" ] || continue
        # A numeric PID may now identify a later process. Drop every older
        # identity for that number before recording the process we just read;
        # otherwise stop_tracked finds the stale entry, declines to signal it,
        # untracks by PID, and silently leaves the new process running.
        [ "${existing%%:*}" = "$pid" ] || kept+=("$existing")
    done
    CLEANUP_PIDS=("${kept[@]}")
    # Preserve starttime with every PID. This also safely tracks the real fcvm
    # state PID when sudo uses a separate monitor process: a reused numeric PID
    # never passes process_matches below.
    CLEANUP_PIDS+=("$pid:$start")
}

untrack() {
    local remove="$1" entry kept=()
    for entry in "${CLEANUP_PIDS[@]}"; do
        [ "${entry%%:*}" = "$remove" ] || kept+=("$entry")
    done
    CLEANUP_PIDS=("${kept[@]}")
}

tracked_entry() {
    local wanted="$1" entry
    for entry in "${CLEANUP_PIDS[@]}"; do
        [ "${entry%%:*}" = "$wanted" ] && { printf '%s\n' "$entry"; return 0; }
    done
    return 1
}

process_matches() {
    # Declare before expanding: bash expands every assignment in one `local`
    # command against the caller's dynamically scoped variables. At top level
    # there is no caller-local `entry`, so `set -u` made a direct identity check
    # fail before it could inspect /proc.
    local entry="$1" pid expected actual
    pid="${entry%%:*}"
    expected="${entry#*:}"
    actual=$(process_identity "$pid") || return 1
    [ "$actual" = "$expected" ]
}

stop_tracked() {
    local pid="$1" timeout="${2:-15}" entry deadline forced=0
    entry=$(tracked_entry "$pid") || return 0
    process_matches "$entry" && $SUDO kill -TERM "$pid" 2>/dev/null || :
    deadline=$((SECONDS + timeout))
    while process_matches "$entry" && [ "$SECONDS" -lt "$deadline" ]; do
        sleep 0.05
    done
    if process_matches "$entry"; then
        forced=1
        $SUDO kill -KILL "$pid" 2>/dev/null || :
        deadline=$((SECONDS + 10))
        while process_matches "$entry" && [ "$SECONDS" -lt "$deadline" ]; do
            sleep 0.05
        done
    fi
    if process_matches "$entry"; then
        log "ERROR: tracked process $pid survived SIGKILL; retaining its lifecycle record"
        return 2
    fi
    wait "$pid" 2>/dev/null || :
    untrack "$pid"
    return "$forced"
}

CLEANUP_RAN=0

cleanup() {
    local rc=$?
    [ "$CLEANUP_RAN" -eq 0 ] || return "$rc"
    CLEANUP_RAN=1
    set +e
    local entry pid deadline cleanup_failed=0
    # The driver owns the active clone and depends on the UFFD serve. Give it the
    # full request+teardown budget before stopping that dependency.
    if [ -n "$ACTIVE_DRIVER_BG" ]; then
        stop_tracked "$ACTIVE_DRIVER_BG" 180
        ACTIVE_DRIVER_BG=""
    fi
    for entry in "${CLEANUP_PIDS[@]}"; do
        pid=${entry%%:*}
        process_matches "$entry" && $SUDO kill -TERM "$pid" 2>/dev/null
    done
    deadline=$((SECONDS + 180))
    while [ "${#CLEANUP_PIDS[@]}" -gt 0 ] && [ "$SECONDS" -lt "$deadline" ]; do
        local remaining=()
        for entry in "${CLEANUP_PIDS[@]}"; do
            pid=${entry%%:*}
            if process_matches "$entry"; then
                remaining+=("$entry")
            else
                wait "$pid" 2>/dev/null
            fi
        done
        CLEANUP_PIDS=("${remaining[@]}")
        [ "${#CLEANUP_PIDS[@]}" -eq 0 ] || sleep 0.05
    done
    for entry in "${CLEANUP_PIDS[@]}"; do
        pid=${entry%%:*}
        process_matches "$entry" && $SUDO kill -KILL "$pid" 2>/dev/null
    done
    deadline=$((SECONDS + 10))
    while [ "${#CLEANUP_PIDS[@]}" -gt 0 ] && [ "$SECONDS" -lt "$deadline" ]; do
        local remaining=()
        for entry in "${CLEANUP_PIDS[@]}"; do
            pid=${entry%%:*}
            if process_matches "$entry"; then
                remaining+=("$entry")
            else
                wait "$pid" 2>/dev/null
            fi
        done
        CLEANUP_PIDS=("${remaining[@]}")
        [ "${#CLEANUP_PIDS[@]}" -eq 0 ] || sleep 0.05
    done
    for entry in "${CLEANUP_PIDS[@]}"; do
        log "ERROR: tracked process ${entry%%:*} survived SIGKILL; retaining its state and disk"
        cleanup_failed=1
    done
    if [ "$rc" -eq 0 ] && [ "$cleanup_failed" -ne 0 ]; then
        rc=1
    fi
    return $rc
}

on_signal() {
    local number="$1"
    trap - INT TERM
    cleanup
    exit $((128 + number))
}

trap cleanup EXIT
trap 'on_signal 2' INT
trap 'on_signal 15' TERM

# Refuse to measure on a box that is already busy. AGENTS.md: contention silently
# inflates every number; a run published without saying so is the failure mode.
vm_process_count() {
    local rows stat comm count=0
    rows=$(ps -eo stat=,comm=) || {
        log "REFUSING: cannot inspect running VM processes"
        return 3
    }
    while read -r stat comm; do
        # A zombie consumes no CPU and cannot contaminate the measurement. It is
        # still a lifecycle defect, but the benchmark's teardown gate reports it.
        [[ "$stat" == Z* ]] && continue
        case "$comm" in
            fcvm|firecracker*|cloud-hypervis*) count=$((count + 1)) ;;
        esac
    done <<<"$rows"
    printf '%s\n' "$count"
}

guard_quiet_sample() {
    # Match comm, not argv. Repository paths in a tmux/Codex command line used to
    # make `pgrep -f` count the benchmark operator as an fcvm process. Prefixes
    # are required because content-addressed Firecracker names and Linux's
    # 15-character comm truncation defeat exact-name matching.
    local fc
    fc=$(vm_process_count) || return $?
    local la
    la=$(cut -d' ' -f1 "$LOADAVG_FILE") || {
        log "REFUSING: cannot read host load from $LOADAVG_FILE"
        return 3
    }
    [[ "$la" =~ ^[0-9]+([.][0-9]+)?$ ]] || {
        log "REFUSING: invalid host load in $LOADAVG_FILE: ${la:-<empty>}"
        return 3
    }
    log "load=$la vm-processes=$fc"
    local load_busy=0 load_whole load_fraction
    load_whole=${la%%.*}
    case "$la" in
        *.*) load_fraction=${la#*.} ;;
        *) load_fraction="" ;;
    esac
    load_fraction=${load_fraction//0/}
    if [ "$load_whole" -gt 2 ] || {
        [ "$load_whole" -eq 2 ] && [ -n "$load_fraction" ];
    }; then
        load_busy=1
    fi
    if [ "${ALLOW_BUSY:-0}" != 1 ] && { [ "${fc:-0}" -gt 0 ] || \
       [ "$load_busy" -ne 0 ]; }; then
        log "REFUSING: box is busy (load=$la, $fc firecracker/fcvm). Set ALLOW_BUSY=1 to override"
        log "and SAY SO in the report — a number measured under contention is not a number."
        return 3
    fi
    QUIET_GUARD_LOADAVG1="$la"
    QUIET_GUARD_VM_PROCESSES="$fc"
}

guard_quiet() {
    # SETTLE_WAIT_SECS > 0 turns a busy first sample into a bounded re-sample
    # loop. The one-shot chain (cmd_all) reaches this gate seconds after its
    # own build, golden and verify phases, so the 1-minute load average still
    # carries that prerequisite work; without the window a cold invocation
    # refuses on its own wake and a retry repeats the phony prerequisites.
    # Default 0 keeps the standalone fail-fast refusal.
    local settle="${SETTLE_WAIT_SECS:-0}"
    [[ "$settle" =~ ^[0-9]+$ ]] || {
        log "REFUSING: SETTLE_WAIT_SECS must be a whole number of seconds (got '$settle')"
        return 3
    }
    # Base 10: "08" passes the validator and is invalid octal to bash
    # arithmetic, and "010" would silently mean eight.
    settle=$((10#$settle))
    local rc=0
    guard_quiet_sample || rc=$?
    [ "$rc" -ne 0 ] || return 0
    [ "$settle" -gt 0 ] || return "$rc"
    local deadline=$((SECONDS + settle)) nap
    while [ "$SECONDS" -lt "$deadline" ]; do
        nap=$((deadline - SECONDS))
        [ "$nap" -le 5 ] || nap=5
        log "settling: box is busy; re-sampling in ${nap}s ($((deadline - SECONDS))s left in the ${settle}s window)"
        sleep "$nap"
        rc=0
        guard_quiet_sample || rc=$?
        [ "$rc" -ne 0 ] || return 0
    done
    log "REFUSING: box still busy after ${settle}s settle wait"
    return "$rc"
}

state_pid_by_name() {
    $SUDO "$FCVM" ls --json 2>/dev/null | python3 -c '
import json,sys
for v in json.load(sys.stdin):
    if v.get("name")==sys.argv[1]: print(v.get("pid") or ""); break' "$1"
}

state_vm_id_by_pid() {
    $SUDO "$FCVM" ls --json --pid "$1" 2>/dev/null | python3 -c '
import json,sys
rows=json.load(sys.stdin)
print(rows[0].get("vm_id") or "" if rows else "")'
}

assert_vm_artifacts_absent() {
    local vm_id="$1" left=0 path
    [[ "$vm_id" =~ ^vm-[0-9a-f]{32}$ ]] \
        || { log "refusing artifact check for invalid vm_id $vm_id"; return 1; }
    for path in \
        "$STATE_DIR/$vm_id.json" \
        "$STATE_DIR/$vm_id.json.lock" \
        "$DATA_ROOT/vm-disks/$vm_id"; do
        if [ -e "$path" ] || [ -L "$path" ]; then
            log "cleanup left exact artifact $path"
            left=1
        fi
    done
    return "$left"
}

# The build-to-golden handshake file for $IMAGE. cmd_build records the image
# ID it published here; cmd_golden consumes it (reads and removes it in one
# rename) so a later golden with no preceding build cannot inherit a stale
# record. The filename keys on a hash of the full image name: character
# substitution is not injective (localhost/a_b:c and localhost/a:b_c would
# share one file and clobber each other's records).
built_image_id_file() {
    printf '%s/reqbench-locks/built-image-%s.id\n' \
        "$DATA_ROOT" "$(printf '%s' "$IMAGE" | sha256sum | cut -c1-32)"
}

cmd_build() {
    log "building $IMAGE"
    local sealed_source ctx built_id id_file id_tmp iid_tmp
    # Fail-fast pre-check. The bundle sealed this invocation's request code at
    # process start, but the repository is what feeds the image, and the
    # staging guard compares git HEAD only, which cannot see an uncommitted
    # edit. Diverged bytes here mean the exec arm (image copy) and the CDP
    # arm (bundle copy) would run different render code under a passing seal.
    if [ -n "${REQBENCH_RUNTIME_BUNDLE:-}" ]; then
        for sealed_source in render.py cdpdrive.py wddrive.py; do
            cmp -s "$REPO/bench/chromium/$sealed_source" \
                "$REQBENCH_RUNTIME_BUNDLE/$sealed_source" \
                || { log "FATAL: $sealed_source in $REPO/bench/chromium diverged from the sealed runtime bundle; the image would bake different bytes than this run executes"; return 1; }
        done
    fi
    # Podman reads COPY sources when the build executes, not when the check
    # above ran, so an edit landing in between would still bake into the
    # image. Build from an immutable staged context instead: the
    # Containerfiles COPY only bench/chromium top-level files and
    # bench/chromium/pages/, and the sealed sources come from the runtime
    # bundle, not the repository. A Containerfile that grows a COPY source
    # outside this set fails the build loudly; extend the staging here.
    ctx=$(mktemp -d "$RESULTS/logs/build-context.XXXXXX") \
        || { log "FATAL: cannot create a staged build context under $RESULTS/logs"; return 1; }
    mkdir -p "$ctx/bench/chromium"
    find "$REPO/bench/chromium" -maxdepth 1 -type f \
        -exec cp --reflink=auto -t "$ctx/bench/chromium" {} + \
        || { log "FATAL: cannot stage bench/chromium sources into $ctx"; return 1; }
    cp -a --reflink=auto "$REPO/bench/chromium/pages" "$ctx/bench/chromium/pages" \
        || { log "FATAL: cannot stage bench/chromium/pages into $ctx"; return 1; }
    cp --reflink=auto "$REPO/$CONTAINERFILE" "$ctx/$CONTAINERFILE" \
        || { log "FATAL: cannot stage $CONTAINERFILE into $ctx"; return 1; }
    if [ -n "${REQBENCH_RUNTIME_BUNDLE:-}" ]; then
        for sealed_source in render.py cdpdrive.py wddrive.py; do
            cp --reflink=auto "$REQBENCH_RUNTIME_BUNDLE/$sealed_source" \
                "$ctx/bench/chromium/$sealed_source" \
                || { log "FATAL: cannot stage sealed $sealed_source into $ctx"; return 1; }
        done
    fi
    # --format docker is LOAD-BEARING: podman's default OCI format DROPS
    # HEALTHCHECK with only a warning, and fcvm's health gate is what
    # triggers the golden snapshot (src/health.rs AND-logic).
    # --iidfile is equally load-bearing: podman records the ID of the image
    # THIS build produced as part of the build operation itself. Inspecting
    # the tag afterwards reads whatever the tag points at by then, which a
    # concurrent retag in that window can change.
    iid_tmp=$(mktemp "$RESULTS/logs/build-iid.XXXXXX") \
        || { log "FATAL: cannot stage the image-ID capture under $RESULTS/logs"; return 1; }
    podman build --format docker --iidfile "$iid_tmp" -t "$IMAGE" \
        -f "$ctx/$CONTAINERFILE" "$ctx"
    built_id=$(tr -d '[:space:]' <"$iid_tmp")
    rm -f "$iid_tmp"
    rm -rf -- "$ctx"
    built_id="sha256:${built_id#sha256:}"
    [[ "$built_id" =~ ^sha256:[0-9a-f]{64}$ ]] \
        || { log "FATAL: podman recorded no valid image ID for $IMAGE (got '$built_id')"; return 1; }
    # The warm gate lives or dies here. fcvm treats a MISSING healthcheck as a
    # PASS, so an image that lost it snapshots a COLD browser and every clone's
    # "warm" latency is really a first-paint number. Assert the healthcheck
    # exists AND is ours, on the built ID rather than the retaggable tag:
    # health_state.sh reports healthy only for a FRESH verdict from the
    # resident checker, which in turn requires the warm marker that entry.sh
    # touches only after a full navigate + screenshot.
    podman inspect "$built_id" --format '{{json .HealthCheck}}' | grep -q health_state \
        || { log "FATAL: image has no HEALTHCHECK naming health_state.sh (OCI format drop, or the Containerfile changed without this check)"; return 1; }
    # Record the ID this build published so cmd_golden, which runs as a
    # separate process with the TAG lock released in between, can refuse a
    # tag another worktree repointed in that window.
    mkdir -p "$DATA_ROOT/reqbench-locks"
    id_file=$(built_image_id_file)
    id_tmp=$(mktemp "$id_file.XXXXXX") \
        || { log "FATAL: cannot stage the built-image record next to $id_file"; return 1; }
    printf '%s\n' "$built_id" >"$id_tmp"
    mv -f "$id_tmp" "$id_file"
    log "build: published $IMAGE as $built_id"
}

cmd_golden() {
    log "golden: podman prepare --tag $TAG (cold build, snapshot at the image health gate, hugepages=$HUGEPAGES)"
    local name="cb-req-g-$RUNID" lf="$RESULTS/logs/golden.log"
    local image_record image_digest image_id image_cache_key
    # GUEST_ENV is checked first, before the build record below is consumed:
    # an entry without KEY= would reach fcvm as `--env <entry>` and be baked
    # into the snapshot as whatever fcvm makes of it.
    local guest_env_entries=() guest_env_flags=() guest_env_entry
    if [ -n "$GUEST_ENV" ]; then
        IFS=',' read -ra guest_env_entries <<<"$GUEST_ENV"
        for guest_env_entry in "${guest_env_entries[@]}"; do
            [[ "$guest_env_entry" =~ ^[A-Za-z_][A-Za-z0-9_]*= ]] \
                || { log "golden: GUEST_ENV entry '$guest_env_entry' is not KEY=VALUE"; return 1; }
            guest_env_flags+=(--env "$guest_env_entry")
        done
    fi
    # One inspect binds the mutable tag's manifest digest and immutable image ID
    # to the same observation. fcvm performs the same atomic resolution before
    # exporting by ID; the cache-path check committed with the snapshot below
    # detects a tag replacement between these two observations and fails closed.
    image_record=$(podman image inspect "$IMAGE") \
        || { log "golden: cannot identify benchmark image $IMAGE"; return 1; }
    image_digest=$(jq -r '.[0].Digest // "" | select(type == "string")' \
        <<<"$image_record") \
        || { log "golden: benchmark image $IMAGE has invalid digest metadata"; return 1; }
    image_id=$(jq -er '.[0].Id | select(type == "string" and length > 0)' \
        <<<"$image_record") \
        || { log "golden: benchmark image $IMAGE has no content ID"; return 1; }
    image_id="sha256:${image_id#sha256:}"
    [[ "$image_id" =~ ^sha256:[0-9a-f]{64}$ ]] \
        || { log "golden: benchmark image $IMAGE has invalid content ID $image_id"; return 1; }
    if [ -n "$image_digest" ]; then
        image_digest="sha256:${image_digest#sha256:}"
        [[ "$image_digest" =~ ^sha256:[0-9a-f]{64}$ ]] \
            || { log "golden: benchmark image $IMAGE has invalid digest $image_digest"; return 1; }
        image_cache_key="${image_digest#sha256:}"
    else
        image_cache_key="${image_id#sha256:}"
    fi
    # The checks below only prove this process's own observation is
    # self-consistent. When cmd_build recorded which ID it published, require
    # the tag to still resolve there: build and golden run as separate
    # processes with the TAG lock released between them, and a tag swap in
    # that window would snapshot the replacement.
    local built_id_file built_image_id=""
    built_id_file=$(built_image_id_file)
    if [ -e "$built_id_file" ]; then
        # Claim the record atomically: the rename makes this golden the only
        # consumer, and removal afterwards keeps a later golden that had no
        # build of its own from attesting this build's ID as its provenance.
        local built_id_claim
        built_id_claim="$built_id_file.consumed.$$"
        mv "$built_id_file" "$built_id_claim" \
            || { log "golden: cannot claim the built-image record $built_id_file"; return 1; }
        built_image_id=$(tr -d '[:space:]' <"$built_id_claim")
        rm -f "$built_id_claim"
        [[ "$built_image_id" =~ ^sha256:[0-9a-f]{64}$ ]] \
            || { log "golden: recorded built-image ID in $built_id_file is invalid: '$built_image_id'"; return 1; }
        if [ "$image_id" != "$built_image_id" ]; then
            log "golden: $IMAGE resolved to $image_id but the build phase recorded $built_image_id; the tag was repointed between build and golden"
            return 1
        fi
    fi
    # --publish carries host -> guest; fc-agent DNATs the published port to
    # guest loopback (fc-agent/src/network.rs::publish_to_loopback), the hop
    # Chromium refuses to make itself. Clones inherit port_mappings from the
    # snapshot metadata, which is what makes a restored clone drivable at all.
    #
    # `podman prepare` owns the lifecycle this phase used to hand-roll: it
    # forces a cold build (no snapshot-cache restore, the contamination the old
    # FCVM_NO_SNAPSHOT=1 dance guarded against), requires and waits for the
    # image's podman HEALTHCHECK — the real CDP round trip — as the capture
    # trigger, installs the generation atomically under the tag, verifies the
    # installed artifact, and tears the source VM down with verified cleanup.
    # --force replaces a stale tag, which also retires the explicit
    # `snapshots delete` this phase used to need.
    # prepare's stdout is a one-line JSON record of the generation it installed,
    # captured separately from the log so the provenance below can be bound to
    # THAT generation rather than to whatever currently sits under the tag.
    local prepared_json="$RESULTS/logs/golden-prepared.json"
    local huge_flag=""
    if [ "$HUGEPAGES" = 1 ]; then
        huge_flag="--hugepages"
        ensure_hugepage_pool || return 1
    fi
    local dns_flag=()
    [ -n "$GUEST_DNS" ] && dns_flag=(--dns "$GUEST_DNS")
    $SUDO env RUST_LOG="$FCVM_LOG" "$FCVM" podman prepare --tag "$TAG" --force $huge_flag "${dns_flag[@]}" \
        "${guest_env_flags[@]}" \
        --name "$name" --cpu "$CPU" --mem "$MEM" --network "$NETMODE" \
        --publish "$CDP_PORT:$CDP_PORT" "$IMAGE" 2>"$lf" >"$prepared_json" \
        || { log "golden: PREPARE FAILED"; tail -20 "$lf" >&2; return 1; }
    local prepared_generation prepared_digest
    prepared_generation=$(jq -er '.generation_id' <"$prepared_json") \
        || { log "golden: prepare reported no generation_id"; return 1; }
    prepared_digest=$(jq -er '.config_digest' <"$prepared_json") \
        || { log "golden: prepare reported no config_digest"; return 1; }
    # Bind the mutable image tag to the exact content captured by this snapshot.
    # The file lives inside the atomically replaced snapshot directory and names
    # its generation, so a recreated tag cannot inherit stale provenance.
    $SUDO python3 - "$DATA_ROOT/snapshots/$TAG/config.json" \
        "$DATA_ROOT/snapshots/$TAG/reqbench-provenance.json" \
        "$DATA_ROOT/snapshots/$TAG.lock" "$IMAGE" "$image_id" "$image_digest" \
        "$image_cache_key" "$built_image_id" \
        "$prepared_generation" "$prepared_digest" \
        "$(sha256sum "$FCVM" | cut -d' ' -f1)" \
        "$(sha256sum "$HERE/MANIFEST.sha256" | cut -d' ' -f1)" \
        "${REQBENCH_SOURCE_REVISION:-}" "$GUEST_DNS" "${guest_env_entries[@]}" <<'PY'
import fcntl, hashlib, json, os, sys, tempfile, uuid
(
    config_path, output_path, lock_path, image_label, image_id, image_digest,
    image_cache_key, built_image_id, prepared_generation, prepared_digest,
    fcvm_sha256, runtime_bundle_sha256, source_revision, guest_dns,
) = sys.argv[1:15]
# The GUEST_ENV entries that became `--env` flags, in order; empty when the
# knob was unset.
guest_env = sys.argv[15:]
lock = open(lock_path, "a+")
fcntl.flock(lock, fcntl.LOCK_SH)
with open(config_path, "rb") as source:
    config_json = source.read()
config = json.loads(config_json)
generation_id = config.get("generation_id")
try:
    canonical_generation_id = str(uuid.UUID(generation_id))
except (AttributeError, TypeError, ValueError):
    raise SystemExit(f"snapshot has invalid generation_id {generation_id!r}")
if canonical_generation_id != generation_id:
    raise SystemExit(f"snapshot has non-canonical generation_id {generation_id!r}")
config_sha256 = hashlib.sha256(config_json).hexdigest()
metadata = config.get("metadata") or {}
if metadata.get("image") != image_label:
    raise SystemExit(
        f"snapshot image {metadata.get('image')!r} does not match label {image_label!r}"
    )
image_disk_path = metadata.get("image_disk_path")
if not isinstance(image_disk_path, str) or not image_disk_path:
    raise SystemExit("snapshot has no content-addressed image disk path")
if not os.path.basename(image_disk_path).startswith(image_cache_key + "."):
    raise SystemExit(
        f"snapshot image disk {image_disk_path!r} does not match inspected "
        f"cache key {image_cache_key!r}; the image tag changed during golden creation"
    )
# Bind the record to the generation `podman prepare` reported installing, not
# merely to whatever sits under the tag now. Any other fcvm command may replace
# the tag between prepare exiting and this shared lock being taken, and a
# replacement carrying the same image would otherwise pass every check here and
# be stamped with this run's creator hashes and source revision.
if generation_id != prepared_generation:
    raise SystemExit(
        f"snapshot generation {generation_id!r} is not the one prepare installed "
        f"({prepared_generation!r}); the tag was replaced before provenance was committed"
    )
if config_sha256 != prepared_digest:
    raise SystemExit(
        f"snapshot config digest {config_sha256!r} does not match prepare's "
        f"({prepared_digest!r}); the generation changed in place"
    )
# --dns is a request; metadata.network_config.dns_server is what fc-agent
# wrote to the guest's resolv.conf. A golden whose snapshot records another
# resolver would carry provenance claiming a replay wiring it does not have.
# Only checked when GUEST_DNS was given: without it fcvm fills dns_server
# from the host resolver, so the recorded null means "not requested".
baked_dns = (metadata.get("network_config") or {}).get("dns_server")
if guest_dns and baked_dns != guest_dns:
    raise SystemExit(
        f"GUEST_DNS={guest_dns!r} was requested but the snapshot's "
        f"metadata.network_config.dns_server is {baked_dns!r}; the guest did not "
        "bake the resolver this golden claims"
    )
record = {
    "guest_dns": guest_dns or None,
    "guest_env": guest_env,
    "snapshot_generation_id": generation_id,
    "snapshot_config_sha256": config_sha256,
    "snapshot_created_at": config.get("created_at"),
    "snapshot_vm_id": config.get("vm_id"),
    "image": image_label,
    "image_label": image_label,
    "image_id": image_id,
    "image_digest": image_digest,
    "image_cache_key": image_cache_key,
    # The ID cmd_build recorded, when a build preceded this golden; null for
    # a golden taken against a pre-existing image with no build record.
    "built_image_id": built_image_id or None,
    "creator_fcvm_sha256": fcvm_sha256,
    "creator_runtime_bundle_sha256": runtime_bundle_sha256,
    "source_revision": source_revision,
}
directory = os.path.dirname(output_path)
fd, temporary = tempfile.mkstemp(prefix=".reqbench-provenance.", dir=directory)
try:
    with os.fdopen(fd, "w") as target:
        json.dump(record, target, sort_keys=True)
        target.write("\n")
        target.flush()
        os.fsync(target.fileno())
    os.replace(temporary, output_path)
    directory_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)
except BaseException:
    try:
        os.unlink(temporary)
    except FileNotFoundError:
        pass
    raise
PY
    log "golden: done ($TAG)"
}

# ---------------------------------------------------------------------------
# The end-to-end chain proof. Run this BEFORE trusting any number: it checks the
# three hops separately so a failure names the hop instead of looking like
# "networking is broken".

# Start one clone against the running serve, or (empty serve pid) from the
# snapshot files. Sets CLONE_PID and CLONE_IP. Factored out so verify can
# start TWO of them: a single clone's target id cannot answer a question
# about stability ACROSS clones.
#
# Results come back in GLOBALS, not on stdout, on purpose: `x=$(start_clone …)`
# runs the function in a subshell, so its `track` calls would update a copy of
# CLEANUP_PIDS that is discarded on return — i.e. the trap would never see the
# clone it just started, which is the exact leak this trap exists to close.
CLONE_PID=""
CLONE_IP=""
CLONE_BG=""
CLONE_VM_ID=""
start_clone() {
    local spid="$1" cname="$2" cl="$3"
    CLONE_PID=""; CLONE_IP=""; CLONE_BG=""; CLONE_VM_ID=""
    # An empty serve pid is BACKEND=file: restore MAP_PRIVATE from the
    # snapshot files under $TAG (reqbench.py's clone_backend_args does the
    # same) instead of from a UFFD serve.
    local -a source_args=(--pid "$spid")
    [ -n "$spid" ] || source_args=(--snapshot "$TAG")
    $SUDO env RUST_LOG=$FCVM_LOG "$FCVM" snapshot run "${source_args[@]}" --name "$cname" \
        --no-dirty-tracking --no-swap >"$cl" 2>&1 &
    CLONE_BG=$!
    track "$CLONE_BG"
    local t0=$SECONDS cpid=""
    until [ -n "$cpid" ]; do
        cpid=$(state_pid_by_name "$cname")
        [ $((SECONDS-t0)) -lt 120 ] || { log "clone $cname never registered"; tail -20 "$cl" >&2; return 1; }
        sleep 0.2
    done
    CLONE_PID="$cpid"
    track "$cpid"
    CLONE_VM_ID=$(state_vm_id_by_pid "$cpid")
    [ -n "$CLONE_VM_ID" ] || { log "clone $cname has no vm_id"; return 1; }
    CLONE_IP=$($SUDO "$FCVM" ls --json --pid "$cpid" | python3 -c '
import json,sys
n=json.load(sys.stdin)[0]["config"]["network"]
print(n.get("loopback_ip") or n.get("host_ip") or n.get("guest_ip") or "")')
}

# The UFFD serve a phase restores its clones from. Results in globals, for
# the reason start_clone gives. $1 = phase label for log lines, $2 = log
# file; anything after is passed to `snapshot serve` (--uffd-mode,
# --uffd-prefetch).
SERVE_PID=""
SERVE_BG=""
start_serve() {
    local phase="$1" sf="$2"
    shift 2
    SERVE_PID=""; SERVE_BG=""
    $SUDO "$FCVM" snapshot serve "$TAG" "$@" >"$sf" 2>&1 &
    SERVE_BG=$!
    track "$SERVE_BG"
    local t0=$SECONDS
    until grep -q "Waiting for VMs" "$sf" 2>/dev/null; do
        [ $((SECONDS-t0)) -lt 60 ] || { log "$phase: serve never came up"; cat "$sf" >&2; return 1; }
        sleep 0.5
    done
    SERVE_PID=$(grep -oP 'Serve PID: \K[0-9]+' "$sf" | head -1)
    [ -n "$SERVE_PID" ] || { log "$phase: could not read Serve PID from $sf"; return 1; }
    track "$SERVE_PID"
}

# Stops what start_serve started; a no-op when nothing was. Returns 1 when
# either process needed SIGKILL or survived it.
stop_serve() {
    local phase="$1" rc=0
    [ -n "$SERVE_BG" ] || return 0
    if [ -n "$SERVE_PID" ]; then
        stop_tracked "$SERVE_PID" || { log "$phase: UFFD serve required SIGKILL"; rc=1; }
    fi
    stop_tracked "$SERVE_BG" || { log "$phase: UFFD serve required SIGKILL"; rc=1; }
    SERVE_PID=""; SERVE_BG=""
    return $rc
}

# Print the page target id for a clone, or nothing.
#
# Delegates to cdpdrive.py --print-target rather than hand-rolling the lookup,
# which fixes TWO defects at once. (1) READINESS: this was a single-shot urlopen
# with `2>/dev/null || true`, and `start_clone` returns as soon as the state file
# carries a pid — it never waits for the CDP port (contrast reqbench.py's
# `wait_port`). Clone 1 is warm because HOPs A/B/C ran against it; clone 2 was
# queried the instant it registered, so a connection refused produced an empty id
# and the documented stability gate failed on a RACE. It fails closed, which is
# the right direction, but a flaky gate is the thing people learn to bypass.
# (2) FILTER MISMATCH: this took the first `type == "page"`, while the driver that
# actually consumes the id skips `devtools://` pages — so the two could compare
# different targets. `resolve_target` now retries against a real deadline and
# applies the devtools:// filter, and both halves are covered by
# CdpDriveResolveThrottling in test_reqbench.py.
TARGET_ID_TIMEOUT="${TARGET_ID_TIMEOUT:-60}"
target_id() {
    python3 "$HERE/cdpdrive.py" "$1" http://unused/ --print-target \
        --timeout "$TARGET_ID_TIMEOUT" 2>/dev/null || true
}

# The webkit twin of target_id(): read the baked session id out of a clone.
# `|| true` for the same reason: under `set -euo pipefail` an exec/cat failure
# inside $() would abort cmd_verify BEFORE its own "HOP C FAILED (no baked
# session id)" / "TARGET ID UNREADABLE" diagnostics and the ordered teardown
# that follows them, the exact failures those branches exist to name.
wd_session_id() {
    $SUDO "$FCVM" exec --pid "$1" -c -- cat /run/bench-session-id 2>/dev/null \
        | tr -d '[:space:]' || true
}

# HOP D: the baked resolver, asked of the restored clone itself.
#
# The corpus campaign bakes GUEST_DNS into the golden so every corpus hostname
# resolves to the pasta gateway and lands on the host replay server. HOPs A-C
# reach the clone by IP and prove nothing about that: a clone whose resolv.conf
# came back pointing at a real resolver renders the live site and records a
# plausible number. This hop reads the resolver the snapshot recorded, then
# checks the clone against it from inside the container, where Chromium runs.
#
# VERIFY_DNS_HOSTS  comma-separated hostnames; each must resolve to
#                   VERIFY_DNS_ANSWER inside the container.
# VERIFY_DNS_URLS   comma-separated URLs; each in-container GET must return a
#                   2xx or 3xx status through the resolver under test.
# With both unset only the recorded resolver is written and no assertion is
# made, so the default golden (no GUEST_DNS) verifies as before. Either one
# set requires a baked resolver and checks both resolv.conf views.
# Evidence lands in $RESULTS/verify-dns.json either way.
VERIFY_DNS_HOSTS="${VERIFY_DNS_HOSTS:-}"
VERIFY_DNS_ANSWER="${VERIFY_DNS_ANSWER:-10.0.2.2}"
VERIFY_DNS_URLS="${VERIFY_DNS_URLS:-}"
# In-container GET with an unverified TLS context: the replay server's
# certificate is self-signed and the resolver is the thing under test. The
# error processor is replaced so every status comes back as a number instead
# of an exception; redirects are therefore not followed, and a 3xx counts as
# the request having reached the replay server. Both bench images ship python3.
#
# No proxy, whatever the environment says. fc-agent runs every container exec
# under the host's saved HTTP_PROXY/HTTPS_PROXY/no_proxy/NO_PROXY
# (fc-agent/src/exec.rs, read_proxy_settings), and build_opener() installs a
# ProxyHandler from the environment by default, so the request would go to
# the proxy and come back with the live site's status while the hostname
# check next to it resolved through the replay resolver. The empty
# ProxyHandler replaces the default one and every *_proxy variable is
# removed before the request, so the only place it can go is the host the
# resolver under test names. The probe prints the status and the proxy
# variables it found and ignored (comma-separated, or "none"); the evidence
# records both.
VERIFY_DNS_URL_PROBE='import os,ssl,sys,urllib.request as u
p=sorted(k for k in os.environ if k.lower().endswith("_proxy"))
for k in p: del os.environ[k]
c=ssl._create_unverified_context()
H=type("H",(u.HTTPErrorProcessor,),{"http_response":lambda s,q,r:r,"https_response":lambda s,q,r:r})
r=u.build_opener(u.ProxyHandler({}),u.HTTPSHandler(context=c),H).open(u.Request(sys.argv[1]),timeout=30)
print(r.status,",".join(p) or "none")'

verify_guest_dns() {
    local cpid="$1" cfg="$DATA_ROOT/snapshots/$TAG/config.json"
    local out="$RESULTS/verify-dns.json" errlog="$RESULTS/logs/verify-dns.log"
    local dns_server
    # jq's exit status is the readability test: a missing binary, a missing or
    # unreadable file and invalid JSON all land here. BLOCKED rather than
    # FAILED, and no evidence file: nothing was evaluated, so nothing can be
    # reported, passing or failing.
    dns_server=$($SUDO jq -r '.metadata.network_config.dns_server | if type == "string" then . else "" end' \
        "$cfg" 2>>"$errlog") \
        || { echo "HOP D BLOCKED: cannot read metadata.network_config.dns_server from $cfg (jq missing, or config.json unreadable)" >&2; return 1; }
    local -a hosts=() urls=()
    [ -z "$VERIFY_DNS_HOSTS" ] || IFS=',' read -ra hosts <<<"$VERIFY_DNS_HOSTS"
    [ -z "$VERIFY_DNS_URLS" ] || IFS=',' read -ra urls <<<"$VERIFY_DNS_URLS"
    echo "--- HOP D: baked resolver INSIDE the restored clone (dns_server=${dns_server:-null}, ${#hosts[@]} host(s), ${#urls[@]} url(s)) ---"
    local failed=0 resolv_vm="" resolv_container="" hosts_json='{}' urls_json='{}'
    if [ -z "$VERIFY_DNS_HOSTS" ] && [ -z "$VERIFY_DNS_URLS" ]; then
        echo "  VERIFY_DNS_HOSTS and VERIFY_DNS_URLS unset: recording the snapshot's resolver, asserting nothing"
    elif [ -z "$dns_server" ]; then
        failed=1
        echo "HOP D FAILED: corpus verify requires a baked resolver, but metadata.network_config.dns_server is null (golden made without GUEST_DNS?)" >&2
    else
        local want="nameserver $dns_server" view
        # Both views: fc-agent writes the VM's resolv.conf from the boot plan
        # and podman derives the container's from it. Chromium reads the
        # second, so the first being right is not enough. The exec status is
        # kept: output from an exec that did not complete proves nothing.
        local rc_vm=0 rc_container=0
        resolv_vm=$($SUDO "$FCVM" exec --pid "$cpid" --vm -- cat /etc/resolv.conf 2>>"$errlog") \
            || rc_vm=$?
        resolv_container=$($SUDO "$FCVM" exec --pid "$cpid" -c -- cat /etc/resolv.conf 2>>"$errlog") \
            || rc_container=$?
        for view in vm container; do
            local text="$resolv_vm" rc="$rc_vm" others=""
            [ "$view" = vm ] || { text="$resolv_container"; rc="$rc_container"; }
            if [ "$rc" -ne 0 ]; then
                failed=1
                echo "HOP D FAILED: reading $view /etc/resolv.conf exited $rc (got: $(tr '\n' '|' <<<"$text"))" >&2
            elif ! grep -qx -- "$want" <<<"$text"; then
                failed=1
                echo "HOP D FAILED: $view /etc/resolv.conf has no '$want' line (got: $(tr '\n' '|' <<<"$text"))" >&2
            else
                # glibc walks the whole nameserver list, so a second entry
                # answers the moment the replay server misses a query.
                others=$(grep -E '^[[:space:]]*nameserver[[:space:]]' <<<"$text" | grep -vx -- "$want" || true)
                if [ -n "$others" ]; then
                    failed=1
                    echo "HOP D FAILED: $view /etc/resolv.conf also names a fallback resolver ($(tr '\n' '|' <<<"$others")), which would answer whenever $dns_server does not" >&2
                else
                    echo "  $view /etc/resolv.conf names $dns_server and nothing else"
                fi
            fi
        done
        local host answer ok
        for host in "${hosts[@]}"; do
            [ -n "$host" ] || continue
            answer=$($SUDO "$FCVM" exec --pid "$cpid" -c -- python3 -c \
                'import socket,sys;print(socket.gethostbyname(sys.argv[1]))' "$host" 2>>"$errlog") \
                || answer="${answer:-}<exec rc=$?>"
            if [ "$answer" = "$VERIFY_DNS_ANSWER" ]; then
                ok=true
            else
                ok=false; failed=1
                echo "HOP D FAILED: $host resolved to '$answer' inside the clone, want $VERIFY_DNS_ANSWER" >&2
            fi
            echo "  $host -> $answer ($ok)"
            hosts_json=$(jq -c --arg h "$host" --arg a "$answer" --argjson ok "$ok" \
                '.[$h] = {answer: $a, ok: $ok}' <<<"$hosts_json")
        done
        local url line status proxy_env rc
        for url in "${urls[@]}"; do
            [ -n "$url" ] || continue
            rc=0
            line=$($SUDO "$FCVM" exec --pid "$cpid" -c -- python3 -c "$VERIFY_DNS_URL_PROBE" "$url" 2>>"$errlog") \
                || rc=$?
            # "<status> <ignored proxy variables|none>". A line without the
            # second field did not come from the probe above and proves
            # nothing about where the request went.
            status=${line%% *}
            proxy_env=""
            [ "$line" = "$status" ] || proxy_env=${line#* }
            [ "$rc" -eq 0 ] || status="${line:-}<exec rc=$rc>"
            if [ "$rc" -eq 0 ] && [ -n "$proxy_env" ] && [[ "$status" =~ ^[0-9]+$ ]] \
                && [ "$status" -ge 200 ] && [ "$status" -le 399 ]; then
                ok=true
            else
                ok=false; failed=1
                if [ "$rc" -eq 0 ] && [ -z "$proxy_env" ]; then
                    echo "HOP D FAILED: GET $url inside the clone printed '$line', not the probe's '<status> <ignored proxies>'" >&2
                else
                    echo "HOP D FAILED: GET $url inside the clone returned '$status', want 200-399" >&2
                fi
            fi
            echo "  GET $url -> $status ($ok)${proxy_env:+ proxy_env_ignored=$proxy_env}"
            urls_json=$(jq -c --arg u "$url" --arg s "$status" --argjson ok "$ok" --arg p "$proxy_env" \
                '.[$u] = {status: (if ($s | test("^[0-9]+$")) then ($s | tonumber) else null end), ok: $ok,
                          proxy_env_ignored: (if $p == "" then null elif $p == "none" then [] else ($p | split(",")) end)}' \
                <<<"$urls_json")
        done
    fi
    local passed=true
    [ "$failed" -eq 0 ] || passed=false
    # proxies_disabled: every URL probe reported the proxy variables it
    # ignored, so none of the requests can have left through a proxy; null
    # when no URL was probed.
    jq -n --arg dns "$dns_server" --arg rv "$resolv_vm" --arg rc "$resolv_container" \
        --argjson hosts "$hosts_json" --argjson urls "$urls_json" \
        --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --argjson passed "$passed" \
        '{dns_server: (if $dns == "" then null else $dns end), resolv_conf_vm: $rv,
          resolv_conf_container: $rc, hosts: $hosts, urls: $urls,
          proxies_disabled: (if ($urls | length) == 0 then null
                             else ($urls | to_entries | all(.value.proxy_env_ignored != null)) end),
          timestamp: $ts, passed: $passed}' \
        >"$out.tmp" && mv "$out.tmp" "$out" \
        || { echo "HOP D BLOCKED: cannot write $out" >&2; return 1; }
    [ "$failed" -eq 0 ] || return 1
    echo "  OK evidence in $out"
}

cmd_verify() {
    # Every hop feeds this counter and the function RETURNS it. Each hop used to
    # be `... || echo "HOP X FAILED"`, which makes the compound command SUCCEED —
    # so cmd_verify exited 0 no matter how many hops failed, and `all` (which
    # relies on `set -e` to stop the chain) went straight on to the measured run
    # after printing three FAILED lines. verify is documented as the gate; a gate
    # that cannot fail is not a gate.
    local fail=0
    # A rebooted or pool-shrunk box re-runs verify against a persisted huge
    # golden with an empty pool; grow it here, not only at golden time.
    # unknown refuses: jq missing or an unreadable config would otherwise
    # skip provisioning fail-OPEN and die mid-serve instead.
    acquire_generation_lock || return 1
    local huge_state
    huge_state=$(hugepage_snapshot_state)
    if [ "$huge_state" = unknown ]; then
        log "verify: cannot determine hugepage state for '$TAG' (config.json unreadable or jq missing)"
        return 1
    fi
    if [ "$huge_state" = huge ]; then
        ensure_hugepage_pool "$(snapshot_memory_mib)" || return 1
    fi
    log "verify: starting serve for $TAG"
    start_serve verify "$RESULTS/logs/verify-serve.log" || return 1
    local spid="$SERVE_PID"
    log "verify: serve pid $spid"

    local cname="cb-req-verify-$RUNID" cl="$RESULTS/logs/verify-clone.log"
    start_clone "$spid" "$cname" "$cl" || return 1
    local cpid="$CLONE_PID" ip="$CLONE_IP" clone1_bg="$CLONE_BG" vmid1="$CLONE_VM_ID"
    # An empty IP is the most likely real misconfiguration, and it used to be
    # fully swallowed: hops B and C then ran against ":$CDP_PORT", failed,
    # printed FAILED, and verify still exited 0.
    [ -n "$ip" ] || { log "verify: NO host-side IP in the clone's network config"; fail=1; }
    log "verify: clone pid=$cpid host-side ip=$ip"

    echo "--- HOP A: healthcheck path, 127.0.0.1:$CDP_PORT INSIDE the container ---"
    local health_probe=cdp_health.py
    [ "$ENGINE" = webkit ] && health_probe=wd_health.py
    $SUDO "$FCVM" exec --pid "$cpid" -c -- python3 "/opt/bench/$health_probe" \
        || { echo "HOP A FAILED (in-container $ENGINE health round trip)"; fail=1; }

    verify_guest_dns "$cpid" || fail=1

    if [ "$ENGINE" = webkit ]; then
        echo "--- HOP B: GET /status from the HOST against $ip:$CDP_PORT ---"
        python3 - "$ip:$CDP_PORT" <<'PYWD' || { echo "HOP B FAILED (host -> clone WD HTTP)"; fail=1; }
import json, sys, urllib.request
host = sys.argv[1]
with urllib.request.urlopen(f"http://{host}/status", timeout=10) as r:
    v = json.load(r)["value"]
print("  OK ready=", v.get("ready"), "|", v.get("message", ""))
PYWD
    else
    echo "--- HOP B: GET /json/version from the HOST against $ip:$CDP_PORT ---"
    python3 - "$ip:$CDP_PORT" <<'PY' || { echo "HOP B FAILED (host -> clone CDP HTTP)"; fail=1; }
import json, sys, urllib.request
host = sys.argv[1]
# Host-header check: Chromium's DevTools endpoint rejects Host values that are
# neither localhost nor an IP literal, with a 403 that reads like a network fault.
req = urllib.request.Request(f"http://{host}/json/version", headers={"Host": host})
with urllib.request.urlopen(req, timeout=10) as r:
    v = json.load(r)
print("  OK", v.get("Browser"), "| protocol", v.get("Protocol-Version"))
req2 = urllib.request.Request(f"http://{host}/json/version", headers={"Host": "evil.example.com"})
try:
    urllib.request.urlopen(req2, timeout=10)
    print("  note: non-IP Host header ACCEPTED (no Host validation on this build)")
except Exception as e:
    print(f"  note: non-IP Host header REJECTED ({e}) — expected; connect by IP")
PY

    fi

    if [ "$ENGINE" = webkit ]; then
        echo "--- HOP C: WD render (navigate + screenshot) from the HOST ---"
        local wd_session
        wd_session=$(wd_session_id "$cpid")
        if [ -z "$wd_session" ]; then
            echo "HOP C FAILED (no baked session id in the clone)"; fail=1
        else
            REQBENCH_HERE="$HERE" python3 - "$ip:$CDP_PORT" "$URL" "$wd_session" "$RESULTS/verify" <<'PYWD3' \
                || { echo "HOP C FAILED (host WD render)"; fail=1; }
import argparse, json, sys
import os; sys.path.insert(0, os.environ["REQBENCH_HERE"])
import wddrive
host, url, session, prefix = sys.argv[1:5]
r = wddrive.drive(argparse.Namespace(cdp_host=host, url=url, timeout=120.0,
                                     session_id=session, out_prefix=prefix))
print(json.dumps({k: r[k] for k in ("ok", "image_bytes", "stages") if k in r}))
sys.exit(0 if r.get("ok") else 1)
PYWD3
        fi
    else
    echo "--- HOP C: WebSocket upgrade + one CDP command from the HOST ---"
    python3 "$HERE/cdpdrive.py" "$ip:$CDP_PORT" "$URL" --format jpeg --nav-timing \
        --out-prefix "$RESULTS/verify" || { echo "HOP C FAILED (host WS + CDP)"; fail=1; }
    fi

    # --- target id stability, ACROSS CLONES, asserted.
    # This block used to print one clone's id and compare it with nothing, under
    # a heading that asked a cross-clone question. One id cannot answer it. The
    # serve is already up, so a second clone is cheap.
    echo "--- target id stability ACROSS CLONES (-> can /json/list be skipped?) ---"
    local id1 id2 cname2="cb-req-verify2-$RUNID" cpid2 ip2 clone2_bg vmid2
    if [ "$ENGINE" = webkit ]; then
        # The id is a baked, immutable file; HOP C already read it from this
        # clone, so reuse that instead of another exec round trip.
        id1="$wd_session"
    else
    id1=$(target_id "$ip:$CDP_PORT")
    fi
    start_clone "$spid" "$cname2" "$RESULTS/logs/verify-clone2.log" || return 1
    cpid2="$CLONE_PID"; ip2="$CLONE_IP"; clone2_bg="$CLONE_BG"; vmid2="$CLONE_VM_ID"
    if [ "$ENGINE" = webkit ]; then
        id2=$(wd_session_id "$cpid2")
    else
    id2=$(target_id "$ip2:$CDP_PORT")
    fi
    echo "  clone1 ($ip) id=${id1:-<none>}"
    echo "  clone2 ($ip2) id=${id2:-<none>}"
    if [ -z "$id1" ] || [ -z "$id2" ]; then
        echo "  TARGET ID UNREADABLE on at least one clone — cannot judge stability"
        fail=1
    elif [ "$id1" = "$id2" ]; then
        echo "  STABLE across 2 clones — --ws-url prewiring is sound for this snapshot"
    else
        echo "  TARGET ID NOT STABLE ACROSS CLONES ($id1 != $id2) — /json/list cannot be skipped"
        fail=1
    fi

    stop_tracked "$cpid2" \
        || { log "verify: clone 2 required SIGKILL"; fail=1; }
    stop_tracked "$clone2_bg" \
        || { log "verify: clone 2 required SIGKILL"; fail=1; }
    stop_tracked "$cpid" \
        || { log "verify: clone 1 required SIGKILL"; fail=1; }
    stop_tracked "$clone1_bg" \
        || { log "verify: clone 1 required SIGKILL"; fail=1; }
    assert_vm_artifacts_absent "$vmid2" || fail=1
    assert_vm_artifacts_absent "$vmid1" || fail=1
    stop_serve verify || fail=1
    # Return AFTER the cleanup above, never before it.
    [ "$fail" -eq 0 ] || { log "verify: FAILED ($fail check(s)) — do NOT run the A/B"; return 1; }
    log "verify: done; both clone state/disk trees are absent"
}

# BOTH memory backends are runnable from here. reqbench.py has had the FILE path
# fully built (`--snapshot-tag` -> fcvm's `--snapshot <name>`, recorded as
# `"backend": "file"` in the run metadata) while this driver hardcoded the UFFD
# serve and never passed `--snapshot-tag` at all — so the recorded metadata was
# honest but could only ever carry one value, and REVIEW.md's re-run gate
# (">=200 CDP requests PER BACKEND at 0 failures") was not runnable.
BACKEND="${BACKEND:-uffd}"
# copy is fcvm's serve default; minor shares clean pages across clones via
# UFFDIO_CONTINUE and is the production-lean configuration. Recorded per run.
UFFD_MODE="${UFFD_MODE:-copy}"
# The diag phase never serves with replay on (cmd_diag says why), so the value
# as it arrived is kept apart from the run default: cmd_diag refuses an
# explicit request for anything but off rather than serving with a knob other
# than the one asked for.
UFFD_PREFETCH_REQUESTED="${UFFD_PREFETCH:-}"
# Replay of recorded fault working sets. PINNED explicitly on the serve line so
# the sealed record proves the effective state — the 08-13 runs left it to the
# binary default and an env override could not be excluded, making replay's
# contribution unattributable.
UFFD_PREFETCH="${UFFD_PREFETCH:-on}"
# Hugepage-backed guest memory (2MB pages). Part of the snapshot identity, so
# the golden must be CREATED with it; serve/restore inherit it from snapshot
# metadata. Use a distinct TAG (e.g. cb-req-golden-huge) so plain and huge
# goldens coexist. faultbench measured huge+minor at zero userspace faults
# per render — this knob puts that configuration on the request path.
HUGEPAGES="${HUGEPAGES:-0}"
# Guest DNS override for the golden (baked into resolv.conf at boot). The
# corpus replay arm sets GUEST_DNS=10.0.2.2 so every corpus hostname
# resolves through the host-loopback replay server via the pasta gateway.
GUEST_DNS="${GUEST_DNS:-}"
# Extra container environment baked into the golden: comma-separated KEY=VALUE
# entries, one `fcvm podman prepare --env` each (a value cannot hold a comma).
# GUEST_ENV=BENCH_RESOLVE_ALL_TO=<ip> is the resolver-rule arm of the corpus
# A/B; entry.sh builds the Chromium flag from that variable. The entries change
# what the snapshot does, so a golden made with GUEST_ENV needs its own TAG
# (--force would otherwise replace the other arm under the default tag). They
# are recorded as guest_env in reqbench-provenance.json.
GUEST_ENV="${GUEST_ENV:-}"
# Arms the analyzer's stall gate (reqanalyze --stall-max-ms). Empty leaves the
# gate unarmed, which campaign_summary refuses to index; the corpus campaign
# sets it.
STALL_MAX_MS="${STALL_MAX_MS:-}"
# The diag phase (cmd_diag, below). DIAG_URLS: comma-separated, default the
# run's URL. DIAG_REPS: clones per URL. DIAG_EXPECT_IPS: comma-separated;
# when set, every remote IP a traced render talked to must be one of them.
# DIAG_MAX_LOAD_MS: when set, a load event over it is a stall.
DIAG_URLS="${DIAG_URLS:-}"
DIAG_REPS="${DIAG_REPS:-3}"
DIAG_EXPECT_IPS="${DIAG_EXPECT_IPS:-}"
DIAG_MAX_LOAD_MS="${DIAG_MAX_LOAD_MS:-}"
# Overridable for unit tests only; production is the real kernel knob.
HUGEPAGE_POOL_FILE="${HUGEPAGE_POOL_FILE:-/proc/sys/vm/nr_hugepages}"

# "huge", "normal", or "unknown" for the INSTALLED snapshot under $TAG.
# unknown (unreadable/missing config.json) must be treated fail-closed by
# callers that would mislabel data on a wrong guess.
hugepage_snapshot_state() {
    local v
    v=$($SUDO jq -r '.metadata.hugepages' \
        "$DATA_ROOT/snapshots/$TAG/config.json" 2>/dev/null) || { echo unknown; return; }
    case "$v" in
        true) echo huge ;;
        false|null) echo normal ;;
        *) echo unknown ;;
    esac
}

# 2MB pages: a MEM-MiB guest needs MEM/2 pages. Sized for the worst
# concurrent set — serve backing memfd + the prepare/verify VM + two clones
# overlapping across a teardown boundary => 4x. Grow-only, and fails closed
# when the kernel cannot deliver the pages (fragmentation): a hugepage phase
# quietly starting on a starved pool would die mid-measurement, or worse,
# measure a mixed-page configuration.
ensure_hugepage_pool() {
    # $1: guest MiB to size for (defaults to $MEM). Measured phases pass the
    # SNAPSHOT's recorded memory_mib — sizing from the caller's ambient MEM
    # under-provisions when a big golden is verified with default knobs.
    local mem="${1:-$MEM}" need cur
    if [ $((mem % 2)) -ne 0 ]; then
        # fcvm rejects odd MEM too, but only AFTER we would have reserved
        # gigabytes of pool for an invocation that can never boot.
        log "hugepages: MEM=${mem} is not divisible by 2 (2MB pages)"
        return 1
    fi
    need=$(( (mem / 2) * 4 ))
    # The pool is host-global state shared by every harness (reqbench,
    # faultbench, bench.sh, make setup-hugepages). One flock serializes all
    # of them (codex P1, PR #815): a phase that DEPENDS on the pool keeps the
    # fd open holding it SHARED for the phase lifetime; a grow upgrades to
    # EXCLUSIVE and atomically downgrades. Bounded waits fail closed rather
    # than hanging behind a stuck owner.
    local wait_s="${HUGEPAGE_POOL_LOCK_WAIT:-60}"
    # Opened READ-ONLY, never with O_CREAT (PR #868): fs.protected_regular
    # refuses an O_CREAT open of a file owned by someone else in a sticky
    # world-writable directory, which is what `<>` did against the lock a
    # root `make setup-hugepages` had created. flock(2) does not care about
    # the open mode. Created atomically when absent (a hard link fails if
    # the name exists), so concurrent creators agree on one inode; same
    # mechanism as scripts/hugepage-pool-lock.sh, which the recipes use.
    local pool_lock="$DATA_ROOT/hugepage-pool.lock"
    mkdir -p "$DATA_ROOT" 2>/dev/null || true
    if [ ! -e "$pool_lock" ]; then
        local tmp
        if tmp="$(mktemp -u "$pool_lock.XXXXXX" 2>/dev/null)"; then
            { install -m 644 /dev/null "$tmp" && ln "$tmp" "$pool_lock"; } 2>/dev/null || true
            rm -f "$tmp"
        fi
    fi
    # The entry is checked before AND after opening (codex + CodeRabbit on
    # #868): a symlink, a non-regular file, or a file owned by anyone but
    # root, this user, or the data root's owner is refused, since its owner
    # could repoint or recreate it under a holder and a later caller would
    # lock a different inode. After the open, the descriptor's inode must be
    # the path's own (lstat) inode.
    if [ -L "$pool_lock" ]; then
        log "hugepages: $pool_lock is a symlink (owner uid $(stat -c %u "$pool_lock")); refusing to lock through a path another user can repoint"
        return 1
    fi
    if [ ! -f "$pool_lock" ]; then
        log "hugepages: pool lock unavailable at $pool_lock (not a regular file)"
        return 1
    fi
    local lock_owner root_owner
    lock_owner="$(stat -c %u "$pool_lock")"
    root_owner="$(stat -c %u "$DATA_ROOT")"
    case "$lock_owner" in
        0|"$(id -u)"|"$root_owner") ;;
        *) log "hugepages: $pool_lock is owned by uid $lock_owner, who can recreate it under a holder; refusing"; return 1 ;;
    esac
    if [ -z "${REQBENCH_POOL_LOCK_FD:-}" ]; then
        exec {REQBENCH_POOL_LOCK_FD}<"$pool_lock" || true
    fi
    if [ -z "${REQBENCH_POOL_LOCK_FD:-}" ]; then
        log "hugepages: pool lock unavailable at $pool_lock"
        return 1
    fi
    if [ "$(stat -c %d:%i "$pool_lock")" != "$(stat -Lc %d:%i "/proc/$$/fd/$REQBENCH_POOL_LOCK_FD")" ]; then
        log "hugepages: $pool_lock was replaced between check and open; refusing"
        return 1
    fi
    if ! flock -s -w "$wait_s" "$REQBENCH_POOL_LOCK_FD"; then
        log "hugepages: pool lock busy for ${wait_s}s; refusing to race the owner"
        return 1
    fi
    cur=$(cat "$HUGEPAGE_POOL_FILE" 2>/dev/null || echo 0)
    if [ "$cur" -lt "$need" ]; then
        if ! flock -x -w "$wait_s" "$REQBENCH_POOL_LOCK_FD"; then
            log "hugepages: pool lock busy for ${wait_s}s; refusing to race the owner"
            return 1
        fi
        # Re-read under the exclusive lock: another grower may have won.
        cur=$(cat "$HUGEPAGE_POOL_FILE" 2>/dev/null || echo 0)
        if [ "$cur" -lt "$need" ]; then
            log "hugepages: growing pool $cur -> $need (${mem}MiB guest x4)"
            # Literal sudo, NOT $SUDO: $SUDO is empty by default (rootless
            # needs no privilege), but writing the kernel pool knob requires
            # root in every mode. $SUDO here would break the rootless path.
            sudo sh -c 'echo "$1" > "$2"' _ "$need" "$HUGEPAGE_POOL_FILE"
            cur=$(cat "$HUGEPAGE_POOL_FILE" 2>/dev/null || echo 0)
        fi
        flock -s "$REQBENCH_POOL_LOCK_FD"
        if [ "$cur" -lt "$need" ]; then
            log "hugepages: pool only $cur/$need pages (fragmentation?)"
            return 1
        fi
    fi
    # fd stays open: the SHARED lease lives until the phase shell exits.
}

# Shared generation lock, held (via fd inheritance) from backend
# classification through the driver handoff, so another fcvm command cannot
# replace $TAG between the hugepage check and the measured run (codex P1,
# PR #815): fcvm snapshot create/delete take this lock exclusive.
acquire_generation_lock() {
    local gen_lock="$DATA_ROOT/snapshots/$TAG.lock"
    mkdir -p "$(dirname "$gen_lock")" 2>/dev/null || true
    touch "$gen_lock" 2>/dev/null || true
    if [ -z "${REQBENCH_GEN_LOCK_FD:-}" ]; then
        exec {REQBENCH_GEN_LOCK_FD}<>"$gen_lock" || true
    fi
    if [ -z "${REQBENCH_GEN_LOCK_FD:-}" ]; then
        log "generation lock unavailable at $gen_lock"
        return 1
    fi
    if ! flock -s -w "${HUGEPAGE_POOL_LOCK_WAIT:-60}" "$REQBENCH_GEN_LOCK_FD"; then
        log "generation lock busy for '$TAG'; refusing to classify a moving target"
        return 1
    fi
}

# Exclusive lock on the diag's OUTPUT DIRECTORY, taken before the first
# removal and held past the summary's atomic rename. Every record, trace and
# temporary file the phase writes is named from $RESULTS and the URL alone, so
# two invocations over one RESULTS write the same paths; the generation lock
# does not serialize them, because two TAGs are two different locks. Unlocked,
# one phase removes the record the other is about to render into and a summary
# naming its own generation counts the other snapshot's renders. Bounded wait
# then refusal, the shape acquire_generation_lock and scripts/hugepage-pool-lock.sh
# use: a diag that cannot own the directory removes nothing, starts no serve
# and writes no summary.
acquire_diag_lock() {
    local lock="$RESULTS/diag/.lock" wait_s="${DIAG_LOCK_WAIT:-${HUGEPAGE_POOL_LOCK_WAIT:-60}}"
    mkdir -p "$RESULTS/diag" || { log "diag: cannot create $RESULTS/diag"; return 1; }
    touch "$lock" 2>/dev/null || true
    if [ -z "${REQBENCH_DIAG_LOCK_FD:-}" ]; then
        exec {REQBENCH_DIAG_LOCK_FD}<>"$lock" || true
    fi
    if [ -z "${REQBENCH_DIAG_LOCK_FD:-}" ]; then
        log "diag: output lock unavailable at $lock"
        return 1
    fi
    if ! flock -x -w "$wait_s" "$REQBENCH_DIAG_LOCK_FD"; then
        log "diag: another diag owns $RESULTS/diag (waited ${wait_s}s); its records, traces and summary share these paths, so this one refuses rather than interleaving with it"
        return 1
    fi
}

# The installed snapshot's guest size; falls back to ambient MEM when the
# config predates the field.
snapshot_memory_mib() {
    local v
    v=$($SUDO jq -er '.metadata.memory_mib' \
        "$DATA_ROOT/snapshots/$TAG/config.json" 2>/dev/null) || v="$MEM"
    echo "$v"
}

cmd_run() {
    # Backend/pool sanity BEFORE the quiet gate: these are configuration
    # errors, not load conditions. A hugepage snapshot cannot be restored
    # file-backed — fcvm silently starts a UFFD server and the record would
    # say backend=file, so the analyzer would gate MISLABELED data. unknown
    # (unreadable config.json) refuses too: fail closed, never guess.
    acquire_generation_lock || return 1
    local huge_state
    huge_state=$(hugepage_snapshot_state)
    if [ "$BACKEND" = file ] && [ "$huge_state" != normal ]; then
        log "run: BACKEND=file refused: snapshot '$TAG' hugepage state is '$huge_state' (a hugepage snapshot restores via an implicit UFFD server and the record would be mislabeled); use BACKEND=uffd or a non-hugepage TAG"
        return 2
    fi
    if [ "$huge_state" = huge ]; then
        ensure_hugepage_pool "$(snapshot_memory_mib)" || return 1
    fi
    if [ -n "${REQBENCH_DRIVER_HOOK:-}" ]; then
        # Test seam: runs inside the held generation + pool locks, exactly
        # where the driver handoff happens.
        "$REQBENCH_DRIVER_HOOK"
        return $?
    fi
    local guard_rc=0
    guard_quiet || guard_rc=$?
    if [ "$guard_rc" -ne 0 ]; then
        log "FATAL: no measurements were taken because the quiet-host guard refused the run"
        return "$guard_rc"
    fi
    local rc=0
    local backend_args=()
    # --prewire only for PREWIRE=1: ${PREWIRE:+--prewire} treats the ordinary
    # false-like PREWIRE=0 as ON, silently changing the measured operation.
    local prewire_args=()
    [ "${PREWIRE:-0}" = "1" ] && prewire_args=(--prewire)
    local image_id provenance="$DATA_ROOT/snapshots/$TAG/reqbench-provenance.json"
    image_id=$($SUDO jq -er '.image_id | select(type == "string")' "$provenance") \
        || { log "run: cannot read immutable image identity from $provenance"; return 1; }
    [[ "$image_id" =~ ^sha256:[0-9a-f]{64}$ ]] \
        || { log "run: invalid benchmark image ID $image_id"; return 1; }
    case "$BACKEND" in
        uffd)
            log "run: BACKEND=uffd — starting serve for $TAG (mode=$UFFD_MODE prefetch=$UFFD_PREFETCH)"
            start_serve run "$RESULTS/logs/serve.log" \
                --uffd-mode "$UFFD_MODE" --uffd-prefetch "$UFFD_PREFETCH" || return 1
            log "run: serve pid $SERVE_PID -> reqbench.py"
            backend_args=(--serve-pid "$SERVE_PID")
            ;;
        file)
            # No serve at all: clones restore MAP_PRIVATE from the snapshot files.
            log "run: BACKEND=file — no UFFD serve, restoring from $TAG directly"
            backend_args=(--snapshot-tag "$TAG")
            ;;
        *)
            log "run: unknown BACKEND=$BACKEND (want uffd|file)"; return 2 ;;
    esac
    # Guarded: unguarded, ANY non-zero exit from reqbench.py (including its
    # exit 4 when a teardown leaves a survivor) exits the shell under `set -e`
    # before the kill below, leaking the serve into the next phase.
    $SUDO env RUST_LOG="$FCVM_LOG" REQBENCH_QUIET_GUARD=1 \
        REQBENCH_RUNTIME_BUNDLE="${REQBENCH_RUNTIME_BUNDLE:-}" \
        REQBENCH_SOURCE_REVISION="${REQBENCH_SOURCE_REVISION:-}" \
        REQBENCH_GUARD_LOADAVG1="$QUIET_GUARD_LOADAVG1" \
        REQBENCH_GUARD_VM_PROCESSES="$QUIET_GUARD_VM_PROCESSES" \
        REQBENCH_QUIET_LOADAVG1_LIMIT="$QUIET_LOADAVG1_LIMIT" \
        ALLOW_BUSY="${ALLOW_BUSY:-0}" python3 "$HERE/reqbench.py" \
        "${backend_args[@]}" --url "$URL" \
        --out-dir "$RESULTS" --reps "${REPS:-10}" --warmup "${WARMUP:-2}" \
        --cdp-port "$CDP_PORT" --fcvm "$FCVM" --rust-log "$FCVM_LOG" \
        --image "$IMAGE" --image-id "$image_id" --snapshot-name "$TAG" \
        --data-root "$DATA_ROOT" --state-dir "$STATE_DIR" \
        --network-mode "$NETMODE" --cpu "$CPU" --memory-mib "$MEM" \
        --run-id "$RUNID" --arms "${ARMS:-exec,cdp,cdp-fast,noop}" \
        --engine "$ENGINE" \
        "${prewire_args[@]}" &
    local driver_bg=$!
    track "$driver_bg"
    ACTIVE_DRIVER_BG="$driver_bg"
    if wait "$driver_bg"; then
        rc=0
    else
        rc=$?
    fi
    untrack "$driver_bg"
    ACTIVE_DRIVER_BG=""
    stop_serve run || rc=1
    verify_runtime_bundle || rc=1
    # A completed driver is not automatically a publishable run: request-level
    # failures and an under-sized backend arm are recorded in JSONL rather than
    # necessarily making the producer exit non-zero. Make the analyzer part of
    # the run contract and propagate its gate status.
    if [ "$rc" -eq 0 ]; then
        log "run: applying publication gates"
        apply_publication_gates || rc=$?
    fi
    log "run: results in $RESULTS (backend=$BACKEND, gated run exit $rc)"
    return $rc
}

apply_publication_gates() {
    local -a stall_args=()
    [ -z "$STALL_MAX_MS" ] || stall_args=(--stall-max-ms "$STALL_MAX_MS")
    $SUDO python3 "$HERE/reqanalyze.py" --json-out "$RESULTS/analysis.json" \
        "${stall_args[@]}" "$RESULTS/reqbench.jsonl"
}

# ---------------------------------------------------------------------------
# DIAG. What holds a page's load event inside a restored clone, on the golden
# the run uses and with the run's serve setup, without a measured arm. One
# clone per (URL, rep): clone, one render, teardown. On Chromium the render
# carries cdpdrive's --net-trace (Network.* rows for the navigation), which
# the measured arms never send, so this is its own phase rather than a knob on
# the run. Everything lands in $RESULTS/diag/: <stem>-<rep>.json (the render
# record), <stem>-<rep>.trace.json (Chromium only) and summary.json, where
# <stem> is the URL's host for a root URL and host plus path otherwise (see
# diag_stem).
#
# The phase exits non-zero, and summary.json says passed=false, on any remote
# IP outside DIAG_EXPECT_IPS, a trace that names no remote address at all
# while DIAG_EXPECT_IPS is set, any net::ERR_NAME_NOT_RESOLVED in a trace,
# any load event over DIAG_MAX_LOAD_MS, any failed render (a record that is
# not this URL's, does not say ok, was written under a non-zero driver exit
# status, or timed no load event), any clone or serve whose teardown was not
# clean, and a sealed runtime bundle that changed during the phase. It
# refuses outright, before removing or writing anything, when another diag
# holds $RESULTS/diag.

# The record stem for a URL: scheme and trailing slashes dropped, every run of
# characters outside [A-Za-z0-9._-] folded to one '-'. A root URL's stem is
# its host; the corpus has five todomvc.com pages, so the path is part of it.
diag_stem() {
    printf '%s\n' "$1" | sed -E 's#^[a-z]+://##; s#/+$##; s#[^A-Za-z0-9._-]+#-#g'
}

# A record for a render that never reached the driver, so summary.json never
# has to guess what a missing record means. $1 = record path, $2 = url,
# $3 = error text, $4 = stage.
diag_failed_record() {
    local record="$1"
    jq -n --arg url "$2" --arg err "$3" --arg stage "$4" \
        '{ok: false, url: $url, error: $err, stage: $stage, driver_status: null}' >"$record.tmp" \
        && mv -f "$record.tmp" "$record"
}

# One render against a clone. $1 = url, $2 = record path, $3 = trace path,
# $4 = clone pid, $5 = clone host-side ip. The record is written whatever the
# driver did: its own, with the driver's exit status added as driver_status,
# when it printed a JSON object; one naming the exit status otherwise. The
# summary holds a render to that status, not to the record's ok alone:
# cdpdrive exits 1 with a record saying ok when only the trace write failed.
# Returns the driver's status.
diag_render() {
    local url="$1" record="$2" trace="$3" cpid="$4" ip="$5"
    local tmp="$record.tmp" status=0
    if [ "$ENGINE" = webkit ]; then
        local wd_session
        wd_session=$(wd_session_id "$cpid")
        if [ -z "$wd_session" ]; then
            diag_failed_record "$record" "$url" "no baked session id in the clone" session
            return 1
        fi
        # argv[1] names the driver so a process listing (and the test stub on
        # PATH) can tell this render from the summary writer, which is also a
        # `python3 -` heredoc. wddrive.drive is the same call the measured
        # webkit arm makes; the record is its return value.
        REQBENCH_HERE="$HERE" python3 - wddrive "$ip:$CDP_PORT" "$url" "$wd_session" "$tmp" \
            2>>"$RESULTS/logs/diag-render.log" <<'PYWD' || status=$?
import argparse, json, os, sys
sys.path.insert(0, os.environ["REQBENCH_HERE"])
import wddrive
_driver, host, url, session, record_path = sys.argv[1:6]
record = wddrive.drive(argparse.Namespace(cdp_host=host, url=url, timeout=120.0,
                                          session_id=session, out_prefix=""))
with open(record_path, "w") as target:
    json.dump(record, target, separators=(",", ":"))
    target.write("\n")
sys.exit(0 if record.get("ok") else 1)
PYWD
    else
        python3 "$HERE/cdpdrive.py" "$ip:$CDP_PORT" "$url" --format jpeg --timeout 120 \
            --net-trace "$trace" >"$tmp" 2>>"$RESULTS/logs/diag-render.log" || status=$?
    fi
    if jq -e 'type == "object"' "$tmp" >/dev/null 2>&1; then
        if jq --argjson status "$status" '. + {driver_status: $status}' "$tmp" >"$tmp.status" \
            && mv -f "$tmp.status" "$record"; then
            rm -f "$tmp"
        else
            rm -f "$tmp" "$tmp.status"
            diag_failed_record "$record" "$url" "driver exited $status; its record could not be kept" driver
        fi
    else
        rm -f "$tmp"
        diag_failed_record "$record" "$url" "driver exited $status without a record" driver
    fi
    return "$status"
}

# Reads every record and trace the loop left in $RESULTS/diag, writes
# summary.json whole (tmp + rename) and returns 1 when it says passed=false
# or when the snapshot under $TAG cannot be identified (then nothing is
# written). $1 = clone/serve teardowns that were not clean, $2 = 1 when the
# sealed runtime bundle was still intact at the end of the phase, then
# url/stem pairs. The summary names the snapshot generation and config it
# diagnosed, read under the generation lock this phase holds, and the sealed
# runtime bundle it ran from, so campaign_summary can bind it to a run
# measured on the same generation from the same code.
diag_write_summary() {
    local teardown_failures="$1" bundle_ok="$2"
    shift 2
    # The serve this phase started ran with replay off (cmd_diag), and the
    # summary says so for campaign_summary to hold it to. On the file backend
    # the mode is "file", the value reqbench.py records in the run meta (so
    # campaign_summary can bind the two), and there was no serve to carry
    # the prefetch knob, which is recorded as null.
    local mode="$UFFD_MODE" prefetch=off
    [ "$BACKEND" = uffd ] || { mode="file"; prefetch=""; }
    local cfg="$DATA_ROOT/snapshots/$TAG/config.json" generation config_sha
    generation=$($SUDO jq -er '.generation_id' "$cfg" 2>/dev/null) \
        || { log "diag: cannot read generation_id from $cfg"; return 1; }
    config_sha=$($SUDO sha256sum "$cfg" 2>/dev/null | cut -d' ' -f1)
    [ -n "$config_sha" ] || { log "diag: cannot hash $cfg"; return 1; }
    # The sealed runtime that rendered these pages, named the way the measured
    # run names it: reqbench.py stamps the sha256 of the staged bundle's
    # MANIFEST.sha256 into every record's meta as runtime_bundle_sha256, and
    # reqanalyze carries it into the cell's seal. runtime_bundle_intact says
    # only that the bundle did not change under this phase, so without the
    # hash a later standalone diag, staged from edited sources, overwrites
    # this summary and still binds to the run. Empty (recorded as null) when
    # the phase ran outside a staged bundle; campaign_summary refuses that
    # rather than reading it as a run's evidence.
    local bundle_sha=""
    if [ -n "${REQBENCH_RUNTIME_BUNDLE:-}" ]; then
        bundle_sha=$(sha256sum "$REQBENCH_RUNTIME_BUNDLE/MANIFEST.sha256" 2>/dev/null | cut -d' ' -f1)
        [ -n "$bundle_sha" ] || {
            log "diag: cannot hash the sealed runtime manifest $REQBENCH_RUNTIME_BUNDLE/MANIFEST.sha256"
            return 1
        }
    fi
    python3 - "$RESULTS/diag" "$ENGINE" "$TAG" "$BACKEND" "$mode" "$prefetch" \
        "$DIAG_REPS" "$DIAG_EXPECT_IPS" "$DIAG_MAX_LOAD_MS" "$teardown_failures" \
        "$bundle_ok" "$generation" "$config_sha" "$bundle_sha" "$@" <<'PY'
import json, os, sys, tempfile, time, uuid
from collections import Counter

(diag_dir, engine, tag, backend, uffd_mode, uffd_prefetch, reps_raw,
 expect_raw, max_load_raw, teardown_raw, bundle_raw, generation_id,
 config_sha256, bundle_sha256) = sys.argv[1:15]
pairs = sys.argv[15:]
reps = int(reps_raw)
expect_ips = [ip for ip in expect_raw.split(",") if ip] or None
max_load_ms = int(max_load_raw) if max_load_raw else None
teardown_failures = int(teardown_raw)
bundle_intact = bundle_raw == "1"
try:
    canonical = str(uuid.UUID(generation_id))
except (TypeError, ValueError):
    canonical = None
if canonical != generation_id:
    raise SystemExit(f"snapshot {tag} has non-canonical generation_id {generation_id!r}")
if len(config_sha256) != 64 or set(config_sha256) - set("0123456789abcdef"):
    raise SystemExit(f"snapshot {tag} config digest is not a sha256: {config_sha256!r}")
if bundle_sha256 and (len(bundle_sha256) != 64
                      or set(bundle_sha256) - set("0123456789abcdef")):
    raise SystemExit(f"runtime bundle digest is not a sha256: {bundle_sha256!r}")
# Chromium's load event is timed from the navigate command's response;
# WebDriver's navigate returns after the classic load event, so its round
# trip is the same question.
load_key = "navigate_ms" if engine == "webkit" else "navigate_load_event_ms"


def read_json(path):
    """(value, None), or (None, why) when there is nothing to read."""
    try:
        with open(path) as source:
            return json.load(source), None
    except FileNotFoundError:
        return None, "is missing"
    except (OSError, ValueError) as error:
        return None, f"cannot be read ({type(error).__name__}: {error})"


def number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def by_count(item):
    return (-item[1], item[0])


urls = {}
violations = []
for i in range(0, len(pairs), 2):
    url, stem = pairs[i], pairs[i + 1]
    remote_ips = Counter()
    errors = Counter()
    renders_ok = 0
    max_load = None
    max_pending = None
    for rep in range(1, reps + 1):
        def violation(kind, detail, rep=rep):
            violations.append({"url": url, "rep": rep, "kind": kind, "detail": detail})

        base = os.path.join(diag_dir, f"{stem}-{rep}")
        record, why = read_json(base + ".json")
        # A render counts only when the record is this URL's, says ok, was
        # written under a zero driver exit status and timed the load event.
        rendered = True
        if not isinstance(record, dict):
            violation("render_failed", f"record {base}.json {why or 'is not a JSON object'}")
            record = {}
            rendered = False
        elif record.get("url") != url:
            violation("render_failed",
                      f"record {base}.json is for {record.get('url')!r}, not this url")
            rendered = False
        elif record.get("ok") is not True:
            violation("render_failed",
                      f"{record.get('error') or 'ok is not true'} "
                      f"(stage {record.get('stage') or 'unknown'})")
            rendered = False
        elif record.get("driver_status") != 0:
            violation("render_failed",
                      f"driver exited {record.get('driver_status')!r} with a record saying ok")
            rendered = False
        stages = record.get("stages")
        load = stages.get(load_key) if isinstance(stages, dict) else None
        if number(load):
            max_load = load if max_load is None else max(max_load, load)
            if max_load_ms is not None and load > max_load_ms:
                violation("stall", f"{load_key}={load:.0f} ms exceeds DIAG_MAX_LOAD_MS={max_load_ms}")
        elif rendered:
            violation("render_failed", f"no {load_key} in the record's stages")
            rendered = False
        if rendered:
            renders_ok += 1
        if engine == "webkit":
            continue
        trace, why = read_json(base + ".trace.json")
        rows = trace.get("requests") if isinstance(trace, dict) else None
        if not isinstance(rows, list):
            violation("render_failed",
                      f"trace {base}.trace.json {why or 'has no requests list'}")
            continue
        pending = 0
        foreign = {}
        unresolved = []
        unaddressed = []
        addressed = 0
        for row in rows:
            if not isinstance(row, dict):
                continue
            ip = row.get("remote_ip") or ""
            if ip:
                addressed += 1
                remote_ips[ip] += 1
                if expect_ips is not None and ip not in expect_ips:
                    foreign.setdefault(ip, []).append(row.get("url", ""))
            elif str(row.get("url", "")).split(":", 1)[0].lower() in ("http", "https") \
                    and not row.get("from_cache") and not row.get("from_service_worker"):
                # No address and nothing that explains one: the request
                # failed, or was still open when the post-load drain ended.
                # Either way the trace does not say where it went, which is
                # the question the expectation asks of every request.
                unaddressed.append(row.get("url", ""))
            if row.get("failed"):
                text = row.get("error_text") or "<no errorText>"
                errors[text] += 1
                if "ERR_NAME_NOT_RESOLVED" in text:
                    unresolved.append(row.get("url", ""))
            if row.get("pending_at_load"):
                pending += 1
        max_pending = pending if max_pending is None else max(max_pending, pending)
        for ip, hit in sorted(foreign.items()):
            violation("remote_ip", f"{ip} served {len(hit)} request(s), first {hit[0]}")
        if expect_ips is not None and addressed == 0:
            # Rows without an address prove nothing about where the page
            # came from; an expectation nothing was held to is not met.
            violation("no_remote_ip",
                      f"none of the {len(rows)} traced request(s) names a remote address")
        elif expect_ips is not None and unaddressed:
            # `addressed > 0` cleared the whole rep on one request: an
            # allowed main document plus a subresource that failed, or that
            # was still open when the drain ended, met the expectation
            # without either being held to it. A cache or service-worker hit
            # names no address either and had no network hop to name one;
            # the trace marks those rows and they are the only exemption.
            violation("no_remote_ip",
                      f"{len(unaddressed)} of {len(rows)} traced request(s) name no "
                      f"remote address, first {unaddressed[0]}")
        if unresolved:
            violation("name_not_resolved",
                      f"{len(unresolved)} request(s) failed to resolve, first {unresolved[0]}")
    urls[url] = {
        "reps": reps,
        "renders_ok": renders_ok,
        "max_load_ms": max_load,
        "max_pending_at_load": max_pending,
        "remote_ips": dict(sorted(remote_ips.items(), key=by_count)),
        "errors": dict(sorted(errors.items(), key=by_count)),
    }

passed = not violations and teardown_failures == 0 and bundle_intact
summary = {
    "engine": engine,
    "tag": tag,
    "backend": backend,
    "uffd_mode": uffd_mode or None,
    "uffd_prefetch": uffd_prefetch or None,
    "snapshot_generation_id": generation_id,
    "snapshot_config_sha256": config_sha256,
    "runtime_bundle_intact": bundle_intact,
    "runtime_bundle_sha256": bundle_sha256 or None,
    "reps": reps,
    "urls": urls,
    "violations": violations,
    "teardown_failures": teardown_failures,
    "passed": passed,
    "limits": {"expect_ips": expect_ips, "max_load_ms": max_load_ms},
    "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
}
fd, temporary = tempfile.mkstemp(prefix=".summary.", dir=diag_dir)
try:
    with os.fdopen(fd, "w") as target:
        json.dump(summary, target, indent=2, sort_keys=True)
        target.write("\n")
        target.flush()
        os.fsync(target.fileno())
    os.replace(temporary, os.path.join(diag_dir, "summary.json"))
except BaseException:
    try:
        os.unlink(temporary)
    except FileNotFoundError:
        pass
    raise
for url, data in urls.items():
    print(f"  {url}: {data['renders_ok']}/{reps} ok, max load {data['max_load_ms']} ms, "
          f"max pending at load {data['max_pending_at_load']}, "
          f"remote ips {data['remote_ips']}, errors {data['errors']}")
for entry in violations:
    print(f"  VIOLATION {entry['kind']} {entry['url']} rep {entry['rep']}: {entry['detail']}")
if teardown_failures:
    print(f"  {teardown_failures} teardown(s) were not clean")
if not bundle_intact:
    print("  the sealed runtime bundle changed during the phase")
sys.exit(0 if passed else 1)
PY
}

cmd_diag() {
    # The output directory first, before anything under it is removed: this
    # phase is the only writer of $RESULTS/diag from here to the summary's
    # rename. Refusing (exit 3, as the TAG lock does) rather than queueing
    # keeps a second invocation out of a directory whose paths it shares.
    acquire_diag_lock || return 3
    # Temporary records an invocation that died mid-render left behind.
    # diag_render promotes a $record.tmp that parses as an object into the
    # record, so an abandoned one is a record waiting to be adopted; they go
    # here, under the lock, before any render can read them.
    rm -f "$RESULTS"/diag/*.tmp "$RESULTS"/diag/*.tmp.status
    # summary.json is the verdict of the latest diag over this RESULTS. Gone
    # before anything can fail, so a diag that ends without writing its own
    # (a refused knob, a serve or clone that never came up) leaves no earlier
    # passed=true summary for campaign_summary to read as this one's.
    rm -f "$RESULTS/diag/summary.json"
    # The knobs first: a refused request must not have taken the generation
    # lock or grown the host's hugepage pool (gigabytes, host-global,
    # persistent) on its way to the refusal.
    [[ "$DIAG_REPS" =~ ^[1-9][0-9]*$ ]] \
        || { log "diag: DIAG_REPS must be a positive integer (got '$DIAG_REPS')"; return 2; }
    if [ -n "$DIAG_MAX_LOAD_MS" ] && [[ ! "$DIAG_MAX_LOAD_MS" =~ ^[1-9][0-9]*$ ]]; then
        log "diag: DIAG_MAX_LOAD_MS must be a positive integer of milliseconds (got '$DIAG_MAX_LOAD_MS')"
        return 2
    fi
    case "$BACKEND" in
        uffd|file) ;;
        *) log "diag: unknown BACKEND=$BACKEND (want uffd|file)"; return 2 ;;
    esac
    # The diag's serve never records a working set. With replay on, the UFFD
    # server unions every clone's faults into memory.bin.working-set beside
    # the golden, and the measured run that follows replays that file: its
    # restores would carry the working set of the diag's renders (every URL,
    # DIAG_REPS clones each) instead of the golden's own, and a fresh golden
    # would measure like a reused one. off opens no store
    # (src/uffd/server.rs, Prefetch::Off): nothing recorded, nothing
    # replayed, no file. A request for anything else is refused rather than
    # served with a knob other than the one asked for.
    if [ -n "$UFFD_PREFETCH_REQUESTED" ] && [ "$UFFD_PREFETCH_REQUESTED" != off ]; then
        log "diag: UFFD_PREFETCH=$UFFD_PREFETCH_REQUESTED refused: the diag serves with --uffd-prefetch off so its renders are not recorded into the golden's working-set sidecar; unset it or pass off"
        return 2
    fi
    # Only the Chromium render carries a network trace; an IP expectation
    # the WebKit phase cannot check is refused rather than passed by default.
    if [ "$ENGINE" = webkit ] && [ -n "$DIAG_EXPECT_IPS" ]; then
        log "diag: DIAG_EXPECT_IPS cannot be checked on webkit (its render carries no network trace); unset it"
        return 2
    fi
    # Name every record before starting anything: two URLs that fold to one
    # stem would overwrite each other's records and the summary would read
    # one page's trace as the other's.
    local -a urls=() pairs=()
    local -A stems=()
    local url stem
    IFS=',' read -ra urls <<<"${DIAG_URLS:-$URL}"
    for url in "${urls[@]}"; do
        [ -n "$url" ] || { log "diag: DIAG_URLS has an empty entry"; return 2; }
        stem=$(diag_stem "$url")
        [ -n "$stem" ] || { log "diag: cannot name a record for '$url'"; return 2; }
        if [ -n "${stems[$stem]:-}" ]; then
            log "diag: '$url' and '${stems[$stem]}' would share the record name $stem"
            return 2
        fi
        stems[$stem]="$url"
        pairs+=("$url" "$stem")
    done
    # The run's own preamble: backend and pool sanity under the generation
    # lock, for the reasons cmd_run gives. No quiet-host guard, nothing here
    # is measured.
    acquire_generation_lock || return 1
    local huge_state
    huge_state=$(hugepage_snapshot_state)
    if [ "$huge_state" = unknown ]; then
        log "diag: snapshot '$TAG' hugepage state is 'unknown' (config.json unreadable or jq missing); refusing to diagnose a snapshot it cannot classify"
        return 1
    fi
    if [ "$BACKEND" = file ] && [ "$huge_state" != normal ]; then
        log "diag: BACKEND=file refused: snapshot '$TAG' hugepage state is '$huge_state' (a hugepage snapshot restores via an implicit UFFD server and the record would be mislabeled); use BACKEND=uffd or a non-hugepage TAG"
        return 2
    fi
    if [ "$huge_state" = huge ]; then
        ensure_hugepage_pool "$(snapshot_memory_mib)" || return 1
    fi
    mkdir -p "$RESULTS/diag" "$RESULTS/logs"
    local rc=0 teardown_failures=0
    if [ "$BACKEND" = uffd ]; then
        log "diag: BACKEND=uffd, starting serve for $TAG (mode=$UFFD_MODE prefetch=off: nothing recorded into the golden's working set)"
        start_serve diag "$RESULTS/logs/diag-serve.log" \
            --uffd-mode "$UFFD_MODE" --uffd-prefetch off || return 1
    else
        log "diag: BACKEND=file, no UFFD serve, restoring from $TAG directly"
    fi
    log "diag: ${#urls[@]} url(s) x $DIAG_REPS clone(s), engine=$ENGINE, expect_ips=${DIAG_EXPECT_IPS:-<any>}, max_load_ms=${DIAG_MAX_LOAD_MS:-<none>}"
    local i rep n=0 base record trace cname
    for ((i = 0; i < ${#pairs[@]}; i += 2)); do
        url="${pairs[i]}"
        stem="${pairs[i + 1]}"
        for ((rep = 1; rep <= DIAG_REPS; rep++)); do
            n=$((n + 1))
            base="$RESULTS/diag/$stem-$rep"
            record="$base.json"
            trace="$base.trace.json"
            cname="cb-req-diag-$n-$RUNID"
            rm -f "$record" "$record.tmp" "$trace"
            if start_clone "$SERVE_PID" "$cname" "$RESULTS/logs/diag-clone-$stem-$rep.log"; then
                if [ -n "$CLONE_IP" ]; then
                    if diag_render "$url" "$record" "$trace" "$CLONE_PID" "$CLONE_IP"; then
                        log "diag: $url rep $rep rendered ($record)"
                    else
                        log "diag: $url rep $rep render FAILED ($record)"
                    fi
                else
                    diag_failed_record "$record" "$url" "clone has no host-side IP" clone
                fi
            else
                diag_failed_record "$record" "$url" "clone never registered" clone
            fi
            # This clone's teardown, whatever the render did: the summary
            # below counts a teardown that needed SIGKILL, survived it, or
            # left state or disk behind as a failure of the phase.
            if [ -n "$CLONE_PID" ]; then
                stop_tracked "$CLONE_PID" \
                    || { log "diag: clone $cname required SIGKILL"; teardown_failures=$((teardown_failures + 1)); }
            fi
            if [ -n "$CLONE_BG" ]; then
                stop_tracked "$CLONE_BG" \
                    || { log "diag: clone $cname required SIGKILL"; teardown_failures=$((teardown_failures + 1)); }
            fi
            if [ -n "$CLONE_VM_ID" ]; then
                assert_vm_artifacts_absent "$CLONE_VM_ID" || teardown_failures=$((teardown_failures + 1))
            fi
        done
    done
    stop_serve diag || teardown_failures=$((teardown_failures + 1))
    # The bundle verdict is part of the summary, not only of the exit
    # status: a consumer reading summary.json alone must see the phase's
    # refusal too.
    local bundle_ok=1
    verify_runtime_bundle || { bundle_ok=0; rc=1; }
    if diag_write_summary "$teardown_failures" "$bundle_ok" "${pairs[@]}"; then
        log "diag: passed ($RESULTS/diag/summary.json)"
    else
        rc=1
        log "diag: FAILED, see $RESULTS/diag/summary.json"
    fi
    return $rc
}

# Only dispatch when EXECUTED. Sourcing the file makes its helpers unit-testable
# (see ReqbenchShell in test_reqbench.py) instead of reachable only through a
# whole phase.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    verify_runtime_bundle
    case "${1:-}" in
        build)  cmd_build ;;
        golden) cmd_golden ;;
        verify) cmd_verify ;;
        run)    cmd_run ;;
        diag)   cmd_diag ;;
        all)
            # The chain's own build/golden/verify phases are the load the run
            # gate reads a minute later; default the settle window so a cold
            # one-shot does not refuse on its own wake. An explicit
            # SETTLE_WAIT_SECS wins.
            export SETTLE_WAIT_SECS="${SETTLE_WAIT_SECS:-120}"
            cmd_build; cmd_golden; cmd_verify; cmd_run ;;
        *) echo "usage: $0 {build|golden|verify|run|diag|all}" >&2; exit 2 ;;
    esac
fi
