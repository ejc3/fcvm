// Wire types for the coordinator's /live socket and the renderer's frame socket,
// and parsers that refuse anything that does not match. The coordinator and the
// renderer are the authority on these shapes (fleet/PROTOCOL.md); this file is the
// browser's reading of them. core/test/contract.test.ts parses the coordinator's own
// snapshot states (#1146, fleet/core/tests/vectors) with it.

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

/** Why a tile is not running when that is a fault: `relaunch_failed` (the VM did not come back; the agent retries at `retry_at`). */
export interface Failure {
  kind: string;
  message: string;
  retry_at: number | null;
}

export interface Tile {
  /** The tile's place, as the coordinator names it (`fcvm:3`). */
  id: string;
  half: Half;
  rect: Rect;
  phase: string;
  /** The slot behind the place (`slot-3`, `ec2-2`), when the coordinator names one. */
  slot: string | null;
  /** The VM's name (`fleet-slot-3`), when it has one. */
  vm: string | null;
  /** How many times this slot has been started. */
  generation: number | null;
  /** The renderer's own frames per second, when reported. */
  fps: number | null;
  error: string | null;
  failure: Failure | null;
  /**
   * The renderer's frame socket over IPv6 (`wss://<label>.<zone>/frames`). Null while the
   * tile has no renderer to connect to, and until the coordinator has published its DNS.
   */
  frame_url: string | null;
  /** The same renderer over IPv4 (`wss://<label>-v4.<zone>[:port]/frames`), null alike. */
  frame_url_v4: string | null;
  gpu: GpuInfo | null;
  host: string | null;
  /** Where the tile runs, worded for a viewer (`EC2 in us-east-1`); null until the slot reports it. */
  place: string | null;
  last_relaunch: LastRelaunch | null;
  /**
   * Lent to a visitor as their own GPU until this time; it sits out races meanwhile. A phone
   * also names the borrow: its `session` (what `phone_action` and `release_phone` pass back as
   * `claim`), the `viewer` holding it and when it was taken.
   */
  borrowed: { until: number; session?: string; viewer?: string; since?: number } | null;
}

export interface CameraKey {
  /** Scene seconds since scene.epoch_unix_ms. */
  t: number;
  /** The intent that produced this key, when one did. */
  intent?: string;
  /** `pos`, `look` ([x, altitude, z] km), `fov_deg`, or a mock's own fields. */
  [field: string]: number | string | number[] | undefined;
}

export interface Word {
  text: string;
}

export interface Light {
  /** Where the coordinator placed it in the world; the page only counts lights. */
  pos: unknown;
  /** A CSS colour, from a string or linear RGB in 0..1. */
  color: string;
}

export interface Scene {
  epoch_unix_ms: number;
  camera: CameraKey[];
  /** Fraction of the day, 0..1 (0.25 sunrise, 0.5 noon, 0.75 sunset). */
  time_of_day: number;
  words: Word[];
  lights: Light[];
}

export interface RelaunchAll {
  started_at: number;
  /** The viewer that pressed restart, or `schedule` for the owner's cadence. */
  by: string;
  fcvm_done_at: number | null;
  ec2_done_at: number | null;
  /** Tiles still to reach their first frame. */
  fcvm_pending: number | null;
  ec2_pending: number | null;
  /** GPUs restarted in each half. */
  fcvm_gpus: number | null;
  ec2_gpus: number | null;
  /** Time the EC2 half has waited for spot capacity so far, not counted in its time. */
  ec2_capacity_wait_ms: number;
  /** When the EC2 half's current wait for capacity began, or null. */
  ec2_waiting_since: number | null;
  /** Each half's measured time once it is back (the coordinator's, capacity waits excluded per tile), or null. */
  fcvm_ms: number | null;
  ec2_ms: number | null;
}

/** One half's measured restart times. Every number is the coordinator's. */
export interface HalfScore {
  /** Median click-to-last-first-frame over the races kept, capacity waits excluded. */
  median_ms: number;
  /** The last race's time, capacity wait excluded. */
  last_ms: number;
  /** Races the median is over. */
  races: number;
  /** GPUs the last race restarted in this half. */
  measured_on: number;
  /** How long the last race waited for spot capacity (EC2), not counted above. */
  last_capacity_wait_ms: number;
}

export interface Scoreboard {
  fcvm: HalfScore | null;
  ec2: HalfScore | null;
}

/** A stop the open vote is among, with its votes. */
export interface NudgeTarget {
  id: string;
  label: string;
  votes: number;
  /** When the camera gets there if it wins the open vote. */
  arrive_at: number | null;
}

/**
 * The directed camera. The vote is always open, for the first hold not yet decided: `next`
 * while it is null, else the hold after it.
 */
export interface Nudges {
  targets: NudgeTarget[];
  /** The target the camera holds on now, or null while it flies, and its label. */
  holding: string | null;
  holding_label: string | null;
  /** When the camera leaves for the next hold. */
  next_hold_at: number | null;
  /** The next hold once decided, its label, and when the camera gets there. */
  next: string | null;
  next_label: string | null;
  arrive_next_at: number | null;
  /** The target that wins if the open vote closed now. */
  leading: string | null;
  /** When the open vote opened and when it closes. */
  opened_at: number | null;
  closes_at: number | null;
}

export interface Host {
  name: string;
  draining: boolean;
}

/** The steps a host goes through when the fleet wakes, in order. */
export const WAKE_PHASES = ["instance_pending", "booted", "store_ready", "agent_connected", "tiles_rendering"] as const;

/** One host's progress through a wake: each phase it has reached, with when. */
export interface HostWake {
  host: string;
  phases: { phase: string; at: number }[];
}

/** The fleet scales to zero with no viewers and wakes when one arrives (`fleet.power`). */
export interface Power {
  state: "asleep" | "waking" | "awake";
  asleep_since: number | null;
  woke_at: number | null;
  hosts: HostWake[];
  /** The last completed wake: when it began, when every GPU was rendering, how many GPUs. */
  last_wake: Wake | null;
  /** The last completed wakes, oldest first (the coordinator keeps 20). */
  wakes: Wake[];
}

export interface Wake {
  started_at: number;
  awake_at: number;
  gpus: number;
}

/** One half of a finished or running race, as `races[]` keeps it. */
export interface RaceHalf {
  done_at: number | null;
  /** Click to the half's last first frame, capacity waits excluded. */
  time_ms: number | null;
  capacity_wait_ms: number;
}

/** A race the coordinator kept (`races`, the last 10). */
export interface RaceRecord {
  id: number;
  started_at: number;
  by: string;
  fcvm: RaceHalf | null;
  ec2: RaceHalf | null;
}

function parseWake(v: unknown, where: string): Wake | null {
  if (!isObj(v)) return null;
  return { started_at: num(v, "started_at", where), awake_at: num(v, "awake_at", where), gpus: num(v, "gpus", where) };
}

function parseRaceHalf(v: unknown, where: string): RaceHalf | null {
  if (!isObj(v)) return null;
  return {
    done_at: numOrNull(v, "done_at", where),
    time_ms: numOrNull(v, "time_ms", where),
    capacity_wait_ms: numOrNull(v, "capacity_wait_ms", where) ?? 0,
  };
}

function parseRaces(v: unknown): RaceRecord[] {
  if (!Array.isArray(v)) return [];
  return v.flatMap((r, i) => {
    const where = `races[${i}]`;
    if (!isObj(r)) return [];
    return [
      {
        id: num(r, "id", where),
        started_at: num(r, "started_at", where),
        by: strOrNull(r, "by", where) ?? "",
        fcvm: parseRaceHalf(r["fcvm"], `${where}.fcvm`),
        ec2: parseRaceHalf(r["ec2"], `${where}.ec2`),
      },
    ];
  });
}

function parsePower(v: unknown): Power {
  // A coordinator that does not scale to zero sends nothing: always awake.
  if (!isObj(v)) return { state: "awake", asleep_since: null, woke_at: null, hosts: [], last_wake: null, wakes: [] };
  const w = "fleet.power";
  const state = str(v, "state", w);
  if (state !== "asleep" && state !== "waking" && state !== "awake") throw new ProtocolError(`${w}.state is ${state}`);
  const lw = v["last_wake"];
  return {
    state,
    asleep_since: numOrNull(v, "asleep_since", w),
    woke_at: numOrNull(v, "woke_at", w),
    hosts: arr(v, "hosts", w).flatMap((h) =>
      isObj(h) && typeof h["host"] === "string"
        ? [
            {
              host: h["host"],
              phases: arr(h, "phases", `${w}.hosts`).flatMap((p) =>
                isObj(p) && typeof p["phase"] === "string" && typeof p["at"] === "number" ? [{ phase: p["phase"], at: p["at"] }] : [],
              ),
            },
          ]
        : [],
    ),
    last_wake: parseWake(lw, `${w}.last_wake`),
    wakes: arr(v, "wakes", w).flatMap((x, i) => {
      const wk = parseWake(x, `${w}.wakes[${i}]`);
      return wk ? [wk] : [];
    }),
  };
}

/** Wall GPUs lent to visitors: at most `cap` at once; the next comes back at `next_return_at`. */
export interface Borrow {
  cap: number;
  lent: number;
  next_return_at: number | null;
}

/** Running totals the coordinator keeps: GPU VMs restored, the last hour's, GPU memory scrubs agents reported. */
export interface Counters {
  restores: number;
  restores_last_hour: number;
  scrubs: number;
}

/**
 * What is wrong beyond one tile: `host_reclaimed` (a spot notice, or the host's agent
 * went away; its tiles have already left the tiling), `coordinator_degraded`,
 * `gpu_quarantined`. Each carries what its kind names; the page reads these.
 */
export interface FleetFailure {
  kind: string;
  host: string | null;
  since: number | null;
}

export interface Action {
  at: number;
  /** The viewer id of the connection that acted (`schedule` for the owner's cadence). */
  viewer: string | null;
  /** The intent's id. */
  id: string | null;
  name: string;
  /** The intent's arguments, when the coordinator includes them. */
  args: Record<string, unknown> | null;
  /** The coordinator's own one-line summary. */
  summary: string | null;
  outcome: string | null;
}

/** A region one aggregator serves: its tiles joined at one t, in the renderer's frame protocol. */
export interface Aggregate {
  id: string;
  half: Half;
  host: string | null;
  rect: Rect;
  /** The host's aggregator over IPv6, and over IPv4: null while it is not serving browsers. */
  frame_url: string | null;
  frame_url_v4: string | null;
  tiles: string[];
}

/**
 * Which tiling of the image this is: a new generation each time GPUs join or leave
 * and the image is re-tiled, and the frame number (on the renderers' t = k/30 grid)
 * from which the renderers draw the new rectangles. Frames before it are drawn with
 * the old ones.
 */
export interface Layout {
  generation: number;
  from_frame: number;
}

/** Renderer frames per second: frame k is drawn at t = k / FRAME_RATE. */
export const FRAME_RATE = 30;

/**
 * Phones mode (PROTOCOL.md, "Phones mode"): phones laid out in an image of their own, never
 * part of the wall. A phone is a Tile, with the borrow named in `borrowed`.
 */
export interface Phones {
  /** Width over height of the phones' image: one row of up to six phones, then rows of equal length. */
  image_aspect: number;
  /** How long a borrow lasts, seconds. */
  claim_secs: number;
  borrow: Borrow;
  /** The take-over actions the coordinator accepts, by name. */
  actions: string[];
  /** Zoomed out for every viewer: the phones show their app's own interface, apart in bezels. */
  zoomed_out: boolean;
  /** Unix ms at which the current zoom state took effect, or null before the first change. */
  zoom_at: number | null;
  tiles: Tile[];
}

export interface FleetState {
  layout: Layout;
  tiles: Tile[];
  /** Width over height of the whole image. */
  image_aspect: number;
  fleet: { hosts: Host[]; gpus: number; n: number; max_tiles: number | null; power: Power };
  borrow: Borrow | null;
  counters: Counters | null;
  failures: FleetFailure[];
  scene: Scene;
  relaunch_all: RelaunchAll | null;
  /** The last races the coordinator kept, oldest first. */
  races: RaceRecord[];
  /** When the owner's cadence restarts everything next. */
  next_race_at: number | null;
  /** A viewer's `restart_now` is refused until then. */
  cooldown_until: number | null;
  scoreboard: Scoreboard;
  nudges: Nudges | null;
  /** Connections, and those whose tab is visible (`active`): the page shows `active`. */
  viewers: { count: number; active: number };
  actions: Action[];
  aggregates: Aggregate[];
  /** Null when the fleet runs no phones. */
  phones: Phones | null;
}

export interface SnapshotMsg {
  type: "snapshot";
  version: number;
  state: FleetState;
}

/** The first message on a connection: who it acts as, and the Turnstile site key when actions need one. */
export interface HelloMsg {
  type: "hello";
  viewer: string;
  turnstile: string | null;
}

/** The answer to a `verify`. */
export interface VerifiedMsg {
  type: "verified";
  ok: boolean;
  reason: string | null;
}

/** A viewer's own GPU, borrowed from a wall tile, sent only to the connection that claimed it. */
export interface Claim {
  id: string;
  /** The wall tile whose GPU it borrowed. */
  tile: string | null;
  /** `restoring`, `running`, or `ended`. */
  phase: string;
  /** The private VM's frames over IPv6, and over IPv4. */
  frame_url: string | null;
  frame_url_v4: string | null;
  clicked_at: number;
  first_frame_at: number | null;
  /** When the GPU goes back to the pool. */
  expires_at: number | null;
  gpu: string | null;
  phases: Record<string, number>;
  camera: CameraKey[];
  /** The camera flies the coordinator's autopilot loop (true) or is the viewer's (false). */
  autopilot: boolean;
  /** Why it ended, once it has. */
  ended: string | null;
}

export interface ClaimMsg {
  type: "claim";
  claim: Claim;
}

/** A credential and when it stops working (unix ms). */
export interface Token {
  value: string;
  expires_at: number;
}

/**
 * How this connection receives the image (PROTOCOL.md, "admission"): a seat (direct streams,
 * and the broadcast if the viewer switches) or, for a viewer that came when every seat was
 * held, the broadcast alone.
 */
export interface Admission {
  /** The seat ticket, to present as /live?seat= after a reconnect; null when late. */
  seat: string | null;
  /** The broadcast is forced on. */
  locked: boolean;
  reason: string | null;
  /** The token every frame URL carries as admit=; null when late. */
  direct: Token | null;
  /** The broadcast's playback URL; null when the fleet runs none. */
  broadcast: Token | null;
  /** A late viewer: a new connection would get a seat now. */
  seat_free: boolean;
  capacity: number | null;
}

export interface AdmissionMsg extends Admission {
  type: "admission";
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

export type ServerMsg = SnapshotMsg | ReceiptMsg | ClockMsg | ErrorMsg | HelloMsg | VerifiedMsg | ClaimMsg | AdmissionMsg;

export const DEFAULT_IMAGE_ASPECT = 16 / 9;

/** The intents the page sends, by the coordinator's names (PROTOCOL.md, "Intents"). */
export const INTENT_NAMES = ["relaunch", "restart_now", "nudge", "word", "light", "claim_gpu", "claim_camera", "claim_autopilot", "release_gpu", "claim_phone", "release_phone", "phone_action", "phones_zoom"] as const;
export type IntentName = (typeof INTENT_NAMES)[number];

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

/** A Turnstile token for this connection. */
export interface VerifyMsg {
  type: "verify";
  token: string;
}

/** Whether this tab is visible, sent on connect and on every change: hidden tabs do not count as viewers. */
export interface VisibilityMsg {
  type: "visibility";
  visible: boolean;
}

export type ClientMsg = IntentMsg | PingMsg | VerifyMsg | VisibilityMsg;

/** Outcomes a receipt can carry that mean the intent took effect. */
export function receiptApplied(r: ReceiptMsg): boolean {
  return r.outcome === "committed" || r.outcome === "applied" || r.outcome === "accepted" || r.outcome === "ok";
}

/** Two spellings of one viewer id (`v-4`, `viewer:v-4`) name the same viewer. */
export function sameViewer(a: string | null, b: string | null): boolean {
  if (a === null || b === null) return false;
  const norm = (v: string) => v.replace(/^viewer:/, "");
  return norm(a) === norm(b);
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
function bool(o: Obj, k: string, where: string): boolean {
  const v = o[k];
  if (typeof v !== "boolean") throw new ProtocolError(`${where}.${k} is not true or false`);
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
    vm: strOrNull(v, "vm", where),
    generation: numOrNull(v, "generation", where),
    fps: numOrNull(v, "fps", where),
    error: strOrNull(v, "error", where),
    failure: parseFailure(v["failure"], `${where}.failure`),
    frame_url: strOrNull(v, "frame_url", where),
    frame_url_v4: strOrNull(v, "frame_url_v4", where),
    gpu: parseGpu(v["gpu"], v["gpu_stats"], `${where}.gpu`),
    host: strOrNull(v, "host", where),
    place: strOrNull(v, "place", where),
    last_relaunch: parseLastRelaunch(v["last_relaunch"], `${where}.last_relaunch`),
    borrowed: isObj(v["borrowed"]) ? parseBorrowed(v["borrowed"], `${where}.borrowed`) : null,
  };
}

function parseBorrowed(b: Obj, where: string): NonNullable<Tile["borrowed"]> {
  const out: NonNullable<Tile["borrowed"]> = { until: num(b, "until", where) };
  const session = strOrNull(b, "session", where);
  if (session !== null) out.session = session;
  const viewer = strOrNull(b, "viewer", where);
  if (viewer !== null) out.viewer = viewer;
  const since = numOrNull(b, "since", where);
  if (since !== null) out.since = since;
  return out;
}

function parsePhones(v: unknown): Phones | null {
  if (v === null || v === undefined) return null;
  if (!isObj(v)) throw new ProtocolError("phones is not an object");
  const w = "phones";
  const aspect = num(v, "image_aspect", w);
  if (aspect <= 0) throw new ProtocolError("phones.image_aspect is not positive");
  const b = obj(v, "borrow", w);
  const tiles = arr(v, "tiles", w).map((t, i) => parseTile(t, i));
  const ids = new Set<string>();
  for (const t of tiles) {
    if (ids.has(t.id)) throw new ProtocolError(`phone ${t.id} appears twice`);
    ids.add(t.id);
  }
  return {
    image_aspect: aspect,
    claim_secs: num(v, "claim_secs", w),
    borrow: { cap: num(b, "cap", `${w}.borrow`), lent: num(b, "lent", `${w}.borrow`), next_return_at: numOrNull(b, "next_return_at", `${w}.borrow`) },
    actions: arr(v, "actions", w).filter((x): x is string => typeof x === "string"),
    zoomed_out: v["zoomed_out"] === true,
    zoom_at: numOrNull(v, "zoom_at", w),
    tiles,
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

function parseCamera(o: Obj, where: string): CameraKey[] {
  const camera: CameraKey[] = [];
  for (const [i, k] of arr(o, "camera", where).entries()) {
    if (!isObj(k)) throw new ProtocolError(`${where}.camera[${i}] is not an object`);
    const key: CameraKey = { t: num(k, "t", `${where}.camera[${i}]`) };
    for (const [f, val] of Object.entries(k)) {
      if (typeof val === "number" || typeof val === "string") key[f] = val;
      else if (Array.isArray(val) && val.every((x) => typeof x === "number")) key[f] = val as number[];
    }
    camera.push(key);
  }
  return camera;
}

function parseScene(o: Obj): Scene {
  const words: Word[] = arr(o, "words", "scene").map((w, i) => {
    const where = `scene.words[${i}]`;
    if (!isObj(w)) throw new ProtocolError(`${where} is not an object`);
    return { text: str(w, "text", where) };
  });
  const lights: Light[] = arr(o, "lights", "scene").map((l, i) => {
    if (!isObj(l)) throw new ProtocolError(`scene.lights[${i}] is not an object`);
    return { pos: l["pos"] ?? null, color: cssColor(l["color"]) };
  });
  return {
    epoch_unix_ms: num(o, "epoch_unix_ms", "scene"),
    camera: parseCamera(o, "scene"),
    time_of_day: numOrNull(o, "time_of_day", "scene") ?? 0.5,
    words,
    lights,
  };
}

function parseFailure(v: unknown, where: string): Failure | null {
  if (v === null || v === undefined) return null;
  if (!isObj(v)) throw new ProtocolError(`${where} is not an object`);
  return { kind: str(v, "kind", where), message: strOrNull(v, "message", where) ?? "", retry_at: numOrNull(v, "retry_at", where) };
}

function parseHalfScore(v: unknown, where: string): HalfScore | null {
  if (v === null || v === undefined) return null;
  if (!isObj(v)) throw new ProtocolError(`${where} is not an object`);
  return {
    median_ms: num(v, "median_ms", where),
    last_ms: num(v, "last_ms", where),
    races: num(v, "races", where),
    measured_on: num(v, "measured_on", where),
    last_capacity_wait_ms: numOrNull(v, "last_capacity_wait_ms", where) ?? 0,
  };
}

function parseNudges(v: unknown): Nudges | null {
  if (v === null || v === undefined) return null;
  if (!isObj(v)) throw new ProtocolError("nudges is not an object");
  return {
    targets: arr(v, "targets", "nudges").map((t, i) => {
      const where = `nudges.targets[${i}]`;
      if (!isObj(t)) throw new ProtocolError(`${where} is not an object`);
      return { id: str(t, "id", where), label: str(t, "label", where), votes: numOrNull(t, "votes", where) ?? 0, arrive_at: numOrNull(t, "arrive_at", where) };
    }),
    holding: strOrNull(v, "holding", "nudges"),
    holding_label: strOrNull(v, "holding_label", "nudges"),
    next_hold_at: numOrNull(v, "next_hold_at", "nudges"),
    next: strOrNull(v, "next", "nudges"),
    next_label: strOrNull(v, "next_label", "nudges"),
    arrive_next_at: numOrNull(v, "arrive_next_at", "nudges"),
    leading: strOrNull(v, "leading", "nudges"),
    opened_at: numOrNull(v, "opened_at", "nudges"),
    closes_at: numOrNull(v, "closes_at", "nudges"),
  };
}

function parseRelaunchAll(ra: unknown): RelaunchAll | null {
  if (ra === null || ra === undefined) return null;
  if (!isObj(ra)) throw new ProtocolError("relaunch_all is neither an object nor null");
  const w = "relaunch_all";
  return {
    started_at: num(ra, "started_at", w),
    by: strOrNull(ra, "by", w) ?? "",
    fcvm_done_at: numOrNull(ra, "fcvm_done_at", w),
    ec2_done_at: numOrNull(ra, "ec2_done_at", w),
    fcvm_pending: numOrNull(ra, "fcvm_pending", w),
    ec2_pending: numOrNull(ra, "ec2_pending", w),
    fcvm_gpus: numOrNull(ra, "fcvm_gpus", w),
    ec2_gpus: numOrNull(ra, "ec2_gpus", w),
    ec2_capacity_wait_ms: numOrNull(ra, "ec2_capacity_wait_ms", w) ?? 0,
    ec2_waiting_since: numOrNull(ra, "ec2_waiting_since", w),
    fcvm_ms: numOrNull(ra, "fcvm_ms", w),
    ec2_ms: numOrNull(ra, "ec2_ms", w),
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
  const viewers = isObj(o["viewers"]) ? o["viewers"] : {};
  const actions: Action[] = arr(o, "actions", "state").flatMap((a) => {
    if (!isObj(a)) return [];
    return [
      {
        at: typeof a["at"] === "number" ? a["at"] : 0,
        viewer: typeof a["viewer"] === "string" ? a["viewer"] : null,
        id: typeof a["id"] === "string" ? a["id"] : null,
        name: typeof a["name"] === "string" ? a["name"] : "",
        args: isObj(a["args"]) ? a["args"] : null,
        summary: typeof a["summary"] === "string" ? a["summary"] : null,
        outcome: typeof a["outcome"] === "string" ? a["outcome"] : null,
      },
    ];
  });
  const aspect = numOrNull(o, "image_aspect", "state") ?? DEFAULT_IMAGE_ASPECT;
  if (aspect <= 0) throw new ProtocolError("image_aspect is not positive");
  const sb = isObj(o["scoreboard"]) ? o["scoreboard"] : {};
  const lay = isObj(o["layout"]) ? o["layout"] : null;
  return {
    layout: lay ? { generation: num(lay, "generation", "layout"), from_frame: num(lay, "from_frame", "layout") } : { generation: 0, from_frame: 0 },
    tiles,
    image_aspect: aspect,
    fleet: {
      hosts: arr(fleet, "hosts", "fleet").map((h, i) => {
        const where = `fleet.hosts[${i}]`;
        if (!isObj(h)) throw new ProtocolError(`${where} is not an object`);
        return { name: str(h, "name", where), draining: h["draining"] === true };
      }),
      gpus: num(fleet, "gpus", "fleet"),
      n: num(fleet, "n", "fleet"),
      max_tiles: numOrNull(fleet, "max_tiles", "fleet"),
      power: parsePower(fleet["power"]),
    },
    borrow: isObj(o["borrow"])
      ? {
          cap: num(o["borrow"], "cap", "borrow"),
          lent: num(o["borrow"], "lent", "borrow"),
          next_return_at: numOrNull(o["borrow"], "next_return_at", "borrow"),
        }
      : null,
    counters: isObj(o["counters"])
      ? {
          restores: num(o["counters"], "restores", "counters"),
          restores_last_hour: numOrNull(o["counters"], "restores_last_hour", "counters") ?? 0,
          scrubs: numOrNull(o["counters"], "scrubs", "counters") ?? 0,
        }
      : null,
    failures: arr(o, "failures", "state").map((f, i) => {
      const where = `failures[${i}]`;
      if (!isObj(f)) throw new ProtocolError(`${where} is not an object`);
      return { kind: str(f, "kind", where), host: strOrNull(f, "host", where), since: numOrNull(f, "since", where) };
    }),
    scene: parseScene(obj(o, "scene", "state")),
    relaunch_all: parseRelaunchAll(o["relaunch_all"]),
    races: parseRaces(o["races"]),
    next_race_at: numOrNull(o, "next_race_at", "state"),
    cooldown_until: numOrNull(o, "cooldown_until", "state"),
    scoreboard: { fcvm: parseHalfScore(sb["fcvm"], "scoreboard.fcvm"), ec2: parseHalfScore(sb["ec2"], "scoreboard.ec2") },
    nudges: parseNudges(o["nudges"]),
    viewers: {
      count: typeof viewers["count"] === "number" ? viewers["count"] : 0,
      active: typeof viewers["active"] === "number" ? viewers["active"] : 0,
    },
    actions,
    aggregates: arr(o, "aggregates", "state").map((a, i) => {
      const where = `aggregates[${i}]`;
      if (!isObj(a)) throw new ProtocolError(`${where} is not an object`);
      const half = str(a, "half", where);
      if (half !== "fcvm" && half !== "ec2") throw new ProtocolError(`${where}.half is ${half}`);
      return {
        id: str(a, "id", where),
        half,
        host: strOrNull(a, "host", where),
        rect: parseRect(obj(a, "rect", where), `${where}.rect`),
        frame_url: strOrNull(a, "frame_url", where),
        frame_url_v4: strOrNull(a, "frame_url_v4", where),
        tiles: arr(a, "tiles", where).filter((x): x is string => typeof x === "string"),
      };
    }),
    phones: parsePhones(o["phones"]),
  };
}

function parseClaim(v: Obj): Claim {
  const w = "claim";
  const phases: Record<string, number> = {};
  if (isObj(v["phases"])) for (const [k, ms] of Object.entries(v["phases"])) if (typeof ms === "number" && Number.isFinite(ms)) phases[k] = ms;
  return {
    id: str(v, "id", w),
    tile: strOrNull(v, "tile", w),
    phase: str(v, "phase", w),
    frame_url: strOrNull(v, "frame_url", w),
    frame_url_v4: strOrNull(v, "frame_url_v4", w),
    clicked_at: num(v, "clicked_at", w),
    first_frame_at: numOrNull(v, "first_frame_at", w),
    expires_at: numOrNull(v, "expires_at", w),
    gpu: strOrNull(v, "gpu", w),
    phases,
    camera: parseCamera(v, "claim"),
    autopilot: bool(v, "autopilot", w),
    ended: strOrNull(v, "ended", w),
  };
}

function parseToken(v: Obj, key: string, w: string): Token | null {
  const t = v[key];
  if (t === null || t === undefined) return null;
  if (!isObj(t)) throw new ProtocolError(`${w}.${key} is not an object`);
  return { value: str(t, "value", `${w}.${key}`), expires_at: num(t, "expires_at", `${w}.${key}`) };
}

function parseAdmission(v: Obj): AdmissionMsg {
  const w = "admission";
  const capacity = v["capacity"];
  if (capacity !== null && capacity !== undefined && !(typeof capacity === "number" && Number.isFinite(capacity))) {
    throw new ProtocolError("admission.capacity is not a number");
  }
  return {
    type: "admission",
    seat: strOrNull(v, "seat", w),
    locked: v["locked"] === true,
    reason: strOrNull(v, "reason", w),
    direct: parseToken(v, "direct", w),
    broadcast: parseToken(v, "broadcast", w),
    seat_free: v["seat_free"] === true,
    capacity: typeof capacity === "number" ? capacity : null,
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
    case "hello":
      return { type: "hello", viewer: str(v, "viewer", "hello"), turnstile: strOrNull(v, "turnstile", "hello") };
    case "verified":
      return { type: "verified", ok: v["ok"] === true, reason: strOrNull(v, "reason", "verified") };
    case "claim":
      return { type: "claim", claim: parseClaim(obj(v, "claim", "claim")) };
    case "admission":
      return parseAdmission(v);
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
