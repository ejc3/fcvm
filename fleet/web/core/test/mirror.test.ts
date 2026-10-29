import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { megabytes, mbits, RateMeter } from "../meter.ts";
import { GLES2, GLES2_BASE, RENDER_CONTROL, RENDER_CONTROL_BASE } from "../mirror/table.ts";
import { decodeCall, encodeCall, encodeRecord, HEADER_BYTES, PacketSplitter, RECORD, RecordReader, wireCall } from "../mirror/wire.ts";

const root = fileURLToPath(new URL("../../", import.meta.url));

/** One tap record as gfxstream's mirror_tap.cpp writes it. */
function record(type: number, channel: number, payload: Uint8Array, aux = 0, timeUs = 0): Uint8Array {
  const out = new Uint8Array(HEADER_BYTES + payload.length);
  const v = new DataView(out.buffer);
  v.setUint32(0, type, true);
  v.setUint32(4, channel, true);
  v.setUint32(8, payload.length, true);
  v.setUint32(12, aux, true);
  v.setUint32(16, timeUs % 2 ** 32, true);
  v.setUint32(20, Math.floor(timeUs / 2 ** 32), true);
  out.set(payload, HEADER_BYTES);
  return out;
}

/** One packet: opcode, size, then the parameter bytes. */
function packet(op: number, body: number[]): Uint8Array {
  const p = new Uint8Array(8 + body.length);
  const v = new DataView(p.buffer);
  v.setUint32(0, op, true);
  v.setUint32(4, p.length, true);
  p.set(body, 8);
  return p;
}
const le32 = (x: number) => [x & 0xff, (x >> 8) & 0xff, (x >> 16) & 0xff, (x >>> 24) & 0xff];
const f32 = (x: number) => Array.from(new Uint8Array(new Float32Array([x]).buffer));
const op = (name: string) => {
  const g = GLES2.findIndex((c) => c[0] === name);
  if (g >= 0) return GLES2_BASE + g;
  return RENDER_CONTROL_BASE + RENDER_CONTROL.findIndex((c) => c[0] === name);
};

describe("the GPU mirror's wire", () => {
  it("holds the table emugen's specs give", () => {
    const r = spawnSync(process.execPath, [`${root}scripts/gen-mirror-table.ts`, "--check"], { encoding: "utf8" });
    expect(r.status, r.stderr).toBe(0);
    expect(GLES2).toHaveLength(439);
    expect(RENDER_CONTROL).toHaveLength(74);
    expect(wireCall(GLES2_BASE)?.[0]).toBe("glActiveTexture");
    expect(wireCall(RENDER_CONTROL_BASE)?.[0]).toBe("rcGetRendererVersion");
    expect(wireCall(1024)).toBeNull(); // GLES 1
  });

  it("cuts tap records however the bytes arrive", () => {
    const a = record(RECORD.data, 7, new Uint8Array([1, 2, 3]), 0, 2 ** 33 + 5);
    const b = record(RECORD.reply, 7, new Uint8Array([9]), 4000);
    const all = new Uint8Array([...a, ...b]);
    const r = new RecordReader();
    const got = [];
    for (let i = 0; i < all.length; i += 5) got.push(...r.push(all.subarray(i, i + 5)));
    expect(got).toHaveLength(2);
    expect(got[0]).toMatchObject({ type: RECORD.data, channel: 7, timeUs: 2 ** 33 + 5 });
    expect(Array.from(got[0]!.payload)).toEqual([1, 2, 3]);
    expect(got[1]).toMatchObject({ type: RECORD.reply, aux: 4000 });
    expect(r.pending).toBe(0);
  });

  it("cuts a channel into packets and decodes each call's parameters", () => {
    const viewport = packet(op("glViewport"), [...le32(0), ...le32(0), ...le32(720), ...le32(1280)]);
    const mask = packet(op("glColorMask"), [1, 0, 1, 0]);
    const gen = packet(op("glGenTextures"), [...le32(2), ...le32(8)]);
    const tex = packet(op("glTexSubImage2D"), [
      ...le32(0x0de1), ...le32(0), ...le32(1), ...le32(2), ...le32(1), ...le32(1), ...le32(0x1908), ...le32(0x1401),
      ...le32(4), 10, 20, 30, 40,
    ]);
    const clear = packet(op("glClearColor"), [...f32(0.5), ...f32(0), ...f32(1), ...f32(0.25)]);
    const s = new PacketSplitter();
    const stream = new Uint8Array([...viewport, ...mask, ...gen, ...tex, ...clear]);
    const packets = [...s.push(stream.subarray(0, 13)), ...s.push(stream.subarray(13))];
    expect(packets).toHaveLength(5);
    const calls = packets.map(decodeCall);
    expect(calls[0]).toMatchObject({ name: "glViewport", args: [0, 0, 720, 1280], replies: false });
    expect(calls[1]).toMatchObject({ name: "glColorMask", args: [1, 0, 1, 0] });
    expect(calls[2]).toMatchObject({ name: "glGenTextures", args: [2, { count: 8 }], replies: true });
    const t = calls[3] as { args: unknown[] };
    expect(Array.from(t.args[8] as Uint8Array)).toEqual([10, 20, 30, 40]);
    expect(calls[4]).toMatchObject({ name: "glClearColor", args: [0.5, 0, 1, 0.25] });
  });

  it("encodes what it decodes, every parameter kind", () => {
    const px = new Uint8Array([1, 2, 3, 4]);
    const calls: [string, (number | bigint | Uint8Array)[]][] = [
      ["glColorMask", [1, 0, 1, 1]],
      ["glClearColor", [0.25, 0.5, 0.75, 1]],
      ["glGenTextures", [1, 4]],
      ["glTexSubImage2D", [0x0de1, 0, 0, 0, 1, 1, 0x1908, 0x1401, px]],
      ["glClientWaitSyncAEMU", [7n, 1, 1000n]],
      ["rcCreateColorBuffer", [720, 1280, 0x1908]],
    ];
    for (const [name, args] of calls) {
      const d = decodeCall(encodeCall(name, args)) as { name: string; args: unknown[] };
      expect(d.name).toBe(name);
      expect(d.args.map((a) => (a instanceof Uint8Array ? Array.from(a) : typeof a === "object" && a !== null && "count" in a ? (a as { count: number }).count : a))).toEqual(
        args.map((a) => (a instanceof Uint8Array ? Array.from(a) : a)),
      );
    }
    expect(encodeRecord(RECORD.reply, 3, px, 9).length).toBe(HEADER_BYTES + 4);
  });

  it("refuses a stream that is not at a packet boundary", () => {
    expect(() => new PacketSplitter().push(new Uint8Array([1, 0, 0, 0, 3, 0, 0, 0]))).toThrow(/packet boundary/);
  });

  it("says when a packet ends inside its parameters", () => {
    const p = packet(op("glTexSubImage2D"), [...le32(0x0de1), ...le32(0), ...le32(0), ...le32(0), ...le32(1), ...le32(1), ...le32(0x1908), ...le32(0x1401), ...le32(64)]);
    expect(() => decodeCall(p)).toThrow(/ends inside/);
  });
});

describe("the take-over panel's meter", () => {
  it("is bytes over the last second, and the total since it started", () => {
    const m = new RateMeter();
    for (let t = 0; t < 3000; t += 100) m.add(t, 12_500); // 125 kB/s = 1 Mbit/s
    expect(m.perSecond(2900)).toBe(125_000);
    expect(mbits(m.perSecond(2900))).toBe("1.00");
    expect(m.total).toBe(375_000);
    expect(m.perSecond(5000)).toBe(0); // quiet for two seconds
    expect(m.total).toBe(375_000);
  });

  it("does not report a burst at start as a rate over a whole second", () => {
    const m = new RateMeter();
    m.add(1000, 50_000);
    // 250 ms is the shortest span it divides by
    expect(m.perSecond(1000)).toBe(200_000);
    m.add(1500, 50_000);
    expect(m.perSecond(1500)).toBe(200_000);
  });

  it("prints Mbit/s and totals", () => {
    expect(mbits(1_250_000)).toBe("10.0");
    expect(mbits(25_000_000)).toBe("200");
    expect(megabytes(12_345_678)).toBe("12.3 MB");
    expect(megabytes(2_500_000_000)).toBe("2.50 GB");
  });
});

describe("the phones page's mirror canvas", () => {
  // Pinned at the source: no test runs the page under React's Strict Mode, which runs a mount
  // twice, and a canvas can be transferred to a worker only once.
  it("is made by each mount and removed with it, never a rendered canvas transferred again", () => {
    const src = readFileSync(`${root}app/_wall/Phones.tsx`, "utf8");
    const effect = src.slice(src.indexOf("const mirrorHostRef"), src.indexOf("const m = view.mirror;"));
    expect(effect).toContain('document.createElement("canvas")');
    expect(effect).toContain("c.transferControlToOffscreen()");
    expect(effect).toContain("c.remove()");
    expect(src).not.toMatch(/<canvas[^>]*ref=\{mirror/);
  });
});
