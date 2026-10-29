// A mock coordinator: the /live socket with hello, snapshots, receipts, clock
// answers and private claim messages, and a fleet whose restarts are timers
// instead of VMs. Restart times are shaped like the measured ones (fcvm about
// 3 s in teardown, restore with a GPU reset, renderer start and first frame;
// EC2 tens of seconds in launch and boot, sometimes after a wait for spot
// capacity), and every number the page shows comes from here. It is not the
// coordinator's design (that is one pure apply function over an event log, in
// Rust); it speaks the shapes fleet/PROTOCOL.md documents for the browser, plus
// the second design review's fields the page assumes (see core/protocol.ts).

import { createReadStream, statSync } from "node:fs";
import http from "node:http";
import path from "node:path";
import { WebSocketServer, type WebSocket } from "ws";
import { DEFAULT_IMAGE_ASPECT, FRAME_RATE, type Half, type Rect } from "../core/protocol.ts";
import { cameraAt, poseAt, projectPoint, sceneT, type Vec3 } from "../core/scene.ts";
import type { MockRenderers } from "./renderer.ts";
import type { PatternScene } from "./pattern.ts";
import { mockLayout } from "./layout.ts";

export interface CoordinatorOptions {
  port: number;
  /** GPUs live on each side: the image is split in proportion to them. */
  fcvmGpus: number;
  ec2Gpus: number;
  /** GPUs per fcvm host. */
  gpusPerHost: number;
  /** Typical restart times; each restart takes 0.87 to 1.13 (fcvm) or 0.67 to 1.33 (EC2) of these. */
  fcvmRelaunchMs: number;
  ec2RelaunchMs: number;
  /** The owner's cadence: a restart of every GPU this often (0: none), the first one this long after start. */
  raceEverySecs: number;
  firstRaceSecs: number;
  /** After any restart of every GPU, a viewer's "restart now" waits this long. */
  cooldownSecs: number;
  /** Chance that a race's EC2 half first waits for spot capacity, and how long such a wait lasts. */
  capacityWaitProb: number;
  capacityWaitMs: [number, number];
  /** Wall GPUs lent to visitors at most at once, and how long a loan lasts. */
  borrowCap: number;
  claimSecs: number;
  /** How long the directed camera holds on a stop, and flies to the next. */
  holdSecs: number;
  travelSecs: number;
  /** Turnstile site key; null: actions need no check. The mock accepts the token `mock-pass`. */
  turnstileSitekey: string | null;
  /** The GPU memory scrub has shipped: every GPU moving to the next VM is scrubbed and counted. */
  gpuScrub?: boolean;
  /** With no visible viewer for this long the fleet sleeps (0: never by itself). */
  sleepAfterSecs?: number;
  /** How long a wake takes, from the first instance requested to every tile rendering. */
  wakeMs?: number;
  /** Where the recording's files are served from at /media (none: /media answers 404). */
  mediaDir?: string;
  /** Words the fake text check refuses. */
  refusedWords: string[];
  /** Delay of the fake text check. */
  checkMs: number;
  epochUnixMs: number;
  renderers: MockRenderers;
  /** Seed for the restart times, so a run is repeatable. */
  seed?: number;
  now?: () => number;
}

const IMAGE_ASPECT = DEFAULT_IMAGE_ASPECT;

/** Phases and their shares of a restart, with the timing key each reports. */
const FCVM_PLAN: { phase: string; share: number; key: string }[] = [
  { phase: "stopping", share: 0.1, key: "teardown_ms" },
  { phase: "restoring", share: 0.56, key: "restore_ms" },
  { phase: "starting", share: 0.2, key: "container_ms" },
  { phase: "starting", share: 0.14, key: "first_frame_ms" },
];
/** Of the restore, the GPU reset and driver load (about 0.58 s of a 3 s restart). */
const GPU_SHARE_OF_RESTORE = 0.35;
/** The EC2 driver's timings, as the coordinator's vectors give them: the launch, then boot to first frame. */
const EC2_PLAN: { phase: string; share: number; key: string }[] = [
  { phase: "starting", share: 0.25, key: "launch_ms" },
  { phase: "starting", share: 0.75, key: "ready_ms" },
];
/** The coordinator shows `relaunching` until the agent or EC2 driver reports. */
const RELAUNCHING_MS = 150;
/** A re-tile's rectangles start this many frames (on t = k/30) after it is decided. */
const RETILE_LEAD_FRAMES = 15;
/** A reclaimed host stays in `failures` (and `fleet.hosts`, draining) this long after its tiles leave. */
const RECLAIM_KEPT_MS = 60_000;
/** Races the scoreboard's medians are over. */
const RACES_KEPT = 20;

const NUDGE_TARGETS: { id: string; label: string; x: number; y: number; zoom: number }[] = [
  { id: "planet", label: "the ringed planet", x: 0.35, y: -0.05, zoom: 1.6 },
  { id: "nebula", label: "the nebula", x: -0.6, y: 0.15, zoom: 1.0 },
  { id: "stars", label: "the star field", x: 1.2, y: -0.4, zoom: 0.7 },
  { id: "words", label: "the viewers' words", x: 0, y: 0, zoom: 1.25 },
];

interface Client {
  ws: WebSocket;
  /** The tab is visible; hidden tabs do not count as viewers. */
  visible: boolean;
  /** The connection's viewer id, as the coordinator gives it. */
  viewer: string;
  verified: boolean;
  claim: ClaimRuntime | null;
}

interface Act {
  viewer: string;
  id: string;
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
  /** The slot behind the tile; null while it has no GPU. */
  slot: string | null;
  index: number;
  host: string | null;
  phase: string;
  generation: number;
  gpu: { sbdf: string; name: string } | null;
  gpuStats: { temp_c: number; power_w: number; util_pct: number } | null;
  lastRelaunch: LastRelaunch | null;
  failure: { kind: string; message: string; retry_at: number | null } | null;
  /** Lent to a visitor: the claim using its GPU, and when it comes back. */
  borrowed: { until: number; claim: ClaimRuntime } | null;
  timer: NodeJS.Timeout | null;
}

interface CameraKey {
  t: number;
  x: number;
  y: number;
  zoom: number;
  /** The intent this key came from. */
  intent?: string;
}

interface ClaimRuntime {
  id: string;
  tile: TileRuntime;
  client: Client;
  phase: "restoring" | "running" | "ended";
  clicked_at: number;
  first_frame_at: number | null;
  expires_at: number | null;
  phases: Record<string, number>;
  camera: CameraKey[];
  /** The camera flies the mock's autopilot loop (see `flyMock`); false once the viewer has it. */
  autopilot: boolean;
  /** Scene t at which the loop's phase is 0 while it flies. */
  flightT0: number | null;
  /** Extends the flight's keys while it flies. */
  flightTimer: NodeJS.Timeout | null;
  ended: string | null;
  timer: NodeJS.Timeout | null;
}

/** The mock's autopilot: a 16 s loop of pans and a zoom over the test pattern. */
const MOCK_LOOP_S = 16;
function mockFlight(phase: number): { x: number; y: number; zoom: number } {
  const a = (2 * Math.PI * phase) / MOCK_LOOP_S;
  return { x: 0.35 * Math.sin(a), y: 0.12 * Math.sin(2 * a), zoom: 1.35 + 0.35 * Math.sin(a + 1) };
}

interface Race {
  started_at: number;
  by: string;
  fcvm_done_at: number | null;
  ec2_done_at: number | null;
  fcvm_gpus: number;
  ec2_gpus: number;
  ec2_capacity_wait_ms: number;
  ec2_waiting_since: number | null;
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

function median(xs: number[]): number {
  const s = [...xs].sort((a, b) => a - b);
  const m = Math.floor(s.length / 2);
  return s.length % 2 === 1 ? s[m]! : (s[m - 1]! + s[m]!) / 2;
}

function rgbArray(v: unknown): [number, number, number] | null {
  if (Array.isArray(v) && v.length === 3 && v.every((x) => typeof x === "number" && x >= 0 && x <= 64)) return v as [number, number, number];
  return null;
}

export class MockCoordinator {
  private opts: CoordinatorOptions;
  private now: () => number;
  private rand: () => number;
  private clients = new Set<Client>();
  private nextViewer = 0;
  private tiles: TileRuntime[] = [];
  private hosts: { name: string; draining: boolean }[] = [];
  private receipts = new Map<string, Receipt | Promise<Receipt>>();
  private appliedIds: string[] = [];
  private version = 0;
  private dirty = false;
  private flushTimer: NodeJS.Timeout | null = null;
  private tickTimer: NodeJS.Timeout | null = null;
  private gpuTimer: NodeJS.Timeout | null = null;
  private timers = new Set<NodeJS.Timeout>();
  private server: http.Server;
  private wss: WebSocketServer;
  private camera: CameraKey[] = [{ t: 0, x: 0, y: 0, zoom: 1 }];
  private timeOfDay = 0.42;
  private words: { text: string; t: number; pos: { x: number; y: number }; text_style: string }[] = [];
  private lights: { pos: { x: number; y: number }; world: Vec3; color: [number, number, number]; t: number }[] = [];
  private race: Race | null = null;
  private races: { fcvm_ms: number; ec2_ms: number; wait_ms: number; fcvm_gpus: number; ec2_gpus: number }[] = [];
  private nextRaceAt: number | null;
  private cooldownUntil: number | null = null;
  /** A test's forced capacity wait for the next race. */
  private forcedWaitMs: number | null = null;
  private nudge: { holding: string | null; nextHoldAt: number; openedAt: number; votes: Map<string, string> };
  private nextClaim = 0;
  private aggregates: { id: string; half: Half; host: string | null; rect: Rect; tiles: string[] }[] = [];
  /** The fcvm GPUs live, by slot number (a host has slots of one block of gpusPerHost). */
  private fcvmSlots: number[] = [];
  private ec2Count = 0;
  /** Which tiling, and the frame (on t = k/30) its rectangles start at. */
  private layout = { generation: 1, from_frame: 0 };
  private failures: { kind: "host_reclaimed"; host: string; since: number; retiling: boolean }[] = [];
  private actions: { at: number; viewer: string; id: string; name: string; args: Record<string, unknown>; summary: string; outcome: string }[] = [];
  private offlineUntil = 0;
  private sitekey: string | null;
  private poster: { at: number; jpeg: Promise<Buffer> } | null = null;
  /** `/mock/fault {no_poster}`: answer /poster.jpg as a coordinator that has none yet. */
  private noPoster = false;
  private restoresTotal = 0;
  private scrubsTotal = 0;
  /** Unix ms of each restore in the last hour. */
  private restoreTimes: number[] = [];
  private gpuScrub: boolean;
  private power: {
    state: "asleep" | "waking" | "awake";
    asleep_since: number | null;
    woke_at: number | null;
    hosts: { host: string; phases: { phase: string; at: number }[] }[];
    last_wake: { started_at: number; awake_at: number; gpus: number; ms: number } | null;
  } = { state: "awake", asleep_since: null, woke_at: null, hosts: [], last_wake: null };
  /** Hosts whose tiles render again during a wake. */
  private rendering = new Set<string>();
  /** The GPUs each side had when the fleet went to sleep; they join again as the fleet wakes. */
  private slept: { fcvm: number[]; ec2: number } | null = null;
  /** Tests: stay asleep whoever watches. */
  private holdAsleep = false;
  private idleSince: number | null = null;
  /** The schedule's next beat, kept while the fleet sleeps. */
  private sleptRaceAt: number | null = null;
  private mediaMissing = false;
  /** Races already added to the scoreboard. */
  private recorded = new WeakSet<Race>();

  constructor(opts: CoordinatorOptions) {
    this.opts = opts;
    this.now = opts.now ?? Date.now;
    this.rand = rng(opts.seed ?? 1136);
    this.sitekey = opts.turnstileSitekey;
    this.gpuScrub = opts.gpuScrub ?? false;
    for (let k = 1; k <= opts.fcvmGpus; k++) this.fcvmSlots.push(k);
    for (const h of new Set(this.fcvmSlots.map((k) => this.hostOf(k)))) this.hosts.push({ name: h, draining: false });
    this.ec2Count = opts.ec2Gpus;
    this.applyLayout(null);
    const start = this.now();
    this.nextRaceAt = opts.raceEverySecs > 0 ? start + opts.firstRaceSecs * 1000 : null;
    this.nudge = { holding: "planet", nextHoldAt: start + (opts.holdSecs + opts.travelSecs) * 1000, openedAt: start, votes: new Map() };
    const t0 = sceneT(opts.epochUnixMs, start);
    const first = NUDGE_TARGETS[0]!;
    this.camera = [{ t: t0, x: first.x, y: first.y, zoom: first.zoom }];
    this.server = http.createServer((req, res) => this.http(req, res));
    this.wss = new WebSocketServer({ noServer: true, perMessageDeflate: false });
    this.server.on("upgrade", (req, socket, head) => {
      const url = new URL(req.url ?? "/", "http://mock");
      if (url.pathname !== "/live") {
        socket.end("HTTP/1.1 404 Not Found\r\n\r\n");
        return;
      }
      if (this.now() < this.offlineUntil) {
        socket.end("HTTP/1.1 503 Service Unavailable\r\n\r\n");
        return;
      }
      this.wss.handleUpgrade(req, socket, head, (ws) => this.connect(ws));
    });
  }

  /** Renderer route ids allow no colon. */
  private streamId(id: string): string {
    return id.replace(/[^A-Za-z0-9_-]/g, "-");
  }

  listen(): Promise<void> {
    return new Promise((resolve) => {
      this.server.listen(this.opts.port, () => {
        this.tickTimer = setInterval(() => this.tick(), 250);
        this.gpuTimer = setInterval(() => this.gpuTick(), 2000);
        resolve();
      });
    });
  }

  get port(): number {
    const a = this.server.address();
    return typeof a === "object" && a ? a.port : this.opts.port;
  }

  private later(ms: number, fn: () => void): NodeJS.Timeout {
    const t = setTimeout(() => {
      this.timers.delete(t);
      fn();
    }, Math.max(0, ms));
    this.timers.add(t);
    return t;
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

  private scoreboard() {
    const half = (h: Half) => {
      if (this.races.length === 0) return null;
      const last = this.races[this.races.length - 1]!;
      const ms = this.races.map((r) => (h === "fcvm" ? r.fcvm_ms : r.ec2_ms));
      return {
        median_ms: Math.round(median(ms)),
        last_ms: h === "fcvm" ? last.fcvm_ms : last.ec2_ms,
        races: this.races.length,
        measured_on: h === "fcvm" ? last.fcvm_gpus : last.ec2_gpus,
        last_capacity_wait_ms: h === "ec2" ? last.wait_ms : 0,
      };
    };
    return { fcvm: half("fcvm"), ec2: half("ec2") };
  }

  /** The snapshot state, in the shape fleet/PROTOCOL.md gives plus the assumed fields. */
  fleetState() {
    const fcvmGpus = this.tiles.filter((t) => t.half === "fcvm" && t.slot).length;
    const ec2 = this.tiles.filter((t) => t.half === "ec2" && t.slot).length;
    const now = this.now();
    return {
      layout: { ...this.layout, from_t: this.layout.from_frame / FRAME_RATE },
      tiles: this.tiles.map((t) => ({
        id: t.id,
        half: t.half,
        rect: t.rect,
        phase: t.phase,
        slot: t.slot,
        frame_url: t.slot ? this.opts.renderers.frameUrl(t.slot) : null,
        frame_url_v4: t.slot ? this.opts.renderers.frameUrlV4(t.slot) : null,
        gpu: t.gpu,
        host: t.host,
        place: t.host === null ? null : "EC2 in mock-region",
        vm: t.slot ? `fleet-${t.slot}` : null,
        generation: t.slot ? t.generation : null,
        fps: t.phase === "running" ? 30 : null,
        gpu_stats: t.gpuStats,
        error: t.failure && t.failure.kind === "relaunch_failed" ? t.failure.message : null,
        failure: t.failure,
        last_relaunch: t.lastRelaunch,
        borrowed: t.borrowed && { until: t.borrowed.until },
      })),
      fleet: {
        hosts: this.hosts.map((h) => ({ name: h.name, arch: "x86_64", gpus: [], draining: h.draining })),
        gpus: fcvmGpus,
        fcvm_gpus: fcvmGpus,
        ec2_instances: ec2,
        n: this.tiles.length,
        max_tiles: 100,
        standby: [],
        power: this.power,
      },
      borrow: {
        cap: this.opts.borrowCap,
        lent: this.lent().length,
        next_return_at: this.lent().length === 0 ? null : Math.min(...this.lent().map((t) => t.borrowed!.until)),
      },
      scene: {
        epoch_unix_ms: this.opts.epochUnixMs,
        camera: this.camera.map((k) => ({ t: k.t, pos: [k.x, k.y, 0], look: [k.x, k.y, -10], fov_deg: 60 / k.zoom, x: k.x, y: k.y, zoom: k.zoom, ...(k.intent ? { intent: k.intent } : {}) })),
        time_of_day: this.timeOfDay,
        words: this.words.map((w) => ({ text: w.text, t: w.t, text_style: w.text_style })),
        lights: this.lights.map((l) => ({ pos: l.world, color: l.color, t: l.t })),
      },
      image_aspect: IMAGE_ASPECT,
      relaunch_all: this.race && {
        ...this.race,
        fcvm_pending: this.pending("fcvm"),
        ec2_pending: this.pending("ec2"),
      },
      counters: {
        restores: this.restoresTotal,
        restores_last_hour: this.restoreTimes.filter((t) => t > now - 3_600_000).length,
        scrubs: this.scrubsTotal,
        scrub_failures: 0,
      },
      failures: this.failures.filter((f) => f.since > now - RECLAIM_KEPT_MS),
      next_race_at: this.nextRaceAt,
      cooldown_until: this.cooldownUntil !== null && this.cooldownUntil > now ? this.cooldownUntil : null,
      scoreboard: this.scoreboard(),
      nudges: {
        targets: NUDGE_TARGETS.map((x) => ({
          id: x.id,
          label: x.label,
          votes: [...this.nudge.votes.values()].filter((v) => v === x.id).length,
          arrive_at: this.nudge.nextHoldAt + this.opts.travelSecs * 1000,
        })),
        holding: this.nudge.holding,
        holding_label: NUDGE_TARGETS.find((x) => x.id === this.nudge.holding)?.label ?? null,
        next_hold_at: this.nudge.nextHoldAt,
        // The mock decides when the camera leaves, so no next hold is ever decided ahead.
        next: null,
        next_label: null,
        arrive_next_at: null,
        leading: this.leadingStop(),
        opened_at: this.nudge.openedAt,
        closes_at: this.nudge.nextHoldAt,
      },
      viewers: { count: this.clients.size, active: [...this.clients].filter((c) => c.visible).length },
      actions: this.actions,
      aggregates: this.aggregates.map((a) => ({
        ...a,
        frame_url: this.aggregateUp(a) ? this.opts.renderers.frameUrl(this.streamId(a.id)) : null,
        frame_url_v4: this.aggregateUp(a) ? this.opts.renderers.frameUrlV4(this.streamId(a.id)) : null,
      })),
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
    const client: Client = { ws, visible: true, viewer: `v-${k + 1}`, verified: this.sitekey === null, claim: null };
    this.clients.add(client);
    this.sendTo(client, { type: "hello", viewer: client.viewer, ...(this.sitekey ? { turnstile: this.sitekey } : {}) });
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
      } else if (m["type"] === "visibility") {
        client.visible = m["visible"] === true;
        this.markDirty();
      } else if (m["type"] === "seat") {
        // The mock seats every page; the ticket is not checked.
      } else if (m["type"] === "verify") {
        const ok = this.sitekey === null || m["token"] === "mock-pass";
        if (ok) client.verified = true;
        this.sendTo(client, { type: "verified", ok, ...(ok ? {} : { reason: "the check did not pass" }) });
      } else if (m["type"] === "intent" && typeof m["id"] === "string" && typeof m["name"] === "string") {
        const args = typeof m["args"] === "object" && m["args"] !== null ? (m["args"] as Record<string, unknown>) : {};
        void this.intent(client, m["id"], m["name"], args);
      } else {
        this.sendTo(client, { type: "error", reason: "unknown message" });
      }
    });
    ws.on("close", () => {
      this.clients.delete(client);
      if (client.claim && client.claim.phase !== "ended") this.endClaim(client.claim, "The viewer left.");
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
    const act = { viewer: client.viewer, id, name, args };
    const p = client.verified ? this.apply(act, client) : Promise.resolve(this.refuse(act, "verify first"));
    this.receipts.set(id, p);
    const r = await p;
    this.receipts.set(id, r);
    this.sendTo(client, r);
  }

  private log(c: Act, summary: string, outcome: string): void {
    // A refused word's text appears nowhere, not even in its arguments.
    const args = c.name === "word" && outcome === "refused" ? {} : c.args;
    this.actions = [...this.actions, { at: this.now(), viewer: c.viewer, id: c.id, name: c.name, args, summary, outcome }].slice(-50);
  }

  private refuse(c: Act, reason: string): Receipt {
    this.log(c, reason, "refused");
    return { type: "receipt", id: c.id, outcome: "refused", reason, version: this.markDirty() };
  }

  private committed(c: Act, summary: string): Receipt {
    this.appliedIds.push(c.id);
    this.log(c, summary, "committed");
    return { type: "receipt", id: c.id, outcome: "committed", version: this.markDirty() };
  }

  private sceneNow(): number {
    return sceneT(this.opts.epochUnixMs, this.now());
  }

  private raceRunning(): boolean {
    return this.race !== null && (this.race.fcvm_done_at === null || this.race.ec2_done_at === null);
  }

  private async apply(c: Act, client: Client): Promise<Receipt> {
    const { name, args } = c;
    switch (name) {
      case "relaunch": {
        const want = String(args["tile"]);
        const r = this.tiles.find((t) => t.id === want || t.slot === want);
        if (!r) return this.refuse(c, `no tile ${want}`);
        if (!r.slot) return this.refuse(c, `no GPU behind ${r.id}`);
        if (this.raceRunning()) return this.refuse(c, "a restart of every GPU is running");
        if (r.phase !== "running") return this.refuse(c, `${r.id} is not running`);
        this.relaunch(r, c.viewer, 0);
        return this.committed(c, `relaunch ${r.slot}`);
      }
      case "restart_now": {
        if (this.raceRunning()) return this.refuse(c, "a restart of every GPU is already running");
        const now = this.now();
        if (this.cooldownUntil !== null && now < this.cooldownUntil) {
          return this.refuse(c, `the next restart is allowed in ${Math.ceil((this.cooldownUntil - now) / 1000)} s`);
        }
        const n = this.startRace(c.viewer);
        return this.committed(c, `relaunch all (${n} tiles)`);
      }
      case "nudge": {
        const target = NUDGE_TARGETS.find((x) => x.id === args["target"]);
        if (!target) return this.refuse(c, "no such stop");
        this.nudge.votes.set(c.viewer, target.id);
        return this.committed(c, `nudge ${target.id}`);
      }
      case "word": {
        const text = typeof args["text"] === "string" ? args["text"].replace(/\s+/g, " ").trim() : "";
        // The sender's style: lockup unless they picked another.
        const style = args["text_style"] ?? "lockup";
        if (style !== "lockup" && style !== "skywriting" && style !== "landmark") return this.refuse(c, "text_style is lockup, skywriting or landmark");
        await new Promise((r) => setTimeout(r, this.opts.checkMs));
        if (text.length === 0 || text.length > 28) return this.refuse(c, "a word is 1 to 28 characters");
        const bad = !/^[A-Za-z0-9 .,!?'-]+$/.test(text) || this.opts.refusedWords.some((w) => text.toLowerCase().includes(w.toLowerCase()));
        if (bad) return this.refuse(c, "the text check refused it");
        const t = this.sceneNow();
        const cam = cameraAt(this.camera, t);
        const k = this.words.length;
        const ang = k * 2.4;
        const rad = 0.25 + 0.12 * (k % 4);
        const pos = { x: (cam["x"] ?? 0) + Math.cos(ang) * rad * 1.6, y: (cam["y"] ?? 0) + Math.sin(ang) * rad - 0.1 };
        this.words = [...this.words, { text, t, pos, text_style: style }].slice(-16);
        return this.committed(c, `word ${JSON.stringify(text)}`);
      }
      case "light": {
        const pos = args["pos"];
        const color = rgbArray(args["color"]);
        if (!Array.isArray(pos) || pos.length !== 3 || !pos.every((x) => typeof x === "number" && Number.isFinite(x))) {
          return this.refuse(c, "a light's pos is [x, y, z]");
        }
        if (!color) return this.refuse(c, "a light's color is [r, g, b]");
        // The mock's world is flat: it shows the light where it appears on the image.
        const t = this.sceneNow();
        const keys = this.fleetState().scene.camera;
        const pose = poseAt(keys, t);
        const at = pose ? projectPoint(pose, pos as Vec3, IMAGE_ASPECT) : null;
        if (!at) return this.refuse(c, "the light is behind the camera");
        const cam = cameraAt(this.camera, t);
        const zoom = cam["zoom"] ?? 1;
        const world = { x: ((at.u - 0.5) * IMAGE_ASPECT) / zoom + (cam["x"] ?? 0), y: (at.v - 0.5) / zoom + (cam["y"] ?? 0) };
        this.lights = [...this.lights, { pos: world, world: pos as Vec3, color, t }].slice(-32);
        return this.committed(c, "light");
      }
      case "claim_gpu": {
        if (client.claim && client.claim.phase !== "ended") return this.refuse(c, "you already have one");
        const lent = this.lent();
        if (lent.length >= this.opts.borrowCap) {
          const back = Math.min(...lent.map((t) => t.borrowed!.until));
          return this.refuse(c, `all ${this.opts.borrowCap} GPUs we lend are out; the next comes back in ${Math.max(1, Math.ceil((back - this.now()) / 1000))} s`);
        }
        if (this.raceRunning()) return this.refuse(c, "a restart of every GPU is running; try again when it is done");
        const tile = [...this.tiles].reverse().find((t) => t.half === "fcvm" && t.slot && t.phase === "running" && !t.borrowed);
        if (!tile) return this.refuse(c, "no fcvm GPU is streaming to lend");
        this.startClaim(client, tile);
        return this.committed(c, `claim ${tile.slot}`);
      }
      case "release_gpu": {
        const cl = client.claim;
        if (!cl || cl.phase === "ended") return this.refuse(c, "you have no GPU to give back");
        this.endClaim(cl, "You gave the GPU back.");
        return this.committed(c, `release ${cl.tile.slot}`);
      }
      case "claim_autopilot": {
        const cl = client.claim;
        if (!cl || cl.phase === "ended") return this.refuse(c, "claim_autopilot needs a GPU of your own (claim_gpu)");
        if (typeof args["on"] !== "boolean") return this.refuse(c, "args.on must be true or false");
        if (args["on"] === cl.autopilot) return this.committed(c, `autopilot already ${cl.autopilot ? "on" : "off"}`);
        if (args["on"]) {
          cl.autopilot = true;
          if (cl.phase === "running") this.flyMock(cl, this.sceneNow() + 1);
        } else {
          this.stopMockFlight(cl, this.sceneNow());
        }
        this.pushClaim(cl);
        return this.committed(c, `autopilot ${cl.autopilot ? "on" : "off"}`);
      }
      case "claim_camera": {
        const cl = client.claim;
        if (!cl || cl.phase !== "running") return this.refuse(c, "you have no GPU streaming");
        // A drag takes the camera from the autopilot where it is.
        if (cl.autopilot) this.stopMockFlight(cl, this.sceneNow());
        const drag = args["drag"] as Record<string, unknown> | undefined;
        const num = (k: string) => (typeof drag?.[k] === "number" ? (drag[k] as number) : 0);
        const yaw = num("yaw_deg");
        const pitch = num("pitch_deg");
        const dive = num("dive");
        if (!drag || ![yaw, pitch, dive].every(Number.isFinite)) return this.refuse(c, "camera drag is yaw_deg, pitch_deg, dive");
        const t = this.sceneNow();
        // A move still easing in is the base for the next one, so drags that
        // arrive together add up.
        const last = cl.camera[cl.camera.length - 1]!;
        const easing = last.t > t;
        const now = cameraAt(cl.camera, t);
        const cur = easing ? last : { t, x: now["x"] ?? 0, y: now["y"] ?? 0, zoom: now["zoom"] ?? 1 };
        const next: CameraKey = {
          t: Math.max(t, cur.t) + 0.05,
          x: cur.x + ((yaw / 90) * IMAGE_ASPECT) / cur.zoom,
          y: cur.y - pitch / 50 / cur.zoom,
          zoom: Math.min(6, Math.max(0.35, cur.zoom * Math.exp(dive * 0.2))),
          intent: c.id,
        };
        const kept = cl.camera.filter((k) => k.t >= t - 2).slice(-4);
        cl.camera = easing ? [...kept, next] : [...kept.filter((k) => k.t <= t), { t, x: cur.x, y: cur.y, zoom: cur.zoom }, next];
        this.pushClaim(cl);
        return this.committed(c, "claim camera");
      }
      default:
        return this.refuse(c, `unknown intent "${name}"`);
    }
  }

  // ---- restarts ---------------------------------------------------------------

  private pending(h: Half): number {
    const ra = this.race;
    if (!ra) return 0;
    return this.tiles.filter((t) => t.half === h && t.slot && !t.borrowed && !((t.lastRelaunch?.first_frame_at ?? 0) >= ra.started_at && t.phase === "running")).length;
  }

  private lent(): TileRuntime[] {
    return this.tiles.filter((t) => t.borrowed !== null);
  }

  /** A restart of every GPU. Returns how many tiles it restarts. */
  private startRace(by: string): number {
    const now = this.now();
    // A tile lent to a visitor sits the race out.
    const live = this.tiles.filter((t) => t.slot && !t.borrowed);
    const p = this.rand();
    const waitMs =
      this.forcedWaitMs ??
      (p < this.opts.capacityWaitProb ? Math.round(this.opts.capacityWaitMs[0] + this.rand() * (this.opts.capacityWaitMs[1] - this.opts.capacityWaitMs[0])) : 0);
    this.forcedWaitMs = null;
    this.race = {
      started_at: now,
      by,
      fcvm_done_at: null,
      ec2_done_at: null,
      fcvm_gpus: live.filter((t) => t.half === "fcvm").length,
      ec2_gpus: live.filter((t) => t.half === "ec2").length,
      ec2_capacity_wait_ms: 0,
      ec2_waiting_since: waitMs > 0 ? now : null,
    };
    this.cooldownUntil = now + this.opts.cooldownSecs * 1000;
    for (const r of live) this.relaunch(r, by, r.half === "ec2" ? waitMs : 0);
    if (waitMs > 0) {
      const race = this.race;
      this.later(waitMs, () => {
        if (this.race !== race) return;
        race.ec2_capacity_wait_ms = waitMs;
        race.ec2_waiting_since = null;
        this.markDirty();
      });
    }
    // A half with nothing to restart is done at once.
    if (this.race.fcvm_gpus === 0) this.race.fcvm_done_at = now;
    if (this.race.ec2_gpus === 0) this.race.ec2_done_at = now;
    // A half done at once records nothing; a race with neither half is not a race.
    this.markDirty();
    return live.length;
  }

  private relaunch(r: TileRuntime, by: string, waitMs: number): void {
    if (r.timer) clearTimeout(r.timer);
    r.generation += 1;
    r.failure = null;
    const gen = r.generation;
    const clickedAt = this.now();
    const fc = r.half === "fcvm";
    const spread = fc ? 0.26 : 0.66;
    const total = (fc ? this.opts.fcvmRelaunchMs : this.opts.ec2RelaunchMs) * (1 - spread / 2 + spread * this.rand());
    const plan = (fc ? FCVM_PLAN : EC2_PLAN).map((p) => ({ ...p, ms: Math.round(total * p.share) }));
    const phases: Record<string, number> = waitMs > 0 ? { capacity_wait_ms: waitMs } : {};
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
        phases["gpu_ms"] = Math.round(p.ms * GPU_SHARE_OF_RESTORE);
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
      if (fc) this.countRestore(done);
      this.checkRace();
      this.wakeCheck();
      this.markDirty();
    };
    const begin = () => {
      if (r.generation !== gen) return;
      r.phase = plan[0]!.phase;
      this.markDirty();
      r.timer = setTimeout(step, Math.max(0, plan[0]!.ms - RELAUNCHING_MS));
    };
    r.timer = setTimeout(() => {
      if (r.generation !== gen) return;
      if (waitMs > 0) {
        // Spot capacity refused: the instance waits before it is launched.
        r.phase = "waiting_capacity";
        this.markDirty();
        r.timer = setTimeout(begin, Math.max(0, waitMs - RELAUNCHING_MS));
      } else {
        begin();
      }
    }, RELAUNCHING_MS);
  }

  private checkRace(): void {
    const ra = this.race;
    if (!ra) return;
    const now = this.now();
    if (ra.fcvm_done_at === null && this.pending("fcvm") === 0) ra.fcvm_done_at = now;
    if (ra.ec2_done_at === null && this.pending("ec2") === 0) ra.ec2_done_at = now;
    if (ra.fcvm_done_at !== null && ra.ec2_done_at !== null && !this.recorded.has(ra)) {
      this.recorded.add(ra);
      const wait = ra.ec2_capacity_wait_ms;
      this.races = [
        ...this.races,
        {
          fcvm_ms: ra.fcvm_done_at - ra.started_at,
          ec2_ms: ra.ec2_done_at - ra.started_at - wait,
          wait_ms: wait,
          fcvm_gpus: ra.fcvm_gpus,
          ec2_gpus: ra.ec2_gpus,
        },
      ].slice(-RACES_KEPT);
    }
  }

  /** A GPU VM restored: its GPU came from the VM before it, so with the scrub shipped it was scrubbed on the way. */
  private countRestore(at: number): void {
    this.restoresTotal += 1;
    if (this.gpuScrub) this.scrubsTotal += 1;
    this.restoreTimes = [...this.restoreTimes.filter((t) => t > at - 3_600_000), at];
  }

  // ---- the clock: the owner's cadence, the directed camera, retries ----------------

  private tick(): void {
    const now = this.now();
    this.powerTick(now);
    if (this.nextRaceAt !== null && now >= this.nextRaceAt) {
      // Skip a beat that falls inside a running race.
      if (!this.raceRunning()) {
        // The cadence is not an intent: the race's `by` says so, and `actions` has nothing.
        this.startRace("schedule");
      }
      const every = this.opts.raceEverySecs * 1000;
      // No cadence: a beat set by hand fires once.
      if (every <= 0) this.nextRaceAt = null;
      else while (this.nextRaceAt <= now) this.nextRaceAt += every;
      this.markDirty();
    }
    if (now >= this.nudge.nextHoldAt) this.nextStop(now);
    for (const t of this.tiles) {
      if (t.failure?.kind === "relaunch_failed" && t.failure.retry_at !== null && now >= t.failure.retry_at && t.slot) {
        this.relaunch(t, "retry", 0);
      }
    }
  }

  /** The stop with the most votes (ties: the next in order). */
  private leadingStop(): string {
    const order = NUDGE_TARGETS.map((x) => x.id);
    const cur = this.nudge.holding === null ? -1 : order.indexOf(this.nudge.holding);
    const counts = new Map<string, number>();
    for (const v of this.nudge.votes.values()) counts.set(v, (counts.get(v) ?? 0) + 1);
    let best = order[(cur + 1) % order.length]!;
    let bestVotes = counts.get(best) ?? 0;
    for (let k = 1; k <= order.length; k++) {
      const id = order[(cur + k) % order.length]!;
      const n = counts.get(id) ?? 0;
      if (n > bestVotes) {
        best = id;
        bestVotes = n;
      }
    }
    return best;
  }

  /** The camera leaves for the leading stop. */
  private nextStop(now: number): void {
    const best = this.leadingStop();
    const target = NUDGE_TARGETS.find((x) => x.id === best)!;
    const t = sceneT(this.opts.epochUnixMs, now);
    const here = cameraAt(this.camera, t);
    const words = target.id === "words" && this.words.length > 0 ? this.words[this.words.length - 1]!.pos : null;
    this.camera = [
      ...this.camera.filter((k) => k.t < t && k.t >= t - 30).slice(-2),
      { t, x: here["x"] ?? 0, y: here["y"] ?? 0, zoom: here["zoom"] ?? 1 },
      { t: t + this.opts.travelSecs, x: words?.x ?? target.x, y: words?.y ?? target.y, zoom: target.zoom },
    ];
    this.nudge = { holding: best, nextHoldAt: now + (this.opts.holdSecs + this.opts.travelSecs) * 1000, openedAt: now, votes: new Map() };
    this.markDirty();
  }

  // ---- the tiling ---------------------------------------------------------------------

  private hostOf(slot: number): string {
    return `mock-host-${1 + Math.floor((slot - 1) / this.opts.gpusPerHost)}`;
  }

  private newTile(half: Half, k: number, rect: Rect, running: boolean): TileRuntime {
    const slot = half === "fcvm" ? `slot-${k}` : `ec2-${k}`;
    return {
      id: `${half}:${k}`,
      half,
      rect,
      slot,
      index: k,
      host: half === "fcvm" ? `g4dn.metal · ${this.hostOf(k)}` : `g4dn.xlarge · mock-i-${String(k).padStart(4, "0")}`,
      phase: running ? "running" : "relaunching",
      generation: 1,
      gpu: { sbdf: half === "fcvm" ? `0000:${(0x18 + k).toString(16)}:00.0` : "0000:00:1e.0", name: "Tesla T4" },
      gpuStats: { temp_c: 55, power_w: 60, util_pct: 90 },
      lastRelaunch: null,
      failure: null,
      borrowed: null,
      timer: null,
    };
  }

  /**
   * Lay the tiles out for the GPUs live: each side as wide as its share. With
   * `atT` (a re-tile), the renderers switch to the new rectangles at that scene t,
   * new GPUs start, and the streams of GPUs that left close after it.
   */
  private applyLayout(atT: number | null): void {
    const r = this.opts.renderers;
    const slots = [...this.fcvmSlots].sort((a, b) => a - b);
    const byHost = new Map<string, number[]>();
    for (const k of slots) byHost.set(this.hostOf(k), [...(byHost.get(this.hostOf(k)) ?? []), k]);
    const lay = mockLayout([...byHost].map(([host, ks]) => ({ host, gpus: ks.length })), this.ec2Count, IMAGE_ASPECT);
    const old = new Map(this.tiles.map((t) => [t.id, t]));
    const next: TileRuntime[] = [];
    const place = (half: Half, k: number, rect: Rect) => {
      const id = `${half}:${k}`;
      const t = old.get(id);
      if (t) {
        old.delete(id);
        t.rect = rect;
        if (atT === null) r.setTile(t.slot!, rect);
        else r.retile(t.slot!, rect, atT);
        next.push(t);
      } else {
        const t2 = this.newTile(half, k, rect, atT === null);
        r.setTile(t2.slot!, rect);
        r.setAvailable(t2.slot!, true);
        next.push(t2);
        // A GPU that joins starts its VM like any restart.
        if (atT !== null) this.relaunch(t2, "join", 0);
      }
    };
    for (const h of lay.hosts) byHost.get(h.host)!.forEach((k, i) => place("fcvm", k, h.rects[i]!));
    lay.ec2.forEach((rect, i) => place("ec2", i + 1, rect));
    // GPUs that left: their streams keep the old tiling's frames until the page swaps.
    for (const t of old.values()) {
      if (t.timer) clearTimeout(t.timer);
      t.timer = null;
      const slot = t.slot!;
      if (atT === null) r.removeTile(slot);
      // Unless the GPU came back meanwhile.
      else this.later(this.msUntil(atT) + 5000, () => {
        if (!this.tiles.some((x) => x.slot === slot)) r.removeTile(slot);
      });
    }
    this.tiles = next;
    const black = (rect: Rect) => () => this.tiles.filter((t) => t.phase !== "running" && within(rect, t.rect)).map((t) => t.rect);
    const aggs = [
      ...lay.hosts.map((h) => ({ id: `agg:${h.host}`, half: "fcvm" as Half, host: h.host as string | null, rect: h.band, tiles: byHost.get(h.host)!.map((k) => `fcvm:${k}`) })),
      ...(lay.ec2Region ? [{ id: "agg:ec2", half: "ec2" as Half, host: null, rect: lay.ec2Region, tiles: lay.ec2.map((_, i) => `ec2:${i + 1}`) }] : []),
    ];
    const was = new Map(this.aggregates.map((a) => [a.id, a]));
    for (const a of aggs) {
      const sid = this.streamId(a.id);
      if (was.has(a.id) && atT !== null) {
        r.setTile(sid, was.get(a.id)!.rect, black(a.rect));
        r.retile(sid, a.rect, atT);
      } else {
        r.setTile(sid, a.rect, black(a.rect));
        r.setAvailable(sid, true);
      }
      was.delete(a.id);
    }
    for (const a of was.values()) {
      const sid = this.streamId(a.id);
      if (atT === null) r.removeTile(sid);
      else this.later(this.msUntil(atT) + 5000, () => {
        if (!this.aggregates.some((x) => this.streamId(x.id) === sid)) r.removeTile(sid);
      });
    }
    this.aggregates = aggs;
  }

  /** Milliseconds from now until scene t `t`. */
  private msUntil(t: number): number {
    return Math.max(0, this.opts.epochUnixMs + t * 1000 - this.now());
  }

  /** Re-tile for the GPUs live now: a new layout generation starting half a second ahead. */
  retile(): void {
    const atFrame = Math.floor(this.sceneNow() * FRAME_RATE) + RETILE_LEAD_FRAMES;
    this.layout = { generation: this.layout.generation + 1, from_frame: atFrame };
    this.applyLayout(atFrame / FRAME_RATE);
    this.markDirty();
  }

  /** Set how many GPUs each side has: fcvm slots are reused lowest first and leave highest first. */
  setGpus(fcvm: number, ec2: number): void {
    const slots = [...this.fcvmSlots].sort((a, b) => a - b);
    while (slots.length > fcvm) slots.pop();
    for (let k = 1; slots.length < fcvm; k++) if (!slots.includes(k)) slots.push(k);
    this.fcvmSlots = slots;
    this.ec2Count = Math.max(0, ec2);
    for (const h of new Set(slots.map((k) => this.hostOf(k)))) if (!this.hosts.some((x) => x.name === h)) this.hosts.push({ name: h, draining: false });
    this.hosts = this.hosts.filter((h) => slots.some((k) => this.hostOf(k) === h.name));
    this.retile();
  }

  // ---- scaling to zero ---------------------------------------------------------------

  /** An aggregate serves only while its host renders. */
  private aggregateUp(a: { host: string | null; half: Half }): boolean {
    if (this.power.state === "awake") return true;
    return this.rendering.has(a.host ?? "ec2");
  }

  private powerTick(now: number): void {
    const visible = [...this.clients].filter((c) => c.visible).length;
    const after = (this.opts.sleepAfterSecs ?? 0) * 1000;
    if (this.power.state === "awake" && after > 0) {
      if (visible > 0) {
        this.idleSince = null;
      } else {
        this.idleSince ??= now;
        if (now - this.idleSince >= after) this.sleep();
      }
    }
    if (this.power.state === "asleep" && visible > 0 && !this.holdAsleep) this.wake();
  }

  /**
   * No viewers: as the coordinator, nothing is placed (`tiles` and `aggregates` empty,
   * no layout rectangles), the schedule waits, loans end.
   */
  sleep(): void {
    if (this.power.state !== "awake") return;
    const now = this.now();
    for (const c of this.clients) if (c.claim && c.claim.phase !== "ended") this.endClaim(c.claim, "The fleet went to sleep.");
    for (const t of this.tiles) {
      if (t.timer) clearTimeout(t.timer);
      t.timer = null;
      t.borrowed = null;
      if (t.slot) this.opts.renderers.setAvailable(t.slot, false);
    }
    for (const a of this.aggregates) this.opts.renderers.setAvailable(this.streamId(a.id), false);
    this.slept = { fcvm: [...this.fcvmSlots], ec2: this.ec2Count };
    this.fcvmSlots = [];
    this.ec2Count = 0;
    this.retile();
    this.rendering.clear();
    this.sleptRaceAt = this.nextRaceAt;
    this.nextRaceAt = null;
    this.power = { ...this.power, state: "asleep", asleep_since: now, woke_at: null, hosts: [] };
    this.idleSince = null;
    this.markDirty();
  }

  /**
   * A viewer arrived. The EC2 half launches its instances at once; each GPU host goes
   * through its phases and its GPUs join the wall when its agent connects (with no tile
   * on the wall, every GPU joins at once, restoring like any restart). Awake once every
   * placed tile runs.
   */
  wake(): void {
    if (this.power.state !== "asleep" || this.slept === null) return;
    const start = this.now();
    const slept = this.slept;
    const names = [...new Set(slept.fcvm.map((k) => this.hostOf(k)))];
    this.power = { ...this.power, state: "waking", woke_at: start, hosts: names.map((host) => ({ host, phases: [] })) };
    this.ec2Count = slept.ec2;
    this.retile();
    const total = this.opts.wakeMs ?? 120_000;
    const shares = [0, 0.4, 0.6, 0.72];
    names.forEach((host, i) => {
      const jitter = (i * 0.04) / Math.max(1, names.length - 1);
      ["instance_pending", "booted", "store_ready", "agent_connected"].forEach((phase, k) => {
        this.later(total * (shares[k]! + jitter * shares[k]!), () => {
          if (this.power.state !== "waking" || this.power.woke_at !== start) return;
          const h = this.power.hosts.find((x) => x.host === host)!;
          h.phases = [...h.phases, { phase, at: this.now() }];
          if (phase === "agent_connected") {
            this.fcvmSlots = [...this.fcvmSlots, ...slept.fcvm.filter((s) => this.hostOf(s) === host)];
            this.retile();
          }
          this.wakeCheck();
          this.markDirty();
        });
      });
    });
    this.markDirty();
  }

  /** During a wake: a host whose placed tiles all run reports `tiles_rendering`; all of them and the EC2 half, awake. */
  private wakeCheck(): void {
    if (this.power.state !== "waking") return;
    const now = this.now();
    const runs = (host: string | null) =>
      this.tiles.filter((t) => (host === null ? t.half === "ec2" : t.half === "fcvm" && t.host?.endsWith(host))).every((t) => t.phase === "running");
    for (const h of this.power.hosts) {
      const has = (p: string) => h.phases.some((x) => x.phase === p);
      if (has("agent_connected") && !has("tiles_rendering") && runs(h.host)) {
        h.phases = [...h.phases, { phase: "tiles_rendering", at: now }];
        this.rendering.add(h.host);
      }
    }
    if (runs(null)) this.rendering.add("ec2");
    if (this.power.hosts.every((h) => h.phases.some((p) => p.phase === "tiles_rendering")) && runs(null)) {
      const started = this.power.woke_at!;
      const gpus = this.tiles.filter((t) => t.slot).length;
      this.power = { ...this.power, state: "awake", last_wake: { started_at: started, awake_at: now, gpus, ms: now - started } };
      this.nextRaceAt = this.sleptRaceAt;
      this.sleptRaceAt = null;
      this.slept = null;
      this.rendering.clear();
    }
    this.markDirty();
  }

  private gpuTick(): void {
    const t = this.now() / 1000;
    for (const r of this.tiles) {
      if (!r.gpuStats) continue;
      const running = r.phase === "running";
      const k = r.index + (r.half === "ec2" ? 50 : 0);
      r.gpuStats = {
        temp_c: Math.round(running ? 62 + 5 * Math.sin(t / 30 + k) : 38),
        power_w: Math.round(running ? 64 + 5 * Math.sin(t / 7 + k * 1.3) : 27),
        util_pct: Math.round(running ? 93 + 5 * Math.sin(t / 5 + k) : 0),
      };
    }
    this.markDirty();
  }

  // ---- a viewer's own GPU ------------------------------------------------------------

  private claimMsg(cl: ClaimRuntime) {
    return {
      type: "claim",
      claim: {
        id: cl.id,
        tile: cl.tile.id,
        phase: cl.phase,
        frame_url: cl.phase === "running" ? this.opts.renderers.frameUrl(cl.id) : null,
        frame_url_v4: cl.phase === "running" ? this.opts.renderers.frameUrlV4(cl.id) : null,
        clicked_at: cl.clicked_at,
        first_frame_at: cl.first_frame_at,
        expires_at: cl.expires_at,
        gpu: "Tesla T4",
        phases: cl.phases,
        camera: cl.camera,
        autopilot: cl.autopilot,
        ended: cl.ended,
      },
    };
  }

  /**
   * Fly the mock autopilot from scene t `start` (where the loop's phase is chosen so the
   * camera continues from where it is held), keys every half second, 8 s ahead.
   */
  private flyMock(cl: ClaimRuntime, start: number): void {
    const t = this.sceneNow();
    const here = cameraAt(cl.camera, t);
    if (cl.flightT0 === null) {
      cl.flightT0 = start;
      const kept = cl.camera.filter((k) => k.t < t).slice(-2);
      const p = mockFlight(0);
      cl.camera = [...kept, { t, x: here["x"] ?? 0, y: here["y"] ?? 0, zoom: here["zoom"] ?? 1 }, { t: start, ...p }];
    }
    const t0 = cl.flightT0;
    const last = cl.camera[cl.camera.length - 1]!.t;
    for (let k = Math.floor((last - t0) / 0.5) + 1; t0 + k * 0.5 <= t + 8; k++) cl.camera.push({ t: t0 + k * 0.5, ...mockFlight(k * 0.5) });
    cl.camera = cl.camera.filter((k) => k.t >= t - 3);
    if (cl.flightTimer) clearTimeout(cl.flightTimer);
    cl.flightTimer = this.later(2000, () => {
      cl.flightTimer = null;
      if (cl.autopilot && cl.phase === "running") {
        this.flyMock(cl, start);
        this.pushClaim(cl);
      }
    });
  }

  /** Stop the mock autopilot: the camera holds where it is at scene t `t`. */
  private stopMockFlight(cl: ClaimRuntime, t: number): void {
    cl.autopilot = false;
    cl.flightT0 = null;
    if (cl.flightTimer) clearTimeout(cl.flightTimer);
    cl.flightTimer = null;
    const here = cameraAt(cl.camera, t);
    cl.camera = [...cl.camera.filter((k) => k.t < t).slice(-2), { t, x: here["x"] ?? 0, y: here["y"] ?? 0, zoom: here["zoom"] ?? 1 }];
  }

  private pushClaim(cl: ClaimRuntime): void {
    this.sendTo(cl.client, this.claimMsg(cl));
  }

  private startClaim(client: Client, tile: TileRuntime): void {
    const now = this.now();
    const t = this.sceneNow();
    const cl: ClaimRuntime = {
      id: `claim-${++this.nextClaim}`,
      tile,
      client,
      phase: "restoring",
      clicked_at: now,
      first_frame_at: null,
      expires_at: null,
      phases: {},
      camera: [{ t, ...mockFlight(0) }],
      autopilot: true,
      flightT0: null,
      flightTimer: null,
      ended: null,
      timer: null,
    };
    client.claim = cl;
    // The tile's GPU leaves the wall for the visitor's private VM.
    if (tile.timer) clearTimeout(tile.timer);
    tile.timer = null;
    tile.generation += 1;
    tile.phase = "borrowed";
    tile.borrowed = { until: now + this.opts.fcvmRelaunchMs + this.opts.claimSecs * 1000, claim: cl };
    this.opts.renderers.setAvailable(tile.slot!, false);
    this.markDirty();
    this.opts.renderers.setTile(cl.id, { x: 0, y: 0, w: 1, h: 1 }, () => [], () => ({ ...this.patternScene(), camera: cl.camera }));
    this.opts.renderers.setAvailable(cl.id, false);
    const total = this.opts.fcvmRelaunchMs * (0.87 + 0.26 * this.rand());
    cl.timer = this.later(total, () => {
      if (cl.phase !== "restoring") return;
      const done = this.now();
      const restore = Math.round(total * 0.62);
      cl.phases = {
        restore_ms: restore,
        gpu_ms: Math.round(restore * GPU_SHARE_OF_RESTORE),
        container_ms: Math.round(total * 0.24),
        first_frame_ms: Math.round(total * 0.14),
        total_ms: done - now,
      };
      cl.phase = "running";
      cl.first_frame_at = done;
      this.countRestore(done);
      cl.expires_at = done + this.opts.claimSecs * 1000;
      if (tile.borrowed?.claim === cl) tile.borrowed.until = cl.expires_at;
      this.opts.renderers.setAvailable(cl.id, true);
      this.markDirty();
      if (cl.autopilot) this.flyMock(cl, this.sceneNow() + 0.5);
      this.pushClaim(cl);
      cl.timer = this.later(this.opts.claimSecs * 1000, () => this.endClaim(cl, "Your minute is up. The GPU went back to the pool."));
    });
    this.pushClaim(cl);
  }

  private endClaim(cl: ClaimRuntime, why: string): void {
    if (cl.phase === "ended") return;
    if (cl.timer) {
      clearTimeout(cl.timer);
      this.timers.delete(cl.timer);
    }
    cl.phase = "ended";
    cl.ended = why;
    if (cl.flightTimer) clearTimeout(cl.flightTimer);
    cl.flightTimer = null;
    this.opts.renderers.removeTile(cl.id);
    // The GPU comes back to the wall: its tile restores like any restart.
    const tile = cl.tile;
    if (tile.borrowed?.claim === cl) {
      tile.borrowed = null;
      if (tile.slot) this.relaunch(tile, "return", 0);
    }
    this.pushClaim(cl);
    this.markDirty();
  }

  // ---- faults, for tests and screenshots -------------------------------------------------

  private fail(tile: TileRuntime, message: string, retryMs: number): void {
    if (tile.timer) clearTimeout(tile.timer);
    tile.timer = null;
    tile.generation += 1;
    tile.phase = "failed";
    tile.failure = { kind: "relaunch_failed", message, retry_at: this.now() + retryMs };
    if (tile.slot) this.opts.renderers.setAvailable(tile.slot, false);
    this.markDirty();
  }

  private reclaim(hostName: string): boolean {
    const host = this.hosts.find((h) => h.name === hostName);
    if (!host) return false;
    // As the coordinator does: the host's tiles leave the tiling in the same event
    // (the GPUs left are re-tiled at once); `failures` and the draining host say why.
    host.draining = true;
    const since = this.now();
    this.failures = [...this.failures.filter((f) => f.host !== hostName), { kind: "host_reclaimed", host: hostName, since, retiling: true }];
    for (const t of this.tiles.filter((x) => x.half === "fcvm" && x.host?.endsWith(hostName) && x.slot)) {
      if (t.timer) clearTimeout(t.timer);
      t.timer = null;
    }
    this.fcvmSlots = this.fcvmSlots.filter((k) => this.hostOf(k) !== hostName);
    this.retile();
    this.later(RECLAIM_KEPT_MS, () => {
      this.hosts = this.hosts.filter((h) => h !== host);
      this.failures = this.failures.filter((f) => f.since !== since || f.host !== hostName);
      this.markDirty();
    });
    return true;
  }

  private replaceHost(hostName: string): void {
    const back = this.hosts.find((h) => h.name === hostName);
    if (back) back.draining = false;
    else this.hosts.push({ name: hostName, draining: false });
    this.failures = this.failures.filter((f) => f.host !== hostName);
    const n = Number(/(\d+)$/.exec(hostName)?.[1] ?? "0");
    const per = this.opts.gpusPerHost;
    for (let k = (n - 1) * per + 1; k <= n * per; k++) if (!this.fcvmSlots.includes(k)) this.fcvmSlots.push(k);
    this.retile();
  }

  // ---- http: the poster and admin routes ------------------------------------------------

  private posterJpeg(): Promise<Buffer> {
    const now = this.now();
    if (this.poster && now - this.poster.at < 5000) return this.poster.jpeg;
    const black = this.tiles.filter((t) => t.phase !== "running").map((t) => t.rect);
    const jpeg = this.opts.renderers.poster(1280, 720, Math.floor(this.sceneNow() * 30) / 30, black);
    this.poster = { at: now, jpeg };
    jpeg.catch(() => {
      if (this.poster?.jpeg === jpeg) this.poster = null;
    });
    return jpeg;
  }

  private http(req: http.IncomingMessage, res: http.ServerResponse): void {
    const url = new URL(req.url ?? "/", "http://mock");
    const json = (code: number, body: unknown) => {
      res.writeHead(code, { "content-type": "application/json" }).end(JSON.stringify(body));
    };
    const body = (fn: (b: Record<string, unknown>) => unknown) => {
      let text = "";
      req.on("data", (d: Buffer) => (text += d.toString()));
      req.on("end", () => {
        try {
          json(200, fn(JSON.parse(text || "{}") as Record<string, unknown>) ?? { ok: true });
        } catch (e) {
          json(400, { ok: false, error: e instanceof Error ? e.message : String(e) });
        }
      });
    };
    if (req.method === "GET" && url.pathname.startsWith("/media/")) {
      this.media(req, res, url.pathname.slice("/media/".length));
      return;
    }
    if (req.method === "GET" && url.pathname === "/poster.jpg") {
      // The real coordinator holds its poster in memory and answers 404 until it has one
      // (after a restart, before any scene was captured).
      if (this.noPoster) {
        res.writeHead(404, { "content-type": "text/plain", "access-control-allow-origin": "*" }).end("no poster yet");
        return;
      }
      this.posterJpeg().then(
        (jpeg) => res.writeHead(200, { "content-type": "image/jpeg", "access-control-allow-origin": "*", "cache-control": "no-store" }).end(jpeg),
        () => res.writeHead(503).end(),
      );
      return;
    }
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
    if (req.method !== "POST") {
      res.writeHead(404).end();
      return;
    }
    switch (url.pathname) {
      case "/mock/fault":
        body((b) => {
          if (typeof b["skew"] === "number" && typeof b["tile"] === "string") this.opts.renderers.setSkew(b["tile"], b["skew"]);
          if (typeof b["fail"] === "string") {
            const t = this.tiles.find((x) => x.id === b["fail"]);
            if (!t || !t.slot) throw new Error(`no tile with a GPU ${String(b["fail"])}`);
            this.fail(t, typeof b["message"] === "string" ? b["message"] : "the VM did not come back", typeof b["retry_ms"] === "number" ? b["retry_ms"] : 15000);
          }
          if (typeof b["reclaim"] === "string" && !this.reclaim(b["reclaim"])) throw new Error(`no host ${b["reclaim"]}`);
          if (typeof b["replace_host"] === "string") this.replaceHost(b["replace_host"]);
          if (typeof b["fcvm_gpus"] === "number" || typeof b["ec2_gpus"] === "number") {
            this.setGpus(typeof b["fcvm_gpus"] === "number" ? b["fcvm_gpus"] : this.fcvmSlots.length, typeof b["ec2_gpus"] === "number" ? b["ec2_gpus"] : this.ec2Count);
          }
          if (typeof b["renderers_silent"] === "boolean") this.opts.renderers.setSilent(b["renderers_silent"]);
          if (typeof b["no_poster"] === "boolean") this.noPoster = b["no_poster"];
          if (typeof b["ipv6_down"] === "boolean") this.opts.renderers.setV6Down(b["ipv6_down"]);
          return { ok: true };
        });
        return;
      case "/mock/offline":
        body((b) => {
          this.offlineUntil = this.now() + (typeof b["ms"] === "number" ? b["ms"] : 5000);
          for (const c of [...this.clients]) c.ws.terminate();
          return { ok: true };
        });
        return;
      case "/mock/power":
        body((b) => {
          if (typeof b["wake_ms"] === "number") this.opts.wakeMs = b["wake_ms"];
          if (typeof b["hold"] === "boolean") this.holdAsleep = b["hold"];
          if (b["sleep"] === true) this.sleep();
          if (b["wake"] === true) {
            this.holdAsleep = false;
            this.wake();
          }
          return { ok: true, state: this.power.state };
        });
        return;
      case "/mock/media":
        body((b) => {
          this.mediaMissing = b["missing"] === true;
          return { ok: true };
        });
        return;
      case "/mock/turnstile":
        body((b) => {
          this.sitekey = typeof b["sitekey"] === "string" ? b["sitekey"] : null;
          return { ok: true };
        });
        return;
      case "/mock/schedule":
        body((b) => {
          if (typeof b["next_in_ms"] === "number") this.nextRaceAt = this.now() + b["next_in_ms"];
          if (b["next_in_ms"] === null) this.nextRaceAt = null;
          if (typeof b["cooldown_s"] === "number") this.opts.cooldownSecs = b["cooldown_s"];
          if (b["clear_cooldown"] === true) this.cooldownUntil = null;
          if (typeof b["capacity_wait_ms"] === "number") this.forcedWaitMs = b["capacity_wait_ms"];
          if (typeof b["hold_in_ms"] === "number") this.nudge.nextHoldAt = this.now() + b["hold_in_ms"];
          if (typeof b["gpu_scrub"] === "boolean") this.gpuScrub = b["gpu_scrub"];
          // The director sets the time of day per hold; tests set it here.
          if (typeof b["time_of_day"] === "number") this.timeOfDay = b["time_of_day"];
          this.markDirty();
          return { ok: true };
        });
        return;
      default:
        res.writeHead(404).end();
    }
  }

  /** The recording's files, with byte ranges (a browser seeks a looping video by range). */
  private media(req: http.IncomingMessage, res: http.ServerResponse, name: string): void {
    const types: Record<string, string> = { ".mp4": "video/mp4", ".webm": "video/webm", ".vtt": "text/vtt", ".json": "application/json" };
    const type = types[path.extname(name)];
    const dir = this.opts.mediaDir;
    const file = dir && type && /^[A-Za-z0-9._-]+$/.test(name) ? path.join(dir, name) : null;
    let size = -1;
    try {
      if (file && !this.mediaMissing) size = statSync(file).size;
    } catch {
      size = -1;
    }
    const head = { "access-control-allow-origin": "*", "accept-ranges": "bytes", "cache-control": "no-store" };
    if (!file || size < 0) {
      res.writeHead(404, head).end();
      return;
    }
    const m = /^bytes=(\d*)-(\d*)$/.exec(req.headers.range ?? "");
    if (m) {
      const first = m[1] === "" ? Math.max(0, size - Number(m[2])) : Number(m[1]);
      const last = m[1] !== "" && m[2] !== "" ? Math.min(size - 1, Number(m[2])) : size - 1;
      if (first > last || first >= size) {
        res.writeHead(416, { ...head, "content-range": `bytes */${size}` }).end();
        return;
      }
      res.writeHead(206, { ...head, "content-type": type!, "content-length": last - first + 1, "content-range": `bytes ${first}-${last}/${size}` });
      createReadStream(file, { start: first, end: last }).pipe(res);
      return;
    }
    res.writeHead(200, { ...head, "content-type": type!, "content-length": size });
    createReadStream(file).pipe(res);
  }

  async close(): Promise<void> {
    if (this.tickTimer) clearInterval(this.tickTimer);
    if (this.gpuTimer) clearInterval(this.gpuTimer);
    if (this.flushTimer) clearTimeout(this.flushTimer);
    for (const t of this.timers) clearTimeout(t);
    for (const r of this.tiles) if (r.timer) clearTimeout(r.timer);
    for (const c of this.clients) c.ws.terminate();
    this.wss.close();
    await new Promise<void>((r) => this.server.close(() => r()));
  }
}
