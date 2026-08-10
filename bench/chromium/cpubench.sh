#!/usr/bin/env bash
# bench/chromium/cpubench.sh — CPU attribution run.
#
# The wall-clock decomposition exists already. This answers the other question:
# where does the CPU go, and therefore what is the throughput ceiling?
#
# Reuses bench.sh's helpers by sourcing it with its `main "$@"` line stripped, so
# the golden-snapshot tags, the in-guest driver template, the serve lifecycle and
# the fixture servers are BYTE-IDENTICAL to the latency run. Divergence between
# the two harnesses would make the CPU numbers unattributable to the wall numbers.
#
# Cells: {file-4k, uffd-4k-copy, uffd-4k-minor} x {noop, medium, heavy}
#   noop   = restore -> exec -> teardown, no egress, no render.
#            (medium - noop) isolates render CPU from orchestration CPU.
#   medium = the page the 573 ms wall baseline was measured on.
#   heavy  = the page where the file-vs-UFFD wall gap grows to 904 ms.
#
# Requests are run STRICTLY SEQUENTIALLY (shared-nothing, one clone at a time) so
# every CPU figure is uncontended. Load is recorded per request regardless.
set -euo pipefail

SD=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)

export RESULTS=${RESULTS:-$SD/results/20260808-cpu}
export FCVM_LOG=${FCVM_LOG:-fcvm=debug,uffd=info}   # uffd=info -> per-VM fault_count
mkdir -p "$RESULTS"/{raw,logs,requests,samples}

# bench.sh ends with `main "$@"`; drop that one line and source the rest.
[ "$(tail -1 "$SD/bench.sh")" = 'main "$@"' ] || {
    echo "FATAL: bench.sh no longer ends with 'main \"\$@\"' — fix the source hack" >&2
    exit 1; }
# shellcheck disable=SC1090
source <(head -n -1 "$SD/bench.sh")

REPS=${REPS:-12}
WARMUP=${WARMUP:-3}
PERIOD_MS=${PERIOD_MS:-5}
CELLS=${CELLS:-"file uffd-copy uffd-minor"}
PAGES=${PAGES:-"noop medium heavy"}
FMODE=rootless-proxy

init_modes
[ "${MODE_AVAIL[$FMODE]}" = yes ] || die "$FMODE unavailable"

TAG=$(golden_tag "${MODE_SNAP[$FMODE]}")
snapshot_exists "$TAG" || die "golden snapshot $TAG missing — run bench.sh phase1"
log "golden snapshot: $TAG"

start_host_servers

# ---------------------------------------------------------------------------
# quiescent machine baseline: what the box burns with nothing of ours running.
# Without it the whole-machine residual has no reference and every unattributed
# millisecond looks like a finding.
# ---------------------------------------------------------------------------
idle_baseline() {  # $1 label, $2 seconds
    python3 - "$RESULTS/raw/idle-$1.json" "$2" <<'EOF'
import json, sys, time
def stat():
    for l in open("/proc/stat"):
        if l.startswith("cpu "):
            v=[int(x) for x in l.split()[1:]]
            return v
out, secs = sys.argv[1], float(sys.argv[2])
a=stat(); t0=time.time(); time.sleep(secs); b=stat(); dt=time.time()-t0
HZ=100.0
k=["user","nice","system","idle","iowait","irq","softirq","steal"]
d={k[i]:(b[i]-a[i])/HZ for i in range(len(k))}
busy=sum(d[x] for x in ["user","nice","system","irq","softirq","steal"])
json.dump({"wall_s":dt,"cpu_s":d,"busy_cpu_s":busy,"busy_cores":busy/dt,
           "loadavg":open("/proc/loadavg").read().split()[:3]}, open(out,"w"), indent=1)
print("idle baseline %s: %.3f busy cpu-s over %.2fs = %.4f cores, load %s"
      % (sys.argv[1], busy, dt, busy/dt, open("/proc/loadavg").read().split()[0]))
EOF
}

log "measuring quiescent baseline (5s)"
idle_baseline pre 5

# ---------------------------------------------------------------------------
# per-request cgroups
# ---------------------------------------------------------------------------
CG=/sys/fs/cgroup/cpuprof-$RUNID.slice
sudo -n mkdir -p "$CG" || die "cannot create $CG (need passwordless sudo)"
log "per-request cgroup base: $CG"

url_for() {  # $1 page label -> url (or the literal noop)
    case "$1" in
        noop) printf 'noop' ;;
        *)    printf 'http://%s:%s/%s.html' "${MODE_HOST[$FMODE]}" "$HTTP_PORT" "$1" ;;
    esac
}

SERVE_PID=""
SERVE_LOG=""
CUR_CELL=""

start_cell() {  # $1 cell
    local cell=$1
    case "$cell" in
        file)
            SERVE_PID=""; SERVE_LOG=""
            prewarm_memory "$TAG"          # page cache warm: file-backed arm must
            log "cell $cell: file-backed, memory.bin prewarmed"    # not measure disk I/O
            ;;
        uffd-copy)
            SERVE_PID=$(start_serve "$TAG" "" copy)
            SERVE_LOG="$RESULTS/logs/serve-$TAG-copy.log"
            log "cell $cell: serve pid=$SERVE_PID mode=copy log=$SERVE_LOG"
            ;;
        uffd-minor)
            SERVE_PID=$(start_serve "$TAG" "" minor)
            SERVE_LOG="$RESULTS/logs/serve-$TAG-minor.log"
            log "cell $cell: serve pid=$SERVE_PID mode=minor log=$SERVE_LOG"
            ;;
    esac
    CUR_CELL=$cell
}

stop_cell() {
    [ -n "$SERVE_PID" ] && stop_serve "$SERVE_PID" ""
    SERVE_PID=""
}

memarm_of() { case "$1" in file) printf file ;; *) printf uffd ;; esac; }

run_one() {  # $1 cell, $2 page, $3 iter, $4 warm|meas
    local cell=$1 page=$2 i=$3 kind=$4
    local name="cb-cpu-$RUNID-$cell-$page-$i"
    local url; url=$(url_for "$page")
    local execcmd; execcmd=$(build_driver "$url" "$SHOT_FMT" "$SHOT_QUALITY" 0)
    local out="$RESULTS/raw/$kind-$cell-$page-$i.json"
    local vmlog="$RESULTS/requests/$kind-$cell-$page-$i.log"
    python3 "$SD/cpuprof.py" \
        --cg-base "$CG" --name "$name" --fcvm "$FCVM" \
        --memarm "$(memarm_of "$cell")" --tag "$TAG" --serve-pid "$SERVE_PID" \
        --exec-cmd "$execcmd" --out "$out" --period-ms "$PERIOD_MS" \
        --rust-log "$FCVM_LOG" --vmlog "$vmlog" --serve-log "$SERVE_LOG" \
        || log "WARN: cpuprof failed for $cell/$page/$i"
}

# ---------------------------------------------------------------------------
# perf cross-check on the SERVE process.
#
# /usr/bin/perf is a broken wrapper on this kernel ("perf not found for kernel
# 6.18.3-fcvm"); the linux-tools-6.8 binary underneath it works fine, including
# hardware counters under sudo. Verified, not assumed — see notes.md.
#
# What this settles: src/uffd/server.rs calls `read_event()`, which the crate
# implements as a read() of a ONE-element uffd_msg buffer. So the syscall count
# per request, not the fault count, is the thing to measure: ioctl ~= faults,
# read ~= faults + drain terminations. `read_events(&mut EventBuffer)` exists in
# the same crate and is unused — if ioctls track faults 1:1 that is the fix.
# ---------------------------------------------------------------------------
PERF=${PERF:-/usr/lib/linux-tools-6.8.0-137/perf}
PERF_REPS=${PERF_REPS:-3}
PERF_WINDOW=${PERF_WINDOW:-8}

perf_pass() {  # $1 cell, $2 page
    local cell=$1 page=$2 i out
    [ -n "$SERVE_PID" ] || return 0
    [ -x "$PERF" ] || { log "perf binary $PERF missing — skipping syscall cross-check"; return 0; }
    for i in $(seq 1 "$PERF_REPS"); do
        out="$RESULTS/raw/perf-$cell-$page-$i.csv"
        # Fixed window with `-- sleep`: an unprivileged parent cannot signal a
        # root perf to make it dump counters, and the serve process is idle
        # outside the request, so a slightly wide window costs nothing.
        sudo -n "$PERF" stat -e syscalls:sys_enter_read,syscalls:sys_enter_ioctl,syscalls:sys_enter_epoll_pwait,syscalls:sys_enter_futex,syscalls:sys_enter_ppoll \
            -p "$SERVE_PID" -x, -o "$out" -- sleep "$PERF_WINDOW" &
        local perfjob=$!
        sleep 0.4
        run_one "$cell" "$page" "perf$i" perfreq
        wait $perfjob || true
    done
    log "cell $cell page $page: perf syscall cross-check done ($PERF_REPS reps)"
}

for cell in $CELLS; do
    start_cell "$cell"
    for page in $PAGES; do
        for i in $(seq 1 "$WARMUP"); do
            run_one "$cell" "$page" "$i" warm
        done
        log "cell $cell page $page: warmup done ($WARMUP), measuring $REPS"
        for i in $(seq 1 "$REPS"); do
            run_one "$cell" "$page" "$i" meas
        done
        log "cell $cell page $page: done"
        case "$page" in medium|heavy) perf_pass "$cell" "$page" ;; esac
    done
    stop_cell
done

log "measuring quiescent baseline (5s, post)"
idle_baseline post 5

sudo -n rmdir "$CG" 2>/dev/null || true
log "CPU attribution raw records in $RESULTS/raw"
