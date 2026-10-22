/// <reference lib="webworker" />
// The frame worker: one WebSocket and one WebCodecs VideoDecoder per visible
// tile, a short buffer of decoded frames per tile, and one OffscreenCanvas that
// draws every tile at one common t. The rules (which t, which frame, when to
// reconnect, which height) come from core/; this file is the platform glue.

import {
  BORROW_FADE_MS,
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
  type DrawSource,
  type DrawTile,
  type StreamMode,
  type FrameStats,
  type SyncChoice,
  type TileFrameStats,
} from "../../core/index.ts";
import type { BenchStats, CaptureResult, DrawSettings, FromWorker, ToWorker, TraceEntry } from "./worker-msgs.ts";

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
let plan: DrawPlan = {
  mode: "direct",
  direct: [],
  aggregated: [],
  tiles: [],
  focus: null,
  image: null,
  dpr: 1,
  darkFill: "#06080B",
  estimatedBps: 0,
  poster: null,
  paused: false,
  darkFadeMs: 0,
  suspended: false,
};
/** The mode being drawn. It becomes plan.mode only once that mode can be drawn whole. */
let shownMode: StreamMode = "direct";
/** When each tile last stopped being dark, for its fade back in. */
const litAt = new Map<string, number>();
const wasDark = new Set<string>();
/** When each tile went dark, for its fade out when the viewer asked for less motion. */
const darkAt = new Map<string, number>();
/** Draws recorded for a test: the t drawn, the mode, regions drawn without a frame, tiles fading dark. */
let trace: TraceEntry[] | null = null;
/** The coordinator's poster, drawn where no stream has shown a frame yet. */
let poster: ImageBitmap | null = null;
/** The last picture drawn before the streams closed (a hidden tab), shown until they are back. */
let held: ImageBitmap | null = null;
/** Streams reopened after a suspension; the picture holds until every region has a frame at one t. */
let resuming = false;
/** When the live picture started fading in over the held one. */
let heldFadeFrom: number | null = null;
let resumeStarted = 0;
let posterUrl: string | null = null;
/** Every stream has shown a frame: the poster is no longer drawn. */
let posterRetired = false;
let painted = false;
/** When the current switch began; after SWITCH_HOLD_LIMIT_MS the image stops waiting for it. */
let switchStarted: number | null = null;
const SWITCH_HOLD_LIMIT_MS = 4000;
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

function sourcesOf(mode: StreamMode): DrawSource[] {
  return mode === "direct" ? plan.direct : plan.aggregated;
}

/** The streams to hold: the mode drawn, the mode wanted (while switching), and an opened tile. */
function wantedSources(): Map<string, DrawSource> {
  const out = new Map<string, DrawSource>();
  if (plan.suspended) return out;
  for (const src of sourcesOf(shownMode)) out.set(src.id, src);
  if (plan.mode !== shownMode) for (const src of sourcesOf(plan.mode)) out.set(src.id, src);
  if (plan.focus) out.set(plan.focus.id, plan.focus);
  return out;
}

function syncStreams(): void {
  const wanted = wantedSources();
  for (const src of wanted.values()) {
    if (src.url === null || src.baseUrl === null) continue;
    const st = streams.get(src.id);
    if (!st || st.baseUrl !== src.baseUrl) {
      st?.close();
      streams.set(src.id, new TileStream(src.id, src.baseUrl, src.url, src.height));
    } else {
      st.setHeight(src.height, src.url);
    }
  }
  for (const [id, st] of streams) {
    const w = wanted.get(id);
    if (!w || w.url === null) {
      st.close();
      streams.delete(id);
    }
  }
}

function loadPoster(url: string | null): void {
  if (url === posterUrl) return;
  posterUrl = url;
  if (url === null) return;
  void (async () => {
    try {
      const res = await fetch(url, { cache: "no-store" });
      if (!res.ok || posterUrl !== url) return;
      const bmp = await createImageBitmap(await res.blob());
      if (posterUrl !== url) {
        bmp.close();
        return;
      }
      poster?.close();
      poster = bmp;
      dirty = true;
      needsClear = true;
    } catch {
      // No poster: the image starts dark and fills in as streams arrive.
    }
  })();
}

/** The poster, while it is still drawn under streams that have not arrived. */
function posterOn(): boolean {
  return poster !== null && !posterRetired && plan.image !== null && plan.focus === null;
}

/** Paint the part of the poster under a box. */
function drawPoster(b: Box): void {
  if (!ctx || !poster || !plan.image) return;
  const img = plan.image;
  const [x, y, w, h] = devBox(b);
  const sx = ((b.x - img.x) / img.w) * poster.width;
  const sy = ((b.y - img.y) / img.h) * poster.height;
  ctx.drawImage(poster, sx, sy, (b.w / img.w) * poster.width, (b.h / img.h) * poster.height, x, y, w, h);
}

/** What fills a region with no frame: the held picture while the live one fades in, the poster at first, dark after. */
function fillEmpty(b: Box): void {
  if (held && heldFadeFrom !== null && ctx) {
    const [x, y, w, h] = devBox(b);
    ctx.drawImage(held, x, y, w, h, x, y, w, h);
  } else if (posterOn()) drawPoster(b);
  else fillDark(b);
}

function applyPlan(p: DrawPlan): void {
  const was = plan.suspended;
  if (!was && p.suspended && canvas) {
    // Keep what is on screen: it stays up while hidden and when the tab comes back.
    held?.close();
    held = null;
    void createImageBitmap(canvas).then((b) => {
      if (plan.suspended) held = b;
      else b.close();
    });
  }
  if (was && !p.suspended) {
    resuming = true;
    resumeStarted = now();
    heldFadeFrom = null;
  }
  plan = p;
  loadPoster(p.poster);
  dirty = true;
  needsClear = true;
  // Before anything is on screen there is nothing to keep: start in the wanted mode.
  if (prevT === null && plan.mode !== shownMode) {
    shownMode = plan.mode;
    post({ type: "mode-shown", mode: shownMode });
  }
  syncStreams();
}

/**
 * Switch to the wanted mode once every one of its regions holds a decoded
 * frame at one t no older than the t on screen: then the next draw shows it
 * whole, at a t that does not go back, and the old streams close after it.
 */
/**
 * While switching: the newest t every incoming region holds. The image on
 * screen does not advance past it, so a later incoming stream (an aggregator
 * joins tiles before it encodes) can catch up with what is shown; the shown
 * streams keep their frames meanwhile. Null when nothing caps.
 */
function switchCap(): number {
  if (plan.mode === shownMode || plan.focus) return Infinity;
  const target = sourcesOf(plan.mode).filter((src) => src.box !== null && src.url !== null);
  if (target.length === 0 || !target.every((src) => (streams.get(src.id)?.frames.length ?? 0) > 0)) return Infinity;
  if (switchStarted !== null && now() - switchStarted > SWITCH_HOLD_LIMIT_MS) return Infinity;
  const newest = Math.min(...target.map((src) => { const f = streams.get(src.id)!.frames; return f[f.length - 1]!.t; }));
  // Never below what is on screen: the shown frames at prevT are still held.
  return prevT === null ? newest : Math.max(newest, prevT);
}

function trySwitch(): void {
  if (plan.mode === shownMode) {
    switchStarted = null;
    return;
  }
  switchStarted ??= now();
  const target = sourcesOf(plan.mode).filter((src) => src.box !== null && src.url !== null);
  if (target.length > 0) {
    if (!target.every((src) => streams.has(src.id))) return;
    const c = chooseCommonT(
      target.map((src) => ({ id: src.id, ts: streams.get(src.id)!.frames.map((h) => h.t) })),
      prevT,
    );
    if (c.T === null || (prevT !== null && c.T < prevT - SAME_T_EPS)) return;
    if (!target.every((src) => c.show.get(src.id)?.kind === "sync")) return;
  }
  shownMode = plan.mode;
  switchStarted = null;
  dirty = true;
  needsClear = true;
  post({ type: "mode-shown", mode: shownMode });
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

/** A region with no frame to show. */
function fillDark(b: Box): void {
  if (!ctx) return;
  const [x, y, w, h] = devBox(b);
  ctx.fillStyle = plan.darkFill;
  ctx.fillRect(x, y, w, h);
}

/** A dark tile, outlined in its half's colour, at an opacity (for its fade). */
function drawDark(t: DrawTile, alpha: number): void {
  if (!ctx || !t.box) return;
  const [x, y, w, h] = devBox(t.box);
  ctx.globalAlpha = alpha;
  ctx.fillStyle = plan.darkFill;
  ctx.fillRect(x, y, w, h);
  if (w > 2 && h > 2) {
    ctx.strokeStyle = t.outline;
    ctx.lineWidth = Math.max(1, Math.round(dpr));
    const inset = ctx.lineWidth / 2;
    ctx.strokeRect(x + inset, y + inset, w - ctx.lineWidth, h - ctx.lineWidth);
  }
  ctx.globalAlpha = 1;
}

function draw(): void {
  if (!ctx || !canvas) return;
  const t0 = performance.now();
  if (lastFrameAt !== null) {
    rafMs.push(t0 - lastFrameAt);
    if (rafMs.length > 240) rafMs.shift();
  }
  lastFrameAt = t0;

  // Suspended (a hidden tab): no streams, nothing new to draw; the canvas keeps its picture.
  if (plan.suspended) {
    answerCaptures(null);
    return;
  }
  // Paused: the picture holds what it shows; only a new plan or size redraws it.
  if (plan.paused && !dirty && captures.length === 0) return;
  if (!plan.paused) trySwitch();
  const shown = plan.focus ? [plan.focus] : sourcesOf(shownMode);
  const drawable = shown.filter((src) => src.box !== null && streams.has(src.id));
  const buffers = drawable.map((src) => ({ id: src.id, ts: streams.get(src.id)!.frames.map((h) => h.t) }));
  const choice = plan.paused ? lastChoice : chooseCommonT(buffers, prevT, undefined, switchCap());
  if (resuming) {
    // Back from hidden: the picture holds until every region has a frame at one
    // t (the mode switch's rule), then the live image fades in over it.
    const live = shown.filter((src) => src.box !== null && src.url !== null);
    // A region that never sends a frame stops holding the rest after the switch's limit.
    const ready =
      (choice.T !== null && live.every((src) => choice.show.get(src.id)?.kind === "sync")) || now() - resumeStarted > SWITCH_HOLD_LIMIT_MS;
    if (!ready) {
      if (held && (dirty || needsClear) && ctx && canvas) {
        ctx.drawImage(held, 0, 0, canvas.width, canvas.height);
        dirty = false;
        needsClear = false;
      }
      if (trace) trace.push({ T: prevT, mode: shownMode, missing: 0, darkIn: 0, poster: false, held: true });
      answerCaptures(prevT);
      return;
    }
    resuming = false;
    heldFadeFrom = now();
    for (const src of live) {
      const st = streams.get(src.id);
      if (st) st.appearedAt = null;
    }
    dirty = true;
  }
  lastChoice = choice;
  if (choice.T !== null) prevT = choice.T;

  // Frames change at the renderers' rate, not the display's: draw only when
  // what would be drawn changed.
  let sig = `${shownMode}|${String(choice.T)}`;
  for (const [id, sh] of choice.show) sig += `|${id}:${sh.kind}${"t" in sh ? sh.t : ""}`;
  if (!dirty && sig === lastSig && captures.length === 0) return;
  lastSig = sig;
  dirty = false;

  ctx.globalAlpha = 1;
  if (needsClear) {
    ctx.fillStyle = settings.background;
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    if (posterOn()) drawPoster(plan.image!);
    needsClear = false;
  }
  ctx.imageSmoothingEnabled = true;
  const at = now();
  let missing = 0;
  for (const src of shown) {
    if (!src.box) continue;
    const st = streams.get(src.id);
    const show = choice.show.get(src.id);
    const frame = st && show && (show.kind === "sync" || show.kind === "behind") ? st.frameAt(show.t) : null;
    if (!st || !show || !frame) {
      if (st) st.appearedAt = null;
      if (src.url !== null) missing++;
      fillEmpty(src.box);
      continue;
    }
    const inner = innerRect({ width: frame.frame.displayWidth, height: frame.frame.displayHeight, crop: frame.crop });
    // In the mosaic the inner rectangle goes where the stream says it sits in
    // the whole image (x0, y0 of full_width x full_height); opened, it fills its box.
    const [x, y, w, h] = plan.image && frame.grid && !plan.focus ? gridBox(plan.image, frame.grid, inner.sw, inner.sh) : devBox(src.box);
    if (st.appearedAt === null) st.appearedAt = at;
    const fade = Math.min(1, (at - st.appearedAt) / settings.fadeInMs);
    if (fade < 1) {
      fillEmpty(src.box);
      dirty = true;
    }
    ctx.globalAlpha = fade;
    // Only the inner rectangle shows: the whole frame is drawn scaled so its
    // inner rectangle covers the box, clipped to the box. (WebKit ignores a
    // VideoFrame's source rectangle and visibleRect when drawing.)
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
  // Each tile's own state, the same in both modes: dark while it relaunches
  // (outlined in its half's colour), fading out when it comes back.
  let darkIn = 0;
  if (!plan.focus) {
    for (const t of plan.tiles) {
      if (!t.box) continue;
      if (t.dark) {
        if (!wasDark.has(t.id)) {
          wasDark.add(t.id);
          // A tile still fading back in starts its dark-out from where it is.
          darkAt.set(t.id, at);
        }
        litAt.delete(t.id);
        const since = darkAt.get(t.id) ?? at;
        const fadeMs = Math.max(plan.darkFadeMs, t.slowFade ? BORROW_FADE_MS : 0);
        const alpha = fadeMs > 0 ? Math.min(1, (at - since) / fadeMs) : 1;
        if (alpha < 1) {
          darkIn++;
          dirty = true;
        }
        drawDark(t, alpha);
        continue;
      }
      darkAt.delete(t.id);
      if (wasDark.has(t.id)) {
        wasDark.delete(t.id);
        litAt.set(t.id, at);
      }
      const since = litAt.get(t.id);
      if (since === undefined) continue;
      const left = 1 - (at - since) / settings.fadeInMs;
      if (left <= 0) {
        litAt.delete(t.id);
        continue;
      }
      drawDark(t, left);
      dirty = true;
    }
  }
  for (const src of shown) {
    const st = streams.get(src.id);
    if (!st) continue;
    const keep = releaseBefore(choice, src.id);
    if (keep !== null) st.releaseOlder(keep);
  }
  if (trace) {
    trace.push({ T: choice.T, mode: shownMode, missing, darkIn, poster: posterOn(), held: heldFadeFrom !== null });
    if (trace.length > 20_000) trace.shift();
  }
  // The held picture goes once the live one has faded in over it.
  if (heldFadeFrom !== null && at - heldFadeFrom > settings.fadeInMs + 50) {
    heldFadeFrom = null;
    held?.close();
    held = null;
  }
  // The poster goes once every stream drawn has shown a frame.
  if (!posterRetired && poster && shown.length > 0 && shown.every((src) => src.box === null || src.url === null || streams.get(src.id)?.appearedAt != null)) {
    posterRetired = true;
    dirty = true;
  }
  if (!painted && (posterOn() || shown.some((src) => streams.get(src.id)?.appearedAt != null))) {
    painted = true;
    post({ type: "painted" });
  }

  // Captures read back exactly what this draw put on the canvas.
  if (captures.length > 0) {
    const shownBy: Record<string, string> = {};
    for (const [id, sh] of choice.show) shownBy[id] = sh.kind === "sync" || sh.kind === "behind" ? `${sh.kind}@${sh.t}` : sh.kind;
    for (const t of plan.tiles) if (t.dark) shownBy[t.id] = "dark";
    answerCaptures(choice.T, shownBy);
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

/** Answer pending captures from what the canvas holds now. */
function answerCaptures(T: number | null, shownBy: Record<string, string> = {}): void {
  if (captures.length === 0 || !ctx) return;
  for (const c of captures) {
    const pixels = c.points.map((p) => {
      const d = ctx!.getImageData(Math.floor(p.x * dpr), Math.floor(p.y * dpr), 1, 1).data;
      return [d[0]!, d[1]!, d[2]!, d[3]!] as [number, number, number, number];
    });
    const result: CaptureResult = { req: c.req, T, shown: shownBy, pixels, mode: shownMode };
    post({ type: "capture", result });
  }
  captures = [];
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
  for (const [id, s] of streams) {
    const fps = s.fps(at);
    decoded += fps;
    bytes += s.bytes;
    decodeAll.push(...s.decodeMs.slice(-20));
    const show = lastChoice.show.get(id);
    tiles[id] = {
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
    tilesDrawn: (plan.focus ? [plan.focus] : sourcesOf(shownMode)).filter((src) => src.box !== null).length,
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
    case "trace":
      if (m.on) {
        trace = [];
      } else {
        post({ type: "trace", req: m.req, entries: trace ?? [] });
        trace = null;
      }
      break;
      case "capture":
      captures.push({ req: m.req, points: m.points });
      break;
  }
};
