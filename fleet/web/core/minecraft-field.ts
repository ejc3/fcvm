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
/** A server that came back from a restore stays on its tile at least this long after, seconds. */
export const BACK_HOLD_S = 4;
/** A cell flashes this long when its server comes back from a restore. */
export const FLASH_MS = 1500;

export type CellPhase = "none" | "restoring" | "forking" | "running" | "failed";
const PHASES: readonly CellPhase[] = ["none", "restoring", "forking", "running", "failed"];

/** One server of the field. Whatever its host did not read is null, and is drawn as nothing. */
export interface FieldCell {
  /** `mc-<n>`. */
  n: number;
  slot: string;
  /**
   * The page's key for its tile, sockets and world: the id the coordinator gives it (`mc:<k>`, k
   * its place among all servers) when the page can tell, else `field:<n>`, which is no
   * coordinator's id. Never sent: an action names its server by `slot`.
   */
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
  /** When the coordinator received the row this was read from (its clock, ms); null from a coordinator that does not say. */
  seenAt: number | null;
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

/**
 * Every server of the field, by number, with what its host last read of it. `listed` is the
 * snapshot's tiles (id and server name). The coordinator's id of a server is its place among
 * all servers, which the page can work out only when the field names every server and the
 * listed tiles agree with that count. When it cannot (a server with no host yet is in the count
 * and in no host's list), a server that is not a listed tile gets a key of the page's own, so
 * it never shares one with another server's listed tile.
 */
export function fieldCells(field: MinecraftField, statuses: Readonly<Record<string, FieldStatus>>, now: number, listed: readonly { id: string; slot: string | null }[] = []): FieldCell[] {
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
        seenAt: known ? (st!.received_at ?? null) : null,
      });
    });
  }
  out.sort((a, b) => a.n - b.n);
  const ids = new Map(listed.filter((t) => t.slot !== null).map((t) => [t.slot!, t.id]));
  const ranked = field.servers === out.length && out.every((c, k) => (ids.get(c.slot) ?? `mc:${k + 1}`) === `mc:${k + 1}`);
  out.forEach((c, k) => (c.id = ids.get(c.slot) ?? (ranked ? `mc:${k + 1}` : `field:${c.n}`)));
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

/**
 * The cursor after an arrow key: one cell over, or one row up or down in the same column. A
 * move off the field (up from the first row, down past the last cell, left of the first cell,
 * right of the last) stays where it is. With no cursor yet, any arrow starts at the first cell.
 */
export function moveCursor(l: FieldLayout, count: number, cursor: number | null, key: "ArrowLeft" | "ArrowRight" | "ArrowUp" | "ArrowDown"): number | null {
  if (count <= 0) return null;
  if (cursor === null) return 0;
  const to = cursor + { ArrowLeft: -1, ArrowRight: 1, ArrowUp: -l.cols, ArrowDown: l.cols }[key];
  return to >= 0 && to < count ? to : cursor;
}

/** How far a scrolling field must be scrolled (from `top`, showing `view` pixels) for cell `i` to be whole in view. */
export function scrollTo(l: FieldLayout, i: number, top: number, view: number): number {
  const b = cellBox(l, i);
  if (b.y < top) return b.y;
  if (b.y + l.cell > top + view) return b.y + l.cell - view;
  return top;
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

/** What the field's marks mean, in the order they are explained: a colour (null: an outline only) and its word. */
export const FIELD_LEGEND: readonly { key: string; rgb: Rgb | null; text: string }[] = [
  { key: "running", rgb: CELL_RUNNING, text: "20 TPS" },
  { key: "slow", rgb: CELL_SLOW, text: "slower" },
  { key: "restoring", rgb: CELL_RESTORING, text: "restoring" },
  { key: "forking", rgb: CELL_FORKING, text: "forking" },
  { key: "failed", rgb: CELL_FAILED, text: "failed" },
  { key: "none", rgb: null, text: "not reported" },
];
/** The two marks that are not a state. */
export const FIELD_LEGEND_RING = "ringed: in 3D";
export const FIELD_LEGEND_YOUNG = "lighter: just up";

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

/**
 * What a cell says when it is pointed at: "mc-217 · running · 20.0 TPS · 3 players · up 4 min 12 s
 * · restored in 0.41 s"; `short` (a phone's row, between its buttons): "mc-217 · running · 20.0 TPS".
 */
export function cellText(c: FieldCell, short = false): string {
  if (c.phase === "none") return `${c.slot} · not reported`;
  if (short) return [c.slot, c.phase, c.tps === null ? null : `${c.tps.toFixed(1)} TPS`].filter((x) => x !== null).join(" · ");
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

/** One 3D tile's server, and since when (the page's clock, ms). `pin`: a viewer's pick put it there. */
export interface SampleSlot {
  n: number;
  since: number;
  pin?: boolean;
}

export interface SampleOpts {
  /** Servers the viewer picked from the field, with when: each keeps a tile for PIN_MS. */
  pins?: readonly { n: number; at: number }[];
  /** The server opened to the whole stage: it keeps its tile until the viewer closes it. */
  held?: number | null;
  /** Reduced motion: no tile cuts by itself. */
  still?: boolean;
}

/** A server whose restore is on its tile stays there until it is back and has been seen back. */
function staying(c: FieldCell | undefined): boolean {
  return c !== undefined && (c.phase === "restoring" || c.phase === "forking" || (c.phase === "running" && c.ageS !== null && c.ageS < BACK_HOLD_S));
}

/**
 * The servers the `k` 3D tiles show at `timeMs`, tile by tile, from what they showed before
 * (`prev`; empty at first). A tile keeps its server for SAMPLE_MS whatever any other server
 * does, and past that while its server is being restored and until it has been back for
 * BACK_HOLD_S; then it cuts to a running server no tile shows, chosen by a hash of the tile, the
 * time and the server's number (never by a place in a list, which every reset would shift). One
 * tile cuts at a time. A server a viewer picked takes the tile whose turn is nearest and keeps
 * it for PIN_MS or until it is unpinned; the opened server keeps its tile until it is closed.
 * A tile whose server left the field takes another at once. Fewer running servers than tiles:
 * the rest show servers that are not.
 */
export function stepSample(prev: readonly SampleSlot[], cells: readonly FieldCell[], k: number, timeMs: number, opts: SampleOpts = {}): SampleSlot[] {
  const byN = new Map(cells.map((c) => [c.n, c]));
  const size = Math.min(k, cells.length);
  const held = opts.held !== null && opts.held !== undefined && byN.has(opts.held) ? opts.held : null;
  const pins: number[] = [];
  for (const p of [...(opts.pins ?? [])].sort((a, b) => b.at - a.at)) {
    if (pins.length < size && timeMs - p.at < PIN_MS && byN.has(p.n) && !pins.includes(p.n)) pins.push(p.n);
  }
  const first = prev.length === 0;
  const slots: (SampleSlot | null)[] = [];
  for (let j = 0; j < size; j++) {
    const s = prev[j];
    slots.push(s !== undefined && byN.has(s.n) && !slots.some((x) => x?.n === s.n) ? { ...s } : null);
  }
  // Whose turn is over. A pick that ran out or was taken back gives its tile up whatever the motion.
  const due: boolean[] = slots.map((s) => {
    if (s === null || s.n === held || pins.includes(s.n) || staying(byN.get(s.n))) return false;
    if (s.pin === true) return true;
    return opts.still !== true && timeMs - s.since >= SAMPLE_MS;
  });
  for (const s of slots) {
    if (s === null) continue;
    if (s.n === held) {
      // opened: its turn starts over for as long as it is open, and it is no pick's any more
      s.since = timeMs;
      delete s.pin;
    }
    if (pins.includes(s.n)) s.pin = true;
  }
  // A picked or opened server that no tile shows takes one: an empty tile, else one whose turn is
  // over, else the one that has had the longest turn.
  const fixed = (s: SampleSlot | null) => s !== null && (s.n === held || pins.includes(s.n));
  for (const n of [...(held === null ? [] : [held]), ...pins]) {
    if (slots.some((s) => s?.n === n)) continue;
    let j = slots.findIndex((s) => s === null);
    if (j < 0) j = due.indexOf(true);
    if (j < 0) {
      const free = slots.map((s, i) => ({ s, i })).filter((x) => !fixed(x.s));
      if (free.length === 0) continue;
      j = free.reduce((a, b) => (b.s!.since < a.s!.since ? b : a)).i;
    }
    slots[j] = { n, since: timeMs, ...(n === held ? {} : { pin: true }) };
    due[j] = false;
  }
  const shown = () => new Set(slots.filter((s) => s !== null).map((s) => s.n));
  const pick = (j: number, not: number | null): number | null => {
    const taken = shown();
    let best: number | null = null;
    let score = -1;
    for (const c of cells) {
      if (c.phase !== "running" || taken.has(c.n) || c.n === not) continue;
      const h = mix(mix(Math.floor(timeMs), j + 1), c.n);
      if (h > score) [best, score] = [c.n, h];
    }
    return best;
  };
  // Empty tiles: a running server, else one being restored, else whatever is left, in order.
  for (let j = 0; j < size; j++) {
    if (slots[j] !== null) continue;
    const taken = shown();
    const n = pick(j, null) ?? cells.find((c) => !taken.has(c.n) && (c.phase === "restoring" || c.phase === "forking"))?.n ?? cells.find((c) => !taken.has(c.n))?.n;
    if (n === undefined) continue;
    // at first the tiles' turns are spread, so they cut one after another
    slots[j] = { n, since: first ? timeMs - (j * SAMPLE_MS) / size : timeMs };
  }
  // One tile whose turn is over cuts, the longest-shown first, and never two within a moment.
  const last = Math.max(...slots.map((s) => (s === null ? Number.NEGATIVE_INFINITY : s.since)));
  if (first || timeMs - last >= SAMPLE_MS / size / 2) {
    const order = due.map((d, j) => ({ d, j })).filter((x) => x.d).sort((a, b) => slots[a.j]!.since - slots[b.j]!.since);
    for (const { j } of order) {
      const n = pick(j, slots[j]!.n);
      if (n === null) continue;
      slots[j] = { n, since: timeMs };
      break;
    }
  }
  return slots.filter((s) => s !== null);
}

// ---- the headline ---------------------------------------------------------------------------

export interface FieldHeadline {
  /** "512 servers on one host" / "512 servers on 2 hosts". */
  servers: string;
  /** "510 running", with what is not: "498 running · 12 restoring · 2 not reported"; with no server reported, "not reported yet" or "no report for 8 s". */
  states: string;
  /** "5.2 per busy CPU (98.7 of 192 busy)"; null until every host has said its CPUs. */
  perCpu: string | null;
  /** "1.4 per GiB in use (362 GiB)"; null until every host has said its memory. */
  perGib: string | null;
  /** "20.0 TPS median, 19.8 lowest"; null with no tick rate read. */
  tps: string | null;
}

/**
 * What to say in place of counts when no server of the field is reported: "not reported yet"
 * before any host's rows came, "no report for 8 s" once they stopped. Null when any server is
 * reported.
 */
export function unreported(field: MinecraftField, cells: readonly FieldCell[], statuses: Readonly<Record<string, FieldStatus>>, now: number): string | null {
  if (cells.some((c) => c.phase !== "none")) return null;
  const last = Math.max(...field.hosts.map((h) => statuses[h.key]?.at ?? Number.NEGATIVE_INFINITY));
  return Number.isFinite(last) && now - last > STATUS_STALE_MS ? `no report for ${Math.floor((now - last) / 1000)} s` : "not reported yet";
}

export function fieldHeadline(field: MinecraftField, cells: readonly FieldCell[], statuses: Readonly<Record<string, FieldStatus>>, now: number): FieldHeadline {
  const count = (p: CellPhase) => cells.filter((c) => c.phase === p).length;
  const running = count("running");
  // With no server reported there is no count to give: a zero would be a reading nobody took.
  const silent = unreported(field, cells, statuses, now);
  const states =
    silent ??
    [`${running} running`, ...(["restoring", "forking", "failed"] as const).map((p) => (count(p) > 0 ? `${count(p)} ${p}` : null)), count("none") > 0 ? `${count("none")} not reported` : null]
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
  /** Servers back from this reset: running, on a launch whose first chunk came after the click. */
  back: number;
  /** The servers the reset was sent to, as the coordinator published it. */
  total: number;
  /**
   * "Reset all: 312 of 512 back · 14.2 a second · 22.0 s" while it runs; "Reset all: 512 servers
   * back in 36 s (14.2 a second)" once done; "Reset all: 510 of 512 back after 36 s" when the
   * rest are not coming.
   */
  text: string;
  done: boolean;
  /** Over: done, or the rest are not coming. Its time then stands (`tookS`), and the page hands it back. */
  settled: boolean;
  /** Seconds from the click to the last server back, once settled; null when the rows cannot say. */
  tookS: number | null;
}

/** With no server being restored and none back for this long, the rest of a reset are not coming, seconds. */
export const WAVE_STALL_S = 10;
/** A reset's line stays this long after its last server came back, seconds. */
export const WAVE_SHOWN_S = 60;

/**
 * A reset of every server (`reset`: the snapshot's `last_reset_all`; `srvNow` on the
 * coordinator's clock, as its `clicked_at` is), while it runs and for a minute after its last
 * server is back. Everything is read from the rows, so a page opened late says the same as one
 * that watched: a server is back when its launch's first chunk came after the click (the time
 * its row came, less its age), and the reset took until the last of those. The rows give whole
 * seconds, so the finished time is in whole seconds; from a coordinator that does not say when a
 * row came, no time is given. A row's age is cut to a whole second at a moment that moves a
 * little from report to report, so the same reset can read 21 s in one report and 22 s in the
 * next: `stood` is the time this page already showed for this reset once it was over, and it
 * stands.
 */
export function resetWave(cells: readonly FieldCell[], reset: { clicked_at: number; servers: number }, srvNow: number, stood: number | null = null): ResetWave | null {
  const elapsed = srvNow - reset.clicked_at;
  const total = reset.servers;
  if (elapsed < 0 || total <= 0) return null;
  const firstChunkAt = (c: FieldCell) => (c.seenAt ?? srvNow) - c.ageS! * 1000;
  const fresh = cells.filter((c) => c.phase === "running" && c.ageS !== null && firstChunkAt(c) >= reset.clicked_at);
  const back = Math.min(total, fresh.length);
  const done = back >= total;
  const timed = fresh.length > 0 && fresh.every((c) => c.seenAt !== null);
  // the last one back: when, and how long ago by its own age
  const read = timed ? Math.max(1, Math.round((Math.max(...fresh.map(firstChunkAt)) - reset.clicked_at) / 1000)) : null;
  const sinceLastS = fresh.length === 0 ? null : Math.min(...fresh.map((c) => c.ageS!));
  const inFlight = cells.some((c) => c.phase === "restoring" || c.phase === "forking");
  const stalled = !done && !inFlight && (sinceLastS === null ? elapsed >= WAVE_SHOWN_S * 1000 : sinceLastS >= WAVE_STALL_S);
  if ((done || stalled) && (sinceLastS === null || sinceLastS > WAVE_SHOWN_S)) return null;
  const settled = done || stalled;
  const tookS = settled ? (stood ?? read) : read;
  let text: string;
  if (done) text = tookS === null ? `Reset all: ${total} servers back` : `Reset all: ${total} servers back in ${tookS} s (${(total / tookS).toFixed(1)} a second)`;
  else if (stalled) text = `Reset all: ${back} of ${total} back${tookS === null ? "" : ` after ${tookS} s`}`;
  else text = `Reset all: ${back} of ${total} back · ${(elapsed > 0 ? back / (elapsed / 1000) : 0).toFixed(1)} a second · ${(elapsed / 1000).toFixed(1)} s`;
  return { back, total, text, done, settled, tookS: settled ? tookS : null };
}
