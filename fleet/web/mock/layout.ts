// The mock's tiling: each side as wide as its share of the GPUs live, the fcvm
// side in one full-width band per host (so a host's tiles are one rectangle its
// aggregator encodes), the EC2 side as one region. Within a region the tiles are
// a grid as close to 16:9 as the count allows, its last row stretched to fill.

import type { Rect } from "../core/protocol.ts";

/** n rectangles covering `region` (of an image `aspect` wide), rows of tiles close to 16:9. */
export function gridIn(region: Rect, n: number, aspect: number): Rect[] {
  if (n <= 0) return [];
  let rows = 1;
  let best = Infinity;
  for (let r = 1; r <= n; r++) {
    const cols = Math.ceil(n / r);
    // Every row holds a tile: with more rows than that, the full rows alone would exceed n.
    if (cols * (r - 1) >= n) continue;
    const tileAspect = ((region.w * aspect) / cols) / (region.h / r);
    const err = Math.abs(Math.log(tileAspect / (16 / 9)));
    if (err < best - 1e-9) {
      best = err;
      rows = r;
    }
  }
  const cols = Math.ceil(n / rows);
  const out: Rect[] = [];
  for (let r = 0; r < rows; r++) {
    const inRow = r < rows - 1 ? cols : n - cols * (rows - 1);
    for (let c = 0; c < inRow; c++) {
      out.push({ x: region.x + (c * region.w) / inRow, y: region.y + (r * region.h) / rows, w: region.w / inRow, h: region.h / rows });
    }
  }
  return out;
}

export interface MockLayout {
  /** Where the fcvm side ends. */
  split: number;
  /** Each fcvm host's band, and the rectangles of its GPUs in slot order. */
  hosts: { host: string; band: Rect; rects: Rect[] }[];
  ec2Region: Rect | null;
  ec2: Rect[];
}

/** The tiling for `fcvm` hosts (each with its GPU count, in order) and `ec2` instances. */
export function mockLayout(fcvm: { host: string; gpus: number }[], ec2: number, aspect: number): MockLayout {
  const nf = fcvm.reduce((a, h) => a + h.gpus, 0);
  const total = nf + ec2;
  const split = total === 0 ? 0.5 : nf / total;
  let y = 0;
  const hosts = fcvm
    .filter((h) => h.gpus > 0)
    .map((h) => {
      const band = { x: 0, y, w: split, h: h.gpus / Math.max(1, nf) };
      y += band.h;
      return { host: h.host, band, rects: gridIn(band, h.gpus, aspect) };
    });
  const ec2Region = ec2 > 0 ? { x: split, y: 0, w: 1 - split, h: 1 } : null;
  return { split, hosts, ec2Region, ec2: ec2Region ? gridIn(ec2Region, ec2, aspect) : [] };
}
