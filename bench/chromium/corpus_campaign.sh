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

# Every phase runs with debug logging: the stage attribution the analysis relies
# on only exists in fcvm=debug output, and a caller who omitted it would produce
# a run whose records cannot be audited afterwards. Exported once here rather
# than repeated per command, so no phase can be left out.
# Forced, not defaulted. `${RUST_LOG:-...}` let a caller with RUST_LOG=info in
# their shell produce a campaign with no stage records at all -- and nothing
# downstream would say so, because a missing stage looks the same as a stage
# that took no time. The one variable the analysis cannot do without is exactly
# the one an ambient environment is most likely to have already set.
if [ -n "${RUST_LOG:-}" ] && [ "${RUST_LOG}" != "fcvm=debug" ]; then
    echo "note: overriding inherited RUST_LOG=${RUST_LOG} with fcvm=debug (stage attribution needs it)" >&2
fi
export RUST_LOG="fcvm=debug"

# Pin the binary, and say which one. The seal already refuses a golden recorded
# under a different runtime bundle, so a rebuild mid-campaign fails the run
# rather than silently mixing binaries WITHIN one. It does not bind comparisons
# ACROSS runs, and that is not hypothetical: on 2026-08-16 a vCPU curve was
# published whose 2 vCPU points came from fcvm 3976d0ba and whose 4 and 8 vCPU
# points came from 3f85bd26, because the tree was rebuilt between the two
# sweeps. Recording the hash up front makes the boundary visible in the log as
# well as in each run's cell.
FCVM_BIN="${FCVM_BIN:-$REPO/target/release/fcvm}"
[ -x "$FCVM_BIN" ] || { echo "BLOCKED: no fcvm binary at $FCVM_BIN; run make build" >&2; exit 2; }
FCVM_SHA=$(sha256sum "$FCVM_BIN" | cut -d" " -f1)

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
        # Retry and REPORT. corpus_serve can still be releasing :53 as this runs
        # (the poll above bounds its exit at 5s, it does not prove the socket is
        # closed), so a single attempt loses the race. Unchecked, this trap left
        # the box with no DNS resolution and said nothing -- the same defect
        # bench-stop had, in the other code path that restores this service.
        #
        # Exiting non-zero FROM the trap is deliberate: a campaign that leaves
        # the host unable to resolve anything has not finished successfully,
        # whatever its measurements say, and the next run's preflight would fail
        # somewhere far less obvious.
        for _ in $(seq 1 10); do
            sudo systemctl start dnsmasq >/dev/null 2>&1 && break
            sleep 1
        done
        if ! systemctl is-active --quiet dnsmasq; then
            echo "FAILED: dnsmasq did not restart; this box has no DNS." >&2
            echo "Something still holds :53 -- check: sudo ss -lnup 'sport = :53'" >&2
            exit 1
        fi
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
# `[ -s f ] && break` would be the last command in the body, so on the first
# iteration (pidfile not written yet) it returns 1 and `set -e` kills the whole
# script -- observed: the server started, loaded 781 urls, and the campaign
# exited straight into cleanup.
for _ in $(seq 1 50); do
    if [ -s "$SERVE_PIDFILE" ]; then break; fi
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
# Readiness is not the pidfile. The wrapper writes $$ and THEN execs python,
# which still has to load the corpus and bind 53/80/443, so the pid exists
# several seconds before the sockets do. The original fixed `sleep 3` hid that;
# after switching to a pidfile the curl below raced the TLS bind and returned
# 000, which `set -e` turned into a bare exit 7. Poll the sockets instead, so
# the wait is as long as it needs to be and no longer.
answer=""
code=""
for _ in $(seq 1 100); do
    answer=$(dig +short +time=2 +tries=1 @127.0.0.1 blog.cloudflare.com A 2>/dev/null | head -1 || true)
    code=$(curl -sk -o /dev/null -w '%{http_code}' --max-time 5 \
           --resolve 'blog.cloudflare.com:443:127.0.0.1' https://blog.cloudflare.com/ 2>/dev/null || true)
    if [ "$answer" = "10.0.2.2" ] && [ "$code" = "200" ]; then break; fi
    sudo kill -0 "$SERVE_PID" 2>/dev/null || { echo "BLOCKED: corpus_serve died during startup" >&2; cat "$LOGDIR/corpus_serve.log" >&2; exit 3; }
    sleep 0.2
done
[ "$answer" = "10.0.2.2" ] || { echo "BLOCKED: wildcard DNS answered '$answer', expected 10.0.2.2" >&2; exit 3; }
[ "$code" = "200" ] || { echo "BLOCKED: HTTPS replay returned '$code' for blog.cloudflare.com" >&2; exit 3; }
say "replay server up: DNS -> 10.0.2.2, HTTPS 200"

# Probe EVERY corpus member, not just the one used to detect startup. A corpus
# holding only blog.cloudflare.com passed the check above, and the other 13 URLs
# then failed INSIDE the measured run -- where a replay miss is a render of an
# error page, which is a perfectly plausible-looking fast number rather than an
# error. Cheap: 14 local requests against a server already proven up.
missing=""
for url in $(printf '%s\n' "$URLS" | tr ',' ' '); do
    host=$(printf '%s' "$url" | sed -E 's#^https?://([^/]+).*#\1#')
    ucode=$(curl -sk -o /dev/null -w '%{http_code}' --max-time 10 \
            --resolve "$host:443:127.0.0.1" "$url" 2>/dev/null || true)
    case "$ucode" in
    200 | 30[1278]) ;;
    *) missing="$missing\n  $ucode  $url" ;;
    esac
done
if [ -n "$missing" ]; then
    # shellcheck disable=SC2059
    printf "BLOCKED: the corpus does not serve every configured URL:$missing\n" >&2
    echo "A partial corpus measures error pages as renders. Re-record the corpus." >&2
    exit 3
fi
say "corpus complete: all $(printf '%s' "$URLS" | tr ',' '\n' | wc -l) URLs replay locally"
say "fcvm binary $FCVM_BIN sha256=$FCVM_SHA (recorded per run in cell.fcvm_sha256)"

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

# --- settle -----------------------------------------------------------------
# Creating the golden is CPU-heavy (container build, cold boot, snapshot), so
# the box is still hot when this phase ends. The run driver refuses above a
# 1-min load of 2.0 and does not wait, so PHASE=all would throw away the golden
# it just spent minutes building:
#   REFUSING: box is busy (load=2.41, 0 firecracker/fcvm)
# Wait for the average to decay instead. 1.5 leaves margin under the gate, and
# the wait is bounded so a genuinely busy box still fails rather than hanging.
settle_deadline=$(( $(date +%s) + 900 ))
while :; do
    load1=$(awk '{print $1}' /proc/loadavg)
    if awk -v l="$load1" 'BEGIN{exit !(l < 1.5)}'; then break; fi
    if [ "$(date +%s)" -ge "$settle_deadline" ]; then
        echo "BLOCKED: load stayed at $load1 for 15 min; refusing to measure on a busy box" >&2
        exit 4
    fi
    say "waiting for the box to go quiet (1-min load $load1, need < 1.5)"
    sleep 20
done
say "box quiet (1-min load $load1)"

# --- measured run ----------------------------------------------------------
say "measured run: $REPS reps/arm, warmup $WARMUP, arms $ARMS, $BACKEND/$UFFD_MODE prefetch=$UFFD_PREFETCH"
TAG="$TAG" URL="$URLS" BACKEND="$BACKEND" UFFD_MODE="$UFFD_MODE" \
    UFFD_PREFETCH="$UFFD_PREFETCH" ARMS="$ARMS" REPS="$REPS" WARMUP="$WARMUP" \
    RESULTS="$RESULTS" \
    make -C "$REPO" bench-chromium-request-run 2>&1 | tee "$LOGDIR/run.log"

say "records: $RESULTS"
say "logs:    $LOGDIR"
