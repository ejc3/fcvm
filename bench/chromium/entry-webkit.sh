#!/bin/sh
# webkit-bench container entry: Xvfb + fixture pageserver + WARM WebKitGTK
# session behind WebKitWebDriver.
#
# Sequence: Xvfb -> pageserver -> WebKitWebDriver -> CREATE the warm session
# (wddrive.py --create: launches MiniBrowser --automation, navigates a fixture,
# screenshots once to heat the raster/encode path, proves the blank transition)
# -> touch the ready file (HEALTHCHECK gates on it) -> print READY -> hold.
#
# REQUEST PATH: WebKitWebDriver's HTTP endpoint is the request server — the
# host reaches published TCP 9515 through fcvm's DNAT-to-loopback and speaks
# classic WebDriver straight to it. Nothing of ours is in the byte path.
#
# THE SESSION IS PART OF THE SNAPSHOT. Classic WebDriver has no session
# discovery: a session created after restore would launch a cold browser, so
# the warm session id minted here is what every clone inherits. It is persisted
# twice: /run/bench-session-id for the in-guest health probe, and
# pages/webdriver-session.txt so the HOST driver can fetch it over the
# published pageserver port without an exec round trip.
set -eu

PAGES_DIR="${BENCH_PAGES_DIR:-/opt/bench/pages}"
HTTP_ADDR="${BENCH_HTTP_ADDR:-0.0.0.0}"
HTTP_PORT="${BENCH_HTTP_PORT:-8000}"
WD_PORT="${BENCH_WD_PORT:-9515}"
READY_FILE="${BENCH_READY_FILE:-/run/bench-ready}"
SESSION_FILE="${BENCH_SESSION_FILE:-/run/bench-session-id}"
DISPLAY_NUM="${BENCH_DISPLAY:-:99}"

rm -f "$READY_FILE" "$SESSION_FILE"

probe() {
    python3 -c 'import sys,urllib.request; urllib.request.urlopen(sys.argv[1], timeout=1)' "$1" 2>/dev/null
}

wait_http() { # url tries label — poll every 100ms
    _i=0
    while ! probe "$1"; do
        _i=$((_i + 1))
        if [ "$_i" -ge "$2" ]; then
            echo "ERROR: $3 not answering at $1 after $2 tries" >&2
            return 1
        fi
        sleep 0.1
    done
}

# WebKitGTK cannot render without a display server; Xvfb is the cheapest one.
# -noreset keeps the server alive when the last client disconnects (MiniBrowser
# restarts during session churn would otherwise kill it).
echo "webkit-bench: starting Xvfb on $DISPLAY_NUM"
Xvfb "$DISPLAY_NUM" -screen 0 1280x800x24 -nolisten tcp -noreset &
export DISPLAY="$DISPLAY_NUM"

# WAIT for the X socket. Nothing else here does: the two wait_http loops below
# can each return on their first probe, and GTK opens the display before it
# parses argv, so on a loaded box MiniBrowser reached the display before Xvfb
# was listening, create_session failed, `set -e` killed PID 1, and the container
# never became healthy. The Containerfile's own build-time probe sleeps 1 s for
# exactly this reason; poll the socket instead of guessing a duration.
x_socket="/tmp/.X11-unix/X${DISPLAY_NUM#:}"
for _ in $(seq 1 200); do
    [ -S "$x_socket" ] && break
    sleep 0.05
done
[ -S "$x_socket" ] || { echo "FATAL: Xvfb never created $x_socket" >&2; exit 1; }
echo "webkit-bench: Xvfb listening ($x_socket)"

echo "webkit-bench: starting pageserver on $HTTP_ADDR:$HTTP_PORT"
python3 /opt/bench/pageserver.py --root "$PAGES_DIR" --addr "$HTTP_ADDR" \
    --port "$HTTP_PORT" --ready-file "$READY_FILE" &
wait_http "http://127.0.0.1:$HTTP_PORT/minimal.html" 50 pageserver

# WebKit's bubblewrap sandbox needs user namespaces the guest container does
# not grant — same trust argument as Chromium's --no-sandbox: the microVM is
# the isolation boundary, each clone is single-tenant and destroyed after one
# request. LIBGL_ALWAYS_SOFTWARE pins Mesa to llvmpipe (no GPU on this
# platform) under the DEFAULT renderer. Do NOT add
# WEBKIT_DISABLE_COMPOSITING_MODE or WEBKIT_DISABLE_DMABUF_RENDERER "for
# headless": measured 2026-08-14 on webkit2gtk 2.50.6 under Xvfb, either one
# makes GET /session/<id>/screenshot hang forever (page still alive — JS
# executes) while the untouched default renders a PNG in 0.1s. The snapshot
# path requires the compositing/DMA-BUF renderer these flags remove.
export WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1
export LIBGL_ALWAYS_SOFTWARE=1

# --host=all: WebKitWebDriver binds wildcard so fcvm's health probe path and
# any future non-loopback ingress work; the published-port DNAT lands on guest
# loopback either way. Session creation is the only mutating surface and the
# port is reachable only through fcvm's per-clone forwarding (see the security
# rationale in entry.sh — identical trust model).
echo "webkit-bench: starting WebKitWebDriver on :$WD_PORT"
WebKitWebDriver --port="$WD_PORT" --host=all &
WD_PID=$!
wait_http "http://127.0.0.1:$WD_PORT/status" 300 webkit-webdriver

echo "webkit-bench: creating warm session + warming renderer"
# TWO screenshot passes, both timed in this log. The first heats
# raster/encode/GTK init; the second PROVES the heat took (its screenshot_ms
# should collapse, the way 1,429 ms -> 87 ms was measured post-restore). If a
# later gated run still shows a cold first screenshot per clone, this log line
# is the evidence that the cost does not survive the snapshot -- a finding, not
# a config error. The quiescing --then-blank stays on the LAST pass so the
# golden still snapshots an idle about:blank.
python3 /opt/bench/wddrive.py "http://127.0.0.1:$HTTP_PORT/warmup.html" \
    --host "127.0.0.1:$WD_PORT" --create --session-file "$SESSION_FILE" \
    --out-prefix /tmp/warmup
python3 /opt/bench/wddrive.py "http://127.0.0.1:$HTTP_PORT/warmup.html" \
    --host "127.0.0.1:$WD_PORT" --session-file "$SESSION_FILE" \
    --out-prefix /tmp/warmup2 --then-blank

# The host driver reads the inherited session id over the published pageserver
# port; the health probe reads the /run copy.
cp "$SESSION_FILE" "$PAGES_DIR/webdriver-session.txt"

# Resident health checker, started BEFORE the warm marker so the interpreter's
# pages are dirtied pre-snapshot and shared by every clone. stdout to /dev/null:
# a per-second writer would otherwise grow podman's container log on the clone's
# CoW disk forever.
python3 /opt/bench/wd_health.py --loop >/dev/null 2>&1 &
echo "webkit-bench: resident health checker started"

# Warm marker. wddrive.py --then-blank returns zero only after the navigate,
# the PNG screenshot, and a verified about:blank transition, and `set -e`
# means any failure exits before this touch — same golden-snapshot contract as
# the Chromium entry.
touch "$READY_FILE"
echo "BENCH_READY engine=webkit wd=127.0.0.1:$WD_PORT pages=http://127.0.0.1:$HTTP_PORT session=$(cat "$SESSION_FILE")"

# Hold the warm driver. wait exits with WebKitWebDriver's status if it dies so
# the container stops instead of hiding a dead driver; the trap keeps `podman
# stop` from eating the 10s SIGKILL fallback (this shell is PID 1).
cleanup_webkit() {
    trap - TERM INT
    kill "$WD_PID" 2>/dev/null || true
    (
        sleep 5
        if kill -0 "$WD_PID" 2>/dev/null; then
            echo "webkit-bench: WebKitWebDriver ignored TERM for 5s; sending KILL" >&2
            kill -KILL "$WD_PID" 2>/dev/null || true
        fi
    ) &
    killer_pid=$!
    wait "$WD_PID" 2>/dev/null || true
    kill "$killer_pid" 2>/dev/null || true
    wait "$killer_pid" 2>/dev/null || true
    exit 0
}
trap cleanup_webkit TERM INT
wait "$WD_PID"
