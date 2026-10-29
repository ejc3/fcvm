/// <reference lib="webworker" />
// The GPU mirror's worker: the phone streamer's GET /mirror stream (or, for the replay test, a
// captured tap stream) decoded and drawn on the canvas the page handed over, off the main
// thread. Messages are in mirror-msgs.ts.

import { admitMessage } from "../../../core/net.ts";
import { RateMeter } from "../../../core/meter.ts";
import { RECORD, RecordReader } from "../../../core/mirror/wire.ts";
import { GlMirror } from "./gl.ts";
import type { MirrorIn, MirrorOut } from "./mirror-msgs.ts";

const now = () => performance.timeOrigin + performance.now();
const post = (m: MirrorOut) => (self as unknown as DedicatedWorkerGlobalScope).postMessage(m);

let gl: WebGL2RenderingContext | null = null;
let mirror: GlMirror | null = null;
let ws: WebSocket | null = null;
let reader = new RecordReader();
let inflate: WritableStreamDefaultWriter<Uint8Array> | null = null;
const wire = new RateMeter();
let tapBytes = 0;
let joined = false;
let timer: ReturnType<typeof setInterval> | null = null;
let pixel: number[] | null = null;

function fresh(): void {
  if (!gl) return;
  mirror = new GlMirror(gl, now);
  reader = new RecordReader();
  wire.reset();
  tapBytes = 0;
  joined = false;
  pixel = null;
  // the canvas is preserved between posts, so its centre can be read after one
  mirror.onPost = () => {
    if (!gl || (mirror!.stats.posts % 15 !== 1)) return;
    const px = new Uint8Array(4);
    gl.readPixels(gl.drawingBufferWidth >> 1, gl.drawingBufferHeight >> 1, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, px);
    pixel = Array.from(px);
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
  }
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
    repliesPerSec: mirror.replyRate.perSecond(t),
    fps: mirror.postRate.perSecond(t),
    posts: mirror.stats.posts,
    calls: mirror.stats.calls,
    skipped: Object.values(mirror.stats.skipped).reduce((a, b) => a + b, 0),
    failed: Object.values(mirror.stats.failed).reduce((a, b) => a + b, 0),
    unknownChannelPackets: mirror.stats.unknownChannelPackets,
    overflowed: mirror.stats.overflowed,
    open: ws !== null && ws.readyState === WebSocket.OPEN,
    pixel,
  });
}

function close(): void {
  if (ws) {
    ws.onclose = null;
    ws.close();
    ws = null;
  }
  void inflate?.close().catch(() => {});
  inflate = null;
}

function open(url: string, admit: string): void {
  close();
  fresh();
  // one raw deflate stream for the connection: each message is flushed to a byte boundary
  const ds = new DecompressionStream("deflate-raw");
  inflate = ds.writable.getWriter() as WritableStreamDefaultWriter<Uint8Array>;
  void (async () => {
    const r = ds.readable.getReader();
    for (;;) {
      const { value, done } = await r.read();
      if (done) break;
      feed(value);
    }
  })().catch((e: unknown) => post({ type: "closed", code: 0, reason: `inflate: ${String(e)}` }));
  const s = new WebSocket(url);
  s.binaryType = "arraybuffer";
  ws = s;
  s.onopen = () => s.send(admitMessage(admit));
  s.onmessage = (e) => {
    if (!(e.data instanceof ArrayBuffer)) return;
    wire.add(now(), e.data.byteLength);
    void inflate?.write(new Uint8Array(e.data));
  };
  s.onclose = (e) => {
    ws = null;
    post({ type: "closed", code: e.code, reason: e.reason });
  };
}

self.onmessage = (e: MessageEvent<MirrorIn>) => {
  const m = e.data;
  if (m.type === "canvas") {
    gl = m.canvas.getContext("webgl2", { antialias: false, alpha: false, preserveDrawingBuffer: true }) as WebGL2RenderingContext | null;
    if (!gl) post({ type: "closed", code: 0, reason: "this browser has no WebGL2" });
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
