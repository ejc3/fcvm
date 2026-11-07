// Where the e2e seam check samples the picture, and which samples it can judge. Shared by
// e2e/helpers.ts (seamReport) and mock/test/seam-pairs.test.ts, which sweeps it over t.

import { expectedRgb, type FrameCtx } from "./pattern.ts";

export interface Box {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** One point of a pair: the tile it lies in and the boundary ("a|b") it samples. */
export interface SeamPoint {
  x: number;
  y: number;
  tile: string;
  pair: string;
}

/**
 * Sample pairs placed along each boundary (a pair: a point 3 px before it and 2 px after it).
 * Only pairs where the picture is smooth count, and with 12 a boundary was often left with 1
 * to 3 of them, so its mean error was one or two pixels' compression noise: on 30 runs of the
 * aggregated check, 26 had a boundary with fewer than 4, and CI failed one at 3.35 (#1348).
 * With 64, over 60 runs of this and the direct check, the fewest was 5 and the worst boundary
 * 1.59 (2.31 with 12, on 30 runs of the aggregated check alone).
 */
export const SEAM_PAIRS = 64;
/** A boundary's error is a mean over at least this many smooth pairs. */
export const SEAM_MIN_PAIRS = 4;

/**
 * The directed camera's stop the seam check is measured at. Whether a tile has SEAM_MIN_PAIRS
 * smooth pairs depends on the picture along its boundaries. At the star field (zoom 0.7) the
 * nebula's gradient is steep at the pixel scale, and the bottom-left tile of the aggregated
 * layout is left with fewer than 4 in 70 of 2801 draws over t (CI failed on one, at t 575.9).
 * At the planet the fewest is 19 at the mock's time of day, and 6 at 0.75 (mock/test/seam-pairs.test.ts).
 */
export const SEAM_STOP = { id: "planet", x: 0.35, y: -0.05, zoom: 1.6 } as const;

/** The pairs along every boundary between two live tiles: points[2k] and points[2k + 1]. */
export function seamPoints(tiles: readonly { id: string; box: Box }[], pairs = SEAM_PAIRS): SeamPoint[] {
  const pts: SeamPoint[] = [];
  const near = (a: number, b: number) => Math.abs(a - b) < 0.75;
  for (const a of tiles) {
    for (const b of tiles) {
      if (a.id >= b.id) continue;
      // A left of B or B left of A: a vertical boundary.
      for (const [l, r] of [[a, b], [b, a]] as const) {
        if (near(l.box.x + l.box.w, r.box.x)) {
          const y0 = Math.max(l.box.y, r.box.y);
          const y1 = Math.min(l.box.y + l.box.h, r.box.y + r.box.h);
          if (y1 - y0 < 8) continue;
          for (let k = 1; k <= pairs; k++) {
            const y = Math.floor(y0 + ((y1 - y0) * k) / (pairs + 1)) + 0.5;
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
          for (let k = 1; k <= pairs; k++) {
            const x = Math.floor(x0 + ((x1 - x0) * k) / (pairs + 1)) + 0.5;
            const y = u.box.y;
            const pair = `${t.id}|${u.id}`;
            pts.push({ x, y: y - 3 + 0.5, tile: t.id, pair }, { x, y: y + 2 + 0.5, tile: u.id, pair });
          }
        }
      }
    }
  }
  return pts;
}

/**
 * Whether the picture is smooth around a point: within 4 px (which covers 4:2:0 chroma and the
 * scaling filter's reach) no channel differs by more than 6. Elsewhere (text, rings, stars, a
 * steep nebula) compression and scaling error says nothing about seams.
 */
export function smoothAt(image: Box, f: FrameCtx, x: number, y: number): boolean {
  const uv = (px: number, py: number) => ({ u: (px - image.x) / image.w, v: (py - image.y) / image.h });
  const c = uv(x, y);
  const e0 = expectedRgb(c.u, c.v, f);
  return [[-2, 0], [2, 0], [0, -2], [0, 2], [-4, 0], [4, 0], [0, -4], [0, 4], [-3, -3], [3, 3], [-3, 3], [3, -3]].every(([dx, dy]) => {
    const n = uv(x + dx!, y + dy!);
    const e1 = expectedRgb(n.u, n.v, f);
    return Math.max(...e0.map((q, k) => Math.abs(q - e1[k]!))) <= 6;
  });
}

/** Smooth pairs per boundary ("a|b") in one frame. */
export function smoothPairs(image: Box, tiles: readonly { id: string; box: Box }[], f: FrameCtx): Record<string, number> {
  const pts = seamPoints(tiles);
  const out: Record<string, number> = {};
  for (let i = 0; i < pts.length; i += 2) {
    const p = pts[i]!;
    out[p.pair] ??= 0;
    if (smoothAt(image, f, p.x, p.y) && smoothAt(image, f, pts[i + 1]!.x, pts[i + 1]!.y)) out[p.pair]! += 1;
  }
  return out;
}

/** Tiles none of whose boundaries has SEAM_MIN_PAIRS smooth pairs in that frame. */
export function unjudged(image: Box, tiles: readonly { id: string; box: Box }[], f: FrameCtx): string[] {
  const n = smoothPairs(image, tiles, f);
  return tiles
    .map((t) => t.id)
    .filter((id) => !Object.entries(n).some(([k, v]) => v >= SEAM_MIN_PAIRS && k.split("|").includes(id)));
}
