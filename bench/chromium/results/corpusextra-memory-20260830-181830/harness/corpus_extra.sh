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
REPO="${REPO:-$(cd "$HERE/../.." && pwd)}"
BENCH="$REPO/bench/chromium"
STAMP="${STAMP:-$(date +%Y%m%d-%H%M%S)}"
RESULTS="${RESULTS:-$BENCH/results/corpusextra-$STAMP}"
LOGDIR="${LOGDIR:-/tmp/corpusextra-$STAMP}"
TAG="${TAG:-cb-req-corpus}"
IMAGE="${IMAGE:-localhost/chromium-bench-req}"
PHASES="${PHASES:-hostcdp,memory}"
REPS="${REPS:-230}"          # total attempts; the first WARMUP are discarded
WARMUP="${WARMUP:-28}"       # two full 14-URL cycles, the campaign's warmup
MEM_NS="${MEM_NS:-1,2,4,8}"
MEM_REPS="${MEM_REPS:-2}"
# CPU-seconds per screenshot on both sides, over three full cycles of the
# corpus. A different metric from the wall-clock arms, kept in its own record.
CPUTIME_REPS="${CPUTIME_REPS:-42}"
# Which side(s) of the memory measurement this invocation runs. Split into two
# invocations so each fits inside one idle-watchdog window on this box.
MEM_SIDES="${MEM_SIDES:-fcvm,container}"
UFFD_MODE="${UFFD_MODE:-minor}"
UFFD_PREFETCH="${UFFD_PREFETCH:-on}"

# The campaign's 14 URLs, in the campaign's order. Copied from corpus_campaign.sh
# and checked against it below: a corpus that has drifted would make the host
# control a different workload from the VM arm it is a control for.
URLS="https://example.com/,https://news.ycombinator.com/,https://developers.cloudflare.com/,https://blog.cloudflare.com/,https://en.wikipedia.org/,https://developer.mozilla.org/en-US/,https://www.elmundo.es/,https://www.rtp.pt/noticias/,https://www.theguardian.com/international,https://todomvc.com/examples/javascript-es6/dist/,https://todomvc.com/examples/react/dist/index.html,https://todomvc.com/examples/vue/dist/,https://todomvc.com/examples/angular/dist/browser/,https://todomvc.com/examples/preact/dist/"

mkdir -p "$RESULTS" "$LOGDIR"
say() { printf '\n=== %s %s\n' "$(date +%H:%M:%S)" "$*"; }

for tool in jq curl dig python3 podman sudo; do
    command -v "$tool" >/dev/null 2>&1 || { echo "BLOCKED: '$tool' missing" >&2; exit 2; }
done

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
    echo " \"git_head\": \"$(git -C "$REPO" rev-parse HEAD)\","
    echo " \"git_dirty\": \"$(git -C "$REPO" status --porcelain --untracked-files=no | tr '\n' ';')\","
    echo " \"host_kernel\": \"$(uname -r)\", \"machine\": \"$(uname -m)\","
    echo " \"image\": \"$IMAGE\", \"image_id\": \"$(podman inspect --format '{{.Id}}' "$IMAGE" 2>/dev/null)\","
    echo " \"tag\": \"$TAG\", \"reps\": $REPS, \"warmup\": $WARMUP,"
    for f in hostcdp.sh cdpdrive.py render.py corpus_mem.py corpus_serve.py report.py; do
        echo " \"$f\": \"$(sha256sum "$BENCH/$f" | cut -d' ' -f1)\","
    done
    echo " \"fcvm\": \"$(sha256sum "$REPO/target/release/fcvm" | cut -d' ' -f1)\""
    echo "}"
} > "$RESULTS/provenance.json"
python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$RESULTS/provenance.json" \
    || { echo "BLOCKED: provenance.json is not valid JSON" >&2; exit 2; }

load1=$(awk '{print $1}' /proc/loadavg)
awk -v l="$load1" 'BEGIN{exit !(l > 2.0)}' && { echo "BLOCKED: 1-min load $load1 > 2.0" >&2; exit 2; }
if pgrep -x fcvm >/dev/null 2>&1 || pgrep -x firecracker >/dev/null 2>&1; then
    echo "BLOCKED: stray fcvm/firecracker processes" >&2; pgrep -a 'fcvm|firecracker' >&2; exit 2
fi

DNSMASQ_WAS_ACTIVE=no
systemctl is-active --quiet dnsmasq 2>/dev/null && DNSMASQ_WAS_ACTIVE=yes
SERVE_PID=""

stop_corpus_serve() {
    [ -n "$SERVE_PID" ] || return 0
    if sudo kill -0 "$SERVE_PID" 2>/dev/null; then
        say "stopping corpus_serve ($SERVE_PID)"
        sudo kill "$SERVE_PID" 2>/dev/null || true
        for _ in $(seq 1 50); do sudo kill -0 "$SERVE_PID" 2>/dev/null || break; sleep 0.1; done
        sudo kill -9 "$SERVE_PID" 2>/dev/null || true
    fi
    for _ in $(seq 1 50); do [ -f "$RESULTS/corpus-serve.status" ] && break; sleep 0.1; done
    [ -f "$RESULTS/corpus-serve.status" ] && say "corpus_serve exit status: $(tr -d '[:space:]' <"$RESULTS/corpus-serve.status")"
}

cleanup() {
    set +e
    podman ps -a --format '{{.Names}}' | grep -E '^(cbmem-|hostcdp-)' | xargs -r podman rm -f >/dev/null 2>&1
    stop_corpus_serve
    if [ "$DNSMASQ_WAS_ACTIVE" = yes ] && ! systemctl is-active --quiet dnsmasq; then
        for _ in $(seq 1 10); do sudo systemctl start dnsmasq >/dev/null 2>&1 && break; sleep 1; done
        systemctl is-active --quiet dnsmasq || {
            echo "FAILED: dnsmasq did not restart; this box has no DNS. Check: sudo ss -lnup 'sport = :53'" >&2
            exit 1; }
    fi
}
trap cleanup EXIT

[ "$DNSMASQ_WAS_ACTIVE" = yes ] && { say "stopping dnsmasq for 127.0.0.1:53"; sudo systemctl stop dnsmasq; }

say "starting corpus_serve (DNS 127.0.0.1:53 -> 10.0.2.2, HTTP 80, HTTPS 443)"
SERVE_PIDFILE="$LOGDIR/corpus_serve.pid"
rm -f "$SERVE_PIDFILE" "$RESULTS/corpus-serve.status"
sudo -b sh -c 'python3 "$2" --root "$3" --port 80 --tls-port 443 --dns-addr 127.0.0.1 --dns-port 53 --answer-ip 10.0.2.2 --dns-log "$4" --access-log "$5" & pid=$!; echo "$pid" > "$1"; wait "$pid"; rc=$?; echo "$rc" > "$6.tmp" && mv "$6.tmp" "$6"' \
    _ "$SERVE_PIDFILE" "$BENCH/corpus_serve.py" "$BENCH/corpus-live" \
    "$RESULTS/corpus-dns.log" "$RESULTS/corpus-access.log" "$RESULTS/corpus-serve.status" \
    > "$LOGDIR/corpus_serve.log" 2>&1
for _ in $(seq 1 50); do [ -s "$SERVE_PIDFILE" ] && break; sleep 0.1; done
SERVE_PID=$(cat "$SERVE_PIDFILE" 2>/dev/null || true)
[ -n "$SERVE_PID" ] || { echo "BLOCKED: corpus_serve did not start" >&2; cat "$LOGDIR/corpus_serve.log" >&2; exit 3; }
grep -q "loaded [1-9]" "$LOGDIR/corpus_serve.log" || {
    echo "BLOCKED: corpus_serve loaded no urls" >&2; cat "$LOGDIR/corpus_serve.log" >&2; exit 3; }

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
    case "$ucode" in 200|30[1278]) ;; *) missing="$missing\n  $ucode  $url" ;; esac
    checked=$((checked + 1))
done
[ -z "$missing" ] || { printf "BLOCKED: the corpus does not serve every URL:$missing\n" >&2; exit 3; }
say "corpus complete: all $checked URLs replay locally"

# Two host arms. "free" is the naive host container: the whole box is available
# to it, which is what a container on this machine actually gets. "cpu2" caps it
# at the VM clone's vCPU count, so the CPU budget is not a second variable in the
# comparison. Both run the same schedule against the same replay.
HOSTCDP_ARMS="${HOSTCDP_ARMS:-free,cpu2}"
if [[ ",$PHASES," == *",hostcdp,"* ]]; then
    for arm in $(printf '%s' "$HOSTCDP_ARMS" | tr ',' ' '); do
        case "$arm" in
            free) cpus="" ;;
            cpu2) cpus=2 ;;
            *) echo "BLOCKED: unknown hostcdp arm '$arm'" >&2; exit 2 ;;
        esac
        say "hostcdp/$arm over the corpus: $REPS attempts, $WARMUP warmup, cpus=${cpus:-<all>}, resolver rule -> 127.0.0.1"
        URL="$URLS" REPS="$REPS" WARMUP="$WARMUP" IMAGE="$IMAGE" CPUS="$cpus" \
            BENCH_RESOLVE_ALL_TO=127.0.0.1 SETTLE_WAIT_SECS=300 \
            RESULTS="$RESULTS/hostcdp-$arm" bash "$BENCH/hostcdp.sh" 2>&1 | tee "$LOGDIR/hostcdp-$arm.log"
    done
fi

if [[ ",$PHASES," == *",memory,"* ]]; then
    say "matched-basis memory: N in $MEM_NS, $MEM_REPS reps, both sides"
    python3 "$BENCH/corpus_mem.py" --results "$RESULTS/memory" --tag "$TAG" --image "$IMAGE" \
        --urls "$URLS" --ns "$MEM_NS" --reps "$MEM_REPS" \
        --uffd-mode "$UFFD_MODE" --uffd-prefetch "$UFFD_PREFETCH" \
        --cputime-reps "$CPUTIME_REPS" --sides "$MEM_SIDES" \
        --fcvm "$REPO/target/release/fcvm" 2>&1 | tee "$LOGDIR/memory.log"
fi

say "records: $RESULTS"
say "logs:    $LOGDIR"
