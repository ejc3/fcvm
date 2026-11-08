// Minecraft mode (`?mode=minecraft`): Pumpkin servers, each a tile the page draws itself from the
// tile's view socket (PROTOCOL.md, "Minecraft tile"), and the take-over panel of the one server
// this viewer holds. Pure: the page measures the stage, passes the time in, feeds the view
// sockets' messages through the parsers below and draws what comes back. The colours are ours:
// one flat colour per material, shaded by height. No texture, sound or other asset of the game.

import type { Box } from "./layout.ts";
import * as fmt from "./format.ts";
import { serverNow } from "./clock.ts";
import { ProtocolError, sameViewer, type Minecraft, type MinecraftTile } from "./protocol.ts";
import type { ClientState, Notice } from "./store.ts";
import { openedArea, tenths } from "./phones.ts";
import { sleepView, verifyView, type SleepView } from "./view.ts";
import { hundredthsText, layoutCaptioned, medianMs, type CaptionSizes } from "./windows.ts";

export const MINECRAFT_TITLE = "Minecraft";
export const MINECRAFT_SENTENCE =
  "Each tile is a Minecraft server in its own virtual machine, restored from one snapshot with its world loaded, drawn from above by your browser. Fork copies a running server into the tile next to it.";
export const NO_MINECRAFT = "This fleet runs no Minecraft servers.";

/** The port Java Edition players connect to. */
export const MINECRAFT_JOIN_PORT = 25565;

/** Room under a tile for its slot, ticks per second, times and buttons. */
export const MINECRAFT_CAPTIONS: CaptionSizes = { full: 104, narrow: 112, compact: 96, fullMinW: 220 };

// ---- materials and colours ----------------------------------------------------------------

/** The tile's materials, by index (fleet/minecraft/src/palette.rs, `MATERIALS`). */
export const MATERIALS = [
  "unknown", "water", "grass", "dirt", "sand", "stone", "gravel", "snow", "ice", "wood",
  "leaves", "lava", "clay", "plant", "planks", "brick", "glass", "path", "mud", "deepslate",
  "netherrack", "red_sand", "flower", "crop", "metal", "glow", "obsidian", "terracotta",
  "moss", "mushroom", "farmland", "cobblestone",
  "white", "orange", "magenta", "light_blue", "yellow", "lime", "pink", "gray",
  "light_gray", "cyan", "purple", "blue", "brown", "green", "red", "black",
] as const;

/** One flat colour per material, in the order of `MATERIALS`: our own choice, read from no game file. */
export const MATERIAL_COLORS: readonly string[] = [
  "#1b2027", // unknown
  "#3a6fd0", // water
  "#5f9e3b", // grass
  "#8a5d3b", // dirt
  "#dccb8f", // sand
  "#8b8e92", // stone
  "#9c958b", // gravel
  "#f2f5f7", // snow
  "#a9d1f1", // ice
  "#6b4a2b", // wood
  "#2f6e2a", // leaves
  "#e2561c", // lava
  "#a2a7b5", // clay
  "#78b24c", // plant
  "#b58b52", // planks
  "#9c4a3c", // brick
  "#c8e6ee", // glass
  "#b39a5c", // path
  "#4e3f36", // mud
  "#4b4b53", // deepslate
  "#7a2e2e", // netherrack
  "#c4682d", // red_sand
  "#e3b23c", // flower
  "#c9b545", // crop
  "#c0c4c9", // metal
  "#f7e08a", // glow
  "#241b33", // obsidian
  "#a05a3e", // terracotta
  "#5a7a2e", // moss
  "#7a5a6e", // mushroom
  "#5b3d24", // farmland
  "#77797c", // cobblestone
  "#f4f4f4", // white
  "#f08a1d", // orange
  "#c34fc0", // magenta
  "#46b6e6", // light_blue
  "#f7d23a", // yellow
  "#7fcc2a", // lime
  "#f39bb8", // pink
  "#4a4f56", // gray
  "#9fa2a7", // light_gray
  "#179aa0", // cyan
  "#7c35b1", // purple
  "#3b44a8", // blue
  "#7a4f2c", // brown
  "#556e1f", // green
  "#b8302a", // red
  "#1c1c21", // black
];

/** The page behind the tiles, and a column the tile has not read yet. */
export const MC_BACKGROUND = "#06080b";
export const MC_UNKNOWN = "#11151a";

/** The wool each bot builds with, by bot number (fleet/minecraft/src/bots.rs, `COLORS`). */
export const BOT_DYES = ["red", "blue", "yellow", "lime", "magenta", "orange", "cyan", "purple"] as const;

const WATER = 1;

function rgbOf(hex: string): [number, number, number] {
  const v = Number.parseInt(hex.slice(1), 16);
  return [(v >> 16) & 255, (v >> 8) & 255, v & 255];
}

const MATERIAL_RGB = MATERIAL_COLORS.map(rgbOf);
const UNKNOWN_RGB = rgbOf(MC_UNKNOWN);

/** The colour of the bot named `name` by its number in the hello's `bot_names`, or white for a person. */
export function playerColor(p: { name: string; bot: boolean }, botNames: readonly string[]): string {
  if (!p.bot) return "#ffffff";
  const i = botNames.indexOf(p.name);
  const dye = BOT_DYES[(i < 0 ? 0 : i) % BOT_DYES.length]!;
  return MATERIAL_COLORS[MATERIALS.indexOf(dye)]!;
}

/**
 * Our material index for each of the tile's (by name): the tile's list is in our order today,
 * and a name we do not know is drawn as unknown rather than as whatever sits at its index.
 */
export function remapOf(materials: readonly string[]): Uint8Array {
  const out = new Uint8Array(256);
  materials.forEach((m, i) => {
    if (i < 256) out[i] = Math.max(0, MATERIALS.indexOf(m as (typeof MATERIALS)[number]));
  });
  return out;
}

// ---- the view socket ------------------------------------------------------------------------

export interface McRegion {
  x0: number;
  z0: number;
  w: number;
  h: number;
}

export interface McPlayer {
  name: string;
  x: number;
  y: number;
  z: number;
  yaw: number;
  bot: boolean;
  driven: boolean;
}

export type ViewMsg =
  | { type: "hello"; tile: string; generation: number; viewer: string | null; materials: string[]; bots: number; bot_names: string[]; driver: string | null }
  | { type: "map"; seq: number; region: McRegion; cells: Uint8Array }
  | { type: "delta"; seq: number; cells: [number, number, number][] }
  | { type: "status"; t: number; tps: number | null; players: McPlayer[]; drive: { seq: number; moved_at: number } | null }
  | { type: "drive_ack"; seq: number; ok: boolean; at: number | null; reason: string | null };

type Obj = Record<string, unknown>;
const isObj = (v: unknown): v is Obj => typeof v === "object" && v !== null && !Array.isArray(v);
const n = (o: Obj, k: string, where: string): number => {
  const v = o[k];
  if (typeof v !== "number" || !Number.isFinite(v)) throw new ProtocolError(`${where}.${k} is not a number`);
  return v;
};
const nOrNull = (o: Obj, k: string, where: string): number | null => (o[k] === null || o[k] === undefined ? null : n(o, k, where));
const s = (o: Obj, k: string, where: string): string => {
  const v = o[k];
  if (typeof v !== "string") throw new ProtocolError(`${where}.${k} is not a string`);
  return v;
};
const sOrNull = (o: Obj, k: string, where: string): string | null => (o[k] === null || o[k] === undefined ? null : s(o, k, where));
const list = (o: Obj, k: string, where: string): unknown[] => {
  const v = o[k];
  if (!Array.isArray(v)) throw new ProtocolError(`${where}.${k} is not an array`);
  return v;
};

const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_INDEX = (() => {
  const t = new Int16Array(128).fill(-1);
  for (let i = 0; i < B64.length; i++) t[B64.charCodeAt(i)] = i;
  return t;
})();

/** Standard base64 (padding optional) to bytes; anything else is a protocol error. */
export function decodeBase64(text: string): Uint8Array {
  const clean = text.replace(/=+$/, "");
  if (clean.length % 4 === 1) throw new ProtocolError("map.cells is not base64");
  const out = new Uint8Array(Math.floor((clean.length * 3) / 4));
  let acc = 0;
  let bits = 0;
  let o = 0;
  for (let i = 0; i < clean.length; i++) {
    const c = clean.charCodeAt(i);
    const v = c < 128 ? B64_INDEX[c]! : -1;
    if (v < 0) throw new ProtocolError("map.cells is not base64");
    acc = (acc << 6) | v;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out[o++] = (acc >> bits) & 255;
    }
  }
  return out;
}

function parsePlayer(v: unknown, where: string): McPlayer {
  if (!isObj(v)) throw new ProtocolError(`${where} is not an object`);
  return {
    name: s(v, "name", where),
    x: n(v, "x", where),
    y: n(v, "y", where),
    z: n(v, "z", where),
    yaw: n(v, "yaw", where),
    bot: v["bot"] === true,
    driven: v["driven"] === true,
  };
}

/** One text message from a tile's view socket; null for a type this page does not know. */
export function parseViewMsg(text: string): ViewMsg | null {
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch {
    throw new ProtocolError("view message is not JSON");
  }
  if (!isObj(v)) throw new ProtocolError("view message is not an object");
  switch (v["type"]) {
    case "hello": {
      const w = "hello";
      const names = list(v, "bot_names", w).filter((x): x is string => typeof x === "string");
      return {
        type: "hello",
        tile: s(v, "tile", w),
        generation: n(v, "generation", w),
        viewer: sOrNull(v, "viewer", w),
        materials: list(v, "materials", w).filter((x): x is string => typeof x === "string"),
        bots: n(v, "bots", w),
        bot_names: names,
        driver: sOrNull(v, "driver", w),
      };
    }
    case "map": {
      const w = "map";
      const r = v["region"];
      if (!isObj(r)) throw new ProtocolError("map.region is not an object");
      const region = { x0: n(r, "x0", "map.region"), z0: n(r, "z0", "map.region"), w: n(r, "w", "map.region"), h: n(r, "h", "map.region") };
      if (!Number.isInteger(region.w) || !Number.isInteger(region.h) || region.w <= 0 || region.h <= 0 || region.w * region.h > 1 << 20) {
        throw new ProtocolError("map.region is not a grid this page draws");
      }
      const cells = decodeBase64(s(v, "cells", w));
      if (cells.length !== region.w * region.h * 2) throw new ProtocolError(`map.cells holds ${cells.length} bytes, not ${region.w * region.h * 2}`);
      return { type: "map", seq: n(v, "seq", w), region, cells };
    }
    case "delta": {
      const w = "delta";
      const cells = list(v, "cells", w).map((c, i) => {
        if (!Array.isArray(c) || c.length !== 3 || c.some((x) => typeof x !== "number" || !Number.isInteger(x) || x < 0)) {
          throw new ProtocolError(`delta.cells[${i}] is not [index, material, height]`);
        }
        return c as [number, number, number];
      });
      return { type: "delta", seq: n(v, "seq", w), cells };
    }
    case "status": {
      const w = "status";
      const d = v["drive"];
      return {
        type: "status",
        t: n(v, "t", w),
        tps: nOrNull(v, "tps", w),
        players: list(v, "players", w).map((p, i) => parsePlayer(p, `status.players[${i}]`)),
        drive: isObj(d) ? { seq: n(d, "seq", "status.drive"), moved_at: n(d, "moved_at", "status.drive") } : null,
      };
    }
    case "drive_ack": {
      const w = "drive_ack";
      return { type: "drive_ack", seq: n(v, "seq", w), ok: v["ok"] === true, at: nOrNull(v, "at", w), reason: sOrNull(v, "reason", w) };
    }
    default:
      return null;
  }
}

/**
 * The first message a view socket sends: the direct token the coordinator's admission gave this
 * page, as the wall's frame sockets send it; with no admission at all (a local run, where the
 * tile has no admission keys) the viewer the coordinator's hello named. Null: nothing to send,
 * so no socket is opened (a late viewer, or before the hello).
 */
export function viewFirstMessage(s: ClientState): string | null {
  const token = s.admission?.direct?.value ?? null;
  if (token !== null) return JSON.stringify({ admit: token });
  if (s.admission === null && s.you !== null) return JSON.stringify({ viewer: s.you });
  return null;
}

// ---- the world a tile draws ---------------------------------------------------------------

/** A tile's columns as its view socket last described them: two bytes each, material and height. */
export interface McWorld {
  region: McRegion;
  cells: Uint8Array;
  seq: number;
}

/** The world after a `map`: the whole grid, replaced. */
export function worldFromMap(m: Extract<ViewMsg, { type: "map" }>): McWorld {
  return { region: { ...m.region }, cells: m.cells.slice(), seq: m.seq };
}

/**
 * Apply a `delta` in place; returns how many columns changed. A delta before any map, or older
 * than what the world holds, changes nothing; an index outside the grid is skipped.
 */
export function applyDelta(world: McWorld | null, d: Extract<ViewMsg, { type: "delta" }>): number {
  if (world === null || d.seq <= world.seq) return 0;
  world.seq = d.seq;
  const len = world.region.w * world.region.h;
  let changed = 0;
  for (const [i, m, h] of d.cells) {
    if (i >= len) continue;
    if (world.cells[i * 2] !== (m & 255) || world.cells[i * 2 + 1] !== (h & 255)) changed++;
    world.cells[i * 2] = m & 255;
    world.cells[i * 2 + 1] = h & 255;
  }
  return changed;
}

/**
 * The world as pixels, one per column (RGBA, row by row: z then x): its material's colour,
 * lighter the higher it stands, with a little relief from the column to its north (lit from the
 * north-west, as a map is). Water is shaded by height alone. A column the tile has not read yet
 * (material 0, height 0) is `MC_UNKNOWN`.
 */
export function shadeWorld(world: McWorld, remap: Uint8Array | null, out?: Uint8ClampedArray): Uint8ClampedArray {
  const { w, h } = world.region;
  const px = out && out.length === w * h * 4 ? out : new Uint8ClampedArray(w * h * 4);
  const cells = world.cells;
  for (let z = 0; z < h; z++) {
    for (let x = 0; x < w; x++) {
      const i = z * w + x;
      const raw = cells[i * 2]!;
      const height = cells[i * 2 + 1]!;
      const o = i * 4;
      if (raw === 0 && height === 0) {
        px[o] = UNKNOWN_RGB[0];
        px[o + 1] = UNKNOWN_RGB[1];
        px[o + 2] = UNKNOWN_RGB[2];
        px[o + 3] = 255;
        continue;
      }
      const m = remap ? remap[raw]! : raw < MATERIAL_RGB.length ? raw : 0;
      const [r, g, b] = MATERIAL_RGB[m] ?? UNKNOWN_RGB;
      let f = shadeOf(height);
      if (m !== WATER) {
        const north = z > 0 ? cells[(i - w) * 2 + 1]! : height;
        const west = x > 0 ? cells[(i - 1) * 2 + 1]! : height;
        const d = height - (north + west) / 2;
        f += Math.max(-3, Math.min(3, d)) * 0.05;
      }
      px[o] = tone(r, f);
      px[o + 1] = tone(g, f);
      px[o + 2] = tone(b, f);
      px[o + 3] = 255;
    }
  }
  return px;
}

/** The brightness of a column at `height` (`y + 64`): 0.8 at sea level (y = 62), lighter above. */
export function shadeOf(height: number): number {
  return Math.max(0.55, Math.min(1.3, 0.8 + (height - 126) * 0.006));
}

function tone(c: number, f: number): number {
  return f <= 1 ? c * f : c + (255 - c) * (f - 1);
}

export interface McMarker {
  name: string;
  /** Centre, in CSS pixels inside the tile's box. */
  x: number;
  y: number;
  r: number;
  color: string;
  /** The end of the facing tick. */
  tx: number;
  ty: number;
  driven: boolean;
  bot: boolean;
  /** Draw the name: there is room for it. */
  label: boolean;
}

/**
 * Where each player stands on a tile `boxW` x `boxH` CSS pixels drawing `region`: x right, z down
 * (north up). Yaw is the game's (0 faces +z, 90 faces -x), so the facing tick points along
 * (-sin yaw, cos yaw). A player outside the region is not drawn.
 */
export function markersOf(region: McRegion, players: readonly McPlayer[], botNames: readonly string[], boxW: number, boxH: number): McMarker[] {
  const r = Math.max(3, Math.min(9, boxW / 48));
  const label = boxW >= 120;
  const out: McMarker[] = [];
  for (const p of players) {
    const fx = (p.x - region.x0) / region.w;
    const fz = (p.z - region.z0) / region.h;
    if (fx < 0 || fx > 1 || fz < 0 || fz > 1) continue;
    const x = fx * boxW;
    const y = fz * boxH;
    const rad = (p.yaw * Math.PI) / 180;
    const len = r * 2.4;
    out.push({ name: p.name, x, y, r, color: playerColor(p, botNames), tx: x - Math.sin(rad) * len, ty: y + Math.cos(rad) * len, driven: p.driven, bot: p.bot, label });
  }
  return out;
}

/** The bot a take-over drives: the one the status marks driven, else the first bot. */
export function drivenPlayer(players: readonly McPlayer[]): McPlayer | null {
  return players.find((p) => p.driven) ?? players.find((p) => p.bot) ?? null;
}

// ---- driving --------------------------------------------------------------------------------

export const DRIVE_KEYS = ["forward", "back", "left", "right", "jump", "sprint"] as const;
export type DriveKey = (typeof DRIVE_KEYS)[number];
export type DriveAction = "place" | "dig";
/** What a key or an on-screen button does. */
export type DriveInput = DriveKey | "turn_left" | "turn_right" | DriveAction;

export interface DriveMsg {
  type: "drive";
  seq: number;
  keys: DriveKey[];
  yaw: number;
  pitch: number;
  action: DriveAction | null;
}

/** While a key is held the page repeats its message this often; the tile drops keys 1 s after the last. */
export const DRIVE_HEARTBEAT_MS = 250;
/** Never two messages closer than this: 25 a second, under the tile's 30. */
export const DRIVE_MIN_GAP_MS = 40;
/** An arrow key turns the bot this fast. */
export const TURN_DEG_PER_S = 135;
/** A mouse or finger drag turns it this much per CSS pixel. */
export const DRAG_DEG_PER_PX = 0.4;
/** Where the bot looks: down ahead, so place and dig reach the block in front of it. */
export const DRIVE_PITCH = 45;
/** A key with no visible change within this long is not measured. */
export const FIRST_CHANGE_GIVE_UP_MS = 5000;

const CODES: Record<string, DriveInput> = {
  KeyW: "forward",
  KeyS: "back",
  KeyA: "left",
  KeyD: "right",
  Space: "jump",
  ShiftLeft: "sprint",
  ShiftRight: "sprint",
  ArrowLeft: "turn_left",
  ArrowRight: "turn_right",
  KeyE: "place",
  KeyQ: "dig",
};

/** The input a key (`KeyboardEvent.code`) drives, or null. */
export function driveInputOf(code: string): DriveInput | null {
  return CODES[code] ?? null;
}

/** The on-screen buttons of a touch layout, in the order they are drawn (a 3-wide grid). */
export const DRIVE_PAD: readonly { input: DriveInput; label: string }[] = [
  { input: "turn_left", label: "Turn left" },
  { input: "forward", label: "Forward" },
  { input: "turn_right", label: "Turn right" },
  { input: "left", label: "Left" },
  { input: "back", label: "Back" },
  { input: "right", label: "Right" },
  { input: "jump", label: "Jump" },
  { input: "sprint", label: "Sprint" },
  { input: "place", label: "Place" },
  { input: "dig", label: "Dig" },
];

const isKey = (i: DriveInput): i is DriveKey => (DRIVE_KEYS as readonly string[]).includes(i);

/** An angle in (-180, 180]. */
export function wrapYaw(deg: number): number {
  let y = deg % 360;
  if (y > 180) y -= 360;
  if (y <= -180) y += 360;
  return y;
}

/**
 * The keys a take-over holds, turned into drive messages, and the time from each key to the
 * first change it made. The page calls `tick` when `nextAt` says (and right after any input);
 * every time comes in as an argument. A message goes on every change, and every
 * `DRIVE_HEARTBEAT_MS` while anything is held, never closer than `DRIVE_MIN_GAP_MS`; an action
 * goes in a message of its own, in order.
 */
export class Driver {
  private held = new Set<DriveKey>();
  private turn = { left: false, right: false, at: 0 };
  private seq = 0;
  private lastSentAt = Number.NEGATIVE_INFINITY;
  private dirty = false;
  private actions: DriveAction[] = [];
  yaw = 0;
  private yawKnown = false;
  /** The driven bot as the last status showed it. */
  private last: McPlayer | null = null;
  private pending: { at: number; kind: "move" | "action"; base: McPlayer | null } | null = null;
  /** Key to first visible change, ms, oldest first. */
  readonly samples: number[] = [];

  /** What is held, in the message's order. */
  keys(): DriveKey[] {
    return DRIVE_KEYS.filter((k) => this.held.has(k));
  }

  private active(): boolean {
    return this.held.size > 0 || this.turn.left || this.turn.right;
  }

  private measure(input: DriveInput, now: number): void {
    if (this.pending !== null && now - this.pending.at < FIRST_CHANGE_GIVE_UP_MS) return;
    if (input === "sprint") return;
    this.pending = { at: now, kind: input === "place" || input === "dig" ? "action" : "move", base: this.last };
  }

  /** A key or button went down. Returns whether it is one the take-over uses. */
  press(input: DriveInput, now: number): boolean {
    if (input === "place" || input === "dig") {
      this.actions.push(input);
      this.measure(input, now);
      return true;
    }
    if (input === "turn_left" || input === "turn_right") {
      const side = input === "turn_left" ? "left" : "right";
      if (this.turn[side]) return true;
      this.integrate(now);
      this.turn[side] = true;
      this.turn.at = now;
      this.measure(input, now);
      this.dirty = true;
      return true;
    }
    if (this.held.has(input)) return true;
    this.held.add(input);
    this.measure(input, now);
    this.dirty = true;
    return true;
  }

  release(input: DriveInput, now: number): void {
    if (input === "turn_left" || input === "turn_right") {
      this.integrate(now);
      this.turn[input === "turn_left" ? "left" : "right"] = false;
      this.dirty = true;
      return;
    }
    if (isKey(input) && this.held.delete(input)) this.dirty = true;
  }

  /** Let go of everything (the window lost focus, the panel closed). */
  releaseAll(now: number): void {
    this.integrate(now);
    if (this.active()) this.dirty = true;
    this.held.clear();
    this.turn.left = false;
    this.turn.right = false;
  }

  /** A drag across the tile: turn by `dx` CSS pixels. */
  drag(dx: number, now: number): void {
    if (dx === 0) return;
    if (this.pending === null) this.measure("turn_right", now);
    this.yaw = wrapYaw(this.yaw + dx * DRAG_DEG_PER_PX);
    this.yawKnown = true;
    this.dirty = true;
  }

  private integrate(now: number): void {
    const dir = (this.turn.right ? 1 : 0) - (this.turn.left ? 1 : 0);
    if (dir !== 0 && now > this.turn.at) {
      this.yaw = wrapYaw(this.yaw + (dir * TURN_DEG_PER_S * (now - this.turn.at)) / 1000);
      this.yawKnown = true;
      this.dirty = true;
    }
    this.turn.at = now;
  }

  /** The message due at `now`, if any. */
  tick(now: number): DriveMsg | null {
    this.integrate(now);
    if (now - this.lastSentAt < DRIVE_MIN_GAP_MS) return null;
    const heartbeat = this.active() && now - this.lastSentAt >= DRIVE_HEARTBEAT_MS;
    if (!this.dirty && this.actions.length === 0 && !heartbeat) return null;
    this.lastSentAt = now;
    this.dirty = false;
    const action = this.actions.shift() ?? null;
    return { type: "drive", seq: ++this.seq, keys: this.keys(), yaw: Math.round(this.yaw * 10) / 10, pitch: DRIVE_PITCH, action };
  }

  /** When `tick` should run next, or null while nothing is held or waiting. */
  nextAt(now: number): number | null {
    const soonest = this.lastSentAt + DRIVE_MIN_GAP_MS;
    if (this.dirty || this.actions.length > 0 || this.turn.left || this.turn.right) return Math.max(now, soonest);
    if (this.held.size > 0) return Math.max(now, this.lastSentAt + DRIVE_HEARTBEAT_MS);
    return null;
  }

  /**
   * A status message arrived at `now`: learn the bot's facing once, and finish a measurement
   * when the bot moved or turned since the key.
   */
  observeStatus(players: readonly McPlayer[], now: number): number | null {
    const p = drivenPlayer(players);
    if (p === null) return null;
    if (!this.yawKnown) {
      this.yaw = wrapYaw(p.yaw);
      this.yawKnown = true;
    }
    this.last = p;
    const m = this.pending;
    if (m === null) return null;
    if (now - m.at > FIRST_CHANGE_GIVE_UP_MS) {
      this.pending = null;
      return null;
    }
    if (m.kind !== "move") return null;
    if (m.base === null) {
      m.base = p;
      return null;
    }
    const b = m.base;
    const moved = Math.hypot(p.x - b.x, p.z - b.z) > 0.05 || Math.abs(p.y - b.y) > 0.05 || Math.abs(wrapYaw(p.yaw - b.yaw)) > 0.5;
    if (!moved) return null;
    return this.finish(now);
  }

  /** A delta arrived at `now`: it is the first change of a place or a dig. */
  observeDelta(now: number): number | null {
    const m = this.pending;
    if (m === null || m.kind !== "action") return null;
    if (now - m.at > FIRST_CHANGE_GIVE_UP_MS) {
      this.pending = null;
      return null;
    }
    return this.finish(now);
  }

  private finish(now: number): number {
    const ms = Math.max(0, now - this.pending!.at);
    this.pending = null;
    this.samples.push(ms);
    if (this.samples.length > 200) this.samples.shift();
    return ms;
  }
}

// ---- refusals -------------------------------------------------------------------------------

const MINECRAFT_INTENTS = new Set(["minecraft_reset", "minecraft_fork", "claim_minecraft", "release_minecraft"]);

/** The newest refusal of a Minecraft intent still on screen, as the reason the coordinator gave. */
export function minecraftNotice(notices: readonly Notice[]): string | null {
  for (let i = notices.length - 1; i >= 0; i--) {
    const x = notices[i]!;
    if (x.kind === "refused" && MINECRAFT_INTENTS.has(x.intent)) return x.reason ?? "The coordinator refused it.";
  }
  return null;
}

// ---- words ----------------------------------------------------------------------------------

/** "mc-3" for the server on slot `mc-3`. */
export function serverName(t: { id: string; slot: string | null }): string {
  return t.slot ?? t.id;
}

const TRANSIENT = new Set(["restoring", "forking", "relaunching", "pending", "requested", "starting", "stopping"]);

export function isMcRestoring(t: { phase: string }): boolean {
  return TRANSIENT.has(t.phase);
}

/** "20.0 TPS", from the tile's own status; "-- TPS" before one. */
export function tpsText(tps: number | null): string {
  return tps === null ? "-- TPS" : `${tps.toFixed(1)} TPS`;
}

/** The click to the first chunk of the server's last launch, ms, or null before one. */
export function upMs(t: MinecraftTile): number | null {
  const lr = t.last_relaunch;
  return lr !== null && lr.first_frame_at !== null ? Math.max(0, lr.first_frame_at - lr.clicked_at) : null;
}

/** "Up in 1.90 s". */
export function upInText(t: MinecraftTile): string | null {
  const ms = upMs(t);
  return ms === null ? null : `Up in ${hundredthsText(ms)}`;
}

/** "Forked from mc-1 in 2.30 s" for a launch that is a copy of another server's running world. */
export function forkedFromText(t: MinecraftTile): string | null {
  if (t.forked_from === null) return null;
  const ms = upMs(t);
  return ms === null ? `Forked from ${t.forked_from}` : `Forked from ${t.forked_from} in ${hundredthsText(ms)}`;
}

/** "snapshot 0.64 s, restore 1.10 s": a fork's split, from the agent's timings. */
export function forkSplitText(snapshotMs: number | null | undefined, restoreMs: number | null | undefined): string | null {
  if (snapshotMs === null || snapshotMs === undefined || restoreMs === null || restoreMs === undefined) return null;
  return `snapshot ${hundredthsText(snapshotMs)}, restore ${hundredthsText(restoreMs)}`;
}

/** The dark tile's running timer from the click: "Restoring 1.4 s", "Forking 0.6 s". */
export function mcTimerText(t: MinecraftTile, srvNow: number): string {
  const word = t.phase === "forking" ? "Forking" : "Restoring";
  const lr = t.last_relaunch;
  return lr === null ? word : `${word} ${tenths(srvNow - lr.clicked_at)}`;
}

export function mcFailedText(t: MinecraftTile): string {
  return `Failed: ${t.error ?? t.failure?.message ?? "the server did not come back"}`;
}

/** Each server's last time to first chunk, ms. */
export function upTimes(tiles: readonly MinecraftTile[]): number[] {
  const out: number[] = [];
  for (const t of tiles) {
    const ms = upMs(t);
    if (ms !== null) out.push(ms);
  }
  return out;
}

/** The version Java Edition players need, from the server's name (`... (Java Edition 26.3)`). */
export function javaVersion(server: string | null): string | null {
  return server === null ? null : (/Java Edition ([0-9][0-9.]*)/.exec(server)?.[1] ?? null);
}

/** The server a fork of `src` would copy into: the next on its host, in index order and wrapping, running and not lent. */
export function forkTarget(tiles: readonly MinecraftTile[], src: MinecraftTile): MinecraftTile | null {
  const idx = (t: MinecraftTile) => Number(/(\d+)$/.exec(t.slot ?? t.id)?.[1] ?? 0);
  const peers = tiles.filter((t) => t.host === src.host).sort((a, b) => idx(a) - idx(b));
  const at = peers.indexOf(src);
  for (let k = 1; k < peers.length; k++) {
    const t = peers[(at + k) % peers.length]!;
    if (t !== src && t.phase === "running" && t.borrowed === null) return t;
  }
  return null;
}

// ---- the view -------------------------------------------------------------------------------

/** What the page knows of a tile from its own view socket. */
export interface McTileLive {
  open: boolean;
  /** The viewer the tile's hello says this socket acts as. */
  viewer: string | null;
  tps: number | null;
  hasMap: boolean;
}

/** The take-over's own figures: the time from each key to its first change, and the tile's last refusal. */
export interface McDriveLive {
  samples: readonly number[];
  refused: string | null;
}

export interface MinecraftCard {
  id: string;
  slot: string;
  tps: string;
  upIn: string | null;
  forkedFrom: string | null;
  forkSplit: string | null;
  /** "mc-3.<zone>:25565", for Java Edition players. */
  join: string | null;
  timer: string | null;
  error: string | null;
  box: Box | null;
  caption: Box | null;
  dark: boolean;
  compact: boolean;
  /** Whether the caption draws its buttons (a compact caption leaves them out while the panel is open). */
  showActions: boolean;
  /** The tile's view socket and its first message; null: no socket now. `key` changes when a new socket is needed. */
  socket: { url: string; first: string; key: string } | null;
  reset: { label: string; disabled: boolean; title: string | null };
  fork: { label: string; disabled: boolean; title: string | null };
  take: { label: string; disabled: boolean; title: string | null };
  lentText: string | null;
  mine: boolean;
  lent: boolean;
  ariaLabel: string;
}

export interface MinecraftHeader {
  up: string;
  server: string | null;
  median: string;
  slowest: string;
  lastFork: string;
  lastResetAll: string;
  /** For Java Edition players: the address of a tile and the port, when tiles have one. */
  join: string | null;
}

export interface MinecraftPanel {
  id: string;
  slot: string;
  session: string;
  countdown: string;
  /** What each key does. */
  keys: string;
  /** "Key to first change: last 85 ms, median 92 ms (6 keys)". */
  firstChange: string;
  /** Whether keys reach the bot now: the tile's socket is open and acts as the borrow's viewer. */
  driving: boolean;
  /** Why keys do not reach the bot, while they do not. */
  waiting: string | null;
  /** The tile's last refusal of a drive message. */
  refused: string | null;
  /** On-screen buttons, on a touch layout. */
  pad: { input: DriveInput; label: string }[] | null;
  giveBack: string;
}

export interface MinecraftInput {
  state: ClientState;
  /** Local Unix ms. */
  now: number;
  dpr: number;
  stage: Box | null;
  narrow: boolean;
  /** A touch layout: the panel gets on-screen buttons. */
  touch: boolean;
  /** Per tile id. */
  live: Readonly<Record<string, McTileLive>>;
  drive: McDriveLive | null;
}

export interface MinecraftView {
  title: string;
  sentence: string;
  online: boolean;
  empty: string | null;
  banner: string | null;
  header: MinecraftHeader | null;
  resetAll: { label: string; disabled: boolean };
  cards: MinecraftCard[];
  panel: MinecraftPanel | null;
  verify: ReturnType<typeof verifyView>;
  notice: string | null;
  sleep: SleepView | null;
}

function who(by: string, you: string | null): string {
  if (sameViewer(by, you)) return "you";
  return by.startsWith("viewer:") ? "another viewer" : by.replace(/_/g, " ");
}

export const DRIVE_KEYS_TEXT = "W A S D walk, the arrow keys or a drag across the tile turn, space jumps, shift sprints, E places a block and Q breaks one. Escape gives it back.";
export const DRIVE_PAD_TEXT = "Hold a button to walk or turn, or drag across the tile to turn. Place and Break act on the block in front of the bot.";

function firstChangeText(samples: readonly number[]): string {
  if (samples.length === 0) return "Press a key: the time to the first change it makes shows here.";
  const last = samples[samples.length - 1]!;
  const med = medianMs(samples)!;
  return `Key to first change: last ${Math.round(last)} ms, median ${Math.round(med)} ms (${fmt.count(samples.length, "key", "keys")})`;
}

function headerOf(m: Minecraft, you: string | null): MinecraftHeader {
  const times = upTimes(m.tiles);
  const median = medianMs(times);
  const up = m.tiles.filter((t) => t.phase === "running").length;
  const servers = m.tiles.length === 1 ? "server" : "servers";
  const f = m.last_fork;
  let lastFork = "Fork: not used yet";
  if (f !== null) {
    const total = f.total_ms ?? (f.first_chunk_at === null ? null : f.first_chunk_at - f.clicked_at);
    const split = forkSplitText(f.snapshot_ms, f.restore_ms);
    lastFork = `Last fork: ${f.from} to ${f.to}${total === null ? "" : ` in ${hundredthsText(total)}`}${split ? ` (${split})` : ""}`;
  }
  const joinable = m.tiles.find((t) => t.join !== null && t.phase === "running") ?? m.tiles.find((t) => t.join !== null);
  const version = javaVersion(m.server);
  return {
    up: `${up} of ${m.tiles.length} ${servers} up`,
    server: m.server,
    median: `Median restore to first chunk ${median === null ? "--" : hundredthsText(median)}`,
    slowest: `Slowest ${times.length === 0 ? "--" : hundredthsText(Math.max(...times))}`,
    lastFork,
    lastResetAll:
      m.last_reset_all === null
        ? "Reset all: not used yet"
        : `Last reset all: ${fmt.utcClock(m.last_reset_all.clicked_at)} UTC, ${fmt.count(m.last_reset_all.servers, "server", "servers")}, by ${who(m.last_reset_all.by, you)}`,
    join:
      joinable === undefined
        ? null
        : `Join from Minecraft: Java Edition${version ? ` ${version}` : ""} at ${joinable.join}, port ${MINECRAFT_JOIN_PORT}. Every tile has its own address, under it.`,
  };
}

function seatsFull(reason: string | null, seatFree: boolean): string {
  const why = reason ?? "Direct streams are full.";
  return seatFree
    ? `${why} A direct seat is free now: reload the page to take it and see the servers.`
    : `${why} The servers' worlds go only to viewers with a direct seat, so the tiles stay dark on this page until one is free.`;
}

export function minecraftView(inp: MinecraftInput): MinecraftView {
  const s = inp.state;
  const m = s.accepted?.minecraft ?? null;
  const online = s.connection === "open";
  const srvNow = serverNow(s.clock, inp.now);
  const common = {
    title: MINECRAFT_TITLE,
    sentence: MINECRAFT_SENTENCE,
    online,
    verify: verifyView(s),
    notice: minecraftNotice(s.notices),
    sleep: s.accepted === null ? null : sleepView(s.accepted, srvNow, online, s.pending.some((p) => p.intent.name === "wake")),
  };
  const banner =
    s.admission?.locked === true
      ? seatsFull(s.admission.reason ?? null, s.admission.seat_free)
      : s.accepted !== null && !online
        ? "The coordinator is offline. The worlds still come straight from the servers; the controls are paused until it is back."
        : null;
  if (m === null || s.accepted === null) {
    return {
      ...common,
      empty: s.accepted === null ? (s.connection === "closed" ? "Cannot reach the fleet yet. Trying again." : "Connecting to the fleet.") : NO_MINECRAFT,
      banner,
      header: null,
      resetAll: { label: "Reset all", disabled: true },
      cards: [],
      panel: null,
    };
  }
  const mine = (t: MinecraftTile) => t.borrowed !== null && sameViewer(t.borrowed.viewer ?? null, s.you);
  const held = m.tiles.find((t) => mine(t) && t.borrowed?.session !== undefined) ?? null;
  const panelOpen = held !== null;
  const area = inp.stage === null ? null : panelOpen ? openedArea(inp.stage, inp.narrow) : inp.stage;
  const placed = new Map(
    area === null ? [] : layoutCaptioned({ area, tiles: m.tiles, imageAspect: m.image_aspect, narrow: inp.narrow, panel: panelOpen, dpr: inp.dpr }, MINECRAFT_CAPTIONS).map((b) => [b.id, b]),
  );
  const first = viewFirstMessage(s);
  const cards: MinecraftCard[] = m.tiles.map((t) => {
    const p = placed.get(t.id);
    const slot = serverName(t);
    const running = t.phase === "running";
    const lentToOther = t.borrowed !== null && !mine(t);
    const live = inp.live[t.id];
    let takeTitle: string | null = null;
    if (held !== null && !mine(t)) takeTitle = `You are driving ${serverName(held)}. Give it back to take another.`;
    else if (lentToOther) takeTitle = "Lent to another viewer.";
    else if (!running) takeTitle = "It can be borrowed once it is running.";
    const target = running ? forkTarget(m.tiles, t) : null;
    const forkTitle = !running ? "It can be copied once it is running." : target === null ? "No other server on its host can take a copy now." : `Copies its world into ${serverName(target)}.`;
    const resetTitle = mine(t) ? "Yours: give it back and it is reset." : lentToOther ? "Lent to another viewer." : null;
    const lastFork = m.last_fork !== null && m.last_fork.to === slot ? m.last_fork : null;
    const phases = t.last_relaunch?.phases ?? {};
    const split = t.forked_from === null ? null : forkSplitText(lastFork?.snapshot_ms ?? phases["snapshot_ms"], lastFork?.restore_ms ?? phases["restore_ms"]);
    const url = running ? t.frame_url : null;
    const socket = url !== null && first !== null ? { url, first, key: `${url}#${t.generation ?? 0}` } : null;
    const state = running ? "running" : t.phase === "failed" ? "failed" : t.phase === "forking" ? "forking" : "restoring";
    return {
      id: t.id,
      slot,
      tps: tpsText(running ? (live?.tps ?? null) : null),
      upIn: upInText(t),
      forkedFrom: forkedFromText(t),
      forkSplit: split,
      join: t.join === null ? null : `${t.join}:${MINECRAFT_JOIN_PORT}`,
      timer: isMcRestoring(t) ? mcTimerText(t, srvNow) : null,
      error: t.phase === "failed" ? mcFailedText(t) : null,
      box: p?.box ?? null,
      caption: p?.caption ?? null,
      dark: !running,
      compact: p?.compact ?? false,
      showActions: !(p?.compact === true && panelOpen),
      socket,
      reset: { label: "Reset", disabled: !online || t.borrowed !== null, title: resetTitle },
      fork: { label: "Fork", disabled: !online || !running || target === null, title: forkTitle },
      take: { label: "Take over", disabled: !online || !running || t.borrowed !== null || held !== null, title: takeTitle },
      lentText: mine(t) ? "Yours" : lentToOther ? `Lent to another viewer until ${fmt.utcClock(t.borrowed!.until)} UTC` : null,
      mine: mine(t),
      lent: lentToOther,
      ariaLabel: `${slot}, ${state}${mine(t) ? ", yours to drive" : lentToOther ? ", lent" : ""}`,
    };
  });
  let panel: MinecraftPanel | null = null;
  if (held !== null) {
    const b = held.borrowed!;
    const left = Math.max(0, Math.ceil((b.until - srvNow) / 1000));
    const live = inp.live[held.id];
    const driving = held.phase === "running" && live?.open === true && sameViewer(live.viewer, b.viewer ?? null);
    const waiting = driving
      ? null
      : held.phase !== "running"
        ? `${serverName(held)} is ${held.phase === "forking" ? "taking a copy" : "restoring"}; the keys reach its bot once it is running.`
        : live?.open === true
          ? "This page's view of the server acts as another viewer, so it cannot drive."
          : "Connecting to the server.";
    panel = {
      id: held.id,
      slot: serverName(held),
      session: b.session!,
      countdown: `Returns in ${left} s`,
      keys: inp.touch ? DRIVE_PAD_TEXT : DRIVE_KEYS_TEXT,
      firstChange: firstChangeText(inp.drive?.samples ?? []),
      driving,
      waiting,
      refused: inp.drive?.refused ?? null,
      pad: inp.touch ? DRIVE_PAD.map((x) => ({ ...x, label: x.input === "dig" ? "Break" : x.label })) : null,
      giveBack: "Give back",
    };
  }
  return {
    ...common,
    empty: null,
    banner,
    header: headerOf(m, s.you),
    resetAll: { label: "Reset all", disabled: !online || m.tiles.length === 0 },
    cards,
    panel,
  };
}
