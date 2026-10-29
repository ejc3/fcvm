// The GPU mirror's wire: the gfxstream mirror tap's records (ejc3/gfxstream host/mirror_tap.h),
// each channel's packets, and each packet's parameters by the emugen table (table.ts).
// Portable: no DOM, no clock.

import { GLES2, GLES2_BASE, RENDER_CONTROL, RENDER_CONTROL_BASE, type WireCall } from "./table.ts";

export const RECORD = { hello: 1, open: 2, close: 3, data: 4, reply: 5, overflow: 6, free: 7 } as const;
export const HEADER_BYTES = 24;
export const PROTOCOL_VERSION = 1;

export interface TapRecord {
  type: number;
  channel: number;
  aux: number;
  /** Microseconds on the phone's monotonic clock. */
  timeUs: number;
  payload: Uint8Array;
}

/** Splits a byte stream into tap records; holds a partial record until the rest arrives. */
export class RecordReader {
  private buf = new Uint8Array(0);
  private len = 0;

  push(chunk: Uint8Array): TapRecord[] {
    if (this.len + chunk.length > this.buf.length) {
      const next = new Uint8Array(Math.max(this.buf.length * 2, this.len + chunk.length, 1 << 16));
      next.set(this.buf.subarray(0, this.len));
      this.buf = next;
    }
    this.buf.set(chunk, this.len);
    this.len += chunk.length;
    const out: TapRecord[] = [];
    let at = 0;
    const v = new DataView(this.buf.buffer, this.buf.byteOffset);
    while (this.len - at >= HEADER_BYTES) {
      const n = v.getUint32(at + 8, true);
      if (this.len - at < HEADER_BYTES + n) break;
      out.push({
        type: v.getUint32(at, true),
        channel: v.getUint32(at + 4, true),
        aux: v.getUint32(at + 12, true),
        timeUs: v.getUint32(at + 16, true) + v.getUint32(at + 20, true) * 2 ** 32,
        payload: this.buf.slice(at + HEADER_BYTES, at + HEADER_BYTES + n),
      });
      at += HEADER_BYTES + n;
    }
    this.buf.copyWithin(0, at, this.len);
    this.len -= at;
    return out;
  }

  /** Bytes held for a record not yet whole. */
  get pending(): number {
    return this.len;
  }
}

export type Arg = number | bigint | Uint8Array | { count: number } | { paddr: bigint; count: number };

export interface Call {
  op: number;
  name: string;
  args: Arg[];
  /** Whether the server answers this call (the next REPLY on its channel is its answer). */
  replies: boolean;
  /** The return value's size in the reply, after the out pointers' bytes. */
  retBytes: number;
}

/** The call an opcode names, or null for one the mirror does not decode (GLES 1, Vulkan). */
export function wireCall(op: number): WireCall | null {
  if (op >= GLES2_BASE && op < GLES2_BASE + GLES2.length) return GLES2[op - GLES2_BASE]!;
  if (op >= RENDER_CONTROL_BASE && op < RENDER_CONTROL_BASE + RENDER_CONTROL.length) return RENDER_CONTROL[op - RENDER_CONTROL_BASE]!;
  return null;
}

/** Decodes one packet's parameters (`p` starts at its opcode). */
export function decodeCall(p: Uint8Array): Call | { op: number; unknown: true } {
  const v = new DataView(p.buffer, p.byteOffset, p.byteLength);
  const op = v.getUint32(0, true);
  const w = wireCall(op);
  if (!w) return { op, unknown: true };
  const [name, params, retBytes, replies] = w;
  const args: Arg[] = [];
  let at = 8;
  const need = (n: number) => {
    if (at + n > p.length) throw new Error(`${name}: packet of ${p.length} bytes ends inside its parameters`);
  };
  for (const k of params) {
    if (k[0] === "s") {
      const bytes = Number(k[1]);
      need(bytes);
      const kind = k[2];
      if (bytes === 1) args.push(kind === "i" ? v.getInt8(at) : v.getUint8(at));
      else if (bytes === 2) args.push(kind === "i" ? v.getInt16(at, true) : v.getUint16(at, true));
      else if (bytes === 4) args.push(kind === "f" ? v.getFloat32(at, true) : kind === "i" ? v.getInt32(at, true) : v.getUint32(at, true));
      else args.push(kind === "i" ? v.getBigInt64(at, true) : v.getBigUint64(at, true));
      at += bytes;
    } else if (k === "pd") {
      need(12);
      args.push({ paddr: v.getBigUint64(at, true), count: v.getUint32(at + 8, true) });
      at += 12;
    } else {
      need(4);
      const count = v.getUint32(at, true);
      at += 4;
      if (k === "po") {
        args.push({ count });
      } else {
        need(count);
        args.push(p.subarray(at, at + count));
        at += count;
      }
    }
  }
  return { op, name, args, replies, retBytes };
}

/**
 * One channel's byte stream cut into packets. `checksumBytes` follow each packet's
 * parameters when the guest chose a checksum (rcSelectChecksumHelper); the packet size covers
 * them, so splitting needs only the size field.
 */
export class PacketSplitter {
  private buf = new Uint8Array(0);
  private len = 0;

  push(chunk: Uint8Array): Uint8Array[] {
    if (this.len + chunk.length > this.buf.length) {
      const next = new Uint8Array(Math.max(this.buf.length * 2, this.len + chunk.length, 1 << 12));
      next.set(this.buf.subarray(0, this.len));
      this.buf = next;
    }
    this.buf.set(chunk, this.len);
    this.len += chunk.length;
    const out: Uint8Array[] = [];
    let at = 0;
    const v = new DataView(this.buf.buffer, this.buf.byteOffset);
    while (this.len - at >= 8) {
      const size = v.getUint32(at + 4, true);
      if (size < 8) throw new Error(`a packet of ${size} bytes: the channel's stream is not at a packet boundary`);
      if (this.len - at < size) break;
      out.push(this.buf.slice(at, at + size));
      at += size;
    }
    this.buf.copyWithin(0, at, this.len);
    this.len -= at;
    return out;
  }
}

/** Op number and wire entry of a call by name (the mock streamer and the tests encode with it). */
export function opOf(name: string): number {
  const g = GLES2.findIndex((c) => c[0] === name);
  if (g >= 0) return GLES2_BASE + g;
  const r = RENDER_CONTROL.findIndex((c) => c[0] === name);
  if (r >= 0) return RENDER_CONTROL_BASE + r;
  throw new Error(`no call ${name}`);
}

/** One packet as the guest's encoder writes it: scalars, and each pointer's bytes (or, for an out pointer, its count). */
export function encodeCall(name: string, args: readonly (number | bigint | Uint8Array)[]): Uint8Array {
  const op = opOf(name);
  const [, params] = wireCall(op)!;
  if (params.length !== args.length) throw new Error(`${name}: ${params.length} parameters, ${args.length} given`);
  const parts: number[] = [];
  const push = (n: number, bytes: number, kind: string) => {
    const b = new DataView(new ArrayBuffer(8));
    if (bytes === 8) b.setBigUint64(0, BigInt.asUintN(64, BigInt(n)), true);
    else if (kind === "f") b.setFloat32(0, n, true);
    else b.setUint32(0, n >>> 0, true);
    for (let i = 0; i < bytes; i++) parts.push(b.getUint8(i));
  };
  params.forEach((k, i) => {
    const a = args[i]!;
    if (k[0] === "s") {
      push(Number(a), Number(k[1]), k[2]!);
    } else if (k === "po") {
      push(Number(a), 4, "u");
    } else if (a instanceof Uint8Array) {
      push(a.length, 4, "u");
      parts.push(...a);
    } else {
      throw new Error(`${name}: parameter ${i} needs bytes`);
    }
  });
  const out = new Uint8Array(8 + parts.length);
  const v = new DataView(out.buffer);
  v.setUint32(0, op, true);
  v.setUint32(4, out.length, true);
  out.set(parts, 8);
  return out;
}

/** One tap record (gfxstream's mirror_tap.h), for the mock streamer and the tests. */
export function encodeRecord(type: number, channel: number, payload: Uint8Array, aux = 0, timeUs = 0): Uint8Array {
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
