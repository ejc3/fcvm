// How tiles come back after a race over a slow link (a port-forward through a tunnel),
// against the mock fleet. A TCP proxy sits in front of the coordinator and the renderers
// with connection setup time, one-way delay and one bandwidth cap shared by every
// connection, as one tunnel has; TCP backpressure is kept (a full queue pauses the
// sender). After `restart_now`, the page's canvas is sampled at each tile's centre every
// 100 ms; a tile is back when that pixel is no longer black or the dark-tile fill. Each
// tile reports when it came back against the coordinator's `first_frame_at` for it.
//
//   pnpm build && node scripts/slow-link.ts [direct|aggregated] [clean|slow]
//
// Uses FLEET_MEASURE_BASE_PORT .. +4 (from the environment or .env.ports). Link profile
// "slow": FLEET_LINK_RTT_MS (100), FLEET_LINK_SETUP_MS (400), FLEET_LINK_KBPS (16000).
import { spawn, type ChildProcess } from "node:child_process";
import net from "node:net";
import { chromium } from "@playwright/test";

if (process.env.FLEET_MEASURE_BASE_PORT === undefined) {
  try {
    process.loadEnvFile(".env.ports");
  } catch {
    // No local file; the variable must come from the environment.
  }
}
const base = Number(process.env.FLEET_MEASURE_BASE_PORT);
if (!Number.isInteger(base) || base <= 0) {
  console.error("FLEET_MEASURE_BASE_PORT is not set.");
  process.exit(2);
}
const mode = (process.argv[2] ?? "aggregated") as "direct" | "aggregated";
const link = process.argv[3] ?? "slow";
const P = { web: base, coord: base + 1, render: base + 2, coordProxy: base + 3, renderProxy: base + 4 };
const slow = link === "slow";
const RTT = slow ? Number(process.env.FLEET_LINK_RTT_MS ?? 100) : 0;
const SETUP = slow ? Number(process.env.FLEET_LINK_SETUP_MS ?? 400) : 0;
const BYTES_PER_MS = slow ? Number(process.env.FLEET_LINK_KBPS ?? 16000) / 8 : Infinity;
const QUEUE_MAX = 256 * 1024;

/** One direction of the shared link: bytes leave no sooner than the cap allows, then take RTT/2. */
const lanes = { up: { freeAt: 0, bytes: 0, peakQueueMs: 0 }, down: { freeAt: 0, bytes: 0, peakQueueMs: 0 } };
function pipe(src: net.Socket, dst: net.Socket, lane: { freeAt: number; bytes: number; peakQueueMs: number }): void {
  let queued = 0;
  src.on("data", (chunk: Buffer) => {
    const now = performance.now();
    lane.freeAt = Math.max(lane.freeAt, now) + chunk.length / BYTES_PER_MS;
    lane.bytes += chunk.length;
    lane.peakQueueMs = Math.max(lane.peakQueueMs, lane.freeAt - now);
    queued += chunk.length;
    if (queued > QUEUE_MAX) src.pause();
    setTimeout(() => {
      queued -= chunk.length;
      if (!dst.destroyed) dst.write(chunk);
      if (queued <= QUEUE_MAX / 2 && src.isPaused()) src.resume();
    }, lane.freeAt - now + RTT / 2);
  });
  src.on("end", () => setTimeout(() => dst.end(), Math.max(0, lane.freeAt - performance.now()) + RTT / 2));
  src.on("error", () => dst.destroy());
}
/** When the browser opened each connection to the renderers, as the tunnel sees it. */
const renderAccepts: number[] = [];
function proxy(listen: number, target: number): net.Server {
  return net
    .createServer((client) => {
      if (listen === P.renderProxy) renderAccepts.push(Date.now());
      client.pause();
      // The tunnel opens its far side after a setup time, as a port-forward session does.
      setTimeout(() => {
        const up = net.connect(target, "127.0.0.1", () => {
          pipe(client, up, lanes.up);
          pipe(up, client, lanes.down);
          client.resume();
        });
        up.on("error", () => client.destroy());
        client.on("error", () => up.destroy());
        client.on("close", () => up.destroy());
        up.on("close", () => setTimeout(() => client.destroy(), RTT / 2 + 50));
      }, SETUP);
    })
    .listen(listen, "127.0.0.1");
}

const children: ChildProcess[] = [];
const stopAll = () => {
  for (const c of children) c.kill("SIGTERM");
};
process.on("exit", stopAll);

const servers = [proxy(P.coordProxy, P.coord), proxy(P.renderProxy, P.render)];
const mock = spawn(process.execPath, ["mock/main.ts"], {
  stdio: ["ignore", "pipe", "inherit"],
  env: {
    ...process.env,
    FLEET_MOCK_COORD_PORT: String(P.coord),
    FLEET_MOCK_RENDER_PORT: String(P.render),
    FLEET_MOCK_PUBLIC_RENDER_PORT: String(P.renderProxy),
    FLEET_MOCK_FCVM_GPUS: "8",
    FLEET_MOCK_EC2_GPUS: "7",
    FLEET_MOCK_GPUS_PER_HOST: "4",
    FLEET_MOCK_FCVM_MS: process.env.FLEET_MOCK_FCVM_MS ?? "5000",
    FLEET_MOCK_EC2_MS: process.env.FLEET_MOCK_EC2_MS ?? "12000",
    FLEET_MOCK_RACE_EVERY_S: "0",
    FLEET_MOCK_COOLDOWN_S: "0",
    FLEET_MOCK_WAIT_PERCENT: "0",
    FLEET_MOCK_SLEEP_AFTER_S: "0",
    FLEET_MOCK_HOLD_S: "600",
    FLEET_MOCK_FPS: "15",
    FLEET_MOCK_THREADS: process.env.FLEET_MOCK_THREADS ?? "12",
  },
});
children.push(mock);
await new Promise<void>((resolve) => {
  mock.stdout!.on("data", (d: Buffer) => {
    if (d.toString().includes('"ready"')) resolve();
  });
});
const next = spawn(process.execPath, ["node_modules/next/dist/bin/next", "start", "-p", String(P.web)], {
  stdio: ["ignore", "pipe", "inherit"],
  env: { ...process.env, FLEET_COORD_URL: `ws://localhost:${P.coordProxy}/live`, FLEET_MEDIA_URL: `http://localhost:${P.coord}/media` },
});
children.push(next);
await new Promise<void>((resolve) => next.stdout!.on("data", (d: Buffer) => d.toString().includes("Ready") && resolve()));

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
await page.addInitScript((m) => window.localStorage.setItem("fleet-wall.mode", m), mode);
await page.goto(`http://localhost:${P.web}/?debug=1`);
await page.waitForFunction(() => window.__fleet?.view().tiles.length === 15 && window.__fleet.client.getState().frames?.allLive === true, null, { timeout: 120_000 });
await page.waitForTimeout(5000);

type Sample = { at: number; dark: Record<string, boolean> };
// The link before the race: bytes over 3 s, and the queue it built.
const b0 = lanes.down.bytes;
lanes.down.peakQueueMs = 0;
await page.waitForTimeout(3000);
const steadyMbps = ((lanes.down.bytes - b0) * 8) / 3e6;
const steadyQueueMs = lanes.down.peakQueueMs;
lanes.down.peakQueueMs = 0;
const raceBytes0 = lanes.down.bytes;
const raceAt = await page.evaluate(() => {
  window.__fleet!.client.send("restart_now", {});
  return Date.now();
});
const samples: Sample[] = [];
const ids: string[] = await page.evaluate(() => window.__fleet!.view().tiles.map((t) => t.id));
let backAll = false;
const deadline = Date.now() + 90_000;
type Seen = { tiles: { id: string; phase: string; last_relaunch: { clicked_at: number; first_frame_at: number | null } | null }[] };
let state = null as Seen | null;
while (Date.now() < deadline && !backAll) {
  const r = await page.evaluate(async () => {
    const v = window.__fleet!.view();
    // Eight points on a ring inside each tile (its centre holds the dark tile's glyph).
    const ring = [[0.2, 0.2], [0.5, 0.2], [0.8, 0.2], [0.2, 0.5], [0.8, 0.5], [0.2, 0.8], [0.5, 0.8], [0.8, 0.8]];
    const pts = v.tiles.flatMap((t) => ring.map(([u, w]) => ({ x: t.box!.x + u! * t.box!.w, y: t.box!.y + w! * t.box!.h })));
    const cap = await window.__fleet!.client.capture(pts);
    return { at: Date.now(), ids: v.tiles.map((t) => t.id), px: cap.pixels.map((p) => Math.max(p[0]!, p[1]!, p[2]!)), st: window.__fleet!.client.getState().accepted };
  });
  const dark: Record<string, boolean> = {};
  // Dark: every point black (the aggregator's fill) or the page's dark-tile fill.
  r.ids.forEach((id, i) => (dark[id] = r.px.slice(i * 8, i * 8 + 8).every((m) => m <= 12)));
  samples.push({ at: r.at, dark });
  state = r.st as unknown as Seen;
  const ran = state!.tiles.every((t) => t.phase === "running" && (t.last_relaunch?.clicked_at ?? 0) >= raceAt - 1000);
  backAll = ran && Object.values(dark).every((d) => !d);
  await page.waitForTimeout(100);
}
const rows = ids.map((id) => {
  const t = state!.tiles.find((x) => x.id === id)!;
  const ff = t.last_relaunch?.first_frame_at ?? null;
  // Back: the first sample after the coordinator's first frame from which the pixel stays lit.
  let back: number | null = null;
  for (const s of samples) {
    if (s.dark[id] === undefined) continue;
    if (s.dark[id]) back = null;
    else back ??= s.at;
  }
  return { id, running_after_ms: ff === null ? null : ff - raceAt, shown_after_running_ms: ff === null || back === null ? null : Math.max(0, back - ff) };
});
const lags = rows.map((r) => r.shown_after_running_ms).filter((x): x is number => x !== null).sort((a, b) => a - b);
const pct = (q: number) => lags[Math.min(lags.length - 1, Math.floor(q * lags.length))] ?? null;
const link_down = { steady_mbps: +steadyMbps.toFixed(2), steady_peak_queue_ms: Math.round(steadyQueueMs), race_mbps: +((((lanes.down.bytes - raceBytes0) * 8) / 1e3 / (Date.now() - raceAt))).toFixed(2), race_peak_queue_ms: Math.round(lanes.down.peakQueueMs) };
// Gaps between the browser's connections to the renderers after the race began.
const after = renderAccepts.filter((t) => t >= raceAt).map((t) => t - raceAt);
const connect_gaps_ms = after.slice(1).map((t, i) => t - after[i]!);
console.log(JSON.stringify({ mode, link, rtt_ms: RTT, setup_ms: SETUP, kbps: slow ? BYTES_PER_MS * 8 : null, link_down, connects_after_race_ms: after, connect_gaps_ms, complete: backAll, tiles: rows, shown_after_running_ms: { p50: pct(0.5), max: lags.at(-1) ?? null } }));
await browser.close();
for (const s of servers) s.close();
stopAll();
process.exit(0);
