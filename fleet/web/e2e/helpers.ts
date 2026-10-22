import { expect, type Page } from "@playwright/test";
import { frameCtx, expectedRgb, type PatternScene } from "../mock/pattern.ts";
import type { CaptureResult } from "../app/_wall/worker-msgs.ts";

export const COORD = `http://localhost:${process.env.FLEET_E2E_COORD_PORT}`;

/** Open the wall and wait until every live tile draws at the common t. */
export async function openWall(page: Page, minSynced = 15): Promise<void> {
  await page.goto("/?debug=1");
  await page.waitForFunction(() => window.__fleet !== undefined);
  await expect
    .poll(async () => page.evaluate(() => window.__fleet!.client.bench?.tilesSynced ?? 0), { timeout: 30_000 })
    .toBeGreaterThanOrEqual(minSynced);
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
  const r = await fetch(`${COORD}/mock/fault`, { method: "POST", body: JSON.stringify({ tile, skew }) });
  expect(r.status).toBe(200);
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
  const sums: Record<string, { s: number; n: number }> = {};
  const pairSums: Record<string, { s: number; n: number }> = {};
  let samples = 0;
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
    samples++;
    const errs = p.map((q, k) => {
      const c = uv(q.x, q.y);
      const e = expectedRgb(c.u, c.v, f);
      const o = res.pixels[i + k]!;
      return (Math.abs(o[0] - e[0]) + Math.abs(o[1] - e[1]) + Math.abs(o[2] - e[2])) / 3;
    });
    p.forEach((q, k) => {
      const s = (sums[q.tile] ??= { s: 0, n: 0 });
      s.s += errs[k]!;
      s.n += 1;
    });
    // Across the boundary: observed difference against the expected difference.
    const o0 = res.pixels[i]!;
    const o1 = res.pixels[i + 1]!;
    const c0 = uv(p[0]!.x, p[0]!.y);
    const c1 = uv(p[1]!.x, p[1]!.y);
    const e0 = expectedRgb(c0.u, c0.v, f);
    const e1 = expectedRgb(c1.u, c1.v, f);
    const across = [0, 1, 2].reduce((acc, k) => acc + Math.abs(o1[k]! - o0[k]! - (e1[k]! - e0[k]!)), 0) / 3;
    const ps = (pairSums[p[0]!.pair] ??= { s: 0, n: 0 });
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
    return { box: view.plan.tiles.find((x) => x.id === id)!.box!, rect: t.rect };
  }, tileId);
  const pts: { x: number; y: number }[] = [];
  for (let j = 1; j < 10; j++) for (let i = 1; i < 16; i++) pts.push({ x: v.box.x + (v.box.w * i) / 16, y: v.box.y + (v.box.h * j) / 10 });
  const res = (await page.evaluate((p) => window.__fleet!.client.capture(p), pts)) as CaptureResult;
  const f = frameCtx(res.T!, (await mockState()).pattern);
  let err = 0;
  let n = 0;
  pts.forEach((p, k) => {
    const u = v.rect.x + ((p.x - v.box.x) / v.box.w) * v.rect.w;
    const vv = v.rect.y + ((p.y - v.box.y) / v.box.h) * v.rect.h;
    const e = expectedRgb(u, vv, f);
    const o = res.pixels[k]!;
    err += (Math.abs(o[0] - e[0]) + Math.abs(o[1] - e[1]) + Math.abs(o[2] - e[2])) / 3;
    n++;
  });
  return { err: err / n, shown: res.shown[tileId] ?? "", T: res.T };
}
