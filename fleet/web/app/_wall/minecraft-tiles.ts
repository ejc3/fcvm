// Platform glue for Minecraft mode: one WebSocket per tile to its view socket (PROTOCOL.md,
// "Minecraft tile"), the world each one describes, and the flat map of it on the tile's canvas
// (the 3D view is minecraft-stage.ts). Every rule lives in core; this file only moves messages in
// and pixels out.

import {
  applyDelta,
  botCount,
  sameColumns,
  countDiff,
  RECENT_GLOW_MS,
  sameGrid,
  markersOf,
  MC_BACKGROUND,
  parseViewMsg,
  MC_CONNECT_DEADLINE_MS,
  reconnectDelayMs,
  remapOf,
  shadeWorld,
  worldFromMap,
  type Box,
  type DriveMsg,
  type McPlayer,
  type McTileLive,
  type McWorld,
  type ViewMsg,
} from "../../core/index.ts";

const now = () => performance.timeOrigin + performance.now();

export type AckMsg = Extract<ViewMsg, { type: "drive_ack" }>;

export interface TileEvents {
  /** A status message, at its arrival (local ms). */
  status(id: string, players: McPlayer[], at: number): void;
  /** A delta that changed at least one column: the changed columns, in block coordinates. */
  delta(id: string, at: number, columns: readonly { x: number; z: number }[]): void;
  ack(id: string, ack: AckMsg): void;
}

/** What the 3D view reads of a tile. */
export interface TileState {
  readonly key: string;
  readonly world: McWorld | null;
  readonly remap: Uint8Array | null;
  readonly players: readonly McPlayer[];
  readonly botNames: readonly string[];
  readonly dayTime: number | null;
  readonly lastChange: { i: number; at: number } | null;
  readonly recent: Map<number, number>;
  readonly rev: number;
}

/** One tile's socket, its world, and what the page last drew of it. */
class TileSocket {
  ws: WebSocket | null = null;
  open = false;
  attempt = 0;
  timer: ReturnType<typeof setTimeout> | null = null;
  /** Ends a connect still pending after MC_CONNECT_DEADLINE_MS: a handshake that never answers is a failure too. */
  connectTimer: ReturnType<typeof setTimeout> | null = null;
  viewer: string | null = null;
  botNames: string[] = [];
  remap: Uint8Array | null = null;
  world: McWorld | null = null;
  players: McPlayer[] = [];
  tps: number | null = null;
  bots = 0;
  /** Messages the page could not read, for the test hook. */
  errors = 0;
  /** Connection attempts that ended without opening since the socket last opened. */
  failures = 0;
  /** When the socket began trying to connect without being open (local ms); null while open. */
  connectingSince: number | null = now();
  /** Something changed since the last draw. */
  dirty = true;
  /** Counts every change to the world or the players: the 3D view redraws a still tile when it moves. */
  rev = 0;
  /** The game's ticks of the day, when the tile sends them. */
  dayTime: number | null = null;
  /** The column that last changed, and when (local ms). */
  lastChange: { i: number; at: number } | null = null;
  /** Columns that changed in the last moments, by index, with when. */
  recent = new Map<number, number>();
  /** The world shaded into a canvas of one pixel per column, and the world seq it shows. */
  image: HTMLCanvasElement | null = null;
  imageSeq = -1;
  pixels: Uint8ClampedArray | undefined;
  closed = false;

  readonly id: string;
  readonly key: string;
  readonly url: string;
  readonly first: string;
  private readonly owner: McTiles;

  constructor(id: string, key: string, url: string, first: string, owner: McTiles) {
    this.id = id;
    this.key = key;
    this.url = url;
    this.first = first;
    this.owner = owner;
    this.connect();
  }

  private connect(): void {
    if (this.closed) return;
    let ws: WebSocket;
    try {
      ws = new WebSocket(this.url);
    } catch {
      this.failures++;
      this.owner.changed();
      this.retry();
      return;
    }
    this.ws = ws;
    this.connectTimer = setTimeout(() => {
      this.connectTimer = null;
      if (this.ws !== ws || this.open) return;
      this.abandon();
      this.failures++;
      this.owner.changed();
      this.retry();
    }, MC_CONNECT_DEADLINE_MS);
    ws.onopen = () => {
      this.clearConnectTimer();
      this.open = true;
      this.attempt = 0;
      this.failures = 0;
      this.connectingSince = null;
      ws.send(this.first);
      this.owner.changed();
    };
    ws.onmessage = (e) => {
      if (typeof e.data !== "string") return;
      let m: ViewMsg | null;
      try {
        m = parseViewMsg(e.data);
      } catch {
        this.errors++;
        return;
      }
      if (m !== null) this.take(m);
    };
    ws.onclose = () => {
      this.clearConnectTimer();
      const was = this.open;
      this.open = false;
      this.ws = null;
      if (was) this.connectingSince = now();
      else this.failures++;
      // a restore of the server closes every socket to it: the next launch is a new world
      this.world = null;
      this.players = [];
      this.tps = null;
      this.dayTime = null;
      this.lastChange = null;
      this.recent.clear();
      this.dirty = true;
      this.rev++;
      this.owner.changed();
      this.retry();
    };
  }

  private clearConnectTimer(): void {
    if (this.connectTimer) clearTimeout(this.connectTimer);
    this.connectTimer = null;
  }

  /** Drop a connect that has not opened, without its close counting or retrying. */
  private abandon(): void {
    this.clearConnectTimer();
    const ws = this.ws;
    this.ws = null;
    if (ws === null) return;
    ws.onopen = ws.onclose = ws.onmessage = null;
    try {
      ws.close();
    } catch {
      // A socket that is not connected yet may refuse a close; it is dropped either way.
    }
  }

  /** Try again now instead of after the backoff (the page's Retry): a pending connect is dropped first. */
  retryNow(): void {
    if (this.closed || this.open) return;
    this.abandon();
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    this.attempt = 0;
    // a fresh attempt: the tile says it is connecting again until this one fails or runs out
    this.failures = 0;
    this.connectingSince = now();
    this.owner.changed();
    this.connect();
  }

  private retry(): void {
    if (this.closed) return;
    const delay = reconnectDelayMs(this.attempt++);
    this.timer = setTimeout(() => {
      this.timer = null;
      this.connect();
    }, delay);
  }

  private take(m: ViewMsg): void {
    const at = now();
    switch (m.type) {
      case "hello":
        this.viewer = m.viewer;
        this.botNames = m.bot_names;
        this.remap = remapOf(m.materials);
        this.owner.changed();
        return;
      case "map": {
        const old = this.world;
        this.world = worldFromMap(m);
        // Column indices remembered from another grid would name other columns in this one.
        if (!sameColumns(old, this.world)) {
          this.recent.clear();
          this.lastChange = null;
        }
        this.dirty = true;
        this.rev++;
        this.owner.changed();
        return;
      }
      case "delta": {
        // every column whose top block or ground changed, once each
        const changed = new Set<number>();
        if (applyDelta(this.world, m, changed) > 0) {
          this.dirty = true;
          this.rev++;
          // Only columns of this grid are remembered, and only for as long as they glow: a tile
          // nobody draws (the flat map, off the screen) must not collect them for ever.
          for (const [i, seen] of this.recent) if (at - seen > RECENT_GLOW_MS) this.recent.delete(i);
          for (const i of changed) {
            this.recent.set(i, at);
            this.lastChange = { i, at };
          }
          // a fork's count of differing columns moves with either world
          this.owner.changed();
          const r = this.world!.region;
          const columns = [...changed].map((i) => ({ x: r.x0 + (i % r.w), z: r.z0 + Math.floor(i / r.w) }));
          this.owner.events?.delta(this.id, at, columns);
        }
        return;
      }
      case "status": {
        this.players = m.players;
        this.dayTime = m.day_time;
        this.dirty = true;
        this.rev++;
        const tps = m.tps === null ? null : Math.round(m.tps * 10) / 10;
        const bots = botCount(m.players);
        if (tps !== this.tps || bots !== this.bots) {
          this.tps = tps;
          this.bots = bots;
          this.owner.changed();
        }
        this.owner.events?.status(this.id, m.players, at);
        return;
      }
      case "drive_ack":
        this.owner.events?.ack(this.id, m);
        return;
    }
  }

  send(text: string): boolean {
    if (!this.open || !this.ws) return false;
    this.ws.send(text);
    return true;
  }

  close(): void {
    this.closed = true;
    this.clearConnectTimer();
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    if (this.ws) {
      this.ws.onclose = null;
      this.ws.close();
    }
    this.ws = null;
    this.open = false;
  }

  /** Draw the world and the players into `canvas`, a `box`-sized tile at `dpr`. */
  draw(canvas: HTMLCanvasElement, box: Box, dpr: number): void {
    const W = Math.max(1, Math.round(box.w * dpr));
    const H = Math.max(1, Math.round(box.h * dpr));
    if (canvas.width !== W || canvas.height !== H) {
      canvas.width = W;
      canvas.height = H;
    }
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.fillStyle = MC_BACKGROUND;
    ctx.fillRect(0, 0, W, H);
    const w = this.world;
    if (w === null) return;
    if (this.image === null || this.imageSeq !== w.seq || this.image.width !== w.region.w || this.image.height !== w.region.h) {
      this.image ??= document.createElement("canvas");
      this.image.width = w.region.w;
      this.image.height = w.region.h;
      this.pixels = shadeWorld(w, this.remap, this.pixels);
      const data = new ImageData(this.pixels as Uint8ClampedArray<ArrayBuffer>, w.region.w, w.region.h);
      this.image.getContext("2d")!.putImageData(data, 0, 0);
      this.imageSeq = w.seq;
    }
    ctx.imageSmoothingEnabled = false;
    ctx.drawImage(this.image, 0, 0, W, H);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.lineCap = "round";
    ctx.font = "600 10px var(--mono, monospace)";
    ctx.textAlign = "center";
    ctx.textBaseline = "bottom";
    for (const mk of markersOf(w.region, this.players, this.botNames, box.w, box.h)) {
      if (mk.driven) {
        ctx.beginPath();
        ctx.arc(mk.x, mk.y, mk.r + 4, 0, Math.PI * 2);
        ctx.fillStyle = "rgba(255, 255, 255, 0.35)";
        ctx.fill();
      }
      ctx.beginPath();
      ctx.moveTo(mk.x, mk.y);
      ctx.lineTo(mk.tx, mk.ty);
      ctx.strokeStyle = "rgba(0, 0, 0, 0.75)";
      ctx.lineWidth = 4;
      ctx.stroke();
      ctx.strokeStyle = mk.driven ? "#ffffff" : mk.color;
      ctx.lineWidth = 2;
      ctx.stroke();
      ctx.beginPath();
      ctx.arc(mk.x, mk.y, mk.r, 0, Math.PI * 2);
      ctx.fillStyle = mk.color;
      ctx.fill();
      ctx.lineWidth = mk.driven ? 2.5 : 1.5;
      ctx.strokeStyle = mk.driven ? "#ffffff" : "rgba(0, 0, 0, 0.8)";
      ctx.stroke();
      if (mk.label) {
        ctx.lineWidth = 3;
        ctx.strokeStyle = "rgba(0, 0, 0, 0.8)";
        ctx.strokeText(mk.name, mk.x, mk.y - mk.r - 3);
        ctx.fillStyle = "#ffffff";
        ctx.fillText(mk.name, mk.x, mk.y - mk.r - 3);
      }
    }
  }
}

/** Every tile's socket, opened and closed as the view model asks. */
export class McTiles {
  private sockets = new Map<string, TileSocket>();
  private listeners = new Set<() => void>();
  private snap: Record<string, McTileLive> = {};
  /** For a fork's copy (by tile id): the tile whose world it was copied from. */
  private forks = new Map<string, string>();
  events: TileEvents | null = null;

  /** Open what is asked for, close what is not; a changed `key` replaces a tile's socket. */
  sync(wanted: readonly { id: string; socket: { url: string; first: string; key: string } | null; forkOf?: string | null }[]): void {
    const keep = new Set<string>();
    this.forks.clear();
    for (const w of wanted) {
      if (w.forkOf) this.forks.set(w.id, w.forkOf);
      if (w.socket === null) continue;
      keep.add(w.id);
      const cur = this.sockets.get(w.id);
      if (cur && cur.key === w.socket.key && cur.first === w.socket.first) continue;
      cur?.close();
      this.sockets.set(w.id, new TileSocket(w.id, w.socket.key, w.socket.url, w.socket.first, this));
    }
    for (const [id, s] of this.sockets) {
      if (keep.has(id)) continue;
      s.close();
      this.sockets.delete(id);
    }
    this.changed();
  }

  /** Reconnect tile `id`'s socket at once, if it is waiting to try again. */
  retryNow(id: string): void {
    this.sockets.get(id)?.retryNow();
  }

  send(id: string, msg: DriveMsg): boolean {
    return this.sockets.get(id)?.send(JSON.stringify(msg)) ?? false;
  }

  /** Draw tile `id` when anything changed or `force`; returns whether it drew. */
  draw(id: string, canvas: HTMLCanvasElement, box: Box, dpr: number, force: boolean): boolean {
    const s = this.sockets.get(id);
    if (!s) {
      if (!force) return false;
      const W = Math.max(1, Math.round(box.w * dpr));
      const H = Math.max(1, Math.round(box.h * dpr));
      if (canvas.width !== W || canvas.height !== H) {
        canvas.width = W;
        canvas.height = H;
      }
      const ctx = canvas.getContext("2d");
      if (ctx) {
        ctx.fillStyle = MC_BACKGROUND;
        ctx.fillRect(0, 0, W, H);
      }
      return true;
    }
    if (!s.dirty && !force) return false;
    s.dirty = false;
    s.draw(canvas, box, dpr);
    return true;
  }

  changed(): void {
    const next: Record<string, McTileLive> = {};
    for (const [id, s] of this.sockets) {
      const from = this.source(id);
      next[id] = { open: s.open, viewer: s.viewer, tps: s.tps, hasMap: s.world !== null, failures: s.failures, connectingSince: s.connectingSince, bots: s.world === null ? 0 : s.bots, diff: from === null ? null : countDiff(from.cells, s.world!.cells) };
    }
    if (JSON.stringify(next) === JSON.stringify(this.snap)) return;
    this.snap = next;
    for (const f of this.listeners) f();
  }

  /** The world tile `id`'s world was forked from, while the page holds both and they are the same grid of the same materials. */
  source(id: string): McWorld | null {
    const mine = this.sockets.get(id);
    const from = this.sockets.get(this.forks.get(id) ?? "");
    if (!mine?.world || !from?.world) return null;
    return sameGrid(mine.world, mine.remap, from.world, from.remap) ? from.world : null;
  }

  /** What tile `id`'s socket holds now, for the 3D view. */
  peek(id: string): TileState | null {
    return this.sockets.get(id) ?? null;
  }

  subscribe = (f: () => void): (() => void) => {
    this.listeners.add(f);
    return () => this.listeners.delete(f);
  };

  getLive = (): Record<string, McTileLive> => this.snap;

  /** For the test hook: what each socket holds. */
  debug(): Record<string, { url: string; open: boolean; viewer: string | null; tps: number | null; worldSeq: number | null; players: McPlayer[]; errors: number }> {
    const out: ReturnType<McTiles["debug"]> = {};
    for (const [id, s] of this.sockets) out[id] = { url: s.url, open: s.open, viewer: s.viewer, tps: s.tps, worldSeq: s.world?.seq ?? null, players: s.players, errors: s.errors };
    return out;
  }

  stop(): void {
    for (const s of this.sockets.values()) s.close();
    this.sockets.clear();
    this.changed();
  }
}
