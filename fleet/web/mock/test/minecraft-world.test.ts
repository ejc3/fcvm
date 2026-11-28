import { describe, expect, it } from "vitest";
import { MATERIALS } from "../../core/minecraft.ts";
import { sceneOf } from "../../core/minecraft3d.ts";
import { goldLayers, goldWorld, MC_REGION, MC_SIDE } from "../minecraft.ts";

const M = (name: (typeof MATERIALS)[number]) => MATERIALS.indexOf(name);

describe("the mock's world", () => {
  const { cells, ground } = goldLayers();
  const n = MC_SIDE * MC_SIDE;
  const share = (m: number) => Array.from({ length: n }, (_, i) => cells[i * 2]).filter((x) => x === m).length / n;

  it("is shaped as the real one is: a forest of wide canopies, a lake, plants standing on the ground", () => {
    // the real world's map was 45% leaves and 21% water (docs/fleet/minecraft.md)
    expect(share(M("leaves"))).toBeGreaterThan(0.25);
    expect(share(M("leaves"))).toBeLessThan(0.5);
    expect(share(M("water"))).toBeGreaterThan(0.1);
    expect(share(M("water"))).toBeLessThan(0.25);
    let wide = 0;
    for (let i = 0; i < n; i++) {
      const m = cells[i * 2]!;
      const top = cells[i * 2 + 1]!;
      const under = ground[i * 2 + 1]!;
      if (m === M("leaves")) {
        expect(top - under).toBeGreaterThanOrEqual(4);
        if (i % MC_SIDE > 0 && cells[(i - 1) * 2] === m) wide++;
      } else if (m === M("plant") || m === M("flower")) expect(top - under).toBe(1);
      else if (m === M("water")) expect(under).toBeLessThan(top);
      else expect([ground[i * 2], under]).toEqual([m, top]);
    }
    expect(wide).toBeGreaterThan(1000);
    expect([...goldWorld()]).toEqual([...cells]);
  });

  it("leaves a clearing where the bots spawn", () => {
    for (let wz = 0; wz <= 6; wz++) for (let wx = 0; wx <= 10; wx++) expect(cells[((wz - MC_REGION.z0) * MC_SIDE + wx - MC_REGION.x0) * 2]).not.toBe(M("leaves"));
  });

  it("draws: with its ground the scene stands on what the mock holds, and without it on a guess close to it", () => {
    const world = { region: MC_REGION, cells, seq: 1 };
    const known = sceneOf(world, null, { ground });
    const guessed = sceneOf(world, null);
    let off = 0;
    let leaves = 0;
    for (let i = 0; i < n; i++) {
      if (cells[i * 2] !== M("leaves")) continue;
      leaves++;
      expect(known.ground[i]).toBe(ground[i * 2 + 1]);
      if (Math.abs(guessed.ground[i]! - ground[i * 2 + 1]!) > 3) off++;
    }
    // the guess under a canopy is within 3 blocks of the truth for nearly every column
    expect(off / leaves).toBeLessThan(0.1);
  });
});
