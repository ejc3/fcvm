/// <reference lib="webworker" />
// The GPU mirror's worker: the phone streamer's GET /mirror stream (or, for the replay test, a
// captured tap stream) decoded and drawn on the canvas the page handed over, off the main
// thread. Messages are in mirror-msgs.ts.

import { admitMessage } from "../../../core/net.ts";
import { RateMeter } from "../../../core/meter.ts";
import { JOIN_PART, RECORD, RecordReader } from "../../../core/mirror/wire.ts";
import { GlMirror } from "./gl.ts";
import type { MirrorIn, MirrorOut } from "./mirror-msgs.ts";
import { ZstdStream } from "./zstd.ts";

const now = () => performance.timeOrigin + performance.now();
const post = (m: MirrorOut) => (self as unknown as DedicatedWorkerGlobalScope).postMessage(m);

let gl: WebGL2RenderingContext | null = null;
let mirror: GlMirror | null = null;
let ws: WebSocket | null = null;
/** Bumped by every open and close: a retired connection's late callbacks are ignored. */
let generation = 0;
/** Compressed bytes received and not yet decoded, at most MAX_PENDING_BYTES. */
let pendingBytes = 0;
/** A viewer that receives faster than it replays drops the stream past this, and joins again. */
const MAX_PENDING_BYTES = 16 << 20;
/**
 * Until the join snapshot has been fed (every color buffer's pixels, tens of MB deflated), the
 * bound is this one: the streamer queues the snapshot apart from its live bound too.
 */
const MAX_JOIN_PENDING_BYTES = 256 << 20;
/** The join snapshot has been fed (its END), or the stream has none: the live bound applies. */
let joinDone = false;
let reader = new RecordReader();
let zstd: Promise<ZstdStream> | null = null;
/** zstd messages waiting for the decoder, in order. */
let queue: Uint8Array[] = [];
let draining = false;
/** Milliseconds spent decoding zstd, as a rate: the share of a core the wire costs this browser. */
const decodeMs = new RateMeter();
const wire = new RateMeter();
let tapBytes = 0;
let joined = false;
let timer: ReturnType<typeof setInterval> | null = null;
let pixel: number[] | null = null;
let stampAt: { k: number; at: number } | null = null;

/** The app's frame number in one row of the stamp band (fleet/phone/streamer/stamp.cpp), or null. */
function stampOf(row: Uint8Array, w: number): number | null {
  let v = 0;
  for (let i = 0; i < 32; i++) {
    const x = Math.floor((Math.floor((i * w) / 32) + Math.floor(((i + 1) * w) / 32)) / 2) * 4;
    const lo = Math.min(row[x]!, row[x + 1]!, row[x + 2]!);
    const hi = Math.max(row[x]!, row[x + 1]!, row[x + 2]!);
    if (lo >= 160) v = v * 2 + 1;
    else if (hi <= 96) v = v * 2;
    else return null;
  }
  return v === 0 || v === 0xffffffff ? null : v;
}

function fresh(): void {
  if (!gl) return;
  mirror = new GlMirror(gl, now);
  reader = new RecordReader();
  wire.reset();
  tapBytes = 0;
  decodeMs.reset();
  joined = false;
  joinDone = false;
  pixel = null;
  stampAt = null;
  // the canvas is preserved between posts, so its centre can be read after one
  mirror.onPost = () => {
    if (!gl || (mirror!.stats.posts % 15 !== 1)) return;
    const px = new Uint8Array(4);
    gl.readPixels(gl.drawingBufferWidth >> 1, gl.drawingBufferHeight >> 1, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, px);
    pixel = Array.from(px);
    // Robot Park's stamp (its frame number) in the bottom band: the scene time the mirror shows,
    // against the video's, says which is fresher
    const w = gl.drawingBufferWidth;
    const row = new Uint8Array(w * 4);
    gl.readPixels(0, 3, w, 1, gl.RGBA, gl.UNSIGNED_BYTE, row);
    const k = stampOf(row, w);
    if (k !== null) stampAt = { k, at: now() };
  };
}

function feed(bytes: Uint8Array): void {
  tapBytes += bytes.length;
  for (const r of reader.push(bytes)) {
    if (r.type === RECORD.hello && !joined) {
      joined = true;
      post({ type: "joined" });
    }
    mirror?.feed(r);
    if (r.type !== RECORD.hello && (r.type !== RECORD.join || r.aux === JOIN_PART.end)) joinDone = true;
    // the phone's ring filled: the copy stopped and the stream has a gap; join again
    if (r.type === RECORD.overflow && ws) {
      retire("the phone's copy of its commands overflowed: join again");
      return;
    }
  }
}

/** Ends the open connection (if any) as the worker's own decision, and says why. */
function retire(reason: string): void {
  close();
  post({ type: "closed", code: 4000, reason });
}

function stats(): void {
  if (!mirror) return;
  const t = now();
  post({
    type: "stats",
    at: t,
    wireBps: wire.perSecond(t),
    wireBytes: wire.total,
    tapBytes,
    decodeMsPerSec: decodeMs.perSecond(t),
    repliesPerSec: mirror.replyRate.perSecond(t),
    fps: mirror.postRate.perSecond(t),
    posts: mirror.stats.posts,
    calls: mirror.stats.calls,
    skipped: Object.values(mirror.stats.skipped).reduce((a, b) => a + b, 0),
    failed: Object.values(mirror.stats.failed).reduce((a, b) => a + b, 0),
    unknownChannelPackets: mirror.stats.unknownChannelPackets,
    overflowed: mirror.stats.overflowed,
    join: mirror.stats.join,
    open: ws !== null && ws.readyState === WebSocket.OPEN,
    pixel,
    stamp: stampAt,
    width: gl?.drawingBufferWidth ?? 0,
    height: gl?.drawingBufferHeight ?? 0,
  });
}

function close(): void {
  generation++;
  if (ws) {
    ws.onclose = null;
    ws.onmessage = null;
    ws.onopen = null;
    ws.close();
    ws = null;
  }
  zstd = null;
  queue = [];
  draining = false;
  pendingBytes = 0;
}

/**
 * Decodes the queued zstd messages in order and replays them, yielding to the event loop
 * every 50 ms so that messages arriving meanwhile are counted against MAX_PENDING_BYTES.
 */
async function drain(gen: number): Promise<void> {
  if (draining) return;
  draining = true;
  try {
    const z = await zstd!;
    let slice = performance.now();
    while (gen === generation && queue.length > 0) {
      const m = queue.shift()!;
      const t0 = performance.now();
      const bytes = z.push(m);
      const t1 = performance.now();
      decodeMs.add(now(), t1 - t0);
      pendingBytes -= m.length;
      feed(bytes);
      if (performance.now() - slice > 50) {
        await new Promise((r) => setTimeout(r, 0));
        slice = performance.now();
      }
    }
  } catch (e: unknown) {
    if (gen === generation) retire(`zstd: ${String(e)}`);
  } finally {
    if (gen === generation) draining = false;
  }
}

function open(url: string, admit: string): void {
  close();
  fresh();
  const gen = generation;
  const s = new WebSocket(url);
  s.binaryType = "arraybuffer";
  ws = s;
  s.onopen = () => s.send(admitMessage(admit));
  s.onmessage = (e) => {
    if (gen !== generation || !(e.data instanceof ArrayBuffer)) return;
    wire.add(now(), e.data.byteLength);
    const n = e.data.byteLength;
    if (pendingBytes + n > (joinDone ? MAX_PENDING_BYTES : MAX_JOIN_PENDING_BYTES)) {
      retire("the mirror fell behind in this browser: join again");
      return;
    }
    zstd ??= ZstdStream.create();
    pendingBytes += n;
    queue.push(new Uint8Array(e.data));
    void drain(gen);
  };
  s.onclose = (e) => {
    if (gen !== generation) return;
    ws = null;
    post({ type: "closed", code: e.code, reason: e.reason });
  };
}

self.onmessage = (e: MessageEvent<MirrorIn>) => {
  const m = e.data;
  if (m.type === "canvas") {
    gl = m.canvas.getContext("webgl2", { antialias: false, alpha: false, preserveDrawingBuffer: true }) as WebGL2RenderingContext | null;
    post(gl ? { type: "ready" } : { type: "unavailable", reason: "this browser has no WebGL2" });
    timer ??= setInterval(stats, 500);
  } else if (m.type === "open") {
    if (gl) open(m.url, m.admit);
  } else if (m.type === "close") {
    close();
  } else if (m.type === "replay") {
    // the replay test: a captured tap stream, uncompressed, fed as it would arrive
    fresh();
    feed(new Uint8Array(m.bytes));
    stats();
  }
};
