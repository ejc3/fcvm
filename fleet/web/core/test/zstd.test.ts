// The GPU mirror's zstd decoder (app/_wall/mirror/zstd.ts) against node's zstd, flushed at the
// end of every message as the phone streamer does: each message gives back exactly what was
// pushed for it, whatever its size; a corrupt stream and a window over 1 MiB are refused; and
// the wasm was built from the sources in the tree.

import { createHash } from "node:crypto";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { constants, createZstdCompress, zstdCompressSync, type ZstdCompress } from "node:zlib";
import { describe, expect, it } from "vitest";
import { ZstdStream } from "../../app/_wall/mirror/zstd.ts";
import { ZSTD_WASM_SOURCES } from "../../app/_wall/mirror/zstd-wasm.ts";

/** One message: what the compressor gives back after `chunk` and a flush (ZSTD_e_flush). */
function flushed(z: ZstdCompress, chunk: Uint8Array): Promise<Uint8Array> {
  return new Promise((resolve, reject) => {
    const out: Buffer[] = [];
    const take = (b: Buffer) => out.push(b);
    z.on("data", take);
    z.write(chunk, (e) => {
      if (e) return reject(e);
      z.flush(constants.ZSTD_e_flush, () => {
        z.off("data", take);
        resolve(new Uint8Array(Buffer.concat(out)));
      });
    });
  });
}

function compressor(windowLog = 20): ZstdCompress {
  return createZstdCompress({ params: { [constants.ZSTD_c_compressionLevel]: 3, [constants.ZSTD_c_windowLog]: windowLog } });
}

/** Tap-like bytes: repetitive with a counter, so they compress and are not all alike. */
function bytes(n: number, seed: number): Uint8Array {
  const b = new Uint8Array(n);
  for (let i = 0; i < n; i++) b[i] = (i % 97) ^ ((i >> 10) + seed) ^ (i % 13 === 0 ? seed * 7 : 0);
  return b;
}

describe("ZstdStream", () => {
  it("gives back each flushed message's bytes as it arrives", async () => {
    const z = compressor();
    const d = await ZstdStream.create();
    // sizes around the decoder's 256 KiB input and output buffers, and one of several MiB
    const sizes = [1, 24, 1000, 65536, (1 << 18) - 1, 1 << 18, (1 << 18) + 1, 700_000, 3 << 20, 5];
    for (const [i, n] of sizes.entries()) {
      const chunk = bytes(n, i);
      const msg = await flushed(z, chunk);
      const got = d.push(msg);
      expect(got.length, `message ${i} of ${n} bytes`).toBe(n);
      expect(Buffer.compare(Buffer.from(got), Buffer.from(chunk)), `message ${i}`).toBe(0);
    }
    z.close();
  });

  it("refuses bytes that are not a zstd stream", async () => {
    const d = await ZstdStream.create();
    // zstd's prefix_unknown
    expect(() => d.push(new Uint8Array([0x78, 0x9c, 1, 2, 3, 4, 5, 6]))).toThrow("zstd error 10");
  });

  it("refuses a frame whose window is over 1 MiB", async () => {
    const big = zstdCompressSync(bytes(4 << 20, 1), {
      params: { [constants.ZSTD_c_compressionLevel]: 3, [constants.ZSTD_c_windowLog]: 22 },
    });
    const d = await ZstdStream.create();
    // zstd's frameParameter_windowTooLarge
    expect(() => d.push(new Uint8Array(big))).toThrow("zstd error 16");
  });

  it("is built from the sources in the tree (rerun scripts/build-zstd-wasm.sh after changing them)", () => {
    const dir = fileURLToPath(new URL("../../app/_wall/mirror/zstd/", import.meta.url));
    const files: string[] = [];
    const walk = (d: string, rel: string) => {
      for (const name of readdirSync(d)) {
        const p = join(d, name);
        if (statSync(p).isDirectory()) walk(p, `${rel}${name}/`);
        else if (name !== "LICENSE") files.push(`./${rel}${name}`);
      }
    };
    walk(dir, "");
    // what the script computes: sha256sum of each file in byte order, then of that listing
    const listing = files
      .sort((a, b) => (Buffer.from(a) < Buffer.from(b) ? -1 : 1))
      .map((f) => `${createHash("sha256").update(readFileSync(join(dir, f))).digest("hex")}  ${f}\n`)
      .join("");
    expect(createHash("sha256").update(listing).digest("hex")).toBe(ZSTD_WASM_SOURCES);
  });
});
