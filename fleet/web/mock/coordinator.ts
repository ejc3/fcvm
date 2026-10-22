// A mock coordinator: the /live socket with snapshots, receipts and clock
// answers, and a fleet whose relaunches are timers instead of VMs. fcvm tiles
// come back in about 3 s, EC2 tiles in about 60 s (both configurable), so the
// race looks like the real one. It is not the coordinator's design (that is one
// pure apply function over an event log, in Rust); it speaks the shapes that
// fleet/PROTOCOL.md documents for the browser, so the page is tested against them.

import http from "node:http";
import { WebSocketServer, type WebSocket } from "ws";
import { DEFAULT_IMAGE_ASPECT, type Half, type Rect } from "../core/protocol.ts";
import { cameraAt, poseAt, projectPoint, sceneT, type Vec3 } from "../core/scene.ts";
import type { MockRenderers } from "./renderer.ts";
import type { PatternScene } from "./pattern.ts";

export interface CoordinatorOptions {
  port: number;
  /** Tiles per half; the grid is cols x rows per half. */
  perHalf: number;
  /** EC2 tiles with no instance (waiting for capacity). */
  ec2Short: number;
  fcvmRelaunchMs: number;
  ec2RelaunchMs: number;
  /** Words the fake text check refuses. */
  refusedWords: string[];
  /** Delay of the fake text check. */
  checkMs: number;
  epochUnixMs: number;
  renderers: MockRenderers;
  now?: () => number;
}

const IMAGE_ASPECT = DEFAULT_IMAGE_ASPECT;

/** Phases and their shares of a relaunch, with the timing key each reports. */
const FCVM_PLAN: { phase: string; share: number; key: string }[] = [
  { phase: "stopping", share: 0.1, key: "teardown_ms" },
  { phase: "restoring", share: 0.57, key: "restore_ms" },
  { phase: "starting", share: 0.2, key: "container_ms" },
  { phase: "starting", share: 0.13, key: "first_frame_ms" },
];
const EC2_PLAN: { phase: string; share: number; key: string }[] = [
  { phase: "launching", share: 0.35, key: "launch_ms" },
  { phase: "starting", share: 0.65, key: "ready_ms" },
];
/** The coordinator shows `relaunching` until the agent or EC2 driver reports. */
const RELAUNCHING_MS = 150;

const NAMES_A = ["teal", "quiet", "amber", "brisk", "silver", "copper", "violet", "mossy", "lucky", "polar"];
const NAMES_B = ["otter", "heron", "lynx", "comet", "falcon", "maple", "badger", "orca", "wren", "ibis"];

interface Client {
  ws: WebSocket;
  name: string;
  /** The connection's viewer id, as the coordinator gives it. */
  viewer: string;
}

/** Who asked for what: the connection's name and viewer id, the intent and its arguments. */
interface Act {
  by: string;
  viewer: string | null;
  name: string;
  args: Record<string, unknown>;
}

interface Receipt {
  type: "receipt";
  id: string;
  outcome: "committed" | "refused";
  reason?: string;
  version: number;
}

interface LastRelaunch {
  clicked_at: number;
  by: string;
  first_frame_at: number | null;
  phases: Record<string, number>;
}

interface TileRuntime {
  id: string;
  half: Half;
  rect: Rect;
  /** The slot behind the tile; null while it waits for capacity. */
  slot: string | null;
  index: number;
  phase: string;
  generation: number;
  gpu: { sbdf: string; name: string } | null;
  gpuStats: { temp_c: number; power_w: number; util_pct: number } | null;
  host: string | null;
  lastRelaunch: LastRelaunch | null;
  timer: NodeJS.Timeout | null;
}

interface CameraKey {
  t: number;
  x: number;
  y: number;
  zoom: number;
  /** The camera intent this key came from. */
  intent?: string;
}

/** The grid for n tiles in a half: the factorisation whose tiles are closest to 16:9. */
export function halfGrid(n: number): { cols: number; rows: number } {
  let best = { cols: 1, rows: n };
  let bestErr = Infinity;
  for (let cols = 1; cols <= n; cols++) {
    if (n % cols !== 0) continue;
    const rows = n / cols;
    const aspect = (0.5 * IMAGE_ASPECT) / cols / (1 / rows);
    const err = Math.abs(Math.log(aspect / (16 / 9)));
    if (err < bestErr) {
      bestErr = err;
      best = { cols, rows };
    }
  }
  return best;
}

function within(outer: Rect, inner: Rect): boolean {
  const e = 1e-9;
  return inner.x >= outer.x - e && inner.y >= outer.y - e && inner.x + inner.w <= outer.x + outer.w + e && inner.y + inner.h <= outer.y + outer.h + e;
}

/** A small deterministic generator, so a mock run is repeatable. */
function rng(seed: number): () => number {
  let s = seed >>> 0;
  return () => {
    s = (s * 1664525 + 1013904223) >>> 0;
    return s / 2 ** 32;
  };
}

function rgbArray(v: unknown): [number, number, number] | null {
  if (Array.isArray(v) && v.length === 3 && v.every((x) => typeof x === "number" && x >= 0 && x <= 64)) return v as [number, number, number];
  return null;
}

export class MockCoordinator {
  private opts: CoordinatorOptions;
  private now: () => number;
  private rand = rng(1136);
  private clients = new Set<Client>();
  private nextViewer = 0;
  private tiles: TileRuntime[] = [];
  private receipts = new Map<string, Receipt | Promise<Receipt>>();
  private appliedIds: string[] = [];
  private version = 0;
  private dirty = false;
  private flushTimer: NodeJS.Timeout | null = null;
  private autoTimer: NodeJS.Timeout | null = null;
  private gpuTimer: NodeJS.Timeout | null = null;
  private server: http.Server;
  private wss: WebSocketServer;
  private camera: CameraKey[] = [{ t: 0, x: 0, y: 0, zoom: 1 }];
  private timeOfDay = 0.42;
  private words: { text: string; by: string; t: number; pos: { x: number; y: number } }[] = [];
  private lights: { pos: { x: number; y: number }; world: Vec3; color: [number, number, number]; by: string; t: number }[] = [];
  private autoSecs: number | null = null;
  private relaunchAll: { started_at: number; by: string; fcvm_done_at: number | null; ec2_done_at: number | null; fcvm_last: string | null; ec2_last: string | null } | null = null;
  private aggregators: { id: string; rect: Rect }[] = [];
  private actions: { at: number; by: string; viewer: string | null; name: string; args: Record<string, unknown>; summary: string; outcome: string }[] = [];

  constructor(opts: CoordinatorOptions) {
    this.opts = opts;
    this.now = opts.now ?? Date.now;
    const { cols, rows } = halfGrid(opts.perHalf);
    for (const half of ["fcvm", "ec2"] as Half[]) {
      for (let r = 0; r < rows; r++) {
        for (let c = 0; c < cols; c++) {
          const i = r * cols + c + 1;
          const short = half === "ec2" && i > opts.perHalf - opts.ec2Short;
          const slot = short ? null : half === "fcvm" ? `slot-${i}` : `ec2-${i}`;
          const rect = { x: (half === "fcvm" ? 0 : 0.5) + (c * 0.5) / cols, y: r / rows, w: 0.5 / cols, h: 1 / rows };
          if (slot) {
            opts.renderers.setTile(slot, rect);
            opts.renderers.setAvailable(slot, true);
          }
          this.tiles.push({
            id: `${half}:${i}`,
            half,
            rect,
            slot,
            index: i,
            phase: short ? "waiting_capacity" : "running",
            generation: 1,
            gpu: short ? null : { sbdf: half === "fcvm" ? `0000:${(0x18 + i).toString(16)}:00.0` : "0000:00:1e.0", name: "Tesla T4" },
            gpuStats: short ? null : { temp_c: 55, power_w: 60, util_pct: 90 },
            host: short ? null : half === "fcvm" ? `g4dn.metal · mock-host-${1 + Math.floor((i - 1) / 8)}` : `g4dn.xlarge · mock-i-${String(i).padStart(4, "0")}`,
            lastRelaunch: null,
            timer: null,
          });
        }
      }
    }
    // Aggregators: on the fcvm half one per band of rows holding about a host's
    // eight GPUs (a host's tiles are one contiguous region), and one for the
    // EC2 half. Each joins its tiles server-side: the mock draws the region
    // whole (the picture is a pure function of position) and blacks out tiles
    // that are not running, as an aggregator has nothing from them.
    const rowsPer = Math.max(1, Math.ceil(8 / cols));
    for (let r0 = 0, b = 1; r0 < rows; r0 += rowsPer, b++) {
      const n = Math.min(rowsPer, rows - r0);
      this.aggregators.push({ id: `agg-fcvm-${b}`, rect: { x: 0, y: r0 / rows, w: 0.5, h: n / rows } });
    }
    this.aggregators.push({ id: "agg-ec2", rect: { x: 0.5, y: 0, w: 0.5, h: 1 } });
    for (const a of this.aggregators) {
      opts.renderers.setTile(a.id, a.rect, () =>
        this.tiles
          .filter((t) => t.phase !== "running" && within(a.rect, t.rect))
          .map((t) => t.rect),
      );
      opts.renderers.setAvailable(a.id, true);
    }
    this.server = http.createServer((req, res) => this.admin(req, res));
    this.wss = new WebSocketServer({ server: this.server, path: "/live", perMessageDeflate: false });
    this.wss.on("connection", (ws) => this.connect(ws));
  }

  listen(): Promise<void> {
    return new Promise((resolve) => {
      this.server.listen(this.opts.port, () => {
        this.autoTimer = setInterval(() => this.autoTick(), 1000);
        this.gpuTimer = setInterval(() => this.gpuTick(), 2000);
        resolve();
      });
    });
  }

  get port(): number {
    const a = this.server.address();
    return typeof a === "object" && a ? a.port : this.opts.port;
  }

  /** The scene the mock renderers draw. */
  patternScene(): PatternScene {
    return {
      epoch_unix_ms: this.opts.epochUnixMs,
      image_aspect: IMAGE_ASPECT,
      camera: this.camera,
      time_of_day: this.timeOfDay,
      words: this.words.map((w) => ({ text: w.text, pos: w.pos })),
      lights: this.lights.map((l) => ({ pos: l.pos, color: l.color })),
    };
  }

  /** The snapshot state, in the shape fleet/PROTOCOL.md gives. */
  fleetState() {
    const fcvmGpus = this.tiles.filter((t) => t.half === "fcvm" && t.slot).length;
    const ec2 = this.tiles.filter((t) => t.half === "ec2" && t.slot).length;
    const hostNames = [...new Set(this.tiles.filter((t) => t.half === "fcvm" && t.host).map((t) => t.host!))];
    return {
      tiles: this.tiles.map((t) => ({
        id: t.id,
        half: t.half,
        rect: t.rect,
        phase: t.phase,
        slot: t.slot,
        frame_url: t.slot ? this.opts.renderers.frameUrl(t.slot) : null,
        gpu: t.gpu,
        host: t.host,
        vm: t.slot ? `fleet-${t.slot}` : null,
        generation: t.slot ? t.generation : null,
        fps: t.phase === "running" ? 20 : null,
        gpu_stats: t.gpuStats,
        error: null,
        last_relaunch: t.lastRelaunch,
      })),
      fleet: {
        hosts: hostNames.map((name) => ({ name, arch: "x86_64", gpus: [], draining: false })),
        gpus: fcvmGpus,
        fcvm_gpus: fcvmGpus,
        ec2_instances: ec2,
        n: this.opts.perHalf,
        max_tiles: 100,
        standby: [],
      },
      scene: {
        epoch_unix_ms: this.opts.epochUnixMs,
        camera: this.camera.map((k) => ({ t: k.t, pos: [k.x, k.y, 0], look: [k.x, k.y, -10], fov_deg: 60 / k.zoom, ...(k.intent ? { intent: k.intent } : {}) })),
        time_of_day: this.timeOfDay,
        words: this.words.map((w) => ({ text: w.text, t: w.t, by: w.by })),
        lights: this.lights.map((l) => ({ pos: l.world, color: l.color, t: l.t, by: l.by })),
      },
      image_aspect: IMAGE_ASPECT,
      settings: { auto_relaunch_secs: this.autoSecs },
      relaunch_all: this.relaunchAll && {
        ...this.relaunchAll,
        fcvm_pending: this.pending("fcvm"),
        ec2_pending: this.pending("ec2"),
      },
      viewers: { count: this.clients.size },
      actions: this.actions,
      aggregators: this.aggregators.map((a) => ({ id: a.id, frame_url: this.opts.renderers.frameUrl(a.id), rect: a.rect })),
    };
  }

  get currentVersion(): number {
    return this.version;
  }

  /** How many times an intent id took effect. A duplicate never adds one. */
  appliedCount(id: string): number {
    return this.appliedIds.filter((x) => x === id).length;
  }

  // ---- connections ------------------------------------------------------------

  private connect(ws: WebSocket): void {
    const k = this.nextViewer++;
    const client: Client = { ws, name: `${NAMES_A[k % NAMES_A.length]} ${NAMES_B[Math.floor(k / NAMES_A.length) % NAMES_B.length]}`, viewer: `viewer:v-${k + 1}` };
    this.clients.add(client);
    this.sendTo(client, { type: "welcome", viewer: client.viewer });
    // Publish now, to everyone: a change made since the last snapshot belongs to
    // the next version, so the newcomer must not get it under the current one.
    this.dirty = true;
    if (this.flushTimer) clearTimeout(this.flushTimer);
    this.flush();
    ws.on("message", (data, isBinary) => {
      if (isBinary) return;
      let parsed: unknown;
      try {
        parsed = JSON.parse(data.toString());
      } catch {
        this.sendTo(client, { type: "error", reason: "message is not JSON" });
        return;
      }
      if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
        this.sendTo(client, { type: "error", reason: "message is not an object" });
        return;
      }
      const m = parsed as Record<string, unknown>;
      if (m["type"] === "ping") {
        this.sendTo(client, { type: "clock", server_unix_ms: this.now(), client_unix_ms: m["client_unix_ms"] });
      } else if (m["type"] === "intent" && typeof m["id"] === "string" && typeof m["name"] === "string") {
        const args = typeof m["args"] === "object" && m["args"] !== null ? (m["args"] as Record<string, unknown>) : {};
        void this.intent(client, m["id"], m["name"], args);
      } else {
        this.sendTo(client, { type: "error", reason: "unknown message" });
      }
    });
    ws.on("close", () => {
      this.clients.delete(client);
      this.markDirty();
    });
    ws.on("error", () => {});
  }

  private sendTo(c: Client, msg: unknown): void {
    if (c.ws.readyState === c.ws.OPEN) c.ws.send(JSON.stringify(msg));
  }

  /** Changes made now land in the next version; snapshots go out coalesced. */
  private markDirty(): number {
    if (!this.dirty) {
      this.dirty = true;
      // At most one snapshot every 50 ms, as the coordinator sends them.
      this.flushTimer = setTimeout(() => this.flush(), 50);
    }
    return this.version + 1;
  }

  private flush(): void {
    this.flushTimer = null;
    if (!this.dirty) return;
    this.dirty = false;
    this.version += 1;
    const msg = JSON.stringify({ type: "snapshot", version: this.version, state: this.fleetState() });
    for (const c of this.clients) if (c.ws.readyState === c.ws.OPEN) c.ws.send(msg);
  }

  // ---- intents ----------------------------------------------------------------

  private async intent(client: Client, id: string, name: string, args: Record<string, unknown>): Promise<void> {
    const seen = this.receipts.get(id);
    if (seen) {
      this.sendTo(client, await seen);
      return;
    }
    const p = this.apply({ by: client.name, viewer: client.viewer, name, args }, id);
    this.receipts.set(id, p);
    const r = await p;
    this.receipts.set(id, r);
    this.sendTo(client, r);
  }

  private log(c: Act, summary: string, outcome: string): void {
    // A refused word's text appears nowhere, not even in its arguments.
    const args = c.name === "word" && outcome === "refused" ? {} : c.args;
    this.actions = [...this.actions, { at: this.now(), by: c.by, viewer: c.viewer, name: c.name, args, summary, outcome }].slice(-50);
  }

  private refuse(id: string, c: Act, reason: string): Receipt {
    this.log(c, reason, "refused");
    return { type: "receipt", id, outcome: "refused", reason, version: this.markDirty() };
  }

  private committed(id: string, c: Act, summary: string): Receipt {
    this.appliedIds.push(id);
    this.log(c, summary, "committed");
    return { type: "receipt", id, outcome: "committed", version: this.markDirty() };
  }

  private sceneNow(): number {
    return sceneT(this.opts.epochUnixMs, this.now());
  }

  private async apply(c: Act, id: string): Promise<Receipt> {
    const { by, name, args } = c;
    switch (name) {
      case "relaunch": {
        const want = String(args["tile"]);
        const r = this.tiles.find((t) => t.id === want || t.slot === want);
        if (!r) return this.refuse(id, c, `no tile ${want}`);
        if (!r.slot) return this.refuse(id, c, `no GPU behind ${r.id}`);
        this.relaunch(r, by);
        return this.committed(id, c, `relaunch ${r.slot}`);
      }
      case "relaunch_all": {
        const ra = this.relaunchAll;
        if (ra && (ra.fcvm_done_at === null || ra.ec2_done_at === null)) return this.refuse(id, c, "a relaunch all is already running");
        const live = this.tiles.filter((t) => t.slot);
        this.relaunchAll = { started_at: this.now(), by, fcvm_done_at: null, ec2_done_at: null, fcvm_last: null, ec2_last: null };
        for (const r of live) this.relaunch(r, by);
        return this.committed(id, c, `relaunch all (${live.length} tiles)`);
      }
      case "set_auto_relaunch": {
        const secs = args["secs"];
        if (secs !== null && (typeof secs !== "number" || !(secs >= 10 && secs <= 86400))) {
          return this.refuse(id, c, "auto-relaunch is off or 10 s to 1 day");
        }
        this.autoSecs = secs as number | null;
        return this.committed(id, c, secs === null ? "auto relaunch off" : `auto relaunch every ${secs} s`);
      }
      case "time_of_day": {
        const v = args["v"];
        if (typeof v !== "number" || !(v >= 0 && v <= 1)) return this.refuse(id, c, "time of day is a fraction of the day in [0, 1]");
        this.timeOfDay = v;
        return this.committed(id, c, `time of day ${v.toFixed(3)}`);
      }
      case "camera": {
        const drag = args["drag"] as Record<string, unknown> | undefined;
        const num = (k: string) => (typeof drag?.[k] === "number" ? (drag[k] as number) : 0);
        const yaw = num("yaw_deg");
        const pitch = num("pitch_deg");
        const dive = num("dive");
        if (!drag || ![yaw, pitch, dive].every(Number.isFinite)) return this.refuse(id, c, "camera drag is yaw_deg, pitch_deg, dive");
        const t = this.sceneNow();
        // A move still easing in is the base for the next one, so drags that
        // arrive together add up.
        const last = this.camera[this.camera.length - 1]!;
        const easing = last.t > t;
        const now = cameraAt(this.camera, t);
        const cur = easing ? last : { t, x: now["x"] ?? 0, y: now["y"] ?? 0, zoom: now["zoom"] ?? 1 };
        const zoom0 = cur.zoom;
        const next: CameraKey = {
          t: Math.max(t, cur.t) + 0.05,
          x: cur.x + ((yaw / 90) * IMAGE_ASPECT) / zoom0,
          y: cur.y - pitch / 50 / zoom0,
          zoom: Math.min(6, Math.max(0.35, zoom0 * Math.exp(dive * 0.2))),
          intent: id,
        };
        const kept = this.camera.filter((k) => k.t >= t - 2).slice(-4);
        this.camera = easing ? [...kept, next] : [...kept.filter((k) => k.t <= t), { t, x: cur.x, y: cur.y, zoom: cur.zoom }, next];
        return this.committed(id, c, "camera");
      }
      case "word": {
        const text = typeof args["text"] === "string" ? args["text"].replace(/\s+/g, " ").trim() : "";
        await new Promise((r) => setTimeout(r, this.opts.checkMs));
        if (text.length === 0 || text.length > 28) return this.refuse(id, c, "a word is 1 to 28 characters");
        const bad = !/^[A-Za-z0-9 .,!?'-]+$/.test(text) || this.opts.refusedWords.some((w) => text.toLowerCase().includes(w.toLowerCase()));
        if (bad) return this.refuse(id, c, "the text check refused it");
        const t = this.sceneNow();
        const cam = cameraAt(this.camera, t);
        const k = this.words.length;
        const ang = k * 2.4;
        const rad = 0.25 + 0.12 * (k % 4);
        const pos = { x: (cam["x"] ?? 0) + Math.cos(ang) * rad * 1.6, y: (cam["y"] ?? 0) + Math.sin(ang) * rad - 0.1 };
        this.words = [...this.words, { text, by, t, pos }].slice(-16);
        return this.committed(id, c, `word ${JSON.stringify(text)}`);
      }
      case "light": {
        const pos = args["pos"];
        const color = rgbArray(args["color"]);
        if (!Array.isArray(pos) || pos.length !== 3 || !pos.every((x) => typeof x === "number" && Number.isFinite(x))) {
          return this.refuse(id, c, "a light's pos is [x, y, z]");
        }
        if (!color) return this.refuse(id, c, "a light's color is [r, g, b]");
        // The mock's world is flat: it shows the light where it appears on the image.
        const t = this.sceneNow();
        const keys = this.fleetState().scene.camera;
        const pose = poseAt(keys, t);
        const at = pose ? projectPoint(pose, pos as Vec3, IMAGE_ASPECT) : null;
        if (!at) return this.refuse(id, c, "the light is behind the camera");
        const cam = cameraAt(this.camera, t);
        const zoom = cam["zoom"] ?? 1;
        const world = { x: ((at.u - 0.5) * IMAGE_ASPECT) / zoom + (cam["x"] ?? 0), y: (at.v - 0.5) / zoom + (cam["y"] ?? 0) };
        this.lights = [...this.lights, { pos: world, world: pos as Vec3, color, by, t }].slice(-32);
        return this.committed(id, c, "light");
      }
      default:
        return this.refuse(id, c, `unknown intent ${name}`);
    }
  }

  // ---- relaunches -------------------------------------------------------------

  private pending(h: Half): number {
    const ra = this.relaunchAll;
    if (!ra) return 0;
    return this.tiles.filter((t) => t.half === h && t.slot && !((t.lastRelaunch?.first_frame_at ?? 0) >= ra.started_at && t.phase === "running")).length;
  }

  private relaunch(r: TileRuntime, by: string): void {
    if (r.timer) clearTimeout(r.timer);
    r.generation += 1;
    const gen = r.generation;
    const clickedAt = this.now();
    const fc = r.half === "fcvm";
    const base = fc ? this.opts.fcvmRelaunchMs : this.opts.ec2RelaunchMs;
    const total = base * (0.8 + 0.4 * this.rand());
    const plan = (fc ? FCVM_PLAN : EC2_PLAN).map((p) => ({ ...p, ms: Math.round(total * p.share) }));
    const phases: Record<string, number> = {};
    r.phase = "relaunching";
    r.lastRelaunch = { clicked_at: clickedAt, by, first_frame_at: null, phases: {} };
    this.opts.renderers.setAvailable(r.slot!, false);
    this.markDirty();
    let i = 0;
    const step = () => {
      if (r.generation !== gen) return;
      const p = plan[i]!;
      phases[p.key] = p.ms;
      if (fc && p.key === "restore_ms") {
        phases["gpu_ms"] = Math.round(p.ms * 0.62);
        phases["ack_ms"] = p.ms - 40;
      }
      i += 1;
      if (i < plan.length) {
        r.phase = plan[i]!.phase;
        r.lastRelaunch = { clicked_at: clickedAt, by, first_frame_at: null, phases: { ...phases } };
        this.markDirty();
        r.timer = setTimeout(step, plan[i]!.ms);
        return;
      }
      r.timer = null;
      const done = this.now();
      phases["total_ms"] = done - clickedAt;
      this.opts.renderers.setAvailable(r.slot!, true);
      r.phase = "running";
      r.lastRelaunch = { clicked_at: clickedAt, by, first_frame_at: done, phases: { ...phases } };
      this.checkRace(r);
      this.markDirty();
    };
    r.timer = setTimeout(() => {
      if (r.generation !== gen) return;
      r.phase = plan[0]!.phase;
      this.markDirty();
      r.timer = setTimeout(step, Math.max(0, plan[0]!.ms - RELAUNCHING_MS));
    }, RELAUNCHING_MS);
  }

  private checkRace(last: TileRuntime): void {
    const ra = this.relaunchAll;
    if (!ra) return;
    if (last.half === "fcvm" && ra.fcvm_done_at === null) ra.fcvm_last = last.slot;
    if (last.half === "ec2" && ra.ec2_done_at === null) ra.ec2_last = last.slot;
    if (ra.fcvm_done_at === null && this.pending("fcvm") === 0) ra.fcvm_done_at = this.now();
    if (ra.ec2_done_at === null && this.pending("ec2") === 0) ra.ec2_done_at = this.now();
  }

  private autoTick(): void {
    const secs = this.autoSecs;
    if (secs === null) return;
    const now = this.now();
    for (const r of this.tiles) {
      if (!r.slot || r.timer !== null) continue;
      const last = r.lastRelaunch?.clicked_at ?? -Infinity; // never relaunched: due now
      if (now - last >= secs * 1000) {
        this.relaunch(r, "auto-relaunch");
        this.log({ by: "auto-relaunch", viewer: null, name: "relaunch", args: { tile: r.id } }, `relaunch ${r.slot}`, "committed");
      }
    }
  }

  private gpuTick(): void {
    const t = this.now() / 1000;
    for (const r of this.tiles) {
      if (!r.gpuStats) continue;
      const running = r.phase === "running";
      const k = r.index + (r.half === "ec2" ? 50 : 0);
      r.gpuStats = {
        temp_c: Math.round(running ? 58 + 6 * Math.sin(t / 30 + k) : 41),
        power_w: Math.round(running ? 64 + 6 * Math.sin(t / 7 + k * 1.3) : 18),
        util_pct: Math.round(running ? 93 + 5 * Math.sin(t / 5 + k) : 0),
      };
    }
    this.markDirty();
  }

  // ---- admin routes, for tests -------------------------------------------------

  private admin(req: http.IncomingMessage, res: http.ServerResponse): void {
    const url = new URL(req.url ?? "/", "http://mock");
    const json = (code: number, body: unknown) => {
      res.writeHead(code, { "content-type": "application/json" }).end(JSON.stringify(body));
    };
    if (req.method === "GET" && url.pathname === "/mock/state") {
      json(200, { version: this.version, state: this.fleetState(), pattern: this.patternScene() });
      return;
    }
    if (req.method === "GET" && url.pathname === "/mock/render-stats") {
      json(200, this.opts.renderers.stats);
      return;
    }
    if (req.method === "GET" && url.pathname === "/mock/applied") {
      json(200, { id: url.searchParams.get("id"), count: this.appliedCount(url.searchParams.get("id") ?? "") });
      return;
    }
    if (req.method === "POST" && url.pathname === "/mock/fault") {
      let body = "";
      req.on("data", (d: Buffer) => (body += d.toString()));
      req.on("end", () => {
        try {
          const b = JSON.parse(body) as { tile: string; skew: number };
          this.opts.renderers.setSkew(b.tile, Number(b.skew) || 0);
          json(200, { ok: true });
        } catch {
          json(400, { ok: false });
        }
      });
      return;
    }
    res.writeHead(404).end();
  }

  async close(): Promise<void> {
    if (this.autoTimer) clearInterval(this.autoTimer);
    if (this.gpuTimer) clearInterval(this.gpuTimer);
    if (this.flushTimer) clearTimeout(this.flushTimer);
    for (const r of this.tiles) if (r.timer) clearTimeout(r.timer);
    for (const c of this.clients) c.ws.terminate();
    this.wss.close();
    await new Promise<void>((r) => this.server.close(() => r()));
  }
}
