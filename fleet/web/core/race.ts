// The race: the scoreboard's bars (fcvm, EC2, and Lambda when the AWS half has Lambda
// tiles), the shared countdown to the next restart, the one button that starts one, and
// what a screen reader hears when a side comes back. Every figure is the coordinator's
// measurement; the page only formats it and draws it to scale.

import * as fmt from "./format.ts";
import type { Backing, FleetState, HalfScore, IntentMsg, RelaunchAll, Tile } from "./protocol.ts";

export const KIND_TAG: Record<Backing, string> = { fcvm: "fcvm", ec2: "EC2", lambda: "Lambda" };

/**
 * What the AWS side waited for in a race, kept out of its time: EC2 had no spot GPU, or
 * Lambda had to start a GPU instance before it could place the new environment.
 */
const WAIT_SAID: Record<Backing, { was: string; when: string; now: string; clock: string }> = {
  fcvm: { was: "", when: "", now: "", clock: "" },
  ec2: { was: "AWS had no spot GPU", when: "AWS had no spot GPU", now: "AWS had no spot GPU: waiting", clock: "AWS spent finding a spot GPU" },
  lambda: {
    was: "Lambda started a new GPU instance",
    when: "Lambda started a new GPU instance",
    now: "Lambda is starting a GPU instance:",
    clock: "Lambda spent starting a GPU instance",
  },
};

/** Lambda's part of a race, or null in a race without Lambda tiles. */
function hasLambda(ra: RelaunchAll): boolean {
  return ra.lambda_gpus != null || ra.lambda_done_at != null || ra.lambda_pending != null;
}

function doneAtOf(ra: RelaunchAll, h: Backing): number | null {
  return h === "fcvm" ? ra.fcvm_done_at : h === "ec2" ? ra.ec2_done_at : ra.lambda_done_at ?? null;
}

/** The axis a race is drawn on before EC2 has a median of its own. */
const FIRST_RACE_AXIS_MS = 60_000;

export interface BarView {
  half: Backing;
  name: string;
  /** Solid bar, as a fraction of the axis. */
  solid: number;
  /** Hatched part (EC2 waiting for spot capacity, Lambda for a GPU instance), as a fraction of the axis. */
  hatch: number;
  /** The hatched part comes first (during a race, where the wait happened) or after the bar (at rest, last race's wait). */
  hatchFirst: boolean;
  /** The last race's time on the same axis, or null. */
  lastMark: number | null;
  /** The big figure: a median at rest, a running clock or a frozen time in a race. */
  value: string;
  /** What the big figure is. */
  valueLabel: string;
  /** One line under it. */
  line: string;
  /** "AWS had no spot GPU for 21 s, not counted", or "". */
  hatchLabel: string;
  measured: string;
  /** Counting up now. */
  live: boolean;
  /** This half is back: its bar stops. */
  done: boolean;
}

export interface CountdownView {
  label: string;
  value: string;
  note: string;
  racing: boolean;
  /** Something to say: a race running, or a restart scheduled while the GPUs are awake. */
  show: boolean;
}

export interface RestartButton {
  label: string;
  disabled: boolean;
  note: string;
  /** Not shown: it would offer to restart zero GPUs. */
  hidden: boolean;
}

export interface ScoreView {
  racing: boolean;
  title: string;
  fcvm: BarView;
  ec2: BarView;
  /** The Lambda tiles' own bar, when the AWS half has Lambda tiles or a race timed them. */
  lambda: BarView | null;
  countdown: CountdownView;
  button: RestartButton;
  /** The aria-live text: each half's result once it is back, in plain words. */
  announce: string;
}

function raceOver(ra: RelaunchAll): boolean {
  return ra.fcvm_done_at !== null && ra.ec2_done_at !== null && (!hasLambda(ra) || ra.lambda_done_at != null);
}

/** When every side of a finished race was back. */
function raceDoneAt(ra: RelaunchAll): number {
  return Math.max(ra.fcvm_done_at!, ra.ec2_done_at!, hasLambda(ra) ? ra.lambda_done_at ?? 0 : 0);
}

/** The running race, or null between races. */
export function runningRace(st: FleetState): RelaunchAll | null {
  const ra = st.relaunch_all;
  return ra !== null && !raceOver(ra) ? ra : null;
}

function halfTiles(st: FleetState, h: Backing): Tile[] {
  return st.tiles.filter((t) => t.half === h);
}

/** GPUs a race restarted in a half, and how many of them stream again. */
function raceCounts(st: FleetState, ra: RelaunchAll, h: Backing): { gpus: number; back: number } {
  const gpusField = h === "fcvm" ? ra.fcvm_gpus : h === "ec2" ? ra.ec2_gpus : ra.lambda_gpus ?? null;
  const pending = h === "fcvm" ? ra.fcvm_pending : h === "ec2" ? ra.ec2_pending : ra.lambda_pending ?? null;
  const tiles = halfTiles(st, h);
  const backTiles = tiles.filter(
    (t) => t.phase === "running" && t.last_relaunch?.first_frame_at != null && t.last_relaunch.first_frame_at >= ra.started_at,
  ).length;
  const gpus = gpusField ?? tiles.filter((t) => (t.slot !== null || t.gpu !== null) && t.borrowed === null).length;
  const back = pending !== null ? Math.max(0, gpus - pending) : backTiles;
  return { gpus, back };
}

/**
 * A finished half's time, click to its last first frame without capacity waits: the
 * coordinator's own measurement (`relaunch_all.<half>_ms`, else the matching race in
 * `races`), which leaves each tile's own wait out. Only a coordinator that sends neither
 * gets the page's estimate, the half's wall time less the longest wait. Null while the
 * half runs.
 */
export function halfTimeMs(st: FleetState, ra: RelaunchAll, h: Backing): number | null {
  const doneAt = doneAtOf(ra, h);
  if (doneAt === null) return null;
  const measured = h === "fcvm" ? ra.fcvm_ms : h === "ec2" ? ra.ec2_ms : ra.lambda_ms ?? null;
  if (measured !== null) return measured;
  const kept = st.races.find((r) => r.started_at === ra.started_at)?.[h]?.time_ms ?? null;
  if (kept !== null) return kept;
  const wall = doneAt - ra.started_at;
  return h === "fcvm" ? wall : wall - Math.min(wall, capacityWaitMs(ra, doneAt, h));
}

/** Milliseconds an AWS backing has waited for capacity in this race, up to now (0 for fcvm). */
export function capacityWaitMs(ra: RelaunchAll, now: number, h: Backing = "ec2"): number {
  if (h === "fcvm") return 0;
  const [done, since] = h === "ec2" ? [ra.ec2_capacity_wait_ms, ra.ec2_waiting_since] : [ra.lambda_capacity_wait_ms ?? 0, ra.lambda_waiting_since ?? null];
  return done + (since !== null ? Math.max(0, now - since) : 0);
}

function waitingNowOf(ra: RelaunchAll, h: Backing): boolean {
  return h === "ec2" ? ra.ec2_waiting_since !== null : h === "lambda" ? ra.lambda_waiting_since != null : false;
}

function backLine(gpus: number, back: number, done: boolean): string {
  if (gpus === 0) return "nothing to restart";
  if (done) return gpus === 1 ? "its GPU is streaming again" : `all ${gpus} GPUs streaming again`;
  return `${back} of ${fmt.count(gpus, "GPU", "GPUs")} streaming again`;
}

function measuredOn(n: number): string {
  return `measured on ${fmt.count(n, "GPU", "GPUs")}`;
}

function restBar(h: Backing, score: HalfScore | null, axis: number): BarView {
  const base = { half: h, name: KIND_TAG[h], hatchFirst: false, live: false, done: false };
  if (score === null) {
    return { ...base, solid: 0, hatch: 0, lastMark: null, value: "--", valueLabel: "median restart", line: "no restart measured yet", hatchLabel: "", measured: "" };
  }
  const wait = h === "fcvm" ? 0 : score.last_capacity_wait_ms;
  return {
    ...base,
    solid: score.median_ms / axis,
    hatch: wait / axis,
    lastMark: score.last_ms / axis,
    value: fmt.seconds(score.median_ms),
    valueLabel: score.races === 1 ? "one restart" : `median of ${score.races} restarts`,
    line: `last restart ${fmt.seconds(score.last_ms)}`,
    hatchLabel: wait > 0 ? `last restart: ${WAIT_SAID[h].was} for ${fmt.seconds(wait)}, not counted` : "",
    measured: measuredOn(score.measured_on),
  };
}

function raceBar(st: FleetState, ra: RelaunchAll, h: Backing, now: number, axis: number): BarView {
  const doneAt = doneAtOf(ra, h);
  const done = doneAt !== null;
  const end = doneAt ?? now;
  const total = Math.max(0, end - ra.started_at);
  const wait = Math.min(total, capacityWaitMs(ra, end, h));
  const counted = total - wait;
  const { gpus, back } = raceCounts(st, ra, h);
  const waitingNow = waitingNowOf(ra, h) && !done;
  const out = sittingOut(st, h);
  const outLine = out === 0 ? "" : ` · ${out === 1 ? "1 sits out" : `${out} sit out`}, lent to ${out === 1 ? "a visitor" : "visitors"}`;
  return {
    half: h,
    name: KIND_TAG[h],
    solid: counted / axis,
    hatch: wait / axis,
    hatchFirst: true,
    lastMark: null,
    value: done ? fmt.seconds(halfTimeMs(st, ra, h)!) : fmt.raceClock(counted),
    valueLabel: done ? "back in" : "restarting",
    line: backLine(gpus, back, done) + outLine,
    hatchLabel: waitingNow
      ? `${WAIT_SAID[h].now} ${fmt.countdown(wait)}, not counted`
      : wait > 0
        ? `${WAIT_SAID[h].was} for ${fmt.seconds(wait)}, not counted`
        : "",
    measured: measuredOn(gpus),
    live: !done,
    done,
  };
}

/** The announcement for a race: one sentence per half that is back. */
export function raceAnnouncement(st: FleetState): string {
  const ra = st.relaunch_all;
  if (!ra) return "";
  const parts: string[] = [];
  if (ra.fcvm_done_at !== null) {
    const { gpus } = raceCounts(st, ra, "fcvm");
    parts.push(`fcvm: ${backLine(gpus, gpus, true)} ${fmt.seconds(halfTimeMs(st, ra, "fcvm")!)} after the restart.`);
  }
  for (const h of ["ec2", "lambda"] as const) {
    const doneAt = doneAtOf(ra, h);
    if (doneAt === null || (h === "lambda" && !hasLambda(ra))) continue;
    const { gpus } = raceCounts(st, ra, h);
    const wait = Math.min(doneAt - ra.started_at, capacityWaitMs(ra, doneAt, h));
    const tail = wait > 0 ? `, not counting ${fmt.seconds(wait)} when ${WAIT_SAID[h].when}` : "";
    parts.push(`${KIND_TAG[h]}: ${backLine(gpus, gpus, true)} ${fmt.seconds(halfTimeMs(st, ra, h)!)} after the restart${tail}.`);
  }
  return parts.join(" ");
}

export interface ScoreInput {
  st: FleetState;
  /** Server Unix ms. */
  now: number;
  pending: readonly IntentMsg[];
  /** The coordinator connection is open; without it nothing can be pressed. */
  online: boolean;
}

/** The fleet is not awake: restarts and loans wait for it. */
export const NOT_AWAKE_NOTE = "works once the GPUs are awake";

/** GPUs a restart-all would restart: every tile with a GPU behind it. */
export function restartableGpus(st: FleetState): number {
  return st.tiles.filter((t) => t.gpu !== null && t.slot !== null && t.borrowed === null).length;
}

/** GPUs a half lends to visitors now: they sit races out. */
export function sittingOut(st: FleetState, h: Backing): number {
  return st.tiles.filter((t) => t.half === h && t.borrowed !== null).length;
}

export function restartButton(i: ScoreInput): RestartButton {
  const { st, now } = i;
  const n = restartableGpus(st);
  // Asleep or waking, the GPUs placed are none or some: the count would say less than a restart will.
  const label = st.fleet.power.state === "awake" ? `Restart all ${fmt.count(n, "GPU", "GPUs")}` : "Restart all GPUs";
  const zero = st.fleet.power.state === "awake" && n === 0;
  if (!i.online) return { label, disabled: true, note: "paused while the coordinator is offline", hidden: zero };
  if (st.fleet.power.state !== "awake") return { label, disabled: true, note: NOT_AWAKE_NOTE, hidden: false };
  const ra = runningRace(st);
  if (ra) return { label: "Restarting now", disabled: true, note: "", hidden: false };
  if (i.pending.some((p) => p.name === "restart_now")) return { label: "Restart requested", disabled: true, note: "", hidden: false };
  if (st.cooldown_until !== null && st.cooldown_until > now) {
    return { label, disabled: true, note: `available again in ${fmt.countdown(st.cooldown_until - now)}`, hidden: zero };
  }
  if (n === 0) return { label, disabled: true, note: "no GPU to restart", hidden: true };
  return { label, disabled: false, note: "", hidden: false };
}

export function scoreView(i: ScoreInput): ScoreView {
  const { st, now } = i;
  const ra = runningRace(st);
  const sb = st.scoreboard;
  let fcvm: BarView;
  let ec2: BarView;
  let lambda: BarView | null = null;
  // A Lambda bar once the AWS half has Lambda tiles, or a race has timed some.
  const lambdaShown = sb.lambda != null || st.tiles.some((t) => t.half === "lambda") || (ra !== null && hasLambda(ra));
  let countdown: CountdownView;
  if (ra) {
    // The AWS bars grow across the panel; the axis is what the slower of them usually
    // takes, or longer once this race passes it.
    const usual = (s: HalfScore | null) => (s ? s.median_ms + s.last_capacity_wait_ms : FIRST_RACE_AXIS_MS);
    const expected = Math.max(usual(sb.ec2), lambdaShown && hasLambda(ra) ? usual(sb.lambda ?? null) : 0);
    const ran = (h: Backing) => (doneAtOf(ra, h) ?? now) - ra.started_at;
    const axis = Math.max(expected, ran("ec2"), ran("fcvm"), hasLambda(ra) ? ran("lambda") : 0, 1000);
    fcvm = raceBar(st, ra, "fcvm", now, axis);
    ec2 = raceBar(st, ra, "ec2", now, axis);
    if (lambdaShown) lambda = hasLambda(ra) ? raceBar(st, ra, "lambda", now, axis) : restBar("lambda", sb.lambda ?? null, axis);
    countdown = {
      label: ra.by === "schedule" ? "Scheduled restart, running" : "Restart running",
      value: fmt.raceClock(now - ra.started_at),
      note: "",
      racing: true,
      show: true,
    };
  } else {
    const axis = Math.max(
      1000,
      sb.fcvm?.median_ms ?? 0,
      sb.fcvm?.last_ms ?? 0,
      (sb.ec2?.median_ms ?? 0) + (sb.ec2?.last_capacity_wait_ms ?? 0),
      sb.ec2?.last_ms ?? 0,
      lambdaShown ? (sb.lambda?.median_ms ?? 0) + (sb.lambda?.last_capacity_wait_ms ?? 0) : 0,
      lambdaShown ? sb.lambda?.last_ms ?? 0 : 0,
    );
    fcvm = restBar("fcvm", sb.fcvm, axis);
    ec2 = restBar("ec2", sb.ec2, axis);
    if (lambdaShown) lambda = restBar("lambda", sb.lambda ?? null, axis);
    countdown =
      st.fleet.power.state !== "awake"
        ? { label: "Restarts resume once the GPUs are awake", value: "", note: "", racing: false, show: false }
        : st.next_race_at === null
        ? { label: "", value: "", note: "none scheduled", racing: false, show: false }
        : { label: "auto in", value: fmt.countdown(st.next_race_at - now), note: "every GPU, both sides at once", racing: false, show: true };
  }
  // Asleep or waking, the bars are the last measured ones: say when.
  const last = st.relaunch_all?.started_at ?? null;
  const dated = st.fleet.power.state !== "awake" && last !== null ? ` · last measured ${fmt.utcDate(last)}` : "";
  return {
    racing: ra !== null,
    title: `Restart time: click to the last GPU's first frame${dated}`,
    fcvm,
    ec2,
    lambda,
    countdown,
    button: restartButton(i),
    announce: raceAnnouncement(st),
  };
}

// ---- the race on the picture ------------------------------------------------------

/** A finished race's result card stays this long, then the strip's one line takes over. */
export const END_CARD_MS = 8000;

/** One half's big clock, centred low in its half of the image. */
export interface HalfClock {
  /** Whose clock: fcvm, EC2, or the AWS half's Lambda tiles. */
  half: Backing;
  /** Its centre, in CSS pixels, kept far enough from the image's edges that the whole card is on it. */
  x: number;
  y: number;
  /** The big figure's font size, px: 9% of the image width within 56 to 160, smaller to fit a narrow half. */
  size: number;
  /** Drawn no wider than `width` (its hatch line wraps): a card over its own tiles, beside another backing's. */
  bounded: boolean;
  /** The card's height as drawn, px (estimated from the size and its lines: the sub line, the hatch line, the steps). */
  height: number;
  /** The card's width as drawn, px (estimated from the size: a running clock is six monospace characters). */
  width: number;
  /** "0:04.9" while counting, "3.1 s" once the half is back. */
  value: string;
  state: "counting" | "done";
  /** "3 of 8 back", "+12 s and counting". */
  sub: string;
  /** "not counting 21 s AWS spent finding a spot GPU", or "". */
  hatch: string;
  /** EC2 while counting: where its slowest tile is, instance to app. Empty otherwise. */
  steps: { label: string; state: "done" | "now" | "todo" }[];
}

export interface RaceOnImage {
  /** Who started it: "Restart running", "Scheduled restart, running". */
  label: string;
  /** While the race runs: one clock per half that restarts GPUs. */
  clocks: HalfClock[];
  /** For END_CARD_MS after both halves are back: "fcvm back in 3.1 s · EC2 back in 48 s · 15× faster". */
  end: string | null;
}

/** IBM Plex Mono's advance is 0.6 em; a running clock ("0:04.9") is six of them, and the card adds 56 px of padding. */
const CLOCK_EM = 6 * 0.6;
const CLOCK_PAD = 56;
/** The narrowest the figure gets, px, however narrow its half: below this it is not read from across a room. */
const CLOCK_MIN_PX = 24;
/** The steps line ("instance → OS → driver → app") at 15 px is about this wide. */
const STEPS_W = 260;
/** The card's vertical padding, the gap between its lines, and each line under the figure at wall.css's sizes (body line-height 1.4). */
const CARD_V_PAD = 24;
const CARD_GAP = 4;
const SUB_LINE_H = 26;
const HATCH_LINE_H = 26;
const STEPS_LINE_H = 22;
/** The hatch line's text at 15 px is about this wide per character, and the line adds its own padding. */
const HATCH_CHAR_W = 7.5;
const HATCH_PAD_W = 20;
/** Room a card leaves at each side of the tiles it sits over, px, so it never touches the next card. */
const CARD_INSET = 6;

/** Where an EC2 tile is in its restart: the four steps a viewer reads. */
const EC2_STEPS = ["instance", "OS", "driver", "app"] as const;
const EC2_STEP_OF: Record<string, number> = {
  requested: 0,
  relaunching: 0,
  stopping: 0,
  waiting_capacity: 0,
  pending: 0,
  launching: 1,
  starting: 2,
};

/**
 * Where a Lambda tile is in its restart: a new version of its function is published, Lambda
 * places the environment (on a GPU instance already running, or one it starts), the renderer
 * comes up.
 */
const LAMBDA_STEPS = ["version", "environment", "app"] as const;
const LAMBDA_STEP_OF: Record<string, number> = {
  requested: 0,
  relaunching: 0,
  launching: 0,
  waiting_capacity: 1,
  starting: 1,
};

/** The bounds of some tiles, as fractions of the image. */
function boundsOf(tiles: readonly Tile[]): { x: number; y: number; w: number; h: number } | null {
  if (tiles.length === 0) return null;
  const x = Math.min(...tiles.map((t) => t.rect.x));
  const y = Math.min(...tiles.map((t) => t.rect.y));
  return { x, y, w: Math.max(...tiles.map((t) => t.rect.x + t.rect.w)) - x, h: Math.max(...tiles.map((t) => t.rect.y + t.rect.h)) - y };
}

/** A card's height for a figure of `size` px, with its hatch text on `hatchLines` lines, and with or without its steps. */
function cardHeight(size: number, hatchLines: number, steps: boolean): number {
  return CARD_V_PAD + size + CARD_GAP + SUB_LINE_H + (hatchLines > 0 ? CARD_GAP + HATCH_LINE_H * hatchLines : 0) + (steps ? CARD_GAP + STEPS_LINE_H : 0);
}

/** The hatch line's width unwrapped, px, and the lines it takes in a card at most `maxW` wide (drawn with that max-width). */
function hatchFit(hatch: string, maxW: number): { w: number; lines: number } {
  if (!hatch) return { w: 0, lines: 0 };
  const w = hatch.length * HATCH_CHAR_W + HATCH_PAD_W;
  return { w, lines: Math.max(1, Math.ceil(w / Math.max(1, maxW - CLOCK_PAD))) };
}

/** Two cards, centred at (x, y) with their width and height, share some of the image. */
function cardsMeet(a: HalfClock, b: HalfClock): boolean {
  return Math.abs(a.x - b.x) * 2 < a.width + b.width && Math.abs(a.y - b.y) * 2 < a.height + b.height;
}

/**
 * The AWS half's two clocks, moved apart when their cards would still meet (tiles of one backing
 * between the other's, or a card wider than its tiles): EC2's in the top half of the AWS side,
 * Lambda's in the bottom half, each no taller than its lane.
 */
function apart(clocks: HalfClock[], image: { x: number; y: number; w: number; h: number }, split: number): HalfClock[] {
  const e = clocks.find((c) => c.half === "ec2");
  const l = clocks.find((c) => c.half === "lambda");
  if (!e || !l || !cardsMeet(e, l)) return clocks;
  const lane = image.h / 2;
  const cx = image.x + image.w * (split + (1 - split) / 2);
  const laneW = image.w * (1 - split) - 2 * CARD_INSET;
  const place = (c: HalfClock, top: number): HalfClock => {
    const ht = hatchFit(c.hatch, laneW);
    const size = Math.max(CLOCK_MIN_PX, Math.min(c.size, (laneW - CLOCK_PAD) / CLOCK_EM, lane - 2 * CARD_INSET - cardHeight(0, ht.lines, false)));
    const width = Math.min(laneW, Math.max(size * CLOCK_EM + CLOCK_PAD, ht.w > 0 ? ht.w + CLOCK_PAD : 0));
    return { ...c, size, steps: [], bounded: true, height: cardHeight(size, ht.lines, false), width, x: cx, y: top + lane / 2 };
  };
  return clocks.map((c) => (c === e ? place(c, image.y) : c === l ? place(c, image.y + lane) : c));
}

/** How many times faster fcvm came back than EC2: "14×", "2.5×". */
export function ratioText(fcvmMs: number, ec2Ms: number): string {
  const r = ec2Ms / Math.max(1, fcvmMs);
  return `${r >= 10 ? Math.round(r) : r.toFixed(1)}×`;
}

/** The finished race in one line, from the coordinator's times, or null while a side still runs. */
export function raceResult(st: FleetState, ra: RelaunchAll): string | null {
  if (!raceOver(ra)) return null;
  const time = (h: Backing) => (raceCounts(st, ra, h).gpus > 0 ? halfTimeMs(st, ra, h) : null);
  return resultLine(time("fcvm"), time("ec2"), hasLambda(ra) ? time("lambda") : null);
}

/**
 * One race's result: each side's time, and how many times faster fcvm was than the AWS
 * side's faster backing ("fcvm back in 3.1 s · EC2 back in 48 s · Lambda back in 25 s · 8× faster").
 */
export function resultLine(f: number | null, e: number | null, l: number | null): string | null {
  const parts: string[] = [];
  if (f !== null) parts.push(`fcvm back in ${fmt.seconds(f)}`);
  if (e !== null) parts.push(`EC2 back in ${fmt.seconds(e)}`);
  if (l !== null) parts.push(`Lambda back in ${fmt.seconds(l)}`);
  const aws = [e, l].filter((x): x is number => x !== null);
  if (f !== null && aws.length > 0) parts.push(`${ratioText(f, Math.min(...aws))} faster`);
  return parts.length > 0 ? parts.join(" · ") : null;
}

/**
 * The race drawn on the image: a big clock per half while it runs, then the result for
 * END_CARD_MS. Null between races. `image` and `split` place the clocks in their halves.
 */
export function raceOnImage(st: FleetState, now: number, image: { x: number; y: number; w: number; h: number } | null, split: number): RaceOnImage | null {
  const ra = st.relaunch_all;
  if (ra === null) return null;
  const label = ra.by === "schedule" ? "Scheduled restart, running" : "Restart running";
  if (raceOver(ra)) {
    if (now - raceDoneAt(ra) >= END_CARD_MS) return null;
    return { label: "", clocks: [], end: raceResult(st, ra) };
  }
  const clocks: HalfClock[] = [];
  const fcvmMs = halfTimeMs(st, ra, "fcvm");
  const backings: Backing[] = hasLambda(ra) ? ["fcvm", "ec2", "lambda"] : ["fcvm", "ec2"];
  for (const h of backings) {
    const { gpus, back } = raceCounts(st, ra, h);
    if (gpus === 0 || !image) continue;
    const doneAt = doneAtOf(ra, h);
    const end = doneAt ?? now;
    const total = Math.max(0, end - ra.started_at);
    const wait = Math.min(total, capacityWaitMs(ra, end, h));
    const counted = total - wait;
    const done = doneAt !== null;
    const measured = halfTimeMs(st, ra, h);
    // Its own tiles' bounds: the AWS half holds EC2 tiles and Lambda tiles, each with its own
    // clock over them, wherever the grid puts them (other columns, or other rows of the same
    // ones); without Lambda tiles the EC2 clock keeps the whole half.
    const own = h === "fcvm" || !hasLambda(ra) ? null : boundsOf(halfTiles(st, h));
    const x0 = own ? image.x + image.w * own.x : h === "fcvm" ? image.x : image.x + image.w * split;
    const w = own ? image.w * own.w : h === "fcvm" ? image.w * split : image.w * (1 - split);
    const y0 = own ? image.y + image.h * own.y : image.y;
    const boxH = own ? image.h * own.h : image.h;
    let sub = done ? (gpus === 1 ? "its GPU is back" : `all ${gpus} GPUs back`) : `${back} of ${fmt.count(gpus, "GPU", "GPUs")} back`;
    // Once fcvm is back and an AWS side's counted time has passed it: how far behind it is.
    if (h !== "fcvm" && !done && fcvmMs !== null && counted >= fcvmMs + 1000) sub = `+${Math.floor((counted - fcvmMs) / 1000)} s and counting`;
    let steps: HalfClock["steps"] = [];
    if (h !== "fcvm" && !done) {
      const [names, of] = h === "ec2" ? [EC2_STEPS, EC2_STEP_OF] : [LAMBDA_STEPS, LAMBDA_STEP_OF];
      const dark = halfTiles(st, h).filter((t) => t.phase !== "running" && t.borrowed === null);
      const at = dark.length === 0 ? names.length - 1 : Math.min(...dark.map((t) => of[t.phase] ?? 0));
      steps = names.map((label, i) => ({ label, state: i < at ? "done" : i === at ? "now" : "todo" }));
    }
    const hatch = wait > 0 ? `not counting ${Math.round(wait / 1000)} s ${WAIT_SAID[h].clock}` : "";
    // Over its own tiles, the steps go when they are wider than those tiles.
    if (own && STEPS_W > w) steps = [];
    // Over its own tiles the card is no wider than they are, and its hatch line wraps.
    const maxW = own ? w - 2 * CARD_INSET : Infinity;
    const ht = hatchFit(hatch, maxW);
    // The design's clamp(56px, 9vw, 160px), no wider than its own half or tiles, and over its
    // own tiles no taller than them.
    let size = Math.max(CLOCK_MIN_PX, Math.min(Math.min(160, Math.max(56, image.w * 0.09)), (Math.min(w, maxW) - CLOCK_PAD) / CLOCK_EM));
    if (own) size = Math.max(CLOCK_MIN_PX, Math.min(size, boxH - 2 * CARD_INSET - cardHeight(0, ht.lines, steps.length > 0)));
    const height = cardHeight(size, ht.lines, steps.length > 0);
    const width = Math.min(maxW, Math.max(size * CLOCK_EM + CLOCK_PAD, steps.length > 0 ? STEPS_W : 0, own && ht.w > 0 ? ht.w + CLOCK_PAD : 0));
    // A half narrower than the card: the card moves in from the image's edge rather than off it.
    const x = Math.min(Math.max(x0 + w / 2, image.x + width / 2), image.x + image.w - width / 2);
    // Low in the half, or in the middle of its own tiles, and on the image.
    const yc = own ? y0 + boxH / 2 : image.y + image.h * 0.62;
    const y = Math.min(Math.max(yc, image.y + height / 2), image.y + image.h - height / 2);
    clocks.push({
      half: h,
      x,
      y,
      size,
      bounded: own !== null,
      height,
      width,
      value: measured !== null ? fmt.seconds(measured) : fmt.raceClock(counted),
      state: done ? "done" : "counting",
      sub,
      hatch,
      steps,
    });
  }
  return { label, clocks: image ? apart(clocks, image, split) : clocks, end: null };
}
