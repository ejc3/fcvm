// The mock phone streamer's GET /mirror (fleet/PROTOCOL.md, "Phone streamer"): a tap stream
// (gfxstream's mirror_tap.h records) compressed as the streamer compresses it. With
// FLEET_MOCK_MIRROR_TAP naming a captured stream it plays that capture at the pace it was
// recorded; otherwise it makes one: a window surface cleared to a colour that steps every
// frame, posted 30 times a second, with the reply a guest waits for on each flush.

import { readFileSync } from "node:fs";
import zlib from "node:zlib";
import type { WebSocket } from "ws";
import { encodeCall, encodeRecord, RECORD, RecordReader, type TapRecord } from "../core/mirror/wire.ts";

const u32 = (x: number) => {
  const b = new Uint8Array(4);
  new DataView(b.buffer).setUint32(0, x, true);
  return b;
};

/** The colour the synthetic phone clears to at frame k. */
export function mirrorMockColor(k: number): [number, number, number] {
  return [(k * 37) % 256, (k * 11 + 80) % 256, 200];
}

/** The synthetic stream's opening: the copy started, a channel, its color buffer, context and surface. */
function opening(): Uint8Array[] {
  const ch = 1;
  const d = (name: string, args: (number | bigint | Uint8Array)[]) => encodeRecord(RECORD.data, ch, encodeCall(name, args));
  const reply = (v: number) => encodeRecord(RECORD.reply, ch, u32(v), 4);
  return [
    encodeRecord(RECORD.hello, 0, u32(0), 1),
    encodeRecord(RECORD.open, ch, new Uint8Array(0)),
    d("rcCreateColorBuffer", [720, 1280, 0x1908]),
    reply(1),
    d("rcCreateContext", [1, 0, 3]),
    reply(1),
    d("rcCreateWindowSurface", [1, 720, 1280]),
    reply(1),
    d("rcSetWindowColorBuffer", [1, 1]),
    d("rcMakeCurrent", [1, 1, 1]),
    reply(1),
    d("glViewport", [0, 0, 720, 1280]),
  ];
}

function frame(k: number): Uint8Array[] {
  const [r, g, b] = mirrorMockColor(k);
  const ch = 1;
  const d = (name: string, args: (number | bigint | Uint8Array)[]) => encodeRecord(RECORD.data, ch, encodeCall(name, args));
  return [
    d("glClearColor", [r / 255, g / 255, b / 255, 1]),
    d("glClear", [0x4000]),
    d("rcFlushWindowColorBuffer", [1]),
    encodeRecord(RECORD.reply, ch, u32(0), 4),
    d("rcFBPost", [1]),
  ];
}

/**
 * Serves one mirror stream on `ws` until it closes. With `overflow`, the synthetic stream sends
 * an OVERFLOW record after ten frames and nothing after it, as a tap whose ring filled.
 */
export function serveMockMirror(ws: WebSocket, capturePath: string | undefined, overflow = false): void {
  // one zstd stream (level 3, 1 MiB window), flushed after each batch; the page decodes it as
  // one stream, so where its messages split does not matter
  const z = zlib.createZstdCompress({
    params: { [zlib.constants.ZSTD_c_compressionLevel]: 3, [zlib.constants.ZSTD_c_windowLog]: 20 },
  });
  z.on("data", (out: Buffer) => {
    if (ws.readyState === ws.OPEN) ws.send(out);
  });
  const send = (parts: Uint8Array[]) => {
    if (ws.readyState !== ws.OPEN) return;
    z.write(Buffer.concat(parts));
    z.flush(zlib.constants.ZSTD_e_flush);
  };
  let timer: NodeJS.Timeout | null = null;
  let admitted = false;
  ws.on("message", (data, isBinary) => {
    if (admitted || isBinary) return;
    try {
      const m = JSON.parse(String(data)) as { admit?: unknown };
      if (typeof m.admit !== "string") throw new Error("no admit");
    } catch {
      ws.close(1008, "the token of the viewer holding this phone is required");
      return;
    }
    admitted = true;
    if (capturePath) {
      // a capture plays at its recorded pace, by the records' own times
      const r = new RecordReader();
      const records: TapRecord[] = r.push(readFileSync(capturePath));
      const t0 = records[0]?.timeUs ?? 0;
      const start = Date.now();
      let i = 0;
      timer = setInterval(() => {
        const due = (Date.now() - start) * 1000 + t0;
        const batch: Uint8Array[] = [];
        while (i < records.length && records[i]!.timeUs <= due) {
          const x = records[i++]!;
          batch.push(encodeRecord(x.type, x.channel, x.payload, x.aux, x.timeUs));
        }
        if (batch.length) send(batch);
        if (i >= records.length && timer) clearInterval(timer);
      }, 10);
      return;
    }
    send(opening());
    let k = 0;
    timer = setInterval(() => {
      if (overflow && k === 10) {
        send([encodeRecord(RECORD.overflow, 1, new Uint8Array(0), 1 << 20)]);
        if (timer) clearInterval(timer);
        return;
      }
      send(frame(k++));
    }, 1000 / 30);
  });
  ws.on("close", () => {
    if (timer) clearInterval(timer);
    z.close();
  });
}
