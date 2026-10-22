// What the streams cost a viewer's connection, and the rule that keeps it in
// a budget. The renderer encodes at a constant bits-per-pixel of the delivered
// picture at 30 fps, with a floor per stream; `q` picks the level
// (fleet/PROTOCOL.md, "Renderer"). The `lo` and `hi` figures here are this
// page's assumption until the renderer documents them: half and double `std`.

import { deliveredGeometry, STREAM_HEIGHTS, tileGeometry } from "./layout.ts";
import type { Rect } from "./protocol.ts";

export type Quality = "hi" | "std" | "lo";

export const ENC_BPP: Record<Quality, number> = { hi: 1.6, std: 0.8, lo: 0.4 };
export const MIN_STREAM_BPS: Record<Quality, number> = { hi: 1_200_000, std: 600_000, lo: 300_000 };
export const STREAM_FPS = 30;
/** What the page lets its direct streams add up to. */
export const BANDWIDTH_BUDGET_BPS = 80_000_000;

/** The renderer's bit rate for a tile asked at height h and quality q. */
export function streamBitrate(rect: Rect, imageAspect: number, h: number, q: Quality = "std"): number {
  const d = deliveredGeometry(tileGeometry(rect, imageAspect, h));
  return Math.max(MIN_STREAM_BPS[q], d.width * d.height * ENC_BPP[q] * STREAM_FPS);
}

export interface StreamAsk {
  id: string;
  rect: Rect;
  height: number;
  /** Not lowered to fit the budget (the opened tile). */
  keep: boolean;
}

export interface Fitted {
  heights: Map<string, number>;
  quality: Map<string, Quality>;
  bps: number;
}

/**
 * Heights and qualities that fit the budget. While the sum is over it, the
 * smallest stream still at `std` drops to `lo` (small tiles first: they lose
 * least on screen); once all are `lo`, the tallest steps down one height.
 * Streams at the smallest height stay there, so the floors can leave the sum
 * over budget.
 */
export function fitBandwidth(asks: readonly StreamAsk[], imageAspect: number, budgetBps: number = BANDWIDTH_BUDGET_BPS): Fitted {
  const heights = new Map(asks.map((a) => [a.id, a.height]));
  const quality = new Map<string, Quality>(asks.map((a) => [a.id, "std"]));
  const rate = (a: StreamAsk) => streamBitrate(a.rect, imageAspect, heights.get(a.id)!, quality.get(a.id)!);
  let total = asks.reduce((s, a) => s + rate(a), 0);
  const area = (a: StreamAsk) => a.rect.w * a.rect.h * heights.get(a.id)! ** 2;
  const smallest = STREAM_HEIGHTS[0];
  for (let guard = 0; total > budgetBps && guard < asks.length * (STREAM_HEIGHTS.length + 1); guard++) {
    let pick: StreamAsk | null = null;
    for (const a of asks) {
      if (a.keep || quality.get(a.id) !== "std") continue;
      if (pick === null || area(a) < area(pick)) pick = a;
    }
    if (pick !== null) {
      const before = rate(pick);
      quality.set(pick.id, "lo");
      total += rate(pick) - before;
      continue;
    }
    for (const a of asks) {
      if (a.keep || heights.get(a.id)! <= smallest) continue;
      if (pick === null || heights.get(a.id)! > heights.get(pick.id)!) pick = a;
    }
    if (pick === null) break;
    const before = rate(pick);
    const h = heights.get(pick.id)!;
    heights.set(pick.id, [...STREAM_HEIGHTS].reverse().find((x) => x < h) ?? smallest);
    total += rate(pick) - before;
  }
  return { heights, quality, bps: total };
}
