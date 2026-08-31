#!/bin/bash
# Two measurements the corpus campaign does not make, against the SAME replay
# server, the SAME image and the SAME 14-URL corpus the campaign's VM arm runs:
#
#   hostcdp  the campaign's cdp arm with the VM removed: one warm host container,
#            driven by the same cdpdrive.py, over the same schedule.
#   memory   per-instance memory for fcvm clones and for host containers, both on
#            the same two bases (see corpus_mem.py).
#
# The replay wiring is the campaign's: corpus_serve.py owns 127.0.0.1 DNS 53 /
# HTTP 80 / HTTPS 443 and answers every name with --answer-ip, dnsmasq is stopped
# for the socket if it holds it and restored on the way out.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CORPUS_EXTRA_STAGED="${CORPUS_EXTRA_STAGED:-0}"
case "$CORPUS_EXTRA_STAGED" in
    0)
        REPO="${REPO:-$(cd "$HERE/../.." && pwd)}"
        REPO="$(cd "$REPO" && pwd -P)"
        SOURCE_BENCH="$REPO/bench/chromium"
        BENCH="$SOURCE_BENCH"
        ;;
    1)
        REPO="${CORPUS_EXTRA_SOURCE_REPO:?CORPUS_EXTRA_SOURCE_REPO is required for a staged run}"
        REPO="$(cd "$REPO" && pwd -P)"
        SOURCE_BENCH="${CORPUS_EXTRA_SOURCE_BENCH:?CORPUS_EXTRA_SOURCE_BENCH is required for a staged run}"
        BENCH="$HERE"
        ;;
    *)
        echo "BLOCKED: CORPUS_EXTRA_STAGED must be 0 or 1" >&2
        exit 2
        ;;
esac
STAMP="${STAMP:-$(date +%Y%m%d-%H%M%S)}"
RUN_ID="${RUN_ID:-$(tr -d - </proc/sys/kernel/random/uuid)}"
CONTAINER_OWNER_TOKEN="${CONTAINER_OWNER_TOKEN:-$(tr -d - </proc/sys/kernel/random/uuid)}"
RESULTS="${RESULTS:-$BENCH/results/corpusextra-$STAMP-$RUN_ID}"
LOGDIR="${LOGDIR:-/tmp/corpusextra-$STAMP-$RUN_ID}"
TAG="${TAG:-cb-req-corpus}"
IMAGE="${IMAGE:-localhost/chromium-bench-req}"
PHASES="${PHASES-hostcdp,memory}"
REPS="${REPS:-202}"          # measured reps; WARMUP is extra, matching the VM arm
WARMUP="${WARMUP:-28}"       # two full 14-URL cycles, the campaign's warmup
MEM_NS="${MEM_NS:-1,2,4,8}"
MEM_REPS="${MEM_REPS:-14}"
MEM_SEED="${MEM_SEED:-20260830}"
UFFD_MODE="${UFFD_MODE:-minor}"
UFFD_PREFETCH="${UFFD_PREFETCH:-on}"

# The campaign's 14 URLs, in the campaign's order. Copied from corpus_campaign.sh
# and checked against it below: a corpus that has drifted would make the host
# control a different workload from the VM arm it is a control for.
URLS="https://example.com/,https://news.ycombinator.com/,https://developers.cloudflare.com/,https://blog.cloudflare.com/,https://en.wikipedia.org/,https://developer.mozilla.org/en-US/,https://www.elmundo.es/,https://www.rtp.pt/noticias/,https://www.theguardian.com/international,https://todomvc.com/examples/javascript-es6/dist/,https://todomvc.com/examples/react/dist/index.html,https://todomvc.com/examples/vue/dist/,https://todomvc.com/examples/angular/dist/browser/,https://todomvc.com/examples/preact/dist/"

say() { printf '\n=== %s %s\n' "$(date +%H:%M:%S)" "$*"; }

canonical_runtime_image_id() {
    local raw="$1" digest
    case "$raw" in
        sha256:*) digest="${raw#sha256:}" ;;
        *) digest="$raw" ;;
    esac
    [[ "$digest" =~ ^[0-9a-f]{64}$ ]] || {
        echo "BLOCKED: image resolved to invalid identity '$raw'" >&2
        return 2
    }
    printf 'sha256:%s\n' "$digest"
}

replay_probe_logged() {
    local qname="$1" request_path="$2"
    python3 - "$RESULTS/corpus-dns.log" "$RESULTS/corpus-access.log" \
            "$qname" "$request_path" <<'PY'
import json
import sys

dns_path, access_path, qname, request_path = sys.argv[1:]

def records(path):
    try:
        with open(path) as handle:
            rows = [json.loads(line) for line in handle if line.strip()]
    except (OSError, ValueError):
        raise SystemExit(1)
    if any(not isinstance(row, dict) for row in rows):
        raise SystemExit(1)
    return rows

dns_ok = any(
    row.get("qname") == qname
    and row.get("qtype") == 1
    and row.get("answer") == "10.0.2.2"
    for row in records(dns_path)
)
access_ok = any(
    row.get("host") == "blog.cloudflare.com"
    and row.get("path") == request_path
    and row.get("status") == 200
    for row in records(access_path)
)
raise SystemExit(0 if dns_ok and access_ok else 1)
PY
}

validate_phases() {
    local remaining="$PHASES" phase seen=","
    [ -n "$remaining" ] || { echo "BLOCKED: PHASES names no phases" >&2; return 2; }
    while :; do
        phase="${remaining%%,*}"
        [ -n "$phase" ] || { echo "BLOCKED: PHASES contains an empty phase" >&2; return 2; }
        case "$phase" in
            hostcdp|memory) ;;
            *) echo "BLOCKED: unknown phase '$phase'" >&2; return 2 ;;
        esac
        case "$seen" in
            *",$phase,"*) echo "BLOCKED: PHASES repeats '$phase'" >&2; return 2 ;;
        esac
        seen="$seen$phase,"
        case "$remaining" in
            *,*) remaining="${remaining#*,}" ;;
            *) break ;;
        esac
    done
}

[[ "$RUN_ID" =~ ^[0-9a-f]{32}$ ]] \
    || { echo "BLOCKED: RUN_ID must be a 32-character lowercase hexadecimal owner ID" >&2; exit 2; }
[[ "$CONTAINER_OWNER_TOKEN" =~ ^[0-9a-f]{32}$ ]] \
    || { echo "BLOCKED: CONTAINER_OWNER_TOKEN must be 32 lowercase hexadecimal characters" >&2; exit 2; }
validate_phases

for tool in awk bash cat chmod cp curl cut date dig dirname env find flock git grep \
        head install kill mkdir mkfifo mktemp mv pgrep podman python3 rm rmdir sed seq \
        sha256sum sleep sort sudo systemctl tee timeout tr uname xargs; do
    command -v "$tool" >/dev/null 2>&1 || { echo "BLOCKED: '$tool' missing" >&2; exit 2; }
done

# DNS 53, HTTP 80, HTTPS 443 and dnsmasq are host-wide resources. Every UID
# therefore has to contend on the same inode before creating output or touching
# any of them. A root-owned directory can be opened read-only by both rootful
# and rootless callers. A regular file created by one caller cannot: Linux's
# fs.protected_regular=2 rejects another UID's O_CREAT open in sticky /run/lock,
# even at mode 0666, which would split the supposed lease by caller identity.
CORPUS_EXTRA_LOCK="/run/lock/fcvm-corpus-extra.lock"

stage_runtime_bundle() {
    local stage source reqbench_manifest_temp manifest_temp
    local reqbench_digest bundle_digest bundle_dest
    local sources=(
        corpus_extra.sh corpus_mem.py hostcdp.sh cdpdrive.py render.py
        corpus_serve.py report.py reqbench.py reqbench.sh reqanalyze.py wddrive.py
        owned_process.py phase_supervisor.py corpus_campaign.sh
    )
    mkdir -- "$RESULTS/runtime"
    stage=$(mktemp -d "$RESULTS/runtime/.stage.XXXXXX")
    for source in "${sources[@]}"; do
        cp --reflink=auto --preserve=mode,timestamps \
            "$SOURCE_BENCH/$source" "$stage/$source"
    done
    cp --reflink=auto --preserve=mode,timestamps \
        "$REPO/target/release/fcvm" "$stage/fcvm"
    cp --reflink=auto --preserve=mode,timestamps \
        "$REPO/target/release/fc-agent" "$stage/fc-agent"
    cp -a --reflink=auto "$SOURCE_BENCH/corpus-live" "$stage/corpus-live"
    chmod 0555 "$stage/fcvm" "$stage/fc-agent"
    reqbench_manifest_temp=$(mktemp "$RESULTS/runtime/.reqbench-manifest.XXXXXX")
    (
        cd "$stage"
        sha256sum fcvm fc-agent reqbench.sh reqbench.py reqanalyze.py \
            cdpdrive.py render.py wddrive.py
    ) > "$reqbench_manifest_temp"
    mv --no-target-directory \
        "$reqbench_manifest_temp" "$stage/REQBENCH_MANIFEST.sha256"
    reqbench_digest=$(sha256sum "$stage/REQBENCH_MANIFEST.sha256" | cut -d' ' -f1)
    manifest_temp=$(mktemp "$RESULTS/runtime/.manifest.XXXXXX")
    (
        cd "$stage"
        find . -type f ! -name MANIFEST.sha256 -print0 \
            | sort -z \
            | xargs -0 sha256sum
    ) > "$manifest_temp"
    mv --no-target-directory "$manifest_temp" "$stage/MANIFEST.sha256"
    bundle_digest=$(sha256sum "$stage/MANIFEST.sha256" | cut -d' ' -f1)
    bundle_dest="$RESULTS/runtime/$bundle_digest"
    chmod -R a-w "$stage"
    mv --no-target-directory "$stage" "$bundle_dest"
    REQBENCH_BUNDLE_SHA256="$reqbench_digest"
    BUNDLE_SHA256="$bundle_digest"
    BUNDLE_DIR="$(cd "$bundle_dest" && pwd -P)"
}

verify_runtime_bundle() {
    local manifest_digest
    [ "$HERE" = "${CORPUS_EXTRA_RUNTIME_BUNDLE:-}" ] || {
        echo "FAILED: the executing harness is not its recorded runtime bundle" >&2
        return 1
    }
    manifest_digest=$(sha256sum "$BENCH/MANIFEST.sha256" | cut -d' ' -f1) || return 1
    [ "$manifest_digest" = "${CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256:-}" ] || {
        echo "FAILED: runtime bundle manifest identity changed" >&2
        return 1
    }
    (cd "$BENCH" && sha256sum --check --strict --status MANIFEST.sha256) || {
        echo "FAILED: runtime bundle bytes changed" >&2
        return 1
    }
}

claim_output_dir() {
    local path="$1" label="$2" parent
    parent=$(dirname -- "$path") \
        || { echo "BLOCKED: cannot identify parent of $label directory $path" >&2; return 2; }
    mkdir -p -- "$parent" \
        || { echo "BLOCKED: cannot create parent of $label directory $path" >&2; return 2; }
    mkdir -- "$path" \
        || { echo "BLOCKED: $label directory $path already exists or cannot be claimed" >&2; return 2; }
}

claim_output_dirs() {
    claim_output_dir "$RESULTS" results || return 2
    if claim_output_dir "$LOGDIR" log; then
        return 0
    fi
    rmdir -- "$RESULTS" \
        || echo "FAILED: could not release newly claimed empty results directory $RESULTS" >&2
    return 2
}

if [ "$CORPUS_EXTRA_STAGED" = 0 ]; then
    sudo -n install -d -o root -g root -m 0755 "$CORPUS_EXTRA_LOCK" \
        || { echo "BLOCKED: cannot provision host-wide lease $CORPUS_EXTRA_LOCK" >&2; exit 2; }
    exec 9<"$CORPUS_EXTRA_LOCK"
    flock -n 9 \
        || { echo "BLOCKED: another corpus-extra run owns $CORPUS_EXTRA_LOCK" >&2; exit 2; }
    claim_output_dirs
    RESULTS="$(cd "$RESULTS" && pwd -P)"
    LOGDIR="$(cd "$LOGDIR" && pwd -P)"
    CONTAINER_CREATE_OPS_DIR="$RESULTS/container-create-ops"
    mkdir -- "$CONTAINER_CREATE_OPS_DIR"
    SOURCE_REVISION=$(git -C "$REPO" rev-parse HEAD)
    SOURCE_GIT_DIRTY=$(git -C "$REPO" status --porcelain --untracked-files=no | tr '\n' ';')
    runtime_image_raw=$(podman image inspect --format '{{.Id}}' "$IMAGE") \
        || { echo "BLOCKED: cannot inspect image $IMAGE" >&2; exit 2; }
    RUNTIME_IMAGE=$(canonical_runtime_image_id "$runtime_image_raw") || exit 2
    stage_runtime_bundle
    bundle_dir="$BUNDLE_DIR"
    exec env \
        CORPUS_EXTRA_STAGED=1 \
        CORPUS_EXTRA_SOURCE_REPO="$REPO" \
        CORPUS_EXTRA_SOURCE_BENCH="$SOURCE_BENCH" \
        CORPUS_EXTRA_RUNTIME_BUNDLE="$bundle_dir" \
        CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256="$BUNDLE_SHA256" \
        REQBENCH_RUNTIME_BUNDLE_SHA256="$REQBENCH_BUNDLE_SHA256" \
        SOURCE_REVISION="$SOURCE_REVISION" \
        SOURCE_GIT_DIRTY="$SOURCE_GIT_DIRTY" \
        RUNTIME_IMAGE="$RUNTIME_IMAGE" \
        CONTAINER_CREATE_OPS_DIR="$CONTAINER_CREATE_OPS_DIR" \
        PYTHONDONTWRITEBYTECODE=1 \
        STAMP="$STAMP" RUN_ID="$RUN_ID" CONTAINER_OWNER_TOKEN="$CONTAINER_OWNER_TOKEN" \
        RESULTS="$RESULTS" LOGDIR="$LOGDIR" TAG="$TAG" IMAGE="$IMAGE" PHASES="$PHASES" \
        REPS="$REPS" WARMUP="$WARMUP" MEM_NS="$MEM_NS" MEM_REPS="$MEM_REPS" \
        MEM_SEED="$MEM_SEED" \
        UFFD_MODE="$UFFD_MODE" UFFD_PREFETCH="$UFFD_PREFETCH" \
        bash "$bundle_dir/corpus_extra.sh"
fi

[ -e /proc/self/fd/9 ] \
    || { echo "BLOCKED: staged run did not inherit the host-wide lease" >&2; exit 2; }
flock -n 9 \
    || { echo "BLOCKED: staged run does not own the host-wide lease" >&2; exit 2; }
RESULTS="$(cd "$RESULTS" && pwd -P)"
LOGDIR="$(cd "$LOGDIR" && pwd -P)"
CONTAINER_CREATE_OPS_DIR="${CONTAINER_CREATE_OPS_DIR:-$RESULTS/container-create-ops}"
[ -d "$CONTAINER_CREATE_OPS_DIR" ] \
    || { echo "BLOCKED: container create-operation directory is missing" >&2; exit 2; }
verify_runtime_bundle || exit 2

campaign_urls=$(grep -m1 '^URLS="https://example.com/' "$BENCH/corpus_campaign.sh" | sed 's/^URLS="//; s/"$//')
[ "$campaign_urls" = "$URLS" ] || {
    echo "BLOCKED: this script's corpus differs from corpus_campaign.sh's; the host control would not be a control" >&2
    exit 2; }

# Provenance for the files that produce these numbers. hostcdp.sh is NOT in
# reqbench's runtime seal, and this run uses a modified copy of it (the corpus
# schedule), so the record has to name the bytes that ran or the numbers cite
# nothing. git_dirty lists every tracked file that differs from HEAD.
{
    echo "{"
    echo " \"git_head\": \"$SOURCE_REVISION\","
    echo " \"git_dirty\": \"$SOURCE_GIT_DIRTY\","
    echo " \"corpus_extra_runtime_bundle_sha256\": \"$CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256\","
    echo " \"reqbench_runtime_bundle_sha256\": \"$REQBENCH_RUNTIME_BUNDLE_SHA256\","
    echo " \"host_kernel\": \"$(uname -r)\", \"machine\": \"$(uname -m)\","
    echo " \"image\": \"$IMAGE\", \"image_id\": \"$RUNTIME_IMAGE\","
    echo " \"tag\": \"$TAG\", \"reps\": $REPS, \"warmup\": $WARMUP,"
    for f in corpus_extra.sh corpus_mem.py hostcdp.sh cdpdrive.py render.py \
            corpus_serve.py report.py reqbench.py reqbench.sh reqanalyze.py wddrive.py \
            owned_process.py phase_supervisor.py corpus_campaign.sh; do
        echo " \"$f\": \"$(sha256sum "$BENCH/$f" | cut -d' ' -f1)\","
    done
    echo " \"fcvm\": \"$(sha256sum "$BENCH/fcvm" | cut -d' ' -f1)\""
    echo "}"
} > "$RESULTS/provenance.json"
# Valid JSON is not the property that matters. Every field here is a command
# substitution inside an echo, so a missing sha256sum target or an image podman
# does not know leaves the field empty, echo still exits 0 under `set -e`, and an
# object full of "" parses. The record would then name no binary, no image and no
# script bytes while reading as complete. git_dirty is exempt: empty is what a
# clean tree says.
python3 - "$RESULTS/provenance.json" <<'PY' || exit 2
import json, sys
try:
    with open(sys.argv[1]) as handle:
        rec = json.load(handle)
except (OSError, ValueError) as exc:
    sys.exit(f"BLOCKED: provenance.json is not valid JSON: {exc}")
empty = sorted(k for k, v in rec.items() if k != "git_dirty" and v in ("", None))
if empty:
    sys.exit("BLOCKED: provenance.json names nothing for " + ", ".join(empty)
             + "; these numbers would cite no bytes")
PY

load1=$(awk '{print $1}' /proc/loadavg)
awk -v l="$load1" 'BEGIN{exit !(l > 2.0)}' && { echo "BLOCKED: 1-min load $load1 > 2.0" >&2; exit 2; }
find_stray_vmms() {
    local process output rc found=""
    for process in fcvm firecracker; do
        set +e
        output=$(pgrep -a -x "$process" 2>&1)
        rc=$?
        set -e
        case "$rc" in
            0) found="${found}${found:+$'\n'}${output}" ;;
            1) ;;
            *) echo "BLOCKED: pgrep exited $rc while checking $process: $output" >&2; return 2 ;;
        esac
    done
    [ -n "$found" ] || return 1
    printf '%s\n' "$found"
}
set +e
stray_vmms=$(find_stray_vmms)
rc=$?
set -e
case "$rc" in
    0) echo "BLOCKED: stray fcvm/firecracker processes" >&2; printf '%s\n' "$stray_vmms" >&2; exit 2 ;;
    1) ;;
    *) exit 2 ;;
esac

DNSMASQ_WAS_ACTIVE=no
systemctl is-active --quiet dnsmasq 2>/dev/null && DNSMASQ_WAS_ACTIVE=yes
SERVE_JOB_PID=""
SERVE_CONTROL_FD=""
SERVE_CONTROL_PATH=""
ACTIVE_PHASE_PID=""
ACTIVE_PHASE_SIGNAL=""
ACTIVE_PHASE_CONTROL_FD=""
ACTIVE_PHASE_CONTROL_PATH=""

run_logged() {
    local log_path="$1" rc
    shift
    if [ -n "$ACTIVE_PHASE_PID" ] || [ -n "$ACTIVE_PHASE_CONTROL_FD" ]; then
        echo "FAILED: another measurement phase is still owned" >&2
        return 1
    fi
    ACTIVE_PHASE_CONTROL_PATH="$LOGDIR/active-phase.control"
    mkfifo -- "$ACTIVE_PHASE_CONTROL_PATH" \
        || { echo "FAILED: cannot create phase control FIFO" >&2; return 1; }
    if ! exec {ACTIVE_PHASE_CONTROL_FD}<>"$ACTIVE_PHASE_CONTROL_PATH"; then
        rm -f -- "$ACTIVE_PHASE_CONTROL_PATH"
        ACTIVE_PHASE_CONTROL_PATH=""
        echo "FAILED: cannot open phase control FIFO" >&2
        return 1
    fi
    ACTIVE_PHASE_SIGNAL=""
    trap 'ACTIVE_PHASE_SIGNAL=130' INT
    trap 'ACTIVE_PHASE_SIGNAL=143' TERM
    (
        # The campaign shell is the only control writer. If it is SIGKILLed,
        # EOF reaches the already-open supervisor and drains the phase tree.
        exec {ACTIVE_PHASE_CONTROL_FD}>&-
        phase_parent=$BASHPID
        set -o pipefail
        python3 "$BENCH/phase_supervisor.py" --expected-parent "$phase_parent" \
            --control-path "$ACTIVE_PHASE_CONTROL_PATH" -- "$@" 2>&1 \
            | tee "$log_path"
    ) &
    ACTIVE_PHASE_PID=$!
    if [ -n "$ACTIVE_PHASE_SIGNAL" ]; then
        stop_active_phase
        rc="$ACTIVE_PHASE_SIGNAL"
        trap 'exit 130' INT
        trap 'exit 143' TERM
        return "$rc"
    fi
    set +e
    wait "$ACTIVE_PHASE_PID"
    rc=$?
    set -e
    if [ -n "$ACTIVE_PHASE_SIGNAL" ]; then
        stop_active_phase
        rc="$ACTIVE_PHASE_SIGNAL"
    else
        exec {ACTIVE_PHASE_CONTROL_FD}>&-
        rm -f -- "$ACTIVE_PHASE_CONTROL_PATH"
        ACTIVE_PHASE_PID=""
        ACTIVE_PHASE_CONTROL_FD=""
        ACTIVE_PHASE_CONTROL_PATH=""
    fi
    trap 'exit 130' INT
    trap 'exit 143' TERM
    if [ -n "$ACTIVE_PHASE_SIGNAL" ]; then
        rc="$ACTIVE_PHASE_SIGNAL"
    fi
    return "$rc"
}

stop_active_phase() {
    local pid="$ACTIVE_PHASE_PID" control_fd="$ACTIVE_PHASE_CONTROL_FD"
    local control_path="$ACTIVE_PHASE_CONTROL_PATH" rc signal_rc=0
    [ -n "$pid" ] || return 0
    if [ -z "$control_fd" ] || [ -z "$control_path" ]; then
        echo "FAILED: active phase $pid has no control channel" >&2
        return 1
    fi
    say "stopping active measurement phase supervisor ($pid)"
    printf T >&"$control_fd" || signal_rc=1
    set +e
    wait "$pid"
    rc=$?
    set -e
    exec {control_fd}>&-
    rm -f -- "$control_path" || signal_rc=1
    ACTIVE_PHASE_PID=""
    ACTIVE_PHASE_CONTROL_FD=""
    ACTIVE_PHASE_CONTROL_PATH=""
    [ "$signal_rc" -eq 0 ] || return 1
    case "$rc" in 0|130|143) return 0 ;; *) return "$rc" ;; esac
}

stop_corpus_serve() {
    local pid="$SERVE_JOB_PID" control_fd="$SERVE_CONTROL_FD"
    local control_path="$SERVE_CONTROL_PATH" rc signal_rc=0
    [ -n "$pid" ] || return 0
    if [ -z "$control_fd" ] || [ -z "$control_path" ]; then
        echo "FAILED: corpus_serve job $pid has no control channel" >&2
        return 1
    fi
    say "stopping corpus_serve job ($pid)"
    printf T >&"$control_fd" || signal_rc=1
    set +e
    wait "$pid"
    rc=$?
    set -e
    exec {control_fd}>&-
    rm -f -- "$control_path" || signal_rc=1
    SERVE_JOB_PID=""
    SERVE_CONTROL_FD=""
    SERVE_CONTROL_PATH=""
    [ "$signal_rc" -eq 0 ] \
        || { echo "FAILED: could not stop corpus_serve through its owned channel" >&2; return 1; }
    [ "$rc" -eq 0 ] \
        || { echo "FAILED: corpus_serve supervisor exited $rc" >&2; return 1; }
    [ -f "$RESULTS/corpus-serve.status" ] \
        || { echo "FAILED: corpus_serve left no exit status" >&2; return 1; }
    say "corpus_serve exit status: $(tr -d '[:space:]' <"$RESULTS/corpus-serve.status")"
}

require_corpus_serve_clean() {
    local status_file="$RESULTS/corpus-serve.status" status
    [ -f "$status_file" ] \
        || { echo "FAILED: corpus_serve left no exit status; replay logs may be incomplete" >&2; return 1; }
    status=$(tr -d '[:space:]' <"$status_file") \
        || { echo "FAILED: cannot read $status_file" >&2; return 1; }
    [ "$status" = 0 ] \
        || { echo "FAILED: corpus_serve exited $status; replay logs are not complete" >&2; return 1; }
}

cleanup_owned_containers() {
    local listed rc=0 exists_rc id name identity actual_id owner extra
    listed=$(timeout 30 podman ps -a --no-trunc --format '{{.ID}} {{.Names}}') \
        || { echo "FAILED: cannot enumerate containers owned by run $RUN_ID" >&2; return 1; }
    while read -r id name extra; do
        [ -n "$id" ] || continue
        if [ -z "$name" ] || [ -n "$extra" ]; then
            echo "FAILED: cannot parse container identity row '$id $name $extra'" >&2
            rc=1
            continue
        fi
        if [[ ! "$id" =~ ^[0-9a-f]{64}$ ]]; then
            echo "FAILED: podman listed non-exact container ID '$id' for $name" >&2
            rc=1
            continue
        fi
        case "$name" in
            cbmem-"$RUN_ID"-*|hostcdp-"$RUN_ID"-*)
                identity=$(timeout 30 podman inspect --format \
                    '{{.Id}} {{ index .Config.Labels "io.fcvm.bench.owner" }}' "$id") \
                    || { echo "FAILED: cannot inspect possible owned container $name ($id)" >&2; rc=1; continue; }
                read -r actual_id owner extra <<<"$identity"
                if [ "$actual_id" != "$id" ] || [ -n "$extra" ]; then
                    echo "FAILED: container identity changed while inspecting $name ($id -> $identity)" >&2
                    rc=1
                    continue
                fi
                [ "$owner" = "$CONTAINER_OWNER_TOKEN" ] || continue
                timeout 30 podman rm -f -- "$actual_id" >/dev/null 2>&1 \
                    || { echo "FAILED: could not remove owned container $name ($actual_id)" >&2; rc=1; }
                if timeout 30 podman container exists "$actual_id" >/dev/null 2>&1; then
                    echo "FAILED: owned container $name ($actual_id) survived podman rm" >&2
                    rc=1
                else
                    exists_rc=$?
                    [ "$exists_rc" -eq 1 ] || {
                        echo "FAILED: cannot verify removal of owned container $name ($actual_id)" >&2
                        rc=1
                    }
                fi
                ;;
        esac
    done <<<"$listed"
    return "$rc"
}

wait_for_container_create_operations() {
    local listing lock lock_fd rc=0
    [ -d "$CONTAINER_CREATE_OPS_DIR" ] \
        || { echo "FAILED: container create-operation directory disappeared" >&2; return 1; }
    listing=$(mktemp "$RESULTS/.create-locks.XXXXXX") || return 1
    if ! find "$CONTAINER_CREATE_OPS_DIR" -maxdepth 1 -type f -name '*.lock' \
            -print0 > "$listing"; then
        rm -f -- "$listing"
        echo "FAILED: cannot enumerate container create-operation leases" >&2
        return 1
    fi
    while IFS= read -r -d '' lock; do
        if ! exec {lock_fd}<>"$lock"; then
            echo "FAILED: cannot open container create-operation lease $lock" >&2
            rc=1
            continue
        fi
        timeout 300 flock -x "$lock_fd" \
            || { echo "FAILED: container create operation at $lock did not quiesce" >&2; rc=1; }
        exec {lock_fd}>&-
    done < "$listing"
    rm -f -- "$listing" || rc=1
    return "$rc"
}

mark_campaign_withdrawn() {
    local reason="$1" marker
    rm -f -- "$RESULTS/campaign-complete.json" || return 1
    marker=$(mktemp "$RESULTS/.WITHDRAWN.XXXXXX") || return 1
    if printf '%s\n' \
            "WITHDRAWN: $reason; no result in this directory is publishable." \
            > "$marker" \
            && mv -f -- "$marker" "$RESULTS/WITHDRAWN"; then
        rm -f -- "$RESULTS/summary.json"
        return $?
    fi
    rm -f -- "$marker"
    return 1
}

publish_campaign_completion() {
    local arm
    local host_completes=()
    if [[ ",$PHASES," == *",hostcdp,"* ]]; then
        for arm in $(printf '%s' "$HOSTCDP_ARMS" | tr ',' ' '); do
            host_completes+=("$RESULTS/hostcdp-$arm/complete.json")
        done
    fi
    python3 - "$RESULTS/campaign-complete.json" "$RESULTS" "$RUN_ID" \
            "$CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256" "${host_completes[@]}" <<'PY'
import hashlib
import json
import os
import stat
import sys
import tempfile

output_path, results, run_id, runtime_bundle_sha256, *complete_paths = sys.argv[1:]
records = []
seen = set()
for path in complete_paths:
    relative = os.path.relpath(path, results)
    if (relative.startswith("../") or relative == ".." or os.path.isabs(relative)
            or relative in seen):
        sys.exit(f"FAILED: invalid or duplicate host completion path {relative!r}")
    seen.add(relative)
    try:
        fd = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    except OSError as exc:
        sys.exit(f"FAILED: cannot open host completion {relative}: {exc}")
    try:
        metadata = os.fstat(fd)
        if not stat.S_ISREG(metadata.st_mode):
            sys.exit(f"FAILED: host completion {relative} is not a regular file")
        digest = hashlib.sha256()
        size = 0
        while True:
            chunk = os.read(fd, 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
            size += len(chunk)
    finally:
        os.close(fd)
    records.append({"path": relative, "size": size,
                    "sha256": digest.hexdigest()})

record = {
    "schema_version": 1,
    "run_id": run_id,
    "runtime_bundle_sha256": runtime_bundle_sha256,
    "host_completes": sorted(records, key=lambda item: item["path"]),
}
directory = os.path.dirname(output_path)
fd, temporary = tempfile.mkstemp(prefix=".campaign-complete.", dir=directory)
try:
    with os.fdopen(fd, "w") as target:
        json.dump(record, target, sort_keys=True, separators=(",", ":"))
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
    for path in (temporary, output_path):
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass
    try:
        directory_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    except OSError:
        pass
    raise
PY
}

cleanup() {
    local original_rc=$? cleanup_rc=0 reason=""
    trap - EXIT
    set +e
    rm -f -- "$RESULTS/campaign-complete.json" || cleanup_rc=1
    stop_active_phase || cleanup_rc=1
    wait_for_container_create_operations || cleanup_rc=1
    cleanup_owned_containers || cleanup_rc=1
    stop_corpus_serve || cleanup_rc=1
    require_corpus_serve_clean || cleanup_rc=1
    if [ "$DNSMASQ_WAS_ACTIVE" = yes ] && ! systemctl is-active --quiet dnsmasq; then
        for _ in $(seq 1 10); do sudo systemctl start dnsmasq >/dev/null 2>&1 && break; sleep 1; done
        systemctl is-active --quiet dnsmasq || {
            echo "FAILED: dnsmasq did not restart; this box has no DNS. Check: sudo ss -lnup 'sport = :53'" >&2
            cleanup_rc=1; }
    fi
    verify_runtime_bundle || cleanup_rc=1
    if [ "$original_rc" -ne 0 ] || [ "$cleanup_rc" -ne 0 ]; then
        if [ "$original_rc" -ne 0 ]; then
            reason="measurement phase exited $original_rc"
        else
            reason="campaign cleanup or replay verification failed"
        fi
        mark_campaign_withdrawn "$reason" || cleanup_rc=1
    elif ! publish_campaign_completion; then
        cleanup_rc=1
        mark_campaign_withdrawn "campaign completion could not be published" \
            || cleanup_rc=1
    fi
    [ "$original_rc" -ne 0 ] && exit "$original_rc"
    exit "$cleanup_rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

[ "$DNSMASQ_WAS_ACTIVE" = yes ] && { say "stopping dnsmasq for 127.0.0.1:53"; sudo systemctl stop dnsmasq; }

say "starting corpus_serve (DNS 127.0.0.1:53 -> 10.0.2.2, HTTP 80, HTTPS 443)"
rm -f "$RESULTS/corpus-serve.status"
SERVE_CONTROL_PATH="$LOGDIR/corpus-serve.control"
mkfifo -- "$SERVE_CONTROL_PATH" \
    || { echo "BLOCKED: cannot create corpus_serve control FIFO" >&2; exit 3; }
if ! exec {SERVE_CONTROL_FD}<>"$SERVE_CONTROL_PATH"; then
    rm -f -- "$SERVE_CONTROL_PATH"
    SERVE_CONTROL_PATH=""
    echo "BLOCKED: cannot open corpus_serve control FIFO" >&2
    exit 3
fi
# The background job closes its copy of the writer before sudo starts. The root
# supervisor opens the FIFO before launching corpus_serve, so an outer process
# exit becomes EOF and drains the complete privileged child tree.
(
    exec {SERVE_CONTROL_FD}>&-
    set +e
    sudo -n sh -c '
        exec python3 "$1" --expected-parent "$PPID" \
            --control-path "$2" --return-command-status-on-signal -- \
            python3 "$3" --root "$4" --port 80 --tls-port 443 \
            --dns-addr 127.0.0.1 --dns-port 53 --answer-ip 10.0.2.2 \
            --dns-log "$5" --access-log "$6"
    ' _ "$BENCH/phase_supervisor.py" "$SERVE_CONTROL_PATH" \
        "$BENCH/corpus_serve.py" "$BENCH/corpus-live" \
        "$RESULTS/corpus-dns.log" "$RESULTS/corpus-access.log"
    serve_rc=$?
    status_tmp=$(mktemp "$RESULTS/.corpus-serve-status.XXXXXX") || exit 125
    if ! printf '%s\n' "$serve_rc" > "$status_tmp" \
            || ! mv --no-target-directory \
                "$status_tmp" "$RESULTS/corpus-serve.status"; then
        rm -f -- "$status_tmp"
        exit 125
    fi
    exit "$serve_rc"
) > "$LOGDIR/corpus_serve.log" 2>&1 &
SERVE_JOB_PID=$!
for _ in $(seq 1 50); do
    grep -q "loaded [1-9]" "$LOGDIR/corpus_serve.log" && break
    [ ! -f "$RESULTS/corpus-serve.status" ] || break
    sleep 0.1
done
grep -q "loaded [1-9]" "$LOGDIR/corpus_serve.log" || {
    echo "BLOCKED: corpus_serve loaded no urls" >&2; cat "$LOGDIR/corpus_serve.log" >&2; exit 3; }

answer=""
code=""
for attempt in $(seq 1 100); do
    readiness_nonce="$RUN_ID-$attempt"
    readiness_qname="ready-$readiness_nonce.blog.cloudflare.com"
    readiness_path="/?fcvm-ready=$readiness_nonce"
    answer=$(dig +short +time=2 +tries=1 @127.0.0.1 "$readiness_qname" A 2>/dev/null | head -1 || true)
    code=$(curl -sk --noproxy '*' -o /dev/null -w '%{http_code}' --max-time 5 \
        --resolve 'blog.cloudflare.com:443:127.0.0.1' \
        "https://blog.cloudflare.com$readiness_path" 2>/dev/null || true)
    if [ "$answer" = "10.0.2.2" ] && [ "$code" = "200" ] \
            && replay_probe_logged "$readiness_qname" "$readiness_path"; then
        break
    fi
    [ ! -f "$RESULTS/corpus-serve.status" ] || {
        echo "BLOCKED: corpus_serve died during startup" >&2
        cat "$LOGDIR/corpus_serve.log" >&2
        exit 3
    }
    sleep 0.2
done
[ "$answer" = "10.0.2.2" ] \
    || { echo "BLOCKED: wildcard DNS answered '$answer', expected 10.0.2.2" >&2; exit 3; }
[ "$code" = "200" ] \
    || { echo "BLOCKED: HTTPS replay returned '$code' for blog.cloudflare.com" >&2; exit 3; }

# Every corpus member must replay before anything is measured: a partial corpus
# measures error pages as renders, which look like fast, plausible numbers.
checked=0
missing=""
for url in $(printf '%s\n' "$URLS" | tr ',' ' '); do
    host=$(printf '%s' "$url" | sed -E 's#^https?://([^/]+).*#\1#')
    for _ in $(seq 1 100); do
        ucode=$(curl -sk --noproxy '*' -o /dev/null -w '%{http_code}' --max-time 10 \
                --resolve "$host:443:127.0.0.1" "$url" 2>/dev/null || true)
        case "$ucode" in 200|30[1278]) break ;; esac
        sleep 0.2
    done
    case "$ucode" in 200|30[1278]) ;; *) missing="${missing}"$'\n'"  $ucode  $url" ;; esac
    checked=$((checked + 1))
done
[ -z "$missing" ] || { printf 'BLOCKED: the corpus does not serve every URL:%s\n' "$missing" >&2; exit 3; }
say "corpus complete: all $checked URLs replay locally"

# Two host arms. "free" is the naive host container: the whole box is available
# to it, which is what a container on this machine actually gets. "cpu2" caps it
# at the VM clone's vCPU count, so the CPU budget is not a second variable in the
# comparison. Both run the same schedule against the same replay.
HOSTCDP_ARMS="${HOSTCDP_ARMS:-free,cpu2}"
if [[ ",$PHASES," == *",hostcdp,"* ]]; then
    for arm in $(printf '%s' "$HOSTCDP_ARMS" | tr ',' ' '); do
        case "$arm" in
            free) cpus=""; cpu_budget=unlimited ;;
            cpu2) cpus=2; cpu_budget=vm-matched ;;
            *) echo "BLOCKED: unknown hostcdp arm '$arm'" >&2; exit 2 ;;
        esac
        say "hostcdp/$arm over the corpus: $REPS measured reps plus $WARMUP warmup, cpus=${cpus:-<all>}, resolver rule -> 127.0.0.1"
        run_logged "$LOGDIR/hostcdp-$arm.log" env \
            URL="$URLS" REPS="$REPS" WARMUP="$WARMUP" \
            IMAGE="$IMAGE" IMAGE_ID="$RUNTIME_IMAGE" CPUS="$cpus" \
            RUNID="$RUN_ID-$arm" BENCH_RESOLVE_ALL_TO=127.0.0.1 SETTLE_WAIT_SECS=300 \
            COMPARISON_LABEL="$arm" CPU_BUDGET="$cpu_budget" \
            CONTAINER_OWNER_TOKEN="$CONTAINER_OWNER_TOKEN" \
            SOURCE_REVISION="$SOURCE_REVISION" \
            REQBENCH_RUNTIME_MANIFEST="$BENCH/REQBENCH_MANIFEST.sha256" \
            REQBENCH_RUNTIME_BUNDLE_SHA256="$REQBENCH_RUNTIME_BUNDLE_SHA256" \
            CORPUS_EXTRA_RUNTIME_MANIFEST="$BENCH/MANIFEST.sha256" \
            CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256="$CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256" \
            CONTAINER_CREATE_OPS_DIR="$CONTAINER_CREATE_OPS_DIR" \
            RESULTS="$RESULTS/hostcdp-$arm" bash "$BENCH/hostcdp.sh"
    done
fi

if [[ ",$PHASES," == *",memory,"* ]]; then
    say "matched-basis memory: N in $MEM_NS, $MEM_REPS reps, interleaved seed $MEM_SEED"
    run_logged "$LOGDIR/memory.log" python3 "$BENCH/corpus_mem.py" \
        --results "$RESULTS/memory" --tag "$TAG" \
        --image "$IMAGE" --image-id "$RUNTIME_IMAGE" \
        --urls "$URLS" --ns "$MEM_NS" --reps "$MEM_REPS" \
        --seed "$MEM_SEED" \
        --uffd-mode "$UFFD_MODE" --uffd-prefetch "$UFFD_PREFETCH" \
        --run-id "$RUN_ID" \
        --container-owner-token "$CONTAINER_OWNER_TOKEN" \
        --container-create-ops-dir "$CONTAINER_CREATE_OPS_DIR" \
        --source-revision "$SOURCE_REVISION" \
        --runtime-bundle-sha256 "$REQBENCH_RUNTIME_BUNDLE_SHA256" \
        --corpus-extra-runtime-bundle-sha256 "$CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256" \
        --fcvm "$BENCH/fcvm"
fi

stop_corpus_serve
require_corpus_serve_clean
verify_runtime_bundle
say "records: $RESULTS"
say "logs:    $LOGDIR"
