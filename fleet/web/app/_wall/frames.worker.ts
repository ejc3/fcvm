/// <reference lib="webworker" />
// The frame worker: one WebSocket and one WebCodecs VideoDecoder per visible
// tile, a short buffer of decoded frames per tile, and one OffscreenCanvas that
// draws every tile at one common t. The rules (which t, which frame, when to
// reconnect, which height) come from core/; this file is the platform glue.

import {
  BUFFER_FRAMES,
  chooseCommonT,
  codecFromSps,
  admitChunk,
  heightMessage,
  heightSent,
  heightToSend,
  innerRect,
  NO_CROP,
  nalType,
  newStream,
  streamOpened,
  wantHeight,
  type Crop,
  type StreamCtl,
  NAL_SPS,
  parseFrame,
  parseStreamHeader,
  reconnectDelayMs,
  releaseBefore,
  SAME_T_EPS,
  splitNals,
  type Box,
  type DrawPlan,
  type DrawTile,
  type FrameStats,
  type SyncChoice,
  type TileFrameStats,
} from "../../core/index.ts";
import type { BenchStats, CaptureResult, DrawSettings, FromWorker, ToWorker } from "./worker-msgs.ts";

declare const self: DedicatedWorkerGlobalScope;

const now = () => performance.timeOrigin + performance.now();

interface Held {
  t: number;
  frame: VideoFrame;
  /** The stream's margins when this frame was decoded. */
  crop: Crop;
  grid: Grid | null;
}

type Grid = { fullWidth: number; fullHeight: number; x0: number; y0: number };

class TileStream {
  readonly id: string;
  baseUrl: string;
  url: string;
  height: number;
  /** What height this socket was opened with and has been sent. */
  ctl: StreamCtl;
  ws: WebSocket | null = null;
  decoder: VideoDecoder | null = null;
  codec: string | null = null;
  codedW = 0;
  codedH = 0;
  waitingKey = true;
  frames: Held[] = [];
  attempt = 0;
  retry: ReturnType<typeof setTimeout> | null = null;
  closed = false;
  decodedTimes: number[] = [];
  decodeStart = new Map<number, number>();
  decodeMs: number[] = [];
  lastW = 0;
  lastH = 0;
  /** Margins of the stream as the last header gave them. */
  crop: Crop = NO_CROP;
  /** Where the inner rectangle sits in the whole image's pixel grid (from the header). */
  grid: Grid | null = null;
  /** Bytes of frame messages received, for the bandwidth reading. */
  bytes = 0;
  /** When the tile last went from nothing shown to a frame (for the fade-in). */
  appearedAt: number | null = null;

  constructor(id: string, baseUrl: string, url: string, height: number) {
    this.id = id;
    this.baseUrl = baseUrl;
    this.url = url;
    this.height = height;
    this.ctl = newStream(height);
    this.open();
  }

  /** Send the wanted height if the open socket does not have it yet. */
  private syncHeight(): void {
    const h = heightToSend(this.ctl);
    if (h === null || !this.ws) return;
    this.ws.send(heightMessage(h));
    this.ctl = heightSent(this.ctl, h);
  }

  open(): void {
    if (this.closed) return;
    const ws = new WebSocket(this.url);
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    this.ctl = { ...newStream(this.height), wanted: this.height };
    ws.onopen = () => {
      if (this.ws !== ws) return;
      this.ctl = streamOpened(this.ctl, true);
      this.syncHeight();
    };
    ws.onmessage = (e) => {
      if (typeof e.data === "string") this.onHeader(e.data);
      else this.onFrame(new Uint8Array(e.data as ArrayBuffer));
    };
    ws.onclose = () => {
      if (this.ws !== ws) return;
      this.ws = null;
      this.resetDecoder();
      if (this.closed) return;
      const delay = reconnectDelayMs(this.attempt++);
      this.retry = setTimeout(() => {
        this.retry = null;
        this.open();
      }, delay);
    };
    ws.onerror = () => {};
  }

  setHeight(height: number, url: string): void {
    this.height = height;
    this.url = url;
    this.ctl = wantHeight(this.ctl, height);
    this.syncHeight();
  }

  private onHeader(text: string): void {
    const h = parseStreamHeader(text);
    if (!h) return;
    this.crop = h.crop;
    this.grid = h.grid;
    this.configure(h.codec, h.width, h.height);
  }

  private configure(codec: string, w: number, h: number): void {
    if (this.decoder && this.decoder.state === "configured" && this.codec === codec && this.codedW === w && this.codedH === h) return;
    if (!this.decoder || this.decoder.state === "closed") {
      this.decoder = new VideoDecoder({
        output: (f) => this.onDecoded(f),
        error: () => this.resetDecoder(),
      });
    }
    this.decoder.configure({ codec, codedWidth: w, codedHeight: h, optimizeForLatency: true });
    this.codec = codec;
    this.codedW = w;
    this.codedH = h;
    this.waitingKey = true;
  }

  private onFrame(buf: Uint8Array): void {
    this.bytes += buf.byteLength;
    const p = parseFrame(buf);
    if (!p.ok) return;
    const f = p.frame;
    if (f.keyframe && (f.width !== this.codedW || f.height !== this.codedH || !this.decoder || this.decoder.state !== "configured")) {
      // A size change, or no header: take the codec from the keyframe's SPS.
      const sps = splitNals(f.data).find((n) => nalType(n) === NAL_SPS);
      const codec = sps ? codecFromSps(sps) : this.codec;
      if (codec) this.configure(codec, f.width, f.height);
    }
    if (!this.decoder || this.decoder.state !== "configured") return;
    const admit = admitChunk({ waitingKey: this.waitingKey }, { keyframe: f.keyframe, queueSize: this.decoder.decodeQueueSize });
    this.waitingKey = admit.waitingKey;
    if (!admit.decode) return;
    this.attempt = 0;
    const ts = Math.round(f.t * 1e6);
    this.decodeStart.set(ts, performance.now());
    if (this.decodeStart.size > 64) this.decodeStart.delete(this.decodeStart.keys().next().value!);
    try {
      this.decoder.decode(new EncodedVideoChunk({ type: f.keyframe ? "key" : "delta", timestamp: ts, data: f.data }));
    } catch {
      this.resetDecoder();
    }
  }

  private onDecoded(frame: VideoFrame): void {
    const t = frame.timestamp / 1e6;
    const started = this.decodeStart.get(frame.timestamp);
    if (started !== undefined) {
      this.decodeMs.push(performance.now() - started);
      if (this.decodeMs.length > 120) this.decodeMs.shift();
      this.decodeStart.delete(frame.timestamp);
    }
    if (this.closed) {
      frame.close();
      return;
    }
    const crop = this.crop;
    const grid = this.grid;
    this.lastW = frame.displayWidth - crop.left - crop.right;
    this.lastH = frame.displayHeight - crop.top - crop.bottom;
    this.decodedTimes.push(now());
    // Keep ascending order; a repeated t replaces the old frame.
    const at = this.frames.findIndex((h) => h.t >= t - SAME_T_EPS);
    if (at >= 0 && Math.abs(this.frames[at]!.t - t) <= SAME_T_EPS) {
      this.frames[at]!.frame.close();
      this.frames[at] = { t, frame, crop, grid };
    } else if (at >= 0) {
      this.frames.splice(at, 0, { t, frame, crop, grid });
    } else {
      this.frames.push({ t, frame, crop, grid });
    }
    while (this.frames.length > BUFFER_FRAMES) this.frames.shift()!.frame.close();
  }

  resetDecoder(): void {
    if (this.decoder && this.decoder.state !== "closed") {
      try {
        this.decoder.close();
      } catch {
        // Already closed.
      }
    }
    this.decoder = null;
    this.codec = null;
    this.codedW = 0;
    this.codedH = 0;
    this.waitingKey = true;
    this.decodeStart.clear();
  }

  dropFrames(): void {
    for (const h of this.frames) h.frame.close();
    this.frames = [];
    this.appearedAt = null;
  }

  releaseOlder(t: number): void {
    while (this.frames.length > 1 && this.frames[0]!.t < t) this.frames.shift()!.frame.close();
  }

  frameAt(t: number): Held | null {
    for (const h of this.frames) if (Math.abs(h.t - t) <= SAME_T_EPS) return h;
    return null;
  }

  fps(at: number): number {
    while (this.decodedTimes.length > 0 && this.decodedTimes[0]! < at - 2000) this.decodedTimes.shift();
    return this.decodedTimes.length / 2;
  }

  close(): void {
    this.closed = true;
    if (this.retry) clearTimeout(this.retry);
    const ws = this.ws;
    this.ws = null;
    ws?.close();
    this.resetDecoder();
    this.dropFrames();
  }
}

// ---- state -------------------------------------------------------------------

let canvas: OffscreenCanvas | null = null;
let ctx: OffscreenCanvasRenderingContext2D | null = null;
let dpr = 1;
let plan: DrawPlan = { tiles: [], image: null, dpr: 1, darkFill: "#06080B", estimatedBps: 0 };
let settings: DrawSettings = { background: "#000000", dimAlpha: 0.5, fadeInMs: 280 };
const streams = new Map<string, TileStream>();
let prevT: number | null = null;
let lastChoice: SyncChoice = { T: null, show: new Map() };
let watches: { id: string; tEff: number }[] = [];
const reported = new Set<string>();
let captures: { req: number; points: { x: number; y: number }[] }[] = [];
const drawMs: number[] = [];
const rafMs: number[] = [];
const drawTimes: number[] = [];
let lastFrameAt: number | null = null;

let mainTicks = false;
/** Something other than a new common t needs a redraw (plan, size, a fade, a capture). */
let dirty = true;
let lastSig = "";
/** The background needs painting (a new plan or size); otherwise tiles repaint only their own boxes. */
let needsClear = true;

function post(m: FromWorker): void {
  self.postMessage(m);
}

function applyPlan(p: DrawPlan): void {
  plan = p;
  dirty = true;
  needsClear = true;
  const wanted = new Set<string>();
  for (const t of p.tiles) {
    if (t.url === null || t.baseUrl === null) continue;
    wanted.add(t.id);
    const s = streams.get(t.id);
    if (!s || s.baseUrl !== t.baseUrl) {
      s?.close();
      streams.set(t.id, new TileStream(t.id, t.baseUrl, t.url, t.height));
    } else {
      s.setHeight(t.height, t.url);
    }
  }
  for (const [id, s] of streams) {
    if (!wanted.has(id)) {
      s.close();
      streams.delete(id);
    }
  }
}

function resize(w: number, h: number, d: number): void {
  dpr = d;
  if (!canvas) return;
  const pw = Math.max(1, Math.round(w * d));
  const ph = Math.max(1, Math.round(h * d));
  if (canvas.width !== pw || canvas.height !== ph) {
    canvas.width = pw;
    canvas.height = ph;
  }
  dirty = true;
  needsClear = true;
}

// ---- drawing -----------------------------------------------------------------

function devBox(b: Box): [number, number, number, number] {
  const x = Math.round(b.x * dpr);
  const y = Math.round(b.y * dpr);
  return [x, y, Math.round((b.x + b.w) * dpr) - x, Math.round((b.y + b.h) * dpr) - y];
}

/** Device-pixel box of a stream's inner rectangle placed in the image by its grid position. */
function gridBox(image: Box, g: Grid, w: number, h: number): [number, number, number, number] {
  const x0 = Math.round((image.x + (g.x0 / g.fullWidth) * image.w) * dpr);
  const y0 = Math.round((image.y + (g.y0 / g.fullHeight) * image.h) * dpr);
  const x1 = Math.round((image.x + ((g.x0 + w) / g.fullWidth) * image.w) * dpr);
  const y1 = Math.round((image.y + ((g.y0 + h) / g.fullHeight) * image.h) * dpr);
  return [x0, y0, x1 - x0, y1 - y0];
}

function drawDark(t: DrawTile): void {
  if (!ctx || !t.box) return;
  const [x, y, w, h] = devBox(t.box);
  ctx.fillStyle = plan.darkFill;
  ctx.fillRect(x, y, w, h);
  if (w <= 2 || h <= 2) return;
  ctx.strokeStyle = t.outline;
  ctx.lineWidth = Math.max(1, Math.round(dpr));
  const inset = ctx.lineWidth / 2;
  ctx.strokeRect(x + inset, y + inset, w - ctx.lineWidth, h - ctx.lineWidth);
}

function draw(): void {
  if (!ctx || !canvas) return;
  const t0 = performance.now();
  if (lastFrameAt !== null) {
    rafMs.push(t0 - lastFrameAt);
    if (rafMs.length > 240) rafMs.shift();
  }
  lastFrameAt = t0;

  const drawable = plan.tiles.filter((t) => t.box !== null && !t.dark && streams.has(t.id));
  const buffers = drawable.map((t) => ({ id: t.id, ts: streams.get(t.id)!.frames.map((h) => h.t) }));
  const choice = chooseCommonT(buffers, prevT);
  lastChoice = choice;
  if (choice.T !== null) prevT = choice.T;

  // Frames change at the renderers' rate, not the display's: draw only when
  // what would be drawn changed.
  let sig = String(choice.T);
  for (const [id, sh] of choice.show) sig += `|${id}:${sh.kind}${"t" in sh ? sh.t : ""}`;
  if (!dirty && sig === lastSig && captures.length === 0) return;
  lastSig = sig;
  dirty = false;

  ctx.globalAlpha = 1;
  if (needsClear) {
    ctx.fillStyle = settings.background;
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    needsClear = false;
  }
  ctx.imageSmoothingEnabled = true;
  const at = now();
  for (const t of plan.tiles) {
    if (!t.box) continue;
    const s = streams.get(t.id);
    const show = choice.show.get(t.id);
    if (t.dark || !s || !show || show.kind === "waiting" || show.kind === "empty") {
      if (s) s.appearedAt = null;
      drawDark(t);
      continue;
    }
    const frame = s.frameAt(show.t);
    if (!frame) {
      drawDark(t);
      continue;
    }
    const inner = innerRect({ width: frame.frame.displayWidth, height: frame.frame.displayHeight, crop: frame.crop });
    // In the mosaic the inner rectangle goes where the renderer says it sits in
    // the whole image (x0, y0 of full_width x full_height); opened, it fills its box.
    const [x, y, w, h] = plan.image && frame.grid ? gridBox(plan.image, frame.grid, inner.sw, inner.sh) : devBox(t.box);
    if (s.appearedAt === null) s.appearedAt = at;
    const fade = Math.min(1, (at - s.appearedAt) / settings.fadeInMs);
    if (fade < 1) {
      drawDark(t);
      dirty = true;
    }
    ctx.globalAlpha = fade;
    // Only the inner rectangle shows: the whole frame is drawn scaled so its
    // inner rectangle covers the tile's box, clipped to the box. (WebKit ignores
    // a VideoFrame's source rectangle and visibleRect when drawing.)
    const kx = w / inner.sw;
    const ky = h / inner.sh;
    if (inner.sx === 0 && inner.sy === 0 && inner.sw === frame.frame.displayWidth && inner.sh === frame.frame.displayHeight) {
      ctx.drawImage(frame.frame, x, y, w, h);
    } else {
      ctx.save();
      ctx.beginPath();
      ctx.rect(x, y, w, h);
      ctx.clip();
      ctx.drawImage(frame.frame, x - inner.sx * kx, y - inner.sy * ky, frame.frame.displayWidth * kx, frame.frame.displayHeight * ky);
      ctx.restore();
    }
    ctx.globalAlpha = 1;
    if (show.kind === "behind") {
      ctx.fillStyle = `rgba(0,0,0,${settings.dimAlpha})`;
      ctx.fillRect(x, y, w, h);
    }
  }
  for (const t of plan.tiles) {
    const s = streams.get(t.id);
    if (!s) continue;
    const keep = releaseBefore(choice, t.id);
    if (keep !== null) s.releaseOlder(keep);
  }

  // Captures read back exactly what this draw put on the canvas.
  if (captures.length > 0) {
    const shown: Record<string, string> = {};
    for (const [id, sh] of choice.show) shown[id] = sh.kind === "sync" || sh.kind === "behind" ? `${sh.kind}@${sh.t}` : sh.kind;
    for (const t of plan.tiles) if (t.dark) shown[t.id] = "dark";
    for (const c of captures) {
      const pixels = c.points.map((p) => {
        const d = ctx!.getImageData(Math.floor(p.x * dpr), Math.floor(p.y * dpr), 1, 1).data;
        return [d[0]!, d[1]!, d[2]!, d[3]!] as [number, number, number, number];
      });
      const result: CaptureResult = { req: c.req, T: choice.T, shown, pixels };
      post({ type: "capture", result });
    }
    captures = [];
  }

  if (choice.T !== null) {
    for (const w of watches) {
      if (!reported.has(w.id) && choice.T >= w.tEff - SAME_T_EPS) {
        reported.add(w.id);
        post({ type: "camera-drawn", id: w.id, drawnAt: at });
      }
    }
  }

  const spent = performance.now() - t0;
  drawMs.push(spent);
  if (drawMs.length > 240) drawMs.shift();
  drawTimes.push(at);
}

function pct(xs: readonly number[], p: number): number {
  if (xs.length === 0) return 0;
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor(p * s.length))]!;
}

function report(): void {
  const at = now();
  while (drawTimes.length > 0 && drawTimes[0]! < at - 2000) drawTimes.shift();
  const tiles: Record<string, TileFrameStats> = {};
  let decoded = 0;
  let bytes = 0;
  const decodeAll: number[] = [];
  for (const t of plan.tiles) {
    const s = streams.get(t.id);
    if (!s) continue;
    const fps = s.fps(at);
    decoded += fps;
    bytes += s.bytes;
    decodeAll.push(...s.decodeMs.slice(-20));
    const show = lastChoice.show.get(t.id);
    tiles[t.id] = {
      connected: s.ws !== null && s.ws.readyState === WebSocket.OPEN,
      fps,
      newestT: s.frames.length > 0 ? s.frames[s.frames.length - 1]!.t : null,
      width: s.lastW,
      height: s.lastH,
      behind: show?.kind === "behind",
      decodeMs: s.decodeMs.length > 0 ? pct(s.decodeMs, 0.5) : null,
    };
  }
  const stats: FrameStats = {
    T: lastChoice.T,
    tiles,
    drawMsP50: pct(drawMs, 0.5),
    drawMsP95: pct(drawMs, 0.95),
    rafMsP50: pct(rafMs, 0.5),
    rafMsP95: pct(rafMs, 0.95),
  };
  const kinds = [...lastChoice.show.values()];
  const bench: BenchStats = {
    decodeMsP50: pct(decodeAll, 0.5),
    decodeMsP95: pct(decodeAll, 0.95),
    decodedPerSec: decoded,
    bytesReceived: bytes,
    drawsPerSec: drawTimes.length / 2,
    tilesSynced: kinds.filter((k) => k.kind === "sync").length,
    tilesBehind: kinds.filter((k) => k.kind === "behind").length,
    tilesDrawn: plan.tiles.filter((t) => t.box !== null).length,
  };
  post({ type: "stats", stats, bench });
}

function loop(): void {
  draw();
  if (!mainTicks) self.requestAnimationFrame(loop);
}

self.onmessage = (e: MessageEvent<ToWorker>) => {
  const m = e.data;
  switch (m.type) {
    case "init":
      canvas = m.canvas;
      ctx = canvas.getContext("2d", { alpha: false, desynchronized: true }) as OffscreenCanvasRenderingContext2D | null;
      if (!ctx) {
        post({ type: "error", message: "no 2D context on the offscreen canvas" });
        return;
      }
      resize(m.width, m.height, m.dpr);
      mainTicks = m.mainTicks || typeof self.requestAnimationFrame !== "function";
      if (mainTicks) post({ type: "need-ticks" });
      else self.requestAnimationFrame(loop);
      setInterval(report, 500);
      break;
    case "resize":
      resize(m.width, m.height, m.dpr);
      break;
    case "plan":
      settings = m.settings;
      applyPlan(m.plan);
      break;
    case "watch":
      watches = m.targets;
      for (const id of [...reported]) if (!watches.some((w) => w.id === id)) reported.delete(id);
      break;
    case "tick":
      if (mainTicks) draw();
      break;
      case "capture":
      captures.push({ req: m.req, points: m.points });
      break;
  }
};
