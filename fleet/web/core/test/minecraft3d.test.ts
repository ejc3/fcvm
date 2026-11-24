import { describe, expect, it } from "vitest";
import { MATERIALS, type McPlayer, type McWorld } from "../minecraft.ts";
import {
  BOX_CANOPY,
  BOX_FLOATS,
  BOX_TRUNK,
  COLUMN_BYTES,
  FLAG_CANOPY,
  chaseCam,
  countDiff,
  DEFAULT_DAY_TIME,
  figureBoxes,
  FLAG_DIFF,
  FLAG_RECENT,
  FLIGHT_LAND_MS,
  flightBox,
  flightProgress,
  FRAME_BUDGET_MS,
  FrameBudget,
  groundAt,
  labelsOf,
  onScreen,
  pixelScale,
  project,
  sceneOf,
  scenePlayer,
  shotOf,
  SHOT_MS,
  showsAbove,
  skyOf,
  viewProj,
  type McScene,
  type ScenePlayer,
} from "../minecraft3d.ts";

const M = (name: (typeof MATERIALS)[number]) => MATERIALS.indexOf(name);
const SIDE = 16;

/** A flat world of grass at byte 130, with what `put` changes: (x, z) -> [material, height]. */
function world(put: Record<string, [number, number]> = {}, side = SIDE): McWorld {
  const cells = new Uint8Array(side * side * 2);
  for (let i = 0; i < side * side; i++) {
    const at = put[`${i % side},${Math.floor(i / side)}`];
    cells[i * 2] = at?.[0] ?? M("grass");
    cells[i * 2 + 1] = at?.[1] ?? 130;
  }
  return { region: { x0: -8, z0: -8, w: side, h: side }, cells, seq: 1 };
}
/** The instances of `list` as [x, z, material, height, base, flags]. */
const insts = (list: Uint8Array) => Array.from({ length: list.length / COLUMN_BYTES }, (_, k) => {
  const b = list.subarray(k * COLUMN_BYTES, (k + 1) * COLUMN_BYTES);
  return [b[0]! | (b[1]! << 8), b[2]! | (b[3]! << 8), b[4]!, b[5]!, b[6]!, b[7]!];
});
/** The ground column at (x, z): [material, height, base, flags]. */
const col = (s: McScene, x: number, z: number) => insts(s.tops).find((q) => q[0] === x && q[1] === z && (q[5]! & FLAG_CANOPY) === 0)!.slice(2);
const canopies = (s: McScene) => insts(s.tops).filter((q) => (q[5]! & FLAG_CANOPY) !== 0);
const boxes = (s: McScene) => Array.from({ length: s.boxCount }, (_, k) => [...s.boxes.slice(k * BOX_FLOATS, (k + 1) * BOX_FLOATS)]);
const bot = (x: number, z: number, y: number, yaw = 0, over: Partial<ScenePlayer> = {}): ScenePlayer => ({ name: "fcvm_bot1", x, y, z, yaw, bot: true, driven: false, ...over });

describe("a world as columns", () => {
  it("gives every column its material, its height and a base at its lowest neighbour", () => {
    const s = sceneOf(world({ "5,5": [M("stone"), 136], "6,5": [M("stone"), 133] }), null);
    // one top a column; sides only where a column stands above a neighbour or the world's edge
    expect(s.tops.length / COLUMN_BYTES).toBe(SIDE * SIDE);
    expect(insts(s.sides).map((q) => `${q[0]},${q[1]}`).filter((k) => !/^(0|15),|,(0|15)$/.test(k))).toEqual(["5,5", "6,5"]);
    expect(s.water).toHaveLength(0);
    expect(s.step).toBe(1);
    expect(col(s, 5, 5)).toEqual([M("stone"), 136, 130, 0]);
    // the lower stone: one neighbour stands higher, its sides reach the grass
    expect(col(s, 6, 5)).toEqual([M("stone"), 133, 130, 1]);
    expect(col(s, 2, 2)).toEqual([M("grass"), 130, 130, 0]);
  });

  it("draws the world's edge down to a floor below its lowest column", () => {
    const s = sceneOf(world({ "3,3": [M("stone"), 124] }), null);
    expect(s.floor).toBe(114);
    expect(col(s, 0, 7)[2]).toBe(114);
    expect(col(s, 7, 7)[2]).toBe(130);
  });

  it("guesses the ground under leaves from the columns beside them, and stands a trunk under a canopy's top", () => {
    const put: Record<string, [number, number]> = {};
    for (let x = 4; x <= 8; x++) for (let z = 4; z <= 8; z++) put[`${x},${z}`] = [M("leaves"), x === 6 && z === 6 ? 138 : 136];
    const s = sceneOf(world(put), null);
    for (let x = 4; x <= 8; x++) for (let z = 4; z <= 8; z++) expect(col(s, x, z).slice(0, 2), `${x},${z}`).toEqual([M("grass"), 130]);
    const b = boxes(s);
    expect(canopies(s)).toHaveLength(25);
    const trunks = b.filter((q) => q[10] === BOX_TRUNK);
    // one under the top, and one for the flat stretch round it (its first column)
    expect(trunks.map((q) => q.slice(0, 3))).toEqual([[4.5, 130, 4.5], [6.5, 130, 6.5]]);
    // the canopy hangs above the ground, not down to it, and only its rim and its top show their sides
    expect(Math.min(...canopies(s).map((q) => q[4]!))).toBeGreaterThanOrEqual(132);
    const sided = insts(s.sides).filter((q) => (q[5]! & FLAG_CANOPY) !== 0);
    expect(sided).toHaveLength(17);
    expect(sided.find((q) => q[0] === 6 && q[1] === 6)!.slice(3, 5)).toEqual([138, 136]);
    expect(sided.some((q) => q[0] === 5 && q[1] === 5)).toBe(false);
  });

  it("makes a lone column of leaves a small tree: a trunk and a crown wider than a block", () => {
    const tree = sceneOf(world({ "7,7": [M("leaves"), 135] }), null);
    expect(canopies(tree)).toHaveLength(0);
    const b = boxes(tree);
    expect(b.filter((q) => q[10] === BOX_TRUNK)).toHaveLength(1);
    expect(Math.max(...b.filter((q) => q[10] === BOX_CANOPY).map((q) => q[3]!))).toBeGreaterThan(1.5);
  });

  it("deepens a lake from its shore when the data has no bed, and a plant is a small box on the block under it", () => {
    const put: Record<string, [number, number]> = { "1,1": [M("flower"), 131] };
    for (let x = 3; x <= 12; x++) for (let z = 3; z <= 12; z++) put[`${x},${z}`] = [M("water"), 126];
    const s = sceneOf(world(put), null);
    const depth = (x: number, z: number) => (col(s, x, z)[3]! >> 2) & 7;
    expect([depth(3, 3), depth(4, 4), depth(5, 5), depth(7, 7), depth(2, 2)]).toEqual([1, 2, 3, 4, 0]);
    expect(col(s, 7, 7).slice(0, 2)).toEqual([M("water"), 126]);
    expect(s.water.length / COLUMN_BYTES).toBe(100);
    // the shore's side reaches down to the bed beside it
    expect(col(s, 2, 5)[2]).toBe(125);
    expect(col(s, 1, 1).slice(0, 2)).toEqual([M("grass"), 130]);
    const flower = boxes(s).find((q) => q[10] === 0)!;
    expect(flower[1]).toBe(130);
    expect(flower[4]).toBeLessThan(1);
  });

  it("uses the tile's ground where it sends one: a lake's true depth and the floor under a canopy", () => {
    const put: Record<string, [number, number]> = { "9,9": [M("leaves"), 140], "9,10": [M("leaves"), 140] };
    for (let x = 2; x <= 5; x++) put[`${x},2`] = [M("water"), 126];
    const w = world(put);
    const ground = w.cells.slice();
    for (let x = 2; x <= 5; x++) ground.set([M("sand"), 126 - x], (2 * SIDE + x) * 2);
    ground.set([M("stone"), 133], (9 * SIDE + 9) * 2);
    ground.set([M("dirt"), 131], (10 * SIDE + 9) * 2);
    const s = sceneOf(w, null, { ground });
    expect([2, 3, 4, 5].map((x) => (col(s, x, 2)[3]! >> 2) & 7)).toEqual([2, 3, 4, 5]);
    expect(col(s, 9, 9).slice(0, 2)).toEqual([M("stone"), 133]);
    expect(col(s, 9, 10).slice(0, 2)).toEqual([M("dirt"), 131]);
    // a ground of another size is not this map's: the guess stands
    expect(col(sceneOf(w, null, { ground: ground.slice(2) }), 9, 9).slice(0, 2)).toEqual([M("grass"), 130]);
  });

  it("draws a material the page does not know as unknown, and a column not read yet at the floor", () => {
    const remap = new Uint8Array(256);
    remap[2] = M("stone");
    const s = sceneOf(world({ "0,0": [0, 0] }), remap);
    expect(col(s, 4, 4)[0]).toBe(M("stone"));
    expect(col(s, 0, 0).slice(0, 3)).toEqual([0, s.floor, s.floor]);
  });

  it("flags the columns that differ from the fork's source and the ones that just changed", () => {
    const src = world();
    const copy = world({ "4,4": [M("red"), 133], "5,4": [M("blue"), 131] });
    const s = sceneOf(copy, null, { diffFrom: src.cells, recent: new Set([4 * SIDE + 5]) });
    expect(s.diff).toBe(2);
    expect(countDiff(src.cells, copy.cells)).toBe(2);
    expect(col(s, 4, 4)[3]! & FLAG_DIFF).toBe(FLAG_DIFF);
    expect(col(s, 5, 4)[3]! & (FLAG_DIFF | FLAG_RECENT)).toBe(FLAG_DIFF | FLAG_RECENT);
    expect(col(s, 6, 4)[3]! & (FLAG_DIFF | FLAG_RECENT)).toBe(0);
    // worlds of different sizes are not compared
    expect(countDiff(src.cells, world({}, 8).cells)).toBe(0);
    expect(sceneOf(copy, null, { diffFrom: world({}, 8).cells }).diff).toBe(0);
  });

  it("draws one column for a square of blocks on a slow device, and keeps what the bots built", () => {
    const put: Record<string, [number, number]> = { "5,7": [M("red"), 140], "4,6": [M("stone"), 150] };
    for (let x = 8; x < 16; x++) for (let z = 8; z < 16; z++) put[`${x},${z}`] = [M("leaves"), 137];
    const w = world(put);
    const full = sceneOf(w, null);
    const half = sceneOf(w, null, { step: 2 });
    expect(half.step).toBe(2);
    expect(half.tops.length / COLUMN_BYTES).toBe(64 + 16);
    expect(full.tops.length / COLUMN_BYTES).toBe(256 + 64);
    // the square holding the tower shows the tower, not its first column (the stone)
    expect(col(half, 4, 6).slice(0, 2)).toEqual([M("red"), 140]);
    expect(col(half, 0, 0).slice(0, 2)).toEqual([M("grass"), 130]);
    expect(canopies(half).every((q) => q[0]! % 2 === 0 && q[1]! % 2 === 0)).toBe(true);
    // the ground a figure stands on keeps every column
    expect(groundAt(half, 4.5, 6.5)).toBe(150);
    expect(sceneOf(w, null, { step: 4 }).tops.length / COLUMN_BYTES).toBe(16 + 4);
    expect(sceneOf(w, null, { step: 3 }).step).toBe(1);
  });

  it("names where the bots built, the most built-up place first", () => {
    const s = sceneOf(world({ "2,2": [M("red"), 134], "3,2": [M("red"), 133], "2,3": [M("blue"), 132], "13,13": [M("yellow"), 133] }, 32), null);
    expect(s.sites).toHaveLength(2);
    expect([2 * 32 + 2, 2 * 32 + 3, 3 * 32 + 2]).toContain(s.sites[0]);
    expect(s.sites[1]).toBe(13 * 32 + 13);
    expect(sceneOf(world(), null).sites).toEqual([]);
  });
});

describe("figures", () => {
  it("puts a player in scene coordinates: feet on the column it stands on", () => {
    const p: McPlayer = { name: "fcvm_bot2", x: -3.5, y: 67, z: 2.25, yaw: 90, bot: true, driven: false };
    const q = scenePlayer({ x0: -8, z0: -8, w: SIDE, h: SIDE }, p);
    expect([q.x, q.y, q.z]).toEqual([4.5, 130, 10.25]);
    expect(q.yaw).toBeCloseTo(Math.PI / 2);
  });

  it("faces its yaw: 0 is +z, a quarter turn is -x", () => {
    const band = (yaw: number) => {
      const out: number[] = [];
      figureBoxes(out, bot(5, 5, 130, yaw), [1, 0, 0], 1, { glow: 0, beacon: false });
      expect(out.length).toBe(7 * BOX_FLOATS);
      return out.slice(6 * BOX_FLOATS, 6 * BOX_FLOATS + 3);
    };
    expect(band(0)[2]).toBeGreaterThan(5.1);
    expect(band(0)[0]).toBeCloseTo(5);
    expect(band(Math.PI / 2)[0]).toBeLessThan(4.9);
    expect(band(Math.PI / 2)[2]).toBeCloseTo(5);
  });

  it("adds a beacon for the far shots and a plate under a driven bot", () => {
    const out: number[] = [];
    figureBoxes(out, bot(5, 5, 130, 0, { driven: true }), [1, 0, 0], 2, { glow: 0, beacon: true });
    expect(out.length).toBe(9 * BOX_FLOATS);
  });

  it("knows a figure under the lake or the ground from one that shows", () => {
    const put: Record<string, [number, number]> = {};
    for (let x = 3; x <= 12; x++) for (let z = 3; z <= 12; z++) put[`${x},${z}`] = [M("water"), 126];
    const s = sceneOf(world(put), null);
    expect(showsAbove(s, bot(1.5, 1.5, 130))).toBe(true);
    expect(showsAbove(s, bot(7.5, 7.5, 125))).toBe(true); // swimming: its head is out
    expect(showsAbove(s, bot(7.5, 7.5, 120))).toBe(false);
    expect(showsAbove(s, bot(-1, 3, 130))).toBe(false);
  });
});

describe("the camera", () => {
  const hill: Record<string, [number, number]> = {};
  for (let x = 20; x < 30; x++) for (let z = 20; z < 30; z++) hill[`${x},${z}`] = [M("stone"), 150];
  const s = sceneOf(world({ ...hill, "40,40": [M("red"), 134] }, 64), null);
  const players = [bot(12.5, 12.5, 130, 0.4), bot(44.5, 20.5, 130, 2, { name: "fcvm_bot2" }), bot(31, 25, 130, 1, { name: "fcvm_bot3" })];
  const inFrame = (cam: Parameters<typeof viewProj>[0], p: ScenePlayer) => {
    const q = project(viewProj(cam, 1), [p.x, p.y + 1, p.z]);
    return q !== null && Math.abs(q[0]) < 1 && Math.abs(q[1]) < 1;
  };

  it("keeps the player a shot follows in the frame and the eye above the ground, through every cut", () => {
    for (let seed = 0; seed < 8; seed++) {
      for (let t = 0; t < 12 * SHOT_MS; t += 1700) {
        const shot = shotOf({ scene: s, players, seed, timeMs: t, still: false });
        expect(shot.cam.eye[1], `${seed} ${t}`).toBeGreaterThan(groundAt(s, shot.cam.eye[0], shot.cam.eye[2]) + 2);
        if (shot.follows !== null) {
          const p = players.find((q) => q.name === shot.follows)!;
          expect(inFrame(shot.cam, p), `${seed} ${t} ${shot.kind}`).toBe(true);
          // and no hill stands between the camera and it
          expect(labelsOf(s, [p], shot.cam, 400, 400, shot.figScale).map((l) => l.name), `${seed} ${t} sight`).toEqual([p.name]);
        }
        else if (shot.kind === "wide") for (const p of players) expect(inFrame(shot.cam, p), `${seed} ${t} wide`).toBe(true);
      }
    }
  });

  it("shows a different shot on every tile at the same instant, and uses every kind", () => {
    const eyes = Array.from({ length: 8 }, (_, seed) => shotOf({ scene: s, players, seed, timeMs: 123_456, still: false }).cam.eye.map((v) => v.toFixed(1)).join());
    expect(new Set(eyes).size).toBe(8);
    const kinds = new Set(Array.from({ length: 40 }, (_, k) => shotOf({ scene: s, players, seed: 3, timeMs: k * SHOT_MS, still: false }).kind));
    expect([...kinds].sort()).toEqual(["chase", "orbit", "wide"]);
  });

  it("cuts at a different moment on each tile", () => {
    const jump = (seed: number, t: number) => {
      const a = shotOf({ scene: s, players, seed, timeMs: t - 1, still: false }).cam.eye;
      const b = shotOf({ scene: s, players, seed, timeMs: t + 1, still: false }).cam.eye;
      return Math.hypot(a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    };
    expect(jump(0, SHOT_MS)).toBeGreaterThan(1);
    expect(jump(1, SHOT_MS)).toBeLessThan(0.1);
    expect(jump(1, SHOT_MS - 3700)).toBeGreaterThan(1);
  });

  it("moves within a shot, and stands still when asked: one wide view per tile, each from its own side", () => {
    const at = (t: number, still: boolean, seed = 2) => shotOf({ scene: s, players, seed, timeMs: t, still }).cam.eye.join();
    expect(at(1000, false)).not.toBe(at(1500, false));
    expect(at(1000, true)).toBe(at(987_654, true));
    expect(shotOf({ scene: s, players, seed: 2, timeMs: 0, still: true }).kind).toBe("wide");
    expect(at(0, true, 2)).not.toBe(at(0, true, 3));
  });

  it("follows a driven bot from behind, whatever the time and even standing still", () => {
    const driven = [players[0]!, { ...players[1]!, driven: true }];
    for (const still of [false, true]) {
      const shot = shotOf({ scene: s, players: driven, seed: 1, timeMs: 5 * SHOT_MS + 10, still });
      expect(shot.follows).toBe("fcvm_bot2");
      const fwd = [-Math.sin(2), Math.cos(2)];
      const back = [shot.cam.eye[0] - 44.5, shot.cam.eye[2] - 20.5];
      expect(back[0]! * fwd[0]! + back[1]! * fwd[1]!).toBeLessThan(-5);
    }
  });

  it("aims at what the bots built when no bot shows, and at the whole region when there is nothing", () => {
    const sunk = players.map((p) => ({ ...p, y: 100 }));
    const shots = Array.from({ length: 8 }, (_, k) => shotOf({ scene: s, players: sunk, seed: 0, timeMs: k * SHOT_MS, still: false }));
    expect(shots.every((x) => x.follows === null)).toBe(true);
    const orbit = shots.find((x) => x.kind === "orbit")!;
    expect(orbit.cam.subject.slice(0, 3)).toEqual([40.5, 134, 40.5]);
    const empty = sceneOf(world({}, 64), null);
    expect(new Set(Array.from({ length: 8 }, (_, k) => shotOf({ scene: empty, players: sunk, seed: 0, timeMs: k * SHOT_MS, still: false }).kind))).toEqual(new Set(["wide"]));
  });

  it("cuts to a column that just changed, to the bot beside it when one shows", () => {
    const i = 21 * 64 + 45;
    const shot = shotOf({ scene: s, players, seed: 0, timeMs: 30_000, still: false, recent: { i, at: 29_000 } });
    expect(shot.follows).toBe("fcvm_bot2");
    const far = shotOf({ scene: s, players, seed: 0, timeMs: 30_000, still: false, recent: { i: 60 * 64 + 60, at: 29_000 } });
    expect(far.follows).toBeNull();
    expect(far.cam.subject[0]).toBe(60.5);
    // an old change draws nothing
    expect(shotOf({ scene: s, players, seed: 0, timeMs: 60_000, still: false, recent: { i, at: 29_000 } })).toEqual(shotOf({ scene: s, players, seed: 0, timeMs: 60_000, still: false }));
  });

  it("puts a name over its bot's head, and none behind a hill, under the lake or outside the frame", () => {
    const cam = chaseCam(s, players[0]!);
    const ls = labelsOf(s, [players[0]!, { ...players[0]!, name: "sunk", y: 100 }], cam, 400, 400, 1);
    expect(ls.map((l) => l.name)).toEqual(["fcvm_bot1"]);
    const head = project(viewProj(cam, 1), [12.5, 130 + 2.25, 12.5])!;
    expect(ls[0]!.x).toBeCloseTo((head[0] * 0.5 + 0.5) * 400, 3);
    expect(ls[0]!.y).toBeCloseTo((0.5 - head[1] * 0.5) * 400 - 4, 3);
    // from the far side of the hill, looking at a bot behind it
    const hidden = bot(25, 31.5, 130, 0, { name: "behind" });
    const over = { ...cam, eye: [25, 133, 12] as [number, number, number], target: [25, 132, 31.5] as [number, number, number] };
    expect(labelsOf(s, [hidden], over, 400, 400, 1)).toEqual([]);
    expect(labelsOf(s, [bot(25, 16, 130, 0, { name: "front" })], over, 400, 400, 1).map((l) => l.name)).toEqual(["front"]);
  });
});

describe("the sky", () => {
  it("lights a world by its time of day, and at mid-morning when its tile sends no clock", () => {
    expect(skyOf(null)).toEqual(skyOf(DEFAULT_DAY_TIME));
    const noon = skyOf(6000);
    const midnight = skyOf(18000);
    expect(noon.sunUp).toBe(true);
    expect(noon.night).toBe(0);
    expect(noon.light[1]).toBeGreaterThan(0.7);
    expect(midnight.sunUp).toBe(false);
    expect(midnight.night).toBe(1);
    expect(midnight.zenith[2]).toBeLessThan(noon.zenith[2] / 3);
    // the light always comes from above
    for (let t = 0; t < 24000; t += 500) expect(skyOf(t).light[1], String(t)).toBeGreaterThan(0.1);
    expect(skyOf(24000 + 6000)).toEqual(noon);
  });
});

describe("a fork's flight", () => {
  const from = { x: 0, y: 100, w: 200, h: 200 };
  const to = { x: 210, y: 100, w: 200, h: 200 };

  it("leaves its source, holds short of the target while the fork runs, and lands when the copy's world is in", () => {
    expect(flightProgress(0, null)).toBe(0);
    expect(flightProgress(400, null)).toBeGreaterThan(0.3);
    expect(flightProgress(60_000, null)).toBeLessThanOrEqual(0.85);
    expect(flightProgress(60_000, null)).toBeGreaterThan(0.84);
    expect(flightProgress(1200, 0)).toBe(flightProgress(1200, null));
    expect(flightProgress(1200, FLIGHT_LAND_MS)).toBe(1);
    expect(flightProgress(1200, FLIGHT_LAND_MS * 3)).toBe(1);
  });

  it("starts on the source's box, ends on the target's, and is smaller and higher between", () => {
    expect(flightBox(from, to, 0)).toEqual(from);
    const end = flightBox(from, to, 1);
    for (const k of ["x", "y", "w", "h"] as const) expect(end[k]).toBeCloseTo(to[k], 6);
    const mid = flightBox(from, to, 0.5);
    expect(mid.w).toBeCloseTo(120);
    expect(mid.x + mid.w / 2).toBeCloseTo(205);
    expect(mid.y + mid.h / 2).toBeLessThan(200);
  });
});

describe("drawing within a device's means", () => {
  it("gives up detail, then frames, then the flight when frames run long, and tries again after 20 s of short frames", () => {
    const b = new FrameBudget();
    expect([b.level, b.everyMs, b.step, b.still]).toEqual([0, 33, 1, false]);
    for (let k = 0; k < 19; k++) b.sample(FRAME_BUDGET_MS * 2, k);
    expect(b.level).toBe(0);
    b.sample(FRAME_BUDGET_MS * 2, 20);
    expect([b.level, b.everyMs, b.step]).toEqual([1, 33, 2]);
    for (let k = 0; k < 20; k++) b.sample(FRAME_BUDGET_MS * 2, 100 + k);
    expect([b.level, b.everyMs, b.step, b.still]).toEqual([2, 100, 2, false]);
    for (let k = 0; k < 20; k++) b.sample(FRAME_BUDGET_MS * 2, 200 + k);
    expect([b.level, b.everyMs, b.step, b.still]).toEqual([3, 1000, 4, true]);
    for (let k = 0; k < 20; k++) b.sample(FRAME_BUDGET_MS * 2, 300 + k);
    expect(b.level).toBe(3);
    // short frames, but too soon
    for (let k = 0; k < 20; k++) b.sample(5, 1000 + k);
    expect(b.level).toBe(3);
    for (let k = 0; k < 20; k++) b.sample(5, 30_000 + k);
    expect(b.level).toBe(2);
    // a software renderer starts at the last level, and earns its way up
    const soft = new FrameBudget(true);
    expect([soft.level, soft.still]).toEqual([3, true]);
    for (let k = 0; k < 20; k++) soft.sample(5, 100 + k);
    expect(soft.level).toBe(2);
    // one long frame among short ones changes nothing
    const c = new FrameBudget();
    for (let k = 0; k < 20; k++) c.sample(k === 3 ? 500 : 10, k);
    expect(c.level).toBe(0);
    // three frames in a row far over the budget do not wait for twenty
    const d = new FrameBudget();
    d.sample(FRAME_BUDGET_MS * 5, 1);
    d.sample(FRAME_BUDGET_MS * 5, 2);
    expect(d.level).toBe(0);
    d.sample(FRAME_BUDGET_MS * 5, 3);
    expect(d.level).toBe(1);
    const e = new FrameBudget();
    for (const cost of [FRAME_BUDGET_MS * 5, 10, FRAME_BUDGET_MS * 5]) e.sample(cost, 1);
    expect(e.level).toBe(0);
    // nor do short frames before them (a page that starts with nothing to draw) hide them
    for (const cost of [FRAME_BUDGET_MS * 5, FRAME_BUDGET_MS * 5]) e.sample(cost, 2);
    expect(e.level).toBe(1);
  });

  it("holds the stage's canvas to a pixel cap, and knows a tile that is off the screen", () => {
    expect(pixelScale(1000, 800, 2)).toBeCloseTo(Math.sqrt(2_500_000 / 3_200_000) * 2);
    expect(pixelScale(800, 600, 1)).toBe(1);
    expect(onScreen({ x: 10, y: 700, w: 100, h: 100 }, 400, 800)).toBe(true);
    expect(onScreen({ x: 10, y: 800, w: 100, h: 100 }, 400, 800)).toBe(false);
    expect(onScreen({ x: -100, y: 0, w: 100, h: 100 }, 400, 800)).toBe(false);
  });
});
