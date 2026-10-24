// Which way this viewer should receive the image: one stream per tile
// (direct) or a few joined regions (aggregated). The page suggests one from
// what it measures; the viewer can override it at any time.

import * as fmt from "./format.ts";
import type { StreamMode } from "./store.ts";

export interface ModeEvidence {
  /** Aggregators in the snapshot. */
  aggregators: number;
  /**
   * The browser's own downlink estimate, Mbit/s, or null. Chrome caps it at
   * 10, so 10 or more means "at least that".
   */
  downlinkMbps: number | null;
  /** What the direct streams would cost at the renderer's rate, bits per second. */
  directBps: number;
  /**
   * In direct mode, the share of drawn tiles that were behind the common t over
   * the last few seconds (streams arriving late), or null when not measured.
   */
  lateShare: number | null;
}

/** Direct streams arriving this late, on this share of tiles, mean the link cannot carry them. */
export const LATE_SHARE_LIMIT = 0.1;
const DOWNLINK_CAP_MBPS = 10;

export function suggestMode(e: ModeEvidence): { mode: StreamMode; reason: string } {
  if (e.aggregators === 0) return { mode: "direct", reason: "this fleet runs no aggregators" };
  const need = fmt.mbps(e.directBps);
  if (e.downlinkMbps !== null && e.downlinkMbps < DOWNLINK_CAP_MBPS && e.downlinkMbps * 1e6 < e.directBps) {
    return { mode: "aggregated", reason: `your connection reports ${e.downlinkMbps} Mbit/s; direct needs about ${need}` };
  }
  if (e.lateShare !== null && e.lateShare > LATE_SHARE_LIMIT) {
    return { mode: "aggregated", reason: `direct streams arrive late on ${Math.round(e.lateShare * 100)}% of tiles` };
  }
  return { mode: "direct", reason: `direct streams fit (about ${need})` };
}
