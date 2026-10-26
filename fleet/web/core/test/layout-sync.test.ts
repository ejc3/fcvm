import { describe, expect, it } from "vitest";
import {
  fitContain,
  layoutFocus,
  layoutMosaic,
  MAX_TILE_PX,
  STREAM_HEIGHTS,
  neighbour,
  seamSegments,
  streamHeightFor,
  tileGeometry,
  deliveredGeometry,
  tileAt,
  frameFitsBox,
} from "../layout.ts";
import { acquireFrozen, chooseCommonT, type SyncChoice } from "../sync.ts";
import { ENC_BPP, fitBandwidth, MAX_STREAM_BPS, MIN_STREAM_BPS, streamBitrate } from "../bandwidth.ts";
import { addDrag, cameraAt, emptyDrag, lightPosition, poseAt, projectPoint, takeDrag } from "../scene.ts";
import { fleet } from "./fixtures.ts";

describe("layout", () => {
  it("fits the image inside the area, centred", () => {
    expect(fitContain({ x: 0, y: 0, w: 1600, h: 1000 }, 16 / 9)).toEqual({ x: 0, y: 50, w: 1600, h: 900 });
    expect(fitContain({ x: 0, y: 0, w: 1000, h: 300 }, 2)).toEqual({ x: 200, y: 0, w: 600, h: 300 });
  });

  it.each([1, 1.25, 1.5, 2, 3])("tiles share edges exactly at dpr %s: no gap, no overlap", (dpr) => {
    const st = fleet(5, 10); // 100 tiles
    const L = layoutMosaic({ x: 0.3, y: 7.1, w: 1333.7, h: 777.7 }, st.tiles, dpr);
    // Every device pixel of the image belongs to exactly one tile.
    const W = Math.round(L.image.w * dpr);
    const H = Math.round(L.image.h * dpr);
    const owners = new Uint16Array(W * H);
    for (const p of L.tiles) {
      const x0 = Math.round((p.box.x - L.image.x) * dpr);
      const y0 = Math.round((p.box.y - L.image.y) * dpr);
      const x1 = Math.round((p.box.x + p.box.w - L.image.x) * dpr);
      const y1 = Math.round((p.box.y + p.box.h - L.image.y) * dpr);
      // Edges land on device pixels.
      expect(Math.abs(p.box.x * dpr - Math.round(p.box.x * dpr))).toBeLessThan(1e-6);
      for (let y = y0; y < y1; y++) for (let x = x0; x < x1; x++) owners[y * W + x]! += 1;
    }
    let bad = 0;
    for (const o of owners) if (o !== 1) bad++;
    expect(bad).toBe(0);
  });

  it("rounds the stream height up to the set and caps it", () => {
    expect(streamHeightFor(70)).toBe(72);
    expect(streamHeightFor(72.2)).toBe(72);
    expect(streamHeightFor(181)).toBe(216);
    expect(streamHeightFor(5000)).toBe(1080);
  });

  it("asks a tile for its on-screen device height", () => {
    const st = fleet(2, 4); // 8 per half, tiles 1/4 of the image tall
    const L = layoutMosaic({ x: 0, y: 0, w: 1920, h: 1080 }, st.tiles, 1);
    expect(L.tiles[0]!.streamHeight).toBe(270); // 270 css px * 1
  });

  it("never asks one tile for more than MAX_TILE_PX, so no viewer drives a renderer under one sample per pixel", () => {
    // 8 per half, each tile a quarter of the image wide and tall: 16:9 tiles.
    const st = fleet(2, 4);
    // A 1920 x 1080 window on a 2x screen covers 540 device pixels per tile: 960 x 540 = 518,400 px.
    const L = layoutMosaic({ x: 0, y: 0, w: 1920, h: 1080 }, st.tiles, 2);
    for (const p of L.tiles) {
      const t = st.tiles.find((x) => x.id === p.id)!;
      const g = tileGeometry(t.rect, 16 / 9, p.streamHeight);
      expect(g.width * g.height, p.id).toBeLessThanOrEqual(MAX_TILE_PX);
      expect(STREAM_HEIGHTS as readonly number[]).toContain(p.streamHeight);
    }
    expect(L.tiles[0]!.streamHeight).toBe(432);
    // One tile alone on a 4K screen: the cap, not 1080.
    const one = { ...st.tiles[0]!, rect: { x: 0, y: 0, w: 1, h: 1 } };
    const big = layoutMosaic({ x: 0, y: 0, w: 1920, h: 1080 }, [one], 2);
    const g = tileGeometry(one.rect, 16 / 9, big.tiles[0]!.streamHeight);
    expect(g.width * g.height).toBeLessThanOrEqual(MAX_TILE_PX);
    // A tall narrow tile may be taller: the cap is on pixels, not height.
    const tall = layoutFocus({ x: 0, y: 0, w: 1920, h: 1080 }, { ...one, rect: { x: 0, y: 0, w: 1 / 16, h: 1 } }, 2);
    expect(tall.streamHeight).toBe(1080);
  });

  it("follows the renderer's tile geometry, including the encoder's minimum", () => {
    expect(tileGeometry({ x: 0.5, y: 0, w: 0.25, h: 0.5 }, 16 / 9, 540)).toEqual({ width: 480, height: 540, fullWidth: 1920, fullHeight: 1080, x0: 960, y0: 0 });
    // fleet/PROTOCOL.md: a 1/16 x 1/9 rect of a 16:9 image asked at h=136 streams at 146 x 146.
    const g = tileGeometry({ x: 0, y: 0, w: 1 / 16, h: 1 / 9 }, 16 / 9, 136);
    expect([g.width, g.height]).toEqual([146, 146]);
  });

  it("finds the tile under a point and its neighbours", () => {
    const st = fleet(2, 2);
    const t = tileAt(st.tiles, 0.3, 0.2)!;
    expect(t.id).toBe("fcvm:2");
    expect(neighbour(st.tiles, t, "right")!.id).toBe("ec2:1");
    expect(neighbour(st.tiles, t, "down")!.id).toBe("fcvm:4");
    expect(neighbour(st.tiles, t, "up")).toBeNull();
  });

  it("lists every internal boundary once", () => {
    const st = fleet(2, 2); // 4 columns, 2 rows: 3 vertical lines x 2 rows + 1 horizontal x 4 columns
    const L = layoutMosaic({ x: 0, y: 0, w: 1600, h: 900 }, st.tiles, 1);
    const segs = seamSegments(L.tiles, L.image, 1);
    expect(segs.filter((s) => s.x0 === s.x1)).toHaveLength(6);
    expect(segs.filter((s) => s.y0 === s.y1)).toHaveLength(4);
    expect(segs.every((s) => s.x0 % 1 === 0.5 || s.y0 % 1 === 0.5)).toBe(true);
  });
});

describe("one common t", () => {
  const b = (id: string, ts: number[]) => ({ id, ts });

  it("shows the latest t every tile holds", () => {
    const c = chooseCommonT([b("a", [1, 1.05, 1.1]), b("b", [1, 1.05])], null);
    expect(c.T).toBe(1.05);
    expect(c.show.get("a")).toEqual({ kind: "sync", t: 1.05 });
    expect(c.show.get("b")).toEqual({ kind: "sync", t: 1.05 });
  });

  it("steps over a frame one tile skipped", () => {
    const c = chooseCommonT([b("a", [1, 1.1]), b("b", [1, 1.05, 1.1])], null);
    expect(c.T).toBe(1.1);
  });

  it("dims a tile that fell behind instead of holding the image", () => {
    const c = chooseCommonT([b("a", [2, 2.05, 2.1]), b("b", [2, 2.05, 2.1]), b("slow", [1.0])], null, 0.35);
    expect(c.T).toBe(2.1);
    expect(c.show.get("slow")).toEqual({ kind: "behind", t: 1.0 });
  });

  it("keeps a tile that just joined dark until the image reaches its first frame", () => {
    const c = chooseCommonT([b("a", [3, 3.05]), b("b", [3, 3.05]), b("new", [3.1])], null);
    expect(c.T).toBe(3.05);
    expect(c.show.get("new")).toEqual({ kind: "waiting" });
  });

  it("never goes back in time", () => {
    const c = chooseCommonT([b("a", [4, 4.05])], 4.1);
    expect(c.T).toBe(4.1);
    expect(c.show.get("a")).toEqual({ kind: "behind", t: 4.05 });
  });

  it("never draws two different times undimmed", () => {
    // A brute check over many random-looking buffers (deterministic).
    let seed = 7;
    const rnd = () => ((seed = (seed * 1103515245 + 12345) % 2 ** 31) / 2 ** 31);
    for (let trial = 0; trial < 500; trial++) {
      const bufs = Array.from({ length: 6 }, (_, i) => {
        const start = Math.floor(rnd() * 8);
        const ts: number[] = [];
        for (let k = start; k < start + 1 + Math.floor(rnd() * 6); k++) if (rnd() > 0.15) ts.push(k * 0.05);
        return b(`t${i}`, ts);
      });
      const c = chooseCommonT(bufs, null);
      const synced = [...c.show.values()].filter((s) => s.kind === "sync").map((s) => (s as { t: number }).t);
      expect(new Set(synced).size).toBeLessThanOrEqual(1);
    }
  });
});

describe("camera", () => {
  it("interpolates numeric fields between keys and holds at the ends", () => {
    const keys = [{ t: 0, x: 0, zoom: 1 }, { t: 2, x: 1, zoom: 2 }];
    expect(cameraAt(keys, 1)).toEqual({ x: 0.5, zoom: 1.5 });
    expect(cameraAt(keys, -1)).toEqual({ x: 0, zoom: 1 });
    expect(cameraAt(keys, 9)).toEqual({ x: 1, zoom: 2 });
  });

  it("coalesces drag deltas into one intent per interval", () => {
    let acc = addDrag(emptyDrag(), 0.01, 0);
    let r = takeDrag(acc, 1000);
    expect(r.args).toEqual({ drag: { yaw_deg: -0.9, pitch_deg: 0, dive: 0 } });
    acc = addDrag(addDrag(r.acc, 0.02, 0.01), 0.03, 0);
    r = takeDrag(acc, 1020);
    expect(r.args).toBeNull();
    r = takeDrag(r.acc, 1050);
    expect(r.args).toEqual({ drag: { yaw_deg: -4.5, pitch_deg: 0.5, dive: 0 } });
  });
});

describe("review findings, pinned", () => {
  const b = (id: string, ts: number[]) => ({ id, ts });

  it("does not leave a keeping-up tile black behind a slow one whose frames it no longer holds", () => {
    const fast = [0.75, 0.8, 0.85, 0.9, 0.95, 1];
    const slow = [0.45, 0.5, 0.55, 0.6, 0.65, 0.7];
    const c = chooseCommonT([b("fast", fast), b("slow", slow)], null);
    expect(c.show.get("fast")).toEqual({ kind: "sync", t: 1 });
    expect(c.show.get("slow")).toEqual({ kind: "behind", t: 0.7 });
  });

  it("does not depend on the order of the tiles", () => {
    const tiles = [b("new", [3.15]), b("a", [3, 3.05]), b("b", [3, 3.1])];
    for (const order of [tiles, [tiles[1]!, tiles[2]!, tiles[0]!], [tiles[2]!, tiles[0]!, tiles[1]!]]) {
      const c = chooseCommonT(order, null);
      expect(c.T).toBe(3);
      expect(c.show.get("a")).toEqual({ kind: "sync", t: 3 });
      expect(c.show.get("b")).toEqual({ kind: "sync", t: 3 });
    }
  });

  it("leaves no gap between neighbours with seven columns a half", () => {
    const st = fleet(7, 7);
    const L = layoutMosaic({ x: 0, y: 0, w: 707, h: 1080 }, st.tiles, 1);
    for (const a of L.tiles) {
      const right = a.box.x + a.box.w;
      const next = L.tiles.find((o) => Math.abs(o.box.y - a.box.y) < 0.01 && o.box.x > a.box.x && o.box.x - right < 1.5);
      if (next) expect(next.box.x, `${a.id} | ${next.id}`).toBe(right);
    }
  });
});

describe("the renderer's camera", () => {
  const keys = [
    { t: 0, pos: [-16, 0.3, -19], look: [-42, 1.3, -72], fov_deg: 60 },
    { t: 10, pos: [-20, 0.5, -25], look: [-42, 1.3, -72], fov_deg: 50 },
  ];

  it("interpolates pose between keys and holds outside them", () => {
    expect(poseAt(keys, 5)).toEqual({ pos: [-18, 0.4, -22], look: [-42, 1.3, -72], fovDeg: 55 });
    expect(poseAt(keys, 99)!.pos).toEqual([-20, 0.5, -25]);
    expect(poseAt([{ t: 0 }], 0)).toBeNull();
  });

  it("places a dropped light on the clicked point, 2 km ahead", () => {
    for (const [u, v] of [[0.5, 0.5], [0.1, 0.8], [0.93, 0.07]] as const) {
      const w = lightPosition(keys, 3, u, v, 16 / 9)!;
      const p = poseAt(keys, 3)!;
      expect(Math.hypot(w[0] - p.pos[0], w[1] - p.pos[1], w[2] - p.pos[2])).toBeCloseTo(2, 5);
      const back = projectPoint(p, w, 16 / 9)!;
      expect(back.u).toBeCloseTo(u, 5);
      expect(back.v).toBeCloseTo(v, 5);
    }
    // Up on the image is up in the world.
    expect(lightPosition(keys, 3, 0.5, 0.1, 16 / 9)![1]).toBeGreaterThan(lightPosition(keys, 3, 0.5, 0.9, 16 / 9)![1]);
  });
});

describe("stream overlap and bandwidth", () => {
  it("delivers the tile plus up to 16 px inside the image, as the renderer's example", () => {
    const g = tileGeometry({ x: 0.25, y: 0, w: 0.25, h: 0.5 }, 16 / 9, 540);
    expect(g).toMatchObject({ width: 480, height: 540, fullWidth: 1920, fullHeight: 1080, x0: 480, y0: 0 });
    expect(deliveredGeometry(g)).toEqual({ width: 512, height: 556, crop: { left: 16, top: 0, right: 16, bottom: 16 } });
    // A corner tile only overlaps inward; near an edge the overlap is what is there.
    expect(deliveredGeometry({ width: 100, height: 50, fullWidth: 1000, fullHeight: 500, x0: 0, y0: 0 }).crop).toEqual({ left: 0, top: 0, right: 16, bottom: 16 });
    expect(deliveredGeometry({ width: 100, height: 50, fullWidth: 1000, fullHeight: 500, x0: 7, y0: 0 })).toEqual({ width: 122, height: 66, crop: { left: 7, top: 0, right: 15, bottom: 16 } });
  });

  it("prices streams as fleet/PROTOCOL.md's quality table: bits per pixel, floor and cap per q", () => {
    // 0.25 / 0.8 / 4 bits per delivered pixel at 30 fps.
    const r = { x: 0.25, y: 0, w: 0.25, h: 0.5 };
    const px = 512 * 556; // deliveredGeometry at h=540, as the renderer's example
    expect(streamBitrate(r, 16 / 9, 540, "lo")).toBeCloseTo(px * 0.25 * 30, 0);
    expect(streamBitrate(r, 16 / 9, 540, "std")).toBeCloseTo(px * 0.8 * 30, 0);
    expect(streamBitrate(r, 16 / 9, 540, "hi")).toBeCloseTo(px * 4 * 30, 0);
    // Floors: 0.15, 0.6 and 2 Mbit/s.
    expect(MIN_STREAM_BPS).toEqual({ hi: 2_000_000, std: 600_000, lo: 150_000 });
    // Caps: 20 Mbit/s for std and lo, 160 for hi.
    const whole = { x: 0, y: 0, w: 1, h: 1 };
    expect(streamBitrate(whole, 16 / 9, 1080, "std")).toBe(20_000_000);
    expect(streamBitrate(whole, 16 / 9, 1080, "lo")).toBeLessThanOrEqual(20_000_000);
    expect(streamBitrate(whole, 16 / 9, 2160, "hi")).toBe(160_000_000);
    expect(ENC_BPP).toEqual({ hi: 4, std: 0.8, lo: 0.25 });
    expect(MAX_STREAM_BPS).toEqual({ hi: 160_000_000, std: 20_000_000, lo: 20_000_000 });
  });

  it("moves every tile together: all to q=lo, then all down a height, keeping the opened one", () => {
    const rect = (x: number, w: number) => ({ x, y: 0, w, h: 1 / 9 });
    const asks = Array.from({ length: 16 }, (_, i) => ({ id: `t${i}`, rect: rect(i / 32, i < 8 ? 1 / 32 : 1 / 16), height: 540, keep: i === 0 }));
    const before = asks.reduce((a, x) => a + streamBitrate(x.rect, 16 / 9, x.height), 0);
    const mild = fitBandwidth(asks, 16 / 9, before * 0.85);
    expect(mild.bps).toBeLessThanOrEqual(before * 0.85);
    // No patchwork: every tile but the opened one is at lo, none at std beside it, and no height changed.
    expect(asks.slice(1).every((a) => mild.quality.get(a.id) === "lo")).toBe(true);
    expect(mild.quality.get("t0")).toBe("std");
    expect([...mild.heights.values()].every((h) => h === 540)).toBe(true);
    // A tight budget: everyone but the opened tile at lo, then heights fall.
    const tight = fitBandwidth(asks, 16 / 9, 30e6);
    expect(tight.bps).toBeLessThanOrEqual(30e6);
    expect(tight.quality.get("t0")).toBe("std");
    expect(tight.heights.get("t0")).toBe(540);
    expect(asks.slice(1).every((a) => tight.quality.get(a.id) === "lo")).toBe(true);
    // Every other tile stepped down together, to one height.
    expect(new Set(asks.slice(1).map((a) => tight.heights.get(a.id)!)).size).toBe(1);
    expect(tight.heights.get("t1")!).toBeLessThan(540);
    // Under the budget nothing changes.
    const easy = fitBandwidth(asks, 16 / 9, 1e12);
    expect(easy.heights.get("t5")).toBe(540);
    expect(easy.quality.get("t5")).toBe("std");
  });

  it("stops at the smallest height, over budget if it must", () => {
    const asks = Array.from({ length: 100 }, (_, i) => ({ id: `t${i}`, rect: { x: 0, y: 0, w: 0.05, h: 0.1 }, height: 90, keep: false }));
    const fit = fitBandwidth(asks, 16 / 9, 10e6);
    expect([...fit.heights.values()].every((h) => h === 72)).toBe(true);
    expect(fit.bps).toBeGreaterThan(10e6);
    expect(fit.bps).toBeGreaterThanOrEqual(100 * MIN_STREAM_BPS.lo);
  });
});

describe("holding the image for a mode switch", () => {
  const b = (id: string, ts: number[]) => ({ id, ts });
  it("does not choose frames after maxT, and never goes back", () => {
    const c = chooseCommonT([b("a", [1, 1.1, 1.2, 1.3]), b("b", [1, 1.1, 1.2, 1.3])], 1.0, 0.35, 1.1);
    expect(c.T).toBe(1.1);
    expect(c.show.get("a")).toEqual({ kind: "sync", t: 1.1 });
    const held = chooseCommonT([b("a", [1.1, 1.2])], 1.2, 0.35, 1.0);
    expect(held.T).toBe(1.2);
  });
});

describe("whether a frame was rendered for the place a plan draws it", () => {
  const image = { x: 0, y: 0, w: 1920, h: 1080 };
  // A 3 x 2 tiling of one half, as fractions of the image.
  const tile = (c: number, r: number) => ({ x: c / 6, y: r / 2, w: 1 / 6, h: 1 / 2 });
  const boxOf = (r: { x: number; y: number; w: number; h: number }) => ({ x: r.x * image.w, y: r.y * image.h, w: r.w * image.w, h: r.h * image.h });

  it("accepts the renderer's frame at the smallest stream height, however it rounds", () => {
    // Asked for height 72, the renderer raises it to its minimum width: 146 x 244 on an 868 x 488 grid,
    // which draws 3 screen pixels wider than the planned 320.
    for (const dpr of [1, 2]) {
      for (let c = 0; c < 3; c++) {
        for (let r = 0; r < 2; r++) {
          const g = tileGeometry(tile(c, r), 16 / 9, 72);
          expect(g).toMatchObject({ width: 146, height: 244, fullWidth: 868, fullHeight: 488 });
          expect(frameFitsBox(image, g, { sw: g.width, sh: g.height }, boxOf(tile(c, r)), dpr), `dpr ${dpr} tile ${c},${r}`).toBe(true);
        }
      }
    }
  });

  it("still refuses a frame rendered for another tiling", () => {
    const g = tileGeometry(tile(1, 0), 16 / 9, 72);
    // The same stream against the neighbouring column's place, and against a tile half the height.
    expect(frameFitsBox(image, g, { sw: g.width, sh: g.height }, boxOf(tile(2, 0)), 1)).toBe(false);
    expect(frameFitsBox(image, g, { sw: g.width, sh: g.height }, { ...boxOf(tile(1, 0)), h: 270 }, 1)).toBe(false);
  });
});

describe("a frozen picture takes a frame for a tile that had none", () => {
  const b = (id: string, ts: number[]) => ({ id, ts });
  const frozen: SyncChoice = {
    T: 2,
    show: new Map([
      ["a", { kind: "sync", t: 2 }],
      ["b", { kind: "empty" }],
      ["c", { kind: "waiting" }],
    ]),
  };

  it("takes the newest frame at or before T for a tile that shows none, and leaves the others alone", () => {
    const got = acquireFrozen(frozen, [b("a", [2, 2.1, 2.2]), b("b", [1.8, 1.9, 2.5]), b("c", [])]);
    expect(got).not.toBeNull();
    expect(got!.T).toBe(2);
    expect(got!.show.get("a")).toEqual({ kind: "sync", t: 2 });
    expect(got!.show.get("b")).toEqual({ kind: "behind", t: 1.9 });
    expect(got!.show.get("c")).toEqual({ kind: "waiting" });
  });

  it("shows a frame at T as in sync", () => {
    expect(acquireFrozen(frozen, [b("b", [1.9, 2])])!.show.get("b")).toEqual({ kind: "sync", t: 2 });
  });

  it("takes the oldest frame after T, dimmed, when a reopened stream has nothing older", () => {
    expect(acquireFrozen(frozen, [b("b", [3.1, 3.2])])!.show.get("b")).toEqual({ kind: "behind", t: 3.1 });
  });

  it("changes nothing when no tile that shows none has a frame, or every tile shows one", () => {
    expect(acquireFrozen(frozen, [b("a", [2, 2.1]), b("b", []), b("c", [])])).toBeNull();
    expect(acquireFrozen({ T: 2, show: new Map([["a", { kind: "behind", t: 1.9 }]]) }, [b("a", [2.5])])).toBeNull();
  });

  it("does not add a tile the choice never named", () => {
    expect(acquireFrozen(frozen, [b("d", [2])])).toBeNull();
  });
});
