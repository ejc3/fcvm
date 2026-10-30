// The direct streams' bandwidth budget, from what this viewer's link carries. The page
// starts every viewer at BANDWIDTH_BUDGET_BPS (bandwidth.ts). Where that holds the wall at
// q=lo (a 3460 x 1946 wall of 16 tiles needs 135 Mbit/s at std and 43 at lo), a link that
// carries more should get std: each tile's picture keeps its texture, which lo smears.
//
// The only measure of a link is the streams themselves. Once they have arrived on time for
// UP_AFTER_MS, the budget steps up a rung. The page then asks for what that rung allows,
// and if the link cannot carry it, frames queue: each tile's arrival time (local time minus
// scene time, lag.ts) grows over the lowest this tile has shown, and tiles fall behind the
// common time. Two such reports in a row step the budget back down. That rung is not tried
// again for HOLD_MS, doubling on each failure up to HOLD_MAX_MS.

import { BANDWIDTH_BUDGET_BPS } from "./bandwidth.ts";

/** The budget's rungs, bits per second. The first is the page's fixed budget. */
export const LINK_RUNGS_BPS = [BANDWIDTH_BUDGET_BPS, 160_000_000, 320_000_000] as const;
/** On time this long before trying the next rung. */
export const UP_AFTER_MS = 10_000;
/** A rung that failed is not tried again for this long, doubling per failure. */
export const HOLD_MS = 60_000;
export const HOLD_MAX_MS = 600_000;
/** Queueing over a tile's lowest arrival time that counts as on time, and as late. */
export const ON_TIME_MS = 60;
export const LATE_MS = 150;
/** Share of drawn tiles behind the common time that counts as on time, and as late. */
export const BEHIND_OK = 0.1;
export const BEHIND_LATE = 0.25;

export interface LinkReport {
  /** Local Unix ms of the report. */
  at: number;
  /** The direct streams are what the page shows; any other mode says nothing about the link. */
  direct: boolean;
  /** Per tile, the p50 of local arrival time minus scene time, ms (lag.ts). */
  arriveMs: Record<string, number>;
  /** Drawn tiles behind the common time over drawn tiles, or null when none is drawn. */
  behindShare: number | null;
}

export class LinkBudget {
  private rung = 0;
  private readonly lowest = new Map<string, number>();
  private onTimeSince: number | null = null;
  private late = 0;
  private holdUntil = new Map<number, number>();
  private holdMs = HOLD_MS;

  get budgetBps(): number {
    return LINK_RUNGS_BPS[this.rung]!;
  }

  /** Feed one drawer report; returns the budget to plan with. */
  observe(r: LinkReport): number {
    if (!r.direct) {
      this.onTimeSince = null;
      this.late = 0;
      return this.budgetBps;
    }
    const queued: number[] = [];
    for (const [id, a] of Object.entries(r.arriveMs)) {
      const low = Math.min(this.lowest.get(id) ?? a, a);
      this.lowest.set(id, low);
      queued.push(a - low);
    }
    if (queued.length === 0) return this.budgetBps;
    queued.sort((a, b) => a - b);
    const q = queued[Math.floor(queued.length / 2)]!;
    const behind = r.behindShare ?? 0;
    if (q > LATE_MS || behind > BEHIND_LATE) {
      this.onTimeSince = null;
      this.late++;
      if (this.late >= 2 && this.rung > 0) {
        this.holdUntil.set(this.rung, r.at + this.holdMs);
        this.holdMs = Math.min(HOLD_MAX_MS, this.holdMs * 2);
        this.rung--;
        this.late = 0;
      }
      return this.budgetBps;
    }
    this.late = 0;
    if (q > ON_TIME_MS || behind > BEHIND_OK) {
      this.onTimeSince = null;
      return this.budgetBps;
    }
    this.onTimeSince ??= r.at;
    const next = this.rung + 1;
    if (next < LINK_RUNGS_BPS.length && r.at - this.onTimeSince >= UP_AFTER_MS && r.at >= (this.holdUntil.get(next) ?? 0)) {
      this.rung = next;
      this.onTimeSince = r.at;
    }
    return this.budgetBps;
  }
}
