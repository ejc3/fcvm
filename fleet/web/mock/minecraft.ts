// The mock fleet's Minecraft servers (PROTOCOL.md, "Minecraft mode" and "Minecraft tile"): the
// coordinator's `state.minecraft` with the four intents and their rules and refusal texts (reset
// one or all, fork into the next server on the host, borrow with a cap, a cooldown and one server
// per client, a reset when a borrow ends), and each server's view socket on a port of its own: a
// small generated world, three bots that wander and build towers of their colour, and the first
// bot driven by the viewer holding the server. The coordinator keys the per-client rules by
// client address; the mock keys them by connection, so one machine can play several viewers.

import http from "node:http";
import { WebSocketServer, type WebSocket } from "ws";
import type { Rect } from "../core/protocol.ts";
import { MATERIALS } from "../core/minecraft.ts";
import type { Reply } from "./phones.ts";

export const MINECRAFT_SERVER = "Pumpkin 3ea7d281 (Java Edition 26.3)";
/** The map's side, in columns (fleet/minecraft: a 128 x 128 region around the spawn). */
export const MC_SIDE = 128;
const BOT_NAMES = ["fcvm_bot1", "fcvm_bot2", "fcvm_bot3"];
const BOT_DYES = ["red", "blue", "yellow"];

const RESET_INTERVAL_MS = 2000;
const RESET_ALL_INTERVAL_MS = 10_000;
const FORK_INTERVAL_MS = 10_000;
/** The tile's drive rules (fleet/minecraft/src/drive.rs). */
const LEASE_MS = 1000;
const MAX_DRIVES_PER_SEC = 30;
const DRIVE_KEYS = ["forward", "back", "left", "right", "jump", "sprint"];
const WALK = 4.3;
const SPRINT = 5.6;

const mat = (name: string) => MATERIALS.indexOf(name as (typeof MATERIALS)[number]);
const M = {
  water: mat("water"), grass: mat("grass"), sand: mat("sand"), stone: mat("stone"), snow: mat("snow"),
  leaves: mat("leaves"), flower: mat("flower"), dirt: mat("dirt"), gravel: mat("gravel"), plant: mat("plant"),
};
/** What stands over a column's ground without being it (the tile's `palette::is_cover`). */
const COVER = new Set([M.leaves, M.water, M.plant, M.flower]);

export interface MockMinecraftOptions {
  count: number;
  /** Servers lent at once at most. */
  cap: number;
  claimSecs: number;
  cooldownSecs: number;
  /** A reset's click to the first chunk. */
  restoreMs: number;
  /** A fork's snapshot of its source; its restore takes `restoreMs`. */
  forkSnapshotMs: number;
  /** The view sockets' port (0 asks the kernel) and the host name frame_url carries. */
  port: number;
  publicHost: string;
  /**
   * The host name frame_url_v4 carries: the mock stands in for the two families with two names
   * for one listener, as its renderers do (renderer.ts).
   */
  publicHostV4?: string;
  /** With a zone, every server's `join` is `mc-<n>.<zone>`; without, null (a `ws://` frame URL). */
  joinZone?: string | null;
  /** Servers on each host (8 unless said): one host holds hundreds when a test asks. */
  perHost?: number;
}

/** With more servers than this the snapshot lists the first this many and every lent one as tiles, and names all in `field` (fleet_core::minecraft::TILES_LISTED). */
export const TILES_LISTED = 16;
/** A reset of many servers brings this many back a second: the mock's stand-in for a host's restore rate. */
const RESTORES_PER_SEC = 32;

/** The key a page knows a host by: 12 hex digits of a hash of its name (fleet_core::minecraft::host_key). */
export function hostKey(name: string): string {
  let h = 0xcbf29ce484222325n;
  for (const b of Buffer.from(name)) h = ((h ^ BigInt(b)) * 0x100000001b3n) & 0xffffffffffffffffn;
  return (h & 0xffffffffffffn).toString(16).padStart(12, "0");
}

/** URLs as what they share and what they do not (fleet_core::minecraft::squeeze). */
export function squeeze(urls: readonly (string | null)[]): { prefix: string; suffix: string; names: (string | null)[] } {
  const some = urls.filter((u): u is string => u !== null);
  if (some.length === 0) return { prefix: "", suffix: "", names: urls.map(() => null) };
  const first = some[0]!;
  let pre = first.length;
  let suf = first.length;
  for (const u of some.slice(1)) {
    let a = 0;
    while (a < pre && a < u.length && first[a] === u[a]) a++;
    pre = a;
    let b = 0;
    while (b < suf && b < u.length && first[first.length - 1 - b] === u[u.length - 1 - b]) b++;
    suf = b;
  }
  const shortest = Math.min(...some.map((u) => u.length));
  pre = Math.min(pre, shortest);
  suf = Math.min(suf, shortest - pre);
  return { prefix: first.slice(0, pre), suffix: first.slice(first.length - suf), names: urls.map((u) => (u === null ? null : u.slice(pre, u.length - suf))) };
}

export interface MinecraftHost {
  now(): number;
  later(ms: number, fn: () => void): NodeJS.Timeout;
  markDirty(): number;
}

interface Borrow {
  session: string;
  viewer: string;
  since: number;
  until: number;
  timer: NodeJS.Timeout;
}

type Phase = "running" | "restoring" | "forking";

interface Bot {
  name: string;
  dye: number;
  x: number;
  z: number;
  yaw: number;
  jump: number;
  target: { x: number; z: number } | null;
  /** Blocks left to stack on the current tower, and the column it stands on. */
  tower: { left: number; i: number } | null;
  nextAt: number;
}

interface Drive {
  viewer: string;
  seq: number;
  keys: string[];
  yaw: number;
  until: number;
  actions: { seq: number; action: "place" | "dig" }[];
  /** The newest message not yet seen to move the bot. */
  waiting: number | null;
  shown: { seq: number; moved_at: number } | null;
}

interface Server {
  n: number;
  id: string;
  slot: string;
  host: string;
  rect: Rect;
  phase: Phase;
  generation: number;
  lastRelaunch: { clicked_at: number; by: string; first_frame_at: number | null; phases: Record<string, number> };
  forkedFrom: string | null;
  borrow: Borrow | null;
  timers: NodeJS.Timeout[];
  world: Uint8Array;
  /** Each column's highest block that is not leaves, a plant or water: what `map.ground` carries. It moves with the world. */
  ground: Uint8Array;
  /** When the current launch's world came up (the mock's clock): its day runs on from the snapshot's. */
  dayFrom: { at: number; tick: number };
  bots: Bot[];
  rng: () => number;
  /** Columns changed since the last delta. */
  changed: Set<number>;
  seq: number;
  drive: Drive | null;
  sockets: Set<Viewer>;
}

interface Viewer {
  ws: WebSocket;
  viewer: string;
  sent: number[];
}

export interface RecordedMinecraftIntent {
  at: number;
  viewer: string;
  name: string;
  args: Record<string, unknown>;
  outcome: "committed" | "refused";
}

/** A drive message as a tile received it, and its answer. */
export interface RecordedDrive {
  at: number;
  slot: string;
  viewer: string;
  msg: Record<string, unknown>;
  ok: boolean;
  reason: string | null;
}

/** The servers' image: one row of up to four, then rows of equal length, each tile square. */
export function minecraftGrid(n: number): { cols: number; rows: number; aspect: number } {
  const rows = Math.ceil(n / 4);
  const cols = Math.ceil(n / rows);
  return { cols, rows, aspect: cols / rows };
}

function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function hash2(x: number, z: number): number {
  let h = Math.imul(x, 374761393) + Math.imul(z, 668265263);
  h = Math.imul(h ^ (h >>> 13), 1274126177);
  return ((h ^ (h >>> 16)) >>> 0) / 4294967296;
}

/** The region the map covers: the spawn chunk's neighbourhood, as fleet/minecraft's `Region::around(0, 0, 128)`. */
export const MC_REGION = { x0: -56, z0: -56, w: MC_SIDE, h: MC_SIDE };

/** The lake: its middle and how far it reaches, in blocks. */
const LAKE = { x: -20, z: 18, r: 22 };
/** The day's tick in the gold snapshot; a restored server's clock runs on from it. */
const GOLD_DAY_TIME = 2900;

/**
 * The world every server is restored to, shaped as the real one is: hills, a lake, a beach, and a
 * forest whose canopies cover many columns each (a clearing round the spawn), with plants standing
 * on the ground. `cells` is what the view socket's `map` carries (each column's top block);
 * `ground` is the block under the leaves, plant or water, which a tile may send as `map.ground`.
 */
export function goldLayers(): { cells: Uint8Array; ground: Uint8Array } {
  const cells = new Uint8Array(MC_SIDE * MC_SIDE * 2);
  const ground = new Uint8Array(MC_SIDE * MC_SIDE * 2);
  for (let z = 0; z < MC_SIDE; z++) {
    for (let x = 0; x < MC_SIDE; x++) {
      const wx = x + MC_REGION.x0;
      const wz = z + MC_REGION.z0;
      const lake = Math.hypot(wx - LAKE.x, wz - LAKE.z) / LAKE.r;
      let y = 66 + 9 * Math.sin(wx / 17) * Math.cos(wz / 23) + 5 * Math.sin((wx + wz) / 11) + 3 * hash2(wx >> 2, wz >> 2);
      y += Math.max(0, (wx - 30) * 0.35) * Math.max(0, (wz + 10) / 60);
      if (lake < 1) y = Math.min(y, 54 + 8 * lake);
      const top = Math.round(y);
      let m = M.grass;
      let h = top;
      let gm = M.grass;
      if (top < 62) {
        m = M.water;
        h = 62;
        gm = M.sand;
      } else if (top <= 63) m = gm = M.sand;
      else if (top >= 86) m = gm = M.snow;
      else if (top >= 80) m = gm = M.stone;
      else {
        const r = hash2(wx, wz);
        const canopy = 0.5 + 0.5 * Math.sin(wx / 7.3 + 1.7) * Math.cos(wz / 5.9) + (hash2(wx >> 1, wz >> 1) - 0.5) * 0.6;
        const clearing = Math.hypot(wx - 5, wz - 3) < 12;
        if (canopy > 0.56 && !clearing) {
          m = M.leaves;
          h = top + 4 + Math.round(3 * hash2(wx >> 2, wz >> 2));
        } else if (r < 0.03) {
          m = M.flower;
          h = top + 1;
        } else if (r < 0.08) {
          m = M.plant;
          h = top + 1;
        } else if (r > 0.995) m = gm = M.gravel;
      }
      const i = z * MC_SIDE + x;
      cells[i * 2] = m;
      cells[i * 2 + 1] = Math.max(0, Math.min(255, h + 64));
      ground[i * 2] = gm;
      ground[i * 2 + 1] = Math.max(0, Math.min(255, top + 64));
    }
  }
  return { cells, ground };
}

/** The gold world's top blocks, as the view socket's `map` carries them. */
export function goldWorld(): Uint8Array {
  return goldLayers().cells;
}

const { cells: GOLD, ground: GOLD_GROUND } = goldLayers();
const INTENTS = new Set(["minecraft_reset", "minecraft_fork", "claim_minecraft", "release_minecraft"]);

export class MockMinecraft {
  opts: MockMinecraftOptions;
  private servers: Server[] = [];
  private host: MinecraftHost;
  private claims = 0;
  private lastEnd = new Map<string, number>();
  private lastReset = new Map<string, number>();
  private lastResetAll = new Map<string, number>();
  private lastForkBy = new Map<string, number>();
  private resetAll: { clicked_at: number; by: string; servers: number } | null = null;
  private lastFork: Record<string, unknown> | null = null;
  readonly intents: RecordedMinecraftIntent[] = [];
  readonly drives: RecordedDrive[] = [];
  aspect: number;
  private http: http.Server;
  private wss: WebSocketServer;
  private tickTimer: NodeJS.Timeout | null = null;
  private deltaTimer: NodeJS.Timeout | null = null;
  private lastTick = 0;

  constructor(opts: MockMinecraftOptions, host: MinecraftHost, epochNow: number) {
    this.opts = opts;
    this.host = host;
    this.aspect = 1;
    this.build(opts.count, epochNow);
    this.http = http.createServer((_req, res) => res.writeHead(404).end());
    this.wss = new WebSocketServer({ noServer: true, perMessageDeflate: false });
    this.http.on("upgrade", (req, socket, head) => {
      // Node hands an upgraded socket over with no error listener, and a page that goes away
      // resets its pending connects: unheard, that reset ends the whole mock.
      socket.on("error", () => {});
      const m = /^\/(mc-\d+)\/view$/.exec(new URL(req.url ?? "/", "http://mock").pathname);
      const s = m ? this.servers.find((x) => x.slot === m[1]) : undefined;
      if (!s) {
        socket.end("HTTP/1.1 404 Not Found\r\n\r\n");
        return;
      }
      // Tests: a handshake that is never answered (the connection stays open, silent).
      if (this.hang) {
        this.held.push(s.slot);
        this.hung.add(socket);
        socket.on("close", () => this.hung.delete(socket));
        return;
      }
      // Tests: no view socket can be reached at all.
      if (this.down) {
        socket.destroy();
        return;
      }
      // A viewer with no IPv6: its connects to the tile's own (IPv6) name fail.
      if (this.v6Down && (req.headers.host ?? "").replace(/:\d+$/, "") !== this.v4Host) {
        socket.destroy();
        return;
      }
      // a server between launches has no view to give, as the tile answers 503 before its scene
      if (s.phase !== "running") {
        socket.end("HTTP/1.1 503 Service Unavailable\r\n\r\n");
        return;
      }
      this.wss.handleUpgrade(req, socket, head, (ws) => this.view(s, ws));
    });
  }

  /** The servers, as a fresh start leaves them. */
  private build(count: number, epochNow: number): void {
    const g = minecraftGrid(count);
    this.aspect = g.aspect;
    const perHost = this.opts.perHost ?? 8;
    for (let i = 0; i < count; i++) {
      const n = i + 1;
      // each host's servers came up one after another
      const total = 1700 + ((i % perHost % 8) + 1) * 110;
      // a large fleet's servers are of every age up to the half hour after which each is reset
      const back = count > TILES_LISTED ? 3000 + ((n * 37) % 1800) * 1000 : 3000;
      const s: Server = {
        n,
        id: `mc:${n}`,
        slot: `mc-${n}`,
        host: `m5.metal · mock-host-${1 + Math.floor(i / perHost)}`,
        rect: { x: (i % g.cols) / g.cols, y: Math.floor(i / g.cols) / g.rows, w: 1 / g.cols, h: 1 / g.rows },
        phase: "running",
        generation: 1,
        lastRelaunch: { clicked_at: epochNow - total - back, by: "start", first_frame_at: epochNow - back, phases: phasesOf(total) },
        forkedFrom: null,
        borrow: null,
        timers: [],
        world: GOLD.slice(),
        ground: GOLD_GROUND.slice(),
        dayFrom: { at: this.host.now(), tick: GOLD_DAY_TIME },
        bots: [],
        rng: mulberry32(n),
        changed: new Set(),
        seq: 1,
        drive: null,
        sockets: new Set(),
      };
      s.bots = spawnBots(s.rng);
      this.servers.push(s);
    }
  }

  /** Tests: run `count` servers, `perHost` on each host, in place of the ones there. Their viewers' sockets close. */
  resize(count: number, perHost?: number): void {
    for (const s of this.servers) {
      for (const t of s.timers) clearTimeout(t);
      if (s.borrow) clearTimeout(s.borrow.timer);
      this.dropSockets(s);
    }
    this.servers = [];
    this.opts = { ...this.opts, count, ...(perHost === undefined ? {} : { perHost }) };
    this.resetAll = null;
    this.lastFork = null;
    this.build(count, this.host.now());
    this.host.markDirty();
  }

  listen(): Promise<void> {
    return new Promise((resolve) => {
      this.http.listen(this.opts.port, () => {
        this.lastTick = this.host.now();
        this.tickTimer = setInterval(() => this.tick(), 100);
        this.deltaTimer = setInterval(() => this.flushDeltas(), 200);
        resolve();
      });
    });
  }

  get port(): number {
    const a = this.http.address();
    return typeof a === "object" && a ? a.port : this.opts.port;
  }

  get count(): number {
    return this.servers.length;
  }

  frameUrl(slot: string): string {
    return `ws://${this.opts.publicHost}:${this.port}/${slot}/view`;
  }

  private get v4Host(): string {
    return this.opts.publicHostV4 ?? "127.0.0.1";
  }

  /** The same view socket over the server's IPv4 name (the host's address and its published port). */
  frameUrlV4(slot: string): string {
    return `ws://${this.v4Host}:${this.port}/${slot}/view`;
  }

  private hang = false;
  private hung = new Set<import("node:stream").Duplex>();
  /** Tests: the server of every handshake the hang fault has held, in the order they came. */
  readonly held: string[] = [];
  /**
   * Tests: leave every new view socket's handshake unanswered; only the page can end one. Turning
   * it off answers new ones again and resets the held ones. Every server here is at one address
   * and port, and Chromium sends one WebSocket handshake at a time to an address and port, so a
   * handshake left held would keep every other server's next one unsent until its page gave it
   * up. A fleet's servers each have an address or a port of their own.
   */
  setHang(hang: boolean): void {
    this.hang = hang;
    if (hang) return;
    for (const s of this.hung) s.destroy();
    this.hung.clear();
  }

  private down = false;
  /** Tests: refuse every view socket, by either name, and drop the open ones. */
  setDown(down: boolean): void {
    this.down = down;
    if (down) for (const c of this.wss.clients) c.terminate();
  }

  private v6Down = false;
  /** Tests: refuse every view socket opened by the IPv6 name, as for a viewer with no IPv6. */
  setV6Down(down: boolean): void {
    this.v6Down = down;
  }

  /** Tests: the hosts stop sending their status rows. */
  statusOff = false;

  private hosts(): Map<string, Server[]> {
    const by = new Map<string, Server[]>();
    for (const s of this.servers) by.set(s.host, [...(by.get(s.host) ?? []), s]);
    return by;
  }

  /**
   * Each host's status rows, as its agent sends them about once a second (PROTOCOL.md, "Many
   * servers"): 8 bytes a server. The readings are a host of 192 CPUs, each server taking a fifth
   * of one and 400 MiB.
   */
  statusMessages(): Record<string, unknown>[] {
    if (this.statusOff) return [];
    const now = this.host.now();
    return [...this.hosts()].map(([host, servers]) => {
      const rows = Buffer.alloc(servers.length * 8);
      servers.forEach((s, i) => {
        const running = s.phase === "running";
        const up = s.lastRelaunch.first_frame_at;
        const launch = s.lastRelaunch.phases["total_ms"];
        rows[i * 8] = running ? 3 : s.phase === "forking" ? 2 : 1;
        rows[i * 8 + 1] = running ? Math.round((19.95 + ((s.n * 7 + Math.floor(now / 1000)) % 6) / 100) * 10) : 255;
        rows[i * 8 + 2] = running ? s.bots.length : 255;
        rows.writeUInt16LE(running && up !== null ? Math.min(65534, Math.max(0, Math.floor((now - up) / 1000))) : 65535, i * 8 + 4);
        rows.writeUInt16LE(running && launch !== undefined ? Math.min(65534, launch) : 65535, i * 8 + 6);
      });
      const running = servers.filter((s) => s.phase === "running").length;
      return { type: "minecraft_status", host: hostKey(host), received_at: now, t: now, rows: rows.toString("base64"), cpus: 192, busy: Math.round(running * 1.9) / 10, mem_total_mib: 385_024, mem_used_mib: 20_480 + servers.length * 400 };
    });
  }

  state() {
    const lent = this.servers.filter((s) => s.borrow !== null);
    // A large fleet lists its first servers and every lent one as tiles, in an image of their
    // own, and names every server in `field`.
    const many = this.servers.length > TILES_LISTED;
    const listed = many ? this.servers.filter((s, i) => i < TILES_LISTED || s.borrow !== null) : this.servers;
    const g = minecraftGrid(listed.length);
    return {
      server: MINECRAFT_SERVER,
      image_aspect: many ? g.aspect : this.aspect,
      ...(many
        ? {
            field: {
              servers: this.servers.length,
              hosts: [...this.hosts()].map(([host, servers]) => ({
                key: hostKey(host),
                servers: servers.map((s) => s.n),
                frames: squeeze(servers.map((s) => (s.phase === "running" ? this.frameUrl(s.slot) : null))),
                frames_v4: squeeze(servers.map((s) => (s.phase === "running" ? this.frameUrlV4(s.slot) : null))),
              })),
            },
          }
        : {}),
      claim_secs: this.opts.claimSecs,
      borrow: { cap: this.opts.cap, lent: lent.length, next_return_at: lent.length === 0 ? null : Math.min(...lent.map((s) => s.borrow!.until)) },
      last_reset_all: this.resetAll,
      last_fork: this.lastFork,
      tiles: listed.map((s, k) => ({
        id: s.id,
        half: "fcvm",
        rect: many ? { x: (k % g.cols) / g.cols, y: Math.floor(k / g.cols) / g.rows, w: 1 / g.cols, h: 1 / g.rows } : s.rect,
        phase: s.phase,
        slot: s.slot,
        vm: `fleet-${s.slot}`,
        generation: s.generation,
        fps: null,
        error: null,
        failure: null,
        frame_url: this.frameUrl(s.slot),
        frame_url_v4: this.frameUrlV4(s.slot),
        gpu: null,
        host: s.host,
        place: null,
        last_relaunch: s.lastRelaunch,
        borrowed: s.borrow && { session: s.borrow.session, viewer: s.borrow.viewer, since: s.borrow.since, until: s.borrow.until },
        join: this.opts.joinZone ? `${s.slot}.${this.opts.joinZone}` : null,
        forked_from: s.forkedFrom,
      })),
    };
  }

  // ---- the coordinator's intents ----------------------------------------------------------

  private own(viewer: string): Server | null {
    return this.servers.find((s) => s.borrow?.viewer === viewer) ?? null;
  }

  private free(s: Server): string | null {
    if (s.borrow) return `${s.slot} is lent to another viewer`;
    if (s.phase !== "running") return `${s.slot} is not running`;
    return null;
  }

  /** The server a fork of `src` copies into: the next on its host, in index order and wrapping, running, not lent. */
  forkTarget(src: Server): Server | null {
    const peers = this.servers.filter((s) => s.host === src.host);
    const at = peers.indexOf(src);
    for (let k = 1; k < peers.length; k++) {
      const t = peers[(at + k) % peers.length]!;
      if (t !== src && t.phase === "running" && t.borrow === null) return t;
    }
    return null;
  }

  /** Run one Minecraft intent. Returns undefined for a name that is not one. */
  apply<R>(viewer: string, name: string, args: Record<string, unknown>, h0: Reply<R>): R | undefined {
    if (!INTENTS.has(name)) return undefined;
    let outcome: "committed" | "refused" = "refused";
    const h: Reply<R> = {
      refuse: (reason) => ((outcome = "refused"), h0.refuse(reason)),
      committed: (summary) => ((outcome = "committed"), h0.committed(summary)),
    };
    const r = this.run(viewer, name, args, h);
    this.intents.push({ at: this.host.now(), viewer, name, args, outcome });
    return r;
  }

  private run<R>(viewer: string, name: string, args: Record<string, unknown>, h: Reply<R>): R {
    const now = this.host.now();
    switch (name) {
      case "minecraft_reset": {
        const slot = args["slot"];
        const all = args["all"];
        if (slot !== undefined && slot !== null && typeof slot !== "string") return h.refuse("args.slot must be a server slot");
        if (all !== undefined && all !== null && typeof all !== "boolean") return h.refuse("args.all must be true");
        const hasSlot = typeof slot === "string";
        const wantsAll = all === true;
        if (hasSlot === wantsAll) return h.refuse("args must name a slot or say all");
        if (wantsAll) {
          const last = this.lastResetAll.get(viewer);
          if (last !== undefined && now < last + RESET_ALL_INTERVAL_MS) return h.refuse("one reset all per 10 s per client");
          const targets = this.servers.filter((s) => s.borrow === null);
          if (targets.length === 0) return h.refuse("no server can be reset now");
          this.lastResetAll.set(viewer, now);
          this.resetAll = { clicked_at: now, by: `viewer:${viewer}`, servers: targets.length };
          // a host restores so many a second: a reset of hundreds comes back as a wave
          targets.forEach((s, k) => this.relaunch(s, "reset_all", targets.length > TILES_LISTED ? Math.floor((k * 1000) / RESTORES_PER_SEC) : 0));
          return h.committed(`reset all (${targets.length} servers)`);
        }
        const last = this.lastReset.get(viewer);
        if (last !== undefined && now < last + RESET_INTERVAL_MS) return h.refuse("one reset per 2 s per client");
        const s = this.servers.find((x) => x.slot === slot);
        if (!s) return h.refuse(`no server "${String(slot)}"`);
        if (s.borrow) return h.refuse(`${s.slot} is lent to a viewer`);
        this.lastReset.set(viewer, now);
        this.relaunch(s, `viewer:${viewer}`);
        return h.committed(`reset ${s.slot}`);
      }
      case "minecraft_fork": {
        const slot = args["slot"];
        if (typeof slot !== "string") return h.refuse("args.slot must be a server slot");
        const last = this.lastForkBy.get(viewer);
        if (last !== undefined && now < last + FORK_INTERVAL_MS) return h.refuse("one fork per 10 s per client");
        const src = this.servers.find((x) => x.slot === slot);
        if (!src) return h.refuse(`no server "${slot}"`);
        if (src.phase !== "running") return h.refuse(`${src.slot} is not running`);
        const target = this.forkTarget(src);
        if (!target) return h.refuse(`no server on ${src.slot}'s host can take a copy`);
        this.lastForkBy.set(viewer, now);
        this.fork(src, target, `viewer:${viewer}`);
        return h.committed(`fork ${src.slot} into ${target.slot}`);
      }
      case "claim_minecraft": {
        const want = args["slot"];
        if (want !== undefined && want !== null && typeof want !== "string") return h.refuse("args.slot must be a server slot");
        if (this.own(viewer)) return h.refuse("one server per client at a time");
        const end = this.lastEnd.get(viewer);
        if (end !== undefined && now < end + this.opts.cooldownSecs * 1000) {
          const wait = Math.ceil((end + this.opts.cooldownSecs * 1000 - now) / 1000);
          return h.refuse(`your last server was returned ${Math.floor((now - end) / 1000)} s ago; borrow again in ${wait} s`);
        }
        const lent = this.servers.filter((s) => s.borrow);
        if (lent.length >= this.opts.cap) {
          const back = Math.ceil(Math.max(0, Math.min(...lent.map((s) => s.borrow!.until)) - now) / 1000);
          return h.refuse(`${lent.length} of ${this.opts.cap} servers are already lent; the next comes back in ${back} s`);
        }
        let s: Server | undefined;
        if (typeof want === "string") {
          s = this.servers.find((x) => x.slot === want);
          if (!s) return h.refuse(`no server "${want}"`);
          const why = this.free(s);
          if (why) return h.refuse(why);
        } else {
          s = this.servers.find((x) => this.free(x) === null);
          if (!s) return h.refuse("no server can be lent now: none is running and free");
        }
        const session = `minecraft-claim-${++this.claims}`;
        const target = s;
        const timer = this.host.later(this.opts.claimSecs * 1000, () => this.release(target));
        s.borrow = { session, viewer, since: now, until: now + this.opts.claimSecs * 1000, timer };
        // the agent posts the scene with its driver; the tile then takes that viewer's keys
        s.drive = { viewer, seq: 0, keys: [], yaw: s.bots[0]?.yaw ?? 0, until: 0, actions: [], waiting: null, shown: null };
        return h.committed(`${session} on ${s.slot}`);
      }
      case "release_minecraft": {
        const claim = args["claim"];
        if (claim !== undefined && claim !== null && typeof claim !== "string") return h.refuse("args.claim must be a claim id");
        const s = this.own(viewer);
        if (!s) return h.refuse("this viewer holds no server");
        if (typeof claim === "string" && claim !== s.borrow!.session) return h.refuse(`"${claim}" is not this viewer's server`);
        this.release(s);
        return h.committed("release server");
      }
      default:
        throw new Error(`not a Minecraft intent: ${name}`);
    }
  }

  /** A viewer left: its server goes back. */
  left(viewer: string): void {
    const s = this.own(viewer);
    if (s) this.release(s);
  }

  private release(s: Server): void {
    const b = s.borrow;
    if (!b) return;
    clearTimeout(b.timer);
    s.borrow = null;
    this.lastEnd.set(b.viewer, this.host.now());
    this.relaunch(s, `return:${b.session}`);
  }

  /** Every socket to a server closes when the server is replaced. */
  private dropSockets(s: Server): void {
    for (const v of s.sockets) v.ws.close(1001, "the server was replaced");
    s.sockets.clear();
  }

  /** Restored from the gold snapshot: the gold world, the bots at the spawn, timed from the click to the first chunk. */
  private relaunch(s: Server, by: string, waitMs = 0): void {
    const h = this.host;
    for (const t of s.timers) clearTimeout(t);
    s.timers = [];
    s.generation += 1;
    const gen = s.generation;
    const clicked = h.now();
    const total = this.opts.restoreMs + waitMs;
    s.phase = "restoring";
    s.forkedFrom = null;
    s.drive = null;
    s.lastRelaunch = { clicked_at: clicked, by, first_frame_at: null, phases: {} };
    this.dropSockets(s);
    h.markDirty();
    s.timers.push(
      h.later(total, () => {
        if (s.generation !== gen) return;
        const done = h.now();
        s.world = GOLD.slice();
        s.ground = GOLD_GROUND.slice();
        s.dayFrom = { at: this.host.now(), tick: GOLD_DAY_TIME };
        s.changed.clear();
        s.seq += 1;
        s.rng = mulberry32(s.n * 1000 + gen);
        s.bots = spawnBots(s.rng);
        s.phase = "running";
        s.timers = [];
        if (s.borrow) s.drive = { viewer: s.borrow.viewer, seq: 0, keys: [], yaw: s.bots[0]!.yaw, until: 0, actions: [], waiting: null, shown: null };
        s.lastRelaunch = { clicked_at: clicked, by, first_frame_at: done, phases: phasesOf(done - clicked) };
        h.markDirty();
      }),
    );
  }

  /**
   * `src`'s running world copied into `dst`: the source keeps running and its world at the
   * snapshot's end is what the copy holds; the copy's bots then choose their own paths.
   */
  private fork(src: Server, dst: Server, by: string): void {
    const h = this.host;
    for (const t of dst.timers) clearTimeout(t);
    dst.timers = [];
    dst.generation += 1;
    const gen = dst.generation;
    const clicked = h.now();
    const snapMs = this.opts.forkSnapshotMs;
    const total = snapMs + this.opts.restoreMs;
    dst.phase = "forking";
    dst.forkedFrom = src.slot;
    dst.drive = null;
    dst.lastRelaunch = { clicked_at: clicked, by: `fork:${src.slot}`, first_frame_at: null, phases: {} };
    this.dropSockets(dst);
    h.markDirty();
    let copy: { world: Uint8Array; ground: Uint8Array; bots: Bot[]; tick: number } | null = null;
    dst.timers.push(
      h.later(snapMs, () => {
        if (dst.generation !== gen) return;
        copy = { world: src.world.slice(), ground: src.ground.slice(), bots: src.bots.map((b) => ({ ...b, target: null, tower: null })), tick: this.dayTime(src) };
      }),
    );
    dst.timers.push(
      h.later(total, () => {
        if (dst.generation !== gen || copy === null) return;
        const done = h.now();
        dst.world = copy.world;
        dst.ground = copy.ground;
        dst.dayFrom = { at: this.host.now(), tick: copy.tick };
        dst.changed.clear();
        dst.seq += 1;
        dst.rng = mulberry32(dst.n * 7919 + gen);
        dst.bots = copy.bots.map((b) => ({ ...b, nextAt: done }));
        dst.phase = "running";
        dst.timers = [];
        const restoreMs = done - clicked - snapMs;
        dst.lastRelaunch = { clicked_at: clicked, by: `fork:${src.slot}`, first_frame_at: done, phases: { snapshot_ms: snapMs, restore_ms: restoreMs, first_chunk_ms: done - clicked, total_ms: done - clicked } };
        this.lastFork = { from: src.slot, to: dst.slot, clicked_at: clicked, by, first_chunk_at: done, snapshot_ms: snapMs, restore_ms: restoreMs, total_ms: done - clicked };
        h.markDirty();
      }),
    );
  }

  // ---- the tiles: the world, the bots, the view sockets -------------------------------------

  /** A column's top block, from the world's two bytes. */
  private column(s: Server, x: number, z: number): { i: number; m: number; y: number } | null {
    const cx = Math.floor(x) - MC_REGION.x0;
    const cz = Math.floor(z) - MC_REGION.z0;
    if (cx < 0 || cz < 0 || cx >= MC_SIDE || cz >= MC_SIDE) return null;
    const i = cz * MC_SIDE + cx;
    return { i, m: s.world[i * 2]!, y: s.world[i * 2 + 1]! - 64 };
  }

  private setColumn(s: Server, i: number, m: number, y: number): void {
    s.world[i * 2] = m;
    s.world[i * 2 + 1] = Math.max(0, Math.min(255, y + 64));
    // A top block that is not leaves, a plant or water is the column's ground too. (The mock's
    // bots build on and dig what they placed; under a canopy or a lake the ground stays.)
    if (!COVER.has(m)) {
      s.ground[i * 2] = s.world[i * 2]!;
      s.ground[i * 2 + 1] = s.world[i * 2 + 1]!;
    }
    s.changed.add(i);
  }

  private tick(): void {
    const now = this.host.now();
    const dt = Math.min(0.5, Math.max(0, (now - this.lastTick) / 1000));
    this.lastTick = now;
    for (const s of this.servers) {
      if (s.phase !== "running") continue;
      s.bots.forEach((b, k) => {
        if (k === 0 && s.drive) this.driveBot(s, b, s.drive, now, dt);
        else this.wander(s, b, now, dt);
      });
      if (s.sockets.size > 0) this.broadcast(s, this.status(s, now));
    }
  }

  private wander(s: Server, b: Bot, now: number, dt: number): void {
    if (now < b.nextAt) return;
    if (b.tower) {
      const col = s.world[b.tower.i * 2 + 1]! - 64;
      this.setColumn(s, b.tower.i, b.dye, col + 1);
      b.tower.left -= 1;
      b.nextAt = now + 300;
      if (b.tower.left <= 0) b.tower = null;
      return;
    }
    if (!b.target) {
      // The third bot walks into the lake and stays under it, as the real bots often do; the
      // others pick a place on dry land a short walk away, so each is building again within seconds.
      const wet = b.name === BOT_NAMES[2];
      const inner = (v: number, lo: number) => Math.max(lo + 24, Math.min(lo + MC_SIDE - 24, v));
      let to = { x: 0, z: 0 };
      for (let k = 0; k < 20; k++) {
        to = wet
          ? { x: LAKE.x + (s.rng() - 0.5) * LAKE.r, z: LAKE.z + (s.rng() - 0.5) * LAKE.r }
          : { x: inner(b.x + (s.rng() - 0.5) * 24, MC_REGION.x0), z: inner(b.z + (s.rng() - 0.5) * 24, MC_REGION.z0) };
        if (wet || this.column(s, to.x, to.z)?.m !== M.water) break;
      }
      b.target = to;
    }
    const dx = b.target.x - b.x;
    const dz = b.target.z - b.z;
    const d = Math.hypot(dx, dz);
    if (d < 0.6) {
      b.target = null;
      // It builds on the nearest column beside it that is neither water nor under a canopy.
      let site: { i: number } | null = null;
      for (let r = 1; r <= 3 && site === null; r++) {
        for (const [dx, dz] of [[r, 0], [0, r], [-r, 0], [0, -r], [r, r], [-r, -r]] as const) {
          const c = this.column(s, b.x + dx, b.z + dz);
          if (c && c.m !== M.water && c.m !== M.leaves) {
            site = c;
            break;
          }
        }
      }
      if (site) b.tower = { left: 3 + Math.floor(s.rng() * 3), i: site.i };
      b.nextAt = now + 200;
      return;
    }
    const step = Math.min(d, WALK * dt);
    b.x += (dx / d) * step;
    b.z += (dz / d) * step;
    b.yaw = (Math.atan2(-dx, dz) * 180) / Math.PI;
  }

  /** The driven bot: the held keys until the lease runs out, the actions in order. */
  private driveBot(s: Server, b: Bot, d: Drive, now: number, dt: number): void {
    const before = { x: b.x, z: b.z, yaw: b.yaw, jump: b.jump };
    b.yaw = d.yaw;
    const keys = now <= d.until ? d.keys : [];
    const f = (keys.includes("forward") ? 1 : 0) - (keys.includes("back") ? 1 : 0);
    const r = (keys.includes("right") ? 1 : 0) - (keys.includes("left") ? 1 : 0);
    const rad = (b.yaw * Math.PI) / 180;
    const fx = -Math.sin(rad);
    const fz = Math.cos(rad);
    const speed = (keys.includes("sprint") ? SPRINT : WALK) * dt;
    if (f !== 0 || r !== 0) {
      const mx = f * fx - r * Math.cos(rad);
      const mz = f * fz - r * Math.sin(rad);
      const len = Math.hypot(mx, mz);
      b.x = Math.max(MC_REGION.x0 + 1, Math.min(MC_REGION.x0 + MC_SIDE - 1, b.x + (mx / len) * speed));
      b.z = Math.max(MC_REGION.z0 + 1, Math.min(MC_REGION.z0 + MC_SIDE - 1, b.z + (mz / len) * speed));
    }
    b.jump = keys.includes("jump") ? 1.2 : 0;
    let acted = false;
    for (const a of d.actions.splice(0)) {
      const c = this.column(s, b.x + fx * 1.5, b.z + fz * 1.5);
      if (!c) continue;
      if (a.action === "place") this.setColumn(s, c.i, b.dye, c.y + 1);
      else this.setColumn(s, c.i, c.m === M.water ? M.water : c.y > 60 ? M.dirt : M.stone, c.y - 1);
      acted = true;
    }
    const moved = b.x !== before.x || b.z !== before.z || b.yaw !== before.yaw || b.jump !== before.jump || acted;
    if (moved && d.waiting !== null) {
      d.shown = { seq: d.waiting, moved_at: now };
      d.waiting = null;
    }
  }

  private status(s: Server, now: number): string {
    return JSON.stringify({
      type: "status",
      t: now,
      tps: 19.95 + ((s.n * 7 + Math.floor(now / 1000)) % 6) / 100,
      players: s.bots.map((b, k) => ({ name: b.name, x: round(b.x), y: round(this.standY(s, b.x, b.z) + b.jump), z: round(b.z), yaw: round(b.yaw), bot: true, driven: k === 0 && s.drive !== null })),
      drive: s.drive?.shown ?? null,
      // Every other server sends its clock and its ground, as a tile that has them does; the rest
      // send what the first tiles sent, and the page draws both.
      ...(this.sendsExtras(s) ? { day_time: this.dayTime(s) } : {}),
    });
  }

  /** Whether this server's tile sends the optional fields (`map.ground`, `status.day_time`). */
  private sendsExtras(s: Server): boolean {
    return s.n % 2 === 0;
  }

  /** The game's tick of the day: the snapshot's, plus 20 a second since this launch's world came up. */
  private dayTime(s: Server): number {
    return Math.floor(s.dayFrom.tick + ((this.host.now() - s.dayFrom.at) / 1000) * 20) % 24000;
  }

  /**
   * Where a bot's feet are at (x, z): on the ground under a canopy or a plant, on a lake's bed (a
   * bot that walks into water sinks, as the real ones do), else on the column's top block.
   */
  private standY(s: Server, x: number, z: number): number {
    const c = this.column(s, x, z);
    if (c === null) return 65;
    if (COVER.has(c.m)) return s.ground[c.i * 2 + 1]! - 64 + 1;
    return c.y + 1;
  }

  private flushDeltas(): void {
    for (const s of this.servers) {
      if (s.changed.size === 0) continue;
      s.seq += 1;
      const cells = [...s.changed].map((i) => [i, s.world[i * 2]!, s.world[i * 2 + 1]!]);
      // a tile that sends its ground sends the ground of the columns that changed
      const ground = this.sendsExtras(s) ? { ground: [...s.changed].map((i) => [i, s.ground[i * 2]!, s.ground[i * 2 + 1]!]) } : {};
      s.changed.clear();
      if (s.phase === "running") this.broadcast(s, JSON.stringify({ type: "delta", seq: s.seq, cells, ...ground }));
    }
  }

  private broadcast(s: Server, text: string): void {
    for (const v of s.sockets) if (v.ws.readyState === v.ws.OPEN) v.ws.send(text);
  }

  private mapOf(s: Server): string {
    return JSON.stringify({ type: "map", seq: s.seq, region: MC_REGION, cells: Buffer.from(s.world).toString("base64"), ...(this.sendsExtras(s) ? { ground: Buffer.from(s.ground).toString("base64") } : {}) });
  }

  /** The direct token the mock coordinator makes (`v1.<expiry s>.<viewer>.mock`): its viewer, or null when it is not one or has expired. */
  private admitted(token: unknown): string | null {
    if (typeof token !== "string") return null;
    const m = /^v1\.(\d+)\.([A-Za-z0-9:_-]+)\.mock$/.exec(token);
    if (!m || Number(m[1]) * 1000 < this.host.now()) return null;
    return m[2]!;
  }

  private view(s: Server, ws: WebSocket): void {
    let v: Viewer | null = null;
    const deadline = setTimeout(() => ws.close(1008, "admission refused"), 5000);
    ws.on("message", (data, isBinary) => {
      if (isBinary) {
        ws.close(1008, "text messages only");
        return;
      }
      let msg: Record<string, unknown>;
      try {
        const parsed = JSON.parse(String(data)) as unknown;
        if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) throw new Error("not an object");
        msg = parsed as Record<string, unknown>;
      } catch {
        if (v === null) ws.close(1008, "admission refused");
        return;
      }
      if (v === null) {
        clearTimeout(deadline);
        const viewer = this.admitted(msg["admit"]);
        if (viewer === null || s.phase !== "running") {
          ws.close(1008, "admission refused");
          return;
        }
        v = { ws, viewer, sent: [] };
        s.sockets.add(v);
        ws.send(JSON.stringify({ type: "hello", tile: s.slot, generation: s.generation, viewer, materials: [...MATERIALS], bots: s.bots.length, bot_names: s.bots.map((b) => b.name), driver: s.drive?.viewer ?? null }));
        ws.send(this.mapOf(s));
        ws.send(this.status(s, this.host.now()));
        return;
      }
      this.drive(s, v, msg);
    });
    ws.on("close", () => {
      clearTimeout(deadline);
      if (v) s.sockets.delete(v);
    });
    ws.on("error", () => {});
  }

  /** A drive message: checked as fleet/minecraft/src/drive.rs checks one, then taken from the driver alone. */
  private drive(s: Server, v: Viewer, msg: Record<string, unknown>): void {
    const now = this.host.now();
    const reply = (ok: boolean, seq: number, reason: string | null) => {
      this.drives.push({ at: now, slot: s.slot, viewer: v.viewer, msg, ok, reason });
      v.ws.send(JSON.stringify(ok ? { type: "drive_ack", seq, ok: true, at: now } : { type: "drive_ack", seq, ok: false, reason }));
    };
    const seq = typeof msg["seq"] === "number" ? msg["seq"] : 0;
    v.sent = v.sent.filter((t) => t > now - 1000);
    if (v.sent.length >= MAX_DRIVES_PER_SEC) return reply(false, seq, `at most ${MAX_DRIVES_PER_SEC} drive messages a second`);
    v.sent.push(now);
    for (const k of Object.keys(msg)) if (!["type", "seq", "keys", "yaw", "pitch", "action"].includes(k)) return reply(false, seq, `unknown field \`${k}\``);
    if (msg["type"] !== "drive") return reply(false, seq, "type must be drive");
    if (!Number.isInteger(seq) || seq < 0) return reply(false, seq, "seq must be a whole number");
    const keys = msg["keys"] ?? [];
    if (!Array.isArray(keys) || keys.some((k) => typeof k !== "string" || !DRIVE_KEYS.includes(k))) return reply(false, seq, "keys must be from forward, back, left, right, jump, sprint");
    if (keys.length > 6) return reply(false, seq, "at most 6 keys");
    const yaw = msg["yaw"];
    if (typeof yaw !== "number" || !Number.isFinite(yaw) || Math.abs(yaw) > 3600) return reply(false, seq, "yaw must be a finite angle within 3600 degrees");
    const pitch = msg["pitch"] ?? 45;
    if (typeof pitch !== "number" || !Number.isFinite(pitch) || Math.abs(pitch) > 90) return reply(false, seq, "pitch must be within 90 degrees");
    const action = msg["action"] ?? null;
    if (action !== null && action !== "place" && action !== "dig") return reply(false, seq, "action must be place, dig or null");
    const d = s.drive;
    if (d === null) return reply(false, seq, "nobody holds this tile; take it over first");
    if (d.viewer !== v.viewer) return reply(false, seq, "another viewer holds this tile");
    if (seq <= d.seq) return reply(false, seq, `seq ${seq} is not after ${d.seq}`);
    d.seq = seq;
    d.keys = keys as string[];
    d.yaw = yaw;
    d.until = now + LEASE_MS;
    if (action !== null && d.actions.length < 8) d.actions.push({ seq, action });
    d.waiting = seq;
    reply(true, seq, null);
  }

  /** Tests: put the servers back as a fresh viewer finds them. */
  reset(over: { release_all?: boolean }): void {
    if (over.release_all) {
      for (const s of this.servers) {
        if (s.borrow) clearTimeout(s.borrow.timer);
        s.borrow = null;
        s.drive = null;
      }
    }
    this.lastEnd.clear();
    this.lastReset.clear();
    this.lastResetAll.clear();
    this.lastForkBy.clear();
    this.resetAll = null;
    this.lastFork = null;
    this.intents.length = 0;
    this.drives.length = 0;
    this.host.markDirty();
  }

  /** Tests: the world a server's view socket would send now, and its bots. */
  peek(slot: string): { world: Uint8Array; ground: Uint8Array; bots: { name: string; x: number; z: number; yaw: number }[] } | null {
    const s = this.servers.find((x) => x.slot === slot);
    return s ? { world: s.world.slice(), ground: s.ground.slice(), bots: s.bots.map((b) => ({ name: b.name, x: b.x, z: b.z, yaw: b.yaw })) } : null;
  }

  async close(): Promise<void> {
    if (this.tickTimer) clearInterval(this.tickTimer);
    if (this.deltaTimer) clearInterval(this.deltaTimer);
    for (const s of this.servers) {
      for (const t of s.timers) clearTimeout(t);
      if (s.borrow) clearTimeout(s.borrow.timer);
      for (const v of s.sockets) v.ws.terminate();
    }
    for (const s of this.hung) s.destroy();
    this.hung.clear();
    this.wss.close();
    await new Promise<void>((r) => this.http.close(() => r()));
  }
}

function spawnBots(rng: () => number): Bot[] {
  return BOT_NAMES.map((name, k) => ({
    name,
    dye: MATERIALS.indexOf(BOT_DYES[k] as (typeof MATERIALS)[number]),
    x: 2 + k * 3 + rng(),
    z: 2 + rng() * 2,
    yaw: Math.round(rng() * 360) - 180,
    jump: 0,
    target: null,
    tower: null,
    nextAt: 0,
  }));
}

function round(v: number): number {
  return Math.round(v * 100) / 100;
}

function phasesOf(total: number): Record<string, number> {
  return { restore_ms: Math.round(total * 0.25), first_chunk_ms: total, total_ms: total };
}
