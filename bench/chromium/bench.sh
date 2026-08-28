#!/usr/bin/env bash
# bench/chromium/bench.sh — host-side harness for the shared-nothing Chromium bench.
#
# Measures per-request latency (spawn -> exec -> egress-ready -> render -> destroy)
# and memory density for Chromium-in-fcvm clones, across every distinct egress
# path a clone can use, against a HOST-SERVED "simulated external" fixture site
# (same bytes as the in-guest control server baked into localhost/chromium-bench).
#
# Egress paths enumerated from src/network/* + fc-agent/src/proxy.rs:
#   rootless-proxy   guest iptables REDIRECT -> fc-agent mux -> vsock -> fcvm
#                    egress proxy -> host TCP (default rootless path, IPv4)
#   rootless-pasta   nat OUTPUT flushed in guest before snapshot -> eth0 -> br0
#                    -> pasta L2<->L4 translation -> host TCP (IPv4)
#   rootless-proxy6  same vsock proxy path, IPv6 destination (ip6tables REDIRECT)
#   rootless-pasta6  pasta IPv6 (-o host-v6), redirect flushed
#   bridged          TAP -> namespace bridge -> veth -> host iptables MASQUERADE
#                    (kernel path, requires sudo)
#   routed           veth + native IPv6 kernel routing + NDP proxy (sudo + host v6)
# (No slirp remnants exist in the tree; egress proxy on/off is not a CLI toggle —
#  it is hardwired to rootless mode, so the pasta arms snapshot a guest with the
#  REDIRECT rules flushed.)
#
# Memory-restore arms (src/commands/snapshot.rs):
#   uffd   snapshot serve + snapshot run --pid  (lazy UFFDIO_COPY, private pages)
#   file   snapshot run --snapshot              (MAP_PRIVATE page-cache sharing)
#   hugepages force UFFD ("Hugepages require UFFD restore (Firecracker rejects
#   File backend for hugepage snapshots)") -> the file+huge cell degrades to an
#   implicit per-clone UFFD server and is reported as such.
#
# Usage:
#   bench/chromium/bench.sh run              # all phases
#   bench/chromium/bench.sh phase1           # golden snapshots only
#   R=6 REBUILD=1 bench/chromium/bench.sh run
# Env knobs: R (reps, default 12), R_CONTROL, R_COLD, RESULTS (reuse a results
# dir across phase invocations), REBUILD=1, FANOUT_MODE, SKIP_SUDO=1.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." &>/dev/null && pwd)
# Overridable so a run can be pinned to a binary built from a KNOWN tree. This
# repo is a shared box: another workload rebuilding target/release/fcvm from its
# own uncommitted changes would otherwise silently swap the thing under test
# mid-benchmark. hostinfo.json records the sha256 that actually ran.
FCVM=${FCVM:-$REPO_ROOT/target/release/fcvm}
PAGES_DIR=$SCRIPT_DIR/pages
IMAGE=localhost/chromium-bench
STATE_DIR=/mnt/fcvm-btrfs/state
SNAP_DIR=/mnt/fcvm-btrfs/snapshots

R=${R:-12}
R_CONTROL=${R_CONTROL:-$(( R / 2 > 3 ? R / 2 : 3 ))}
R_COLD=${R_COLD:-3}
CPU=${CPU:-2}
MEM=${MEM:-2048}
REBUILD=${REBUILD:-0}
SKIP_SUDO=${SKIP_SUDO:-0}
BURST_NS=${BURST_NS:-"4 8 16"}
SUST_RATES=${SUST_RATES:-"1 2 4 8"}
SUST_RATES_HUGE=${SUST_RATES_HUGE:-"1 2 4"}   # pool-bounded, see phase0
SUST_SECS=${SUST_SECS:-60}
HUGEPAGE_POOL=${HUGEPAGE_POOL:-18432}          # 36GB of 2MB pages for huge cells
MAX_INFLIGHT=${MAX_INFLIGHT:-40}
REQ_TIMEOUT=${REQ_TIMEOUT:-120}

# Stage attribution needs the exec client's retry-count / cumulative-wait debug
# lines ("connected to exec server after retries"). At fcvm=info they are
# invisible and the exec stage is unattributable — that is exactly what sank the
# first run's stage decomposition, so debug is the DEFAULT here, not an option.
FCVM_LOG=${FCVM_LOG:-fcvm=debug}

# Interleaving (defect 2). Every per-request cell is emitted in one seeded
# shuffled schedule so mode is not confounded with wall-clock drift, and control
# cells that touch no egress are mixed into the SAME schedule so drift is
# measurable and removable.
SEED=${SEED:-20260808}
BURST_REPS=${BURST_REPS:-5}       # repeats per (cell, N) — the burst is the unit
DENSITY_NS=${DENSITY_NS:-"1 2 4 8 16"}
DENSITY_REPS=${DENSITY_REPS:-3}
SHOT_FMT=${SHOT_FMT:-jpeg}        # default artifact encoding for the matrix
SHOT_QUALITY=${SHOT_QUALITY:-80}

RUNID=${RUNID:-$(date +%H%M%S)-$$}
STAMP=$(date +%Y%m%d-%H%M%S)
RESULTS=${RESULTS:-$SCRIPT_DIR/results/$STAMP}
mkdir -p "$RESULTS"/{requests,logs,samples}

HTTP_PORT=$(( 18000 + ($$ % 400) * 2 ))
HTTPS_PORT=$(( HTTP_PORT + 1 ))

TSPY='import sys,time
for l in sys.stdin:
    sys.stdout.write("%.3f %s" % (time.time(), l)); sys.stdout.flush()'

log() { printf '[bench %s] %s\n' "$(date +%H:%M:%S)" "$*"; }
die() { log "FATAL: $*"; exit 1; }
now() { date +%s.%N; }

[ -x "$FCVM" ] || die "$FCVM not built (run: make build)"

# ---------------------------------------------------------------------------
# host info / config snapshot
# ---------------------------------------------------------------------------
HOST4=$(ip -4 route get 1.1.1.1 2>/dev/null | grep -oP 'src \K\S+' | head -1)
HOST6=$(ip -6 addr show scope global 2>/dev/null | grep -oP 'inet6 \K[0-9a-f:]+' | head -1)
[ -n "$HOST4" ] || die "no host IPv4 (route to 1.1.1.1)"

write_hostinfo() {
    python3 - "$RESULTS" "$HOST4" "${HOST6:-}" "$R" "$CPU" "$MEM" "$FCVM" <<'EOF'
import hashlib, json, os, platform, subprocess, sys
d, h4, h6, r, cpu, mem, fcvm = sys.argv[1:8]
meminfo = {l.split(":")[0]: l.split()[1] for l in open("/proc/meminfo") if ":" in l}


def sha256(p):
    try:
        h = hashlib.sha256()
        with open(p, "rb") as f:
            for blk in iter(lambda: f.read(1 << 20), b""):
                h.update(blk)
        return h.hexdigest()
    except OSError:
        return None


def sh(*a):
    try:
        return subprocess.run(a, capture_output=True, text=True, timeout=30).stdout.strip()
    except Exception:
        return ""


# Contention is the single biggest silent inflator on this shared box, so the
# load at run start is RECORDED, not remembered. A reader can now tell whether a
# cell was measured on a quiet machine without taking the write-up's word for it.
load1, load5, load15 = open("/proc/loadavg").read().split()[:3]
info = {
    "uname": " ".join(platform.uname()),
    "nproc": os.cpu_count(),
    "mem_total_kb": int(meminfo["MemTotal"]),
    "cpu_model": next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo")
                       if l.lower().startswith(("model name", "hardware", "cpu part"))), "unknown-arm64"),
    "host_ipv4": h4, "host_ipv6": h6 or None,
    "reps": int(r), "vm_cpu": int(cpu), "vm_mem_mib": int(mem),
    "started_utc": subprocess.run(["date", "-u", "+%Y-%m-%dT%H:%M:%SZ"],
                                  capture_output=True, text=True).stdout.strip(),
    "loadavg_at_start": [float(load1), float(load5), float(load15)],
    "firecracker_procs_at_start": int(sh("pgrep", "-c", "firecracker") or 0),
    # provenance: exactly which binaries and image produced these numbers
    "fcvm_sha256": sha256(fcvm),
    "fc_agent_sha256": sha256(os.path.join(os.path.dirname(fcvm), "fc-agent")),
    "git_commit": sh("git", "-C", os.path.dirname(os.path.dirname(os.path.dirname(d))), "rev-parse", "HEAD"),
    "image_id": sh("podman", "images", "--format", "{{.ID}}", "localhost/chromium-bench").split("\n")[0],
    "contention_note": "shared dev box: verify loadavg_at_start; cells measured under load are not publishable",
}
json.dump(info, open(os.path.join(d, "hostinfo.json"), "w"), indent=1)
EOF
}

# ---------------------------------------------------------------------------
# cleanup
# ---------------------------------------------------------------------------
declare -a HOSTSRV_PIDS=()
ORIG_HUGEPAGES=$(cat /proc/sys/vm/nr_hugepages 2>/dev/null || echo 0)
HUGE_CHANGED=0

# ---------------------------------------------------------------------------
# continuous load sampling
# ---------------------------------------------------------------------------
# This box is shared. Contention silently inflates every number, and "the box was
# quiet" is not a claim anyone should have to take on trust — so load is sampled
# for the WHOLE run and every request can be joined to the load at its timestamp.
# That turns "was this cell contended?" from a memory into a query.
LOAD_PID=""
start_load_sampler() {
    ( while :; do
        read -r l1 l5 l15 _ < /proc/loadavg
        printf '{"ts":%s,"load1":%s,"load5":%s,"load15":%s,"fc_procs":%s}\n' \
            "$(now)" "$l1" "$l5" "$l15" "$(pgrep -c firecracker 2>/dev/null || echo 0)" \
            >> "$RESULTS/samples/loadavg.jsonl"
        sleep 5
      done ) &
    LOAD_PID=$!
    log "load sampler started (pid $LOAD_PID) -> samples/loadavg.jsonl"
}

cleanup() {
    local rc=$?
    log "cleanup (rc=$rc)"
    [ -n "${LOAD_PID:-}" ] && kill "$LOAD_PID" 2>/dev/null || true
    # our clones / baselines still alive (state file names carry the run id)
    for f in "$STATE_DIR"/*.json; do
        [ -e "$f" ] || continue
        local nm pid
        nm=$(jq -r '.name // ""' "$f" 2>/dev/null) || continue
        pid=$(jq -r '.pid // ""' "$f" 2>/dev/null) || continue
        case "$nm" in
            cb-*) [ -n "$pid" ] && { kill "$pid" 2>/dev/null || sudo -n kill "$pid" 2>/dev/null || true; } ;;
        esac
    done
    if [ -f "$RESULTS/serve.pids" ]; then
        while IFS=: read -r s p; do
            [ -n "$p" ] || continue
            if [ "$s" = sudo ]; then sudo -n kill "$p" 2>/dev/null || true; else kill "$p" 2>/dev/null || true; fi
        done < "$RESULTS/serve.pids"
    fi
    for p in "${HOSTSRV_PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
    podman ps -a --format '{{.Names}}' 2>/dev/null | grep '^cbpool-' | xargs -r podman rm -f >/dev/null 2>&1 || true
    cg_cleanup_all
    if [ "$HUGE_CHANGED" = 1 ]; then
        log "restoring nr_hugepages=$ORIG_HUGEPAGES"
        "$REPO_ROOT/scripts/hugepage-pool-lock.sh" sudo -n sh -c "echo $ORIG_HUGEPAGES > /proc/sys/vm/nr_hugepages" || true
    fi
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# mode table
# ---------------------------------------------------------------------------
# mode -> "snapkey sudo urlhost"   (snapkey: which golden snapshot; urlhost for fixtures)
declare -A MODE_SNAP MODE_SUDO MODE_HOST MODE_AVAIL MODE_REASON
init_modes() {
    MODE_SNAP=([rootless-proxy]=rootless [rootless-pasta]=noredir
               [rootless-proxy6]=rootless [rootless-pasta6]=noredir
               [bridged]=bridged [routed]=routed)
    MODE_SUDO=([rootless-proxy]="" [rootless-pasta]="" [rootless-proxy6]="" [rootless-pasta6]=""
               [bridged]=sudo [routed]=sudo)
    MODE_HOST=([rootless-proxy]=$HOST4 [rootless-pasta]=$HOST4
               [rootless-proxy6]="[$HOST6]" [rootless-pasta6]="[$HOST6]"
               [bridged]=$HOST4 [routed]="[$HOST6]")
    for m in "${!MODE_SNAP[@]}"; do MODE_AVAIL[$m]=yes; MODE_REASON[$m]=""; done
    if [ -z "${HOST6:-}" ]; then
        for m in rootless-proxy6 rootless-pasta6 routed; do
            MODE_AVAIL[$m]=no; MODE_REASON[$m]="host has no global IPv6 address"
        done
    fi
    if [ "$SKIP_SUDO" = 1 ] || ! sudo -n true 2>/dev/null; then
        for m in bridged routed; do
            MODE_AVAIL[$m]=no; MODE_REASON[$m]="passwordless sudo unavailable (or SKIP_SUDO=1)"
        done
    fi
}

mode_urls() {  # $1 mode -> "label url" lines (http minimal/medium/heavy + https medium)
    local h=${MODE_HOST[$1]}
    printf '%s\n' \
        "minimal http://$h:$HTTP_PORT/minimal.html" \
        "medium http://$h:$HTTP_PORT/medium.html" \
        "heavy http://$h:$HTTP_PORT/heavy.html" \
        "medium-https https://$h:$HTTPS_PORT/medium.html"
}

# ---------------------------------------------------------------------------
# per-clone cgroups — the matched memory-accounting basis (defect 1)
# ---------------------------------------------------------------------------
# The refuted density claim summed PSS over firecracker processes only and
# compared it against a whole container cgroup. Here EVERY process of a clone
# (fcvm supervisor, firecracker, pasta, the unshare holder, and anything they
# spawn) is placed in one leaf cgroup before fcvm is exec'd, so both bases —
# cgroup memory.current and PSS over the cgroup's process set — cover the same
# process set as podman's container cgroup on the comparator side. cgroup
# membership is inherited across fork and survives setuid/unshare, so nothing
# can escape the accounting by reparenting.
CG_BASE=/sys/fs/cgroup/cbbench-$RUNID.slice
CG_OK=0

cg_setup() {
    sudo -n mkdir -p "$CG_BASE" 2>/dev/null || { log "cgroup accounting unavailable (mkdir)"; return 1; }
    sudo -n sh -c "echo '+memory' > $CG_BASE/cgroup.subtree_control" 2>/dev/null || {
        log "cgroup accounting unavailable (subtree_control)"; return 1; }
    CG_OK=1
    log "per-clone cgroup accounting enabled at $CG_BASE"
}

cg_new() {  # $1 leaf name — create and echo the path
    [ "$CG_OK" = 1 ] || return 1
    sudo -n mkdir -p "$CG_BASE/$1" 2>/dev/null || return 1
    printf '%s' "$CG_BASE/$1"
}

cg_join() {  # $1 leaf name — move the CALLING shell (and thus its exec'd child)
    [ "$CG_OK" = 1 ] || return 0
    sudo -n sh -c "echo $BASHPID > $CG_BASE/$1/cgroup.procs" 2>/dev/null || return 0
}

cg_rm() {  # $1 leaf name
    [ "$CG_OK" = 1 ] || return 0
    sudo -n rmdir "$CG_BASE/$1" 2>/dev/null || true
}

cg_cleanup_all() {
    [ "$CG_OK" = 1 ] || return 0
    for d in "$CG_BASE"/*/; do [ -d "$d" ] && sudo -n rmdir "$d" 2>/dev/null || true; done
    sudo -n rmdir "$CG_BASE" 2>/dev/null || true
}

# quiescent machine baseline for the independent MemAvailable-delta basis
mem_baseline() {  # $1 label
    sync
    sleep 3
    python3 "$SCRIPT_DIR/report.py" sample --extra "\"phase\":\"baseline\",\"label\":\"$1\"" \
        >> "$RESULTS/samples/baselines.jsonl" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# fcvm helpers
# ---------------------------------------------------------------------------
fx() {  # fx <sudoflag> <args...>
    local s=$1; shift
    if [ -n "$s" ]; then sudo env RUST_LOG=$FCVM_LOG "$FCVM" "$@"
    else RUST_LOG=$FCVM_LOG "$FCVM" "$@"; fi
}

state_pid_by_name() {  # $1 vm name -> pid or empty
    jq -r --arg n "$1" 'select(.name==$n) | .pid // empty' "$STATE_DIR"/*.json 2>/dev/null | head -1
}

serve_pid_by_snap() {  # $1 snapshot tag
    jq -r --arg s "$1" 'select(.config.process_type=="serve" and .config.snapshot_name==$s) | .pid // empty' \
        "$STATE_DIR"/*.json 2>/dev/null | head -1
}

wait_state_gone() {  # $1 name, $2 timeout_s
    local t0=$SECONDS
    while [ -n "$(state_pid_by_name "$1")" ]; do
        [ $((SECONDS - t0)) -ge "$2" ] && return 1
        sleep 0.3
    done
    return 0
}

snapshot_exists() { [ -d "$SNAP_DIR/$1" ]; }

image_id() { podman images --format '{{.ID}}' "$IMAGE" 2>/dev/null | head -1; }

golden_tag() {  # $1 snapkey -> content-addressed tag
    local id; id=$(image_id)
    local h; h=$(printf '%s' "$id|$CPU|$MEM|$1|v1" | sha256sum | cut -c1-8)
    printf 'cb-golden-%s-%s' "$1" "$h"
}

# ---------------------------------------------------------------------------
# per-request driver (runs inside the clone's container via snapshot run --exec)
# NOTE: must contain NO single quotes — it travels single-quoted through
# shell_words inside the --exec string.
# ---------------------------------------------------------------------------
DRIVER_TEMPLATE='import json,os,socket,sys,time,runpy
print("BENCH_EXEC_UP",flush=True)
t0=time.monotonic()
while True:
    try:
        s=socket.create_connection(("@PHOST@",@PPORT@),0.25); s.close(); break
    except OSError:
        if time.monotonic()-t0>20:
            print("BENCH_NET_FAIL",flush=True); sys.exit(3)
        time.sleep(0.01)
print("BENCH_NET_UP wait_ms=%.1f"%((time.monotonic()-t0)*1000),flush=True)
sys.argv=["render.py","@URL@","--out-prefix","/tmp/bench-req","--format","@FMT@","--quality","@QUAL@"]
rc=0
try:
    runpy.run_path("/opt/bench/render.py",run_name="__main__")
except SystemExit as e:
    rc=int(e.code or 0)
if rc==0:
    try:
        g=runpy.run_path("/opt/bench/render.py")
        dl=time.monotonic()+10
        ws=g["WsClient"](g["find_page_ws_url"]("127.0.0.1:9222",dl),dl)
        cdp=g["Cdp"](ws)
        r=cdp.cmd("Runtime.evaluate",{"expression":"JSON.stringify(performance.getEntriesByType(\"navigation\")[0]||{})","returnByValue":True},deadline=dl)
        nav=json.loads(r["result"]["value"])
        def dd(a,b):
            return max(0.0,nav.get(a,0)-nav.get(b,0))
        tls=0.0
        if nav.get("secureConnectionStart",0)>0:
            tls=dd("connectEnd","secureConnectionStart")
        print("NAV_TIMING dns_ms=%.1f connect_ms=%.1f tls_ms=%.1f ttfb_ms=%.1f resp_ms=%.1f load_ms=%.1f"%(dd("domainLookupEnd","domainLookupStart"),dd("connectEnd","connectStart"),tls,dd("responseStart","requestStart"),dd("responseEnd","responseStart"),nav.get("loadEventEnd",0.0)),flush=True)
        ws.close()
    except Exception as e:
        print("NAV_TIMING_FAIL %r"%e,flush=True)
    try:
        rss=0;pss=0;nproc=0
        for pid in os.listdir("/proc"):
            if not pid.isdigit(): continue
            try:
                cm=open("/proc/"+pid+"/cmdline","rb").read().decode("utf8","replace")
            except OSError:
                continue
            if "chrom" not in cm: continue
            nproc+=1
            try:
                for ln in open("/proc/"+pid+"/status"):
                    if ln.startswith("VmRSS:"): rss+=int(ln.split()[1])
            except OSError:
                pass
            try:
                for ln in open("/proc/"+pid+"/smaps_rollup"):
                    if ln.startswith("Pss:"): pss+=int(ln.split()[1])
            except OSError:
                pass
        print("CHROME_MEM rss_kb=%d pss_kb=%d procs=%d"%(rss,pss,nproc),flush=True)
    except Exception as e:
        print("CHROME_MEM_FAIL %r"%e,flush=True)
print("BENCH_HOLD_START hold_s=@HOLD@",flush=True)
time.sleep(@HOLD@)
sys.exit(rc)'

# Pure-orchestration control probe: restore -> exec -> teardown, no egress and
# no render. Interleaved into the SAME schedule as the real cells so wall-clock
# drift in the orchestration path is measured, not assumed away. Emits the same
# BENCH_EXEC_UP marker so the restore/exec split parses identically.
# Shell-only probe: no interpreter to start, so (this) minus (control-noop)
# isolates the driver's Python startup from fcvm's exec path.
NOOPSH_TEMPLATE='echo BENCH_EXEC_UP; echo BENCH_NET_UP wait_ms=0.0; echo RENDER_OK url=noop-sh connect_ms=0.0 navigate_ms=0.0 idle_ms=0.0 idle_timeout=0 screenshot_ms=0.0 dom_ms=0.0 total_ms=0.0 shot_fmt=none shot_quality=0 png_bytes=0 dom_bytes=0 png=none dom=none'

NOOP_TEMPLATE='import time
print("BENCH_EXEC_UP",flush=True)
print("BENCH_NET_UP wait_ms=0.0",flush=True)
print("RENDER_OK url=noop connect_ms=0.0 navigate_ms=0.0 idle_ms=0.0 idle_timeout=0 screenshot_ms=0.0 dom_ms=0.0 total_ms=0.0 shot_fmt=none shot_quality=0 png_bytes=0 dom_bytes=0 png=none dom=none",flush=True)'

probe_host_of() {  # $1 url -> host for the tcp probe (strip scheme/brackets)
    local rest=${1#*://}; rest=${rest%%/*}
    if [[ $rest == \[* ]]; then printf '%s' "${rest#\[}" | cut -d] -f1
    else printf '%s' "${rest%%:*}"; fi
}
probe_port_of() {  # $1 url
    local rest=${1#*://}; rest=${rest%%/*}
    if [[ $rest == \[* ]]; then printf '%s' "${rest##*]:}"
    else printf '%s' "${rest##*:}"; fi
}

# build_driver <url> <fmt> <quality> -> echoes the in-guest driver source.
# url == "noop" selects the pure-orchestration control probe.
# -> the full --exec command string for this cell.
#   noop-sh : shell only. Isolates fcvm's exec path from the driver's Python
#             interpreter startup, so the ~240ms between GO and first output is
#             attributed to the right side of the boundary rather than guessed.
#   noop    : python driver with no egress and no render (orchestration drift)
build_driver() {
    local url=$1 fmt=$2 qual=$3 hold=${4:-0}
    if [ "$url" = noop-sh ]; then printf 'sh -c %s%s%s' "'" "$NOOPSH_TEMPLATE" "'"; return; fi
    local code
    if [ "$url" = noop ]; then
        code=$NOOP_TEMPLATE
    else
        local phost pport
        phost=$(probe_host_of "$url"); pport=$(probe_port_of "$url")
        code=${DRIVER_TEMPLATE//@PHOST@/$phost}
        code=${code//@PPORT@/$pport}
        code=${code//@URL@/$url}
        code=${code//@FMT@/$fmt}
        code=${code//@QUAL@/$qual}
        code=${code//@HOLD@/$hold}
    fi
    printf 'python3 -c %s%s%s' "'" "$code" "'"
}

# req <logbase> <sudoflag> <memarm uffd|file> <tag> <servepid|-> <url|noop> <name>
#     [fmt] [quality] [uffd_mode]
# One full shared-nothing request: restore clone -> exec driver -> teardown,
# with the whole process set confined to one leaf cgroup so its memory can be
# attributed on the same basis as a container (defect 1).
req() {
    local logbase=$1 sudoflag=$2 memarm=$3 tag=$4 servepid=$5 url=$6 name=$7
    local fmt=${8:-$SHOT_FMT} qual=${9:-$SHOT_QUALITY} hold=${10:-0}
    local lf=$RESULTS/requests/$logbase.log
    # build_driver returns the COMPLETE --exec command string (interpreter
    # included), so it is passed through verbatim. Wrapping it in another
    # `python3 -c '...'` here produced `python3 -c 'python3 -c ...'` and a
    # SyntaxError in every request.
    local execcmd; execcmd=$(build_driver "$url" "$fmt" "$qual" "$hold")
    local -a src
    if [ "$memarm" = uffd ]; then src=(--pid "$servepid"); else src=(--snapshot "$tag"); fi
    # Per-clone cgroups cost one sudo call per request, so they are enabled only
    # where they are the measurement (REQ_CGROUP=1, the density/fan-out phases).
    # The latency matrix runs without them and carries zero added overhead.
    local leaf=""
    if [ "${REQ_CGROUP:-0}" = 1 ]; then
        leaf="req-$name"
        cg_new "$leaf" >/dev/null 2>&1 || leaf=""
    fi
    printf '%s BENCH_SCHED cgroup=%s fmt=%s\n' "$(now)" "${leaf:-none}" "$fmt" > "$lf"
    local rc=0
    if [ -n "$leaf" ]; then
        # The subshell joins the cgroup and then execs; every descendant
        # inherits, and inheritance survives fork/setuid/unshare, so nothing can
        # escape the accounting by reparenting. BENCH_T0 is emitted after the
        # join so the join's sudo cost is never charged to the restore stage.
        # `echo` then `exec`: bash flushes builtin output to the pipe before
        # exec, so BENCH_T0 is timestamped live by TSPY at the instant the
        # cgroup join finished — verified, not assumed.
        (
            cg_join "$leaf"
            echo BENCH_T0
            if [ -n "$sudoflag" ]; then
                exec timeout -k 10 "$REQ_TIMEOUT" \
                    sudo env RUST_LOG=$FCVM_LOG "$FCVM" snapshot run "${src[@]}" --name "$name" \
                    --no-dirty-tracking --no-swap --exec "$execcmd" 2>&1
            else
                exec timeout -k 10 "$REQ_TIMEOUT" \
                    env RUST_LOG=$FCVM_LOG "$FCVM" snapshot run "${src[@]}" --name "$name" \
                    --no-dirty-tracking --no-swap --exec "$execcmd" 2>&1
            fi
        ) | python3 -c "$TSPY" >> "$lf" || rc=$?
    else
        printf '%s BENCH_T0\n' "$(now)" >> "$lf"
        if [ -n "$sudoflag" ]; then
            timeout -k 10 "$REQ_TIMEOUT" \
                sudo env RUST_LOG=$FCVM_LOG "$FCVM" snapshot run "${src[@]}" --name "$name" \
                --no-dirty-tracking --no-swap --exec "$execcmd" 2>&1 \
                | python3 -c "$TSPY" >> "$lf" || rc=$?
        else
            timeout -k 10 "$REQ_TIMEOUT" \
                env RUST_LOG=$FCVM_LOG "$FCVM" snapshot run "${src[@]}" --name "$name" \
                --no-dirty-tracking --no-swap --exec "$execcmd" 2>&1 \
                | python3 -c "$TSPY" >> "$lf" || rc=$?
        fi
    fi
    printf '%s BENCH_EXIT rc=%d\n' "$(now)" "$rc" >> "$lf"
    [ -n "$leaf" ] && cg_rm "$leaf"
    return 0
}

# ---------------------------------------------------------------------------
# host fixture servers ("simulated external site")
# ---------------------------------------------------------------------------
start_host_servers() {
    if ! [ -f "$RESULTS/cert.pem" ]; then
        openssl req -x509 -newkey rsa:2048 -keyout "$RESULTS/key.pem" -out "$RESULTS/cert.pem" \
            -days 7 -nodes -subj "/CN=chromium-bench" 2>/dev/null
    fi
    # disown: these live for the whole run — they must NOT sit in the job table
    # or every bare `wait`/`jobs -r` in the fan-out phases includes them (deadlock)
    python3 "$SCRIPT_DIR/hostserver.py" --root "$PAGES_DIR" --port "$HTTP_PORT" \
        > "$RESULTS/logs/hostserver-http.log" 2>&1 &
    HOSTSRV_PIDS+=($!); disown $!
    python3 "$SCRIPT_DIR/hostserver.py" --root "$PAGES_DIR" --port "$HTTPS_PORT" \
        --certfile "$RESULTS/cert.pem" --keyfile "$RESULTS/key.pem" \
        > "$RESULTS/logs/hostserver-https.log" 2>&1 &
    HOSTSRV_PIDS+=($!); disown $!
    sleep 0.7
    curl -sf -o /dev/null "http://$HOST4:$HTTP_PORT/minimal.html" || die "host http server unreachable"
    curl -skf -o /dev/null "https://$HOST4:$HTTPS_PORT/minimal.html" || die "host https server unreachable"
    log "host fixture servers up: http://$HOST4:$HTTP_PORT https://$HOST4:$HTTPS_PORT"
}

# ---------------------------------------------------------------------------
# phase 0: availability probes + config record
# ---------------------------------------------------------------------------
HUGE_AVAIL=yes HUGE_REASON=""
phase0() {
    init_modes
    write_hostinfo
    # root podman needs the image for sudo modes (identical bytes via save|load)
    if [ "${MODE_AVAIL[bridged]}" = yes ]; then
        local uid rid
        uid=$(image_id)
        rid=$(sudo podman images --format '{{.ID}}' "$IMAGE" 2>/dev/null | head -1)
        if [ "$uid" != "$rid" ]; then
            log "syncing $IMAGE into root podman storage (save|load)"
            if ! podman save "$IMAGE" | sudo podman load >/dev/null 2>&1; then
                MODE_AVAIL[bridged]=no; MODE_REASON[bridged]="image sync to root podman failed"
                MODE_AVAIL[routed]=no;  MODE_REASON[routed]="image sync to root podman failed"
            fi
        fi
    fi
    # hugepage pool for the huge cells (restored on exit)
    if sudo -n true 2>/dev/null; then
        local cur; cur=$(cat /proc/sys/vm/nr_hugepages)
        if [ "$cur" -lt "$HUGEPAGE_POOL" ]; then
            "$REPO_ROOT/scripts/hugepage-pool-lock.sh" sudo sh -c "echo $HUGEPAGE_POOL > /proc/sys/vm/nr_hugepages"
            HUGE_CHANGED=1
            cur=$(cat /proc/sys/vm/nr_hugepages)
        fi
        if [ "$cur" -lt 1024 ]; then
            HUGE_AVAIL=no HUGE_REASON="could not allocate 2MB hugepage pool (got $cur pages)"
        elif [ "$cur" -lt "$HUGEPAGE_POOL" ]; then
            log "WARNING: hugepage pool only $cur/$HUGEPAGE_POOL pages (fragmentation); huge fan-out will be capped"
        fi
    else
        HUGE_AVAIL=no HUGE_REASON="hugepage pool needs sudo"
    fi
    {
        echo '{'
        echo ' "modes": {'
        local first=1
        for m in rootless-proxy rootless-pasta rootless-proxy6 rootless-pasta6 bridged routed; do
            [ $first = 1 ] || echo ','
            first=0
            printf '  "%s": {"available": "%s", "reason": "%s"}' "$m" "${MODE_AVAIL[$m]}" "${MODE_REASON[$m]}"
        done
        echo ''
        echo ' },'
        printf ' "hugepages": {"available": "%s", "reason": "%s", "pool_pages": %s},\n' \
            "$HUGE_AVAIL" "$HUGE_REASON" "$(cat /proc/sys/vm/nr_hugepages)"
        printf ' "file_huge_cell": "UNAVAILABLE as file-backed: snapshot.rs requires UFFD for hugepage snapshots (Firecracker rejects the File memory backend); measured as the implicit per-clone UFFD fallback instead",\n'
        printf ' "http_port": %s, "https_port": %s\n' "$HTTP_PORT" "$HTTPS_PORT"
        echo '}'
    } > "$RESULTS/availability.json"
    log "availability: $(jq -c '.modes | map_values(.available)' "$RESULTS/availability.json")"
}

# ---------------------------------------------------------------------------
# phase 1: golden snapshots (one per network mode + hugepages variant)
# ---------------------------------------------------------------------------
# boot_golden <snapkey> <sudoflag> <extra flags...>  -> creates tag if missing
boot_golden() {
    local key=$1 sudoflag=$2; shift 2
    local tag; tag=$(golden_tag "$key")
    if snapshot_exists "$tag" && [ "$REBUILD" != 1 ]; then
        log "golden $key: reusing snapshot $tag"
        return 0
    fi
    if snapshot_exists "$tag"; then
        log "golden $key: REBUILD requested, deleting $tag"
        fx "$sudoflag" snapshots delete -f "$tag" >/dev/null || true
    fi
    local name=cb-g-$key-$RUNID
    local lf=$RESULTS/logs/prep-$key.log
    log "golden $key: cold boot ($name) -> $tag"
    printf '%s BENCH_T0\n' "$(now)" > "$lf"
    if [ -n "$sudoflag" ]; then
        sudo env FCVM_NO_SNAPSHOT=1 RUST_LOG=$FCVM_LOG "$FCVM" podman run --name "$name" \
            --cpu "$CPU" --mem "$MEM" "$@" "$IMAGE" 2>&1 | python3 -c "$TSPY" >> "$lf" &
    else
        FCVM_NO_SNAPSHOT=1 RUST_LOG=$FCVM_LOG "$FCVM" podman run --name "$name" \
            --cpu "$CPU" --mem "$MEM" "$@" "$IMAGE" 2>&1 | python3 -c "$TSPY" >> "$lf" &
    fi
    local t0=$SECONDS
    until grep -q CHROMIUM_BENCH_READY "$lf" 2>/dev/null; do
        if [ $((SECONDS - t0)) -ge 300 ]; then
            log "golden $key: BOOT TIMEOUT; tail:"; tail -5 "$lf" | sed 's/^/    /'
            return 1
        fi
        if grep -qE 'ERROR fcvm: Error' "$lf" 2>/dev/null; then
            log "golden $key: BOOT FAILED; tail:"; tail -5 "$lf" | sed 's/^/    /'
            return 1
        fi
        sleep 1
    done
    local pid; pid=$(state_pid_by_name "$name")
    [ -n "$pid" ] || { log "golden $key: no state pid"; return 1; }
    printf '%s BENCH_READY pid=%s\n' "$(now)" "$pid" >> "$lf"

    # pasta arms: flush the guest REDIRECT rules so egress uses pasta natively,
    # BEFORE the snapshot (iptables state is part of guest memory).
    if [ "$key" = noredir ]; then
        fx "$sudoflag" exec --pid "$pid" --vm -- iptables -t nat -F OUTPUT >> "$lf" 2>&1
        fx "$sudoflag" exec --pid "$pid" --vm -- ip6tables -t nat -F OUTPUT >> "$lf" 2>&1 || \
            log "golden $key: ip6tables flush failed (no guest v6 nat?)"
        printf '%s BENCH_REDIRECT_FLUSHED\n' "$(now)" >> "$lf"
    fi

    # egress verification: render one host-served fixture through this mode's path
    local vhost=$HOST4
    case "$key" in routed) vhost="[$HOST6]" ;; esac
    local vurl="http://$vhost:$HTTP_PORT/minimal.html"
    printf '%s BENCH_VERIFY_START url=%s\n' "$(now)" "$vurl" >> "$lf"
    if ! fx "$sudoflag" exec --pid "$pid" -c -- python3 /opt/bench/render.py "$vurl" \
            --out-prefix /tmp/verify 2>&1 | python3 -c "$TSPY" >> "$lf"; then
        log "golden $key: egress verification FAILED; tail:"; tail -5 "$lf" | sed 's/^/    /'
        if [ -n "$sudoflag" ]; then sudo kill "$pid" || true; else kill "$pid" || true; fi
        return 1
    fi
    grep -q RENDER_OK "$lf" || { log "golden $key: no RENDER_OK in verification"; return 1; }

    fx "$sudoflag" snapshot create --pid "$pid" --tag "$tag" >> "$lf" 2>&1
    printf '%s BENCH_SNAPSHOTTED tag=%s\n' "$(now)" "$tag" >> "$lf"
    if [ -n "$sudoflag" ]; then sudo kill "$pid"; else kill "$pid"; fi
    wait_state_gone "$name" 60 || log "golden $key: baseline did not exit cleanly"
    log "golden $key: done ($tag)"
}

NOISO_AVAIL=yes
phase1() {
    log "=== phase 1: golden snapshots ==="
    boot_golden rootless "" || die "rootless golden failed"
    boot_golden noredir "" || die "noredir golden failed"
    # site-isolation-off variant: the renderer process structure is baked into
    # guest memory, so it must be a separate golden snapshot, not a per-request
    # flag. Measured here on merged main rather than inherited from the old run.
    boot_golden noiso "" --env CB_SITE_ISOLATION=off || {
        NOISO_AVAIL=no; log "noiso golden failed — site-isolation arm will be absent"; }
    if [ "$HUGE_AVAIL" = yes ]; then
        boot_golden huge "" --hugepages || { HUGE_AVAIL=no; HUGE_REASON="hugepages golden boot failed (see logs/prep-huge.log)"; }
    fi
    if [ "${MODE_AVAIL[bridged]}" = yes ]; then
        boot_golden bridged sudo --network bridged || { MODE_AVAIL[bridged]=no; MODE_REASON[bridged]="golden boot failed (see logs/prep-bridged.log)"; }
    fi
    if [ "${MODE_AVAIL[routed]}" = yes ]; then
        boot_golden routed sudo --network routed || { MODE_AVAIL[routed]=no; MODE_REASON[routed]="golden boot failed (see logs/prep-routed.log)"; }
    fi
    # refresh availability with any prep failures
    python3 - "$RESULTS/availability.json" \
        "bridged=${MODE_AVAIL[bridged]}:${MODE_REASON[bridged]}" \
        "routed=${MODE_AVAIL[routed]}:${MODE_REASON[routed]}" \
        "hugepages=$HUGE_AVAIL:$HUGE_REASON" <<'EOF'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
for kv in sys.argv[2:]:
    k, v = kv.split("=", 1); avail, reason = v.split(":", 1)
    tgt = d["modes"].get(k, d.get(k) if k != "hugepages" else None)
    if k == "hugepages":
        d["hugepages"]["available"] = avail
        if reason: d["hugepages"]["reason"] = reason
    else:
        d["modes"][k] = {"available": avail, "reason": reason}
json.dump(d, open(p, "w"), indent=1)
EOF
}

# ---------------------------------------------------------------------------
# serve management (phase 2, on demand)
# ---------------------------------------------------------------------------
# A serve is identified by (snapshot tag, uffd mode). Reusing one across modes
# is what silently made the MINOR arm measure a COPY server, so the mode a
# running serve was started in is tracked explicitly rather than inferred.
declare -A SERVE_MODE_BY_TAG=()
declare -A SERVE_PID_BY_TAG=()

# `[ -d /proc/<pid> ]`, never `kill -0`: a non-root shell probing a sudo-owned
# serve gets EPERM, which `kill -0` reports as failure and would read as "gone".
serve_alive() { [ -n "${1:-}" ] && [ "$1" != - ] && [ -d "/proc/$1" ]; }

start_serve() {  # $1 tag, $2 sudoflag, [$3 uffd mode copy|minor] -> echoes fcvm serve pid
    local tag=$1 sudoflag=$2 umode=${3:-copy}
    local existing; existing=$(serve_pid_by_snap "$tag")
    if serve_alive "$existing"; then
        # Reuse ONLY a server this script started in the SAME uffd mode. Anything
        # else (wrong mode, or a server already draining) is retired first —
        # handing back a dying COPY server made every MINOR clone fail to connect.
        if [ "${SERVE_MODE_BY_TAG[$tag]:-}" = "$umode" ] \
           && [ "${SERVE_PID_BY_TAG[$tag]:-}" = "$existing" ]; then
            printf '%s' "$existing"; return 0
        fi
        if [ -z "${SERVE_PID_BY_TAG[$tag]:-}" ]; then
            # A serve for this tag exists that WE did not start. This box is
            # shared; killing it would destroy another run's measurement (and
            # silently corrupt ours). Refuse loudly instead of stopping it.
            die "snapshot '$tag' is already being served by pid $existing, which this run did not start.
     Another workload is using this golden snapshot. Wait for it to finish, or
     run with a different golden tag. Refusing to kill a serve we do not own."
        fi
        stop_serve "$existing" "$sudoflag"
    fi
    local -a umflag=()
    [ "$umode" = minor ] && umflag=(--uffd-mode minor)
    if [ -n "$sudoflag" ]; then
        sudo env RUST_LOG=$FCVM_LOG "$FCVM" snapshot serve "$tag" "${umflag[@]}" \
            > "$RESULTS/logs/serve-$tag-$umode.log" 2>&1 &
    else
        RUST_LOG=$FCVM_LOG "$FCVM" snapshot serve "$tag" "${umflag[@]}" \
            > "$RESULTS/logs/serve-$tag-$umode.log" 2>&1 &
    fi
    local t0=$SECONDS pid=""
    while [ -z "$pid" ]; do
        [ $((SECONDS - t0)) -ge 30 ] && die "serve for $tag did not register in state"
        sleep 0.3
        pid=$(serve_pid_by_snap "$tag")
    done
    printf '%s:%s\n' "${sudoflag:-user}" "$pid" >> "$RESULTS/serve.pids"
    SERVE_MODE_BY_TAG[$tag]=$umode
    SERVE_PID_BY_TAG[$tag]=$pid
    printf '%s' "$pid"
}

stop_serve() {  # $1 pid, $2 sudoflag — returns only once the server is really gone
    local pid=${1:-} sflag=${2:-} tg="" k t0=$SECONDS
    [ -n "$pid" ] && [ "$pid" != - ] || return 0
    for k in "${!SERVE_PID_BY_TAG[@]}"; do
        [ "${SERVE_PID_BY_TAG[$k]}" = "$pid" ] && tg=$k
    done
    if [ -n "$sflag" ]; then sudo kill "$pid" 2>/dev/null || true; else kill "$pid" 2>/dev/null || true; fi
    # Event-driven, not a fixed sleep: the previous `sleep 1` returned while the
    # server was still draining, and the next cell then adopted it by tag.
    # Wait for BOTH the process and its serve state entry to disappear.
    while serve_alive "$pid" || [ "$(serve_pid_by_snap "${tg:-__none__}")" = "$pid" ]; do
        if [ $((SECONDS - t0)) -ge 90 ]; then
            log "stop_serve: pid $pid still present after 90s — continuing"
            break
        fi
        sleep 0.2
    done
    if [ -n "$tg" ]; then unset "SERVE_PID_BY_TAG[$tg]" "SERVE_MODE_BY_TAG[$tg]"; fi
}

prewarm_memory() {  # $1 tag — pull memory.bin into page cache before file-backed fan-out
    if [ -r "$SNAP_DIR/$1/memory.bin" ]; then cat "$SNAP_DIR/$1/memory.bin" > /dev/null
    else sudo cat "$SNAP_DIR/$1/memory.bin" > /dev/null 2>&1 || true; fi
}

# ---------------------------------------------------------------------------
# phase 3: per-request matrix — ONE seeded interleaved schedule
# ---------------------------------------------------------------------------
# Defect 2 fix. The first run executed each egress mode as a contiguous ~47s
# block, so "mode" and "wall-clock" were the same factor and a pure-orchestration
# probe drifted 631->706ms across the blocks. Here every cell of the matrix is
# emitted into one list, shuffled with a fixed seed, and executed request by
# request, with two control arms mixed into the SAME shuffled stream:
#   control-noop     restore -> exec -> teardown, no egress, no render
#                    (measures orchestration drift directly)
#   control-inguest  full render of the byte-identical fixture served inside the
#                    guest (measures render-without-egress)
# Every request records BENCH_SCHED, so drift is a measurable regressor rather
# than an assumption.
build_schedule() {   # -> tab-separated cells on stdout
    local out=$RESULTS/schedule.tsv
    : > "$out.in"
    local m label url i fmode=${FANOUT_MODE:-rootless-proxy}
    # (1) egress matrix: every available mode x every fixture page, uffd arm
    for m in rootless-proxy rootless-pasta rootless-proxy6 rootless-pasta6 bridged routed; do
        [ "${MODE_AVAIL[$m]}" = yes ] || { log "phase3: $m UNAVAILABLE (${MODE_REASON[$m]})"; continue; }
        snapshot_exists "$(golden_tag "${MODE_SNAP[$m]}")" || continue
        while read -r label url; do
            for i in $(seq 1 "$R"); do
                printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
                    "$m" "uffd-4k" "uffd" "${MODE_SNAP[$m]}" "$url" "$label" "$SHOT_FMT" "$i" >> "$out.in"
            done
        done < <(mode_urls "$m")
    done
    # (2) file-backed arm, same pages, same interleave
    if snapshot_exists "$(golden_tag "${MODE_SNAP[$fmode]}")"; then
        while read -r label url; do
            for i in $(seq 1 "$R"); do
                printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
                    "$fmode" "file-4k" "file" "${MODE_SNAP[$fmode]}" "$url" "$label" "$SHOT_FMT" "$i" >> "$out.in"
            done
        done < <(mode_urls "$fmode")
    fi
    # (3) screenshot-format arm: png vs jpeg on the same page, same mode
    for f in png jpeg; do
        for i in $(seq 1 "$R"); do
            printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
                "$fmode" "shot-$f" "uffd" "${MODE_SNAP[$fmode]}" \
                "http://${MODE_HOST[$fmode]}:$HTTP_PORT/medium.html" "medium" "$f" "$i" >> "$out.in"
        done
    done
    # (4) site-isolation arm: on (the normal golden) vs off (its own golden)
    if [ "$NOISO_AVAIL" = yes ] && snapshot_exists "$(golden_tag noiso)"; then
        for i in $(seq 1 "$R"); do
            printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
                "$fmode" "siteiso-off" "uffd" "noiso" \
                "http://${MODE_HOST[$fmode]}:$HTTP_PORT/medium.html" "medium" "$SHOT_FMT" "$i" >> "$out.in"
            printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
                "$fmode" "siteiso-on" "uffd" "${MODE_SNAP[$fmode]}" \
                "http://${MODE_HOST[$fmode]}:$HTTP_PORT/medium.html" "medium" "$SHOT_FMT" "$i" >> "$out.in"
        done
    fi
    # (5) controls, interleaved into the same stream
    for i in $(seq 1 "$R"); do
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$fmode" "control-noop" "uffd" "${MODE_SNAP[$fmode]}" "noop" "noop" "none" "$i" >> "$out.in"
        # shell-only probe: no interpreter to start, so (control-noop) minus
        # (control-noop-sh) is the driver's Python startup and the remainder is
        # fcvm's own exec path. Without it the ~220 ms GO->first-output gap is
        # attributed by assertion rather than by measurement.
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$fmode" "control-noop-sh" "uffd" "${MODE_SNAP[$fmode]}" "noop-sh" "noopsh" "none" "$i" >> "$out.in"
    done
    for label in minimal medium heavy; do
        for i in $(seq 1 "$R_CONTROL"); do
            printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
                "$fmode" "control-inguest" "uffd" "${MODE_SNAP[$fmode]}" \
                "http://127.0.0.1:8000/$label.html" "$label" "$SHOT_FMT" "$i" >> "$out.in"
        done
    done
    python3 - "$out.in" "$out" "$SEED" <<'PYSHUF'
import random, sys
src, dst, seed = sys.argv[1], sys.argv[2], int(sys.argv[3])
rows = [l for l in open(src).read().splitlines() if l.strip()]
random.Random(seed).shuffle(rows)
open(dst, "w").write("\n".join(rows) + "\n")
print(f"schedule: {len(rows)} requests, seed={seed}", file=sys.stderr)
PYSHUF
    rm -f "$out.in"
}

phase3() {
    log "=== phase 3: interleaved per-request matrix (R=$R, seed=$SEED) ==="
    build_schedule
    local total; total=$(grep -c . "$RESULTS/schedule.tsv")
    log "phase3: $total requests in one shuffled stream"

    # every snapshot the schedule references needs its serve running for the
    # WHOLE phase — that is what makes interleaving possible at all
    declare -A SPID=()
    local key tag sflag spid
    for key in $(cut -f4 "$RESULTS/schedule.tsv" | sort -u); do
        tag=$(golden_tag "$key")
        sflag=""
        case "$key" in bridged|routed) sflag=sudo ;; esac
        spid=$(start_serve "$tag" "$sflag")
        [ -n "$spid" ] || die "phase3: serve failed for $tag"
        SPID[$key]=$spid
        prewarm_memory "$tag"
        log "phase3: serve $key ($tag) pid=$spid"
    done

    local n=0 mode arm memarm snapkey url label fmt rep
    while IFS=$'\t' read -r mode arm memarm snapkey url label fmt rep; do
        n=$((n + 1))
        tag=$(golden_tag "$snapkey")
        sflag=${MODE_SUDO[$mode]}
        req "p3__${mode}__${arm}__${label}__r${rep}" "$sflag" "$memarm" "$tag" "${SPID[$snapkey]}" \
            "$url" "cb-$RUNID-p3-$n" "$fmt" "$SHOT_QUALITY"
        if [ $((n % 50)) = 0 ]; then log "phase3: $n/$total"; fi
    done < "$RESULTS/schedule.tsv"

    for key in "${!SPID[@]}"; do
        sflag=""; case "$key" in bridged|routed) sflag=sudo ;; esac
        stop_serve "${SPID[$key]}" "$sflag"
    done
    log "phase3: done ($n requests)"
}

# ---------------------------------------------------------------------------
# phase 4: memory density + throughput
# ---------------------------------------------------------------------------
# Defect 1 fix: density is measured over EVERY process of each clone (per-clone
# cgroup) AND as a whole-machine MemAvailable delta from a quiescent baseline.
# Defect 3 fix: every burst cell is repeated BURST_REPS times and the BURST is
# the experimental unit; clones inside one burst are pseudoreplicates.
# Defect 5 fix: the density sweep spans concrete N so slope AND intercept are
# both estimable, instead of quoting an asymptotic slope alone.
sample_once() {  # $1 outfile  $2 extra-json
    python3 "$SCRIPT_DIR/report.py" sample --cgroup-root "$CG_BASE" --cgroup-prefix req- \
        --state-dir "$STATE_DIR" --name-prefix "cb-$RUNID" --extra "$2" >> "$1" 2>/dev/null || true
}

wait_clones_gone() {  # $1 timeout
    local t0=$SECONDS
    while :; do
        local n
        n=$(jq -r --arg p "cb-$RUNID" 'select((.name // "") | startswith($p)) | .name' \
            "$STATE_DIR"/*.json 2>/dev/null | grep -c '' || true)
        [ "${n:-0}" = 0 ] && return 0
        [ $((SECONDS - t0)) -ge "$1" ] && { log "WARN: $n clones still up after ${1}s"; return 1; }
        sleep 1
    done
}

# cell -> memarm/uffd-mode/pages decoding
cell_memarm() { case "$1" in file-*) echo file ;; *) echo uffd ;; esac; }
cell_umode()  { case "$1" in *-minor) echo minor ;; *) echo copy ;; esac; }
cell_pages()  { case "$1" in *huge*) echo huge ;; *) echo 4k ;; esac; }

DENSITY_HOLD=${DENSITY_HOLD:-25}   # seconds a clone idles post-render while sampled

# density_cell <cell> <N> <rep>
# Bring N clones to the post-render steady state simultaneously, sample memory
# on both bases, tear down. Returns with all clones gone.
density_cell() {
    local cell=$1 N=$2 rep=$3
    local memarm umode pages ctag spid sflag=${MODE_SUDO[${FANOUT_MODE:-rootless-proxy}]}
    memarm=$(cell_memarm "$cell"); umode=$(cell_umode "$cell"); pages=$(cell_pages "$cell")
    ctag=$4; spid=$5
    local fmode=${FANOUT_MODE:-rootless-proxy}
    local url="http://${MODE_HOST[$fmode]}:$HTTP_PORT/medium.html"
    local sf=$RESULTS/samples/density.jsonl

    # quiescent baseline immediately before the fan-out (the MemAvailable basis
    # is a DELTA, so it must be re-anchored per measurement, not once per run)
    sync; sleep 2
    sample_once "$sf" "\"cell\":\"$cell\",\"n\":$N,\"rep\":$rep,\"phase\":\"pre\""
    local -a pids=()
    local i
    for i in $(seq 1 "$N"); do
        req "dens${N}__${fmode}__${cell}__medium__r${rep}i${i}" "$sflag" "$memarm" "$ctag" "$spid" \
            "$url" "cb-$RUNID-d$cell-$rep-$i" "$SHOT_FMT" "$SHOT_QUALITY" "$DENSITY_HOLD" &
        pids+=($!)
    done
    # wait for all N to reach the hold point, then sample the steady state
    local t0=$SECONDS held=0
    while [ $((SECONDS - t0)) -lt 90 ]; do
        held=$(grep -l BENCH_HOLD_START "$RESULTS"/requests/dens${N}__${fmode}__${cell}__medium__r${rep}i*.log 2>/dev/null | grep -c '' || true)
        [ "${held:-0}" -ge "$N" ] && break
        sleep 0.5
    done
    if [ "${held:-0}" -lt "$N" ]; then
        log "phase4 density $cell N=$N rep=$rep: only $held/$N reached hold — cell marked incomplete"
        printf '{"phase":"density-incomplete","cell":"%s","n":%s,"rep":%s,"held":%s}\n' \
            "$cell" "$N" "$rep" "${held:-0}" >> "$RESULTS/samples/bursts.jsonl"
    fi
    sleep 2
    for i in 1 2 3; do
        sample_once "$sf" "\"cell\":\"$cell\",\"n\":$N,\"rep\":$rep,\"phase\":\"steady\",\"held\":${held:-0}"
        sleep 1
    done
    wait "${pids[@]}" 2>/dev/null || true
    wait_clones_gone 90 || true
    sleep 2
    sample_once "$sf" "\"cell\":\"$cell\",\"n\":$N,\"rep\":$rep,\"phase\":\"post\""
}

# burst_cell <cell> <N> <rep> <tag> <servepid>
burst_cell() {
    local cell=$1 N=$2 rep=$3 ctag=$4 spid=$5
    local memarm; memarm=$(cell_memarm "$cell")
    local fmode=${FANOUT_MODE:-rootless-proxy}
    local sflag=${MODE_SUDO[$fmode]}
    local urls=() label url
    while read -r label url; do
        case "$label" in medium-https) ;; *) urls+=("$label|$url") ;; esac
    done < <(mode_urls "$fmode")
    local t_burst0; t_burst0=$(now)
    local -a bp=()
    local i k pair
    for i in $(seq 1 "$N"); do
        k=$(( (i - 1) % ${#urls[@]} )); pair=${urls[$k]}
        req "burst${N}__${fmode}__${cell}__${pair%%|*}__r${rep}i${i}" "$sflag" "$memarm" "$ctag" "$spid" \
            "${pair##*|}" "cb-$RUNID-b$cell-$rep-$i" &
        bp+=($!)
    done
    wait "${bp[@]}" 2>/dev/null || true
    printf '{"phase":"burst","cell":"%s","n":%s,"rep":%s,"t0":%s,"t1":%s}\n' \
        "$cell" "$N" "$rep" "$t_burst0" "$(now)" >> "$RESULTS/samples/bursts.jsonl"
    wait_clones_gone 90 || true
    sleep 2
}

phase4() {
    log "=== phase 4: density + throughput (burst reps=$BURST_REPS) ==="
    REQ_CGROUP=1                       # per-clone cgroups ARE the measurement here
    cg_setup || log "phase4: continuing without cgroup basis (PSS + MemAvailable only)"
    local fmode=${FANOUT_MODE:-rootless-proxy}
    local sflag=${MODE_SUDO[$fmode]}
    local tag4k taghuge
    tag4k=$(golden_tag "${MODE_SNAP[$fmode]}")
    taghuge=$(golden_tag huge)
    # The three backends that matter, incl. the newly-merged MINOR mode.
    local -a cells=("file-4k" "uffd-4k-copy" "uffd-4k-minor")
    if [ "$HUGE_AVAIL" = yes ] && snapshot_exists "$taghuge"; then
        cells+=("uffd-huge-minor")
    else
        log "phase4: huge cells UNAVAILABLE ($HUGE_REASON)"
    fi

    local cell memarm umode pages ctag spid N rep
    for cell in "${cells[@]}"; do
        memarm=$(cell_memarm "$cell"); umode=$(cell_umode "$cell"); pages=$(cell_pages "$cell")
        if [ "$pages" = huge ]; then ctag=$taghuge; else ctag=$tag4k; fi
        spid=-
        if [ "$memarm" = uffd ]; then
            spid=$(start_serve "$ctag" "$sflag" "$umode")
            [ -n "$spid" ] || die "phase4 $cell: serve failed for $ctag"
            log "phase4 $cell: serve=$spid uffd-mode=$umode"
        else
            prewarm_memory "$ctag"
            log "phase4 $cell: file-backed (page cache prewarmed)"
        fi

        # ---- density sweep (defect 1 + defect 5)
        for N in $DENSITY_NS; do
            if [ "$pages" = huge ]; then
                local pool free_needed
                pool=$(cat /proc/sys/vm/nr_hugepages)
                free_needed=$(( N * MEM / 2 ))
                if [ "$pool" -lt "$free_needed" ]; then
                    log "phase4 $cell: density N=$N SKIPPED (pool $pool < needed $free_needed)"
                    continue
                fi
            fi
            for rep in $(seq 1 "$DENSITY_REPS"); do
                log "phase4 $cell: density N=$N rep=$rep/$DENSITY_REPS"
                density_cell "$cell" "$N" "$rep" "$ctag" "$spid"
            done
        done

        # ---- repeated bursts (defect 3)
        for N in $BURST_NS; do
            if [ "$pages" = huge ]; then
                local pool2; pool2=$(cat /proc/sys/vm/nr_hugepages)
                [ "$pool2" -lt $(( N * MEM / 2 )) ] && { log "phase4 $cell: burst N=$N SKIPPED (pool)"; continue; }
            fi
            for rep in $(seq 1 "$BURST_REPS"); do
                log "phase4 $cell: burst N=$N rep=$rep/$BURST_REPS"
                burst_cell "$cell" "$N" "$rep" "$ctag" "$spid"
            done
        done

        if [ "$memarm" = uffd ]; then stop_serve "$spid" "$sflag"; fi
    done
    REQ_CGROUP=0
}

# ---------------------------------------------------------------------------
# phase 4b: sustained rate (reported SEPARATELY from bursts — the first run led
# with the burst number while the sustained phase contradicted it)
# ---------------------------------------------------------------------------
phase4b() {
    log "=== phase 4b: sustained rate ==="
    REQ_CGROUP=1
    cg_setup || true
    local fmode=${FANOUT_MODE:-rootless-proxy}
    local sflag=${MODE_SUDO[$fmode]}
    local tag; tag=$(golden_tag "${MODE_SNAP[$fmode]}")
    local urls=() label url
    while read -r label url; do
        case "$label" in medium-https) ;; *) urls+=("$label|$url") ;; esac
    done < <(mode_urls "$fmode")
    local cell rate spid memarm umode
    for cell in file-4k uffd-4k-minor; do
        memarm=$(cell_memarm "$cell"); umode=$(cell_umode "$cell")
        spid=-
        if [ "$memarm" = uffd ]; then spid=$(start_serve "$tag" "$sflag" "$umode"); else prewarm_memory "$tag"; fi
        for rate in $SUST_RATES; do
            local sf=$RESULTS/samples/sustained__${cell}__rate${rate}.jsonl
            log "phase4b $cell: sustained ${rate}rps x ${SUST_SECS}s"
            : > "$sf"
            local t_end=$(( SECONDS + SUST_SECS )) n=0 skipped=0
            local interval; interval=$(python3 -c "print(1.0/$rate)")
            : > "$sf.run"
            (   while [ -e "$sf.run" ]; do
                    sample_once "$sf" "\"cell\":\"$cell\",\"rate\":$rate,\"phase\":\"load\""
                    sleep 2
                done
            ) &
            local sampler=$!
            local -a reqpids=() k pair
            local t_sust0; t_sust0=$(now)
            while [ $SECONDS -lt $t_end ]; do
                local inflight; inflight=$(jobs -r | grep -c '' || true)
                if [ "$inflight" -le "$MAX_INFLIGHT" ]; then
                    n=$((n + 1))
                    k=$(( (n - 1) % ${#urls[@]} )); pair=${urls[$k]}
                    req "sust-r${rate}__${fmode}__${cell}__${pair%%|*}__r$n" "$sflag" "$memarm" "$tag" "$spid" \
                        "${pair##*|}" "cb-$RUNID-s$cell-$rate-$n" &
                    reqpids+=($!)
                else
                    skipped=$((skipped + 1))
                fi
                sleep "$interval"
            done
            log "phase4b $cell rate=$rate: launched=$n skipped=$skipped; draining"
            if [ "${#reqpids[@]}" -gt 0 ]; then wait "${reqpids[@]}" 2>/dev/null || true; fi
            rm -f "$sf.run"
            wait "$sampler" 2>/dev/null || true
            printf '{"phase":"sustained-meta","cell":"%s","rate":%s,"launched":%s,"skipped":%s,"t0":%s,"t1":%s}\n' \
                "$cell" "$rate" "$n" "$skipped" "$t_sust0" "$(now)" >> "$RESULTS/samples/bursts.jsonl"
            wait_clones_gone 120 || true
            sleep 2
        done
        if [ "$memarm" = uffd ]; then stop_serve "$spid" "$sflag"; fi
    done
    REQ_CGROUP=0
}

# ---------------------------------------------------------------------------
# phase 5: baselines (host-native cold/warm podman, fcvm cold boot, pool contrast)
# ---------------------------------------------------------------------------
phase5() {
    log "=== phase 5: baselines ==="
    local url="http://$HOST4:$HTTP_PORT/medium.html"
    local i

    # (a) host-native COLD: fresh container per request
    for i in $(seq 1 "$R_COLD"); do
        local lf=$RESULTS/requests/base-podman-cold__host__native__medium__r$i.log
        local name=cbpool-cold-$i
        printf '%s BENCH_T0\n' "$(now)" > "$lf"
        podman run --rm --name "$name" "$IMAGE" 2>&1 | python3 -c "$TSPY" >> "$lf" &
        local coldpid=$!
        local t0=$SECONDS
        until grep -q CHROMIUM_BENCH_READY "$lf" 2>/dev/null; do
            [ $((SECONDS - t0)) -ge 120 ] && break
            sleep 0.5
        done
        printf '%s BENCH_READY\n' "$(now)" >> "$lf"
        podman exec "$name" python3 /opt/bench/render.py "$url" --out-prefix /tmp/cold 2>&1 \
            | python3 -c "$TSPY" >> "$lf" || true
        podman rm -f "$name" >/dev/null 2>&1 || true
        printf '%s BENCH_EXIT rc=0\n' "$(now)" >> "$lf"
        # Wait for THIS job only. A bare `wait` also waits on the load sampler,
        # which never exits — that hung phase 5 for 20+ minutes with no output.
        wait "$coldpid" 2>/dev/null || true
    done

    # (b) host-native WARM: long-running container, render only (the physics floor)
    local wname=cbpool-warm
    podman rm -f "$wname" >/dev/null 2>&1 || true
    podman run -d --name "$wname" "$IMAGE" >/dev/null
    local t0=$SECONDS
    until podman logs "$wname" 2>&1 | grep -q CHROMIUM_BENCH_READY; do
        [ $((SECONDS - t0)) -ge 120 ] && die "warm baseline container never became ready"
        sleep 0.5
    done
    # Host-native warm renders, INTERLEAVED across page x format the same way the
    # fcvm matrix is, so the host floor is not itself a block-confounded number.
    local label u f
    local -a hostcells=()
    while read -r label u; do
        case "$label" in medium-https) u="https://$HOST4:$HTTPS_PORT/medium.html" ;; esac
        for f in $SHOT_FMT png; do
            [ "$f" = png ] && [ "$label" != medium ] && continue   # png only on medium
            for i in $(seq 1 "$R"); do hostcells+=("$label|$u|$f|$i"); done
        done
    done < <(mode_urls rootless-proxy)
    local shuffled; shuffled=$(printf '%s\n' "${hostcells[@]}" | \
        python3 -c "import random,sys;r=sys.stdin.read().split();random.Random($SEED).shuffle(r);print('\n'.join(r))")
    local cellspec
    while read -r cellspec; do
        [ -n "$cellspec" ] || continue
        IFS='|' read -r label u f i <<< "$cellspec"
        # ${f} MUST be braced: `$f__` is a legal variable name, so the unbraced
        # form expanded the (unset) variable `f__` and aborted phase 5 under `set -u`.
        local lf=$RESULTS/requests/base-podman-warm__host__native-${f}__${label}__r${i}.log
        printf '%s BENCH_T0\n' "$(now)" > "$lf"
        printf '%s BENCH_EXEC_UP\n' "$(now)" >> "$lf"
        podman exec "$wname" python3 /opt/bench/render.py "$u" --out-prefix /tmp/warm \
            --format "$f" --quality "$SHOT_QUALITY" 2>&1 \
            | python3 -c "$TSPY" >> "$lf" || true
        printf '%s BENCH_EXIT rc=0\n' "$(now)" >> "$lf"
    done <<< "$shuffled"

    # (c-contrast) marginal memory of a host-native warm POOL, on the SAME two
    # bases and the SAME concurrency grid as the fcvm density sweep (defect 1).
    # A container pool is a warm-pool architecture, not shared-nothing — the
    # contrast is "what would the naive alternative cost per concurrent request".
    local pf=$RESULTS/samples/podman-pool.jsonl
    : > "$pf"
    local maxpool; maxpool=$(echo $DENSITY_NS | tr ' ' '\n' | sort -n | tail -1)
    local rep
    for rep in $(seq 1 "$DENSITY_REPS"); do
        podman ps --format '{{.Names}}' | grep '^cbpool-' | grep -v "^$wname$" \
            | xargs -r podman rm -f >/dev/null 2>&1 || true
        sleep 3; sync; sleep 2
        python3 "$SCRIPT_DIR/report.py" sample --podman-prefix cbpool- \
            --extra "\"pool_n\":0,\"rep\":$rep,\"phase\":\"pre\"" >> "$pf" || true
        local cur=1        # the warm container itself is pool member 1
        for i in $(seq 2 "$maxpool"); do
            podman run -d --name "cbpool-$i" "$IMAGE" >/dev/null
            t0=$SECONDS
            until podman logs "cbpool-$i" 2>&1 | grep -q CHROMIUM_BENCH_READY; do
                [ $((SECONDS - t0)) -ge 180 ] && break
                sleep 0.5
            done
            cur=$i
            case " $DENSITY_NS " in *" $cur "*)
                sleep 3
                python3 "$SCRIPT_DIR/report.py" sample --podman-prefix cbpool- \
                    --extra "\"pool_n\":$cur,\"rep\":$rep,\"phase\":\"steady\"" >> "$pf" || true
                ;;
            esac
        done
    done
    podman ps --format '{{.Names}}' | grep '^cbpool-' | grep -v "^$wname$" | xargs -r podman rm -f >/dev/null 2>&1 || true
    podman rm -f "$wname" >/dev/null 2>&1 || true

    # (c) fcvm COLD boot (no snapshot) -> READY -> render, rootless. Expensive; R=2.
    for i in 1 2; do
        local name=cb-$RUNID-cold-$i
        local lf=$RESULTS/requests/base-fcvm-cold__rootless-proxy__coldboot__medium__r$i.log
        printf '%s BENCH_T0\n' "$(now)" > "$lf"
        FCVM_NO_SNAPSHOT=1 RUST_LOG=$FCVM_LOG "$FCVM" podman run --name "$name" \
            --cpu "$CPU" --mem "$MEM" "$IMAGE" 2>&1 | python3 -c "$TSPY" >> "$lf" &
        local bootpid=$!
        local t0=$SECONDS ok=1
        until grep -q CHROMIUM_BENCH_READY "$lf" 2>/dev/null; do
            [ $((SECONDS - t0)) -ge 300 ] && { ok=0; break; }
            sleep 1
        done
        printf '%s BENCH_READY\n' "$(now)" >> "$lf"
        if [ "$ok" = 1 ]; then
            local pid; pid=$(state_pid_by_name "$name")
            printf '%s BENCH_EXEC_UP\n' "$(now)" >> "$lf"
            "$FCVM" exec --pid "$pid" -c -- python3 /opt/bench/render.py "$url" --out-prefix /tmp/cold 2>&1 \
                | python3 -c "$TSPY" >> "$lf" || true
            kill "$pid" 2>/dev/null || true
            wait_state_gone "$name" 60 || true
        fi
        printf '%s BENCH_EXIT rc=0\n' "$(now)" >> "$lf"
        wait "$bootpid" 2>/dev/null || true
    done
}

# ---------------------------------------------------------------------------
# phase 6: report
# ---------------------------------------------------------------------------
phase6() {
    log "=== phase 6: report ==="
    python3 "$SCRIPT_DIR/report.py" finalize "$RESULTS"
    log "report: $RESULTS/report.md"
}

# ---------------------------------------------------------------------------
main() {
    local what=${1:-run}
    case "$what" in
        run)    phase0; start_load_sampler; start_host_servers; phase1; mem_baseline quiescent-pre
                phase3; phase4; phase4b; phase5; mem_baseline quiescent-post; phase6 ;;
        phase0) phase0 ;;
        phase1) phase0; start_load_sampler; start_host_servers; phase1 ;;
        phase3) phase0; start_load_sampler; start_host_servers; phase3 ;;
        phase4) phase0; start_load_sampler; start_host_servers; phase4 ;;
        phase4b) phase0; start_load_sampler; start_host_servers; phase4b ;;
        phase5) phase0; start_load_sampler; start_host_servers; phase5 ;;
        phase6) phase6 ;;
        *) die "unknown command: $what (use run|phase0|phase1|phase3|phase4|phase4b|phase5|phase6)" ;;
    esac
    log "results in $RESULTS"
}
main "$@"
