import { describe, expect, it } from "vitest";
import { HOLD_MS, LINK_RUNGS_BPS, LinkBudget, UP_AFTER_MS } from "../linkbudget.ts";
import { BANDWIDTH_BUDGET_BPS, fitBandwidth } from "../bandwidth.ts";
import { streamHeightFor, tileAspectOf } from "../layout.ts";

const tiles = (arrive: number) => Object.fromEntries(Array.from({ length: 16 }, (_, k) => [`t${k}`, arrive + k]));
const report = (at: number, arrive: number, behindShare = 0, direct = true) => ({ at, direct, arriveMs: tiles(arrive), behindShare });

// Reports every 500 ms, as the drawer sends them; returns the budget after the last.
function run(b: LinkBudget, from: number, to: number, arrive: (at: number) => number, behind = 0): number {
  let bps = b.budgetBps;
  for (let at = from; at <= to; at += 500) bps = b.observe(report(at, arrive(at), behind));
  return bps;
}

describe("LinkBudget", () => {
  it("starts at the page's fixed budget", () => {
    expect(new LinkBudget().budgetBps).toBe(BANDWIDTH_BUDGET_BPS);
  });

  it("steps up a rung after UP_AFTER_MS on time, and on to the top", () => {
    const b = new LinkBudget();
    expect(run(b, 0, UP_AFTER_MS - 500, () => 80)).toBe(LINK_RUNGS_BPS[0]);
    expect(run(b, UP_AFTER_MS, UP_AFTER_MS, () => 80)).toBe(LINK_RUNGS_BPS[1]);
    expect(run(b, UP_AFTER_MS + 500, 2 * UP_AFTER_MS + 500, () => 80)).toBe(LINK_RUNGS_BPS[2]);
    expect(run(b, 2 * UP_AFTER_MS + 1000, 10 * UP_AFTER_MS, () => 80)).toBe(LINK_RUNGS_BPS[2]);
  });

  it("steps back down when frames queue, and holds that rung off", () => {
    const b = new LinkBudget();
    run(b, 0, UP_AFTER_MS, () => 80);
    expect(b.budgetBps).toBe(LINK_RUNGS_BPS[1]);
    // the link cannot carry the higher rung: arrival times grow 400 ms over the lowest
    const t0 = UP_AFTER_MS + 500;
    expect(b.observe(report(t0, 480))).toBe(LINK_RUNGS_BPS[1]);
    expect(b.observe(report(t0 + 500, 480))).toBe(LINK_RUNGS_BPS[0]);
    // on time again, but the failed rung waits HOLD_MS
    expect(run(b, t0 + 1000, t0 + HOLD_MS - 500, () => 80)).toBe(LINK_RUNGS_BPS[0]);
    expect(run(b, t0 + HOLD_MS, t0 + HOLD_MS + 500, () => 80)).toBe(LINK_RUNGS_BPS[1]);
  });

  it("does not step up while tiles fall behind, and steps down when many do", () => {
    const b = new LinkBudget();
    expect(run(b, 0, 3 * UP_AFTER_MS, () => 80, 0.15)).toBe(LINK_RUNGS_BPS[0]);
    run(b, 3 * UP_AFTER_MS + 500, 4 * UP_AFTER_MS + 500, () => 80);
    expect(b.budgetBps).toBe(LINK_RUNGS_BPS[1]);
    expect(run(b, 5 * UP_AFTER_MS, 5 * UP_AFTER_MS + 500, () => 80, 0.5)).toBe(LINK_RUNGS_BPS[0]);
  });

  it("starts a tile's lowest arrival again when its scene clock changes", () => {
    // A renderer streams its default scene before the coordinator's, whose epoch moves
    // every arrival time (local time minus scene time) by the epoch.
    const b = new LinkBudget();
    for (let at = 0; at < 2000; at += 500) b.observe(report(at, 40));
    const epoch = 1_760_000_000_000;
    expect(run(b, 2000, 2000 + UP_AFTER_MS + 500, () => epoch + 80)).toBe(LINK_RUNGS_BPS[1]);
  });

  it("does not count a stretch with no streams reporting as on time, or join late reports across it", () => {
    const b = new LinkBudget();
    const empty = (at: number) => b.observe({ at, direct: true, arriveMs: {}, behindShare: null });
    b.observe(report(0, 80));
    for (let at = 500; at < 60_000; at += 500) empty(at);
    expect(b.observe(report(60_000, 80))).toBe(LINK_RUNGS_BPS[0]);
    expect(run(b, 60_500, 60_000 + UP_AFTER_MS - 500, () => 80)).toBe(LINK_RUNGS_BPS[0]);
    expect(run(b, 60_000 + UP_AFTER_MS, 60_000 + UP_AFTER_MS, () => 80)).toBe(LINK_RUNGS_BPS[1]);
    // one late report, an outage, one late report: not two in a row
    const t = 60_000 + UP_AFTER_MS + 500;
    b.observe(report(t, 480));
    empty(t + 500);
    expect(b.observe(report(t + 1000, 480))).toBe(LINK_RUNGS_BPS[1]);
  });

  it("learns nothing outside direct mode", () => {
    const b = new LinkBudget();
    for (let at = 0; at <= 3 * UP_AFTER_MS; at += 500) b.observe(report(at, 80, 0, false));
    expect(b.budgetBps).toBe(LINK_RUNGS_BPS[0]);
  });

  it("lets the owner's 16-tile wall at 3460 x 1946 stream std on the second rung", () => {
    const aspect = 16 / 9;
    const asks = [];
    for (let k = 0; k < 16; k++) {
      const rect = { x: (k % 4) / 4, y: Math.floor(k / 4) / 4, w: 0.25, h: 0.25 };
      asks.push({ id: `t${k}`, rect, height: streamHeightFor(1946 / 4, tileAspectOf(rect, aspect)), keep: false });
    }
    expect(fitBandwidth(asks, aspect, LINK_RUNGS_BPS[0]).quality.get("t0")).toBe("lo");
    const up = fitBandwidth(asks, aspect, LINK_RUNGS_BPS[1]);
    expect(up.quality.get("t0")).toBe("std");
    expect(up.heights.get("t0")).toBe(432);
  });
});
