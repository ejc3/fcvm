// Minecraft mode in 3D: a tile's columns as geometry, the camera that flies through its world,
// its sky, a fork's flight between two tiles, and the guard that slows the drawing down on a slow
// device. Pure: the page passes the worlds and the time in and draws what comes back with WebGL2
// (app/_wall/minecraft-gl.ts). Blocks are plain boxes in `MATERIAL_COLORS`; nothing of the game's.
//
// Scene coordinates: x and z are a column's place in the grid, y is the height byte of the view
// socket (`y + 64`), so a column whose byte is h has its top face at y = h.

import type { Box } from "./layout.ts";
import { MATERIAL_COLORS, MATERIALS, type McPlayer, type McRegion, type McWorld } from "./minecraft.ts";

const idx = (name: (typeof MATERIALS)[number]) => MATERIALS.indexOf(name);
const WATER = idx("water");
const GRASS = idx("grass");
const LEAVES = idx("leaves");
const WOOD = idx("wood");
const PLANT = idx("plant");
/** Blocks that stand on the ground without filling their column. */
const SMALL = new Set([PLANT, idx("flower"), idx("crop"), idx("mushroom")]);
/** The first of the 16 dye colours: what the bots build with. */
const FIRST_DYE = idx("white");

export const MATERIAL_RGB01: readonly (readonly [number, number, number])[] = MATERIAL_COLORS.map((hex) => {
  const v = Number.parseInt(hex.slice(1), 16);
  return [((v >> 16) & 255) / 255, ((v >> 8) & 255) / 255, (v & 255) / 255] as const;
});

/** Bytes per column instance: x and z (two bytes each, little-endian), material, height, base (how far down its sides reach), flags. */
export const COLUMN_BYTES = 8;
/** Flags: bits 0-1 how many neighbours stand higher, bits 2-4 water depth in blocks, bit 5 just changed, bit 6 a canopy, bit 7 differs from the fork's source. */
export const FLAG_RECENT = 32;
export const FLAG_CANOPY = 64;
export const FLAG_DIFF = 128;
/** Floats per box instance: centre x, base y, centre z; size x, y, z; yaw; r, g, b; kind. */
export const BOX_FLOATS = 11;
/** A box's kind: 0 plain, above 0 lit by itself that much; a canopy is not drawn near the camera, a trunk always is. */
export const BOX_CANOPY = -1;
export const BOX_TRUNK = -2;
/** How far the world's edge reaches below its lowest column. */
const FLOOR_DEPTH = 10;
/** A lake with no ground in the data deepens this far from its shore; with ground, to its true depth. */
const GUESSED_DEPTH = 4;
const MAX_DEPTH = 7;

function hash2(x: number, z: number): number {
  let h = Math.imul(x, 374761393) + Math.imul(z, 668265263);
  h = Math.imul(h ^ (h >>> 13), 1274126177);
  return ((h ^ (h >>> 16)) >>> 0) / 4294967296;
}

/** Instances written into one typed array that grows, not into a plain array spread element by element. */
class Bytes {
  private buf: Uint8Array;
  private n = 0;
  constructor(room: number) {
    this.buf = new Uint8Array(Math.max(64, room));
  }
  put(x: number, z: number, m: number, y: number, base: number, flags: number): void {
    if (this.n + COLUMN_BYTES > this.buf.length) {
      const next = new Uint8Array(this.buf.length * 2);
      next.set(this.buf);
      this.buf = next;
    }
    const b = this.buf;
    const o = this.n;
    b[o] = x & 255;
    b[o + 1] = x >> 8;
    b[o + 2] = z & 255;
    b[o + 3] = z >> 8;
    b[o + 4] = m;
    b[o + 5] = y;
    b[o + 6] = base;
    b[o + 7] = flags;
    this.n = o + COLUMN_BYTES;
  }
  done(): Uint8Array {
    return this.buf.slice(0, this.n);
  }
}

class Boxes {
  private buf: Float32Array;
  private n = 0;
  constructor(room: number) {
    this.buf = new Float32Array(Math.max(BOX_FLOATS * 16, room));
  }
  put(x: number, y: number, z: number, sx: number, sy: number, sz: number, yaw: number, rgb: readonly [number, number, number], kind: number, tint = 1): void {
    if (this.n + BOX_FLOATS > this.buf.length) {
      const next = new Float32Array(this.buf.length * 2);
      next.set(this.buf);
      this.buf = next;
    }
    const b = this.buf;
    const o = this.n;
    b[o] = x;
    b[o + 1] = y;
    b[o + 2] = z;
    b[o + 3] = sx;
    b[o + 4] = sy;
    b[o + 5] = sz;
    b[o + 6] = yaw;
    b[o + 7] = rgb[0] * tint;
    b[o + 8] = rgb[1] * tint;
    b[o + 9] = rgb[2] * tint;
    b[o + 10] = kind;
    this.n = o + BOX_FLOATS;
  }
  done(): Float32Array {
    return this.buf.slice(0, this.n);
  }
}

export interface McScene {
  w: number;
  h: number;
  /** Per column, where a figure stands: the ground, or a lake's surface. */
  ground: Uint8Array;
  /** Blocks a drawn column spans each way: 1, or more on a device that draws slowly. */
  step: number;
  /** Instances whose top face is drawn: every column, and every canopy. */
  tops: Uint8Array;
  /** The ones among them whose sides show: they stand above a neighbour. */
  sides: Uint8Array;
  /** Water columns, for the surface drawn over the lake's bed. */
  water: Uint8Array;
  boxes: Float32Array;
  boxCount: number;
  floor: number;
  /** Up to six columns the bots built on, the most built-up neighbourhoods first. */
  sites: number[];
  /** Columns that differ from `diffFrom`. */
  diff: number;
}

export interface SceneOpts {
  /** `map.ground`: two bytes a column, the highest block that is not leaves, a plant or water. Without it the ground under those is guessed. */
  ground?: Uint8Array | null;
  /** The cells of the world this one was forked from: columns that differ are flagged. */
  diffFrom?: Uint8Array | null;
  /** Columns that changed a moment ago. */
  recent?: ReadonlySet<number> | null;
  /** Draw one column for every `step` x `step` blocks (1, 2 or 4): a quarter or a sixteenth of the work. */
  step?: number;
}

/**
 * A world as instances. The socket carries one block per column, so: a cliff is solid down to its
 * lowest neighbour; under leaves the ground comes from the columns beside them that are not
 * leaves, spreading inward, with a trunk under each canopy's top; a lake's bed deepens from its
 * shore; a plant is a small box on the block under it. `ground` replaces each of those guesses.
 * With `step` above 1 each drawn column stands for a square of blocks: the one the bots built
 * highest on, else its first.
 */
export function sceneOf(world: McWorld, remap: Uint8Array | null, opts: SceneOpts = {}): McScene {
  const { w, h } = world.region;
  const n = w * h;
  const c = world.cells;
  const g = opts.ground && opts.ground.length === n * 2 ? opts.ground : null;
  const matOf = (raw: number) => (remap ? remap[raw]! : raw < MATERIALS.length ? raw : 0);
  const top = new Uint8Array(n);
  const gm = new Uint8Array(n);
  const gh = new Uint8Array(n);
  const known = new Uint8Array(n);
  for (let i = 0; i < n; i++) {
    const m = matOf(c[i * 2]!);
    const y = c[i * 2 + 1]!;
    top[i] = m;
    if (g !== null && (m === LEAVES || m === WATER || SMALL.has(m))) {
      gm[i] = m === WATER ? WATER : matOf(g[i * 2]!);
      gh[i] = m === WATER ? y : Math.min(y, g[i * 2 + 1]!);
      known[i] = 1;
    } else if (m === LEAVES) continue;
    else if (SMALL.has(m)) {
      gm[i] = GRASS;
      gh[i] = Math.max(0, y - 1);
      known[i] = 1;
    } else {
      gm[i] = m;
      gh[i] = y;
      known[i] = 1;
    }
  }
  for (let pass = 0; pass < 64; pass++) {
    const add: number[] = [];
    for (let i = 0; i < n; i++) {
      if (known[i]) continue;
      const x = i % w;
      let sum = 0;
      let k = 0;
      for (const j of [x > 0 ? i - 1 : -1, x < w - 1 ? i + 1 : -1, i - w, i + w]) {
        if (j < 0 || j >= n || !known[j]) continue;
        sum += gm[j] === WATER ? gh[j]! + 1 : gh[j]!;
        k++;
      }
      if (k > 0) add.push(i, Math.min(Math.round(sum / k), c[i * 2 + 1]! - 1));
    }
    if (add.length === 0) break;
    for (let a = 0; a < add.length; a += 2) {
      gh[add[a]!] = Math.max(0, add[a + 1]!);
      gm[add[a]!] = GRASS;
      known[add[a]!] = 1;
    }
  }
  let lowest = 255;
  for (let i = 0; i < n; i++) {
    if (!known[i]) {
      gh[i] = Math.max(0, c[i * 2 + 1]! - 4);
      gm[i] = GRASS;
    }
    if (gh[i]! < lowest && !(top[i] === 0 && gh[i] === 0)) lowest = gh[i]!;
  }
  const floor = Math.max(0, (lowest === 255 ? 0 : lowest) - FLOOR_DEPTH);
  // How deep each water column is drawn.
  const depth = new Uint8Array(n);
  if (g !== null) {
    for (let i = 0; i < n; i++) if (gm[i] === WATER) depth[i] = Math.max(1, Math.min(MAX_DEPTH, gh[i]! - g[i * 2 + 1]!));
  } else {
    for (let i = 0; i < n; i++) depth[i] = gm[i] === WATER ? GUESSED_DEPTH : 0;
    for (let pass = 1; pass < GUESSED_DEPTH; pass++) {
      for (let i = 0; i < n; i++) {
        if (!depth[i]) continue;
        const x = i % w;
        let m = GUESSED_DEPTH;
        if (x > 0) m = Math.min(m, depth[i - 1]!);
        if (x < w - 1) m = Math.min(m, depth[i + 1]!);
        if (i >= w) m = Math.min(m, depth[i - w]!);
        if (i < n - w) m = Math.min(m, depth[i + w]!);
        depth[i] = Math.min(depth[i]!, m + 1);
      }
    }
  }
  const src = opts.diffFrom && opts.diffFrom.length === c.length ? opts.diffFrom : null;
  let diff = 0;
  const builds: number[] = [];
  const differs = new Uint8Array(n);
  for (let i = 0; i < n; i++) {
    if (top[i]! >= FIRST_DYE) builds.push(i);
    if (src !== null && (src[i * 2] !== c[i * 2] || src[i * 2 + 1] !== c[i * 2 + 1])) {
      differs[i] = 1;
      diff++;
    }
  }

  // The grid that is drawn: every column, or one for each square of `step` blocks.
  const step = opts.step === 2 || opts.step === 4 ? opts.step : 1;
  const dw = Math.ceil(w / step);
  const dh = Math.ceil(h / step);
  const rep = new Int32Array(dw * dh);
  // Whether any column a drawn cell stands for differs or just changed: the outline and the
  // glow then agree with the count, whatever the detail.
  const cellDiff = new Uint8Array(dw * dh);
  const cellRecent = new Uint8Array(dw * dh);
  for (let dz = 0; dz < dh; dz++) {
    for (let dx = 0; dx < dw; dx++) {
      const d = dz * dw + dx;
      let pick = dz * step * w + dx * step;
      let best = -1;
      for (let z = dz * step; z < Math.min(h, (dz + 1) * step); z++) {
        for (let x = dx * step; x < Math.min(w, (dx + 1) * step); x++) {
          const i = z * w + x;
          if (differs[i]) cellDiff[d] = 1;
          if (opts.recent?.has(i)) cellRecent[d] = 1;
          if (step > 1 && top[i]! >= FIRST_DYE && c[i * 2 + 1]! > best) {
            best = c[i * 2 + 1]!;
            pick = i;
          }
        }
      }
      rep[d] = pick;
    }
  }
  const bedOf = (d: number) => gh[rep[d]!]! - depth[rep[d]!]!;
  const isLeaf = (d: number) => top[rep[d]!] === LEAVES;
  const crownOf = (d: number) => c[rep[d]! * 2 + 1]!;
  /** Where a canopy's underside is: a few blocks under its crown, clear of the ground. */
  const underOf = (d: number) => Math.max(gh[rep[d]!]! + 2, crownOf(d) - 3);
  const cells = dw * dh;
  const tops = new Bytes(cells * COLUMN_BYTES * 2);
  const sides = new Bytes(cells * COLUMN_BYTES);
  const water = new Bytes((cells * COLUMN_BYTES) >> 2);
  const boxes = new Boxes(cells * BOX_FLOATS >> 3);
  const put = (x: number, z: number, m: number, y: number, base: number, flags: number) => {
    tops.put(x, z, m, y, base, flags);
    if (base < y) sides.put(x, z, m, y, base, flags);
    if (m === WATER && (flags & FLAG_CANOPY) === 0) water.put(x, z, m, y, base, flags);
  };
  for (let d = 0; d < dw * dh; d++) {
    const dx = d % dw;
    const dz = (d - dx) / dw;
    const i = rep[d]!;
    const x = dx * step;
    const z = dz * step;
    const y = gh[i]!;
    const around = [dx > 0 ? d - 1 : -1, dx < dw - 1 ? d + 1 : -1, dz > 0 ? d - dw : -1, dz < dh - 1 ? d + dw : -1];
    const nb = around.map((j) => (j < 0 ? floor : bedOf(j)));
    let higher = 0;
    for (const v of nb) if (v > y) higher++;
    let flags = Math.min(3, higher) | (depth[i]! << 2);
    if (cellRecent[d]) flags |= FLAG_RECENT;
    if (cellDiff[d]) flags |= FLAG_DIFF;
    const unknown = top[i] === 0 && c[i * 2 + 1] === 0;
    const bed = y - depth[i]!;
    put(x, z, gm[i]!, unknown ? floor : y, unknown ? floor : Math.max(floor, Math.min(bed, ...nb)), flags);
    const t = top[i]!;
    if (SMALL.has(t) && step === 1) {
      const v = hash2(x, z);
      boxes.put(x + 0.3 + v * 0.4, y, z + 0.3 + hash2(z, x) * 0.4, 0.28, t === PLANT ? 0.55 : 0.4, 0.28, v * 3, MATERIAL_RGB01[t]!, 0);
    }
    if (t !== LEAVES) continue;
    const crown = crownOf(d);
    const wood = MATERIAL_RGB01[WOOD]!;
    const mid = step / 2;
    if (crown - y <= 2) {
      // A bush: leaves down to the ground.
      put(x, z, LEAVES, crown, y, FLAG_CANOPY);
      continue;
    }
    const under = underOf(d);
    if (!around.some((j) => j >= 0 && isLeaf(j)) && step === 1) {
      // A lone column of leaves is a small tree: a trunk and a crown wider than a block.
      const v = hash2(x, z);
      const leaf = MATERIAL_RGB01[LEAVES]!;
      const tint = 0.82 + 0.34 * v;
      boxes.put(x + 0.5, y, z + 0.5, 0.42, under - y + 0.5, 0.42, 0, wood, BOX_TRUNK);
      boxes.put(x + 0.5, under, z + 0.5, 1.9 + v * 0.5, crown - under - 0.9, 1.9 + v * 0.5, v * 1.5, leaf, BOX_CANOPY, tint);
      boxes.put(x + 0.5, crown - 0.9, z + 0.5, 1.1, 0.9, 1.1, v * 1.5, leaf, BOX_CANOPY, tint);
      continue;
    }
    // Part of a wide canopy: its top, and its sides down to the canopy beside it or its own underside.
    let base = crown;
    for (const j of around) base = Math.min(base, j >= 0 && isLeaf(j) ? Math.max(under, crownOf(j)) : under);
    put(x, z, LEAVES, crown, base, FLAG_CANOPY);
    // A trunk under each top of the canopy.
    let peak = true;
    for (let oz = -1; oz <= 1 && peak; oz++) {
      for (let ox = -1; ox <= 1; ox++) {
        const xx = dx + ox;
        const zz = dz + oz;
        if ((ox === 0 && oz === 0) || xx < 0 || zz < 0 || xx >= dw || zz >= dh) continue;
        const j = zz * dw + xx;
        if (isLeaf(j) && (crownOf(j) > crown || (crownOf(j) === crown && j < d))) {
          peak = false;
          break;
        }
      }
    }
    if (peak) boxes.put(x + mid, y, z + mid, 0.5, under - y, 0.5, 0, wood, BOX_TRUNK);
  }
  const boxArray = boxes.done();
  return {
    w,
    h,
    ground: gh,
    step,
    tops: tops.done(),
    sides: sides.done(),
    water: water.done(),
    boxes: boxArray,
    boxCount: boxArray.length / BOX_FLOATS,
    floor,
    sites: sitesOf(builds, w),
    diff,
  };
}

function sitesOf(builds: readonly number[], w: number): number[] {
  if (builds.length === 0) return [];
  const sample = builds.length > 400 ? builds.filter((_, k) => k % Math.ceil(builds.length / 400) === 0) : builds;
  const near = (i: number) => {
    let k = 0;
    for (const j of sample) if (Math.abs((j % w) - (i % w)) <= 5 && Math.abs(Math.floor(j / w) - Math.floor(i / w)) <= 5) k++;
    return k;
  };
  const ranked = sample.map((i) => [near(i), i] as const).sort((a, b) => b[0] - a[0] || a[1] - b[1]);
  const out: number[] = [];
  for (const [, i] of ranked) {
    if (out.every((j) => Math.abs((j % w) - (i % w)) > 8 || Math.abs(Math.floor(j / w) - Math.floor(i / w)) > 8)) out.push(i);
    if (out.length === 6) break;
  }
  return out;
}

/** How many columns of two worlds of one size differ (0 when the sizes do not match). */
export function countDiff(a: Uint8Array, b: Uint8Array): number {
  if (a.length !== b.length) return 0;
  let k = 0;
  for (let i = 0; i < a.length; i += 2) if (a[i] !== b[i] || a[i + 1] !== b[i + 1]) k++;
  return k;
}

/** The ground (or lake surface) under a point; outside the grid, the nearest column's. */
export function groundAt(s: McScene, x: number, z: number): number {
  const cx = Math.max(0, Math.min(s.w - 1, Math.floor(x)));
  const cz = Math.max(0, Math.min(s.h - 1, Math.floor(z)));
  return s.ground[cz * s.w + cx]!;
}

// ---- figures ---------------------------------------------------------------------------------

export interface ScenePlayer {
  name: string;
  x: number;
  y: number;
  z: number;
  /** Radians; 0 faces +z, a quarter turn faces -x (the game's yaw). */
  yaw: number;
  bot: boolean;
  driven: boolean;
}

/** A player in scene coordinates: feet at `y + 63` (standing on a column of byte h is y = h). */
export function scenePlayer(region: McRegion, p: McPlayer): ScenePlayer {
  return { name: p.name, x: p.x - region.x0, y: p.y + 63, z: p.z - region.z0, yaw: (p.yaw * Math.PI) / 180, bot: p.bot, driven: p.driven };
}

/** Whether any of a figure shows above the ground or water it stands in. */
export function showsAbove(s: McScene, p: ScenePlayer): boolean {
  return p.x >= 0 && p.z >= 0 && p.x < s.w && p.z < s.h && p.y + 1.7 >= groundAt(s, p.x, p.z);
}

/**
 * A figure as boxes appended to `out`: legs, body, arms, head, and a dark band on the side it
 * faces; `beacon` adds a thin light above it for the shots that stand far back, `driven` a plate
 * under its feet.
 */
export function figureBoxes(out: number[], p: ScenePlayer, rgb: readonly [number, number, number], scale: number, o: { glow: number; beacon: boolean }): void {
  const cy = Math.cos(p.yaw);
  const sy = Math.sin(p.yaw);
  const dark = [rgb[0] * 0.55, rgb[1] * 0.55, rgb[2] * 0.55] as const;
  const put = (ox: number, oy: number, oz: number, sx: number, sh: number, sz: number, col: readonly [number, number, number], kind: number) => {
    out.push(p.x + (ox * cy - oz * sy) * scale, p.y + oy * scale, p.z + (ox * sy + oz * cy) * scale, sx * scale, sh * scale, sz * scale, p.yaw, col[0], col[1], col[2], kind);
  };
  put(-0.13, 0, 0, 0.22, 0.72, 0.24, dark, 0);
  put(0.13, 0, 0, 0.22, 0.72, 0.24, dark, 0);
  put(0, 0.72, 0, 0.52, 0.7, 0.28, rgb, o.glow * 0.25);
  put(-0.36, 0.76, 0, 0.17, 0.66, 0.22, rgb, 0);
  put(0.36, 0.76, 0, 0.17, 0.66, 0.22, rgb, 0);
  put(0, 1.44, 0, 0.44, 0.44, 0.44, [0.93, 0.9, 0.84], o.glow * 0.2);
  put(0, 1.6, 0.21, 0.34, 0.12, 0.06, [0.08, 0.09, 0.11], 0);
  if (o.beacon) out.push(p.x, p.y + 2.4 * scale, p.z, 0.09 * scale, 7 * scale, 0.09 * scale, 0, rgb[0], rgb[1], rgb[2], 1);
  if (p.driven) out.push(p.x, p.y + 0.03, p.z, 1.5 * scale, 0.05, 1.5 * scale, p.yaw, 0.49, 0.77, 1, 1);
}

// ---- cameras ---------------------------------------------------------------------------------

export type McVec3 = [number, number, number];

export interface McCamera {
  eye: McVec3;
  target: McVec3;
  /** Vertical field of view, radians. */
  fov: number;
  fog: [number, number];
  /** Canopies closer to the eye than this, or closer to `subject` than `subCut`, are not drawn. */
  nearCut: number;
  subject: McVec3;
  subCut: number;
}

export type ShotKind = "chase" | "orbit" | "wide";

export interface McShot {
  kind: ShotKind;
  cam: McCamera;
  /** Figures are drawn this many times life size, with a beacon from 1.25 up. */
  figScale: number;
  /** The player the shot follows, if it follows one. */
  follows: string | null;
}

/** A tile holds one shot this long, then cuts to the next. */
export const SHOT_MS = 10_000;
/** A column that changed this recently draws the next cut to it. */
export const RECENT_CUT_MS = 15_000;
const KINDS: readonly ShotKind[] = ["orbit", "chase", "orbit", "wide"];

function keepAbove(s: McScene, eye: McVec3, clear: number): McVec3 {
  let top = 0;
  for (let dz = -2; dz <= 2; dz++) for (let dx = -2; dx <= 2; dx++) top = Math.max(top, groundAt(s, eye[0] + dx, eye[2] + dz));
  eye[1] = Math.max(eye[1], top + clear);
  return eye;
}

/** Raise the eye, up to `lifts` times, until no ground stands between it and `to`; whether it then sees it. */
function clearSight(s: McScene, eye: McVec3, to: McVec3, lifts: number): boolean {
  for (let lift = 0; ; lift++) {
    let blocked = false;
    for (let k = 1; k < 24 && !blocked; k++) {
      const t = k / 24;
      const x = eye[0] + (to[0] - eye[0]) * t;
      const z = eye[2] + (to[2] - eye[2]) * t;
      if (x >= 0 && z >= 0 && x < s.w && z < s.h && groundAt(s, x, z) > eye[1] + (to[1] - eye[1]) * t - 0.3) blocked = true;
    }
    if (!blocked) return true;
    if (lift >= lifts) return false;
    eye[1] += 3;
  }
}

/** Behind and above a player, looking past it at what it faces. */
export function chaseCam(s: McScene, p: { x: number; y: number; z: number; yaw: number }, o: { back?: number; up?: number; side?: number; cut?: number } = {}): McCamera {
  const fx = -Math.sin(p.yaw);
  const fz = Math.cos(p.yaw);
  const back = o.back ?? 8;
  const side = o.side ?? 2.5;
  const eye = keepAbove(s, [p.x - fx * back - fz * side, p.y + (o.up ?? 6), p.z - fz * back + fx * side], 2.2);
  clearSight(s, eye, [p.x, p.y + 2.4, p.z], 6);
  return { eye, target: [p.x + fx * 3, p.y + 1.2, p.z + fz * 3], fov: 1.0, fog: [55, 150], nearCut: back + 1, subject: [p.x, p.y, p.z], subCut: o.cut ?? 7 };
}

/** A low circle round a point, from where the point can be seen. */
export function orbitCam(s: McScene, p: { x: number; z: number }, o: { az: number; r?: number; up?: number; look?: number; cut?: number }): McCamera {
  const r = o.r ?? 15;
  const gy = groundAt(s, p.x, p.z);
  // From the side asked for; if a hill hides the point from there, from the next side that sees it.
  let eye: McVec3 = [0, 0, 0];
  for (const turn of [0, 1.2, -1.2, 2.4, -2.4, Math.PI]) {
    eye = keepAbove(s, [p.x + Math.cos(o.az + turn) * r, gy + (o.up ?? 9), p.z + Math.sin(o.az + turn) * r], 2.5);
    if (clearSight(s, eye, [p.x, gy + 2.4, p.z], 2)) break;
  }
  return { eye, target: [p.x, gy + (o.look ?? 2), p.z], fov: 0.9, fog: [60, 170], nearCut: r * 0.6, subject: [p.x, gy, p.z], subCut: o.cut ?? 8 };
}

/** The whole region from outside it. */
export function wideCam(s: McScene, az: number): McCamera {
  const r = Math.max(s.w, s.h) * 0.92;
  const mid = groundAt(s, s.w / 2, s.h / 2);
  return { eye: [s.w / 2 + Math.cos(az) * r, mid + 64, s.h / 2 + Math.sin(az) * r], target: [s.w / 2, mid + 2, s.h / 2], fov: 0.72, fog: [150, 330], nearCut: 0, subject: [0, 0, 0], subCut: 0 };
}

export interface ShotInput {
  scene: McScene;
  players: readonly ScenePlayer[];
  /** The tile's place in the wall: tiles cut at different times and start on different shots. */
  seed: number;
  timeMs: number;
  /** No flight (reduced motion, or a device that draws too slowly): one wide view from the tile's own side. */
  still: boolean;
  /** This page's viewer drives the tile's bot: the view from behind it is theirs also when the tile stands still. */
  mine?: boolean;
  /** The changed column this cut features, chosen when the cut began (`shotCue`). */
  cue?: number | null;
}

/** The cut a tile is in at `timeMs`: tiles cut at different times by their seed. */
export function cutOf(timeMs: number, seed: number): number {
  return Math.floor((timeMs + seed * 3700) / SHOT_MS);
}

/** The cut a tile's cue was chosen for, and the changed column it features (none: the tile's own shot). */
export interface ShotCue {
  cut: number;
  site: number | null;
}

/**
 * Which changed column, if any, a tile's present cut features. Chosen once, when the cut begins,
 * and kept for the whole cut: bots that go on building do not move the camera's subject in the
 * middle of a shot. A column that changed within RECENT_CUT_MS takes the next cut, but never two
 * cuts in a row, so the tile's own sequence of shots goes on however long the bots build.
 */
export function shotCue(prev: ShotCue | null, inp: { seed: number; timeMs: number; last: { i: number; at: number } | null }): ShotCue {
  const cut = cutOf(inp.timeMs, inp.seed);
  if (prev !== null && prev.cut === cut) return prev;
  const fresh = inp.last !== null && inp.timeMs - inp.last.at < RECENT_CUT_MS;
  const afterCue = prev !== null && prev.cut === cut - 1 && prev.site !== null;
  return { cut, site: fresh && !afterCue ? inp.last!.i : null };
}

/**
 * Where a tile's camera is at `timeMs`. A driven bot is followed from behind (on a tile that
 * stands still, only for the viewer who drives it). Otherwise every SHOT_MS the tile cuts to its
 * next shot: behind a bot that shows above ground, a circle round a bot or round something the
 * bots built, or the whole region; a cut with a cue (`shotCue`) circles the column that changed.
 */
export function shotOf(inp: ShotInput): McShot {
  const s = inp.scene;
  const driven = inp.players.find((p) => p.driven) ?? null;
  // Standing still (reduced motion), another viewer's driving does not move this page's tile.
  if (driven !== null && (!inp.still || inp.mine === true)) return { kind: "chase", cam: chaseCam(s, driven, { back: 7, up: 5, side: 0 }), figScale: 1, follows: driven.name };
  if (inp.still) return { kind: "wide", cam: wideCam(s, 0.7 + inp.seed * 1.9), figScale: 3.2, follows: null };
  const cut = cutOf(inp.timeMs, inp.seed);
  const u = (inp.timeMs + inp.seed * 3700) / SHOT_MS - cut;
  const pick = cut + inp.seed;
  const turn = hash2(cut, inp.seed) * Math.PI * 2;
  const shown = inp.players.filter((p) => showsAbove(s, p));
  const siteOf = (i: number) => ({ x: (i % s.w) + 0.5, z: Math.floor(i / s.w) + 0.5 });
  const orbit = (p: { x: number; z: number }, name: string | null, r: number): McShot => ({ kind: "orbit", cam: orbitCam(s, p, { az: turn + u * 1.0, r, up: r * 0.6 }), figScale: r > 16 ? 1.4 : 1.15, follows: name });
  const cue = inp.cue ?? null;
  const recent = cue !== null && cue >= 0 && cue < s.w * s.h ? siteOf(cue) : null;
  if (recent !== null) {
    const by = shown.find((p) => Math.hypot(p.x - recent.x, p.z - recent.z) < 12) ?? null;
    return orbit(by ?? recent, by?.name ?? null, 13);
  }
  const kind = KINDS[pick % KINDS.length]!;
  const bot = shown.length > 0 ? shown[pick % shown.length]! : null;
  if (kind === "wide" || (bot === null && s.sites.length === 0)) return { kind: "wide", cam: wideCam(s, turn + u * 0.35), figScale: 3.2, follows: null };
  if (bot === null) return orbit(siteOf(s.sites[pick % s.sites.length]!), null, 14 + (pick % 3) * 4);
  if (kind === "chase") return { kind: "chase", cam: chaseCam(s, bot, { side: 2.5 + Math.sin(u * Math.PI) }), figScale: 1, follows: bot.name };
  return orbit(bot, bot.name, 12 + (pick % 3) * 5);
}

// ---- matrices (column-major, as WebGL takes them) ----------------------------------------------

const sub = (a: McVec3, b: McVec3): McVec3 => [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
const cross = (a: McVec3, b: McVec3): McVec3 => [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
const dot = (a: McVec3, b: McVec3) => a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
const unit = (a: McVec3): McVec3 => {
  const l = Math.hypot(a[0], a[1], a[2]) || 1;
  return [a[0] / l, a[1] / l, a[2] / l];
};

/** View times projection for `cam` on a rectangle of `aspect` (width over height). */
export function viewProj(cam: McCamera, aspect: number): Float32Array {
  const near = 0.4;
  const far = 420;
  const f = 1 / Math.tan(cam.fov / 2);
  const fw = unit(sub(cam.target, cam.eye));
  const sd = unit(cross(fw, [0, 1, 0]));
  const up = cross(sd, fw);
  const view = [sd[0], up[0], -fw[0], 0, sd[1], up[1], -fw[1], 0, sd[2], up[2], -fw[2], 0, -dot(sd, cam.eye), -dot(up, cam.eye), dot(fw, cam.eye), 1];
  const proj = [f / aspect, 0, 0, 0, 0, f, 0, 0, 0, 0, (far + near) / (near - far), -1, 0, 0, (2 * far * near) / (near - far), 0];
  const out = new Float32Array(16);
  for (let col = 0; col < 4; col++) {
    for (let row = 0; row < 4; row++) {
      out[col * 4 + row] = proj[row]! * view[col * 4]! + proj[4 + row]! * view[col * 4 + 1]! + proj[8 + row]! * view[col * 4 + 2]! + proj[12 + row]! * view[col * 4 + 3]!;
    }
  }
  return out;
}

/** A point in clip space divided through: x and y in -1..1 inside the frame; null behind the camera. */
export function project(vp: Float32Array, p: McVec3): [number, number] | null {
  const w = vp[3]! * p[0] + vp[7]! * p[1] + vp[11]! * p[2] + vp[15]!;
  if (w <= 0.01) return null;
  return [(vp[0]! * p[0] + vp[4]! * p[1] + vp[8]! * p[2] + vp[12]!) / w, (vp[1]! * p[0] + vp[5]! * p[1] + vp[9]! * p[2] + vp[13]!) / w];
}

export interface McLabel {
  name: string;
  /** CSS pixels inside the tile: the middle of the name's baseline, over the head. */
  x: number;
  y: number;
  driven: boolean;
}

/** Each player's name over its head, in a tile `boxW` x `boxH`: not one outside the frame, or with ground or water between the camera and the head. */
export function labelsOf(s: McScene, players: readonly ScenePlayer[], cam: McCamera, boxW: number, boxH: number, figScale: number): McLabel[] {
  const vp = viewProj(cam, boxW / boxH);
  const out: McLabel[] = [];
  for (const p of players) {
    const head: McVec3 = [p.x, p.y + 2.25 * figScale, p.z];
    const q = project(vp, head);
    if (q === null || Math.abs(q[0]) > 0.97 || Math.abs(q[1]) > 0.95) continue;
    let hidden = false;
    for (let k = 1; k < 32 && !hidden; k++) {
      const t = k / 32;
      const x = cam.eye[0] + (head[0] - cam.eye[0]) * t;
      const z = cam.eye[2] + (head[2] - cam.eye[2]) * t;
      if (x >= 0 && z >= 0 && x < s.w && z < s.h && groundAt(s, x, z) > cam.eye[1] + (head[1] - cam.eye[1]) * t + 0.2) hidden = true;
    }
    if (!hidden) out.push({ name: p.name, x: (q[0] * 0.5 + 0.5) * boxW, y: (0.5 - q[1] * 0.5) * boxH - 4, driven: p.driven });
  }
  return out;
}

// ---- sky -------------------------------------------------------------------------------------

export interface McSky {
  /** Towards the sun, or the moon at night. */
  light: McVec3;
  lightColor: McVec3;
  ambient: McVec3;
  zenith: McVec3;
  horizon: McVec3;
  fog: McVec3;
  /** 0 by day, 1 at night: the bots carry a light then. */
  night: number;
  sunUp: boolean;
}

/** The time of day every world is lit at when its tile sends no clock: mid-morning. */
export const DEFAULT_DAY_TIME = 2900;

const mix = (a: McVec3, b: McVec3, t: number): McVec3 => [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t];
const smooth = (a: number, b: number, x: number) => {
  const t = Math.max(0, Math.min(1, (x - a) / (b - a)));
  return t * t * (3 - 2 * t);
};

/** The light of a world at `dayTime`, the game's ticks of the day (0 sunrise, 6000 noon, 12000 sunset, 18000 midnight). */
export function skyOf(dayTime: number | null): McSky {
  const d = (((dayTime ?? DEFAULT_DAY_TIME) % 24000) + 24000) % 24000;
  const a = (d / 24000) * Math.PI * 2;
  const el = Math.sin(a);
  const az = Math.cos(a);
  const day = smooth(-0.08, 0.3, el);
  const gold = Math.max(0, 1 - Math.abs(el) / 0.32);
  const sunUp = el >= -0.05;
  let zenith = mix([0.03, 0.05, 0.12], [0.25, 0.5, 0.86], day);
  let horizon = mix([0.09, 0.13, 0.26], [0.7, 0.84, 0.95], day);
  horizon = mix(horizon, [0.96, 0.6, 0.32], gold * 0.85);
  zenith = mix(zenith, [0.24, 0.26, 0.5], gold * 0.6);
  const light = unit(sunUp ? [az * 0.85, Math.max(0.12, el), 0.4] : [-az * 0.85, Math.max(0.2, -el), -0.4]);
  const k = 0.35 + 0.65 * day;
  const warm = mix([1.0, 0.62, 0.36], [1.0, 0.97, 0.9], smooth(0.05, 0.45, el));
  const lightColor: McVec3 = sunUp ? [warm[0] * k, warm[1] * k, warm[2] * k] : [0.3, 0.36, 0.56];
  return { light, lightColor, ambient: mix([0.2, 0.25, 0.42], [0.46, 0.5, 0.56], day), zenith, horizon, fog: mix(horizon, zenith, 0.25), night: smooth(0.08, -0.22, el), sunUp };
}

// ---- a fork's flight -------------------------------------------------------------------------

/** A landing takes this long once the copy's world is in. */
export const FLIGHT_LAND_MS = 300;

/**
 * How far a fork's picture is from its source (0) to the tile taking the copy (1): it closes on
 * 0.85 for as long as the fork runs, and lands in FLIGHT_LAND_MS once the copy's world is in.
 */
export function flightProgress(elapsedMs: number, landedForMs: number | null): number {
  const out = 0.85 * (1 - Math.exp(-Math.max(0, elapsedMs) / 700));
  if (landedForMs === null) return out;
  const t = Math.max(0, Math.min(1, landedForMs / FLIGHT_LAND_MS));
  return out + (1 - out) * t * t * (3 - 2 * t);
}

/** The flying picture's box at progress `p`: 0.6 of a tile in mid flight, on an arc above the straight line. */
export function flightBox(from: Box, to: Box, p: number): Box {
  const t = Math.max(0, Math.min(1, p));
  const mid = Math.sin(Math.PI * t);
  const k = 1 - 0.4 * mid;
  const w = (from.w + (to.w - from.w) * t) * k;
  const h = (from.h + (to.h - from.h) * t) * k;
  const cx = from.x + from.w / 2 + (to.x + to.w / 2 - from.x - from.w / 2) * t;
  const cy = from.y + from.h / 2 + (to.y + to.h / 2 - from.y - from.h / 2) * t - mid * Math.min(from.h, to.h) * 0.12;
  return { x: cx - w / 2, y: cy - h / 2, w, h };
}

// ---- drawing within a device's means -----------------------------------------------------------

/**
 * What a device that draws slowly gives up, a level at a time: first detail (one column drawn for
 * every 2 x 2 blocks), then frames, and at the last level the flight: one still view a second,
 * a column for every 4 x 4 blocks.
 */
export const FRAME_LEVELS: readonly { everyMs: number; step: number }[] = [
  { everyMs: 33, step: 1 },
  { everyMs: 33, step: 2 },
  { everyMs: 100, step: 2 },
  { everyMs: 1000, step: 4 },
];
/** A drawn frame should be on screen within this. */
export const FRAME_BUDGET_MS = 40;
const BUDGET_WINDOW = 20;
const BUDGET_RECOVER_MS = 20_000;
const BUDGET_RECOVER_MAX_MS = 320_000;

/** How long a changed column glows, and so how long the page remembers that it changed. */
export const RECENT_GLOW_MS = 1200;
/** A tile's geometry is rebuilt at most this often, however fast its deltas, glows and source change. */
export const REBUILD_MIN_MS = 200;

/**
 * The time between the display's ticks, from the spacings of the page's last animation frames:
 * their median, within 4 to 100 ms. Not their least: one callback that came late makes the next
 * spacing short, and a 30 Hz display read as a faster one never keeps up by [`FrameBudget`]'s
 * rule, so it would never step back up. With no spacing yet, 100.
 */
export function displayTick(spacings: readonly number[]): number {
  if (spacings.length === 0) return 100;
  const sorted = [...spacings].sort((a, b) => a - b);
  return Math.max(4, Math.min(100, sorted[sorted.length >> 1]!));
}

/**
 * Slows the drawing down when frames run long, rather than stutter: the page gives it the time
 * each drawn frame took until the GPU had drawn it. When the median of the last 20 is over
 * FRAME_BUDGET_MS, or the last three each took over four times that, it goes down a level
 * (FRAME_LEVELS). It tries one level up after 20 s of frames that keep up with the display (done
 * by the display's next tick, whatever its rate). A recovery that fails doubles the wait before
 * the next (to 320 s), so a device between two levels settles on the lower; one that holds
 * starts the wait over. A software renderer starts at the last level.
 */
export class FrameBudget {
  level: number;
  private costs: number[] = [];
  /** When the window being filled began. */
  private windowAt = 0;
  /** Since when every window has kept up with the display: from the last level change, or from the first window after one that did not. */
  private keptUpSince: number | null = null;
  private wait = BUDGET_RECOVER_MS;
  /** When the last step up was made, until a step down judges it. */
  private triedUpAt: number | null = null;

  /** `slow`: start at the last level and work up (a software renderer), not at the first and down. */
  constructor(slow = false) {
    this.level = slow ? FRAME_LEVELS.length - 1 : 0;
  }

  get everyMs(): number {
    return FRAME_LEVELS[this.level]!.everyMs;
  }

  /** Blocks each drawn column spans each way. */
  get step(): number {
    return FRAME_LEVELS[this.level]!.step;
  }

  /** The camera stands still: the last level. */
  get still(): boolean {
    return this.level === FRAME_LEVELS.length - 1;
  }

  /** `displayMs`: the time between the display's ticks (16.7 at 60 Hz), as the page measured it ([`displayTick`]). */
  sample(costMs: number, nowMs: number, displayMs = 16.7): void {
    if (this.costs.length === 0) this.windowAt = nowMs;
    this.costs.push(costMs);
    const far = this.costs.length >= 3 && this.costs.slice(-3).every((x) => x > FRAME_BUDGET_MS * 4);
    if (this.costs.length < BUDGET_WINDOW && !far) return;
    const sorted = [...this.costs].sort((a, b) => a - b);
    const median = sorted[sorted.length >> 1]!;
    this.costs = [];
    const keepsUp = !far && median <= FRAME_BUDGET_MS && median <= Math.max(FRAME_BUDGET_MS / 2, displayMs * 1.25);
    // Three frames far over the budget step down whatever came before them in the window.
    if ((median > FRAME_BUDGET_MS || far) && this.level < FRAME_LEVELS.length - 1) {
      if (this.triedUpAt !== null) this.wait = nowMs - this.triedUpAt < this.wait ? Math.min(this.wait * 2, BUDGET_RECOVER_MAX_MS) : BUDGET_RECOVER_MS;
      this.triedUpAt = null;
      this.level++;
      this.keptUpSince = nowMs;
      return;
    }
    if (!keepsUp) {
      // A window that missed the display starts the wait again, even when it broke no budget.
      this.keptUpSince = null;
      return;
    }
    this.keptUpSince ??= this.windowAt;
    if (this.level > 0 && nowMs - this.keptUpSince >= this.wait) {
      this.level--;
      this.keptUpSince = nowMs;
      this.triedUpAt = nowMs;
    }
  }
}

/** The factor that holds a canvas of `w` x `h` CSS pixels at `dpr` to `cap` device pixels. */
export function pixelScale(w: number, h: number, dpr: number, cap = 2_500_000): number {
  const px = w * h * dpr * dpr;
  return px <= cap ? dpr : dpr * Math.sqrt(cap / px);
}

/** Whether any of `box` is inside a viewport `vw` x `vh`. */
export function onScreen(box: Box, vw: number, vh: number): boolean {
  return box.x < vw && box.y < vh && box.x + box.w > 0 && box.y + box.h > 0;
}
