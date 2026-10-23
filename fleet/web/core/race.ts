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
}

export interface RestartButton {
  label: string;
  disabled: boolean;
  note: string;
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
    value: done ? fmt.seconds(counted) : fmt.raceClock(counted),
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
    parts.push(`fcvm: ${backLine(gpus, gpus, true)} ${fmt.seconds(ra.fcvm_done_at - ra.started_at)} after the restart.`);
  }
  if (ra.ec2_done_at !== null) {
    const { gpus } = raceCounts(st, ra, "ec2");
    const wait = Math.min(ra.ec2_done_at - ra.started_at, capacityWaitMs(ra, ra.ec2_done_at));
    const counted = ra.ec2_done_at - ra.started_at - wait;
    const tail = wait > 0 ? `, not counting ${fmt.seconds(wait)} when AWS had no spot GPU` : "";
    parts.push(`EC2: ${backLine(gpus, gpus, true)} ${fmt.seconds(counted)} after the restart${tail}.`);
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
  const label = `Restart all ${fmt.count(n, "GPU", "GPUs")}`;
  if (!i.online) return { label, disabled: true, note: "paused while the coordinator is offline" };
  if (st.fleet.power.state !== "awake") return { label, disabled: true, note: NOT_AWAKE_NOTE };
  const ra = runningRace(st);
  if (ra) return { label: "Restarting now", disabled: true, note: "" };
  if (i.pending.some((p) => p.name === "relaunch_all")) return { label: "Restart requested", disabled: true, note: "" };
  if (st.cooldown_until !== null && st.cooldown_until > now) {
    return { label, disabled: true, note: `available again in ${fmt.countdown(st.cooldown_until - now)}` };
  }
  if (n === 0) return { label, disabled: true, note: "no GPU to restart" };
  return { label, disabled: false, note: "" };
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
    };
  } else {
    const axis = Math.max(1000, sb.fcvm?.median_ms ?? 0, sb.fcvm?.last_ms ?? 0, (sb.ec2?.median_ms ?? 0) + (sb.ec2?.last_capacity_wait_ms ?? 0), sb.ec2?.last_ms ?? 0);
    fcvm = restBar("fcvm", sb.fcvm, axis);
    ec2 = restBar("ec2", sb.ec2, axis);
    countdown =
      st.fleet.power.state !== "awake"
        ? { label: "Restarts resume once the GPUs are awake", value: "--", note: "", racing: false }
        : st.next_race_at === null
        ? { label: "Next restart", value: "--", note: "none scheduled", racing: false }
        : { label: "Next restart in", value: fmt.countdown(st.next_race_at - now), note: "every GPU, both sides at once", racing: false };
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
