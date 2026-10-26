// The mock's picture is what the browser tests check pixels against, so making it
// cheaper to draw must not change one byte of it. These hashes are of renderI420's
// output before the render loop lost its per-pixel allocations (a closure for the
// planet's ring and an array of offsets for each chroma sample).
import { createHash } from "node:crypto";
import { describe, expect, it } from "vitest";
import { deliveredGeometry, tileGeometry, type TileGeometry } from "../../core/layout.ts";
import { frameCtx, renderI420, type PatternScene } from "../pattern.ts";

const scene: PatternScene = {
  epoch_unix_ms: 0,
  image_aspect: 16 / 9,
  camera: [],
  time_of_day: 0.65,
  words: [{ text: "fcvm", pos: { x: -0.2, y: -0.1 } }],
  lights: [{ pos: { x: 0.1, y: 0.05 }, color: [1, 0.8, 0.5] }],
};

/** A tile as the mock delivers it: its rectangle at height h plus the overlap. */
function delivered(rect: { x: number; y: number; w: number; h: number }, h: number): TileGeometry {
  const t = tileGeometry(rect, 16 / 9, h);
  const d = deliveredGeometry(t);
  return { ...t, width: d.width, height: d.height, x0: t.x0 - d.crop.left, y0: t.y0 - d.crop.top };
}

const sha = (b: Uint8Array) => createHash("sha256").update(b).digest("hex").slice(0, 16);

describe("the mock's picture", () => {
  it("is byte for byte what it was before the render loop was made cheaper", () => {
    // Tiles that cross the planet, its ring (front and back), the words, a light and the clock.
    const cases: [string, TileGeometry, number][] = [
      ["centre", delivered({ x: 0.375, y: 0.25, w: 0.25, h: 0.5 }, 270), 12.3],
      ["left", delivered({ x: 0, y: 0.5, w: 0.5, h: 0.5 }, 144), 3.1],
      ["ring", delivered({ x: 0.25, y: 0.4, w: 0.5, h: 0.2 }, 90), 40.05],
    ];
    expect(Object.fromEntries(cases.map(([name, g, t]) => [name, sha(renderI420(g, frameCtx(t, scene)))]))).toEqual(GOLDEN);
  });
});

const GOLDEN = { centre: "78bf5f411c712114", left: "1c5b8d8057a05cd8", ring: "b02e857489531d1e" };
