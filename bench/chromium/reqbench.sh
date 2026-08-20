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

guard_quiet() {
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
# ID it published here; cmd_golden refuses a tag that resolves elsewhere.
built_image_id_file() {
    printf '%s/reqbench-locks/built-image-%s.id\n' \
        "$DATA_ROOT" "$(printf '%s' "$IMAGE" | tr '/:' '__')"
}

cmd_build() {
    log "building $IMAGE"
    # The bundle sealed this invocation's request code at process start, but
    # the image build COPYs from the live repository. An edit in between puts
    # different render bytes in the exec arm (image copy) than in the CDP arm
    # (bundle copy) while every seal check passes: the staging guard compares
    # git HEAD only, which cannot see an uncommitted edit.
    if [ -n "${REQBENCH_RUNTIME_BUNDLE:-}" ]; then
        local sealed_source
        for sealed_source in render.py cdpdrive.py wddrive.py; do
            cmp -s "$REPO/bench/chromium/$sealed_source" \
                "$REQBENCH_RUNTIME_BUNDLE/$sealed_source" \
                || { log "FATAL: $sealed_source in $REPO/bench/chromium diverged from the sealed runtime bundle; the image would bake different bytes than this run executes"; return 1; }
        done
    fi
    # --format docker is LOAD-BEARING: podman's default OCI format DROPS
    # HEALTHCHECK with only a warning, and fcvm's health gate is what
    # triggers the golden snapshot (src/health.rs AND-logic).
    podman build --format docker -t "$IMAGE" -f "$REPO/$CONTAINERFILE" "$REPO"
    # The warm gate lives or dies here. fcvm treats a MISSING healthcheck as a
    # PASS, so an image that lost it snapshots a COLD browser and every clone's
    # "warm" latency is really a first-paint number. Assert the healthcheck
    # exists AND is ours: health_state.sh, which reports healthy only for a
    # FRESH verdict from the resident checker, which in turn requires the warm
    # marker that entry.sh touches only after a full navigate + screenshot.
    podman inspect "$IMAGE" --format '{{json .HealthCheck}}' | grep -q health_state \
        || { log "FATAL: image has no HEALTHCHECK naming health_state.sh (OCI format drop, or the Containerfile changed without this check)"; return 1; }
    # Record the ID this build published so cmd_golden, which runs as a
    # separate process with the TAG lock released in between, can refuse a
    # tag another worktree repointed in that window.
    local built_id id_file id_tmp
    built_id=$(podman image inspect --format '{{.Id}}' "$IMAGE") \
        || { log "FATAL: cannot read the image ID podman published for $IMAGE"; return 1; }
    built_id="sha256:${built_id#sha256:}"
    [[ "$built_id" =~ ^sha256:[0-9a-f]{64}$ ]] \
        || { log "FATAL: podman reported invalid image ID '$built_id' for $IMAGE"; return 1; }
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
        built_image_id=$(tr -d '[:space:]' <"$built_id_file")
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
        "${REQBENCH_SOURCE_REVISION:-}" <<'PY'
import fcntl, hashlib, json, os, sys, tempfile, uuid
(
    config_path, output_path, lock_path, image_label, image_id, image_digest,
    image_cache_key, built_image_id, prepared_generation, prepared_digest,
    fcvm_sha256, runtime_bundle_sha256, source_revision,
) = sys.argv[1:]
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
record = {
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

# Start one clone against the running serve. Sets CLONE_PID and CLONE_IP.
# Factored out so verify can start TWO of them: a single clone's target id
# cannot answer a question about stability ACROSS clones.
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
    $SUDO env RUST_LOG=$FCVM_LOG "$FCVM" snapshot run --pid "$spid" --name "$cname" \
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
    local sf="$RESULTS/logs/verify-serve.log"
    $SUDO "$FCVM" snapshot serve "$TAG" >"$sf" 2>&1 &
    local serve_bg=$!
    track "$serve_bg"
    local t0=$SECONDS
    until grep -q "Waiting for VMs" "$sf" 2>/dev/null; do
        [ $((SECONDS-t0)) -lt 60 ] || { log "verify: serve never came up"; cat "$sf" >&2; return 1; }
        sleep 0.5
    done
    local spid; spid=$(grep -oP 'Serve PID: \K[0-9]+' "$sf" | head -1)
    track "$spid"
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
    stop_tracked "$spid" \
        || { log "verify: serve required SIGKILL"; fail=1; }
    stop_tracked "$serve_bg" \
        || { log "verify: serve required SIGKILL"; fail=1; }
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
    local pool_lock="$DATA_ROOT/hugepage-pool.lock"
    mkdir -p "$DATA_ROOT" 2>/dev/null || true
    touch "$pool_lock" 2>/dev/null || true
    if [ -z "${REQBENCH_POOL_LOCK_FD:-}" ]; then
        exec {REQBENCH_POOL_LOCK_FD}<>"$pool_lock" || true
    fi
    if [ -z "${REQBENCH_POOL_LOCK_FD:-}" ]; then
        log "hugepages: pool lock unavailable at $pool_lock"
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
    local rc=0 spid="" serve_bg=""
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
            local sf="$RESULTS/logs/serve.log"
            $SUDO "$FCVM" snapshot serve "$TAG" --uffd-mode "$UFFD_MODE" \
                --uffd-prefetch "$UFFD_PREFETCH" >"$sf" 2>&1 &
            serve_bg=$!
            track "$serve_bg"
            local t0=$SECONDS
            until grep -q "Waiting for VMs" "$sf" 2>/dev/null; do
                [ $((SECONDS-t0)) -lt 60 ] || { log "run: serve never came up"; cat "$sf" >&2; return 1; }
                sleep 0.5
            done
            spid=$(grep -oP 'Serve PID: \K[0-9]+' "$sf" | head -1)
            [ -n "$spid" ] || { log "run: could not read Serve PID from $sf"; return 1; }
            track "$spid"
            log "run: serve pid $spid -> reqbench.py"
            backend_args=(--serve-pid "$spid")
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
    if [ -n "$spid" ]; then
        stop_tracked "$spid" \
            || { log "run: UFFD serve required SIGKILL"; rc=1; }
        stop_tracked "$serve_bg" \
            || { log "run: UFFD serve required SIGKILL"; rc=1; }
    fi
    verify_runtime_bundle || rc=1
    # A completed driver is not automatically a publishable run: request-level
    # failures and an under-sized backend arm are recorded in JSONL rather than
    # necessarily making the producer exit non-zero. Make the analyzer part of
    # the run contract and propagate its gate status.
    if [ "$rc" -eq 0 ]; then
        log "run: applying publication gates"
        $SUDO python3 "$HERE/reqanalyze.py" --json-out "$RESULTS/analysis.json" \
            "$RESULTS/reqbench.jsonl" || rc=$?
    fi
    log "run: results in $RESULTS (backend=$BACKEND, gated run exit $rc)"
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
        all)    cmd_build; cmd_golden; cmd_verify; cmd_run ;;
        *) echo "usage: $0 {build|golden|verify|run|all}" >&2; exit 2 ;;
    esac
fi
