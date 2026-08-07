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
FCVM=$REPO_ROOT/target/release/fcvm
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
    python3 - "$RESULTS" "$HOST4" "${HOST6:-}" "$R" "$CPU" "$MEM" <<'EOF'
import json, os, platform, subprocess, sys
d, h4, h6, r, cpu, mem = sys.argv[1:7]
meminfo = {l.split(":")[0]: l.split()[1] for l in open("/proc/meminfo") if ":" in l}
info = {
    "uname": " ".join(platform.uname()),
    "nproc": os.cpu_count(),
    "mem_total_kb": int(meminfo["MemTotal"]),
    "cpu_model": next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo")
                       if l.lower().startswith(("model name", "hardware", "cpu part"))), "unknown-arm64"),
    "host_ipv4": h4, "host_ipv6": h6 or None,
    "reps": int(r), "vm_cpu": int(cpu), "vm_mem_mib": int(mem),
    "contention_note": "shared dev box: other test workloads run intermittently; all timings carry that caveat",
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

cleanup() {
    local rc=$?
    log "cleanup (rc=$rc)"
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
    if [ "$HUGE_CHANGED" = 1 ]; then
        log "restoring nr_hugepages=$ORIG_HUGEPAGES"
        sudo -n sh -c "echo $ORIG_HUGEPAGES > /proc/sys/vm/nr_hugepages" || true
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
# fcvm helpers
# ---------------------------------------------------------------------------
fx() {  # fx <sudoflag> <args...>
    local s=$1; shift
    if [ -n "$s" ]; then sudo env RUST_LOG=fcvm=info "$FCVM" "$@"
    else RUST_LOG=fcvm=info "$FCVM" "$@"; fi
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
DRIVER_TEMPLATE='import json,socket,sys,time,runpy
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
sys.argv=["render.py","@URL@","--out-prefix","/tmp/bench-req"]
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
sys.exit(rc)'

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

# run_request <logbase> <sudoflag(ignored:"")> <memarm uffd|file> <tag> <servepid|-> <url> <name>
# One full shared-nothing request: restore clone -> exec driver -> teardown.
run_request() {
    local logbase=$1 _unused=$2 memarm=$3 tag=$4 servepid=$5 url=$6 name=$7
    local lf=$RESULTS/requests/$logbase.log
    local phost pport code
    phost=$(probe_host_of "$url"); pport=$(probe_port_of "$url")
    code=${DRIVER_TEMPLATE//@PHOST@/$phost}
    code=${code//@PPORT@/$pport}
    code=${code//@URL@/$url}
    local -a src
    if [ "$memarm" = uffd ]; then src=(--pid "$servepid"); else src=(--snapshot "$tag"); fi
    printf '%s BENCH_T0\n' "$(now)" > "$lf"
    local rc=0
    timeout -k 10 "$REQ_TIMEOUT" \
        env RUST_LOG=fcvm=info \
        "$FCVM" snapshot run "${src[@]}" --name "$name" \
        --no-dirty-tracking --no-swap --exec "python3 -c '$code'" 2>&1 \
        | python3 -c "$TSPY" >> "$lf" || rc=$?
    printf '%s BENCH_EXIT rc=%d\n' "$(now)" "$rc" >> "$lf"
    return 0
}

# sudo variant kept separate: `timeout` must wrap sudo, and sudo env must carry RUST_LOG
run_request_sudo() {
    local logbase=$1 memarm=$2 tag=$3 servepid=$4 url=$5 name=$6
    local lf=$RESULTS/requests/$logbase.log
    local phost pport code
    phost=$(probe_host_of "$url"); pport=$(probe_port_of "$url")
    code=${DRIVER_TEMPLATE//@PHOST@/$phost}
    code=${code//@PPORT@/$pport}
    code=${code//@URL@/$url}
    local -a src
    if [ "$memarm" = uffd ]; then src=(--pid "$servepid"); else src=(--snapshot "$tag"); fi
    printf '%s BENCH_T0\n' "$(now)" > "$lf"
    local rc=0
    timeout -k 10 "$REQ_TIMEOUT" \
        sudo env RUST_LOG=fcvm=info "$FCVM" snapshot run "${src[@]}" --name "$name" \
        --no-dirty-tracking --no-swap --exec "python3 -c '$code'" 2>&1 \
        | python3 -c "$TSPY" >> "$lf" || rc=$?
    printf '%s BENCH_EXIT rc=%d\n' "$(now)" "$rc" >> "$lf"
    return 0
}

req() {  # dispatch on sudo flag
    local logbase=$1 sudoflag=$2; shift 2
    if [ -n "$sudoflag" ]; then run_request_sudo "$logbase" "$@"
    else run_request "$logbase" "" "$@"; fi
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
            sudo sh -c "echo $HUGEPAGE_POOL > /proc/sys/vm/nr_hugepages"
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
        sudo env FCVM_NO_SNAPSHOT=1 RUST_LOG=fcvm=info "$FCVM" podman run --name "$name" \
            --cpu "$CPU" --mem "$MEM" "$@" "$IMAGE" 2>&1 | python3 -c "$TSPY" >> "$lf" &
    else
        FCVM_NO_SNAPSHOT=1 RUST_LOG=fcvm=info "$FCVM" podman run --name "$name" \
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

phase1() {
    log "=== phase 1: golden snapshots ==="
    boot_golden rootless "" || die "rootless golden failed"
    boot_golden noredir "" || die "noredir golden failed"
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
start_serve() {  # $1 tag, $2 sudoflag -> echoes fcvm serve pid
    local tag=$1 sudoflag=$2
    local existing; existing=$(serve_pid_by_snap "$tag")
    if [ -n "$existing" ] && kill -0 "$existing" 2>/dev/null; then
        printf '%s' "$existing"; return 0
    fi
    if [ -n "$sudoflag" ]; then
        sudo env RUST_LOG=fcvm=info "$FCVM" snapshot serve "$tag" \
            > "$RESULTS/logs/serve-$tag.log" 2>&1 &
    else
        RUST_LOG=fcvm=info "$FCVM" snapshot serve "$tag" \
            > "$RESULTS/logs/serve-$tag.log" 2>&1 &
    fi
    local t0=$SECONDS pid=""
    while [ -z "$pid" ]; do
        [ $((SECONDS - t0)) -ge 30 ] && die "serve for $tag did not register in state"
        sleep 0.3
        pid=$(serve_pid_by_snap "$tag")
    done
    printf '%s:%s\n' "${sudoflag:-user}" "$pid" >> "$RESULTS/serve.pids"
    printf '%s' "$pid"
}

stop_serve() {  # $1 pid, $2 sudoflag
    if [ -n "$2" ]; then sudo kill "$1" 2>/dev/null || true; else kill "$1" 2>/dev/null || true; fi
    sleep 1
}

prewarm_memory() {  # $1 tag — pull memory.bin into page cache before file-backed fan-out
    if [ -r "$SNAP_DIR/$1/memory.bin" ]; then cat "$SNAP_DIR/$1/memory.bin" > /dev/null
    else sudo cat "$SNAP_DIR/$1/memory.bin" > /dev/null 2>&1 || true; fi
}

# ---------------------------------------------------------------------------
# phase 3: per-request matrix (mode x url), uffd arm + file arm + control
# ---------------------------------------------------------------------------
phase3() {
    log "=== phase 3: per-request matrix (R=$R) ==="
    local m tag sflag spid label url i
    for m in rootless-proxy rootless-pasta rootless-proxy6 rootless-pasta6 bridged routed; do
        [ "${MODE_AVAIL[$m]}" = yes ] || { log "phase3: $m UNAVAILABLE (${MODE_REASON[$m]})"; continue; }
        tag=$(golden_tag "${MODE_SNAP[$m]}")
        snapshot_exists "$tag" || { log "phase3: $m missing snapshot $tag, skipping"; continue; }
        sflag=${MODE_SUDO[$m]}
        spid=$(start_serve "$tag" "$sflag")
        [ -n "$spid" ] || die "phase3 $m: serve failed for $tag"
        log "phase3 $m: uffd arm (serve=$spid)"
        while read -r label url; do
            for i in $(seq 1 "$R"); do
                req "p3__${m}__uffd-4k__${label}__r$i" "$sflag" uffd "$tag" "$spid" "$url" "cb-$RUNID-p3-$m-$label-$i"
            done
        done < <(mode_urls "$m")
        stop_serve "$spid" "$sflag"
    done

    # file-backed arm on the fastest rootless mode (page-cache sharing variant)
    local fmode=${FANOUT_MODE:-rootless-proxy}
    tag=$(golden_tag "${MODE_SNAP[$fmode]}")
    if snapshot_exists "$tag"; then
        prewarm_memory "$tag"
        log "phase3 $fmode: file arm"
        while read -r label url; do
            for i in $(seq 1 "$R"); do
                req "p3__${fmode}__file-4k__${label}__r$i" "${MODE_SUDO[$fmode]}" file "$tag" - "$url" "cb-$RUNID-p3f-$label-$i"
            done
        done < <(mode_urls "$fmode")
    fi

    # control arm: identical clone flow, in-guest fixture URL (no host network) —
    # isolates render-from-network. One rootless + one root mode, reduced R.
    for m in rootless-proxy bridged; do
        [ "${MODE_AVAIL[$m]}" = yes ] || continue
        tag=$(golden_tag "${MODE_SNAP[$m]}")
        snapshot_exists "$tag" || continue
        sflag=${MODE_SUDO[$m]}
        spid=$(start_serve "$tag" "$sflag")
        log "phase3 $m: control arm (in-guest fixtures, R=$R_CONTROL)"
        for label in minimal medium heavy; do
            for i in $(seq 1 "$R_CONTROL"); do
                req "p3__${m}__uffd-4k__control-${label}__r$i" "$sflag" uffd "$tag" "$spid" \
                    "http://127.0.0.1:8000/$label.html" "cb-$RUNID-p3c-$m-$label-$i"
            done
        done
        stop_serve "$spid" "$sflag"
    done
}

# ---------------------------------------------------------------------------
# phase 4: fan-out + memory density (2x2: {uffd,file} x {4k,huge})
# ---------------------------------------------------------------------------
sample_once() {  # $1 outfile  $2 extra-json (e.g. '"cell":"uffd-4k","rate":2')
    python3 "$SCRIPT_DIR/report.py" sample --state-dir "$STATE_DIR" --name-prefix "cb-$RUNID" \
        --extra "$2" >> "$1" 2>/dev/null || true
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

phase4() {
    log "=== phase 4: fan-out + memory density ==="
    local fmode=${FANOUT_MODE:-rootless-proxy}
    local sflag=${MODE_SUDO[$fmode]}
    local tag4k taghuge
    tag4k=$(golden_tag "${MODE_SNAP[$fmode]}")
    taghuge=$(golden_tag huge)
    local -a cells=("uffd-4k" "file-4k")
    if [ "$HUGE_AVAIL" = yes ] && snapshot_exists "$taghuge"; then
        cells+=("uffd-huge" "file-huge")
    else
        log "phase4: huge cells UNAVAILABLE ($HUGE_REASON)"
    fi
    local urls=() label url
    while read -r label url; do
        case "$label" in medium-https) ;; *) urls+=("$label|$url") ;; esac
    done < <(mode_urls "$fmode")

    local cell memarm pages ctag spid
    for cell in "${cells[@]}"; do
        memarm=${cell%%-*}; pages=${cell##*-}
        if [ "$pages" = huge ]; then ctag=$taghuge; else ctag=$tag4k; fi
        spid=-
        if [ "$memarm" = uffd ]; then
            spid=$(start_serve "$ctag" "$sflag")
            [ -n "$spid" ] || die "phase4 $cell: serve failed for $ctag"
        else
            prewarm_memory "$ctag"
        fi

        # ---- burst fan-out
        local N i k pair
        for N in $BURST_NS; do
            if [ "$pages" = huge ]; then
                local pool free_needed
                pool=$(cat /proc/sys/vm/nr_hugepages)
                free_needed=$(( N * MEM / 2 ))
                if [ "$pool" -lt "$free_needed" ]; then
                    log "phase4 $cell: burst N=$N SKIPPED (pool $pool pages < needed $free_needed)"
                    continue
                fi
            fi
            log "phase4 $cell: burst N=$N"
            local t_burst0; t_burst0=$(now)
            local -a burstpids=()
            for i in $(seq 1 "$N"); do
                k=$(( (i - 1) % ${#urls[@]} )); pair=${urls[$k]}
                req "burst${N}__${fmode}__${cell}__${pair%%|*}__r$i" "$sflag" "$memarm" "$ctag" "$spid" \
                    "${pair##*|}" "cb-$RUNID-b$N-$cell-$i" &
                burstpids+=($!)
            done
            # sample near the concurrency peak while the burst is in flight
            sleep 1
            sample_once "$RESULTS/samples/burst-peaks.jsonl" "\"cell\":\"$cell\",\"phase\":\"burst-peak\",\"burst_n\":$N"
            sleep 1
            sample_once "$RESULTS/samples/burst-peaks.jsonl" "\"cell\":\"$cell\",\"phase\":\"burst-peak\",\"burst_n\":$N"
            wait "${burstpids[@]}" 2>/dev/null || true
            printf '{"phase":"burst","cell":"%s","n":%s,"t0":%s,"t1":%s}\n' \
                "$cell" "$N" "$t_burst0" "$(now)" >> "$RESULTS/samples/bursts.jsonl"
            wait_clones_gone 60 || true
            sleep 2
        done

        # ---- sustained-rate memory curve
        local rates=$SUST_RATES
        [ "$pages" = huge ] && rates=$SUST_RATES_HUGE
        local rate
        for rate in $rates; do
            local sf=$RESULTS/samples/sustained__${cell}__rate${rate}.jsonl
            log "phase4 $cell: sustained ${rate}rps x ${SUST_SECS}s"
            : > "$sf"
            for i in 1 2 3; do sample_once "$sf" "\"cell\":\"$cell\",\"rate\":$rate,\"phase\":\"quiescent\""; sleep 1; done
            local t_end=$(( SECONDS + SUST_SECS )) n=0 skipped=0
            local interval; interval=$(python3 -c "print(1.0/$rate)")
            : > "$sf.run"       # sampler runs while this exists (created BEFORE spawn)
            (   while [ -e "$sf.run" ]; do
                    sample_once "$sf" "\"cell\":\"$cell\",\"rate\":$rate,\"phase\":\"load\""
                    sleep 2
                done
            ) &
            local sampler=$!
            local -a reqpids=()
            while [ $SECONDS -lt $t_end ]; do
                local inflight
                inflight=$(jobs -r | grep -c '' || true)
                if [ "$inflight" -le "$MAX_INFLIGHT" ]; then   # sampler counts as one job
                    n=$((n + 1))
                    k=$(( (n - 1) % ${#urls[@]} )); pair=${urls[$k]}
                    req "sust-r${rate}__${fmode}__${cell}__${pair%%|*}__r$n" "$sflag" "$memarm" "$ctag" "$spid" \
                        "${pair##*|}" "cb-$RUNID-s$rate-$cell-$n" &
                    reqpids+=($!)
                else
                    skipped=$((skipped + 1))
                fi
                sleep "$interval"
            done
            log "phase4 $cell rate=$rate: launched=$n skipped=$skipped; draining"
            # drain requests first (sampler keeps recording), then stop the sampler
            if [ "${#reqpids[@]}" -gt 0 ]; then wait "${reqpids[@]}" 2>/dev/null || true; fi
            rm -f "$sf.run"
            wait "$sampler" 2>/dev/null || true
            printf '{"phase":"sustained-meta","cell":"%s","rate":%s,"launched":%s,"skipped":%s}\n' \
                "$cell" "$rate" "$n" "$skipped" >> "$RESULTS/samples/bursts.jsonl"
            wait_clones_gone 90 || true
            sleep 2
        done

        if [ "$memarm" = uffd ]; then stop_serve "$spid" "$sflag"; fi
    done
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
        wait || true
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
    local label u
    while read -r label u; do
        case "$label" in medium-https) u="https://$HOST4:$HTTPS_PORT/medium.html" ;; esac
        for i in $(seq 1 "$R"); do
            local lf=$RESULTS/requests/base-podman-warm__host__native__${label}__r$i.log
            printf '%s BENCH_T0\n' "$(now)" > "$lf"
            printf '%s BENCH_EXEC_UP\n' "$(now)" >> "$lf"
            podman exec "$wname" python3 /opt/bench/render.py "$u" --out-prefix /tmp/warm 2>&1 \
                | python3 -c "$TSPY" >> "$lf" || true
            printf '%s BENCH_EXIT rc=0\n' "$(now)" >> "$lf"
        done
    done < <(mode_urls rootless-proxy)

    # (c-contrast) marginal memory of a host-native warm POOL (naive alternative)
    local pf=$RESULTS/samples/podman-pool.jsonl
    : > "$pf"
    python3 "$SCRIPT_DIR/report.py" sample --podman-prefix cbpool- --extra '"pool_n":1' >> "$pf" || true
    for i in 2 3 4; do
        podman run -d --name "cbpool-$i" "$IMAGE" >/dev/null
        t0=$SECONDS
        until podman logs "cbpool-$i" 2>&1 | grep -q CHROMIUM_BENCH_READY; do
            [ $((SECONDS - t0)) -ge 120 ] && break
            sleep 0.5
        done
        sleep 2
        python3 "$SCRIPT_DIR/report.py" sample --podman-prefix cbpool- --extra "\"pool_n\":$i" >> "$pf" || true
    done
    podman ps --format '{{.Names}}' | grep '^cbpool-' | grep -v "^$wname$" | xargs -r podman rm -f >/dev/null 2>&1 || true
    podman rm -f "$wname" >/dev/null 2>&1 || true

    # (c) fcvm COLD boot (no snapshot) -> READY -> render, rootless. Expensive; R=2.
    for i in 1 2; do
        local name=cb-$RUNID-cold-$i
        local lf=$RESULTS/requests/base-fcvm-cold__rootless-proxy__coldboot__medium__r$i.log
        printf '%s BENCH_T0\n' "$(now)" > "$lf"
        FCVM_NO_SNAPSHOT=1 RUST_LOG=fcvm=info "$FCVM" podman run --name "$name" \
            --cpu "$CPU" --mem "$MEM" "$IMAGE" 2>&1 | python3 -c "$TSPY" >> "$lf" &
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
        wait || true
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
        run)    phase0; start_host_servers; phase1; phase3; phase4; phase5; phase6 ;;
        phase0) phase0 ;;
        phase1) phase0; start_host_servers; phase1 ;;
        phase3) phase0; start_host_servers; phase3 ;;
        phase4) phase0; start_host_servers; phase4 ;;
        phase5) phase0; start_host_servers; phase5 ;;
        phase6) phase6 ;;
        *) die "unknown command: $what (use run|phase0|phase1|phase3|phase4|phase5|phase6)" ;;
    esac
    log "results in $RESULTS"
}
main "$@"
