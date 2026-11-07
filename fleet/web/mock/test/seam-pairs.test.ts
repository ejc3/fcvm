// The e2e seam check judges a tile only along a boundary with SEAM_MIN_PAIRS smooth pairs, and
// which pairs are smooth depends on the picture. These sweeps fix the camera at a stop and step
// t, over the aggregated layout CI failed on (main run 42379130378: fcvm:7, the bottom-left tile,
// had 1 and 0 smooth pairs at t 575.9 with the camera on the star field).
import { describe, expect, it } from "vitest";
import { NUDGE_TARGETS } from "../coordinator.ts";
import { frameCtx, type PatternScene } from "../pattern.ts";
import { SEAM_MIN_PAIRS, SEAM_STOP, smoothPairs, unjudged, type Box } from "../seam-pairs.ts";

const image: Box = { x: 0, y: 30, w: 960, h: 540 };
const fcvm = (k: number): { id: string; box: Box } => ({ id: `fcvm:${k}`, box: { x: ((k - 1) % 2) * 256, y: 30 + Math.floor((k - 1) / 2) * 135, w: 256, h: 135 } });
const ec2 = (k: number): { id: string; box: Box } =>
  k === 7 ? { id: "ec2:7", box: { x: 512, y: 435, w: 448, h: 135 } } : { id: `ec2:${k}`, box: { x: 512 + ((k - 1) % 2) * 224, y: 30 + Math.floor((k - 1) / 2) * 135, w: 224, h: 135 } };
const tiles = [...[1, 2, 3, 4, 5, 6, 7, 8].map(fcvm), ...[1, 2, 3, 4, 5, 6, 7].map(ec2)];

const held = (stop: { x: number; y: number; zoom: number }, tod: number): PatternScene => ({
  // The run's epoch: the clock text the picture draws at t follows from it.
  epoch_unix_ms: 1794060009000,
  image_aspect: 16 / 9,
  camera: [{ t: 0, x: stop.x, y: stop.y, zoom: stop.zoom }] as never,
  time_of_day: tod,
  words: [],
  lights: [],
});

describe("the seam check's judged boundaries", () => {
  it("are measured at one of the directed camera's stops", () => {
    const stop = NUDGE_TARGETS.find((x) => x.id === SEAM_STOP.id)!;
    expect({ id: stop.id, x: stop.x, y: stop.y, zoom: stop.zoom }).toEqual(SEAM_STOP);
  });

  it("leave the bottom-left tile unjudged with the camera on the star field, as on CI", () => {
    const f = frameCtx(575.9, held({ x: 1.2, y: -0.4, zoom: 0.7 }, 0.42));
    const n = smoothPairs(image, tiles, f);
    expect(n["fcvm:5|fcvm:7"]).toBeLessThan(SEAM_MIN_PAIRS);
    expect(n["fcvm:7|fcvm:8"]).toBeLessThan(SEAM_MIN_PAIRS);
    expect(unjudged(image, tiles, f)).toEqual(["fcvm:7"]);
  });

  it("judge every tile at the stop the seam tests hold, at every t and time of day the tests use", () => {
    // Fewest smooth pairs on a tile's best boundary, over t 0 to 1400 in steps of 0.5: 19 at
    // time of day 0.42 (the mock's, which the seam tests run at), 23 at 0.5, 6 at 0.75.
    for (const tod of [0.42, 0.5, 0.75]) {
      for (let t = 0; t <= 600; t += 2) {
        expect(unjudged(image, tiles, frameCtx(t, held(SEAM_STOP, tod))), `t ${t}, time of day ${tod}`).toEqual([]);
      }
    }
  }, 120_000);
});
