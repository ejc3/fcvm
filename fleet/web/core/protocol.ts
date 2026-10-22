// Wire types for the coordinator's /live socket and the renderer's frame socket,
// and parsers that refuse anything that does not match. The coordinator and the
// renderer are the authority on these shapes (fleet/PROTOCOL.md); this file is the
// browser's reading of them.

export type Half = "fcvm" | "ec2";

/** A rectangle of the whole image, in fractions of its width and height (0..1). */
export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** Timings of a tile's last relaunch, in Unix milliseconds. */
export interface LastRelaunch {
  clicked_at: number;
  first_frame_at: number | null;
  /** Milliseconds spent in each phase, in the order the phases ran. */
  phases: Record<string, number>;
}

export interface GpuInfo {
  name: string;
  temp_c: number | null;
  power_w: number | null;
  util_pct: number | null;
}

export interface Tile {
  /** The tile's place, as the coordinator names it (`fcvm:3`). */
  id: string;
  half: Half;
  rect: Rect;
  phase: string;
  /** The slot behind the place (`slot-3`, `ec2-2`), when the coordinator names one. */
  slot: string | null;
  /** The renderer's own frames per second, when reported. */
  fps: number | null;
  error: string | null;
  /** Null while the tile has no renderer to connect to (waiting for capacity). */
  frame_url: string | null;
  gpu: GpuInfo | null;
  host: string | null;
  last_relaunch: LastRelaunch | null;
}

export interface CameraKey {
  /** Scene seconds since scene.epoch_unix_ms. */
  t: number;
  /** The intent that produced this key, when one did. */
  intent?: string;
  [field: string]: number | string | undefined;
}

export interface Word {
  text: string;
  by: string;
}

export interface Light {
  /** Where the coordinator placed it in the world; the page only counts lights. */
  pos: unknown;
  /** A CSS colour, from a string or linear RGB in 0..1. */
  color: string;
  by: string;
}

export interface Scene {
  epoch_unix_ms: number;
  camera: CameraKey[];
  /** Hours, 0..24. */
  time_of_day: number;
  words: Word[];
  lights: Light[];
}

export interface RelaunchAll {
  started_at: number;
  by: string;
  fcvm_done_at: number | null;
  ec2_done_at: number | null;
}

export interface Action {
  at: number;
  by: string;
  name: string;
  /** The intent's arguments, when the coordinator includes them. */
  args: Record<string, unknown> | null;
  /** The coordinator's own one-line summary, when it sends one. */
  summary: string | null;
  outcome: string | null;
}

export interface FleetState {
  tiles: Tile[];
  /** Width over height of the whole image. */
  image_aspect: number;
  fleet: { hosts: number; gpus: number; n: number; max_tiles: number | null };
  scene: Scene;
  settings: { auto_relaunch_secs: number | null };
  relaunch_all: RelaunchAll | null;
  viewers: { count: number };
  actions: Action[];
}

export interface SnapshotMsg {
  type: "snapshot";
  version: number;
  state: FleetState;
}

export interface ReceiptMsg {
  type: "receipt";
  id: string;
  outcome: string;
  reason?: string;
  version: number;
}

export interface ClockMsg {
  type: "clock";
  server_unix_ms: number;
  /** Echo of the ping this answers, when the coordinator sends it. */
  client_unix_ms?: number;
}

export interface ErrorMsg {
  type: "error";
  reason: string;
}

export type ServerMsg = SnapshotMsg | ReceiptMsg | ClockMsg | ErrorMsg;

export const DEFAULT_IMAGE_ASPECT = 16 / 9;

export type IntentName =
  | "relaunch"
  | "relaunch_all"
  | "set_auto_relaunch"
  | "camera"
  | "time_of_day"
  | "word"
  | "light";

export interface IntentMsg {
  type: "intent";
  id: string;
  name: IntentName;
  args: Record<string, unknown>;
}

export interface PingMsg {
  type: "ping";
  client_unix_ms: number;
}

export type ClientMsg = IntentMsg | PingMsg;

/** Outcomes a receipt can carry that mean the intent took effect. */
export function receiptApplied(r: ReceiptMsg): boolean {
  return r.outcome === "committed" || r.outcome === "applied" || r.outcome === "accepted" || r.outcome === "ok";
}

// ---- parsing ---------------------------------------------------------------

export class ProtocolError extends Error {}

type Obj = Record<string, unknown>;

function isObj(v: unknown): v is Obj {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}
function num(o: Obj, k: string, where: string): number {
  const v = o[k];
  if (typeof v !== "number" || !Number.isFinite(v)) throw new ProtocolError(`${where}.${k} is not a number`);
  return v;
}
function numOrNull(o: Obj, k: string, where: string): number | null {
  const v = o[k];
  if (v === null || v === undefined) return null;
  return num(o, k, where);
}
function str(o: Obj, k: string, where: string): string {
  const v = o[k];
  if (typeof v !== "string") throw new ProtocolError(`${where}.${k} is not a string`);
  return v;
}
function strOrNull(o: Obj, k: string, where: string): string | null {
  const v = o[k];
  if (v === null || v === undefined) return null;
  return str(o, k, where);
}
function obj(o: Obj, k: string, where: string): Obj {
  const v = o[k];
  if (!isObj(v)) throw new ProtocolError(`${where}.${k} is not an object`);
  return v;
}
function arr(o: Obj, k: string, where: string): unknown[] {
  const v = o[k];
  if (v === undefined) return [];
  if (!Array.isArray(v)) throw new ProtocolError(`${where}.${k} is not an array`);
  return v;
}

function parseRect(o: Obj, where: string): Rect {
  const r = { x: num(o, "x", where), y: num(o, "y", where), w: num(o, "w", where), h: num(o, "h", where) };
  if (r.w <= 0 || r.h <= 0 || r.x < 0 || r.y < 0 || r.x + r.w > 1.000001 || r.y + r.h > 1.000001) {
    throw new ProtocolError(`${where} is not inside the unit image`);
  }
  return r;
}

/** A reading, or null when absent or negative (NVML's "not available"). */
function reading(o: Obj | null, k: string): number | null {
  const v = o?.[k];
  return typeof v === "number" && Number.isFinite(v) && v >= 0 ? v : null;
}

function parseGpu(v: unknown, stats: unknown, where: string): GpuInfo | null {
  if (v === null || v === undefined) return null;
  const st = isObj(stats) ? stats : null;
  if (typeof v === "string") return { name: v, temp_c: reading(st, "temp_c"), power_w: reading(st, "power_w"), util_pct: reading(st, "util_pct") };
  if (!isObj(v)) throw new ProtocolError(`${where} is neither a name nor an object`);
  return {
    name: str(v, "name", where),
    temp_c: reading(st, "temp_c") ?? reading(v, "temp_c"),
    power_w: reading(st, "power_w") ?? reading(v, "power_w"),
    util_pct: reading(st, "util_pct") ?? reading(v, "util_pct"),
  };
}

function parseLastRelaunch(v: unknown, where: string): LastRelaunch | null {
  if (v === null || v === undefined) return null;
  if (!isObj(v)) throw new ProtocolError(`${where} is not an object`);
  const phases: Record<string, number> = {};
  const p = v["phases"];
  if (isObj(p)) {
    for (const [k, ms] of Object.entries(p)) {
      if (typeof ms === "number" && Number.isFinite(ms)) phases[k] = ms;
    }
  }
  return { clicked_at: num(v, "clicked_at", where), first_frame_at: numOrNull(v, "first_frame_at", where), phases };
}

function parseTile(v: unknown, i: number): Tile {
  const where = `tiles[${i}]`;
  if (!isObj(v)) throw new ProtocolError(`${where} is not an object`);
  const half = str(v, "half", where);
  if (half !== "fcvm" && half !== "ec2") throw new ProtocolError(`${where}.half is ${half}`);
  return {
    id: str(v, "id", where),
    half,
    rect: parseRect(obj(v, "rect", where), `${where}.rect`),
    phase: str(v, "phase", where),
    slot: strOrNull(v, "slot", where),
    fps: numOrNull(v, "fps", where),
    error: strOrNull(v, "error", where),
    frame_url: strOrNull(v, "frame_url", where),
    gpu: parseGpu(v["gpu"], v["gpu_stats"], `${where}.gpu`),
    host: strOrNull(v, "host", where),
    last_relaunch: parseLastRelaunch(v["last_relaunch"], `${where}.last_relaunch`),
  };
}

/** A colour as CSS: a string as given, linear RGB 0..1 converted to sRGB hex. */
export function cssColor(v: unknown): string {
  if (typeof v === "string") return v;
  if (Array.isArray(v) && v.length >= 3 && v.every((x) => typeof x === "number")) {
    const enc = (c: number) => {
      const x = Math.min(1, Math.max(0, c));
      const s = x <= 0.0031308 ? 12.92 * x : 1.055 * x ** (1 / 2.4) - 0.055;
      return Math.round(s * 255).toString(16).padStart(2, "0");
    };
    return `#${enc(v[0] as number)}${enc(v[1] as number)}${enc(v[2] as number)}`;
  }
  return "#ffffff";
}

function parseScene(o: Obj): Scene {
  const camera: CameraKey[] = [];
  for (const [i, k] of arr(o, "camera", "scene").entries()) {
    if (!isObj(k)) throw new ProtocolError(`scene.camera[${i}] is not an object`);
    const key: CameraKey = { t: num(k, "t", `scene.camera[${i}]`) };
    for (const [f, val] of Object.entries(k)) {
      if (typeof val === "number" || typeof val === "string") key[f] = val;
    }
    camera.push(key);
  }
  const words: Word[] = arr(o, "words", "scene").map((w, i) => {
    const where = `scene.words[${i}]`;
    if (!isObj(w)) throw new ProtocolError(`${where} is not an object`);
    return { text: str(w, "text", where), by: strOrNull(w, "by", where) ?? "" };
  });
  const lights: Light[] = arr(o, "lights", "scene").map((l, i) => {
    const where = `scene.lights[${i}]`;
    if (!isObj(l)) throw new ProtocolError(`${where} is not an object`);
    return { pos: l["pos"] ?? null, color: cssColor(l["color"]), by: strOrNull(l, "by", where) ?? "" };
  });
  return {
    epoch_unix_ms: num(o, "epoch_unix_ms", "scene"),
    camera,
    time_of_day: numOrNull(o, "time_of_day", "scene") ?? 12,
    words,
    lights,
  };
}

function parseState(o: Obj): FleetState {
  const tiles = arr(o, "tiles", "state").map(parseTile);
  const ids = new Set<string>();
  for (const t of tiles) {
    if (ids.has(t.id)) throw new ProtocolError(`tile id ${t.id} appears twice`);
    ids.add(t.id);
  }
  const fleet = obj(o, "fleet", "state");
  const settings = isObj(o["settings"]) ? o["settings"] : {};
  const ra = o["relaunch_all"];
  let relaunchAll: RelaunchAll | null = null;
  if (isObj(ra)) {
    relaunchAll = {
      started_at: num(ra, "started_at", "relaunch_all"),
      by: strOrNull(ra, "by", "relaunch_all") ?? "",
      fcvm_done_at: numOrNull(ra, "fcvm_done_at", "relaunch_all"),
      ec2_done_at: numOrNull(ra, "ec2_done_at", "relaunch_all"),
    };
  } else if (ra !== null && ra !== undefined) {
    throw new ProtocolError("relaunch_all is neither an object nor null");
  }
  const viewers = isObj(o["viewers"]) ? o["viewers"] : {};
  const actions: Action[] = arr(o, "actions", "state").flatMap((a) => {
    if (!isObj(a)) return [];
    return [
      {
        at: typeof a["at"] === "number" ? a["at"] : 0,
        by: typeof a["by"] === "string" ? a["by"] : "",
        name: typeof a["name"] === "string" ? a["name"] : "",
        args: isObj(a["args"]) ? a["args"] : null,
        summary: typeof a["summary"] === "string" ? a["summary"] : null,
        outcome: typeof a["outcome"] === "string" ? a["outcome"] : null,
      },
    ];
  });
  const hosts = fleet["hosts"];
  const aspect = numOrNull(o, "image_aspect", "state") ?? DEFAULT_IMAGE_ASPECT;
  if (aspect <= 0) throw new ProtocolError("image_aspect is not positive");
  return {
    tiles,
    image_aspect: aspect,
    fleet: {
      hosts: Array.isArray(hosts) ? hosts.length : num(fleet, "hosts", "fleet"),
      gpus: num(fleet, "gpus", "fleet"),
      n: num(fleet, "n", "fleet"),
      max_tiles: numOrNull(fleet, "max_tiles", "fleet"),
    },
    scene: parseScene(obj(o, "scene", "state")),
    settings: { auto_relaunch_secs: numOrNull(settings, "auto_relaunch_secs", "settings") },
    relaunch_all: relaunchAll,
    viewers: { count: typeof viewers["count"] === "number" ? viewers["count"] : 0 },
    actions,
  };
}

/** Parse one text message from /live. Throws ProtocolError on anything malformed. */
export function parseServerMsg(text: string): ServerMsg {
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch {
    throw new ProtocolError("message is not JSON");
  }
  if (!isObj(v)) throw new ProtocolError("message is not an object");
  switch (v["type"]) {
    case "snapshot":
      return { type: "snapshot", version: num(v, "version", "snapshot"), state: parseState(obj(v, "state", "snapshot")) };
    case "receipt": {
      const r: ReceiptMsg = {
        type: "receipt",
        id: str(v, "id", "receipt"),
        outcome: str(v, "outcome", "receipt"),
        version: num(v, "version", "receipt"),
      };
      const reason = strOrNull(v, "reason", "receipt");
      if (reason !== null) r.reason = reason;
      return r;
    }
    case "clock": {
      const c: ClockMsg = { type: "clock", server_unix_ms: num(v, "server_unix_ms", "clock") };
      const echo = numOrNull(v, "client_unix_ms", "clock");
      if (echo !== null) c.client_unix_ms = echo;
      return c;
    }
    case "error":
      return { type: "error", reason: typeof v["reason"] === "string" ? v["reason"] : "" };
    default:
      throw new ProtocolError(`unknown message type ${String(v["type"])}`);
  }
}
