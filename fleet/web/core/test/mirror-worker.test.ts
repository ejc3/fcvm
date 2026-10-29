// The GPU mirror's worker (app/_wall/mirror/mirror.worker.ts) in a stand-in worker scope: a
// fake WebSocket the test drives and a recording stand-in for WebGL2. What a connection the
// worker retired does after it was retired, the bound on bytes received and not yet decoded,
// an OVERFLOW record, a browser without WebGL2, and a stream from a streamer that sends raw
// deflate.

import { constants, createZstdCompress, deflateRawSync, type ZstdCompress } from "node:zlib";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { encodeCall, encodeRecord, RECORD } from "../mirror/wire.ts";
import type { MirrorIn, MirrorOut } from "../../app/_wall/mirror/mirror-msgs.ts";

class FakeSocket {
  static all: FakeSocket[] = [];
  binaryType = "blob";
  readyState = 0;
  sent: string[] = [];
  closed = false;
  onopen: (() => void) | null = null;
  onmessage: ((e: { data: unknown }) => void) | null = null;
  onclose: ((e: { code: number; reason: string }) => void) | null = null;
  readonly url: string;
  constructor(url: string) {
    this.url = url;
    FakeSocket.all.push(this);
  }
  send(s: string): void {
    this.sent.push(s);
  }
  close(): void {
    this.closed = true;
    this.readyState = 3;
  }
  /** What the server sends, delivered as the browser would. */
  message(bytes: Uint8Array): void {
    this.onmessage?.({ data: bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) });
  }
}
(FakeSocket as unknown as { OPEN: number }).OPEN = 1;

function fakeGl(): WebGL2RenderingContext {
  let n = 0;
  return new Proxy(
    {},
    {
      get(_t, prop: string) {
        if (prop === "canvas") return { width: 720, height: 1280 };
        if (prop === "drawingBufferWidth") return 720;
        if (prop === "drawingBufferHeight") return 1280;
        if (/^[A-Z_0-9]+$/.test(prop)) return 0;
        return () => (prop.startsWith("create") ? { obj: prop, id: ++n } : prop === "getParameter" ? null : undefined);
      },
    },
  ) as unknown as WebGL2RenderingContext;
}

let posted: MirrorOut[] = [];
let current: ((m: MirrorIn) => void) | null = null;
/** Each worker's stats timer, cleared after its test so no worker outlives it. */
const realSetInterval = globalThis.setInterval;
const timers: ReturnType<typeof setInterval>[] = [];
let scope: { postMessage: (m: MirrorOut) => void; onmessage: ((e: { data: MirrorIn }) => void) | null };

/** A fresh worker module in a fresh scope; `gl` false is a browser without WebGL2. */
async function worker(gl = true): Promise<(m: MirrorIn) => void> {
  vi.resetModules();
  posted = [];
  scope = { postMessage: (m) => posted.push(m), onmessage: null };
  vi.stubGlobal("self", scope);
  vi.stubGlobal("WebSocket", FakeSocket);
  vi.stubGlobal("setInterval", (f: () => void, ms: number) => {
    const t = realSetInterval(f, ms);
    timers.push(t);
    return t;
  });
  await import("../../app/_wall/mirror/mirror.worker.ts");
  const send = (m: MirrorIn) => scope.onmessage!({ data: m });
  send({ type: "canvas", canvas: { getContext: () => (gl ? fakeGl() : null) } as unknown as OffscreenCanvas });
  current = send;
  return send;
}

const settle = () => new Promise((r) => setTimeout(r, 20));
const closes = () => posted.filter((m) => m.type === "closed") as Extract<MirrorOut, { type: "closed" }>[];
const joins = () => posted.filter((m) => m.type === "joined").length;

/** A zstd stream as the streamer sends it: each message flushed. */
function streamer(): { z: ZstdCompress; msg: (bytes: Uint8Array) => Promise<Uint8Array> } {
  const z = createZstdCompress({ params: { [constants.ZSTD_c_compressionLevel]: 3, [constants.ZSTD_c_windowLog]: 20 } });
  const msg = (bytes: Uint8Array) =>
    new Promise<Uint8Array>((resolve) => {
      const out: Buffer[] = [];
      const take = (b: Buffer) => out.push(b);
      z.on("data", take);
      z.write(bytes, () =>
        z.flush(constants.ZSTD_e_flush, () => {
          z.off("data", take);
          resolve(new Uint8Array(Buffer.concat(out)));
        }),
      );
    });
  return { z, msg };
}

const hello = () => encodeRecord(RECORD.hello, 0, new Uint8Array(4), 1);
const opening = () =>
  Buffer.concat([hello(), encodeRecord(RECORD.open, 1, new Uint8Array(0)), encodeRecord(RECORD.data, 1, encodeCall("glViewport", [0, 0, 720, 1280]))]);

describe("the mirror worker", () => {
  beforeEach(() => {
    FakeSocket.all = [];
  });
  afterEach(() => {
    // a worker left with a connection would keep decoding into the next test's scope
    current?.({ type: "close" });
    current = null;
    for (const t of timers.splice(0)) clearInterval(t);
    vi.unstubAllGlobals();
  });

  it("says a browser without WebGL2 is unavailable, and opens nothing", async () => {
    const send = await worker(false);
    expect(posted).toEqual([{ type: "unavailable", reason: "this browser has no WebGL2" }]);
    send({ type: "open", url: "wss://phone/mirror", admit: "t" });
    expect(FakeSocket.all).toHaveLength(0);
  });

  it("decodes the streamer's zstd and joins, admitted with the viewer's token", async () => {
    const send = await worker();
    expect(posted).toEqual([{ type: "ready" }]);
    send({ type: "open", url: "wss://phone/mirror", admit: "tok" });
    const ws = FakeSocket.all[0]!;
    ws.onopen!();
    expect(JSON.parse(ws.sent[0]!)).toEqual({ admit: "tok" });
    const { z, msg } = streamer();
    ws.message(await msg(opening()));
    await settle();
    expect(joins()).toBe(1);
    expect(closes()).toEqual([]);
    z.close();
  });

  it("replays nothing a retired connection received, and reports nothing it says afterwards", async () => {
    const send = await worker();
    send({ type: "open", url: "wss://phone/mirror", admit: "a" });
    const a = FakeSocket.all[0]!;
    const sa = streamer();
    a.message(await sa.msg(opening()));
    await settle();
    expect(joins()).toBe(1);
    // a's next message is queued for a's decoder, which is in the middle of a's stream, when the
    // page opens b, and b's first message arrives before a's decoder runs again
    const later = await sa.msg(encodeRecord(RECORD.data, 1, encodeCall("glViewport", [0, 0, 1, 1])));
    const sb = streamer();
    const bFirst = await sb.msg(opening());
    a.message(later);
    send({ type: "open", url: "wss://phone/mirror", admit: "b" });
    const b = FakeSocket.all[1]!;
    b.message(bFirst);
    expect(a.closed).toBe(true);
    await settle();
    // b's stream was decoded by b's decoder alone: it joined, and nothing failed
    expect(joins()).toBe(2);
    expect(closes()).toEqual([]);
    // whatever a's socket does afterwards reaches nothing (its handlers were let go)
    expect(a.onmessage).toBeNull();
    expect(a.onclose).toBeNull();
    sa.z.close();
    sb.z.close();
  });

  it("ends the stream when more than 16 MiB arrived and was not decoded yet, so the page joins again", async () => {
    const send = await worker();
    send({ type: "open", url: "wss://phone/mirror", admit: "t" });
    const ws = FakeSocket.all[0]!;
    // incompressible bytes, so each message is about 1 MiB on the wire; all arrive before the
    // decoder has run once
    const { z, msg } = streamer();
    const noise = new Uint8Array(1 << 20).map(() => Math.floor(Math.random() * 256));
    const m = await msg(noise);
    expect(m.length).toBeGreaterThan(1 << 20);
    for (let i = 0; i < 17; i++) ws.message(m);
    expect(closes()).toEqual([{ type: "closed", code: 4000, reason: "the mirror fell behind in this browser: join again" }]);
    expect(ws.closed).toBe(true);
    z.close();
  });

  it("ends the stream on an OVERFLOW record (the phone's copy stopped), so the page joins again", async () => {
    const send = await worker();
    send({ type: "open", url: "wss://phone/mirror", admit: "t" });
    const ws = FakeSocket.all[0]!;
    const { z, msg } = streamer();
    ws.message(await msg(Buffer.concat([opening(), encodeRecord(RECORD.overflow, 1, new Uint8Array(0), 1 << 20)])));
    await settle();
    expect(closes()).toEqual([{ type: "closed", code: 4000, reason: "the phone's copy of its commands overflowed: join again" }]);
    expect(ws.closed).toBe(true);
    z.close();
  });

  it("leaves no earlier test's worker posting after its test", async () => {
    // a fresh scope with no worker in it: the earlier workers' 500 ms stats timers must be gone
    posted = [];
    scope = { postMessage: (m) => posted.push(m), onmessage: null };
    vi.stubGlobal("self", scope);
    await new Promise((r) => setTimeout(r, 600));
    expect(posted).toEqual([]);
  });

  it("reads raw deflate from a streamer built before zstd", async () => {
    const send = await worker();
    send({ type: "open", url: "wss://phone/mirror", admit: "t" });
    const ws = FakeSocket.all[0]!;
    ws.message(new Uint8Array(deflateRawSync(opening(), { finishFlush: constants.Z_SYNC_FLUSH })));
    await settle();
    expect(joins()).toBe(1);
    const stats = posted.filter((m) => m.type === "stats");
    expect(closes()).toEqual([]);
    expect(stats.every((s) => s.type !== "stats" || s.codec !== "zstd")).toBe(true);
  });
});
