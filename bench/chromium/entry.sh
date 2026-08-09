#!/bin/sh
# chromium-bench container entry: fixture pageserver + WARM headless Chromium.
#
# Sequence: start pageserver -> launch Chromium with CDP -> warm it (navigate a
# fixture, screenshot once to heat the raster/encode path, park on about:blank)
# -> touch the ready file (which the container HEALTHCHECK gates on) -> print the
# READY marker -> hold the browser open.
#
# REQUEST PATH: there is no benchmark-owned userspace relay or request server.
# Chromium's own DevTools Protocol endpoint is the request server -- it is a
# fully specified, already-implemented protocol that returns the screenshot as
# base64 in the CDP response. The host opens a WebSocket through fcvm's published
# TCP path and fc-agent's PREROUTING DNAT to the page target, then sends
# Page.navigate + Page.captureScreenshot. Chromium's listener is an ordinary
# in-guest TCP socket, not a virtio-vsock socket, so the
# VIRTIO_VSOCK_EVENT_TRANSPORT_RESET that a restore raises does not touch it.
#
# The pageserver and render.py remain in the image but are OFF the request path:
# the pageserver serves fixtures (the in-guest fixture arm), and render.py is
# used once here to warm the renderer and by the legacy per-request exec arm the
# benchmark A/Bs against.
#
# The pageserver binds 0.0.0.0 by default (not 127.0.0.1): fcvm health checks
# arrive on the guest's eth0 IP via nsenter, not on guest loopback, so a
# loopback-only bind would break a `--health-check http://127.0.0.1:<port>/ready`
# warm-point trigger. Fixture URLs used by the driver stay on 127.0.0.1. Set
# BENCH_HTTP_ADDR=127.0.0.1 for a strictly loopback-only server.
set -eu

PAGES_DIR="${BENCH_PAGES_DIR:-/opt/bench/pages}"
HTTP_ADDR="${BENCH_HTTP_ADDR:-0.0.0.0}"
HTTP_PORT="${BENCH_HTTP_PORT:-8000}"
CDP_PORT="${BENCH_CDP_PORT:-9222}"
# Wildcard by default so the host can ingress. See the flag comment below for the
# security rationale and the one condition that makes it acceptable.
CDP_ADDR="${BENCH_CDP_ADDR:-0.0.0.0}"
READY_FILE="${BENCH_READY_FILE:-/run/bench-ready}"

rm -f "$READY_FILE"

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

echo "chromium-bench: starting pageserver on $HTTP_ADDR:$HTTP_PORT"
python3 /opt/bench/pageserver.py --root "$PAGES_DIR" --addr "$HTTP_ADDR" \
    --port "$HTTP_PORT" --ready-file "$READY_FILE" &
wait_http "http://127.0.0.1:$HTTP_PORT/minimal.html" 50 pageserver

echo "chromium-bench: launching chromium"
# ---------------------------------------------------------------------------
# WORKAROUND for an upstream Chromium/glibc data race. DO NOT DELETE.
#
# Symptom: Chromium 151.0.7922.71 (Debian bookworm arm64) SIGSEGVs during early
# init at ~7% under launch concurrency, before CDP ever comes up. Faulting stack
# is getenv() <- FcConfigGetFilename <- FcInitLoadConfigAndFonts <-
# gfx::InitializeGlobalFontConfigAsync()'s posted task, on a thread-pool worker;
# the faulting instruction is glibc's environ walk `ldr x19,[x20,#8]!` with a
# freed-slot pointer in x19.
#
# Cause: with the GPU in-process (--single-process / --in-process-gpu) ANGLE's
# ScopedVkLoaderEnvironment probes the SwiftShader ICD on Chrome_InProcGpuThread
# and calls setenv("VK_ICD_FILENAMES", ...) at the same time as the async
# fontconfig init is inside getenv() on a thread-pool worker. glibc's setenv of
# a name that is NOT yet present grows the environ array with realloc(); when
# the block moves, the old array is freed under the reader and PartitionAlloc
# hands the memory straight back out -> poisoned pointer -> SIGSEGV. glibc
# documents setenv as MT-Unsafe const:env; getenv and setenv are not safe
# concurrently, and nothing in Chromium serialises these two.
#
# Fix: pre-seed the name in our environment, with the exact value ANGLE would
# compose from its module directory (verify with
#   podman run --rm --entrypoint sh IMAGE -c 'ls /usr/lib/chromium/vk_swiftshader_icd.json'
# -- the "/./" is ANGLE's path concatenation, not a typo). Because the name
# already exists, glibc takes the slot-replace path: it swaps one pointer inside
# the array, never reallocating and never freeing it, so there is nothing for
# the concurrent getenv() walk to fall off. It also flips ANGLE's teardown from
# unsetenv() (array shift) to setenv() (slot replace), so the array is never
# structurally mutated at all.
#
# Whether the realloc actually moves depends on how many variables the parent
# happens to export -- glibc's environ block alternates "grew in place" and
# "moved" as the count goes up -- which is why some launchers never see this and
# a clean run is NOT evidence of a fix. Under a race amplifier that widens the
# getenv() window (LD_PRELOAD getenv shim, 20ms), at the crash-prone parity:
#   unfixed  15/15 crashed (sh parent) and 15/15 (python3 parent, all SIGSEGV)
#   this fix  0/30 (sh parent) and 0/30 (python3 parent), at both parities
# Natural rate, 12 concurrent containers x 28 launches = 336 launches each:
#   unfixed  25/336 = 7.4% [5.1%, 10.8%] Wilson 95%
#   this fix  0/336 = 0.0% [0.0%,  1.1%] Wilson 95%
#
# Upstream: https://issues.angleproject.org/issues/543664586 (ANGLE
# ScopedVkLoaderEnvironment setenv vs gfx::InitializeGlobalFontConfigAsync).
# Remove this only once that bug is fixed AND the fixed Chromium is in the image.
#
# NOTE: harmless when the GPU is out-of-process (today's flag set -- traced: the
# setenv then happens in the GPU *process*, which has its own environ), and load
# bearing the moment --single-process or --in-process-gpu is turned on. It is
# not conditional on those flags precisely so that turning them on cannot
# silently reintroduce a 7% startup crash.
#
# REJECTED alternative: --disable-software-rasterizer also stops the ICD probe
# (0/15 amplified) and drops ~18 threads, but it takes WebGL with it -- our
# webgl.html fixture reports "no-webgl" instead of "webgl:WebGL 2.0 (OpenGL ES
# 3.0 Chromium)" and its screenshot changes, while CDP still reports success.
# Silent capability loss; do not add it. --use-gl=disabled has the same problem.
# ---------------------------------------------------------------------------
VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/usr/lib/chromium/./vk_swiftshader_icd.json}"
export VK_ICD_FILENAMES

# CB_SITE_ISOLATION=off adds --disable-site-isolation-trials, which collapses
# per-origin renderer processes into one. It must be decided BEFORE the golden
# snapshot is taken (the process structure is baked into guest memory), so it is
# a boot-time env var (`fcvm podman run --env CB_SITE_ISOLATION=off`), not a
# per-request flag. The bench measures on and off as separate golden snapshots.
SITE_ISO_FLAGS=""
if [ "${CB_SITE_ISOLATION:-on}" = "off" ]; then
    SITE_ISO_FLAGS="--disable-site-isolation-trials"
    echo "chromium-bench: site isolation DISABLED (--disable-site-isolation-trials)"
fi

# --no-sandbox           : no user namespaces inside the guest container
# --remote-debugging-address=0.0.0.0 : IGNORED BY THIS CHROMIUM. Kept deliberately.
#                          MEASURED 2026-08-08 on chromium 151.0.7922.71 (Debian bookworm
#                          arm64): with BOTH --remote-debugging-port=9222 and
#                          --remote-debugging-address=0.0.0.0 on the command line
#                          (confirmed present in /proc/<pid>/cmdline), /proc/net/tcp shows
#                          the listener bound to 127.0.0.1:9222 and nothing else. A host
#                          connect to the published port TCP-connects and is then RESET —
#                          which looks exactly like a Host-header rejection and is not one.
#                          Reproduced with a minimal `chromium --headless=new --no-sandbox
#                          --disable-gpu --remote-debugging-address=0.0.0.0
#                          --remote-debugging-port=9222` too, so it is the build, not our
#                          flag set. Host ingress therefore CANNOT come from a wildcard
#                          bind on this image — and it CANNOT come from --forward-localhost
#                          either. That flag runs GUEST -> HOST ("Enables containers to
#                          reach host-only services via localhost", src/cli/args.rs:236;
#                          fc-agent/src/network.rs::setup_localhost_forwarding binds the
#                          GUEST's 127.0.0.1:<port> and dials the host gateway 10.0.2.2),
#                          so pointing it at 9222 HIJACKS Chromium's own loopback CDP port
#                          and breaks the readiness probe below. Ingress comes from the
#                          ordinary --publish: fcvm DNATs a published port to guest
#                          127.0.0.1, so no relay is needed. See publish_to_loopback.
#                          The flag stays because it is free and correct-in-intent: if a
#                          future Chromium honours it, the wildcard bind becomes available
#                          without another archaeology session. Re-verify /proc/net/tcp
#                          before believing it works.
#                          SECURITY: were it honoured, it would hand full browser control (arbitrary
#                          navigation, JS execution, local file reads via CDP) to
#                          anything that can reach the port. Acceptable HERE for exactly
#                          the reason --no-sandbox is: the microVM is the isolation
#                          boundary, each clone is single-tenant, serves one request and
#                          is destroyed, and the port is reachable only through fcvm's
#                          per-clone forwarding. NOT acceptable in a shared or
#                          long-lived VM -- do not copy this into a context where the VM
#                          boundary is not doing that work.
# --remote-allow-origins : primary bench arm connects CDP from outside the page origin;
#                          without it the WebSocket upgrade is rejected on Origin check
# --ignore-certificate-errors : bench arm renders self-signed https from the host
#                          fixture server; must be baked in BEFORE the snapshot
# --disable-gpu          : no GPU on this platform; forces deterministic software raster
# --disable-dev-shm-usage: podman default /dev/shm is 64MB; raster buffers go to /tmp
# --user-data-dir=/tmp   : keep profile writes in /tmp so a --tmpfs /tmp mount keeps
#                          dirtied rootfs extents (and destroy cost) down
# remaining flags        : startup-variance reduction (no first-run, no background
#                          fetches, no crash uploader, fixed window)
HOME=/tmp chromium \
    --headless=new \
    --no-sandbox \
    --remote-debugging-port="$CDP_PORT" \
    --remote-debugging-address="$CDP_ADDR" \
    --remote-allow-origins='*' \
    --ignore-certificate-errors \
    --disable-gpu \
    --disable-dev-shm-usage \
    --window-size=1280,800 \
    --hide-scrollbars \
    --mute-audio \
    --no-first-run \
    --no-default-browser-check \
    --disable-background-networking \
    --disable-breakpad \
    --disable-component-update \
    --user-data-dir=/tmp/chrome-profile \
    $SITE_ISO_FLAGS \
    about:blank &
CHROME_PID=$!
wait_http "http://127.0.0.1:$CDP_PORT/json/version" 300 chromium-cdp

# ---------------------------------------------------------------------------
# NO RELAY. Chromium binds guest loopback only (it ignores
# --remote-debugging-address; see the flag block above), and fcvm now DNATs each
# eligible published TCP port to 127.0.0.1 inside the guest — so
# `--publish 9222:9222` reaches Chromium directly. See
# fc-agent/src/network.rs::publish_to_loopback and
# DESIGN.md "Eligible published TCP ports reach guest loopback".
#
# The deleted socat relay was one process and one byte-path hop per clone. The
# earlier request-path A/B that reported connection drops was withdrawn because
# its arms were not comparable; it is not evidence that the relay caused those
# failures.
# ---------------------------------------------------------------------------

echo "chromium-bench: warming renderer"
python3 /opt/bench/render.py "http://127.0.0.1:$HTTP_PORT/warmup.html" \
    --out-prefix /tmp/warmup --then-blank

# Warm marker. The container HEALTHCHECK (cdp_health.py) requires BOTH this file
# AND a live CDP round trip that finds a page target, so fcvm's health gate — the
# trigger for the golden snapshot — cannot fire on a browser that is merely
# listening. "Healthy" therefore means "warm and provably able to screenshot".
touch "$READY_FILE"
echo "CHROMIUM_BENCH_READY cdp=127.0.0.1:$CDP_PORT pages=http://127.0.0.1:$HTTP_PORT"

# Hold the warm browser. wait exits with chromium's status if it dies, so the
# container stops instead of hiding a dead browser behind sleep infinity. As
# PID 1 this shell gets no default signal handlers — without the trap a
# `podman stop` SIGTERM is dropped and teardown eats the 10s SIGKILL fallback.
cleanup_chromium() {
    # Disable re-entry while this handler waits and reaps.
    trap - TERM INT
    kill "$CHROME_PID" 2>/dev/null || true

    # POSIX sh has no timed wait. A reaped timer bounds Chromium's TERM grace
    # period without polling a zombie (for which kill -0 remains true).
    (
        sleep 5
        if kill -0 "$CHROME_PID" 2>/dev/null; then
            echo "chromium-bench: Chromium ignored TERM for 5s; sending KILL" >&2
            kill -KILL "$CHROME_PID" 2>/dev/null || true
        fi
    ) &
    killer_pid=$!
    wait "$CHROME_PID" 2>/dev/null || true
    kill "$killer_pid" 2>/dev/null || true
    wait "$killer_pid" 2>/dev/null || true
    exit 0
}
trap cleanup_chromium TERM INT
wait "$CHROME_PID"
