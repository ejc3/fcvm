// Minecraft mode with more servers than a page lists as tiles (PROTOCOL.md, "Many servers"): the
// field of every server, one cell each, from the hosts' status rows; which of them the six 3D
// tiles show; the headline's counts; and a reset of every server as it runs. Pure: the page
// passes the snapshot's field, the rows and the time in, and paints what comes back.

import type { Box } from "./layout.ts";
import { decodeBase64 } from "./minecraft.ts";
import { ProtocolError, type MinecraftField, type MinecraftStatusMsg } from "./protocol.ts";

/** Bytes of one server's status row. */
export const STATUS_ROW_BYTES = 8;
/** Rows older than this (by the page's clock) say nothing about a server now. */
export const STATUS_STALE_MS = 5000;
/** How many servers are drawn in 3D at once. */
export const FIELD_TILES = 6;
/** A 3D tile shows one server this long, then cuts to another. */
export const SAMPLE_MS = 12_000;
/** A server a viewer picked from the field keeps its 3D tile this long. */
export const PIN_MS = 60_000;
/** A cell flashes this long when its server comes back from a restore. */
export const FLASH_MS = 1500;

export type CellPhase = "none" | "restoring" | "forking" | "running" | "failed";
const PHASES: readonly CellPhase[] = ["none", "restoring", "forking", "running", "failed"];

/** One server of the field. Whatever its host did not read is null, and is drawn as nothing. */
export interface FieldCell {
  /** `mc-<n>`. */
  n: number;
  slot: string;
  /** The tile id the coordinator gives it: `mc:<k>`, k its place among all servers. */
  id: string;
  frame: string | null;
  frameV4: string | null;
  /** "none": its host has not reported it (no rows, rows too old, or a row that says so). */
  phase: CellPhase;
  tps: number | null;
  players: number | null;
  /** Seconds since this launch's first chunk. */
  ageS: number | null;
  /** The launch's milliseconds to its first chunk. */
  launchMs: number | null;
}

export type FieldStatus = MinecraftStatusMsg & { at: number };

const u16 = (b: Uint8Array, o: number): number | null => {
  const v = b[o]! | (b[o + 1]! << 8);
  return v === 0xffff ? null : v;
};

/** A host's rows as bytes, or null when they are not 8 bytes for each of `servers` (another launch of the host: nothing is drawn from them). */
export function statusRows(status: { rows: string }, servers: number): Uint8Array | null {
  let bytes: Uint8Array;
  try {
    bytes = decodeBase64(status.rows);
  } catch (e) {
    if (e instanceof ProtocolError) return null;
    throw e;
  }
  return bytes.length === servers * STATUS_ROW_BYTES ? bytes : null;
}

/** Every server of the field, by number, with what its host last read of it. */
export function fieldCells(field: MinecraftField, statuses: Readonly<Record<string, FieldStatus>>, now: number): FieldCell[] {
  const out: FieldCell[] = [];
  for (const h of field.hosts) {
    const st = statuses[h.key];
    const rows = st !== undefined && now - st.at <= STATUS_STALE_MS ? statusRows(st, h.servers.length) : null;
    h.servers.forEach((n, i) => {
      const o = i * STATUS_ROW_BYTES;
      const phase = rows === null ? "none" : (PHASES[rows[o]!] ?? "none");
      const known = rows !== null && phase !== "none";
      out.push({
        n,
        slot: `mc-${n}`,
        id: "",
        frame: h.frames[i] ?? null,
        frameV4: h.frames_v4[i] ?? null,
        phase,
        tps: known && rows[o + 1] !== 255 ? rows[o + 1]! / 10 : null,
        players: known && rows[o + 2] !== 255 ? rows[o + 2]! : null,
        ageS: known ? u16(rows, o + 4) : null,
        launchMs: known ? u16(rows, o + 6) : null,
      });
    });
  }
  out.sort((a, b) => a.n - b.n);
  out.forEach((c, k) => (c.id = `mc:${k + 1}`));
  return out;
}

// ---- layout ---------------------------------------------------------------------------------

export interface FieldLayout {
  /** A cell's side with its gap, CSS pixels. */
  cell: number;
  cols: number;
  rows: number;
  /** The grid's own size: taller than the area when it scrolls. */
  w: number;
  h: number;
  scrolls: boolean;
}

/** The smallest cell a finger or an eye can pick out; below it the field scrolls instead. */
export const FIELD_MIN_CELL = 9;
export const FIELD_MAX_CELL = 28;

/** The largest whole cell that fits `count` cells in `area`; when even the smallest does not fit, that one, and the field scrolls. */
export function fieldLayout(area: { w: number; h: number }, count: number): FieldLayout {
  const fit = (cell: number) => {
    const cols = Math.max(1, Math.floor(area.w / cell));
    const rows = Math.ceil(count / cols);
    return { cell, cols, rows, w: cols * cell, h: rows * cell, scrolls: rows * cell > area.h };
  };
  for (let cell = FIELD_MAX_CELL; cell > FIELD_MIN_CELL; cell--) {
    const l = fit(cell);
    if (!l.scrolls) return l;
  }
  return fit(FIELD_MIN_CELL);
}

/** The cell (its index in the field) at a point of the grid, or null between and beyond the cells. */
export function cellAt(l: FieldLayout, count: number, x: number, y: number): number | null {
  if (x < 0 || y < 0 || x >= l.w) return null;
  const i = Math.floor(y / l.cell) * l.cols + Math.floor(x / l.cell);
  return i < count ? i : null;
}

/** A cell's place in the grid. */
export function cellBox(l: FieldLayout, i: number): Box {
  return { x: (i % l.cols) * l.cell, y: Math.floor(i / l.cols) * l.cell, w: l.cell - 1, h: l.cell - 1 };
}

// ---- colour ---------------------------------------------------------------------------------

export type Rgb = readonly [number, number, number];
/** Not reported: an outline of this, and no fill. */
export const CELL_NONE: Rgb = [52, 58, 66];
export const CELL_RESTORING: Rgb = [124, 196, 255];
export const CELL_FORKING: Rgb = [255, 237, 140];
export const CELL_FAILED: Rgb = [255, 122, 107];
/** Running at 20 ticks a second, and running slow. */
export const CELL_RUNNING: Rgb = [84, 176, 96];
export const CELL_SLOW: Rgb = [214, 148, 52];
/** A running server whose tile did not say its tick rate. */
export const CELL_SILENT: Rgb = [58, 96, 66];

/** A running server keeps up at this tick rate or above. */
export const HOLD_TPS = 19.5;

/** A cell's colour: its state, and for a running server how its tick rate stands. */
export function cellColor(c: FieldCell): Rgb {
  switch (c.phase) {
    case "restoring":
      return CELL_RESTORING;
    case "forking":
      return CELL_FORKING;
    case "failed":
      return CELL_FAILED;
    case "running": {
      if (c.tps === null) return CELL_SILENT;
      if (c.tps >= HOLD_TPS) return CELL_RUNNING;
      const t = Math.max(0, Math.min(1, c.tps / HOLD_TPS));
      return [0, 1, 2].map((k) => Math.round(CELL_FAILED[k]! + (CELL_SLOW[k]! - CELL_FAILED[k]!) * t)) as unknown as Rgb;
    }
    default:
      return CELL_NONE;
  }
}

/** A running server's cell is lighter for this long after its launch's first chunk, fading as it ages: a reset of many servers shows as a wave. */
export const YOUNG_S = 60;
export const YOUNG_LIFT = 0.45;

/** How far toward white a cell is lifted for its server's age, 0 to YOUNG_LIFT. An age that was not read lifts nothing. */
export function youngLift(c: FieldCell): number {
  if (c.phase !== "running" || c.ageS === null || c.ageS >= YOUNG_S) return 0;
  return YOUNG_LIFT * (1 - c.ageS / YOUNG_S);
}

/** What a cell says when it is pointed at: "mc-217 · running · 20.0 TPS · 3 players · up 4 min 12 s · restored in 0.41 s". */
export function cellText(c: FieldCell): string {
  if (c.phase === "none") return `${c.slot} · not reported`;
  const age = c.ageS === null ? null : c.ageS < 60 ? `up ${c.ageS} s` : `up ${Math.floor(c.ageS / 60)} min ${c.ageS % 60} s`;
  return [
    c.slot,
    c.phase,
    c.tps === null ? null : `${c.tps.toFixed(1)} TPS`,
    c.players === null ? null : `${c.players} player${c.players === 1 ? "" : "s"}`,
    c.phase === "running" ? age : null,
    c.launchMs === null ? null : `restored in ${(c.launchMs / 1000).toFixed(2)} s`,
  ]
    .filter((x) => x !== null)
    .join(" · ");
}

// ---- which servers are drawn ----------------------------------------------------------------

function mix(a: number, b: number): number {
  let h = Math.imul(a ^ 0x9e3779b9, 0x85ebca6b) ^ Math.imul(b + 0x7f4a7c15, 0xc2b2ae35);
  h ^= h >>> 15;
  h = Math.imul(h, 0x2c1b3c6d);
  return (h ^ (h >>> 13)) >>> 0;
}

/**
 * The servers (by number) the `k` 3D tiles show at `timeMs`, in tile order. A server a viewer
 * picked keeps a tile for PIN_MS, the newest pick first. Each other tile holds a running server
 * for SAMPLE_MS and then cuts to another, the tiles cutting one after another, never two tiles
 * on one server. Fewer running servers than tiles: the rest show the first servers that are not.
 */
export function sampleOf(cells: readonly FieldCell[], k: number, timeMs: number, pins: readonly { n: number; at: number }[] = []): number[] {
  const known = new Set(cells.map((c) => c.n));
  const out: number[] = [];
  for (const p of [...pins].sort((a, b) => b.at - a.at)) {
    if (out.length < k && timeMs - p.at < PIN_MS && known.has(p.n) && !out.includes(p.n)) out.push(p.n);
  }
  const pinned = out.length;
  const running = cells.filter((c) => c.phase === "running").map((c) => c.n);
  for (let j = pinned; j < k && running.length > 0; j++) {
    // each tile cuts at its own moment
    const epoch = Math.floor((timeMs + (j * SAMPLE_MS) / k) / SAMPLE_MS);
    let at = mix(epoch, j) % running.length;
    let tries = 0;
    while (out.includes(running[at]!) && tries++ < running.length) at = (at + 1) % running.length;
    if (!out.includes(running[at]!)) out.push(running[at]!);
  }
  for (const c of cells) {
    if (out.length >= k) break;
    if (!out.includes(c.n)) out.push(c.n);
  }
  return out;
}

// ---- the headline ---------------------------------------------------------------------------

export interface FieldHeadline {
  /** "512 servers on one host" / "512 servers on 2 hosts". */
  servers: string;
  /** "510 running", with what is not: "498 running · 12 restoring · 2 not reported". */
  states: string;
  /** "5.2 per busy CPU (98.7 of 192 busy)"; null until every host has said its CPUs. */
  perCpu: string | null;
  /** "1.4 per GiB in use (362 GiB)"; null until every host has said its memory. */
  perGib: string | null;
  /** "20.0 TPS median, 19.8 lowest"; null with no tick rate read. */
  tps: string | null;
}

export function fieldHeadline(field: MinecraftField, cells: readonly FieldCell[], statuses: Readonly<Record<string, FieldStatus>>, now: number): FieldHeadline {
  const count = (p: CellPhase) => cells.filter((c) => c.phase === p).length;
  const running = count("running");
  const states = [`${running} running`, ...(["restoring", "forking", "failed"] as const).map((p) => (count(p) > 0 ? `${count(p)} ${p}` : null)), count("none") > 0 ? `${count("none")} not reported` : null]
    .filter((x) => x !== null)
    .join(" · ");
  const fresh = field.hosts.map((h) => statuses[h.key]).filter((s): s is FieldStatus => s !== undefined && now - s.at <= STATUS_STALE_MS);
  const all = fresh.length === field.hosts.length && fresh.length > 0;
  const sum = (k: "busy" | "cpus" | "mem_used_mib") => (all && fresh.every((s) => s[k] !== null) ? fresh.reduce((a, s) => a + s[k]!, 0) : null);
  const busy = sum("busy");
  const cpus = sum("cpus");
  const usedGib = sum("mem_used_mib") === null ? null : sum("mem_used_mib")! / 1024;
  const tps = cells.filter((c) => c.tps !== null).map((c) => c.tps!).sort((a, b) => a - b);
  return {
    servers: `${cells.length} server${cells.length === 1 ? "" : "s"} on ${field.hosts.length === 1 ? "one host" : `${field.hosts.length} hosts`}`,
    states,
    perCpu: busy === null || cpus === null || busy <= 0 || running === 0 ? null : `${(running / busy).toFixed(1)} per busy CPU (${busy.toFixed(1)} of ${cpus} busy)`,
    perGib: usedGib === null || usedGib <= 0 || running === 0 ? null : `${(running / usedGib).toFixed(1)} per GiB in use (${Math.round(usedGib)} GiB)`,
    tps: tps.length === 0 ? null : `${tps[tps.length >> 1]!.toFixed(1)} TPS median, ${tps[0]!.toFixed(1)} lowest`,
  };
}

// ---- a reset of every server ----------------------------------------------------------------

export interface ResetWave {
  /** Servers back from this reset: running, with a launch younger than the click. */
  back: number;
  total: number;
  /** "Reset all: 312 of 512 back · 14.2 a second · 22.0 s" while it runs; "Reset all: 512 servers back in 36.2 s (14.1 a second)" once done. */
  text: string;
  done: boolean;
}

/**
 * A reset of every server, from the click (`clickedAt`, the coordinator's clock, as `srvNow` is)
 * while it runs and for a minute after its last server is back. `doneAt` is when the page first
 * saw every server back (it keeps that between calls); null until then.
 */
export function resetWave(cells: readonly FieldCell[], clickedAt: number, srvNow: number, doneAt: number | null): ResetWave | null {
  const elapsed = srvNow - clickedAt;
  if (elapsed < 0 || cells.length === 0) return null;
  // back: its launch's first chunk came after the click (a second's slack for the row's whole seconds)
  const back = cells.filter((c) => c.phase === "running" && c.ageS !== null && c.ageS * 1000 <= elapsed + 1000).length;
  const total = cells.length;
  const done = doneAt !== null || back >= total;
  const took = ((doneAt ?? srvNow) - clickedAt) / 1000;
  if (done && srvNow - (doneAt ?? srvNow) > 60_000) return null;
  const rate = took > 0 ? (done ? total : back) / took : 0;
  const text = done
    ? `Reset all: ${total} servers back in ${took.toFixed(1)} s (${rate.toFixed(1)} a second)`
    : `Reset all: ${back} of ${total} back · ${rate.toFixed(1)} a second · ${took.toFixed(1)} s`;
  return { back, total, text, done };
}
