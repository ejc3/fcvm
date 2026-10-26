// What the streams cost a viewer's connection, and the rule that keeps it in
// a budget. The renderer encodes at a constant bits-per-pixel of the delivered
// picture at 30 fps, between a floor and a cap per stream; `q` picks the level
// (fleet/PROTOCOL.md, "Stream quality").

import { deliveredGeometry, STREAM_HEIGHTS, tileGeometry } from "./layout.ts";
import type { Rect } from "./protocol.ts";

export type Quality = "hi" | "std" | "lo";

export const ENC_BPP: Record<Quality, number> = { hi: 4, std: 0.8, lo: 0.25 };
export const MIN_STREAM_BPS: Record<Quality, number> = { hi: 2_000_000, std: 600_000, lo: 150_000 };
export const MAX_STREAM_BPS: Record<Quality, number> = { hi: 160_000_000, std: 20_000_000, lo: 20_000_000 };
export const STREAM_FPS = 30;
/** What the page lets its direct streams add up to. */
export const BANDWIDTH_BUDGET_BPS = 80_000_000;

/** The renderer's bit rate for a tile asked at height h and quality q. */
export function streamBitrate(rect: Rect, imageAspect: number, h: number, q: Quality = "std"): number {
  const d = deliveredGeometry(tileGeometry(rect, imageAspect, h));
  return Math.min(MAX_STREAM_BPS[q], Math.max(MIN_STREAM_BPS[q], d.width * d.height * ENC_BPP[q] * STREAM_FPS));
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
 * Heights and qualities that fit the budget. Every stream but the kept one moves
 * together, so neighbouring tiles never differ in quality: while the sum is over
 * the budget, all of them drop from `std` to `lo`; then all of them step down one
 * height at a time. Streams at the smallest height stay there, so the floors can
 * leave the sum over budget.
 */
export function fitBandwidth(asks: readonly StreamAsk[], imageAspect: number, budgetBps: number = BANDWIDTH_BUDGET_BPS): Fitted {
  const heights = new Map(asks.map((a) => [a.id, a.height]));
  const quality = new Map<string, Quality>(asks.map((a) => [a.id, "std"]));
  const total = () => asks.reduce((s, a) => s + streamBitrate(a.rect, imageAspect, heights.get(a.id)!, quality.get(a.id)!), 0);
  const movable = asks.filter((a) => !a.keep);
  const smallest = STREAM_HEIGHTS[0];
  let bps = total();
  if (bps > budgetBps && movable.length > 0) {
    for (const a of movable) quality.set(a.id, "lo");
    bps = total();
  }
  while (bps > budgetBps) {
    let stepped = false;
    for (const a of movable) {
      const h = heights.get(a.id)!;
      if (h <= smallest) continue;
      heights.set(a.id, [...STREAM_HEIGHTS].reverse().find((x) => x < h) ?? smallest);
      stepped = true;
    }
    if (!stepped) break;
    bps = total();
  }
  return { heights, quality, bps };
}
