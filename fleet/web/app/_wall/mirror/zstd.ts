// The GPU mirror's wire decoder. The phone streamer sends one zstd stream per connection and
// flushes it at the end of every WebSocket message (fleet/phone/streamer/mirror.cpp), so each
// message decodes to exactly the tap bytes it carries. zstd's own decoder, built for wasm32
// with no libc (zstd/zdec.c, scripts/build-zstd-wasm.sh), in a fixed workspace: frames whose
// window is over 1 MiB are refused rather than given more memory.

import { ZSTD_WASM_BASE64 } from "./zstd-wasm.ts";

interface Exports {
  memory: WebAssembly.Memory;
  zd_reset(): number;
  zd_in(): number;
  zd_out(): number;
  zd_in_cap(): number;
  zd_out_cap(): number;
  zd_feed(n: number): void;
  zd_in_left(): number;
  zd_step(): number;
}

let compiled: Promise<WebAssembly.Module> | null = null;

function wasmBytes(): Uint8Array<ArrayBuffer> {
  const s = atob(ZSTD_WASM_BASE64);
  const b = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) b[i] = s.charCodeAt(i);
  return b;
}

/** One zstd stream: create() per connection, push() each message in order. */
export class ZstdStream {
  private readonly x: Exports;

  private constructor(x: Exports) {
    this.x = x;
  }

  static async create(): Promise<ZstdStream> {
    compiled ??= WebAssembly.compile(wasmBytes());
    const instance = await WebAssembly.instantiate(await compiled, {});
    const x = instance.exports as unknown as Exports;
    const rc = x.zd_reset();
    if (rc !== 0) throw new Error(`zstd decoder: no workspace (${rc})`);
    return new ZstdStream(x);
  }

  /** The bytes one message carries. Throws on a stream that is not zstd or is corrupt. */
  push(message: Uint8Array): Uint8Array {
    const x = this.x;
    const inCap = x.zd_in_cap();
    const outCap = x.zd_out_cap();
    const parts: Uint8Array[] = [];
    let total = 0;
    for (let at = 0; at < message.length; at += inCap) {
      const piece = message.subarray(at, at + inCap);
      new Uint8Array(x.memory.buffer, x.zd_in(), piece.length).set(piece);
      x.zd_feed(piece.length);
      for (;;) {
        const n = x.zd_step();
        if (n < 0) throw new Error(`zstd error ${-100 - n}`);
        if (n > 0) {
          parts.push(new Uint8Array(x.memory.buffer, x.zd_out(), n).slice());
          total += n;
        }
        // a full output may hold more; otherwise the piece is done once it is all read
        if (n < outCap && x.zd_in_left() === 0) break;
      }
    }
    if (parts.length === 1) return parts[0]!;
    const out = new Uint8Array(total);
    let o = 0;
    for (const p of parts) {
      out.set(p, o);
      o += p.length;
    }
    return out;
  }
}
