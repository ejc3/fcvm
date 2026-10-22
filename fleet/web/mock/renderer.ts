// Mock renderers: one WebSocket route per tile, speaking the renderer's frame
// protocol. A tile's picture is a pure function of (t, its rectangle, the scene),
// drawn at the height each subscriber asks for and encoded with libx264. All
// tiles step one shared clock, so their frames land on the same t values.

import http from "node:http";
import { WebSocketServer, type WebSocket } from "ws";
import { buildFrame } from "../core/frame.ts";
import { tileGeometry, type TileGeometry } from "../core/layout.ts";
import type { Rect } from "../core/protocol.ts";
import { Encoder } from "./encoder.ts";
import type { PatternScene } from "./pattern.ts";
import { RenderPool } from "./render-pool.ts";

function groupsAlive(subs: Set<{ tile: string; width: number; height: number }>, key: string): boolean {
  for (const s of subs) if (`${s.tile}|${s.width}x${s.height}` === key) return true;
  return false;
}

/** Frames of one stream drawn at the same time, at most. */
const MAX_INFLIGHT = 4;

export interface RendererOptions {
  port: number;
  /** Host name the browser uses to reach this server, for frame_url. */
  publicHost: string;
  fps: number;
  threads: number;
  maxHeight: number;
  epochUnixMs: number;
  scene: () => PatternScene;
  /** Tests: hold each drawn frame this long before encoding it. */
  holdMs?: number;
}

interface TileEntry {
  rect: Rect;
  available: boolean;
  /** Seconds added to the drawn t while keeping the stamp: a seam fault for tests. */
  skew: number;
}

interface Sub {
  ws: WebSocket;
  tile: string;
  width: number;
  height: number;
  geometry: TileGeometry | null;
  encoder: Encoder | null;
  /** Stamps of frames written to the encoder, oldest first. */
  stamps: { t: number; frame: number }[];
  sentKey: boolean;
}

export interface RendererStats {
  subscribers: number;
  framesSent: number;
  framesSkipped: number;
  renderBacklog: number;
}

export class MockRenderers {
  private opts: RendererOptions;
  private tiles = new Map<string, TileEntry>();
  private subs = new Set<Sub>();
  /** Per stream key: frames being drawn, and the promise of the last one delivered. */
  private lanes = new Map<string, { inflight: number; tail: Promise<void>; lastMs: number }>();
  private pool: RenderPool;
  private server: http.Server;
  private wss: WebSocketServer;
  private timer: NodeJS.Timeout | null = null;
  private lastFrame = -1;
  readonly stats: RendererStats = { subscribers: 0, framesSent: 0, framesSkipped: 0, renderBacklog: 0 };

  constructor(opts: RendererOptions) {
    this.opts = opts;
    this.pool = new RenderPool(opts.threads);
    this.server = http.createServer((_req, res) => {
      res.writeHead(404).end();
    });
    this.wss = new WebSocketServer({ noServer: true, perMessageDeflate: false });
    this.server.on("upgrade", (req, socket, head) => {
      const url = new URL(req.url ?? "/", "http://mock");
      const m = /^\/frames\/([A-Za-z0-9_-]+)$/.exec(url.pathname);
      const tile = m ? this.tiles.get(m[1]!) : undefined;
      if (!m || !tile) {
        socket.end("HTTP/1.1 404 Not Found\r\n\r\n");
        return;
      }
      if (!tile.available) {
        socket.end("HTTP/1.1 503 Service Unavailable\r\n\r\n");
        return;
      }
      const h = Number(url.searchParams.get("h") ?? "144");
      this.wss.handleUpgrade(req, socket, head, (ws) => this.subscribe(ws, m[1]!, h));
    });
  }

  listen(): Promise<void> {
    return new Promise((resolve) => {
      this.server.listen(this.opts.port, () => {
        const period = 1000 / this.opts.fps;
        this.timer = setInterval(() => this.tick(), Math.max(4, period / 4));
        resolve();
      });
    });
  }

  get port(): number {
    const a = this.server.address();
    return typeof a === "object" && a ? a.port : this.opts.port;
  }

  frameUrl(id: string): string {
    return `ws://${this.opts.publicHost}:${this.port}/frames/${id}`;
  }

  setTile(id: string, rect: Rect): void {
    const e = this.tiles.get(id);
    this.tiles.set(id, { rect, available: e?.available ?? true, skew: e?.skew ?? 0 });
  }

  /** A tile with no renderer refuses connections and drops the ones it has (its VM is gone). */
  setAvailable(id: string, available: boolean): void {
    const e = this.tiles.get(id);
    if (!e) return;
    e.available = available;
    if (!available) {
      for (const s of [...this.subs]) if (s.tile === id) s.ws.close(1001, "relaunching");
    }
  }

  setSkew(id: string, seconds: number): void {
    const e = this.tiles.get(id);
    if (e) e.skew = seconds;
  }

  private clampHeight(h: number): number {
    return Math.min(this.opts.maxHeight, Number.isFinite(h) ? h : 144);
  }

  private subscribe(ws: WebSocket, tile: string, h: number): void {
    const sub: Sub = { ws, tile, width: 0, height: 0, geometry: null, encoder: null, stamps: [], sentKey: false };
    this.subs.add(sub);
    this.stats.subscribers = this.subs.size;
    this.resize(sub, h);
    ws.on("message", (data, isBinary) => {
      if (isBinary) return;
      try {
        const m = JSON.parse(data.toString()) as { h?: unknown };
        if (typeof m.h === "number") this.resize(sub, m.h);
      } catch {
        // Not a control message; ignored.
      }
    });
    ws.on("close", () => {
      sub.encoder?.close();
      this.subs.delete(sub);
      this.stats.subscribers = this.subs.size;
    });
    ws.on("error", () => {});
  }

  private resize(sub: Sub, h: number): void {
    const entry = this.tiles.get(sub.tile);
    if (!entry) return;
    const geometry = tileGeometry(entry.rect, this.opts.scene().image_aspect, this.clampHeight(h));
    const { width, height } = geometry;
    if (sub.encoder && width === sub.width && height === sub.height) return;
    sub.encoder?.close();
    sub.width = width;
    sub.height = height;
    sub.geometry = geometry;
    sub.stamps = [];
    sub.sentKey = false;
    const fps = this.opts.fps;
    const enc: Encoder = new Encoder({
      width,
      height,
      fps,
      gop: Math.round(fps * 2),
      onCodec: (codec) => {
        if (sub.encoder !== enc || sub.ws.readyState !== sub.ws.OPEN) return;
        sub.ws.send(
          JSON.stringify({
            codec,
            width,
            height,
            full_width: geometry.fullWidth,
            full_height: geometry.fullHeight,
            x0: geometry.x0,
            y0: geometry.y0,
            fps,
          }),
        );
      },
      onUnit: (u) => {
        if (sub.encoder !== enc) return;
        const stamp = sub.stamps.shift();
        if (!stamp || sub.ws.readyState !== sub.ws.OPEN) return;
        if (!sub.sentKey && !u.keyframe) return;
        sub.sentKey = true;
        sub.ws.send(buildFrame({ keyframe: u.keyframe, width, height, t: stamp.t, frame: stamp.frame }, u.data));
        this.stats.framesSent++;
      },
      onExit: (code, stderr) => {
        if (sub.encoder === enc && code !== 0 && code !== null) {
          console.error(`mock renderer: ffmpeg for ${sub.tile} exited ${code}: ${stderr.trim()}`);
          sub.ws.close(1011, "encoder failed");
        }
      },
    });
    sub.encoder = enc;
  }

  private tick(): void {
    const fps = this.opts.fps;
    const n = Math.floor(((Date.now() - this.opts.epochUnixMs) / 1000) * fps);
    if (n <= this.lastFrame) return;
    this.lastFrame = n;
    const t = n / fps;
    this.stats.renderBacklog = this.pool.backlog;
    const groups = new Map<string, Sub[]>();
    for (const s of this.subs) {
      if (!s.encoder) continue;
      const key = `${s.tile}|${s.width}x${s.height}`;
      const g = groups.get(key);
      if (g) g.push(s);
      else groups.set(key, [s]);
    }
    const scene = this.opts.scene();
    for (const [key, subs] of groups) {
      // Several frames of one stream are drawn at once on the pool, and delivered
      // to the encoder in frame order.
      // Only a stream whose frame takes longer to draw than a frame period gets
      // more than one in flight; the rest keep the shortest latency.
      const lane = this.lanes.get(key) ?? { inflight: 0, tail: Promise.resolve(), lastMs: 0 };
      this.lanes.set(key, lane);
      const allowed = Math.min(MAX_INFLIGHT, Math.max(1, Math.ceil(lane.lastMs / (1000 / fps))));
      if (lane.inflight >= allowed) {
        this.stats.framesSkipped += subs.length;
        continue;
      }
      const s0 = subs[0]!;
      const entry = this.tiles.get(s0.tile);
      if (!entry || !entry.available) continue;
      // The size this frame is drawn at: a subscriber that changed height while
      // it was drawn has a new encoder and must not get it.
      const width = s0.width;
      const height = s0.height;
      const geometry = s0.geometry!;
      lane.inflight++;
      const started = Date.now();
      const drawn = this.pool.render({ geometry, drawT: t + entry.skew, scene }).then((b) => {
        lane.lastMs = Date.now() - started;
        return b;
      });
      lane.tail = Promise.all([drawn, lane.tail])
        .then(async ([buf]) => {
          if (this.opts.holdMs) await new Promise((r) => setTimeout(r, this.opts.holdMs));
          for (const s of subs) {
            if (!this.subs.has(s) || s.width !== width || s.height !== height || !s.encoder) continue;
            if (s.ws.bufferedAmount > 4 << 20) {
              this.stats.framesSkipped++;
              continue;
            }
            if (s.encoder.write(buf)) s.stamps.push({ t, frame: n });
            else this.stats.framesSkipped++;
          }
        })
        .catch((e: unknown) => {
          console.error("mock renderer: render failed", e);
        })
        .finally(() => {
          lane.inflight--;
          if (lane.inflight === 0 && !groupsAlive(this.subs, key)) this.lanes.delete(key);
        });
    }
  }

  async close(): Promise<void> {
    if (this.timer) clearInterval(this.timer);
    for (const s of this.subs) {
      s.encoder?.close();
      s.ws.terminate();
    }
    this.wss.close();
    await new Promise<void>((r) => this.server.close(() => r()));
    await this.pool.close();
  }
}
