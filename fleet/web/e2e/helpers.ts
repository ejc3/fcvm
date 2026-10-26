import { expect, type Page } from "@playwright/test";
import { frameCtx, expectedRgb, type PatternScene } from "../mock/pattern.ts";
import type { CaptureResult } from "../app/_wall/worker-msgs.ts";

export const COORD = `http://localhost:${process.env.FLEET_E2E_COORD_PORT}`;

/**
 * Open the wall in a stream mode the viewer has chosen (so the page's own
 * suggestion cannot move it mid-test) and wait until every live region draws
 * at the common t.
 */
export async function openWall(page: Page, minSynced = 15, mode: "direct" | "aggregated" = "direct"): Promise<void> {
  await page.addInitScript((m) => {
    try {
      window.localStorage.setItem("fleet-wall.mode", m);
    } catch {
      // Without storage the page starts direct anyway.
    }
  }, mode);
  await page.goto("/?debug=1");
  await page.waitForFunction(() => window.__fleet !== undefined);
  try {
    await expect
      .poll(async () => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 })
      .toBeGreaterThanOrEqual(minSynced);
  } catch (e) {
    // Whether the mock kept up: frames it dropped because a render was still in flight, and renders queued.
    const rs = (await (await fetch(`${COORD}/mock/render-stats`)).json()) as { subscribers: number; framesSent: number; framesSkipped: number; renderBacklog: number };
    console.log(`openWall: mock render stats at the timeout ${JSON.stringify(rs)}`);
    throw e;
  }
}

export async function mockState(): Promise<{
  version: number;
  state: Record<string, unknown> & { scene: { words: { text: string }[] } };
  /** The scene as the mock renderers draw it. */
  pattern: PatternScene;
}> {
  const r = await fetch(`${COORD}/mock/state`);
  return (await r.json()) as never;
}

export async function setSkew(tile: string, skew: number): Promise<void> {
  await admin("/mock/fault", { tile, skew });
}

/** The time of day, as the coordinator's director sets it per hold (there is no viewer intent for it). */
export async function setTimeOfDay(v: number): Promise<void> {
  await admin("/mock/schedule", { time_of_day: v });
}

/** A mock admin route: faults, the schedule, Turnstile, going offline. */
export async function admin(path: string, body: unknown): Promise<void> {
  const r = await fetch(`${COORD}${path}`, { method: "POST", body: JSON.stringify(body) });
  expect(r.status, `${path} ${await r.clone().text()}`).toBe(200);
}

/** The mock's state at the moment, typed loosely for tests. */
export async function fleetNow(): Promise<FleetNow> {
  return (await mockState()).state as unknown as FleetNow;
}

export interface FleetNow {
  tiles: { id: string; phase: string; borrowed: { until: number } | null; last_relaunch: { first_frame_at: number | null; phases: Record<string, number> } | null }[];
  relaunch_all: { started_at: number; by: string; fcvm_done_at: number | null; ec2_done_at: number | null; ec2_capacity_wait_ms: number } | null;
  scoreboard: { fcvm: { last_ms: number; median_ms: number } | null; ec2: { last_ms: number; median_ms: number; last_capacity_wait_ms: number } | null };
  nudges: { holding: string | null };
  scene: { time_of_day: number };
}

/** Wait until no race runs, so the next test starts at rest. */
export async function raceOver(): Promise<void> {
  await expect.poll(async () => {
    const ra = (await fleetNow()).relaunch_all;
    return ra === null || (ra.fcvm_done_at !== null && ra.ec2_done_at !== null);
  }, { timeout: 40_000 }).toBe(true);
}

export interface SeamReport {
  T: number | null;
  shown: Record<string, string>;
  /** Mean absolute error per tile against the pattern at T, 0..255. */
  perTile: Record<string, number>;
  /** Mean absolute difference across each internal boundary, against the pattern's own. */
  boundaries: { a: string; b: string; err: number }[];
  samples: number;
}

/**
 * Read back pixels on both sides of every boundary between two drawn tiles, in
 * one draw, and compare them with the pattern at that draw's t.
 */
export async function seamReport(page: Page): Promise<SeamReport> {
  const v = await page.evaluate(() => {
    const view = window.__fleet!.view();
    return { image: view.layout!.image, tiles: view.tiles.map((t) => ({ id: t.id, box: t.box!, dark: t.dark })) };
  });
  const live = v.tiles.filter((t) => !t.dark && t.box);
  type P = { x: number; y: number; tile: string; pair: string };
  const pts: P[] = [];
  const near = (a: number, b: number) => Math.abs(a - b) < 0.75;
  for (const a of live) {
    for (const b of live) {
      if (a.id >= b.id) continue;
      // A left of B or B left of A: a vertical boundary.
      for (const [l, r] of [[a, b], [b, a]] as const) {
        if (near(l.box.x + l.box.w, r.box.x)) {
          const y0 = Math.max(l.box.y, r.box.y);
          const y1 = Math.min(l.box.y + l.box.h, r.box.y + r.box.h);
          if (y1 - y0 < 8) continue;
          for (let k = 1; k <= 12; k++) {
            const y = Math.floor(y0 + ((y1 - y0) * k) / 13) + 0.5;
            const x = r.box.x;
            const pair = `${l.id}|${r.id}`;
            pts.push({ x: x - 3 + 0.5, y, tile: l.id, pair }, { x: x + 2 + 0.5, y, tile: r.id, pair });
          }
        }
      }
      for (const [t, u] of [[a, b], [b, a]] as const) {
        if (near(t.box.y + t.box.h, u.box.y)) {
          const x0 = Math.max(t.box.x, u.box.x);
          const x1 = Math.min(t.box.x + t.box.w, u.box.x + u.box.w);
          if (x1 - x0 < 8) continue;
          for (let k = 1; k <= 12; k++) {
            const x = Math.floor(x0 + ((x1 - x0) * k) / 13) + 0.5;
            const y = u.box.y;
            const pair = `${t.id}|${u.id}`;
            pts.push({ x, y: y - 3 + 0.5, tile: t.id, pair }, { x, y: y + 2 + 0.5, tile: u.id, pair });
          }
        }
      }
    }
  }
  const res = (await page.evaluate((p) => window.__fleet!.client.capture(p), pts.map(({ x, y }) => ({ x, y })))) as CaptureResult;
  const f = frameCtx(res.T!, (await mockState()).pattern);
  const uv = (x: number, y: number) => ({ u: (x - v.image.x) / v.image.w, v: (y - v.image.y) / v.image.h });
  // Smooth sample pairs: expected and observed colour on each side.
  type Sample = { tile: string; pair: string; e: [number, number, number]; o: [number, number, number] };
  const pairs: [Sample, Sample][] = [];
  for (let i = 0; i < pts.length; i += 2) {
    const p = [pts[i]!, pts[i + 1]!];
    // Skip pairs where the picture itself changes fast (text, rings, stars):
    // compression and scaling error there says nothing about seams.
    const smooth = p.every((q) => {
      const c = uv(q.x, q.y);
      const e0 = expectedRgb(c.u, c.v, f);
      // Within 4 px, which covers 4:2:0 chroma and the scaling filter's reach.
      return [[-2, 0], [2, 0], [0, -2], [0, 2], [-4, 0], [4, 0], [0, -4], [0, 4], [-3, -3], [3, 3], [-3, 3], [3, -3]].every(([dx, dy]) => {
        const n = uv(q.x + dx!, q.y + dy!);
        const e1 = expectedRgb(n.u, n.v, f);
        return Math.max(...e0.map((x, k) => Math.abs(x - e1[k]!))) <= 6;
      });
    });
    if (!smooth) continue;
    const mk = (k: number): Sample => {
      const c = uv(p[k]!.x, p[k]!.y);
      const o = res.pixels[i + k]!;
      return { tile: p[k]!.tile, pair: p[k]!.pair, e: expectedRgb(c.u, c.v, f), o: [o[0], o[1], o[2]] };
    };
    pairs.push([mk(0), mk(1)]);
  }
  // Browsers convert the decoded YUV to RGB in their own way (matrix, range,
  // rounding), so compare up to one colour transform fitted over every sample:
  // what is left is what differs between tiles.
  // Fit once, then again without tiles far off the rest, so one wrong tile
  // cannot bend the transform toward itself.
  const residual = (m: (e: [number, number, number]) => [number, number, number], x: Sample) => {
    const v = m(x.e);
    return (Math.abs(x.o[0] - v[0]) + Math.abs(x.o[1] - v[1]) + Math.abs(x.o[2] - v[2])) / 3;
  };
  const first = fitColour(pairs.flat());
  const byTile = new Map<string, number[]>();
  for (const x of pairs.flat()) byTile.set(x.tile, [...(byTile.get(x.tile) ?? []), residual(first, x)]);
  const tileMeans = [...byTile.entries()].map(([t, rs]) => [t, rs.reduce((a, b) => a + b, 0) / rs.length] as const);
  const sorted = tileMeans.map(([, m]) => m).sort((a, b) => a - b);
  const med = sorted[Math.floor(sorted.length / 2)] ?? 0;
  const outliers = new Set(tileMeans.filter(([, m]) => m > Math.max(2 * med, 1)).map(([t]) => t));
  const map = fitColour(pairs.flat().filter((x) => !outliers.has(x.tile)));
  const sums: Record<string, { s: number; n: number }> = {};
  const pairSums: Record<string, { s: number; n: number }> = {};
  const samples = pairs.length;
  for (const [a, b] of pairs) {
    for (const x of [a, b]) {
      const m = map(x.e);
      const s = (sums[x.tile] ??= { s: 0, n: 0 });
      s.s += (Math.abs(x.o[0] - m[0]) + Math.abs(x.o[1] - m[1]) + Math.abs(x.o[2] - m[2])) / 3;
      s.n += 1;
    }
    // Across the boundary: observed difference against the expected difference.
    const ma = map(a.e);
    const mb = map(b.e);
    const across = [0, 1, 2].reduce((acc, k) => acc + Math.abs(b.o[k]! - a.o[k]! - (mb[k]! - ma[k]!)), 0) / 3;
    const ps = (pairSums[a.pair] ??= { s: 0, n: 0 });
    ps.s += across;
    ps.n += 1;
  }
  const perTile: Record<string, number> = {};
  for (const [id, s] of Object.entries(sums)) perTile[id] = s.s / s.n;
  const boundaries = Object.entries(pairSums).map(([k, s]) => {
    const [a, b] = k.split("|") as [string, string];
    return { a, b, err: s.s / s.n };
  });
  return { T: res.T, shown: res.shown, perTile, boundaries, samples };
}

/** Mean error of the opened tile against the picture at the drawn t, 0..255. */
export async function focusError(page: Page, tileId: string): Promise<{ err: number; shown: string; T: number | null }> {
  const v = await page.evaluate((id) => {
    const view = window.__fleet!.view();
    const t = window.__fleet!.client.getState().accepted!.tiles.find((x) => x.id === id)!;
    return { box: view.plan.focus!.box!, rect: t.rect };
  }, tileId);
  const pts: { x: number; y: number }[] = [];
  for (let j = 1; j < 10; j++) for (let i = 1; i < 16; i++) pts.push({ x: v.box.x + (v.box.w * i) / 16, y: v.box.y + (v.box.h * j) / 10 });
  const res = (await page.evaluate((p) => window.__fleet!.client.capture(p), pts)) as CaptureResult;
  const f = frameCtx(res.T!, (await mockState()).pattern);
  const samples = pts.map((p, k) => {
    const u = v.rect.x + ((p.x - v.box.x) / v.box.w) * v.rect.w;
    const vv = v.rect.y + ((p.y - v.box.y) / v.box.h) * v.rect.h;
    const o = res.pixels[k]!;
    return { e: expectedRgb(u, vv, f), o: [o[0], o[1], o[2]] as [number, number, number] };
  });
  // Up to the browser's colour conversion, as in seamReport.
  const map = fitColour(samples);
  let err = 0;
  for (const s of samples) {
    const m = map(s.e);
    err += (Math.abs(s.o[0] - m[0]) + Math.abs(s.o[1] - m[1]) + Math.abs(s.o[2] - m[2])) / 3;
  }
  return { err: err / samples.length, shown: res.shown[`d:${tileId}`] ?? "", T: res.T };
}

/**
 * The colour transform that best maps expected RGB to observed RGB (each output
 * channel an affine function of the three inputs, least squares).
 */
export function fitColour(samples: { e: [number, number, number]; o: [number, number, number] }[]): (e: [number, number, number]) => [number, number, number] {
  const coef: number[][] = [];
  for (let c = 0; c < 3; c++) {
    // Normal equations for [r, g, b, 1] . w = o_c.
    const A = Array.from({ length: 4 }, () => [0, 0, 0, 0]);
    const y = [0, 0, 0, 0];
    for (const s of samples) {
      const x = [s.e[0], s.e[1], s.e[2], 1];
      for (let i = 0; i < 4; i++) {
        y[i]! += x[i]! * s.o[c]!;
        for (let j = 0; j < 4; j++) A[i]![j]! += x[i]! * x[j]!;
      }
    }
    for (let i = 0; i < 4; i++) A[i]![i]! += 1e-6; // a flat colour sample set stays solvable
    coef.push(solve4(A, y));
  }
  return (e) => [0, 1, 2].map((c) => coef[c]![0]! * e[0] + coef[c]![1]! * e[1] + coef[c]![2]! * e[2] + coef[c]![3]!) as [number, number, number];
}

function solve4(A: number[][], y: number[]): number[] {
  const m = A.map((row, i) => [...row, y[i]!]);
  for (let col = 0; col < 4; col++) {
    let piv = col;
    for (let r = col + 1; r < 4; r++) if (Math.abs(m[r]![col]!) > Math.abs(m[piv]![col]!)) piv = r;
    [m[col], m[piv]] = [m[piv]!, m[col]!];
    for (let r = 0; r < 4; r++) {
      if (r === col) continue;
      const k = m[r]![col]! / m[col]![col]!;
      for (let c = col; c < 5; c++) m[r]![c]! -= k * m[col]![c]!;
    }
  }
  return m.map((row, i) => row[4]! / row[i]!);
}
