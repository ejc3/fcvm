// Measure what one browser pays to play the wall: decode and draw cost per tile
// and the frame budget at 16, 50 and 100 tiles, in Playwright's Chromium, against
// the mock fleet. Indicative only: it runs on whatever machine runs it, with the
// mock's renderers on the same machine.
//
//   pnpm build && FLEET_MEASURE_BASE_PORT=<port> node scripts/measure.ts [16 50 100]
//
// FLEET_MEASURE_VIEW=2560x1440 measures a larger screen (taller streams per tile).
//
// Uses FLEET_MEASURE_BASE_PORT .. +2 (from the environment or .env.ports).

import { spawn } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
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
const counts = process.argv.slice(2).map(Number).filter((n) => n > 0);
const totals = counts.length > 0 ? counts : [16, 50, 100];
const WARM_MS = Number(process.env.FLEET_MEASURE_WARM_MS ?? 8000);
const RUN_MS = Number(process.env.FLEET_MEASURE_RUN_MS ?? 15000);
const [vw, vh] = (process.env.FLEET_MEASURE_VIEW ?? "1440x900").split("x").map(Number) as [number, number];
const VIEW = { width: vw, height: vh };
const TICK = 100; // clock ticks per second (USER_HZ)

function descendants(root: number): number[] {
  const kids = new Map<number, number[]>();
  for (const d of readdirSync("/proc")) {
    if (!/^\d+$/.test(d)) continue;
    try {
      const stat = readFileSync(`/proc/${d}/stat`, "utf8");
      const ppid = Number(stat.slice(stat.lastIndexOf(")") + 2).split(" ")[1]);
      const list = kids.get(ppid) ?? [];
      list.push(Number(d));
      kids.set(ppid, list);
    } catch {
      // Gone.
    }
  }
  const out = [root];
  for (let i = 0; i < out.length; i++) out.push(...(kids.get(out[i]!) ?? []));
  return out;
}

function cpuTicks(pids: number[]): number {
  let sum = 0;
  for (const p of pids) {
    try {
      const stat = readFileSync(`/proc/${p}/stat`, "utf8");
      const f = stat.slice(stat.lastIndexOf(")") + 2).split(" ");
      sum += Number(f[11]) + Number(f[12]);
    } catch {
      // Gone.
    }
  }
  return sum;
}

const pct = (xs: number[], p: number) => {
  const s = [...xs].sort((a, b) => a - b);
  return s.length === 0 ? 0 : s[Math.min(s.length - 1, Math.floor(p * s.length))]!;
};

const results: Record<string, unknown>[] = [];
for (const total of totals) {
  const perHalf = Math.round(total / 2);
  const env = {
    ...process.env,
    FLEET_WEB_PORT: String(base),
    FLEET_MOCK_COORD_PORT: String(base + 1),
    FLEET_MOCK_RENDER_PORT: String(base + 2),
    FLEET_MOCK_FCVM_GPUS: String(perHalf),
    FLEET_MOCK_EC2_GPUS: String(perHalf),
    // Nothing restarts while it measures.
    FLEET_MOCK_RACE_EVERY_S: "0",
    FLEET_MOCK_THREADS: process.env.FLEET_MOCK_THREADS ?? "32",
  };
  const server = spawn(process.execPath, ["scripts/dev-mock.ts", "--prod"], { env, stdio: ["ignore", "pipe", "inherit"], detached: true });
  await new Promise<void>((resolve) => {
    server.stdout!.on("data", (d: Buffer) => {
      if (d.toString().includes("Ready")) resolve();
    });
  });
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: VIEW, deviceScaleFactor: 1 });
  // Direct mode: one stream per tile, what the browser must decode at most.
  await page.addInitScript(() => window.localStorage.setItem("fleet-wall.mode", "direct"));
  await page.goto(`http://localhost:${base}/?debug=1`);
  await page.waitForFunction(() => window.__fleet?.client.bench !== null && window.__fleet !== undefined);
  await page.waitForTimeout(WARM_MS);
  // The browser is a child of this process; the server tree is not the browser.
  const browserPids = () => {
    const serverTree = new Set(descendants(server.pid!));
    return descendants(process.pid).filter((p) => p !== process.pid && !serverTree.has(p));
  };
  const pidsBefore = browserPids();
  const cpu0 = cpuTicks(pidsBefore);
  const render0 = (await (await fetch(`http://localhost:${base + 1}/mock/render-stats`)).json()) as { framesSent: number; framesSkipped: number };
  const bytes0 = await page.evaluate(() => window.__fleet!.client.bench!.bytesReceived);
  const samples: { decoded: number; draws: number; synced: number; behind: number; drawP50: number; drawP95: number; rafP50: number; rafP95: number; decP50: number; decP95: number; height: number }[] = [];
  const t0 = Date.now();
  while (Date.now() - t0 < RUN_MS) {
    await page.waitForTimeout(500);
    const s = await page.evaluate(() => {
      const c = window.__fleet!.client;
      const st = c.getState();
      const hs = Object.values(st.frames?.tiles ?? {}).map((t) => t.height);
      return { b: c.bench!, f: st.frames!, height: hs.length ? Math.max(...hs) : 0 };
    });
    samples.push({
      decoded: s.b.decodedPerSec,
      draws: s.b.drawsPerSec,
      synced: s.b.tilesSynced,
      behind: s.b.tilesBehind,
      drawP50: s.f.drawMsP50,
      drawP95: s.f.drawMsP95,
      rafP50: s.f.rafMsP50,
      rafP95: s.f.rafMsP95,
      decP50: s.b.decodeMsP50,
      decP95: s.b.decodeMsP95,
      height: s.height,
    });
  }
  const seconds = (Date.now() - t0) / 1000;
  const pidsAfter = [...new Set([...pidsBefore, ...browserPids()])];
  const cpu1 = cpuTicks(pidsAfter);
  const render1 = (await (await fetch(`http://localhost:${base + 1}/mock/render-stats`)).json()) as { framesSent: number; framesSkipped: number };
  const cores = (cpu1 - cpu0) / TICK / seconds;
  const bytes1 = await page.evaluate(() => window.__fleet!.client.bench!.bytesReceived);
  const rendererBps = await page.evaluate(() => window.__fleet!.view().plan.estimatedBps);
  const decodedPerSec = samples.reduce((a, s) => a + s.decoded, 0) / samples.length;
  const row = {
    tiles: perHalf * 2,
    streamHeight: pct(samples.map((s) => s.height), 0.5),
    sentPerSec: Math.round((render1.framesSent - render0.framesSent) / seconds),
    mockSkippedPerSec: Math.round((render1.framesSkipped - render0.framesSkipped) / seconds),
    decodedPerSec: Math.round(decodedPerSec),
    drawsPerSec: Math.round(pct(samples.map((s) => s.draws), 0.5)),
    tilesSyncedMedian: pct(samples.map((s) => s.synced), 0.5),
    tilesBehindMax: Math.max(...samples.map((s) => s.behind)),
    drawMsP50: +pct(samples.map((s) => s.drawP50), 0.5).toFixed(2),
    drawMsP95: +pct(samples.map((s) => s.drawP95), 0.95).toFixed(2),
    rafMsP50: +pct(samples.map((s) => s.rafP50), 0.5).toFixed(2),
    rafMsP95: +pct(samples.map((s) => s.rafP95), 0.95).toFixed(2),
    decodeLatencyMsP50: +pct(samples.map((s) => s.decP50), 0.5).toFixed(2),
    decodeLatencyMsP95: +pct(samples.map((s) => s.decP95), 0.95).toFixed(2),
    incomingMbps: +(((bytes1 - bytes0) * 8) / seconds / 1e6).toFixed(1),
    rendererRateMbps: +(rendererBps / 1e6).toFixed(1),
    browserCores: +cores.toFixed(2),
    cpuMsPerDecodedFrame: +((cores * 1000) / decodedPerSec).toFixed(3),
  };
  results.push(row);
  console.log(JSON.stringify(row));
  await browser.close();
  try {
    process.kill(-server.pid!, "SIGTERM");
  } catch {
    // Already gone.
  }
  await new Promise((r) => setTimeout(r, 1500));
}
console.log(JSON.stringify({ viewport: VIEW, runMs: RUN_MS, results }, null, 2));
