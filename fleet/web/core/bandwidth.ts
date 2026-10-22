// What the streams cost a viewer's connection, and the rule that keeps it in
// a budget. The renderer encodes at about ENC_BPP bits per delivered pixel at
// 30 fps, with a floor per stream (fleet/PROTOCOL.md, "Renderer").

import { deliveredGeometry, STREAM_HEIGHTS, tileGeometry } from "./layout.ts";
import type { Rect } from "./protocol.ts";

export const ENC_BPP = 0.8;
export const STREAM_FPS = 30;
export const MIN_STREAM_BPS = 600_000;
/** What the page lets its streams add up to. */
export const BANDWIDTH_BUDGET_BPS = 80_000_000;

/** The renderer's bit rate for a tile asked at height h. */
export function streamBitrate(rect: Rect, imageAspect: number, h: number): number {
  const d = deliveredGeometry(tileGeometry(rect, imageAspect, h));
  return Math.max(MIN_STREAM_BPS, d.width * d.height * ENC_BPP * STREAM_FPS);
}

export interface StreamAsk {
  id: string;
  rect: Rect;
  height: number;
  /** Not lowered to fit the budget (the opened tile). */
  keep: boolean;
}

/**
 * Heights that fit the budget: while the sum is over it, the tallest stream
 * that may be lowered steps down one height. Streams already at the smallest
 * height stay there, so the floor per stream can leave the sum over budget.
 */
export function fitBandwidth(asks: readonly StreamAsk[], imageAspect: number, budgetBps: number = BANDWIDTH_BUDGET_BPS): { heights: Map<string, number>; bps: number } {
  const heights = new Map(asks.map((a) => [a.id, a.height]));
  const rate = (a: StreamAsk) => streamBitrate(a.rect, imageAspect, heights.get(a.id)!);
  let total = asks.reduce((s, a) => s + rate(a), 0);
  const smallest = STREAM_HEIGHTS[0];
  for (let guard = 0; total > budgetBps && guard < asks.length * STREAM_HEIGHTS.length; guard++) {
    let pick: StreamAsk | null = null;
    for (const a of asks) {
      if (a.keep || heights.get(a.id)! <= smallest) continue;
      if (pick === null || heights.get(a.id)! > heights.get(pick.id)!) pick = a;
    }
    if (pick === null) break;
    const before = rate(pick);
    const h = heights.get(pick.id)!;
    const lower = [...STREAM_HEIGHTS].reverse().find((x) => x < h) ?? smallest;
    heights.set(pick.id, lower);
    total += rate(pick) - before;
  }
  return { heights, bps: total };
}
