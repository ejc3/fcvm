// The mock renderer's picture: a pure function of (t, image position, scene).
// Every pixel is computed in whole-image coordinates, so two tiles drawn at the
// same t meet without a seam, and a tile drawn at another t shows one. It stands
// in for the real renderer's path-traced planet: a moving nebula, a ringed
// planet, stars, the viewers' words and lights, and the UTC time, rendered
// from t, across the bottom of the image.

import { cameraAt } from "../core/scene.ts";

export interface MockWord {
  text: string;
  pos: { x: number; y: number };
}

export interface MockLight {
  pos: { x: number; y: number };
  /** RGB, 0..1. */
  color: [number, number, number];
}

export interface PatternScene {
  epoch_unix_ms: number;
  image_aspect: number;
  /** Keys with x, y and zoom: the mock's 2D camera. */
  camera: readonly { t: number }[];
  /** Hours, 0..24. */
  time_of_day: number;
  words: MockWord[];
  lights: MockLight[];
}

// ---- a 5x7 bitmap font --------------------------------------------------------

const GLYPHS: Record<string, string> = {
  "0": "0e11131519110e", "1": "040c040404040e", "2": "0e11010204081f", "3": "1f02040201110e",
  "4": "02060a121f0202", "5": "1f101e0101110e", "6": "0608101e11110e", "7": "1f010204080808",
  "8": "0e11110e11110e", "9": "0e11110f01020c", ":": "000c0c000c0c00", " ": "00000000000000",
  "A": "0e11111f111111", "B": "1e11111e11111e", "C": "0e11101010110e", "D": "1c12111111121c",
  "E": "1f10101e10101f", "F": "1f10101e101010", "G": "0e11101713110f", "H": "1111111f111111",
  "I": "0e04040404040e", "J": "0702020202120c", "K": "11121418141211", "L": "1010101010101f",
  "M": "111b1515111111", "N": "11111915131111", "O": "0e11111111110e", "P": "1e11111e101010",
  "Q": "0e11111115120d", "R": "1e11111e141211", "S": "0f10100e01011e", "T": "1f040404040404",
  "U": "1111111111110e", "V": "11111111110a04", "W": "11111115151b11", "X": "11110a040a1111",
  "Y": "11110a04040404", "Z": "1f01020408101f", "-": "0000001f000000", ".": "000000000c0c00",
  "!": "04040404000400", "?": "0e110102040004", "'": "0c040800000000", ",": "000000000c0408",
};

const ROWS: Record<string, number[]> = {};
for (const [ch, hex] of Object.entries(GLYPHS)) {
  const rows: number[] = [];
  for (let i = 0; i < 7; i++) rows.push(parseInt(hex.slice(i * 2, i * 2 + 2), 16) || 0);
  ROWS[ch] = rows;
}

/** Is cell (col, row) of `ch` lit? 5 columns, 7 rows. */
function lit(ch: string, col: number, row: number): boolean {
  const rows = ROWS[ch] ?? ROWS["?"]!;
  return ((rows[row]! >> (4 - col)) & 1) === 1;
}

/**
 * Coverage of LED-style text at a point. (px, py) is in text cells from the text's
 * top-left; each glyph is 5x7 cells with one cell of spacing. Returns 0..1.
 */
function textCoverage(text: string, px: number, py: number): number {
  if (py < 0 || py >= 7 || px < 0 || px >= text.length * 6) return 0;
  const gi = Math.floor(px / 6);
  const cx = px - gi * 6;
  if (cx >= 5) return 0;
  const col = Math.floor(cx);
  const row = Math.floor(py);
  if (!lit(text[gi]!, col, row)) return 0;
  // A rounded dot per cell, anti-aliased.
  const dx = cx - col - 0.5;
  const dy = py - row - 0.5;
  const d = Math.sqrt(dx * dx + dy * dy);
  return Math.min(1, Math.max(0, (0.5 - d) * 6 + 0.5));
}

// ---- the picture --------------------------------------------------------------

const TAU = Math.PI * 2;

/** Per-frame values every pixel of a frame shares. */
export interface FrameCtx {
  t: number;
  aspect: number;
  camX: number;
  camY: number;
  zoom: number;
  day: number;
  hue: number;
  /** Unit vector toward the sun. */
  sun: [number, number, number];
  clock: string;
  words: { text: string; x: number; y: number }[];
  lights: { x: number; y: number; rgb: [number, number, number]; pulse: number }[];
}

export const WORD_HEIGHT = 0.07; // world units
export const CLOCK_HEIGHT = 0.075; // fraction of the image height
export const CLOCK_V = 0.83;

export function frameCtx(t: number, scene: PatternScene): FrameCtx {
  const cam = cameraAt(scene.camera, t);
  const unix = scene.epoch_unix_ms + t * 1000;
  const s = Math.floor(unix / 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  const clock = `${pad(Math.floor((s % 86400) / 3600))}:${pad(Math.floor((s % 3600) / 60))}:${pad(s % 60)} UTC`;
  const tod = scene.time_of_day / 24;
  const day = 0.5 - 0.5 * Math.cos(TAU * tod);
  const sunA = Math.PI * (0.15 + 0.7 * day);
  const sv = [-Math.cos(sunA), -0.45, Math.sin(sunA)];
  const sl = Math.hypot(sv[0]!, sv[1]!, sv[2]!);
  return {
    sun: [sv[0]! / sl, sv[1]! / sl, sv[2]! / sl],
    t,
    aspect: scene.image_aspect,
    camX: cam["x"] ?? 0,
    camY: cam["y"] ?? 0,
    zoom: Math.max(0.05, cam["zoom"] ?? 1),
    day,
    hue: t * 0.045 + tod * 0.6,
    clock,
    words: scene.words.map((w) => ({ text: w.text.toUpperCase(), x: w.pos.x, y: w.pos.y })),
    lights: scene.lights.map((l, i) => ({
      x: l.pos.x,
      y: l.pos.y,
      rgb: l.color,
      pulse: 0.75 + 0.25 * Math.sin(t * 3 + i * 1.7),
    })),
  };
}

/** Linear RGB 0..1 of the image at (u, v) for one frame. Pure. */
export function shade(u: number, v: number, f: FrameCtx, out: Float64Array | number[] = [0, 0, 0]): typeof out {
  const t = f.t;
  const X = ((u - 0.5) * f.aspect) / f.zoom + f.camX;
  const Y = (v - 0.5) / f.zoom + f.camY;

  // A nebula: two domain warps feeding a cosine palette that drifts with t,
  // over deep space.
  const w1 = Math.sin(X * 1.3 + t * 0.35) + Math.sin(Y * 1.7 - t * 0.25);
  const w2 = Math.sin(X * 0.9 - t * 0.2 + Y * 1.1) + Math.cos(Y * 2.1 + t * 0.3);
  const p = Math.sin((X + 0.55 * w1) * 2.2 + t * 0.4) * Math.cos((Y + 0.55 * w2) * 2.6 - t * 0.3);
  const q = 0.5 + 0.5 * p;
  const k = TAU * (q * 0.7 + f.hue);
  const dens = 0.24 + 0.76 * q * q * (0.75 + 0.25 * Math.sin(X * 3.1 - Y * 2.3 + t * 0.6));
  // A slow swell of light that travels across the whole image; it is what makes
  // a tile drawn at the wrong t stand out against its neighbours.
  const swell = 1 + 0.32 * Math.sin(2.0 * X + 0.7 * Y - 1.9 * t);
  const night = (0.45 + 0.55 * f.day) * swell;
  let r = 0.015 + dens * night * (0.5 + 0.45 * Math.cos(k + 0.0));
  let g = 0.02 + dens * night * (0.42 + 0.4 * Math.cos(k + 2.1));
  let b = 0.06 + dens * night * (0.6 + 0.4 * Math.cos(k + 4.2));

  // A ringed planet straddling the line between the halves, lit by a sun whose
  // angle follows the time of day.
  const px = X - 0.02;
  const py = Y + 0.02;
  const R = 0.3;
  const d2 = px * px + py * py;
  const d = Math.sqrt(d2);
  const sx0 = f.sun[0];
  const sy0 = f.sun[1];
  const sz0 = f.sun[2];
  const nearRing = Math.abs(py) < (R * 2.15) / 4.2 && Math.abs(px) < R * 2.15;
  const ringR = nearRing ? Math.sqrt(px * px + py * 4.2 * (py * 4.2)) : 0;
  const inRing = nearRing && ringR > R * 1.3 && ringR < R * 2.15;
  const ringBand = inRing
    ? (0.55 + 0.45 * Math.sin(ringR * 95)) * Math.min(1, (ringR - R * 1.3) * 40, (R * 2.15 - ringR) * 40)
    : 0;
  const drawRing = (front: boolean) => {
    if (ringBand <= 0 || (py > 0) !== front) return;
    const shadowed = !front && d < R ? 0 : 1;
    const a = ringBand * 0.75 * shadowed;
    r = r * (1 - a) + 0.85 * a * (0.5 + 0.5 * f.day);
    g = g * (1 - a) + 0.74 * a * (0.5 + 0.5 * f.day);
    b = b * (1 - a) + 0.6 * a * (0.5 + 0.5 * f.day);
  };
  drawRing(false);
  if (d < R) {
    const nz = Math.sqrt(R * R - d2) / R;
    const nx = px / R;
    const ny = py / R;
    const lam = Math.max(0, nx * sx0 + ny * sy0 + nz * sz0);
    const lat = ny;
    const band = 0.5 + 0.5 * Math.sin(lat * 16 + Math.sin(nx * 5 + t * 0.45) * 0.9 + t * 0.12);
    const storm = Math.exp(-((nx + 0.25) ** 2 + (ny - 0.3) ** 2) / 0.006);
    const lightLevel = 0.03 + 1.05 * lam;
    const pr = (0.62 + 0.3 * band + storm * 0.3) * lightLevel;
    const pg = (0.45 + 0.25 * band + storm * 0.05) * lightLevel;
    const pb = (0.3 + 0.18 * (1 - band)) * lightLevel;
    const edge = Math.min(1, (R - d) * 400);
    r = r * (1 - edge) + pr * edge;
    g = g * (1 - edge) + pg * edge;
    b = b * (1 - edge) + pb * edge;
  }
  if (Math.abs(d - R) < 0.06) {
    const rimLit = Math.max(0.15, (px / (d || 1)) * sx0 * 0.8 + 0.4);
    const rim = Math.exp(-((d - R) * (d - R)) / 0.00025) * rimLit;
    r += rim * 0.3;
    g += rim * 0.6;
    b += rim * 1.0;
  }
  drawRing(true);

  // Stars, fixed in the world.
  const sx = Math.floor(X * 28);
  const sy = Math.floor(Y * 28);
  const h = Math.sin(sx * 127.1 + sy * 311.7) * 43758.5453;
  const hf = h - Math.floor(h);
  if (hf > 0.965 && d > R && ringBand === 0) {
    const cx = (sx + 0.5) / 28 - X;
    const cy = (sy + 0.5) / 28 - Y;
    const tw = 0.6 + 0.4 * Math.sin(t * 2 + hf * 50);
    const s = Math.exp(-(cx * cx + cy * cy) / 0.000012) * tw * (1 - f.day * 0.7);
    r += s;
    g += s;
    b += s;
  }

  // Viewers' lights.
  for (const L of f.lights) {
    const lx = X - L.x;
    const ly = Y - L.y;
    const l2 = lx * lx + ly * ly;
    const glow = (Math.exp(-l2 / 0.00012) * 1.4 + Math.exp(-l2 / 0.004) * 0.35) * L.pulse;
    r += L.rgb[0] * glow;
    g += L.rgb[1] * glow;
    b += L.rgb[2] * glow;
  }

  // Viewers' words, written in the world and lit by the sun.
  for (const W of f.words) {
    const cell = WORD_HEIGHT / 7;
    const width = W.text.length * 6 - 1;
    const tx = (X - W.x) / cell + width / 2;
    const ty = (Y - W.y) / cell + 3.5;
    const c = textCoverage(W.text, tx, ty);
    if (c > 0) {
      const lum = 0.55 + 0.45 * f.day;
      r = r * (1 - c) + 1.0 * lum * c;
      g = g * (1 - c) + 0.93 * lum * c;
      b = b * (1 - c) + 0.78 * lum * c;
    }
  }

  // The UTC time, rendered from t, across the bottom of the whole image.
  {
    const cell = CLOCK_HEIGHT / 7;
    const width = f.clock.length * 6 - 1;
    const tx = ((u - 0.5) * f.aspect) / cell + width / 2;
    const ty = (v - CLOCK_V) / cell + 3.5;
    const c = textCoverage(f.clock, tx, ty);
    const shadow = textCoverage(f.clock, tx - 0.35, ty - 0.35);
    if (shadow > 0) {
      r *= 1 - 0.6 * shadow;
      g *= 1 - 0.6 * shadow;
      b *= 1 - 0.6 * shadow;
    }
    if (c > 0) {
      r = r * (1 - c) + 0.95 * c;
      g = g * (1 - c) + 0.98 * c;
      b = b * (1 - c) + 1.0 * c;
    }
  }

  out[0] = r < 0 ? 0 : r > 1 ? 1 : r;
  out[1] = g < 0 ? 0 : g > 1 ? 1 : g;
  out[2] = b < 0 ? 0 : b > 1 ? 1 : b;
  return out;
}

/** 8-bit sRGB-ish value the page should show at (u, v), for tests. */
export function expectedRgb(u: number, v: number, f: FrameCtx): [number, number, number] {
  const o = shade(u, v, f, [0, 0, 0]);
  return [Math.round(o[0]! * 255), Math.round(o[1]! * 255), Math.round(o[2]! * 255)];
}

export interface TileRect {
  x: number;
  y: number;
  w: number;
  h: number;
}

/**
 * Render a tile's rectangle at width x height into I420 (BT.709, limited range).
 * Pixel (i, j) samples the image at its centre.
 */
export function renderI420(rect: TileRect, width: number, height: number, f: FrameCtx): Uint8Array {
  const ySize = width * height;
  const cw = width >> 1;
  const ch = height >> 1;
  const out = new Uint8Array(ySize + 2 * cw * ch);
  const rgb = new Float32Array(ySize * 3);
  const px = new Float64Array(3);
  for (let j = 0; j < height; j++) {
    const v = rect.y + ((j + 0.5) / height) * rect.h;
    for (let i = 0; i < width; i++) {
      const u = rect.x + ((i + 0.5) / width) * rect.w;
      shade(u, v, f, px);
      const k = (j * width + i) * 3;
      rgb[k] = px[0]!;
      rgb[k + 1] = px[1]!;
      rgb[k + 2] = px[2]!;
      out[j * width + i] = Math.round(16 + 219 * (0.2126 * px[0]! + 0.7152 * px[1]! + 0.0722 * px[2]!));
    }
  }
  const uOff = ySize;
  const vOff = ySize + cw * ch;
  for (let j = 0; j < ch; j++) {
    for (let i = 0; i < cw; i++) {
      let R = 0;
      let G = 0;
      let B = 0;
      for (const [di, dj] of [[0, 0], [1, 0], [0, 1], [1, 1]] as const) {
        const k = ((2 * j + dj) * width + 2 * i + di) * 3;
        R += rgb[k]!;
        G += rgb[k + 1]!;
        B += rgb[k + 2]!;
      }
      R /= 4;
      G /= 4;
      B /= 4;
      const Yp = 0.2126 * R + 0.7152 * G + 0.0722 * B;
      out[uOff + j * cw + i] = Math.round(128 + (224 * (B - Yp)) / 1.8556);
      out[vOff + j * cw + i] = Math.round(128 + (224 * (R - Yp)) / 1.5748);
    }
  }
  return out;
}
