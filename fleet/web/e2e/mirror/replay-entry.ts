// The GPU mirror's replay test, in the page (e2e/mirror.spec.ts bundles it): a captured tap
// stream fed to the mirror on a WebGL2 canvas, and each posted frame whose stamp matches one of
// the phone's own screenshots compared with it.

import { GlMirror, type JoinStats } from "../../app/_wall/mirror/gl.ts";
import { RECORD, RecordReader } from "../../core/mirror/wire.ts";

/** The app's frame number in the stamp band of an RGBA image whose rows run top first (stamp.cpp). */
export function readStamp(px: Uint8ClampedArray | Uint8Array, w: number, h: number): number | null {
  const y = h - 4;
  let v = 0;
  for (let i = 0; i < 32; i++) {
    const x = Math.floor((Math.floor((i * w) / 32) + Math.floor(((i + 1) * w) / 32)) / 2);
    const o = (y * w + x) * 4;
    const lo = Math.min(px[o]!, px[o + 1]!, px[o + 2]!);
    const hi = Math.max(px[o]!, px[o + 1]!, px[o + 2]!);
    let bit: number;
    if (lo >= 160) bit = 1;
    else if (hi <= 96) bit = 0;
    else return null;
    v = v * 2 + bit;
  }
  // all black or all white is a screen that happens to show no stamp, not frame 0 or 2^32 - 1
  return v === 0 || v === 0xffffffff ? null : v;
}

export interface Ref {
  k: number;
  w: number;
  h: number;
  px: Uint8ClampedArray;
}

export interface Match {
  k: number;
  /** Pixels (stamp band excluded) whose largest channel difference is above 16 levels, as a share. */
  over16: number;
  /** Mean absolute difference per channel, in levels. */
  meanAbs: number;
  psnr: number;
}

export interface ReplayResult {
  posts: number;
  stamped: number[];
  matches: Match[];
  missing: number[];
  skipped: Record<string, number>;
  failed: Record<string, number>;
  unknownChannelPackets: number;
  flipped: boolean | null;
}

export function compare(a: Uint8Array, b: Uint8ClampedArray, w: number, h: number): Omit<Match, "k"> {
  let over = 0;
  let sum = 0;
  let sq = 0;
  const rows = h - 8;
  for (let y = 0; y < rows; y++) {
    for (let x = 0; x < w; x++) {
      const o = (y * w + x) * 4;
      let m = 0;
      for (let c = 0; c < 3; c++) {
        const d = Math.abs(a[o + c]! - b[o + c]!);
        sum += d;
        sq += d * d;
        if (d > m) m = d;
      }
      if (m > 16) over++;
    }
  }
  const n = rows * w;
  const mse = sq / (n * 3);
  return { over16: over / n, meanAbs: sum / (n * 3), psnr: mse === 0 ? 99 : 10 * Math.log10((255 * 255) / mse) };
}

export function replay(canvas: HTMLCanvasElement, tap: Uint8Array, refs: Ref[]): ReplayResult {
  const gl = canvas.getContext("webgl2", { antialias: false, alpha: false, preserveDrawingBuffer: true })!;
  const m = new GlMirror(gl, () => 0);
  const want = new Map(refs.map((r) => [r.k, r]));
  const stamped: number[] = [];
  const missed: number[] = [];
  const matches: Match[] = [];
  let flipped: boolean | null = null;
  // Robot Park draws every frame whole, so the frames between the ones compared need not be
  // rasterized: after the first stamp the replay fast-forwards to 200 ms before each screenshot
  // (the app draws at 60 Hz, so a stamp k is k/60 s of the scene clock).
  let anchor: { k: number; timeUs: number } | null = null;
  const pendingKs = () => [...want.keys()].sort((a, b) => a - b);
  const dueUs = (k: number) => (anchor ? anchor.timeUs + ((k - anchor.k) / 60) * 1e6 : 0);
  const readRow = (y: number, w: number) => {
    const row = new Uint8Array(w * 4);
    gl.readPixels(0, y, w, 1, gl.RGBA, gl.UNSIGNED_BYTE, row);
    return row;
  };
  const rowStamp = (row: Uint8Array, w: number) => readStamp(row, w, 1 + 3);
  m.onPost = (_cb, timeUs) => {
    const w = gl.drawingBufferWidth;
    const h = gl.drawingBufferHeight;
    if (m.skipDraws) return;
    // the stamp band's middle row, at the bottom of the screen (readPixels counts rows from the bottom)
    const k0 = rowStamp(readRow(3, w), w) ?? rowStamp(readRow(h - 4, w), w);
    if (k0 !== null && anchor === null) anchor = { k: k0, timeUs };
    // a screenshot whose frame went by unmatched (drawn while fast-forwarding, or never posted)
    for (const k of pendingKs()) {
      if (anchor && timeUs - dueUs(k) > 1_000_000) {
        want.delete(k);
        missed.push(k);
      }
    }
    if (k0 === null || !want.has(k0)) {
      stamped.push(k0 ?? -1);
      const next = pendingKs()[0];
      if (anchor && next !== undefined && dueUs(next) - timeUs > 300_000) m.skipDraws = true;
      return;
    }
    const raw = new Uint8Array(w * h * 4);
    gl.readPixels(0, 0, w, h, gl.RGBA, gl.UNSIGNED_BYTE, raw);
    // readPixels gives the bottom row first; the stamp is the screen's bottom band
    const top = new Uint8Array(w * h * 4);
    for (let y = 0; y < h; y++) top.set(raw.subarray((h - 1 - y) * w * 4, (h - y) * w * 4), y * w * 4);
    let k = readStamp(top, w, h);
    let img = top;
    if (k === null) {
      const kk = readStamp(raw, w, h);
      if (kk !== null) {
        k = kk;
        img = raw;
        flipped = true;
      }
    } else {
      flipped = false;
    }
    if (k === null) return;
    stamped.push(k);
    const r = want.get(k);
    if (r && r.w === w && r.h === h) {
      matches.push({ k, ...compare(img, r.px, w, h) });
      want.delete(k);
    }
  };
  const rr = new RecordReader();
  for (const rec of rr.push(tap)) {
    const next = pendingKs()[0];
    // every screenshot compared or given up: the rest of the stream draws nothing compared
    if (next === undefined) break;
    if (m.skipDraws && anchor && dueUs(next) - rec.timeUs < 200_000) m.skipDraws = false;
    m.feed(rec);
  }
  return {
    posts: m.stats.posts,
    stamped,
    matches,
    missing: [...missed, ...want.keys()].sort((a, b) => a - b),
    skipped: m.stats.skipped,
    failed: m.stats.failed,
    unknownChannelPackets: m.stats.unknownChannelPackets,
    flipped,
  };
}

export interface JoinResult {
  join: JoinStats | null;
  /** Whether the first frame the mirror drew was drawn from the join snapshot, before any live record. */
  firstFromJoin: boolean;
  /** The first frame against the phone's screenshot, or null when the mirror drew none. */
  match: Omit<Match, "k"> | null;
  w: number;
  h: number;
  failed: Record<string, number>;
}

/**
 * A stream that joined a running phone (its JOIN records first): the first frame the mirror
 * draws, against the phone's screenshot taken as the stream was captured. The phone showed a
 * screen that does not change (its home screen), so the screenshot is that frame.
 */
export function replayJoin(canvas: HTMLCanvasElement, tap: Uint8Array, ref: Ref): JoinResult {
  const gl = canvas.getContext("webgl2", { antialias: false, alpha: false, preserveDrawingBuffer: true })!;
  const m = new GlMirror(gl, () => 0);
  let first: Uint8Array | null = null;
  let live = false;
  let firstFromJoin = false;
  m.onPost = () => {
    if (first) return;
    const w = gl.drawingBufferWidth;
    const h = gl.drawingBufferHeight;
    const raw = new Uint8Array(w * h * 4);
    gl.readPixels(0, 0, w, h, gl.RGBA, gl.UNSIGNED_BYTE, raw);
    // readPixels gives the bottom row first, as the live frames' replay above reads them
    first = new Uint8Array(w * h * 4);
    for (let y = 0; y < h; y++) first.set(raw.subarray((h - 1 - y) * w * 4, (h - y) * w * 4), y * w * 4);
    firstFromJoin = !live;
  };
  for (const rec of new RecordReader().push(tap)) {
    if (rec.type !== RECORD.hello && rec.type !== RECORD.join) live = true;
    m.feed(rec);
    if (first) break;
  }
  const w = gl.drawingBufferWidth;
  const h = gl.drawingBufferHeight;
  return {
    join: m.stats.join,
    firstFromJoin,
    match: first && ref.w === w && ref.h === h ? compare(first, ref.px, w, h) : null,
    w,
    h,
    failed: m.stats.failed,
  };
}

declare global {
  interface Window {
    mirrorReplay: { replay: typeof replay; replayJoin: typeof replayJoin; readStamp: typeof readStamp };
  }
}
window.mirrorReplay = { replay, replayJoin, readStamp };
