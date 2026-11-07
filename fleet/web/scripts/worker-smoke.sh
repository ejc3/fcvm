#!/usr/bin/env bash
# Serve the built Worker (.open-next, from `pnpm cf:build`) with `wrangler dev --local`
# and require GET / to return 200 with the page's title. An Access 302 in front of the
# deployed host says nothing about the Worker behind it: on 2026-10-24 every request to
# stage.fcvm.dev failed with error 1101 (OpenNext 1.20.9 did not inline Next 16.4's
# preview-props.json) while the deploy's Access check passed.
#
# Then it serves the Worker again with a test service token in front of a stand-in
# coordinator (scripts/worker-proxy-check.ts) and requires the page to connect to its own
# origin, and /live and /poster.jpg to reach the coordinator with the token and without the
# viewer's Access cookie (core/live-proxy.ts), and without leaking coordinator Access
# credentials back to the viewer on either the HTTP or the WebSocket response.
#
# Usage: scripts/worker-smoke.sh <port>   (run from fleet/web; uses <port> to <port>+2)
set -euo pipefail

port="${1:?usage: scripts/worker-smoke.sh <port>}"
want='<title>Every GPU, live</title>'
work=$(mktemp -d)
log="$work/wrangler.log"
body="$work/body.html"
pids=()

export WRANGLER_SEND_METRICS=false
pnpm exec wrangler dev --local --ip 127.0.0.1 --port "$port" >"$log" 2>&1 &
pid=$!
pids+=("$pid")
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  for p in "${pids[@]}"; do wait "$p" 2>/dev/null || true; done
  rm -rf "$work"
}
trap cleanup EXIT

fail() {
  echo "worker-smoke: $1" >&2
  echo "--- wrangler dev output ---" >&2
  cat "$log" >&2
  exit 1
}

wait_ready() {
  for _ in $(seq 1 120); do
    grep -q "Ready on" "$log" && break
    kill -0 "$pid" 2>/dev/null || fail "wrangler dev exited before it was ready"
    sleep 0.5
  done
  grep -q "Ready on" "$log" || fail "wrangler dev was not ready after 60 s"
}
wait_ready

# Node, not curl: the Fleet Web job runs in Playwright's image, which may not carry curl.
code=0
status=$(node -e '
  const [url, out] = process.argv.slice(1);
  fetch(url, { signal: AbortSignal.timeout(30000) })
    .then(async (r) => { require("node:fs").writeFileSync(out, await r.text()); console.log(r.status); })
    .catch((e) => { console.error(String(e)); process.exit(1); });
' "http://127.0.0.1:$port/" "$body") || code=$?
[ "$code" -eq 0 ] || fail "GET / failed: node exit $code"
[ "$status" = "200" ] || fail "GET / returned $status, not 200: $(head -c 300 "$body")"
grep -qF "$want" "$body" || fail "GET / returned 200 without $want: $(head -c 300 "$body")"
echo "worker-smoke: GET / 200 with $want"
grep -qF 'wss://fleet-api-stage.fcvm.dev/live' "$body" || fail "without a service token the page does not connect to FLEET_COORD_URL"

# The forwarding: a stand-in coordinator, and the Worker with a test token in front of it.
coord_port=$((port + 2))
worker_port=$((port + 1))
node scripts/worker-proxy-check.ts serve "$coord_port" >"$work/coord.log" 2>&1 &
pids+=("$!")
log="$work/wrangler-proxy.log"
pnpm exec wrangler dev --local --ip 127.0.0.1 --port "$worker_port" \
  --var "FLEET_COORD_URL:ws://127.0.0.1:$coord_port/live" \
  --var FLEET_ACCESS_CLIENT_ID:smoke-id --var FLEET_ACCESS_CLIENT_SECRET:smoke-secret >"$log" 2>&1 &
pid=$!
pids+=("$pid")
wait_ready
node scripts/worker-proxy-check.ts check "$worker_port" || fail "the Worker does not forward the coordinator's paths ($(cat "$work/coord.log"))"
