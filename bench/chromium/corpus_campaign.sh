#!/usr/bin/env bash
# Corpus campaign: golden + gated measured run over Cloudflare's 14-URL corpus.
#
# The report's publication rule is that published numbers come from this corpus
# mix and nothing else; medium.html is a micro-benchmark for optimisation work.
# The procedure was never written down, so regenerating it meant reverse
# engineering the invocation out of a sealed analysis.json. This is that
# procedure, written down.
#
# Wiring. The guest renders the corpus from a host-side byte replay:
#
#   guest Chromium --> resolv.conf 10.0.2.2 (baked into the golden by GUEST_DNS)
#                 --> pasta maps the gateway onto the host's loopback
#                 --> corpus_serve.py on 127.0.0.1: DNS 53, HTTP 80, HTTPS 443
#                 --> wildcard A record answering 10.0.2.2, so EVERY hostname a
#                     page pulls (assets, beacons, subdomains) comes back here
#
# The wildcard is why a dnsmasq `address=/domain/` list is not a substitute, and
# why this needs 127.0.0.1:53 specifically. Ubuntu's dnsmasq owns that socket,
# so the campaign stops it and restarts it on the way out. Host name resolution
# is unaffected: /etc/resolv.conf points at systemd-resolved on 127.0.0.53.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="${REPO:-$(cd "$HERE/../.." && pwd)}"
TAG="${TAG:-cb-req-corpus}"
REPS="${REPS:-202}"
WARMUP="${WARMUP:-28}"          # two full 14-URL cycles; the harness fails closed below this
ARMS="${ARMS:-cdp,cdp-fast,noop}"
UFFD_MODE="${UFFD_MODE:-minor}"
UFFD_PREFETCH="${UFFD_PREFETCH:-on}"
BACKEND="${BACKEND:-uffd}"
STAMP="$(date +%Y%m%d-%H%M%S)"
RESULTS="${RESULTS:-$REPO/bench/chromium/results/reqbench-$STAMP-corpus}"
LOGDIR="${LOGDIR:-/tmp/corpus-campaign-$STAMP}"
mkdir -p "$LOGDIR"

# The 14 URLs, in the order the sealed 2026-08-14 run cycled them. Order is part
# of the schedule: reqanalyze re-derives the expected URL per record from it, so
# a reordering is a different experiment, not a cosmetic change.
URLS="https://example.com/,https://news.ycombinator.com/,https://developers.cloudflare.com/,https://blog.cloudflare.com/,https://en.wikipedia.org/,https://developer.mozilla.org/en-US/,https://www.elmundo.es/,https://www.rtp.pt/noticias/,https://www.theguardian.com/international,https://todomvc.com/examples/javascript-es6/dist/,https://todomvc.com/examples/react/dist/index.html,https://todomvc.com/examples/vue/dist/,https://todomvc.com/examples/angular/dist/browser/,https://todomvc.com/examples/preact/dist/"

say() { printf '\n=== %s\n' "$*"; }

# --- preflight -------------------------------------------------------------
# The run driver enforces its own quiet gate and would void the record anyway;
# failing here costs seconds instead of the golden's minutes.
load1=$(awk '{print $1}' /proc/loadavg)
if awk -v l="$load1" 'BEGIN{exit !(l > 2.0)}'; then
    echo "BLOCKED: 1-min load $load1 exceeds the quiet gate (2.0)" >&2
    exit 2
fi
if pgrep -x fcvm >/dev/null 2>&1 || pgrep -x firecracker >/dev/null 2>&1; then
    echo "BLOCKED: stray fcvm/firecracker processes; the run gate refuses these" >&2
    pgrep -a 'fcvm|firecracker' >&2 || true
    exit 2
fi
for f in corpus_serve.py reqbench.sh; do
    [ -f "$REPO/bench/chromium/$f" ] || { echo "BLOCKED: missing bench/chromium/$f" >&2; exit 2; }
done

# --- host replay server ----------------------------------------------------
DNSMASQ_WAS_ACTIVE=no
if systemctl is-active --quiet dnsmasq 2>/dev/null; then
    DNSMASQ_WAS_ACTIVE=yes
fi
SERVE_PID=""

cleanup() {
    set +e
    # `sudo kill -0`, not bare `kill -0`: corpus_serve runs as root (sudo -b)
    # while this script does not, so an unprivileged liveness probe gets EPERM
    # and the guard is ALWAYS false. The server then survives holding
    # 127.0.0.1:53/80/443, the `systemctl start dnsmasq` below cannot bind, and
    # the next run picks up the leaked pid and records DNSMASQ_WAS_ACTIVE=no.
    if [ -n "$SERVE_PID" ] && sudo kill -0 "$SERVE_PID" 2>/dev/null; then
        say "stopping corpus_serve ($SERVE_PID)"
        sudo kill "$SERVE_PID" 2>/dev/null
        # Not `wait`: the server is not a child of this shell (sudo -b detached
        # it), so wait returns immediately and proves nothing. Poll instead.
        for _ in $(seq 1 50); do
            sudo kill -0 "$SERVE_PID" 2>/dev/null || break
            sleep 0.1
        done
        if sudo kill -0 "$SERVE_PID" 2>/dev/null; then
            say "corpus_serve $SERVE_PID did not exit; escalating to SIGKILL"
            sudo kill -9 "$SERVE_PID" 2>/dev/null
        fi
    fi
    if [ "$DNSMASQ_WAS_ACTIVE" = yes ] && ! systemctl is-active --quiet dnsmasq; then
        say "restarting dnsmasq"
        sudo systemctl start dnsmasq
    fi
}
trap cleanup EXIT

if [ "$DNSMASQ_WAS_ACTIVE" = yes ]; then
    say "stopping dnsmasq so corpus_serve can own 127.0.0.1:53"
    sudo systemctl stop dnsmasq
fi

say "starting corpus_serve (DNS 127.0.0.1:53 answering 10.0.2.2; HTTP 80; HTTPS 443)"
# The PID comes from THIS invocation, via a pidfile the wrapper shell writes
# before exec'ing. `pgrep -f corpus_serve.py` would match any campaign's server,
# so a concurrent run's cleanup could kill this one's and leave its own alive --
# and the survivor still holds :53/:80/:443, so the next campaign's preflight
# passes against a server nobody is tracking. `exec` means the shell BECOMES
# python, so $$ is the server's own pid, not a parent's.
SERVE_PIDFILE="$LOGDIR/corpus_serve.pid"
sudo -b sh -c 'echo $$ > "$1"; exec python3 "$2" --root "$3" --port 80 --tls-port 443 --dns-addr 127.0.0.1 --dns-port 53 --answer-ip 10.0.2.2' \
    _ "$SERVE_PIDFILE" "$REPO/bench/chromium/corpus_serve.py" "$REPO/bench/chromium/corpus-live" \
    > "$LOGDIR/corpus_serve.log" 2>&1
for _ in $(seq 1 50); do
    [ -s "$SERVE_PIDFILE" ] && break
    sleep 0.1
done
SERVE_PID=$(cat "$SERVE_PIDFILE" 2>/dev/null || true)
[ -n "$SERVE_PID" ] || { echo "BLOCKED: corpus_serve did not start; see $LOGDIR/corpus_serve.log" >&2; cat "$LOGDIR/corpus_serve.log" >&2; exit 3; }
sudo kill -0 "$SERVE_PID" 2>/dev/null || { echo "BLOCKED: corpus_serve pid $SERVE_PID is not alive; see $LOGDIR/corpus_serve.log" >&2; cat "$LOGDIR/corpus_serve.log" >&2; exit 3; }

# Prove all three sockets answer before spending minutes on a golden. A replay
# server that loaded zero urls, or a DNS socket that silently lost the bind,
# would otherwise surface as a corpus of 404s inside the guest.
grep -q "loaded [1-9]" "$LOGDIR/corpus_serve.log" || {
    echo "BLOCKED: corpus_serve loaded no urls" >&2; cat "$LOGDIR/corpus_serve.log" >&2; exit 3; }
answer=$(dig +short +time=2 +tries=1 @127.0.0.1 blog.cloudflare.com A | head -1)
[ "$answer" = "10.0.2.2" ] || { echo "BLOCKED: wildcard DNS answered '$answer', expected 10.0.2.2" >&2; exit 3; }
code=$(curl -sk -o /dev/null -w '%{http_code}' --resolve 'blog.cloudflare.com:443:127.0.0.1' https://blog.cloudflare.com/)
[ "$code" = "200" ] || { echo "BLOCKED: HTTPS replay returned $code for blog.cloudflare.com" >&2; exit 3; }
say "replay server up: DNS -> 10.0.2.2, HTTPS 200"

# --- golden ----------------------------------------------------------------
# PHASE=run reuses the installed golden. The working-set sidecar beside the
# snapshot is the reason that is worth doing separately: a freshly created
# golden has none, so the first measured run pays cold-working-set costs that
# every later run does not. Comparing the two is a one-variable experiment.
PHASE="${PHASE:-all}"
if [ "$PHASE" = all ]; then
    say "golden $TAG (GUEST_DNS=10.0.2.2 baked into resolv.conf at boot)"
    GUEST_DNS=10.0.2.2 TAG="$TAG" \
        make -C "$REPO" bench-chromium-request-golden 2>&1 | tee "$LOGDIR/golden.log"

    say "verify: CDP hops on a restored clone"
    TAG="$TAG" make -C "$REPO" bench-chromium-request-verify 2>&1 | tee "$LOGDIR/verify.log"
else
    snap="${DATA_ROOT:-/mnt/fcvm-btrfs}/snapshots/$TAG"
    [ -f "$snap/config.json" ] || { echo "BLOCKED: PHASE=$PHASE but no golden at $snap" >&2; exit 2; }
    ws="$snap/memory.bin.working-set"
    if [ -f "$ws" ]; then
        say "reusing golden $TAG (working-set sidecar present: $(stat -c%s "$ws") bytes, mtime $(stat -c%y "$ws"))"
    else
        say "reusing golden $TAG (NO working-set sidecar: this run records one cold)"
    fi
fi

# --- measured run ----------------------------------------------------------
say "measured run: $REPS reps/arm, warmup $WARMUP, arms $ARMS, $BACKEND/$UFFD_MODE prefetch=$UFFD_PREFETCH"
TAG="$TAG" URL="$URLS" BACKEND="$BACKEND" UFFD_MODE="$UFFD_MODE" \
    UFFD_PREFETCH="$UFFD_PREFETCH" ARMS="$ARMS" REPS="$REPS" WARMUP="$WARMUP" \
    RESULTS="$RESULTS" \
    make -C "$REPO" bench-chromium-request-run 2>&1 | tee "$LOGDIR/run.log"

say "records: $RESULTS"
say "logs:    $LOGDIR"
