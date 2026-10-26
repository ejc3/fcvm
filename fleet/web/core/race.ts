// The race: the scoreboard's two bars, the shared countdown to the next
// restart, the one button that starts one, and what a screen reader hears
// when a half comes back. Every figure is the coordinator's measurement; the
// page only formats it and draws it to scale.

import * as fmt from "./format.ts";
import type { FleetState, Half, HalfScore, IntentMsg, RelaunchAll, Tile } from "./protocol.ts";

export const KIND_TAG: Record<Half, string> = { fcvm: "fcvm", ec2: "EC2" };

/** The axis a race is drawn on before EC2 has a median of its own. */
const FIRST_RACE_AXIS_MS = 60_000;

export interface BarView {
  half: Half;
  name: string;
  /** Solid bar, as a fraction of the axis. */
  solid: number;
  /** Hatched part (EC2 waiting for spot capacity), as a fraction of the axis. */
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
  countdown: CountdownView;
  button: RestartButton;
  /** The aria-live text: each half's result once it is back, in plain words. */
  announce: string;
}

function raceOver(ra: RelaunchAll): boolean {
  return ra.fcvm_done_at !== null && ra.ec2_done_at !== null;
}

/** The running race, or null between races. */
export function runningRace(st: FleetState): RelaunchAll | null {
  const ra = st.relaunch_all;
  return ra !== null && !raceOver(ra) ? ra : null;
}

function halfTiles(st: FleetState, h: Half): Tile[] {
  return st.tiles.filter((t) => t.half === h);
}

/** GPUs a race restarted in a half, and how many of them stream again. */
function raceCounts(st: FleetState, ra: RelaunchAll, h: Half): { gpus: number; back: number } {
  const gpusField = h === "fcvm" ? ra.fcvm_gpus : ra.ec2_gpus;
  const pending = h === "fcvm" ? ra.fcvm_pending : ra.ec2_pending;
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
export function halfTimeMs(st: FleetState, ra: RelaunchAll, h: Half): number | null {
  const doneAt = h === "fcvm" ? ra.fcvm_done_at : ra.ec2_done_at;
  if (doneAt === null) return null;
  const measured = h === "fcvm" ? ra.fcvm_ms : ra.ec2_ms;
  if (measured !== null) return measured;
  const kept = st.races.find((r) => r.started_at === ra.started_at)?.[h]?.time_ms ?? null;
  if (kept !== null) return kept;
  const wall = doneAt - ra.started_at;
  return h === "ec2" ? wall - Math.min(wall, capacityWaitMs(ra, doneAt)) : wall;
}

/** Milliseconds EC2 has waited for capacity in this race, up to now. */
export function capacityWaitMs(ra: RelaunchAll, now: number): number {
  return ra.ec2_capacity_wait_ms + (ra.ec2_waiting_since !== null ? Math.max(0, now - ra.ec2_waiting_since) : 0);
}

function backLine(gpus: number, back: number, done: boolean): string {
  if (gpus === 0) return "nothing to restart";
  if (done) return gpus === 1 ? "its GPU is streaming again" : `all ${gpus} GPUs streaming again`;
  return `${back} of ${fmt.count(gpus, "GPU", "GPUs")} streaming again`;
}

function measuredOn(n: number): string {
  return `measured on ${fmt.count(n, "GPU", "GPUs")}`;
}

function restBar(h: Half, score: HalfScore | null, axis: number): BarView {
  const base = { half: h, name: KIND_TAG[h], hatchFirst: false, live: false, done: false };
  if (score === null) {
    return { ...base, solid: 0, hatch: 0, lastMark: null, value: "--", valueLabel: "median restart", line: "no restart measured yet", hatchLabel: "", measured: "" };
  }
  const wait = h === "ec2" ? score.last_capacity_wait_ms : 0;
  return {
    ...base,
    solid: score.median_ms / axis,
    hatch: wait / axis,
    lastMark: score.last_ms / axis,
    value: fmt.seconds(score.median_ms),
    valueLabel: score.races === 1 ? "one restart" : `median of ${score.races} restarts`,
    line: `last restart ${fmt.seconds(score.last_ms)}`,
    hatchLabel: wait > 0 ? `last restart: AWS had no spot GPU for ${fmt.seconds(wait)}, not counted` : "",
    measured: measuredOn(score.measured_on),
  };
}

function raceBar(st: FleetState, ra: RelaunchAll, h: Half, now: number, axis: number): BarView {
  const doneAt = h === "fcvm" ? ra.fcvm_done_at : ra.ec2_done_at;
  const done = doneAt !== null;
  const end = doneAt ?? now;
  const total = Math.max(0, end - ra.started_at);
  const wait = h === "ec2" ? Math.min(total, capacityWaitMs(ra, end)) : 0;
  const counted = total - wait;
  const { gpus, back } = raceCounts(st, ra, h);
  const waitingNow = h === "ec2" && ra.ec2_waiting_since !== null && !done;
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
      ? `AWS had no spot GPU: waiting ${fmt.countdown(wait)}, not counted`
      : wait > 0
        ? `AWS had no spot GPU for ${fmt.seconds(wait)}, not counted`
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
  if (ra.ec2_done_at !== null) {
    const { gpus } = raceCounts(st, ra, "ec2");
    const wait = Math.min(ra.ec2_done_at - ra.started_at, capacityWaitMs(ra, ra.ec2_done_at));
    const tail = wait > 0 ? `, not counting ${fmt.seconds(wait)} when AWS had no spot GPU` : "";
    parts.push(`EC2: ${backLine(gpus, gpus, true)} ${fmt.seconds(halfTimeMs(st, ra, "ec2")!)} after the restart${tail}.`);
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
export function sittingOut(st: FleetState, h: Half): number {
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
  let countdown: CountdownView;
  if (ra) {
    // EC2's bar grows across the panel; the axis is what EC2 usually takes, or
    // longer once this race passes it.
    const expected = sb.ec2 ? sb.ec2.median_ms + sb.ec2.last_capacity_wait_ms : FIRST_RACE_AXIS_MS;
    const ec2End = ra.ec2_done_at ?? now;
    const axis = Math.max(expected, ec2End - ra.started_at, (ra.fcvm_done_at ?? now) - ra.started_at, 1000);
    fcvm = raceBar(st, ra, "fcvm", now, axis);
    ec2 = raceBar(st, ra, "ec2", now, axis);
    countdown = {
      label: ra.by === "schedule" ? "Scheduled restart, running" : "Restart running",
      value: fmt.raceClock(now - ra.started_at),
      note: "",
      racing: true,
      show: true,
    };
  } else {
    const axis = Math.max(1000, sb.fcvm?.median_ms ?? 0, sb.fcvm?.last_ms ?? 0, (sb.ec2?.median_ms ?? 0) + (sb.ec2?.last_capacity_wait_ms ?? 0), sb.ec2?.last_ms ?? 0);
    fcvm = restBar("fcvm", sb.fcvm, axis);
    ec2 = restBar("ec2", sb.ec2, axis);
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
  half: Half;
  /** Its centre, in CSS pixels. */
  x: number;
  y: number;
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

/** How many times faster fcvm came back than EC2: "14×", "2.5×". */
export function ratioText(fcvmMs: number, ec2Ms: number): string {
  const r = ec2Ms / Math.max(1, fcvmMs);
  return `${r >= 10 ? Math.round(r) : r.toFixed(1)}×`;
}

/** The finished race in one line, from the coordinator's times, or null while a half still runs. */
export function raceResult(st: FleetState, ra: RelaunchAll): string | null {
  if (ra.fcvm_done_at === null || ra.ec2_done_at === null) return null;
  const f = raceCounts(st, ra, "fcvm").gpus > 0 ? halfTimeMs(st, ra, "fcvm") : null;
  const e = raceCounts(st, ra, "ec2").gpus > 0 ? halfTimeMs(st, ra, "ec2") : null;
  const parts: string[] = [];
  if (f !== null) parts.push(`fcvm back in ${fmt.seconds(f)}`);
  if (e !== null) parts.push(`EC2 back in ${fmt.seconds(e)}`);
  if (f !== null && e !== null) parts.push(`${ratioText(f, e)} faster`);
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
    if (now - Math.max(ra.fcvm_done_at!, ra.ec2_done_at!) >= END_CARD_MS) return null;
    return { label: "", clocks: [], end: raceResult(st, ra) };
  }
  const clocks: HalfClock[] = [];
  const fcvmMs = halfTimeMs(st, ra, "fcvm");
  for (const h of ["fcvm", "ec2"] as const) {
    const { gpus, back } = raceCounts(st, ra, h);
    if (gpus === 0 || !image) continue;
    const doneAt = h === "fcvm" ? ra.fcvm_done_at : ra.ec2_done_at;
    const end = doneAt ?? now;
    const total = Math.max(0, end - ra.started_at);
    const wait = h === "ec2" ? Math.min(total, capacityWaitMs(ra, end)) : 0;
    const counted = total - wait;
    const done = doneAt !== null;
    const measured = halfTimeMs(st, ra, h);
    const x0 = h === "fcvm" ? image.x : image.x + image.w * split;
    const w = h === "fcvm" ? image.w * split : image.w * (1 - split);
    let sub = done ? (gpus === 1 ? "its GPU is back" : `all ${gpus} GPUs back`) : `${back} of ${fmt.count(gpus, "GPU", "GPUs")} back`;
    // Once fcvm is back and EC2's counted time has passed it: how far behind EC2 is.
    if (h === "ec2" && !done && fcvmMs !== null && counted >= fcvmMs + 1000) sub = `+${Math.floor((counted - fcvmMs) / 1000)} s and counting`;
    let steps: HalfClock["steps"] = [];
    if (h === "ec2" && !done) {
      const dark = halfTiles(st, "ec2").filter((t) => t.phase !== "running" && t.borrowed === null);
      const at = dark.length === 0 ? EC2_STEPS.length - 1 : Math.min(...dark.map((t) => EC2_STEP_OF[t.phase] ?? 0));
      steps = EC2_STEPS.map((label, i) => ({ label, state: i < at ? "done" : i === at ? "now" : "todo" }));
    }
    clocks.push({
      half: h,
      x: x0 + w / 2,
      y: image.y + image.h * 0.62,
      value: measured !== null ? fmt.seconds(measured) : fmt.raceClock(counted),
      state: done ? "done" : "counting",
      sub,
      hatch: wait > 0 ? `not counting ${Math.round(wait / 1000)} s AWS spent finding a spot GPU` : "",
      steps,
    });
  }
  return { label, clocks, end: null };
}
