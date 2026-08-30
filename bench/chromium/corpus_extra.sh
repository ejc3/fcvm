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
MEM_REPS="${MEM_REPS:-2}"
MEM_SEED="${MEM_SEED:-20260830}"
UFFD_MODE="${UFFD_MODE:-minor}"
UFFD_PREFETCH="${UFFD_PREFETCH:-on}"

# The campaign's 14 URLs, in the campaign's order. Copied from corpus_campaign.sh
# and checked against it below: a corpus that has drifted would make the host
# control a different workload from the VM arm it is a control for.
URLS="https://example.com/,https://news.ycombinator.com/,https://developers.cloudflare.com/,https://blog.cloudflare.com/,https://en.wikipedia.org/,https://developer.mozilla.org/en-US/,https://www.elmundo.es/,https://www.rtp.pt/noticias/,https://www.theguardian.com/international,https://todomvc.com/examples/javascript-es6/dist/,https://todomvc.com/examples/react/dist/index.html,https://todomvc.com/examples/vue/dist/,https://todomvc.com/examples/angular/dist/browser/,https://todomvc.com/examples/preact/dist/"

say() { printf '\n=== %s %s\n' "$(date +%H:%M:%S)" "$*"; }

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
        head install jq kill mkdir mktemp mv pgrep podman python3 rm rmdir sed seq \
        setsid sha256sum sleep sort sudo systemctl tee timeout tr uname xargs; do
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
        owned_process.py corpus_campaign.sh
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
    SOURCE_REVISION=$(git -C "$REPO" rev-parse HEAD)
    SOURCE_GIT_DIRTY=$(git -C "$REPO" status --porcelain --untracked-files=no | tr '\n' ';')
    RUNTIME_IMAGE=$(podman inspect --format '{{.Id}}' "$IMAGE")
    [[ "$RUNTIME_IMAGE" =~ ^sha256:[0-9a-f]{64}$ ]] || {
        echo "BLOCKED: image $IMAGE resolved to invalid identity '$RUNTIME_IMAGE'" >&2
        exit 2
    }
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
            owned_process.py corpus_campaign.sh; do
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
SERVE_PID=""
SERVE_START_TIME=""
ACTIVE_PHASE_PID=""

run_logged() {
    local log_path="$1" rc
    shift
    setsid "$@" > >(tee "$log_path") 2>&1 &
    ACTIVE_PHASE_PID=$!
    set +e
    wait "$ACTIVE_PHASE_PID"
    rc=$?
    set -e
    if kill -0 -- "-$ACTIVE_PHASE_PID" 2>/dev/null; then
        say "measurement phase leader $ACTIVE_PHASE_PID exited with descendants still running"
        stop_active_phase
        [ "$rc" -ne 0 ] && return "$rc"
        return 1
    fi
    ACTIVE_PHASE_PID=""
    return "$rc"
}

stop_active_phase() {
    local pid="$ACTIVE_PHASE_PID"
    [ -n "$pid" ] || return 0
    if kill -0 -- "-$pid" 2>/dev/null || kill -0 "$pid" 2>/dev/null; then
        say "stopping active measurement phase ($pid)"
        kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
        for _ in $(seq 1 50); do
            if ! kill -0 -- "-$pid" 2>/dev/null && ! kill -0 "$pid" 2>/dev/null; then break; fi
            sleep 0.1
        done
        if kill -0 -- "-$pid" 2>/dev/null || kill -0 "$pid" 2>/dev/null; then
            say "measurement phase $pid did not exit; escalating to SIGKILL"
            kill -KILL -- "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
        fi
    fi
    wait "$pid" 2>/dev/null || true
    ACTIVE_PHASE_PID=""
}

stop_corpus_serve() {
    local rc alive=0
    [ -n "$SERVE_PID" ] || return 0
    set +e
    sudo python3 "$BENCH/owned_process.py" signal \
        "$SERVE_PID" "$SERVE_START_TIME" 15 >/dev/null 2>&1
    rc=$?
    set -e
    case "$rc" in
        0)
        say "stopping corpus_serve ($SERVE_PID)"
        ;;
        3) ;;
        *) echo "FAILED: cannot signal owned corpus_serve process $SERVE_PID" >&2; return 1 ;;
    esac
    for _ in $(seq 1 50); do
        set +e
        sudo python3 "$BENCH/owned_process.py" signal \
            "$SERVE_PID" "$SERVE_START_TIME" 0 >/dev/null 2>&1
        rc=$?
        set -e
        case "$rc" in
            0) alive=1; sleep 0.1 ;;
            3) alive=0; break ;;
            *) echo "FAILED: cannot verify owned corpus_serve process $SERVE_PID" >&2; return 1 ;;
        esac
    done
    if [ "$alive" -eq 1 ]; then
        say "corpus_serve $SERVE_PID did not exit; escalating to SIGKILL"
        set +e
        sudo python3 "$BENCH/owned_process.py" signal \
            "$SERVE_PID" "$SERVE_START_TIME" 9 >/dev/null 2>&1
        rc=$?
        set -e
        case "$rc" in 0|3) ;; *) return 1 ;; esac
        for _ in $(seq 1 50); do
            set +e
            sudo python3 "$BENCH/owned_process.py" signal \
                "$SERVE_PID" "$SERVE_START_TIME" 0 >/dev/null 2>&1
            rc=$?
            set -e
            case "$rc" in
                0) sleep 0.1 ;;
                3) alive=0; break ;;
                *) return 1 ;;
            esac
        done
        [ "$alive" -eq 0 ] \
            || { echo "FAILED: owned corpus_serve process $SERVE_PID survived SIGKILL" >&2; return 1; }
    fi
    for _ in $(seq 1 50); do [ -f "$RESULTS/corpus-serve.status" ] && break; sleep 0.1; done
    [ -f "$RESULTS/corpus-serve.status" ] \
        || { echo "FAILED: corpus_serve left no exit status" >&2; return 1; }
    say "corpus_serve exit status: $(tr -d '[:space:]' <"$RESULTS/corpus-serve.status")"
    SERVE_PID=""
    SERVE_START_TIME=""
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
    local listed rc=0 id name identity actual_id owner extra
    listed=$(timeout 30 podman ps -a --no-trunc --format '{{.ID}} {{.Names}}') \
        || { echo "FAILED: cannot enumerate containers owned by run $RUN_ID" >&2; return 1; }
    while read -r id name extra; do
        [ -n "$id" ] || continue
        if [ -z "$name" ] || [ -n "$extra" ]; then
            echo "FAILED: cannot parse container identity row '$id $name $extra'" >&2
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
                ;;
        esac
    done <<<"$listed"
    return "$rc"
}

mark_runtime_bundle_withdrawn() {
    local marker="$RESULTS/.WITHDRAWN.$$"
    printf '%s\n' \
        'WITHDRAWN: the staged runtime bundle changed during measurement; no result in this directory is publishable.' \
        > "$marker" \
        && mv -f -- "$marker" "$RESULTS/WITHDRAWN"
}

cleanup() {
    local original_rc=$? cleanup_rc=0 bundle_ok=1 phase_cleanup_rc=0
    trap - EXIT
    set +e
    stop_active_phase || phase_cleanup_rc=1
    verify_runtime_bundle || cleanup_rc=1
    [ "$cleanup_rc" -eq 0 ] || bundle_ok=0
    [ "$phase_cleanup_rc" -eq 0 ] || cleanup_rc=1
    cleanup_owned_containers || cleanup_rc=1
    stop_corpus_serve || cleanup_rc=1
    if [ "$DNSMASQ_WAS_ACTIVE" = yes ] && ! systemctl is-active --quiet dnsmasq; then
        for _ in $(seq 1 10); do sudo systemctl start dnsmasq >/dev/null 2>&1 && break; sleep 1; done
        systemctl is-active --quiet dnsmasq || {
            echo "FAILED: dnsmasq did not restart; this box has no DNS. Check: sudo ss -lnup 'sport = :53'" >&2
            cleanup_rc=1; }
    fi
    if [ "$bundle_ok" -eq 0 ]; then
        mark_runtime_bundle_withdrawn || cleanup_rc=1
    fi
    [ "$original_rc" -ne 0 ] && exit "$original_rc"
    exit "$cleanup_rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

[ "$DNSMASQ_WAS_ACTIVE" = yes ] && { say "stopping dnsmasq for 127.0.0.1:53"; sudo systemctl stop dnsmasq; }

say "starting corpus_serve (DNS 127.0.0.1:53 -> 10.0.2.2, HTTP 80, HTTPS 443)"
SERVE_PIDFILE="$LOGDIR/corpus_serve.pid"
rm -f "$SERVE_PIDFILE" "$RESULTS/corpus-serve.status"
# The caller owns LOGDIR; sudo applies only to the detached replay wrapper.
# shellcheck disable=SC2024
sudo -b sh -c 'python3 "$2" --root "$3" --port 80 --tls-port 443 --dns-addr 127.0.0.1 --dns-port 53 --answer-ip 10.0.2.2 --dns-log "$4" --access-log "$5" & pid=$!; echo "$pid" > "$1"; wait "$pid"; rc=$?; echo "$rc" > "$6.tmp" && mv "$6.tmp" "$6"' \
    _ "$SERVE_PIDFILE" "$BENCH/corpus_serve.py" "$BENCH/corpus-live" \
    "$RESULTS/corpus-dns.log" "$RESULTS/corpus-access.log" "$RESULTS/corpus-serve.status" \
    > "$LOGDIR/corpus_serve.log" 2>&1
for _ in $(seq 1 50); do [ -s "$SERVE_PIDFILE" ] && break; sleep 0.1; done
SERVE_PID=$(cat "$SERVE_PIDFILE" 2>/dev/null || true)
[ -n "$SERVE_PID" ] || { echo "BLOCKED: corpus_serve did not start" >&2; cat "$LOGDIR/corpus_serve.log" >&2; exit 3; }
SERVE_START_TIME=$(sudo python3 "$BENCH/owned_process.py" identity "$SERVE_PID") \
    || { echo "BLOCKED: cannot record corpus_serve process identity for pid $SERVE_PID" >&2; exit 3; }
sudo kill -0 "$SERVE_PID" 2>/dev/null || {
    echo "BLOCKED: corpus_serve pid $SERVE_PID is not alive" >&2
    cat "$LOGDIR/corpus_serve.log" >&2
    exit 3
}
grep -q "loaded [1-9]" "$LOGDIR/corpus_serve.log" || {
    echo "BLOCKED: corpus_serve loaded no urls" >&2; cat "$LOGDIR/corpus_serve.log" >&2; exit 3; }

answer=""
code=""
for _ in $(seq 1 100); do
    answer=$(dig +short +time=2 +tries=1 @127.0.0.1 blog.cloudflare.com A 2>/dev/null | head -1 || true)
    code=$(curl -sk --noproxy '*' -o /dev/null -w '%{http_code}' --max-time 5 \
        --resolve 'blog.cloudflare.com:443:127.0.0.1' https://blog.cloudflare.com/ 2>/dev/null || true)
    if [ "$answer" = "10.0.2.2" ] && [ "$code" = "200" ]; then break; fi
    sudo kill -0 "$SERVE_PID" 2>/dev/null || {
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
            URL="$URLS" REPS="$REPS" WARMUP="$WARMUP" IMAGE="$RUNTIME_IMAGE" CPUS="$cpus" \
            RUNID="$RUN_ID-$arm" BENCH_RESOLVE_ALL_TO=127.0.0.1 SETTLE_WAIT_SECS=300 \
            COMPARISON_LABEL="$arm" CPU_BUDGET="$cpu_budget" \
            CONTAINER_OWNER_TOKEN="$CONTAINER_OWNER_TOKEN" \
            SOURCE_REVISION="$SOURCE_REVISION" \
            REQBENCH_RUNTIME_MANIFEST="$BENCH/REQBENCH_MANIFEST.sha256" \
            REQBENCH_RUNTIME_BUNDLE_SHA256="$REQBENCH_RUNTIME_BUNDLE_SHA256" \
            CORPUS_EXTRA_RUNTIME_MANIFEST="$BENCH/MANIFEST.sha256" \
            CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256="$CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256" \
            RESULTS="$RESULTS/hostcdp-$arm" bash "$BENCH/hostcdp.sh"
    done
fi

if [[ ",$PHASES," == *",memory,"* ]]; then
    say "matched-basis memory: N in $MEM_NS, $MEM_REPS reps, interleaved seed $MEM_SEED"
    run_logged "$LOGDIR/memory.log" python3 "$BENCH/corpus_mem.py" \
        --results "$RESULTS/memory" --tag "$TAG" --image "$RUNTIME_IMAGE" \
        --urls "$URLS" --ns "$MEM_NS" --reps "$MEM_REPS" \
        --seed "$MEM_SEED" \
        --uffd-mode "$UFFD_MODE" --uffd-prefetch "$UFFD_PREFETCH" \
        --run-id "$RUN_ID" \
        --container-owner-token "$CONTAINER_OWNER_TOKEN" \
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
