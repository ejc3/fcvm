// Potato mode (`?mode=potato`): a ring of CPU-only player VMs passing a ball, the pace, the
// ladder of ring sizes, the last drop and the records (docs/fleet/ARCHITECTURE.md, "Potato
// mode"; PROTOCOL.md, "Potato mode"). Pure: the page measures the stage, passes the time in
// and draws what comes back. The coordinator is never drawn.

import type { Box } from "./layout.ts";
import { serverNow } from "./clock.ts";
import { sameViewer, type FleetState, type IntentName, type Potato, type PotatoDrop, type PotatoPlayer, type PotatoRound } from "./protocol.ts";
import type { ClientState, Notice } from "./store.ts";
import { sleepView, verifyView, type SleepView } from "./view.ts";

export const POTATO_TITLE = "Hot potato";
export const POTATO_SENTENCE =
  "Each tile is a CPU-only virtual machine. A player that has thrown the ball goes to sleep (a snapshot, and its process exits) and has to wake before the ball comes round again.";
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
      state = "not started yet";
      break;
    case "sleeping":
      line = "going to sleep";
      state = `going to sleep for ${ms(Math.max(0, now - pl!.since))}`;
      break;
    case "asleep":
      line = "asleep";
      state = `asleep for ${ms(Math.max(0, now - pl!.since))}`;
      break;
    case "waking": {
      const el = Math.max(0, now - pl!.since);
      const last = pl!.last_wake_ms;
      wake = { fraction: last !== null && last > 0 ? Math.min(1, el / last) : 0, text: ms(el) };
      line = `waking ${ms(el)}`;
      state = `waking for ${ms(el)}${last !== null ? `; its last wake took ${ms(last)}` : ""}`;
      break;
    }
    case "failed":
      line = "failed";
      state = "failed";
      break;
    default:
      line = holder ? "has the ball" : "awake";
      state = "awake";
  }
  const catches = pl === null ? "" : `, ${plural(pl.counter, "catch", "catches")}`;
  return {
    player: n,
    box,
    colour: potatoColour(pl?.colour ?? 0, p.colours),
    phase,
    number: box.w >= NUMBER_MIN_TILE ? String(n) : null,
    line: box.w >= LINE_MIN_TILE ? line : null,
    wake,
    holder,
    ariaLabel: `Player ${n}, ${state}${catches}${holder ? ", holding the ball" : ""}`,
  };
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
  const caption = r
    ? r.seq === null
      ? `Ring of ${r.size}: waiting for the first catch`
      : `Ring of ${r.size}: player ${r.holder} has the ball, catch ${r.seq + 1}`
    : drop
      ? `Ring of ${drop.size}: the ball dropped`
      : `Next: a ring of ${players.length}`;
  return { geometry: g, tiles, ball, caption, ariaLabel: `${caption}. ${plural(players.length, "player", "players")} on a circle.` };
}

// ---- the pace --------------------------------------------------------------------

export interface PaceView {
  hold: string;
  /** "Lap 2 · ring of 4 · catch 7". */
  where: string;
  /** The meter: the time each player has to sleep and wake, against the slowest sleep and wake in the ring. */
  meter: {
    /** 0 to 1 of the track. */
    turnaround: number;
    need: number | null;
    turnaroundText: string;
    needText: string | null;
    /** The slowest player needs more than it has: the next lap can drop the ball. */
    short: boolean;
  };
  /** "Faster: holds capped at 479 ms", or null. */
  pace: string | null;
  start: string;
}

/** The slowest last sleep plus last wake among the ring's players, or null before any has done both. */
export function slowestCycleMs(p: Potato, ring: readonly number[]): number | null {
  let worst: number | null = null;
  for (const n of ring) {
    const pl = playerOf(p, n);
    if (pl === null || pl.last_sleep_ms === null || pl.last_wake_ms === null) continue;
    const c = pl.last_sleep_ms + pl.last_wake_ms;
    if (worst === null || c > worst) worst = c;
  }
  return worst;
}

export function paceOf(p: Potato, r: PotatoRound): PaceView {
  const need = slowestCycleMs(p, r.ring);
  const scale = Math.max(1, r.turnaround_ms, need ?? 0) * 1.15;
  const holds = r.size - 1;
  return {
    hold: ms(r.hold_ms),
    where: [`Lap ${r.lap + 1}`, `ring of ${r.size}`, r.seq === null ? "no catch yet" : `catch ${r.seq + 1}`].join(" · "),
    meter: {
      turnaround: r.turnaround_ms / scale,
      need: need === null ? null : need / scale,
      turnaroundText: `${ms(r.turnaround_ms)} for each player to sleep and wake (${plural(holds, "hold", "holds")})`,
      needText: need === null ? null : `slowest sleep and wake in the ring: ${ms(need)}`,
      short: need !== null && need > r.turnaround_ms,
    },
    pace: r.pace_ms === null ? null : `Faster: holds capped at ${ms(r.pace_ms)}`,
    start: `Started at ${ms(r.start_hold_ms)}, a tenth shorter each lap; floor ${ms(r.min_hold_ms)}`,
  };
}

// ---- drops, the ladder and the records ---------------------------------------------

const STAGE_WORD: Record<string, string> = { asleep: "asleep", sleeping: "going to sleep", waking: "waking" };

/** What ended a round, in a short phrase for the ladder: "player 1 still waking (90 ms)". */
export function brokeText(d: PotatoDrop): string {
  const who = d.player === null ? "a player" : `player ${d.player}`;
  switch (d.reason) {
    case "not_listening":
      return `${who} still ${STAGE_WORD[d.stage] ?? d.stage} (${ms(d.stage_ms)})`;
    case "proof":
      return `${who}'s proof did not match`;
    case "deadline":
      return "no catch reported in time";
    case "host_gone":
      return "the host disconnected";
    default:
      if (d.reason.endsWith("_failed")) return `${who}'s ${d.reason.slice(0, -"_failed".length)} failed`;
      return d.reason.replace(/_/g, " ");
  }
}

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

export interface DropCard {
  title: string;
  headline: string;
  /** "It started accepting 84 ms after the ball dropped.", when the agent reported it. */
  listening: string | null;
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
  return {
    title: "The ball dropped",
    headline: dropHeadline(d),
    listening: d.listening_after_ms === null ? null : `It started accepting ${ms(d.listening_after_ms)} after the ball dropped.`,
    context: `Ring of ${d.size}, lap ${d.lap + 1}, at a hold of ${ms(d.hold_ms)}, after ${plural(d.seq, "catch", "catches")}.`,
    stages,
    next: p.next_round_at === null ? "" : `Next: a ring of ${next} in ${seconds(p.next_round_at - now)}`,
    detail: d.detail,
  };
}

export interface LadderRow {
  size: number;
  players: string;
  record: string;
  broke: string;
  rounds: string;
  /** The size being played, or the next. */
  current: boolean;
  /** Larger than the host runs: skipped. */
  skipped: boolean;
}

export interface LadderView {
  caption: string;
  head: { players: string; record: string; broke: string; rounds: string };
  rows: LadderRow[];
}

export function ladderOf(p: Potato): LadderView {
  return {
    caption: "Ring sizes, smallest first. A record is the shortest hold of a lap that completed.",
    head: { players: "Ring", record: "Record hold", broke: "What ended its last round", rounds: "Rounds" },
    rows: p.ladder.map((g) => {
      const skipped = p.capacity > 0 && g.size > p.capacity;
      return {
        size: g.size,
        players: String(g.size),
        record: g.record_hold_ms === null ? (g.rounds > 0 ? "no lap completed" : "--") : ms(g.record_hold_ms),
        broke: g.broke ? `${brokeText(g.broke)}, lap ${g.broke.lap + 1} at ${ms(g.broke.hold_ms)}` : skipped ? `skipped: the host runs ${p.capacity} players` : "",
        rounds: String(g.rounds),
        current: g.size === p.size,
        skipped,
      };
    }),
  };
}

export interface RecordRow {
  name: string;
  label: string;
  value: string;
  note: string;
}

export function recordsOf(p: Potato): RecordRow[] {
  const f = p.records.fastest_cycle_ms;
  const l = p.records.longest_round;
  return [
    {
      name: "fastest-cycle",
      label: "Fastest sleep and wake",
      value: f === null ? "--" : tenthsMs(f.value),
      note: f === null ? "none yet" : `${f.player === null ? "a player" : `player ${f.player}`} in a ring of ${f.size}`,
    },
    {
      name: "longest-round",
      label: "Longest round",
      value: l === null ? "--" : plural(Math.round(l.value), "catch", "catches"),
      note: l === null ? "none yet" : `a ring of ${l.size}`,
    },
  ];
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
  if (r.hold_ms <= r.min_hold_ms) return { label, disabled: true, note: "the hold is at its floor" };
  const next = Math.max(r.min_hold_ms, r.hold_ms - Math.floor(r.hold_ms / 5));
  return { label, disabled: false, note: `cuts the hold to ${ms(next)}` };
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
  return { label, disabled: false, note: size === null ? "starts the ring again" : `starts the ring of ${size} again` };
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
  sentence: string;
  online: boolean;
  /** Why there is nothing to show, when there is nothing. */
  empty: string | null;
  banner: string | null;
  ring: RingView | null;
  pace: PaceView | null;
  drop: DropCard | null;
  ladder: LadderView | null;
  records: RecordRow[];
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
    sentence: POTATO_SENTENCE,
    online,
    verify: verifyView(s),
    notice: potatoNotice(s.notices),
    sleep: s.accepted === null ? null : sleepView(s.accepted, now, online, s.pending.some((x) => x.intent.name === "wake")),
    banner: s.accepted !== null && !online ? "The coordinator is offline. The game is shown as it last was; the buttons wait until it is back." : null,
  };
  if (s.accepted === null || p === null) {
    return {
      ...common,
      empty: s.accepted === null ? (s.connection === "closed" ? "Cannot reach the fleet yet. Trying again." : "Connecting to the fleet.") : NO_POTATO,
      ring: null,
      pace: null,
      drop: null,
      ladder: null,
      records: [],
      faster: { ...off, label: "Faster" },
      restart: { ...off, label: "Restart" },
    };
  }
  const drop = p.round === null && p.last_drop !== null && p.next_round_at !== null ? dropCard(p, p.last_drop, now) : null;
  return {
    ...common,
    empty: p.round === null && p.capacity === 0 ? NO_HOST : null,
    ring: ringOf(p, inp.stage, now, inp.reducedMotion),
    pace: p.round ? paceOf(p, p.round) : null,
    drop,
    ladder: ladderOf(p),
    records: recordsOf(p),
    faster: potatoFasterButton(s, p, now),
    restart: potatoRestartButton(s, p, now),
  };
}
