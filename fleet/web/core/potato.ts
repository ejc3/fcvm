// Potato mode (`?mode=potato`): a ring of CPU-only player VMs passing a ball, the pace, the
// ladder of ring sizes, the last drop and the records (docs/fleet/ARCHITECTURE.md, "Potato
// mode"; PROTOCOL.md, "Potato mode"). Pure: the page measures the stage, passes the time in
// and draws what comes back. The coordinator is never drawn.

import { landingView, type LandingView } from "./landing.ts";
import type { Box } from "./layout.ts";
import { serverNow } from "./clock.ts";
import { sameViewer, type FleetState, type IntentName, type Potato, type PotatoDrop, type PotatoPlayer, type PotatoRound } from "./protocol.ts";
import type { ClientState, Notice } from "./store.ts";
import { sleepView, verifyView, type SleepView } from "./view.ts";

export const POTATO_TITLE = "Hot potato";
export const NO_POTATO = "No host has run potato players yet.";
export const NO_HOST = "No host is running players now. The ladder and the records are kept.";

/** The coordinator's rate limits (fleet/core/src/potato.rs, `PotatoConfig`). */
export const FASTER_INTERVAL_MS = 2000;
export const RESTART_INTERVAL_MS = 10_000;

/** Tiles are at most this wide, CSS px, and the ring leaves this much room round it for labels and the ball's arc. */
export const MAX_TILE = 96;
export const RING_MARGIN = 28;
/** A tile's width over the chord between neighbours: under 1/sqrt(2), so squares at 45 degrees do not touch. */
const TILE_SHARE = 0.66;
/** Tiles at least this wide carry their player number inside, and the line under them. */
export const NUMBER_MIN_TILE = 28;
export const LINE_MIN_TILE = 56;
/** The ball crosses in about a millisecond; it is drawn arcing over half the hold, at most this long. */
export const ARC_MAX_MS = 300;
const ARC_MIN_MS = 60;

const ms = (v: number) => `${Math.round(v)} ms`;
const tenthsMs = (v: number) => `${Math.round(v * 10) / 10} ms`;
const seconds = (v: number) => `${Math.max(0, Math.ceil(v / 1000))} s`;
const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

// ---- colours ---------------------------------------------------------------------

/** A colour index's hue, degrees: `colours` hues spaced evenly round the wheel, red first. */
export function potatoHue(colour: number, colours: number): number {
  const n = Math.max(1, Math.floor(colours));
  const k = ((Math.floor(colour) % n) + n) % n;
  return Math.round((k * 360) / n);
}

/** A colour index as CSS. The same in light and dark themes: it is the player's state, not decoration. */
export function potatoColour(colour: number, colours: number): string {
  return `hsl(${potatoHue(colour, colours)} 72% 52%)`;
}

// ---- the ring --------------------------------------------------------------------

export interface Point {
  x: number;
  y: number;
}

export interface RingGeometry {
  centre: Point;
  radius: number;
  /** Each tile's width and height, CSS px. */
  tile: number;
  /** Every place's centre, in ring order, the first at the top and the rest clockwise. */
  places: Point[];
}

/** N places on a circle inside `area`, with tiles as large as fit without touching (at most MAX_TILE). */
export function ringGeometry(area: Box, n: number): RingGeometry {
  const centre = { x: area.x + area.w / 2, y: area.y + area.h / 2 };
  const outer = Math.max(0, Math.min(area.w, area.h) / 2 - RING_MARGIN);
  const sin = n > 1 ? Math.sin(Math.PI / n) : 1;
  // the chord between neighbours is 2 r sin(pi/n), and the ring's outer edge is r + tile / 2
  const fit = (2 * TILE_SHARE * sin * outer) / (1 + TILE_SHARE * sin);
  const tile = Math.max(0, Math.min(MAX_TILE, outer * 0.5, fit));
  const radius = n > 1 ? Math.max(0, outer - tile / 2) : 0;
  const places: Point[] = [];
  for (let i = 0; i < n; i++) {
    const a = -Math.PI / 2 + (2 * Math.PI * i) / n;
    places.push({ x: centre.x + radius * Math.cos(a), y: centre.y + radius * Math.sin(a) });
  }
  return { centre, radius, tile, places };
}

export type TilePhase = "awake" | "sleeping" | "asleep" | "waking" | "failed" | "absent";

export interface RingTile {
  player: number;
  /** Where it is drawn, CSS px. */
  box: Box;
  colour: string;
  phase: TilePhase;
  /** Inside the tile, when it is wide enough (NUMBER_MIN_TILE). */
  number: string | null;
  /** Under the tile, when it is wide enough (LINE_MIN_TILE). */
  line: string | null;
  /** While it wakes: how far, against its last wake, and the milliseconds so far. */
  wake: { fraction: number; text: string } | null;
  holder: boolean;
  /** On a drop, the catcher that was not ready: "84 ms late", "waited 131 ms for a CPU", "still saving", "proof failed". */
  late: string | null;
  /** What hovering, focusing or long-pressing the tile shows; the aria label is these two joined. */
  readout: { head: string; detail: string };
  ariaLabel: string;
}

export interface BallView {
  /** Its centre, CSS px. */
  at: Point;
  size: number;
  /** Arcing between two tiles; false while it rests on the holder, or lies where it dropped. */
  flying: boolean;
  dropped: boolean;
}

export interface RingView {
  geometry: RingGeometry;
  tiles: RingTile[];
  ball: BallView | null;
  /** One line over the ring. */
  caption: string;
  ariaLabel: string;
  /** From the ball to the furthest machine being woken, drawn inside the ring. */
  arc: { centre: Point; radius: number; from: Point; to: Point; large: boolean; labelAt: Point; label: string; ahead: number } | null;
  /** This round just set the ring's record: the track flashes once. */
  flash: boolean;
}

function playerOf(p: Potato, n: number): PotatoPlayer | null {
  return p.players.find((x) => x.player === n) ?? null;
}

function tileOf(p: Potato, n: number, box: Box, now: number, holder: boolean): RingTile {
  const pl = playerOf(p, n);
  const phase: TilePhase =
    pl === null
      ? "absent"
      : pl.phase === "sleeping" || pl.phase === "asleep" || pl.phase === "waking" || pl.phase === "failed"
        ? pl.phase
        : "awake";
  let wake: RingTile["wake"] = null;
  let line: string;
  let state: string;
  switch (phase) {
    case "absent":
      line = "not started";
      state = "not started";
      break;
    case "sleeping":
      line = "going to sleep";
      state = `going to sleep ${ms(Math.max(0, now - pl!.since))}`;
      break;
    case "asleep":
      line = "asleep";
      state = `asleep ${ms(Math.max(0, now - pl!.since))}`;
      break;
    case "waking": {
      const el = Math.max(0, now - pl!.since);
      const last = pl!.last_wake_ms;
      wake = { fraction: last !== null && last > 0 ? Math.min(1, el / last) : 0, text: ms(el) };
      line = `waking ${ms(el)}`;
      state = `waking ${ms(el)}`;
      break;
    }
    case "failed":
      line = "failed";
      state = "failed";
      break;
    default:
      line = holder ? "has the ball" : "awake";
      state = holder ? "has the ball" : "awake";
  }
  const readout = tileReadout(n, state, phase !== "awake" && holder, pl);
  return {
    player: n,
    box,
    colour: potatoColour(pl?.colour ?? 0, p.colours),
    phase,
    number: box.w >= NUMBER_MIN_TILE ? String(n) : null,
    line: box.w >= LINE_MIN_TILE ? line : null,
    wake,
    holder,
    late: null,
    readout,
    ariaLabel: `${readout.head}. ${readout.detail}`,
  };
}

/** The tile's readout: "Player 7 · asleep 312 ms", then its catches and last sleep and wake. The one source of the tile's text on hover and for screen readers. */
export function tileReadout(n: number, state: string, holdsAsWell: boolean, pl: PotatoPlayer | null): { head: string; detail: string } {
  const head = `Player ${n} · ${state}${holdsAsWell ? " · has the ball" : ""}`;
  if (pl === null) return { head, detail: "" };
  const parts = [plural(pl.counter, "catch", "catches")];
  if (pl.last_sleep_ms !== null) parts.push(`last sleep ${ms(pl.last_sleep_ms)}`);
  if (pl.last_wake_ms !== null) parts.push(`last wake ${ms(pl.last_wake_ms)}`);
  return { head, detail: parts.join(" · ") };
}

/** The ball's size, CSS px: a share of a tile, at least 12 px so it is seen on a ring of 96. */
export function ballSize(g: RingGeometry): number {
  return Math.max(12, Math.min(28, g.tile * 0.4));
}

/** Where the ball sits on a place's tile: its upper right corner, clear of the player's number. */
export function ballAnchor(g: RingGeometry, i: number): Point {
  const p = g.places[i]!;
  return { x: p.x + g.tile * 0.3, y: p.y - g.tile * 0.3 };
}

/** How long the ball is drawn arcing: half the hold, between 60 and 300 ms. */
export function arcMs(holdMs: number): number {
  return Math.min(ARC_MAX_MS, Math.max(ARC_MIN_MS, holdMs / 2));
}

/** A point on the throw from `a` to `b` at `t` (0 to 1): a curve bowed away from the ring's centre. */
export function arcPoint(a: Point, b: Point, centre: Point, t: number): Point {
  const k = Math.min(1, Math.max(0, t));
  const mid = { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
  const dx = b.x - a.x;
  const dy = b.y - a.y;
  const len = Math.hypot(dx, dy);
  if (len === 0) return { ...a };
  // the normal that points away from the centre; for a throw across the centre, the one to the thrower's left
  let nx = -dy / len;
  let ny = dx / len;
  const ox = mid.x - centre.x;
  const oy = mid.y - centre.y;
  if (Math.hypot(ox, oy) > 1e-6 && nx * ox + ny * oy < 0) {
    nx = -nx;
    ny = -ny;
  }
  const lift = 0.35 * len;
  const c = { x: mid.x + nx * lift, y: mid.y + ny * lift };
  const u = 1 - k;
  return { x: u * u * a.x + 2 * u * k * c.x + k * k * b.x, y: u * u * a.y + 2 * u * k * c.y + k * k * b.y };
}

/**
 * Where the ball is: arcing from `flight.from` to `flight.to` from `flight.at`; after a catch,
 * arcing from the holder's neighbour before it (the thrower) from `last_catch_at`, since the
 * throw and the catch often reach the page in one snapshot; then resting on the holder.
 * Null before the round's first catch.
 */
export function ballOf(r: PotatoRound, g: RingGeometry, now: number, reducedMotion: boolean): BallView | null {
  const place = (player: number) => {
    const i = r.ring.indexOf(player);
    return i < 0 ? null : ballAnchor(g, i);
  };
  const size = ballSize(g);
  const arc = arcMs(r.hold_ms);
  const throwOf = (from: number, to: number, at: number): BallView | null => {
    const a = place(from);
    const b = place(to);
    if (a === null || b === null) return null;
    const t = reducedMotion ? 1 : (now - at) / arc;
    return { at: arcPoint(a, b, g.centre, t), size, flying: t < 1, dropped: false };
  };
  if (r.flight) {
    const b = throwOf(r.flight.from, r.flight.to, r.flight.at);
    if (b) return b;
  }
  if (r.holder === null) return null;
  const i = r.ring.indexOf(r.holder);
  if (i < 0) return null;
  if (r.seq !== null && r.seq > 0 && r.last_catch_at !== null && r.ring.length > 1) {
    const from = r.ring[(i - 1 + r.ring.length) % r.ring.length]!;
    const b = throwOf(from, r.holder, r.last_catch_at);
    if (b) return b;
  }
  return { at: ballAnchor(g, i), size, flying: false, dropped: false };
}

function ringOf(p: Potato, area: Box | null, now: number, reducedMotion: boolean): RingView | null {
  const r = p.round;
  const drop = r === null && p.next_round_at !== null ? p.last_drop : null;
  // The ring being played; after a drop, the ring that dropped it until the next starts; else the next ring.
  const players = r ? r.ring : drop ? Array.from({ length: drop.size }, (_, i) => i) : p.size !== null && p.capacity > 0 ? Array.from({ length: p.size }, (_, i) => i) : [];
  if (players.length === 0 || area === null) return null;
  const g = ringGeometry(area, players.length);
  const holder = r?.holder ?? null;
  const tiles = players.map((n, i) => {
    const c = g.places[i]!;
    return tileOf(p, n, { x: c.x - g.tile / 2, y: c.y - g.tile / 2, w: g.tile, h: g.tile }, now, n === holder);
  });
  let ball: BallView | null = null;
  if (r) ball = ballOf(r, g, now, reducedMotion);
  else if (drop && drop.player !== null) {
    // the ball lies inside the ring on the line to the catcher that did not take it, clear of the tiles' lines
    const i = players.indexOf(drop.player);
    if (i >= 0) {
      const c = ballAnchor(g, i);
      ball = { at: { x: g.centre.x + (c.x - g.centre.x) * 0.25, y: g.centre.y + (c.y - g.centre.y) * 0.25 }, size: ballSize(g), flying: false, dropped: true };
    }
  }
  if (drop && drop.player !== null) {
    const late = causeOf(drop)?.tile ?? null;
    const t = tiles.find((x) => x.player === drop.player);
    if (t) t.late = late;
  }
  let arc: RingView["arc"] = null;
  const a = r ? arcOf(r, p) : null;
  if (a) {
    // along the ring, clockwise, just inside the tiles: it never crosses the slots in the centre
    const n = players.length;
    const rho = arcRadius(g);
    const a0 = -Math.PI / 2 + (2 * Math.PI * a.fromIndex) / n;
    const sweep = (2 * Math.PI * a.ahead) / n;
    const at = (ang: number, rad: number) => ({ x: g.centre.x + rad * Math.cos(ang), y: g.centre.y + rad * Math.sin(ang) });
    // the label sits inside the arc, far enough in that its own box (about 112 by 20 px) clears the tiles
    const mid = a0 + sweep / 2;
    const depth = LABEL_HALF_W * Math.abs(Math.cos(mid)) + LABEL_HALF_H * Math.abs(Math.sin(mid)) + 6;
    arc = { centre: g.centre, radius: rho, from: at(a0, rho), to: at(a0 + sweep, rho), large: sweep > Math.PI, labelAt: at(mid, rho - depth), label: a.label, ahead: a.ahead };
  }
  const ladderRung = r ? p.ladder.find((x) => x.size === r.size) : undefined;
  const flash = r !== null && ladderRung?.record_pace_at != null && ladderRung.record_prev_pace != null && now - ladderRung.record_pace_at < 2500 && ladderRung.record_pace_at >= r.started_at;
  const caption = r
    ? r.seq === null
      ? `Ring of ${r.size}: waiting for the first catch`
      : `Ring of ${r.size}: player ${r.holder} has the ball, catch ${r.seq + 1}`
    : drop
      ? `Ring of ${drop.size}: the ball dropped`
      : `Next: a ring of ${players.length}`;
  return { geometry: g, tiles, ball, arc, flash, caption, ariaLabel: `${caption}. ${plural(players.length, "player", "players")} on a circle.` };
}

// ---- the density: CPU slots, the figures, the chart and the ladder ----------------------

/** A duration for the new figures: milliseconds, then seconds from one second up. */
export function fmtMs(v: number): string {
  return v >= 1000 ? `${(Math.round(v / 10) / 100).toFixed(2)} s` : `${Math.round(v)} ms`;
}
const oneDecimal = (v: number) => (Math.round(v * 10) / 10).toFixed(1);
/** Machines per CPU: a whole number plain, else one decimal. */
export function perCpu(machines: number, cpus: number): string {
  const q = Math.round((machines / cpus) * 10) / 10;
  return Number.isInteger(q) ? String(q) : q.toFixed(1);
}

export type SlotGroupKey = "ball" | "ready" | "waking" | "saving" | "free";
export const SLOT_GROUP_LABEL: Record<SlotGroupKey, string> = { ball: "ball", ready: "ready", waking: "waking", saving: "saving", free: "free" };
const SLOT_ORDER: SlotGroupKey[] = ["ball", "ready", "waking", "saving", "free"];

export interface SlotView {
  /** The machine holding this CPU; null for a free one. */
  player: number | null;
  colour: string | null;
  number: string | null;
}
export interface SlotGroupView {
  key: SlotGroupKey;
  label: string;
  slots: SlotView[];
}
export interface CpuSlotsView {
  /** "6 of 8 CPUs in use"; "CPU slots" when the agent only caps the count. */
  heading: string;
  /** "2.1 busy": the CPU the ring really burned over the last lap, when the agent measures it. */
  busy: string | null;
  /** The CPUs: slots drawn, held or free. */
  k: number;
  groups: SlotGroupView[];
  /** "26 of 32 asleep", when the coordinator says. */
  asleep: string | null;
}

/** Which group a machine's CPU is in: from the agent's slot list when it sends one, else from the players' phases. */
function slotGroups(p: Potato, r: PotatoRound): { key: SlotGroupKey; player: number }[] {
  if (r.cpu_slots !== null) {
    return r.cpu_slots.map((x) => ({ key: x.phase === "ball" ? "ball" : x.phase === "ready" ? "ready" : x.phase === "saving" ? "saving" : "waking", player: x.player }));
  }
  const out: { key: SlotGroupKey; player: number }[] = [];
  for (const n of r.ring) {
    const pl = playerOf(p, n);
    if (pl === null) continue;
    if (pl.phase === "sleeping") out.push({ key: "saving", player: n });
    else if (pl.phase === "waking") out.push({ key: "waking", player: n });
    else if (pl.phase === "awake") out.push({ key: n === r.holder ? "ball" : "ready", player: n });
  }
  return out;
}

/** The K CPUs as slots, each holding the chip of the machine that holds it. Null without an allotment. */
export function cpuSlotsOf(p: Potato, r: PotatoRound): CpuSlotsView | null {
  const k = p.cpus;
  if (k === null || k <= 0) return null;
  const held = slotGroups(p, r);
  const by = new Map<SlotGroupKey, SlotView[]>();
  for (const h of held) {
    const pl = playerOf(p, h.player);
    const list = by.get(h.key) ?? [];
    list.push({ player: h.player, colour: potatoColour(pl?.colour ?? 0, p.colours), number: String(h.player) });
    by.set(h.key, list);
  }
  const free = Math.max(0, k - held.length);
  by.set("free", Array.from({ length: free }, () => ({ player: null, colour: null, number: null })));
  const noun = p.confined === false ? "CPU slots" : "CPUs";
  const groups = SLOT_ORDER.map((key) => ({ key, label: SLOT_GROUP_LABEL[key], slots: by.get(key) ?? [] })).filter((g) => g.slots.length > 0);
  return {
    heading: `${held.length} of ${k} ${noun} in use`,
    busy: r.cpus_busy === null ? null : `${oneDecimal(r.cpus_busy)} busy`,
    k: groups.reduce((n, g) => n + g.slots.length, 0),
    groups,
    asleep: r.asleep === null ? null : `${r.asleep} of ${r.size} asleep`,
  };
}

const LABEL_HALF_W = 32;
const LABEL_HALF_H = 18;
/** The arc runs along the ring just inside the tiles. */
function arcRadius(g: RingGeometry): number {
  return Math.max(0, g.radius - g.tile * 0.5 - 6);
}
/** Whether a box w by h centred in the ring sits inside the arc and clear of the arc's label wherever the label lands (sampled every 3 degrees). */
export function centreClear(g: RingGeometry, w: number, h: number): boolean {
  const rho = arcRadius(g);
  if (Math.hypot(w / 2, h / 2) > rho - 10) return false;
  for (let deg = 0; deg < 360; deg += 3) {
    const t = (deg * Math.PI) / 180;
    const c = Math.cos(t);
    const s = Math.sin(t);
    const depth = LABEL_HALF_W * Math.abs(c) + LABEL_HALF_H * Math.abs(s) + 6;
    const lx = (rho - depth) * c;
    const ly = (rho - depth) * s;
    const apart = lx - LABEL_HALF_W >= w / 2 + 4 || lx + LABEL_HALF_W <= -w / 2 - 4 || ly - LABEL_HALF_H >= h / 2 + 2 || ly + LABEL_HALF_H <= -h / 2 - 2;
    if (!apart) return false;
  }
  return true;
}

export type SlotsForm = "full" | "compact" | "below";
export interface SlotsLayout {
  form: SlotsForm;
  /** Rows of chips: 1 or 2 in the compact form. */
  rows: number;
  /** A chip's side, px: 24 to 40 in the full form, 20 in the compact one. */
  chip: number;
  /** The block's size, px. */
  w: number;
  h: number;
}
const CHIP_GAP = 4;
const GROUP_GAP = 6;
const COMPACT_CHIP = 23;
/** The heading "4 of 8 CPUs in use" in 20 px type; the busy figure is a line of its own. */
const HEADING_W = 200;

/**
 * Where the K CPUs go: labelled at the largest chip from 40 to 24 px that fits the ring's centre
 * (the arc and its label kept clear), else small unlabelled chips in one or two rows there, else
 * under the ring.
 */
export function slotsLayout(g: RingGeometry, k: number): SlotsLayout {
  for (let chip = 40; chip >= 24; chip -= 2) {
    const w = Math.max(HEADING_W, k * (chip + CHIP_GAP) + 4 * GROUP_GAP);
    const h = chip + 104;
    if (centreClear(g, w, h)) return { form: "full", rows: 1, chip, w, h };
  }
  for (const rows of [1, 2]) {
    const w = Math.max(150, Math.ceil(k / rows) * COMPACT_CHIP);
    const h = 44 + rows * 24;
    if (centreClear(g, w, h)) return { form: "compact", rows, chip: 20, w, h };
  }
  return { form: "below", rows: 1, chip: 36, w: 0, h: 0 };
}

export interface ArcView {
  /** The ball's tile and the furthest machine being woken, as ring places (indices into the ring's tiles). */
  fromIndex: number;
  toIndex: number;
  /** "woken 4 ahead". */
  label: string;
  ahead: number;
}

/** From the ball to the furthest machine ahead of it that is being woken. Null with no ball or nobody waking. */
export function arcOf(r: PotatoRound, p: Potato): ArcView | null {
  if (r.holder === null) return null;
  const n = r.ring.length;
  const at = r.ring.indexOf(r.holder);
  if (at < 0 || n < 2) return null;
  let far = 0;
  let to = -1;
  for (let i = 0; i < n; i++) {
    const pl = playerOf(p, r.ring[i]!);
    if (pl === null || pl.phase !== "waking") continue;
    const d = (i - at + n) % n;
    if (d > far) {
      far = d;
      to = i;
    }
  }
  if (to < 0) return null;
  return { fromIndex: at, toIndex: to, label: `woken ${far} ahead`, ahead: far };
}

export interface DensityFigure {
  key: "asleep" | "pace" | "slowest" | "fastest";
  value: string;
  label: string;
}
export interface DensityView {
  /** "Machines per CPU", or "Machines" without an allotment. */
  heading: string;
  /** The large figure: machines per CPU, or the machine count without an allotment. */
  big: string;
  /** "32 machines · 8 CPUs"; empty without an allotment. */
  machines: string;
  where: string;
  figures: DensityFigure[];
  /** This round set the rung's record and the one before it is known: "9.8", else null (no pill). */
  record: { was: string } | null;
}

export function densityOf(p: Potato, r: PotatoRound): DensityView {
  const k = p.cpus !== null && p.cpus > 0 ? p.cpus : null;
  const noun = p.confined === false ? "CPU slots" : "CPUs";
  const figures: DensityFigure[] = [];
  if (r.asleep !== null) figures.push({ key: "asleep", value: `${Math.round((r.asleep / r.size) * 100)}%`, label: "asleep" });
  if (r.pace !== null) figures.push({ key: "pace", value: oneDecimal(r.pace), label: "catches / s" });
  if (r.slowest_wake_ms !== null) figures.push({ key: "slowest", value: fmtMs(r.slowest_wake_ms), label: "slowest wake" });
  const fastest = p.records.fastest_cycle_ms;
  if (fastest !== null) figures.push({ key: "fastest", value: fmtMs(fastest.value), label: "fastest sleep and wake" });
  const rung = p.ladder.find((g) => g.size === r.size);
  return {
    heading: k === null ? "Machines" : p.confined === false ? "Machines per CPU slot" : "Machines per CPU",
    big: k === null ? String(r.size) : perCpu(r.size, k),
    machines: k === null ? "" : `${r.size} machines · ${k} ${noun}`,
    where: [`ring of ${r.size}`, `lap ${r.lap + 1}`].join(" · "),
    figures,
    record: rung?.record_pace_at != null && rung.record_pace_at >= r.started_at && rung.record_prev_pace != null ? { was: oneDecimal(rung.record_prev_pace) } : null,
  };
}

export interface ChartPoint {
  pace: number;
  cpus: number;
}
export interface ChartView {
  title: string;
  /** One point per completed lap of this round, oldest first. */
  points: ChartPoint[];
  /** Where the round is now, when the agent and the coordinator both say. */
  now: ChartPoint | null;
  /** The axes' ends: pace from 0, CPUs from 0 to the allotment. */
  paceMax: number;
  cpusMax: number;
  k: number;
  kLabel: string;
  noun: string;
}

/** CPUs in use against pace over this round's laps, with the allotment as the line the machines cannot pass. */
export function chartOf(p: Potato, r: PotatoRound): ChartView | null {
  const k = p.cpus;
  if (k === null || k <= 0) return null;
  const points = r.laps.filter((l) => l.cpus_peak !== null && l.pace !== null).map((l) => ({ pace: l.pace!, cpus: l.cpus_peak! }));
  if (points.length === 0) return null;
  const now = r.pace !== null && r.cpus_in_use !== null ? { pace: r.pace, cpus: r.cpus_in_use } : null;
  // the pace axis runs on to the rung's record, or to well past the points, so a lone point is not floating
  const rung = p.ladder.find((g) => g.size === r.size);
  const top = Math.max(Math.max(...points.map((x) => x.pace), now?.pace ?? 0) * 1.6, rung?.record_pace ?? 0);
  return {
    title: "CPUs in use as the pace rises",
    points,
    now,
    paceMax: Math.max(1, Math.ceil(top)),
    cpusMax: k,
    k,
    kLabel: "no CPU for the next wake",
    noun: p.confined === false ? "CPU slots" : "CPUs",
  };
}

export interface LadderBar {
  size: number;
  /** "4 per CPU", or "32 machines" without an allotment. */
  label: string;
  /** The fastest pace held, as text; "none yet" before a lap completes. */
  value: string;
  /** 0 to 1 of the longest bar; null without a pace to draw. */
  length: number | null;
  current: boolean;
  /** Larger than the host runs: skipped. */
  skipped: boolean;
}
export interface LadderBarsView {
  title: string;
  unit: string;
  rows: LadderBar[];
}

/** One bar per rung: its length is the fastest pace held on that ring. */
export function ladderBarsOf(p: Potato): LadderBarsView {
  const k = p.cpus !== null && p.cpus > 0 ? p.cpus : null;
  const top = Math.max(0, ...p.ladder.map((g) => g.record_pace ?? 0));
  return {
    title: k === null ? "Fastest pace held" : `Fastest pace held on ${k} ${p.confined === false ? "CPU slots" : "CPUs"}`,
    unit: "catches / s",
    rows: p.ladder.map((g) => {
      const skipped = p.capacity > 0 && g.size > p.capacity;
      const pace = g.record_pace;
      const hold = pace === null && g.record_hold_ms !== null ? g.record_hold_ms : null;
      return {
        size: g.size,
        label: k === null ? `${g.size} machines` : `${perCpu(g.size, k)} per CPU`,
        value: pace !== null ? oneDecimal(pace) : hold !== null ? `${fmtMs(hold)} hold` : skipped ? "skipped" : "none yet",
        length: pace !== null && top > 0 ? pace / top : null,
        current: g.size === p.size,
        skipped,
      };
    }),
  };
}

// ---- drops, the ladder and the records ---------------------------------------------

/** The drop in a sentence: which player, the late stage and its milliseconds. */
export function dropHeadline(d: PotatoDrop): string {
  const who = d.player === null ? "A player" : `Player ${d.player}`;
  switch (d.reason) {
    case "not_listening":
      switch (d.stage) {
        case "waking":
          return `${who} was still waking, ${ms(d.stage_ms)} into its wake, when the ball dropped.`;
        case "asleep":
          return `${who} was still asleep when the ball dropped; it had slept ${ms(d.stage_ms)}.`;
        case "sleeping":
          return `${who} was still going to sleep, ${ms(d.stage_ms)} in, when the ball dropped.`;
        default:
          return `${who} was not accepting the ball (${d.stage}, ${ms(d.stage_ms)}) when it dropped.`;
      }
    case "proof":
      return `${who}'s colour, count or chain was not the step of its last catch.`;
    case "deadline":
      return d.player === null ? "No catch was reported in time." : `No catch was reported in time; player ${d.player} was next.`;
    case "host_gone":
      return "The host running the players disconnected.";
    default:
      if (d.reason.endsWith("_failed")) return `${who}'s ${d.reason.slice(0, -"_failed".length)} failed.`;
      return `The ball dropped: ${d.reason.replace(/_/g, " ")}.`;
  }
}

/** "prepare 17.6 ms, pause 0.5 ms" from a stage map, in the order the coordinator gives it. */
function stagesText(stages: Record<string, number>): string {
  return Object.entries(stages)
    .map(([k, v]) => `${k.replace(/_ms$/, "").replace(/_/g, " ")} ${tenthsMs(v)}`)
    .join(", ");
}

/** What a drop measured, in the words of the ring's tile, the card's big figure and its line. Null when the reason names no measured cause. */
export function causeOf(d: PotatoDrop): { tile: string; line: string; big: { value: string; word: string } | null } | null {
  switch (d.cause) {
    case "waking":
      return d.cause_ms === null
        ? { tile: "still waking", line: "Still waking when the ball arrived.", big: null }
        : { tile: `${fmtMs(d.cause_ms)} late`, line: `Still waking when the ball arrived; it accepted ${fmtMs(d.cause_ms)} later.`, big: { value: fmtMs(d.cause_ms), word: "late" } };
    case "cpu_wait": {
      // the wait is in cause_ms, or in cpu_wait_ms while the cause was still "waking"
      const w = d.cause_ms ?? d.cpu_wait_ms;
      return w === null
        ? { tile: "waiting for a CPU", line: "Its wake was waiting for a CPU.", big: null }
        : { tile: `waited ${fmtMs(w)} for a CPU`, line: `All CPUs were busy: its wake waited ${fmtMs(w)} for one.`, big: { value: fmtMs(w), word: "waiting for a CPU" } };
    }
    case "saving":
      return d.cause_ms === null
        ? { tile: "still saving", line: "Still saving when the ball arrived.", big: null }
        : { tile: "still saving", line: `Still saving, ${fmtMs(d.cause_ms)} in, when the ball arrived.`, big: null };
    case "proof":
      return { tile: "proof failed", line: "Its colour, count or chain was not the step of its last catch.", big: null };
    default:
      return null;
  }
}

export interface DropCard {
  title: string;
  /** The measured cause when there is one, else the reason in a sentence. */
  headline: string;
  /** "84 ms" and "late", when the cause has a figure. */
  big: { value: string; word: string } | null;
  context: string;
  stages: string[];
  next: string;
  detail: string;
}

export function dropCard(p: Potato, d: PotatoDrop, now: number): DropCard {
  const stages: string[] = [];
  if (Object.keys(d.sleep_stages).length > 0) stages.push(`Its last sleep: ${stagesText(d.sleep_stages)}`);
  if (Object.keys(d.wake_stages).length > 0) stages.push(`Its last wake: ${stagesText(d.wake_stages)}`);
  const next = p.size ?? d.size;
  const cause = causeOf(d);
  return {
    title: "The ball dropped",
    headline: cause?.line ?? dropHeadline(d),
    big: cause?.big ?? null,
    context: [`Ring of ${d.size}`, `lap ${d.lap + 1}`, d.pace === null ? null : `${oneDecimal(d.pace)} catches / s`].filter((x) => x !== null).join(" · "),
    stages,
    next: p.next_round_at === null ? "" : `Next: a ring of ${next} in ${seconds(p.next_round_at - now)}`,
    detail: d.detail,
  };
}

// ---- the two buttons ---------------------------------------------------------------

export interface PotatoButton {
  label: string;
  disabled: boolean;
  note: string;
}

/** This viewer's latest committed intent of a kind, by the coordinator's clock. */
function lastMineAt(st: FleetState, you: string | null, name: IntentName): number | null {
  let best: number | null = null;
  for (const a of st.actions) {
    if (a.name !== name || a.outcome === "refused" || !sameViewer(a.viewer, you)) continue;
    if (best === null || a.at > best) best = a.at;
  }
  return best;
}

export function potatoFasterButton(s: ClientState, p: Potato, now: number): PotatoButton {
  const label = "Faster";
  if (s.connection !== "open") return { label, disabled: true, note: "paused while the coordinator is offline" };
  const r = p.round;
  if (r === null) return { label, disabled: true, note: "works while a round is being played" };
  if (s.pending.some((x) => x.intent.name === "potato_faster")) return { label: "Faster requested", disabled: true, note: "" };
  const last = s.accepted ? lastMineAt(s.accepted, s.you, "potato_faster") : null;
  if (last !== null && now < last + FASTER_INTERVAL_MS) return { label, disabled: true, note: `again in ${seconds(last + FASTER_INTERVAL_MS - now)}` };
  if (r.hold_ms <= r.min_hold_ms) return { label, disabled: true, note: "as fast as it goes" };
  return { label, disabled: false, note: "" };
}

export function potatoRestartButton(s: ClientState, p: Potato, now: number): PotatoButton {
  const label = "Restart";
  if (s.connection !== "open") return { label, disabled: true, note: "paused while the coordinator is offline" };
  if (p.round === null && p.capacity === 0) return { label, disabled: true, note: "works while a host runs players" };
  if (s.pending.some((x) => x.intent.name === "potato_restart")) return { label: "Restart requested", disabled: true, note: "" };
  const last = s.accepted ? lastMineAt(s.accepted, s.you, "potato_restart") : null;
  if (last !== null && now < last + RESTART_INTERVAL_MS) return { label, disabled: true, note: `again in ${seconds(last + RESTART_INTERVAL_MS - now)}` };
  const r = p.round;
  if (r !== null && now < r.started_at + RESTART_INTERVAL_MS) {
    return { label, disabled: true, note: `possible ${seconds(RESTART_INTERVAL_MS)} into a round, in ${seconds(r.started_at + RESTART_INTERVAL_MS - now)}` };
  }
  const size = r?.size ?? p.size;
  return { label, disabled: false, note: size === null ? "" : `ring of ${size} again` };
}

const POTATO_INTENTS: Record<string, string> = { potato_faster: "Faster", potato_restart: "Restart" };

/** The newest refusal of a potato intent still on screen. */
export function potatoNotice(notices: readonly Notice[]): string | null {
  for (let i = notices.length - 1; i >= 0; i--) {
    const n = notices[i]!;
    const what = POTATO_INTENTS[n.intent];
    if (n.kind === "refused" && what !== undefined) return `${what} was not applied${n.reason ? `: ${n.reason}` : ""}`;
  }
  return null;
}

// ---- the view ----------------------------------------------------------------------

export interface PotatoInput {
  state: ClientState;
  /** Local Unix ms. */
  now: number;
  /** Where the ring may go, measured by the page. */
  stage: Box | null;
  reducedMotion: boolean;
}

export interface PotatoView {
  title: string;
  landing: LandingView;
  online: boolean;
  /** Why there is nothing to show, when there is nothing. */
  empty: string | null;
  banner: string | null;
  ring: RingView | null;
  density: DensityView | null;
  cpuSlots: CpuSlotsView | null;
  chart: ChartView | null;
  drop: DropCard | null;
  ladder: LadderBarsView | null;
  faster: PotatoButton;
  restart: PotatoButton;
  notice: string | null;
  verify: ReturnType<typeof verifyView>;
  sleep: SleepView | null;
}

export function potatoView(inp: PotatoInput): PotatoView {
  const s = inp.state;
  const online = s.connection === "open";
  const now = serverNow(s.clock, inp.now);
  const p = s.accepted?.potato ?? null;
  const off: PotatoButton = { label: "", disabled: true, note: "" };
  const common = {
    title: POTATO_TITLE,
    landing: landingView("potato"),
    online,
    verify: verifyView(s),
    notice: potatoNotice(s.notices),
    sleep: s.accepted === null ? null : sleepView(s.accepted, now, online, s.pending.some((x) => x.intent.name === "wake")),
    banner: s.accepted !== null && !online ? "Reconnecting to the coordinator. The game is shown as it last was; the buttons wait until it is back." : null,
  };
  if (s.accepted === null || p === null) {
    return {
      ...common,
      empty: s.accepted === null ? (s.connection === "closed" ? "Cannot reach the fleet yet. Trying again." : "Connecting to the fleet.") : NO_POTATO,
      ring: null,
      density: null,
      cpuSlots: null,
      chart: null,
      drop: null,
      ladder: null,
      faster: { ...off, label: "Faster" },
      restart: { ...off, label: "Restart" },
    };
  }
  const drop = p.round === null && p.last_drop !== null && p.next_round_at !== null ? dropCard(p, p.last_drop, now) : null;
  return {
    ...common,
    empty: p.round === null && p.capacity === 0 ? NO_HOST : null,
    ring: ringOf(p, inp.stage, now, inp.reducedMotion),
    density: p.round ? densityOf(p, p.round) : null,
    cpuSlots: p.round ? cpuSlotsOf(p, p.round) : null,
    chart: p.round ? chartOf(p, p.round) : null,
    drop,
    ladder: ladderBarsOf(p),
    faster: potatoFasterButton(s, p, now),
    restart: potatoRestartButton(s, p, now),
  };
}
