// The renderer's binary frame message: a 22-byte header and one H.264 Annex-B
// access unit.
//
//   0-3   "FCVF"
//   4     version, 1
//   5     flags, bit 0 = keyframe
//   6-7   width, u16 little-endian
//   8-9   height, u16 little-endian
//   10-17 t, f64 little-endian (scene seconds since scene.epoch_unix_ms)
//   18-21 frame number, u32 little-endian
//   22-   the access unit

export const FRAME_HEADER_BYTES = 22;
export const FRAME_VERSION = 1;
const MAGIC = [0x46, 0x43, 0x56, 0x46]; // "FCVF"

export interface FrameHeader {
  keyframe: boolean;
  width: number;
  height: number;
  t: number;
  frame: number;
}

export interface Frame extends FrameHeader {
  /** The access unit, a view into the message. */
  data: Uint8Array;
}

export type FrameParse = { ok: true; frame: Frame } | { ok: false; error: string };

export function parseFrame(buf: Uint8Array): FrameParse {
  if (buf.byteLength < FRAME_HEADER_BYTES + 4) return { ok: false, error: "frame message too short" };
  for (let i = 0; i < 4; i++) {
    if (buf[i] !== MAGIC[i]) return { ok: false, error: "frame message has no FCVF magic" };
  }
  if (buf[4] !== FRAME_VERSION) return { ok: false, error: `frame version ${buf[4]} is not ${FRAME_VERSION}` };
  const dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const width = dv.getUint16(6, true);
  const height = dv.getUint16(8, true);
  const t = dv.getFloat64(10, true);
  if (width === 0 || height === 0) return { ok: false, error: "frame has zero size" };
  if (!Number.isFinite(t)) return { ok: false, error: "frame t is not finite" };
  return {
    ok: true,
    frame: {
      keyframe: (buf[5]! & 1) === 1,
      width,
      height,
      t,
      frame: dv.getUint32(18, true),
      data: buf.subarray(FRAME_HEADER_BYTES),
    },
  };
}

/** Build a frame message. The mock renderer uses this; so do the tests. */
export function buildFrame(h: FrameHeader, accessUnit: Uint8Array): Uint8Array {
  const out = new Uint8Array(FRAME_HEADER_BYTES + accessUnit.byteLength);
  out.set(MAGIC, 0);
  out[4] = FRAME_VERSION;
  out[5] = h.keyframe ? 1 : 0;
  const dv = new DataView(out.buffer);
  dv.setUint16(6, h.width, true);
  dv.setUint16(8, h.height, true);
  dv.setFloat64(10, h.t, true);
  dv.setUint32(18, h.frame >>> 0, true);
  out.set(accessUnit, FRAME_HEADER_BYTES);
  return out;
}

export interface StreamHeader {
  codec: string;
  width: number;
  height: number;
}

/** The text message a frame socket sends first (and, from some renderers, after a size change). */
export function parseStreamHeader(text: string): StreamHeader | null {
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch {
    return null;
  }
  if (typeof v !== "object" || v === null) return null;
  const o = v as Record<string, unknown>;
  if (typeof o["codec"] !== "string" || !/^avc1\.[0-9A-Fa-f]{6}$/.test(o["codec"])) return null;
  if (typeof o["width"] !== "number" || typeof o["height"] !== "number") return null;
  return { codec: o["codec"], width: o["width"], height: o["height"] };
}

/** The codec string for an SPS NAL unit (without its start code): avc1.PPCCLL. */
export function codecFromSps(sps: Uint8Array): string {
  const hex = (b: number) => b.toString(16).padStart(2, "0").toUpperCase();
  return `avc1.${hex(sps[1]!)}${hex(sps[2]!)}${hex(sps[3]!)}`;
}

/**
 * Split an Annex-B byte stream into NAL units (start codes removed). Used by the
 * mock to cut the encoder's output into access units and by tests.
 */
export function splitNals(buf: Uint8Array): Uint8Array[] {
  const starts: { at: number; len: number }[] = [];
  for (let i = 0; i + 2 < buf.length; i++) {
    if (buf[i] === 0 && buf[i + 1] === 0) {
      if (buf[i + 2] === 1) {
        starts.push({ at: i, len: 3 });
        i += 2;
      } else if (buf[i + 2] === 0 && i + 3 < buf.length && buf[i + 3] === 1) {
        starts.push({ at: i, len: 4 });
        i += 3;
      }
    }
  }
  const out: Uint8Array[] = [];
  for (let k = 0; k < starts.length; k++) {
    const begin = starts[k]!.at + starts[k]!.len;
    const end = k + 1 < starts.length ? starts[k + 1]!.at : buf.length;
    if (end > begin) out.push(buf.subarray(begin, end));
  }
  return out;
}

export const NAL_IDR = 5;
export const NAL_SPS = 7;
export const NAL_AUD = 9;

export function nalType(nal: Uint8Array): number {
  return nal[0]! & 0x1f;
}
