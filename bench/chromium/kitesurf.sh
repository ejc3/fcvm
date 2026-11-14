#!/bin/bash
# The Kitesurf arm: Cloudflare's Kitesurf engine, built at a pinned commit
# (kitesurf_build.sh), run locally under `wrangler dev` (workerd) on this box,
# and driven by the same cdpdrive.py over the same 14-URL corpus replay the fcvm
# arm renders. corpus_extra.sh runs it as PHASES=kitesurf, inside its replay
# bracket; it is not meant to run on its own.
#
# What this measures, and what it does not. `wrangler dev` runs the engine
# Worker in one workerd process on this host, and every page in a fresh isolate
# that workerd creates through the Worker Loader binding, which is how Kitesurf
# runs on Cloudflare too. Each request opens its own CDP WebSocket, and Kitesurf
# keeps no state across connections (its AGENTS.md: "nothing persists across
# connections"), so every request starts from no page state, as an fcvm clone
# does. It is NOT Cloudflare's hosted Browser Run: there is no network hop
# between the client and the engine (CDP on 127.0.0.1), the page's subresources
# come from the replay on this host rather than from the sites, all isolates
# share this one host's cores rather than being placed across Cloudflare's
# fleet, and workerd here is the open-source build wrangler ships, not
# Cloudflare's production runtime with its own limits and scheduling. Cloudflare's
# published Kitesurf figures were measured on the hosted product; these were not.
#
# The replay. corpus_serve.py answers on 127.0.0.1 (HTTP 80, HTTPS 443), but its
# DNS answers 10.0.2.2, which is the guests' gateway and not reachable here. The
# engine therefore runs in its own user and mount namespaces in which
# /etc/hosts names every host the capture holds as 127.0.0.1 and
# /etc/nsswitch.conf resolves from that file alone: the equivalent of the host
# Chromium's "MAP * 127.0.0.1" for the names the capture can answer, and no path
# to the live web for any other name (it fails to resolve, where Chromium's rule
# would reach the replay and get a 404). workerd verifies certificates, so the
# replay serves a leaf naming those hosts, signed by a CA it writes for this run
# (corpus_serve.py --tls-ca-out), which miniflare adds to workerd's trust store
# from NODE_EXTRA_CA_CERTS. The network namespace is the host's.
#
# Correctness. Kitesurf implements part of CDP and of the web platform. Before
# the measured reps, render_check.py renders every page once on Kitesurf and
# once on the bench image's Chromium (the image fcvm's arm restores) and keeps
# both screenshots and both documents. A page that fails or comes back blank
# there counts every measured request on it as a failure, and the summary gives
# per-page success for both engines.
#
# Resources. The engine runs in a systemd user scope of its own: CPU per request
# is that cgroup's usage_usec, split by process (workerd and the wrangler node
# process) from /proc, and memory is the cgroup's memory.current per request and
# memory.peak at the end, which includes the engine's start-up.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=kitesurf_pins.sh
. "$HERE/kitesurf_pins.sh"

: "${RESULTS:?RESULTS is required}"
: "${URLS:?URLS is required (comma-separated)}"
: "${RUNID:?RUNID is required}"
: "${CA_CERT:?CA_CERT is required: the replay's CA certificate (corpus_serve.py --tls-ca-out)}"
: "${CORPUS_ROOT:?CORPUS_ROOT is required: the corpus-live directory the replay serves}"
: "${CORPUS_ACCESS_LOG:?CORPUS_ACCESS_LOG is required: the replay's access log}"
REPS="${REPS:-202}"
WARMUP="${WARMUP:-28}"
ARMS="${KITESURF_ARMS:-cdp,html}"
KITESURF_DIR="${KITESURF_DIR:-/mnt/fcvm-btrfs/kitesurf}"
KITESURF_PORT="${KITESURF_PORT:-28787}"
KITESURF_INSPECTOR_PORT="${KITESURF_INSPECTOR_PORT:-28788}"
CHROMIUM_CHECK="${CHROMIUM_CHECK:-1}"
IMAGE="${IMAGE:-localhost/chromium-bench-req}"
IMAGE_ID="${IMAGE_ID:-}"
CONTAINER_OWNER_TOKEN="${CONTAINER_OWNER_TOKEN:-}"
CHROMIUM_CDP_PORT="${CHROMIUM_CDP_PORT:-9222}"
SOURCE_REVISION="${SOURCE_REVISION:-}"
LOADAVG_FILE="${LOADAVG_FILE:-/proc/loadavg}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >&2; }

[[ "$RUNID" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,95}$ ]] \
    || { echo "REFUSING: RUNID must be 1-96 filename-safe characters" >&2; exit 2; }
[[ "$REPS" =~ ^[1-9][0-9]*$ && "$WARMUP" =~ ^[0-9]+$ ]] \
    || { echo "REFUSING: REPS must be a positive integer and WARMUP a nonnegative one" >&2; exit 2; }
case ",$ARMS," in
    *,cdp,*|*,html,*) ;;
    *) echo "REFUSING: KITESURF_ARMS must name cdp, html or both" >&2; exit 2 ;;
esac
for arm in ${ARMS//,/ }; do
    case "$arm" in cdp|html) ;; *) echo "REFUSING: unknown Kitesurf arm '$arm'" >&2; exit 2 ;; esac
done
for port in "$KITESURF_PORT" "$KITESURF_INSPECTOR_PORT" "$CHROMIUM_CDP_PORT"; do
    [[ "$port" =~ ^[0-9]+$ ]] && [ "$port" -ge 1 ] && [ "$port" -le 65535 ] \
        || { echo "REFUSING: port '$port' is not in 1..65535" >&2; exit 2; }
done
[ -s "$CA_CERT" ] || { echo "REFUSING: CA_CERT $CA_CERT is missing or empty" >&2; exit 2; }
for tool in awk curl grep id pgrep podman python3 sha256sum systemctl systemd-run timeout unshare; do
    command -v "$tool" >/dev/null 2>&1 || { echo "REFUSING: '$tool' missing" >&2; exit 2; }
done

SRC="$KITESURF_DIR/src"
SEAL="$KITESURF_DIR/BUILD.json"
[ -f "$SEAL" ] || { echo "REFUSING: no Kitesurf build at $KITESURF_DIR; run make bench-kitesurf-build" >&2; exit 2; }
arch=$(uname -m)
case "$arch" in aarch64) node_arch=arm64 ;; x86_64) node_arch=x64 ;; *) echo "REFUSING: $arch" >&2; exit 2 ;; esac
NODE_BIN="$KITESURF_DIR/tools/node-$KITESURF_NODE_VERSION-linux-$node_arch/bin"
[ -x "$NODE_BIN/node" ] || { echo "REFUSING: pinned node missing at $NODE_BIN" >&2; exit 2; }
# wrangler's own entry point, run by the pinned node; pnpm's .bin shim is an
# install artifact the seal does not cover.
WRANGLER="$SRC/packages/engine/node_modules/wrangler/bin/wrangler.js"
[ -f "$WRANGLER" ] || { echo "REFUSING: wrangler missing at $WRANGLER" >&2; exit 2; }

mkdir -p "$RESULTS"
log "verifying the Kitesurf build seal ($KITESURF_REV)"
seal_before=$(python3 "$HERE/kitesurf_seal.py" verify --src "$SRC" --seal "$SEAL" --rev "$KITESURF_REV") \
    || exit 2
tree_sha256=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["tree_sha256"])' "$seal_before")
cp "$SEAL" "$RESULTS/kitesurf-build.json"

find_strays() {
    local process output rc found=""
    for process in fcvm firecracker workerd; do
        set +e
        output=$(pgrep -a -x "$process" 2>&1)
        rc=$?
        set -e
        case "$rc" in
            0) found="${found}${found:+$'\n'}${output}" ;;
            1) ;;
            *) echo "REFUSING: pgrep exited $rc while checking $process: $output" >&2; return 2 ;;
        esac
    done
    [ -z "$found" ] || { printf 'REFUSING: stray processes:\n%s\n' "$found" >&2; return 1; }
}
find_strays || exit 2
load1=$(awk '{print $1}' "$LOADAVG_FILE")
awk -v l="$load1" 'BEGIN{exit !(l > 2.0)}' && { echo "REFUSING: 1-min load $load1 > 2.0" >&2; exit 2; }

# The namespace's name service: every captured host on the replay's address.
NS="$RESULTS/ns"
mkdir -p "$NS"
python3 - "$CORPUS_ROOT" "$NS/hosts" <<'PY'
import glob, json, os, sys, urllib.parse
root, out = sys.argv[1:]
hosts = set()
for path in glob.glob(os.path.join(root, "*", "index.json")):
    with open(path) as handle:
        data = json.load(handle)
    for url in list(data.get("resources", {})) + list(data.get("redirects", {})):
        name = urllib.parse.urlsplit(url).hostname
        if name:
            hosts.add(name)
if not hosts:
    sys.exit(f"REFUSING: no captured hosts under {root}")
with open(out, "w") as handle:
    handle.write("127.0.0.1 localhost\n::1 localhost\n")
    for name in sorted(hosts):
        handle.write(f"127.0.0.1 {name}\n")
print(f"{len(hosts)} captured hosts mapped to 127.0.0.1", file=sys.stderr)
PY
printf 'passwd: files\ngroup: files\nhosts: files\n' > "$NS/nsswitch.conf"
printf '# no resolver: names resolve from /etc/hosts alone\n' > "$NS/resolv.conf"

UNIT="kitesurf-$RUNID"
ENGINE_PID=""
CHROMIUM_CONTAINER=""
stop_engine() {
    if [ -n "$ENGINE_PID" ]; then
        systemctl --user stop "$UNIT.scope" >/dev/null 2>&1 || kill -TERM "$ENGINE_PID" 2>/dev/null || true
        for _ in $(seq 1 100); do kill -0 "$ENGINE_PID" 2>/dev/null || break; sleep 0.1; done
        kill -KILL "$ENGINE_PID" 2>/dev/null || true
        wait "$ENGINE_PID" 2>/dev/null || true
        ENGINE_PID=""
    fi
}
# The check's Chromium must be gone before Kitesurf is measured: a container
# left running would be foreign CPU the stray check cannot see. Removal is
# proved with `podman container exists`; on any doubt the id is kept (so the
# exit trap tries again) and the caller stops.
stop_chromium() {
    local exists_rc
    [ -n "$CHROMIUM_CONTAINER" ] || return 0
    timeout --kill-after=5s 60s podman rm -f -t 5 "$CHROMIUM_CONTAINER" >/dev/null 2>&1
    if timeout --kill-after=5s 30s podman container exists "$CHROMIUM_CONTAINER" >/dev/null 2>&1; then
        echo "FAILED: the correctness check's Chromium ($CHROMIUM_CONTAINER) survived podman rm" >&2
        return 1
    else
        exists_rc=$?
    fi
    [ "$exists_rc" -eq 1 ] || {
        echo "FAILED: cannot verify removal of the correctness check's Chromium ($CHROMIUM_CONTAINER)" >&2
        return 1
    }
    CHROMIUM_CONTAINER=""
}
on_exit() {
    local rc=$?
    trap - EXIT
    stop_engine
    stop_chromium || { [ "$rc" -ne 0 ] || rc=1; }
    exit "$rc"
}
trap on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

URLS_ARG="$URLS"

# Chromium's side of the correctness pass: the image fcvm's arm restores, on the
# host network, every name mapped to the replay as the host control does.
if [ "$CHROMIUM_CHECK" = 1 ]; then
    [[ "$CONTAINER_OWNER_TOKEN" =~ ^[0-9a-f]{32}$ ]] \
        || { echo "REFUSING: CONTAINER_OWNER_TOKEN must be 32 hex characters for the Chromium check" >&2; exit 2; }
    image_ref="${IMAGE_ID:-$IMAGE}"
    log "correctness: starting the bench image's Chromium for its own render check"
    CHROMIUM_CONTAINER=$(timeout --kill-after=5s 120s podman run -d --rm \
        --name "kschk-$RUNID" --label "io.fcvm.bench.owner=$CONTAINER_OWNER_TOKEN" \
        --network host -e BENCH_RESOLVE_ALL_TO=127.0.0.1 "$image_ref")
    for _ in $(seq 1 240); do
        curl -s --noproxy '*' --max-time 2 "http://127.0.0.1:$CHROMIUM_CDP_PORT/json/list" 2>/dev/null \
            | grep -q '"type": *"page"' && break
        sleep 0.5
    done
    python3 "$HERE/render_check.py" "127.0.0.1:$CHROMIUM_CDP_PORT" --urls "$URLS_ARG" \
        --out "$RESULTS/correctness/chromium" --engine chromium > "$RESULTS/correctness-chromium.log" 2>&1 \
        || { cat "$RESULTS/correctness-chromium.log" >&2; echo "FAILED: Chromium render check" >&2; exit 1; }
    stop_chromium || exit 1
fi

log "starting Kitesurf (wrangler dev, workerd) on 127.0.0.1:$KITESURF_PORT in scope $UNIT"
ENGINE_LOG="$RESULTS/engine.log"
# The command `pnpm run dev` runs after its Bazel prebuild, which
# make bench-kitesurf-build already did.
# The engine's environment: the pinned node first on PATH, the replay's CA for
# miniflare to hand workerd, no telemetry.
systemd-run --user --scope --quiet --collect --unit "$UNIT" -- \
    env PATH="$NODE_BIN:/usr/bin:/bin" NODE_EXTRA_CA_CERTS="$CA_CERT" \
    WRANGLER_SEND_METRICS=false WRANGLER_LOG_PATH="$RESULTS/wrangler-logs" CI=1 \
    unshare --user --map-root-user --mount -- /bin/sh -c '
        set -e
        mount --bind "$1/hosts" /etc/hosts
        mount --bind "$1/nsswitch.conf" /etc/nsswitch.conf
        mount --bind "$1/resolv.conf" /etc/resolv.conf
        cd "$2/packages/engine"
        exec node "$3" dev --env local -c wrangler.jsonc -c test/fixtures-worker/wrangler.jsonc \
            --port "$4" --inspector-port "$5" --ip 127.0.0.1
    ' _ "$NS" "$SRC" "$WRANGLER" "$KITESURF_PORT" "$KITESURF_INSPECTOR_PORT" \
    > "$ENGINE_LOG" 2>&1 < /dev/null &
ENGINE_PID=$!
ready=false
for _ in $(seq 1 600); do
    kill -0 "$ENGINE_PID" 2>/dev/null || { cat "$ENGINE_LOG" >&2; echo "FAILED: the engine exited during start-up" >&2; exit 1; }
    if [ "$(curl -s --noproxy '*' --max-time 2 "http://127.0.0.1:$KITESURF_PORT/health" 2>/dev/null)" = "kitesurf: OK" ]; then
        ready=true
        break
    fi
    sleep 0.5
done
[ "$ready" = true ] || { tail -40 "$ENGINE_LOG" >&2; echo "FAILED: Kitesurf never answered /health" >&2; exit 1; }
cgroup_rel=$(systemctl --user show -p ControlGroup --value "$UNIT.scope")
CGROUP="/sys/fs/cgroup$cgroup_rel"
case "$cgroup_rel" in /?*) ;; *) echo "FAILED: scope $UNIT has no cgroup" >&2; exit 1 ;; esac
for f in cpu.stat memory.current memory.peak memory.stat cgroup.procs; do
    [ -r "$CGROUP/$f" ] || { echo "FAILED: $CGROUP/$f is not readable" >&2; exit 1; }
done
# The counters below are this cgroup's, so workerd has to be in it.
engine_in_scope=false
while read -r pid; do
    [ "$(cat "/proc/$pid/comm" 2>/dev/null)" = workerd ] && engine_in_scope=true
done < "$CGROUP/cgroup.procs"
[ "$engine_in_scope" = true ] || { echo "FAILED: no workerd process in $CGROUP" >&2; exit 1; }
version_json=$(curl -s --noproxy '*' --max-time 5 "http://127.0.0.1:$KITESURF_PORT/json/version")
log "engine up: $version_json"

python3 - "$RESULTS/run.json" "$RUNID" "$KITESURF_REV" "$KITESURF_REPO_URL" "$tree_sha256" \
        "$SEAL" "$version_json" "$SOURCE_REVISION" "$URLS_ARG" "$REPS" "$WARMUP" "$ARMS" \
        "$cgroup_rel" "$LOADAVG_FILE" <<'PY'
import json, os, platform, sys
(out, run_id, rev, repo, tree, seal, version_json, source_revision, urls, reps, warmup,
 arms, cgroup, loadavg_file) = sys.argv[1:]
with open(seal) as handle:
    build = json.load(handle)
with open(loadavg_file) as handle:
    load = handle.read().split()[0]
record = {
    "run_id": run_id, "engine": "kitesurf",
    "kitesurf_rev": rev, "kitesurf_repo": repo, "kitesurf_tree_sha256": tree,
    "kitesurf_build": build, "json_version": json.loads(version_json),
    "source_revision": source_revision or None,
    "urls": urls.split(","), "reps": int(reps), "warmup": int(warmup),
    "arms": arms.split(","), "driver": "cdpdrive.py", "driver_process": "in-process",
    "format": "jpeg", "quality": 80, "nav_timing": True,
    "network": "host loopback; names from the namespace's /etc/hosts (captured hosts -> 127.0.0.1)",
    "wrangler_command": "wrangler dev --env local -c wrangler.jsonc -c test/fixtures-worker/wrangler.jsonc",
    "cgroup": cgroup, "host_machine": platform.machine(), "host_kernel": platform.release(),
    "loadavg_at_start": load,
    "hosted_comparison": ("local wrangler dev on this host: no network hop to the engine, "
                          "subresources from the local replay, one host's cores; not Browser Run"),
}
with open(out + ".tmp", "w") as handle:
    json.dump(record, handle, indent=1, sort_keys=True)
    handle.write("\n")
os.replace(out + ".tmp", out)
PY

# The engine must reach the replay, and only the replay: render the first page
# once and find the request in the replay's access log.
probe_url="${URLS_ARG%%,*}"
probe_host=$(printf '%s' "$probe_url" | sed -E 's#^https?://([^/]+).*#\1#')
before=$(wc -l < "$CORPUS_ACCESS_LOG")
python3 "$HERE/cdpdrive.py" "127.0.0.1:$KITESURF_PORT" "$probe_url" --format jpeg > "$RESULTS/probe.json" || true
python3 - "$CORPUS_ACCESS_LOG" "$before" "$probe_host" <<'PY' \
    || { cat "$RESULTS/probe.json" >&2; tail -20 "$ENGINE_LOG" >&2; exit 1; }
import json, sys
path, before, host = sys.argv[1], int(sys.argv[2]), sys.argv[3]
rows = [json.loads(l) for l in open(path).read().splitlines()[before:] if l.strip()]
if not any(r.get("host") == host for r in rows):
    sys.exit(f"FAILED: the engine's probe render of {host} never reached the replay")
print(f"probe: {sum(r.get('host') == host for r in rows)} replay requests for {host}", file=sys.stderr)
PY

log "correctness: Kitesurf render check over the corpus"
python3 "$HERE/render_check.py" "127.0.0.1:$KITESURF_PORT" --urls "$URLS_ARG" \
    --out "$RESULTS/correctness/kitesurf" --engine kitesurf > "$RESULTS/correctness-kitesurf.log" 2>&1 \
    || { cat "$RESULTS/correctness-kitesurf.log" >&2; echo "FAILED: Kitesurf render check" >&2; exit 1; }

find_strays_but_engine() {
    local out
    out=$(pgrep -a -x fcvm 2>/dev/null; pgrep -a -x firecracker 2>/dev/null) || true
    [ -z "$out" ] || { printf 'REFUSING: stray VMMs:\n%s\n' "$out" >&2; return 1; }
}
find_strays_but_engine || exit 2

log "measuring: arms $ARMS, $REPS measured reps plus $WARMUP warmup each, interleaved"
measure_status=0
python3 "$HERE/kitesurf_measure.py" --address "127.0.0.1:$KITESURF_PORT" --urls "$URLS_ARG" \
    --arms "$ARMS" --reps "$REPS" --warmup "$WARMUP" --cgroup "$CGROUP" \
    --loadavg-file "$LOADAVG_FILE" --run-json "$RESULTS/run.json" \
    --out "$RESULTS/kitesurf.jsonl" || measure_status=$?

peak=$(cat "$CGROUP/memory.peak")
stop_engine
seal_after=$(python3 "$HERE/kitesurf_seal.py" verify --src "$SRC" --seal "$SEAL" --rev "$KITESURF_REV") \
    || { echo "FAILED: the Kitesurf tree changed during the run" >&2; exit 5; }
[ "$measure_status" -eq 0 ] || { echo "FAILED: the measurement loop exited $measure_status" >&2; exit "$measure_status"; }

python3 "$HERE/kitesurf_measure.py" summarize --jsonl "$RESULTS/kitesurf.jsonl" \
    --run-json "$RESULTS/run.json" --memory-peak "$peak" \
    --correctness "$RESULTS/correctness" --out "$RESULTS/summary.json"
# Last: the record corpus_extra.sh's campaign completion seals. It names every
# artifact the arm's numbers come from by size and sha256.
python3 "$HERE/kitesurf_measure.py" complete --results "$RESULTS" --run-id "$RUNID" \
    --out "$RESULTS/complete.json"
log "results in $RESULTS"
