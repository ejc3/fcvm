// Mock renderers: one WebSocket route per tile, speaking the renderer's frame
// protocol. A tile's picture is a pure function of (t, its rectangle, the scene),
// drawn at the height each subscriber asks for and encoded with libx264. All
// tiles step one shared clock, so their frames land on the same t values.

import { spawn } from "node:child_process";
import http from "node:http";
import { WebSocketServer, type WebSocket } from "ws";
import { buildFrame } from "../core/frame.ts";
import { deliveredGeometry, tileGeometry, type TileGeometry } from "../core/layout.ts";
import type { Rect } from "../core/protocol.ts";
import { serveMockMirror } from "./mirror.ts";
import { Encoder, FFMPEG } from "./encoder.ts";
import type { PatternScene } from "./pattern.ts";
import { RenderPool } from "./render-pool.ts";

function groupsAlive(subs: Set<{ tile: string; geomKey: string }>, key: string): boolean {
  for (const s of subs) if (`${s.tile}|${s.geomKey}` === key) return true;
  return false;
}

/** Frames of one stream drawn at the same time, at most. */
const MAX_INFLIGHT = 4;

export interface RendererOptions {
  port: number;
  /** Host name the browser uses to reach this server, for frame_url (the IPv6 address). */
  publicHost: string;
  /**
   * Host name for frame_url_v4. The mock stands in for the two families with two names for
   * one server: a socket is IPv4 when its Host is this name, IPv6 otherwise.
   */
  publicHostV4?: string;
  /** The port the browser reaches it on, when that is not its own (a proxy in front). */
  publicPort?: number;
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
  /** For an aggregator: the image rectangles of its tiles that are not running, drawn black. */
  black: () => Rect[];
  /** Seconds added to the drawn t while keeping the stamp: a seam fault for tests. */
  skew: number;
  /** A scene of its own (a viewer's own GPU, with its own camera), or null for the shared one. */
  scene: (() => PatternScene) | null;
  /** A re-tile: the rectangle this stream draws from scene t `atT` on. */
  pending: { rect: Rect; atT: number } | null;
}

/** The renderer's q levels as libx264 quality: hi, std (the default), lo. */
const CRF: Record<string, number> = { hi: 14, std: 18, lo: 26 };

interface Sub {
  q: string;
  family: "v6" | "v4";
  ws: WebSocket;
  tile: string;
  width: number;
  height: number;
  geometry: TileGeometry | null;
  /** The height the subscriber asked for. */
  reqH: number;
  /** Where the encoder's frames sit in the image; a frame drawn for another placement is not sent. */
  geomKey: string;
  encoder: Encoder | null;
  /** Stamps of frames written to the encoder, oldest first. */
  stamps: { t: number; frame: number }[];
  sentKey: boolean;
}

export interface RendererStats {
  subscribers: number;
  /** Open frame sockets by the quality they were opened with (q is read from the URL at connection time only). */
  subscribersByQuality: Record<string, number>;
  /** Open frame sockets by the address family they came over (v6 or v4). */
  subscribersByFamily: Record<string, number>;
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
  readonly stats: RendererStats = { subscribers: 0, subscribersByQuality: {}, subscribersByFamily: {}, framesSent: 0, framesSkipped: 0, renderBacklog: 0 };

  constructor(opts: RendererOptions) {
    this.opts = opts;
    this.pool = new RenderPool(opts.threads);
    this.server = http.createServer((_req, res) => {
      res.writeHead(404).end();
    });
    this.wss = new WebSocketServer({ noServer: true, perMessageDeflate: false });
    this.server.on("upgrade", (req, socket, head) => {
      const url = new URL(req.url ?? "/", "http://mock");
      // a phone's GPU mirror (mock/mirror.ts)
      const mm = /^\/frames\/([A-Za-z0-9_-]+)\/mirror$/.exec(url.pathname);
      if (mm && this.tiles.has(mm[1]!)) {
        this.wss.handleUpgrade(req, socket, head, (ws) => serveMockMirror(ws, process.env.FLEET_MOCK_MIRROR_TAP || undefined));
        return;
      }
      const m = /^\/frames\/([A-Za-z0-9_-]+)$/.exec(url.pathname);
      const tile = m ? this.tiles.get(m[1]!) : undefined;
      if (!m || !tile) {
        socket.end("HTTP/1.1 404 Not Found\r\n\r\n");
        return;
      }
      if (!tile.available || this.silent) {
        socket.end("HTTP/1.1 503 Service Unavailable\r\n\r\n");
        return;
      }
      const family = (req.headers.host ?? "").replace(/:\d+$/, "") === this.v4Host ? "v4" : "v6";
      // A viewer with no IPv6: its IPv6 connects fail.
      if (family === "v6" && this.v6Down) {
        socket.destroy();
        return;
      }
      const h = Number(url.searchParams.get("h") ?? "144");
      const q = url.searchParams.get("q") ?? "std";
      if (!(q in CRF)) {
        socket.end("HTTP/1.1 400 Bad Request\r\n\r\n");
        return;
      }
      this.wss.handleUpgrade(req, socket, head, (ws) => this.subscribe(ws, m[1]!, h, q, family));
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
    return `ws://${this.opts.publicHost}:${this.opts.publicPort ?? this.port}/frames/${id}`;
  }

  private get v4Host(): string {
    return this.opts.publicHostV4 ?? "127.0.0.1";
  }

  /** The same stream as frameUrl over the IPv4 name. */
  frameUrlV4(id: string): string {
    return `ws://${this.v4Host}:${this.opts.publicPort ?? this.port}/frames/${id}`;
  }

  private v6Down = false;
  /** Tests: refuse every IPv6 frame socket, as for a viewer with no IPv6. */
  setV6Down(down: boolean): void {
    this.v6Down = down;
  }

  setTile(id: string, rect: Rect, black: () => Rect[] = () => [], scene: (() => PatternScene) | null = null): void {
    const e = this.tiles.get(id);
    this.tiles.set(id, { rect, available: e?.available ?? true, skew: e?.skew ?? 0, black, scene, pending: null });
    if (e && (e.rect.x !== rect.x || e.rect.y !== rect.y || e.rect.w !== rect.w || e.rect.h !== rect.h)) {
      for (const s of this.subs) if (s.tile === id) this.resize(s, s.reqH);
    }
  }

  /** A re-tile: from scene t `atT` on, the stream draws `rect` (a new header and keyframe then). */
  retile(id: string, rect: Rect, atT: number): void {
    const e = this.tiles.get(id);
    if (e) e.pending = { rect, atT };
  }

  /** Forget a stream (a viewer's own GPU went back to the pool) and close its sockets. */
  removeTile(id: string): void {
    this.setAvailable(id, false);
    this.tiles.delete(id);
  }

  /** The whole image at t as a JPEG, for the coordinator's poster. */
  async poster(width: number, height: number, t: number, black: Rect[]): Promise<Buffer> {
    const geometry: TileGeometry = { width, height, fullWidth: width, fullHeight: height, x0: 0, y0: 0 };
    const i420 = await this.pool.render({ geometry, drawT: t, scene: this.opts.scene(), black });
    return await new Promise<Buffer>((resolve, reject) => {
      const p = spawn(FFMPEG, ["-hide_banner", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", `${width}x${height}`, "-i", "pipe:0", "-frames:v", "1", "-q:v", "4", "-f", "mjpeg", "pipe:1"]);
      const out: Buffer[] = [];
      let err = "";
      p.stdout.on("data", (d: Buffer) => out.push(d));
      p.stderr.on("data", (d: Buffer) => (err += d.toString()));
      p.on("error", reject);
      p.on("close", (code) => (code === 0 ? resolve(Buffer.concat(out)) : reject(new Error(`ffmpeg exited ${code}: ${err.trim()}`))));
      p.stdin.end(Buffer.from(i420.buffer, i420.byteOffset, i420.byteLength));
    });
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

  /** Tests: every renderer refuses connections (and drops its own), as if none had started. */
  private silent = false;
  setSilent(silent: boolean): void {
    this.silent = silent;
    if (silent) for (const s of [...this.subs]) s.ws.close(1001, "silent");
  }

  setSkew(id: string, seconds: number): void {
    const e = this.tiles.get(id);
    if (e) e.skew = seconds;
  }

  private clampHeight(h: number): number {
    return Math.min(this.opts.maxHeight, Number.isFinite(h) ? h : 144);
  }

  private subscribe(ws: WebSocket, tile: string, h: number, q: string, family: "v6" | "v4"): void {
    const sub: Sub = { q, family, ws, tile, width: 0, height: 0, geometry: null, reqH: h, geomKey: "", encoder: null, stamps: [], sentKey: false };
    this.subs.add(sub);
    this.countSubs();
    this.resize(sub, h);
    ws.on("message", (data, isBinary) => {
      if (isBinary) return;
      try {
        const m = JSON.parse(data.toString()) as { h?: unknown };
        if (typeof m.h === "number") {
          sub.reqH = m.h;
          this.resize(sub, m.h);
        }
      } catch {
        // Not a control message; ignored.
      }
    });
    ws.on("close", () => {
      sub.encoder?.close();
      this.subs.delete(sub);
      this.countSubs();
    });
    ws.on("error", () => {});
  }

  private countSubs(): void {
    this.stats.subscribers = this.subs.size;
    const by: Record<string, number> = {};
    const fam: Record<string, number> = {};
    for (const s of this.subs) {
      by[s.q] = (by[s.q] ?? 0) + 1;
      fam[s.family] = (fam[s.family] ?? 0) + 1;
    }
    this.stats.subscribersByQuality = by;
    this.stats.subscribersByFamily = fam;
  }

  private resize(sub: Sub, h: number): void {
    const entry = this.tiles.get(sub.tile);
    if (!entry) return;
    const tile = tileGeometry(entry.rect, (entry.scene ?? this.opts.scene)().image_aspect, this.clampHeight(h));
    // Like the renderer, encode the tile plus up to 16 px of the image around it
    // and say in the header how much to crop.
    const delivered = deliveredGeometry(tile);
    const crop = delivered.crop;
    const geometry: TileGeometry = {
      ...tile,
      width: delivered.width,
      height: delivered.height,
      x0: tile.x0 - crop.left,
      y0: tile.y0 - crop.top,
    };
    const { width, height } = geometry;
    const key = JSON.stringify(geometry);
    if (sub.encoder && key === sub.geomKey) return;
    sub.geomKey = key;
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
      crf: CRF[sub.q] ?? 18,
      onCodec: (codec) => {
        if (sub.encoder !== enc || sub.ws.readyState !== sub.ws.OPEN) return;
        sub.ws.send(
          JSON.stringify({
            codec,
            width,
            height,
            full_width: tile.fullWidth,
            full_height: tile.fullHeight,
            x0: tile.x0,
            y0: tile.y0,
            fps,
            crop,
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
    // Re-tiles due at this frame take effect before it is drawn.
    for (const [id, e] of this.tiles) {
      if (!e.pending || t < e.pending.atT - 1e-9) continue;
      e.rect = e.pending.rect;
      e.pending = null;
      for (const s of this.subs) if (s.tile === id) this.resize(s, s.reqH);
    }
    this.stats.renderBacklog = this.pool.backlog;
    // Behind its clock (the last frame's draws have not all started), the renderer skips
    // this frame for every stream at once. Skipping per stream would leave each tile a
    // different subset of t, and a page draws all of them at one t: with 15 tiles each
    // missing 4 frames in 10, no t is held by all 15, and the page never shows them in sync.
    if (this.pool.backlog > 0) {
      for (const s of this.subs) if (s.encoder) this.stats.framesSkipped++;
      return;
    }
    const groups = new Map<string, Sub[]>();
    for (const s of this.subs) {
      if (!s.encoder) continue;
      const gk = `${s.tile}|${s.geomKey}`;
      const g = groups.get(gk);
      if (g) g.push(s);
      else groups.set(gk, [s]);
    }
    const scene = this.opts.scene();
    for (const [laneKey, subs] of groups) {
      // Several frames of one stream are drawn at once on the pool, and delivered
      // to the encoder in frame order.
      // Only a stream whose frame takes longer to draw than a frame period gets
      // more than one in flight; the rest keep the shortest latency.
      const lane = this.lanes.get(laneKey) ?? { inflight: 0, tail: Promise.resolve(), lastMs: 0 };
      this.lanes.set(laneKey, lane);
      const allowed = Math.min(MAX_INFLIGHT, Math.max(1, Math.ceil(lane.lastMs / (1000 / fps))));
      if (lane.inflight >= allowed) {
        this.stats.framesSkipped += subs.length;
        continue;
      }
      const s0 = subs[0]!;
      const entry = this.tiles.get(s0.tile);
      if (!entry || !entry.available) continue;
      // The placement this frame is drawn for: a subscriber whose height or
      // rectangle changed while it was drawn has a new encoder and must not get it.
      const key = s0.geomKey;
      const geometry = s0.geometry!;
      lane.inflight++;
      const started = Date.now();
      const drawn = this.pool.render({ geometry, drawT: t + entry.skew, scene: entry.scene ? entry.scene() : scene, black: entry.black() }).then((b) => {
        lane.lastMs = Date.now() - started;
        return b;
      });
      lane.tail = Promise.all([drawn, lane.tail])
        .then(async ([buf]) => {
          if (this.opts.holdMs) await new Promise((r) => setTimeout(r, this.opts.holdMs));
          for (const s of subs) {
            if (!this.subs.has(s) || s.geomKey !== key || !s.encoder) continue;
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
          if (lane.inflight === 0 && !groupsAlive(this.subs, laneKey)) this.lanes.delete(laneKey);
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
