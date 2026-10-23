// View models: everything the page draws, every word it shows, every number it
// prints. React and the frame drawer read these and compute nothing themselves.

import { serverNow } from "./clock.ts";
import * as fmt from "./format.ts";
import {
  STREAM_HEIGHTS,
  fitContain,
  layoutMosaic,
  rectOnScreen,
  snap,
  splitOf,
  streamHeightFor,
  seamSegments,
  type Box,
  type MosaicLayout,
  type PlacedTile,
  type Segment,
} from "./layout.ts";
import { frameSocketUrl } from "./net.ts";
import { fitBandwidth, type StreamAsk } from "./bandwidth.ts";
import { suggestMode } from "./mode.ts";
import { FRAME_RATE, sameViewer, WAKE_PHASES, type Action, type FleetState, type Half, type IntentMsg, type Rect, type Tile } from "./protocol.ts";
import { KIND_TAG, NOT_AWAKE_NOTE, scoreView, type ScoreView } from "./race.ts";
import { sceneUnixMs } from "./scene.ts";
import { reelView, type ReelMeta, type ReelView } from "./reel.ts";
import type { ClientState, GpuSample, Notice, StreamMode } from "./store.ts";

export const SITE = "fcvm.dev";
export const TITLE = "Every GPU, live";
export const SENTENCE = "Each square is its own virtual machine with its own NVIDIA GPU. Restart them all and watch which side comes back.";
export const CAPTIONS: Record<Half, string> = {
  fcvm: "a fresh VM restored from a snapshot on a running host, GPU reset",
  ec2: "a fresh EC2 instance launched from an AMI",
};
export const NO_FLEET = "FLEET_COORD_URL is not set, so this page has no fleet to show.";

export const COLORS = {
  fcvm: { accent: "#7CC4FF", soft: "#CFE8FF", darkOutline: "#1E3A52" },
  ec2: { accent: "#F5A524", soft: "#FFE2B0", darkOutline: "#4A3410" },
} as const;

export const KIND_NAME: Record<Half, string> = { fcvm: "fcvm VM", ec2: "EC2 instance" };

/** A dark-out fades in over this long when the viewer asks for less motion (no flash). */
export const REDUCED_MOTION_DARK_FADE_MS = 700;
/** A tile lent to a visitor fades out of the image over this long, so the change reads. */
export const BORROW_FADE_MS = 1200;

const PHASE_TEXT: Record<string, string> = {
  running: "streaming",
  relaunching: "restart sent",
  requested: "restart sent",
  stopping: "stopping the old VM",
  restoring: "restoring the snapshot",
  starting: "starting the renderer",
  pending: "waiting for its host",
  launching: "launching an instance",
  terminating: "terminating the old instance",
  waiting_capacity: "AWS had no spot GPU",
  draining: "host reclaimed by AWS",
  failed: "failed",
  asleep: "asleep",
};

/**
 * The parts of a restart, in the order they run, with the label each gets. The
 * coordinator reports `<part>_ms`; `restore_ms` contains `gpu_ms`, so the
 * timeline shows the restore without the GPU and the GPU after it. `total_ms`
 * and `ack_ms` are sums or other clocks and are not parts.
 */
const PART_ORDER: [string, string][] = [
  ["capacity_wait_ms", "waited for spot capacity"],
  ["teardown_ms", "old VM stopped"],
  ["restore_ms", "snapshot restored"],
  ["gpu_ms", "GPU reset and driver loaded"],
  ["launch_ms", "instance launched"],
  ["boot_ms", "instance booted"],
  ["ready_ms", "instance boot to first frame"],
  ["container_ms", "renderer started"],
  ["first_frame_ms", "first frame"],
];
const NOT_PARTS = new Set(["total_ms", "ack_ms"]);

/** The parts of a restart's timings, in order, as [key, label, ms]. */
export function relaunchParts(phases: Record<string, number>): [string, string, number][] {
  const out: [string, string, number][] = [];
  const restore = phases["restore_ms"];
  const gpu = phases["gpu_ms"];
  for (const [k, label] of PART_ORDER) {
    let ms = phases[k];
    if (ms === undefined) continue;
    if (k === "restore_ms" && gpu !== undefined && restore !== undefined) ms = Math.max(0, restore - gpu);
    out.push([k, label, ms]);
  }
  for (const [k, ms] of Object.entries(phases)) {
    if (NOT_PARTS.has(k) || PART_ORDER.some(([p]) => p === k)) continue;
    out.push([k, k.replace(/_ms$/, "").replace(/_/g, " "), ms]);
  }
  return out;
}

export function phaseText(phase: string): string {
  return PHASE_TEXT[phase] ?? phase.replace(/_/g, " ");
}

/** The name a viewer reads for a tile: "fcvm VM 3", "EC2 VM 3". */
export function tileName(t: { id: string; half: Half }): string {
  const k = /:(\d+)$/.exec(t.id)?.[1];
  return k === undefined ? t.id : `${KIND_TAG[t.half]} VM ${k}`;
}

/** A viewer as the activity feed names it. */
export function viewerName(v: string | null, you: string | null): string {
  if (v === null || v === "") return "someone";
  if (sameViewer(v, you)) return "you";
  if (v === "schedule") return "the schedule";
  const k = /(\d+)$/.exec(v)?.[1];
  return k === undefined ? v : `viewer ${k}`;
}

export interface WallInput {
  state: ClientState;
  /** Local Unix ms. */
  now: number;
  dpr: number;
  /** Where the image and its captions may go, measured by the page. */
  mosaicArea: Box | null;
  /** Where an opened VM's stream is drawn, measured by the page. */
  focusArea: Box | null;
  /** Where the viewer's own GPU is drawn, measured by the page. */
  ownArea?: Box | null;
  /** WebCodecs H.264 is available. */
  canDecode: boolean;
  phone: boolean;
  /** The viewer asked for less motion. */
  reducedMotion?: boolean;
  /** The browser's downlink estimate (Mbit/s), when it gives one. */
  downlinkMbps?: number | null;
  /** Share of drawn tiles behind the common t lately, in direct mode. */
  lateShare?: number | null;
  /** Boxes the hover label must not cover, measured by the page. */
  avoid?: Box[];
  /** The hover label's measured size. */
  labelSize?: { w: number; h: number };
  /** The direct streams' bandwidth budget, bits per second (BANDWIDTH_BUDGET_BPS when absent). */
  budgetBps?: number;
  /** The coordinator's poster URL. */
  poster?: string | null;
  /** Where the recording's files are served from, and what its metadata says. */
  media?: { base: string; meta: ReelMeta | null };
  /** The browser asks to save data. */
  saveData?: boolean;
}

// ---- tiles -------------------------------------------------------------------

export interface HoverLabel {
  id: string;
  kind: string;
  gpu: string;
  host: string;
  last: string;
}

/** What a dark tile shows instead of a clock: how far its restart is, or what is wrong. */
export interface Glyph {
  kind: "progress" | "failed" | "capacity" | "reclaimed" | "empty" | "borrowed";
  /** Steps of a restart in this half, and how many are done. */
  steps: number;
  done: number;
}

export interface TileView {
  id: string;
  name: string;
  half: Half;
  box: Box | null;
  accent: string;
  /** Drawn dark, with its glyph and phase over it. */
  dark: boolean;
  /** Hatched: EC2 waiting for spot capacity. */
  hatch: boolean;
  glyph: Glyph;
  /** One line over a dark tile: the phase, or the failure. */
  phase: string;
  /** A second line for a failure: "retrying in 0:12". */
  detail: string;
  darkOutline: string;
  /** Leaves the image slowly (lent to a visitor), whatever the motion setting. */
  slowFade: boolean;
  /** Shown dimmed: behind the common time. */
  behind: boolean;
  ariaLabel: string;
  hover: HoverLabel;
  stats: string[];
}

const STEPS: Record<Half, Record<string, number>> = {
  fcvm: { relaunching: 0, requested: 0, stopping: 1, restoring: 2, starting: 3 },
  ec2: { relaunching: 0, requested: 0, terminating: 0, waiting_capacity: 0, pending: 1, launching: 1, starting: 2 },
};
const STEP_COUNT: Record<Half, number> = { fcvm: 4, ec2: 3 };

function relaunchMs(t: Tile): number | null {
  const lr = t.last_relaunch;
  if (!lr || lr.first_frame_at === null) return null;
  return lr.first_frame_at - lr.clicked_at;
}

function pendingRelaunch(pending: readonly IntentMsg[], t: Tile): boolean {
  return pending.some((i) => (i.name === "relaunch" && i.args["tile"] === t.id) || i.name === "relaunch_all");
}

function isLive(t: Tile): boolean {
  return t.phase === "running" && t.frame_url !== null;
}

function glyphFor(t: Tile, phase: string): Glyph {
  const steps = STEP_COUNT[t.half];
  if (phase === "borrowed" || (t.borrowed !== null && phase !== "running")) return { kind: "borrowed", steps, done: 0 };
  if (t.failure?.kind === "host_reclaimed" || phase === "draining") return { kind: "reclaimed", steps, done: 0 };
  if (phase === "failed" || t.failure) return { kind: "failed", steps, done: 0 };
  if (phase === "waiting_capacity") return { kind: t.half === "ec2" ? "capacity" : "empty", steps, done: 0 };
  const step = STEPS[t.half][phase];
  return { kind: "progress", steps, done: step ?? 0 };
}

function tileView(
  t: Tile,
  s: ClientState,
  st: FleetState,
  box: Box | null,
  srvNow: number,
  pendingIntents: readonly IntentMsg[],
): TileView {
  const requested = pendingRelaunch(pendingIntents, t) && isLive(t);
  const phase = requested ? "requested" : t.phase;
  const fs = s.frames?.tiles[sourceIdFor(t, s.ui.shownMode, st.aggregates)];
  const waitingForFrames = isLive(t) && !requested && (!fs || fs.newestT === null);
  const dark = !isLive(t) || requested;
  const accent = COLORS[t.half].accent;
  const rms = relaunchMs(t);
  const glyph = glyphFor(t, phase);
  const name = tileName(t);

  let phaseLine = phaseText(phase);
  let detail = "";
  if (glyph.kind === "empty") phaseLine = "no GPU here yet";
  if (glyph.kind === "borrowed") {
    phaseLine = "a visitor's private GPU";
    const left = t.borrowed ? Math.max(0, t.borrowed.until - srvNow) : 0;
    detail = st.relaunch_all !== null && st.relaunch_all.fcvm_done_at === null && t.half === "fcvm" ? `sitting out this race · back in ${fmt.countdown(left)}` : `back in ${fmt.countdown(left)}`;
  }
  if (glyph.kind === "reclaimed") {
    phaseLine = "host reclaimed by AWS";
    detail = "re-tiling";
  } else if (glyph.kind === "failed") {
    phaseLine = `failed: ${t.failure?.message || t.error || "no reason given"}`;
    const retry = t.failure?.retry_at ?? null;
    detail = retry === null ? "" : retry > srvNow ? `retrying in ${fmt.countdown(retry - srvNow)}` : "retrying now";
  }

  const hover: HoverLabel = {
    id: name,
    kind: KIND_TAG[t.half],
    gpu: t.gpu?.name ?? "no GPU yet",
    host: t.host ?? "no host yet",
    last: rms === null ? "no restart yet" : `last restart ${fmt.seconds(rms)}`,
  };

  const stats: string[] = [];
  if (fs && fs.newestT !== null) {
    stats.push(`${Math.round(fs.fps)} fps · ${fs.height}p`);
    const age = srvNow - sceneUnixMs(st.scene.epoch_unix_ms, fs.newestT);
    stats.push(`frame age ${fmt.millis(Math.max(0, age))}`);
  } else {
    stats.push(dark ? phaseLine : waitingForFrames ? "connecting" : "no frames");
  }
  if (t.gpu) {
    const parts = [
      t.gpu.temp_c === null ? null : `${Math.round(t.gpu.temp_c)} °C`,
      t.gpu.power_w === null ? null : `${Math.round(t.gpu.power_w)} W`,
      t.gpu.util_pct === null ? null : `${Math.round(t.gpu.util_pct)}%`,
    ].filter((x): x is string => x !== null);
    if (parts.length > 0) stats.push(parts.join(" · "));
  }
  stats.push(name);

  const ariaLabel = `${name}, ${KIND_NAME[t.half]}, ${dark ? [phaseLine, detail].filter(Boolean).join(", ") : "streaming"}${
    rms === null ? "" : `, last restart ${fmt.seconds(rms)}`
  }. Open it.`;

  return {
    id: t.id,
    name,
    half: t.half,
    box,
    accent,
    dark,
    hatch: dark && glyph.kind === "capacity",
    glyph,
    phase: phaseLine,
    detail,
    darkOutline: glyph.kind === "failed" || glyph.kind === "reclaimed" ? "#B04A3E" : COLORS[t.half].darkOutline,
    slowFade: glyph.kind === "borrowed",
    behind: fs?.behind ?? false,
    ariaLabel,
    hover,
    stats,
  };
}

// ---- counters and isolation ------------------------------------------------------

export interface CounterView {
  label: string;
  value: string;
  /** The raw total, so the page can mark a change. */
  total: number;
  lastHour: string;
  /** "GPU memory scrubs: 1,284", or "" until the scrub has shipped. */
  scrubs: string;
  /** The last wake from zero, as measured: "Last wake: 16 GPUs from zero in 2:07", or "". */
  wake: string;
}

function wakeStat(st: FleetState): string {
  const w = st.fleet.power.last_wake;
  if (w === null) return "";
  return `Last wake: ${fmt.count(w.gpus, "GPU", "GPUs")} from zero in ${fmt.minSec(w.awake_at - w.started_at)} (${fmt.utcDate(w.started_at)})`;
}

function counterView(st: FleetState): CounterView | null {
  const c = st.counters;
  if (c === null) return null;
  return {
    wake: wakeStat(st),
    label: "GPU VMs restored",
    value: fmt.thousands(c.restores_total),
    total: c.restores_total,
    lastHour: `${fmt.thousands(c.restores_last_hour)} in the last hour`,
    scrubs: st.security.gpu_scrub ? `GPU memory scrubs: ${fmt.thousands(c.scrubs_total)}` : "",
  };
}

export const REPOSITORY_URL = "https://github.com/ejc3/fcvm";

export interface IsolationView {
  title: string;
  /** The link that opens it, under the scoreboard. */
  open: string;
  sentences: string[];
  link: { label: string; href: string };
}

/**
 * How a square is isolated, in plain words. Each sentence is what
 * docs/fleet/ARCHITECTURE.md describes: one Firecracker microVM per slot with
 * its own guest kernel, hosts with the IOMMU on and every GPU bound to
 * vfio-pci, a fresh VM restored from the gold snapshot with the slot's GPU
 * (`fcvm snapshot run --snapshot <gold> --gpu <sbdf>`, the GPU attached during
 * the restore). The scrub sentence appears only when the coordinator says the
 * scrub has shipped.
 */
export function isolationView(st: FleetState | null): IsolationView {
  const sentences = [
    "Each square is its own virtual machine with its own Linux kernel.",
    "Its GPU is handed to that virtual machine alone by the hardware's IOMMU (VFIO passthrough), not shared through a proxy in the host kernel.",
    "A restart throws the old virtual machine away, restores a fresh one from a snapshot, and attaches the GPU as it restores.",
  ];
  if (st?.security.gpu_scrub) sentences.push("GPU memory is cleared before a GPU moves to the next virtual machine.");
  return {
    title: "How it's isolated",
    open: "How it's isolated",
    sentences,
    link: { label: "The fcvm repository", href: REPOSITORY_URL },
  };
}

// ---- asleep and waking -------------------------------------------------------------

const WAKE_LABEL: Record<string, string> = {
  instance_pending: "instances requested",
  booted: "hosts booted",
  store_ready: "snapshot store ready",
  agent_connected: "agents connected",
  tiles_rendering: "tiles rendering",
};

export interface WakeStep {
  phase: string;
  label: string;
  state: "done" | "partial" | "waiting";
  /** When every host had reached it, since the wake began ("0:41.2"), or "1 of 2 hosts". */
  value: string;
}

export interface SleepView {
  state: "asleep" | "waking";
  title: string;
  line: string;
  /** Since the wake began, counting; "" while asleep. */
  clock: string;
  steps: WakeStep[];
}

function sleepView(st: FleetState, srvNow: number): SleepView | null {
  const p = st.fleet.power;
  if (p.state === "awake") return null;
  if (p.state === "asleep") {
    const since = p.asleep_since === null ? "" : ` ${fmt.ago(srvNow - p.asleep_since)}`;
    return {
      state: "asleep",
      title: "The GPUs are asleep",
      line: `Nobody was watching, so the fleet scaled to zero${since}. It wakes when someone watches.`,
      clock: "",
      steps: [],
    };
  }
  const start = p.woke_at ?? srvNow;
  const hosts = p.hosts.length;
  const steps: WakeStep[] = WAKE_PHASES.map((phase) => {
    const reached = p.hosts.map((h) => h.phases.find((x) => x.phase === phase)?.at ?? null);
    const done = reached.filter((x): x is number => x !== null);
    const all = hosts > 0 && done.length === hosts;
    return {
      phase,
      label: WAKE_LABEL[phase] ?? phase.replace(/_/g, " "),
      state: all ? "done" : done.length > 0 ? "partial" : "waiting",
      value: all ? fmt.raceClock(Math.max(...done) - start) : done.length > 0 ? `${done.length} of ${fmt.count(hosts, "host", "hosts")}` : "",
    };
  });
  return {
    state: "waking",
    title: "Waking up the GPUs",
    line: "The fleet scaled to zero while nobody was watching. Each step below is reported by the hosts as it completes.",
    clock: fmt.raceClock(srvNow - start),
    steps,
  };
}

// ---- the Play drawer -----------------------------------------------------------

export const WORD_MAX = 28;

export interface PlayView {
  title: string;
  close: string;
  nudge: {
    title: string;
    note: string;
    options: { id: string; label: string; votes: string; holding: boolean; chosen: boolean }[];
    disabled: boolean;
  } | null;
  word: { label: string; placeholder: string; submit: string; maxLength: number; status: string; statusKind: "idle" | "pending" | "applied" | "refused" };
  light: { label: string; on: boolean };
  timeOfDay: { label: string; value: number; min: number; max: number; step: number; valueText: string };
  streams: { label: string; on: boolean; note: string; disabled: boolean };
  stats: { label: string; on: boolean };
  verify: { needed: boolean; title: string; text: string; error: string };
  disabled: boolean;
}

function playView(s: ClientState, st: FleetState, srvNow: number, pend: IntentMsg[], plan: DrawPlan, input: WallInput, online: boolean): PlayView {
  const pendingWord = pend.find((i) => i.name === "word");
  const lastWordNotice = [...s.notices].reverse().find((n) => n.intent === "word");
  let wordStatus = "";
  let statusKind: PlayView["word"]["statusKind"] = "idle";
  if (pendingWord) {
    wordStatus = `Checking “${String(pendingWord.args["text"] ?? "")}”`;
    statusKind = "pending";
  } else if (lastWordNotice) {
    const text = String(lastWordNotice.args["text"] ?? "");
    if (lastWordNotice.kind === "applied") {
      wordStatus = `“${text}” is in the world`;
      statusKind = "applied";
    } else {
      wordStatus = `“${text}” was not added${lastWordNotice.reason ? `: ${lastWordNotice.reason}` : ""}`;
      statusKind = "refused";
    }
  }
  const pendingTod = [...pend].reverse().find((i) => i.name === "time_of_day");
  const tod = pendingTod && typeof pendingTod.args["v"] === "number" ? (pendingTod.args["v"] as number) : st.scene.time_of_day;

  let nudge: PlayView["nudge"] = null;
  const nd = st.nudges;
  if (nd) {
    const mine = lastMine(st, s.you, "nudge", nd.next_hold_at);
    const pendingNudge = [...pend].reverse().find((i) => i.name === "nudge");
    const chosen = pendingNudge ? String(pendingNudge.args["target"] ?? "") : mine ? String(mine.args?.["target"] ?? "") : null;
    const holding = nd.targets.find((x) => x.id === nd.holding);
    const next = nd.next_hold_at === null ? "" : `next stop in ${fmt.countdown(nd.next_hold_at - srvNow)}`;
    nudge = {
      title: "Where the camera stops next",
      note: [holding ? `holding on ${holding.label}` : "flying", next].filter(Boolean).join(" · "),
      options: nd.targets.map((x) => ({
        id: x.id,
        label: x.label,
        votes: fmt.count(x.votes, "vote", "votes"),
        holding: x.id === nd.holding,
        chosen: chosen === x.id,
      })),
      disabled: !online,
    };
  }

  const hasAgg = st.aggregates.some((a) => a.frame_url !== null);
  const sug = suggestMode({
    aggregators: st.aggregates.length,
    downlinkMbps: input.downlinkMbps ?? null,
    directBps: plan.estimatedBps,
    lateShare: input.lateShare ?? null,
  });
  const direct = !hasAgg || s.ui.mode === "direct";
  let streamsNote = direct
    ? `${fmt.count(plan.direct.length, "stream", "streams")} straight from the VMs, about ${fmt.mbps(plan.estimatedBps)}`
    : `${fmt.count(plan.aggregated.length, "stream", "streams")}, one per host, each joining its VMs' frames`;
  if (direct && hasAgg && sug.mode === "aggregated") streamsNote += `. ${sug.reason}`;
  if (s.ui.mode !== s.ui.shownMode) streamsNote = `switching. ${streamsNote}`;

  const ts = s.turnstile;
  return {
    title: "Play",
    close: "Close",
    nudge,
    word: { label: "Write a word into the world", placeholder: "a word", submit: "Write", maxLength: WORD_MAX, status: wordStatus, statusKind },
    light: { label: s.ui.lightMode ? "Click the picture to drop it" : "Drop a light", on: s.ui.lightMode },
    timeOfDay: { label: "Time of day", value: tod, min: 0, max: 1 - 1 / 96, step: 1 / 96, valueText: fmt.timeOfDay(tod) },
    streams: { label: "See every VM's own stream", on: direct, note: streamsNote, disabled: !hasAgg },
    stats: { label: "Show stream stats", on: s.ui.stats },
    verify: {
      needed: ts.sitekey !== null && !ts.verified,
      title: "One check first",
      text: ts.verifying ? "Checking" : "One check before your first action, so the fleet stays for people.",
      error: ts.error ?? "",
    },
    disabled: !online,
  };
}

/** This viewer's latest committed action of a kind since a time. */
function lastMine(st: FleetState, you: string | null, name: string, before: number | null): Action | null {
  let best: Action | null = null;
  for (const a of st.actions) {
    if (a.name !== name || a.outcome === "refused" || !sameViewer(a.viewer, you)) continue;
    if (before !== null && a.at > before) continue;
    if (best === null || a.at >= best.at) best = a;
  }
  return best;
}

// ---- activity feed -------------------------------------------------------------

function actionText(a: Action, st: FleetState, you: string | null): string | null {
  if (a.outcome === "refused") return null;
  const who = viewerName(a.viewer, you);
  const args = a.args ?? {};
  switch (a.name) {
    case "relaunch_all":
      return a.viewer === "schedule" ? "the scheduled restart of every GPU began" : `${who} restarted every GPU`;
    case "relaunch": {
      const t = st.tiles.find((x) => x.id === args["tile"] || x.slot === args["tile"]);
      return `${who} restarted ${t ? tileName(t) : "a VM"}`;
    }
    case "word":
      return typeof args["text"] === "string" ? `${who} wrote “${args["text"]}”` : `${who} wrote a word`;
    case "light":
      return `${who} dropped a light`;
    case "time_of_day":
      return typeof args["v"] === "number" ? `${who} set the time of day to ${fmt.timeOfDay(args["v"] as number)}` : null;
    case "nudge": {
      const target = st.nudges?.targets.find((x) => x.id === args["target"]);
      return `${who} chose ${target ? target.label : "a stop"} for the camera`;
    }
    case "claim_gpu":
      return `${who} got ${who === "you" ? "your" : "their"} own GPU`;
    default:
      return null;
  }
}

export interface FeedItem {
  key: string;
  text: string;
  when: string;
}

function feedView(st: FleetState, srvNow: number, you: string | null, n: number): FeedItem[] {
  const out: FeedItem[] = [];
  const sorted = [...st.actions].sort((a, b) => b.at - a.at);
  for (const a of sorted) {
    const text = actionText(a, st, you);
    if (text === null) continue;
    out.push({ key: a.id ?? `${a.at}-${out.length}`, text, when: fmt.ago(srvNow - a.at) });
    if (out.length === n) break;
  }
  return out;
}

// ---- one VM opened -------------------------------------------------------------

export interface TimelineStep {
  label: string;
  value: string;
  /** Start and length as fractions of the whole restart. */
  start: number;
  fraction: number;
  shade: number;
  hatch: boolean;
}

export interface Reading {
  label: string;
  value: string;
  /** SVG polyline points in a 100 x 24 box, oldest left. */
  points: string;
}

export interface FocusView {
  id: string;
  accent: string;
  title: string;
  kind: string;
  state: string;
  back: string;
  streamTag: string;
  rows: { term: string; value: string }[];
  restart: { label: string; disabled: boolean };
  timelineTitle: string;
  timelineTotal: string;
  timelineNote: string;
  timeline: TimelineStep[];
  /** While it restarts: where it is now. */
  now: string;
  readingsTitle: string;
  readings: Reading[];
  placed: { box: Box; streamHeight: number } | null;
}

/** The renderer's own resolution for one tile: what "native size" asks for. */
export const NATIVE_TILE_HEIGHT = 720;

function sparkline(samples: readonly GpuSample[], key: "temp_c" | "power_w" | "util_pct"): string {
  const vals = samples.map((x) => x[key]).filter((v): v is number => v !== null);
  if (vals.length < 2) return "";
  const lo = Math.min(...vals);
  const hi = Math.max(...vals);
  const span = hi - lo || 1;
  return vals.map((v, i) => `${((i / (vals.length - 1)) * 100).toFixed(1)},${(22 - ((v - lo) / span) * 20).toFixed(1)}`).join(" ");
}

/** Where a VM's stream goes: its native height at one device pixel per pixel, smaller only when the area is. */
export function nativePlacement(area: Box, rect: Rect, imageAspect: number, dpr: number): { box: Box; streamHeight: number } {
  const aspect = (rect.w * imageAspect) / rect.h;
  const nativeH = NATIVE_TILE_HEIGHT / dpr;
  const fit = fitContain(area, aspect);
  const h = Math.min(fit.h, nativeH);
  const w = h * aspect;
  const x = area.x + (area.w - w) / 2;
  const y = area.y + (area.h - h) / 2;
  const box = { x: snap(x, dpr), y: snap(y, dpr), w: snap(x + w, dpr) - snap(x, dpr), h: snap(y + h, dpr) - snap(y, dpr) };
  return { box, streamHeight: Math.min(NATIVE_TILE_HEIGHT, streamHeightFor(box.h * dpr)) };
}

function timelineOf(phases: Record<string, number>): { steps: TimelineStep[]; total: number } {
  const parts = relaunchParts(phases);
  const total = parts.reduce((a, [, , ms]) => a + ms, 0);
  let at = 0;
  const steps = parts.map(([k, label, ms], i) => {
    const step = {
      label,
      value: ms < 10_000 ? `${(ms / 1000).toFixed(2)} s` : fmt.seconds(ms),
      start: total > 0 ? at / total : 0,
      fraction: total > 0 ? ms / total : 0,
      shade: i,
      hatch: k === "capacity_wait_ms",
    };
    at += ms;
    return step;
  });
  return { steps, total };
}

function focusView(s: ClientState, st: FleetState, tv: TileView, t: Tile, input: WallInput, srvNow: number, online: boolean): FocusView {
  const fs = s.frames?.tiles[`d:${t.id}`];
  const lr = t.last_relaunch;
  const { steps, total } = lr ? timelineOf(lr.phases) : { steps: [], total: 0 };
  const rms = relaunchMs(t);
  const placed = input.focusArea ? nativePlacement(input.focusArea, t.rect, st.image_aspect, input.dpr) : null;
  const history = s.gpuHistory[t.id] ?? [];
  const g = t.gpu;
  const readings: Reading[] = g
    ? [
        { label: "Temperature", value: g.temp_c === null ? "--" : `${Math.round(g.temp_c)} °C`, points: sparkline(history, "temp_c") },
        { label: "Power", value: g.power_w === null ? "--" : `${Math.round(g.power_w)} W`, points: sparkline(history, "power_w") },
        { label: "GPU busy", value: g.util_pct === null ? "--" : `${Math.round(g.util_pct)}%`, points: sparkline(history, "util_pct") },
      ]
    : [];
  let streamTag = "no frames yet";
  if (fs && fs.newestT !== null && placed) {
    const scale = placed.box.h * input.dpr / fs.height;
    streamTag = `${fs.height}p · ${Math.round(fs.fps)} fps · ${scale >= 0.995 ? "native size" : `shown at ${Math.round(scale * 100)}%`}`;
  }
  const restarting = tv.dark && lr !== null && lr.first_frame_at === null;
  const up = lr?.first_frame_at ?? null;
  return {
    id: t.id,
    accent: tv.accent,
    title: tv.name,
    kind: t.half === "fcvm" ? `fcvm: ${CAPTIONS.fcvm}` : `EC2: ${CAPTIONS.ec2}`,
    state: tv.dark ? [tv.phase, tv.detail].filter(Boolean).join(", ") : "streaming",
    back: "Back to the wall",
    streamTag,
    rows: [
      { term: "GPU", value: g?.name ?? "none yet" },
      { term: "Host", value: t.host ?? "none yet" },
      { term: "VM", value: t.vm ?? "none yet" },
      { term: "Started", value: t.generation === null ? "--" : fmt.count(t.generation, "time", "times") },
      { term: "Up for", value: up === null || tv.dark ? "--" : fmt.span(srvNow - up) },
      ...(t.error ? [{ term: "Error", value: t.error }] : []),
    ],
    restart: { label: "Restart this VM", disabled: !online || t.gpu === null || tv.dark },
    timelineTitle: restarting ? "This restart" : "Last restart",
    timelineTotal: rms === null ? (restarting && lr ? fmt.raceClock(srvNow - lr.clicked_at) : "--") : fmt.seconds(rms),
    timelineNote: "From the click to its first frame, measured by the coordinator.",
    timeline: steps.map((x) => ({ ...x, fraction: total > 0 ? x.fraction : 0 })),
    now: restarting ? `now: ${tv.phase}` : "",
    readingsTitle: "GPU, live",
    readings,
    placed,
  };
}

// ---- your own GPU --------------------------------------------------------------

/** A camera button on the viewer's own GPU: a turn (fractions of the image) or a move along the view. */
export interface CameraMove {
  label: string;
  symbol: string;
  dx: number;
  dy: number;
  dive: number;
}

/** One press turns the camera this far, as a fraction of the image. */
export const CAMERA_KEY_STEP = 0.08;

export const CAMERA_MOVES: readonly CameraMove[] = [
  { label: "Turn left", symbol: "←", dx: -CAMERA_KEY_STEP, dy: 0, dive: 0 },
  { label: "Turn right", symbol: "→", dx: CAMERA_KEY_STEP, dy: 0, dive: 0 },
  { label: "Tilt up", symbol: "↑", dx: 0, dy: -CAMERA_KEY_STEP, dive: 0 },
  { label: "Tilt down", symbol: "↓", dx: 0, dy: CAMERA_KEY_STEP, dive: 0 },
  { label: "Move closer", symbol: "+", dx: 0, dy: 0, dive: 1 },
  { label: "Move back", symbol: "−", dx: 0, dy: 0, dive: -1 },
];

/** Keys that move the camera on the viewer's own GPU, by CAMERA_MOVES index. */
export const CAMERA_KEYS: Record<string, number> = { ArrowLeft: 0, ArrowRight: 1, ArrowUp: 2, ArrowDown: 3, "+": 4, "=": 4, "-": 5 };

export interface OwnView {
  title: string;
  cameraLabel: string;
  moves: readonly CameraMove[];
  status: string;
  timerLabel: string;
  timer: string;
  timerRunning: boolean;
  countdownLabel: string;
  countdown: string;
  ended: boolean;
  back: string;
  help: string;
  latency: string;
  gpu: string;
  steps: TimelineStep[];
  placed: { box: Box; streamHeight: number } | null;
}

export interface OwnButton {
  label: string;
  note: string;
  disabled: boolean;
}

export const OWN_GPU_SECONDS = 60;

function ownButton(st: FleetState, srvNow: number, online: boolean, busy: boolean): OwnButton {
  const b = st.borrow;
  const label = "Get your own GPU";
  if (st.fleet.power.state !== "awake") return { label, note: NOT_AWAKE_NOTE, disabled: true };
  if (b === null || b.cap === 0) return { label, note: "this fleet lends no GPUs", disabled: true };
  if (b.lent >= b.cap) {
    const when = b.next_return_at === null ? "soon" : `in ${fmt.countdown(b.next_return_at - srvNow)}`;
    const out = b.cap === 1 ? "the one GPU we lend is out" : `all ${b.cap} GPUs we lend are out`;
    return { label, note: `${out}; try again ${when}`, disabled: true };
  }
  return { label, note: `borrow one of the wall's GPUs for a minute (${b.cap - b.lent} of ${b.cap} free)`, disabled: !online || busy };
}

function ownView(s: ClientState, st: FleetState, srvNow: number, input: WallInput): OwnView | null {
  const own = s.own;
  if (own.requestId === null && own.claim === null) return null;
  const c = own.claim;
  const clickedAt = c?.clicked_at ?? (own.requestedAt !== null ? serverNow(s.clock, own.requestedAt) : srvNow);
  const first = c?.first_frame_at ?? null;
  const ended = c?.phase === "ended";
  const placed = input.ownArea && c && !ended ? (() => {
    const box = fitContain(input.ownArea!, st.image_aspect);
    const b = { x: snap(box.x, input.dpr), y: snap(box.y, input.dpr), w: snap(box.x + box.w, input.dpr) - snap(box.x, input.dpr), h: snap(box.y + box.h, input.dpr) - snap(box.y, input.dpr) };
    return { box: b, streamHeight: Math.min(NATIVE_TILE_HEIGHT, streamHeightFor(b.h * input.dpr)) };
  })() : null;
  const left = c?.expires_at != null ? c.expires_at - srvNow : null;
  const status = ended
    ? c?.ended ?? "Your minute is up. The GPU went back to the pool."
    : first === null
      ? c ? "Restoring a VM for you" : "Asking for a GPU"
      : "Your GPU is streaming";
  return {
    title: "Your own GPU",
    cameraLabel: "Camera",
    moves: CAMERA_MOVES,
    status,
    timerLabel: "Click to first frame",
    timer: first === null ? fmt.raceClock(srvNow - clickedAt) : fmt.seconds(first - clickedAt),
    timerRunning: first === null && !ended,
    countdownLabel: ended ? "" : "Back to the pool in",
    countdown: ended ? "" : left === null ? fmt.countdown(OWN_GPU_SECONDS * 1000) : fmt.countdown(left),
    ended,
    back: "Back to the wall",
    help: "Drag to turn the camera, scroll to move closer. Arrow keys and plus or minus work too.",
    latency: s.lastInputLatencyMs === null ? "move the camera to measure input to pixels" : `input to pixels: ${fmt.millis(s.lastInputLatencyMs)}`,
    gpu: c?.gpu ?? "",
    steps: c ? timelineOf(c.phases).steps : [],
    placed,
  };
}

// ---- what the frame drawer gets -------------------------------------------------

/** One frame stream the drawer may open: a tile's renderer, an aggregator, or the viewer's own GPU. */
export interface DrawSource {
  /** `d:<tile id>`, `a:<aggregate id>`, or `o:<claim id>`. */
  id: string;
  /** Frame socket URL with h (and q), or null: do not connect. */
  url: string | null;
  baseUrl: string | null;
  height: number;
  /** Where it is drawn, or null: keep streaming, do not draw. */
  box: Box | null;
}

/** A tile's own state, drawn the same way whatever stream its pixels came from. */
export interface DrawTile {
  id: string;
  half: Half;
  box: Box | null;
  /** Drawn dark over any frame (a restart, no GPU, a failure). */
  dark: boolean;
  /** Outline of a dark tile. */
  outline: string;
  /** Its dark-out fades over at least BORROW_FADE_MS. */
  slowFade: boolean;
}

export interface DrawPlan {
  /** The mode this viewer wants; the drawer switches to it when it can do so without a gap. */
  mode: StreamMode;
  direct: DrawSource[];
  aggregated: DrawSource[];
  tiles: DrawTile[];
  /** With a VM opened or the viewer's own GPU: that stream, drawn alone. */
  focus: DrawSource | null;
  image: Box | null;
  dpr: number;
  darkFill: string;
  /** What the direct streams cost at the renderer's bit rate, bits per second. */
  estimatedBps: number;
  /** The poster drawn where no stream has shown a frame yet. */
  poster: string | null;
  /** Hold the picture still. */
  paused: boolean;
  /** A tile going dark fades over this long (0: at once). */
  darkFadeMs: number;
  /** The tab has been hidden long enough: close every frame stream, keep the last picture. */
  suspended: boolean;
  /** The fleet is asleep or waking: the poster fills the image again, under tiles as they arrive. */
  posterFull: boolean;
  /**
   * The layout generation this plan is, and the scene t from which its tiles are
   * drawn. A plan of a newer generation is drawn once every one of its regions has
   * a frame at or after `atT`; until then the drawer keeps drawing the previous one.
   */
  gen: number;
  atT: number;
}

/** The source a tile's pixels come from in a mode: its own stream, or the aggregate holding it. */
export function sourceIdFor(t: Tile, mode: StreamMode, aggregates: readonly { id: string; rect: Rect; tiles?: string[] }[]): string {
  if (mode === "direct") return `d:${t.id}`;
  const listed = aggregates.find((g) => g.tiles?.includes(t.id));
  if (listed) return `a:${listed.id}`;
  const cx = t.rect.x + t.rect.w / 2;
  const cy = t.rect.y + t.rect.h / 2;
  const a = aggregates.find((g) => cx >= g.rect.x && cx < g.rect.x + g.rect.w && cy >= g.rect.y && cy < g.rect.y + g.rect.h);
  return a ? `a:${a.id}` : `d:${t.id}`;
}

// ---- seams, labels, captions ------------------------------------------------------

export interface SeamView {
  /** Fade every boundary in (the pointer is over the image, or a press or key focus). */
  visible: boolean;
  segments: Segment[];
  /** One device pixel, in CSS pixels. */
  lineWidth: number;
  outline: { box: Box; color: string } | null;
  /** The label's top-left corner, placed off the boxes to avoid and inside the image's area. */
  label: (HoverLabel & { accent: string; x: number; y: number }) | null;
}

function overlaps(a: Box, b: Box): boolean {
  return a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
}

/**
 * Where the hover label goes: the first of above, below, beside and inside the
 * tile that stays inside the area and covers none of the boxes to avoid.
 * Failing all, the same places pushed past what they hit, then the one covering
 * the least.
 */
export function placeLabel(tile: Box, size: { w: number; h: number }, area: Box, avoid: readonly Box[]): { x: number; y: number } {
  const gap = 6;
  const { w, h } = size;
  const candidates: { x: number; y: number }[] = [
    { x: tile.x, y: tile.y - h - gap },
    { x: tile.x + tile.w - w, y: tile.y - h - gap },
    { x: tile.x, y: tile.y + tile.h + gap },
    { x: tile.x + tile.w - w, y: tile.y + tile.h + gap },
    { x: tile.x + tile.w + gap, y: tile.y },
    { x: tile.x - w - gap, y: tile.y },
    { x: tile.x + gap, y: tile.y + gap },
    { x: tile.x + tile.w - w - gap, y: tile.y + tile.h - h - gap },
  ];
  const inside = (c: { x: number; y: number }) => c.x >= area.x && c.y >= area.y && c.x + w <= area.x + area.w && c.y + h <= area.y + area.h;
  const cover = (c: { x: number; y: number }) => {
    const b = { x: c.x, y: c.y, w, h };
    let n = 0;
    for (const a of avoid) {
      if (!overlaps(a, b)) continue;
      n += (Math.min(a.x + a.w, b.x + b.w) - Math.max(a.x, b.x)) * (Math.min(a.y + a.h, b.y + b.h) - Math.max(a.y, b.y));
    }
    return n;
  };
  const clean = candidates.find((c) => inside(c) && cover(c) === 0);
  if (clean) return clean;
  const pushed: { x: number; y: number }[] = [];
  for (const c of candidates) {
    for (const a of avoid) {
      if (!overlaps(a, { x: c.x, y: c.y, w, h })) continue;
      pushed.push({ x: c.x, y: a.y + a.h + gap }, { x: a.x + a.w + gap, y: c.y }, { x: c.x, y: a.y - h - gap }, { x: a.x - w - gap, y: c.y });
    }
  }
  const moved = pushed.find((c) => inside(c) && cover(c) === 0);
  if (moved) return moved;
  const fits = candidates.filter(inside);
  const pool = fits.length > 0 ? fits : candidates;
  return pool.reduce((best, c) => (cover(c) < cover(best) ? c : best));
}

/** How long the returning label stays after the borrowed tile's first frame. */
export const RETURNING_HOLD_MS = 4000;

const RESTORING_PHASES = new Set(["borrowed", "relaunching", "requested", "stopping", "restoring", "starting", "pending"]);

/** Whether this viewer's borrowed tile is still coming back (not yet streaming for RETURNING_HOLD_MS). */
export function stillReturning(st: FleetState, id: string | null, srvNow: number): boolean {
  if (id === null) return false;
  const t = st.tiles.find((x) => x.id === id);
  if (!t) return false;
  // Still lent, or restoring back in; a tile that is not on its way back (its host reclaimed, failed) drops the label.
  if (t.borrowed !== null || RESTORING_PHASES.has(t.phase)) return true;
  if (t.phase !== "running") return false;
  const ff = t.last_relaunch?.first_frame_at ?? null;
  return ff !== null && srvNow - ff < RETURNING_HOLD_MS;
}

function seamView(s: ClientState, layout: MosaicLayout | null, tiles: TileView[], input: WallInput, returning: boolean): SeamView {
  const dpr = input.dpr;
  const hovered = s.ui.hoverId ?? s.ui.keyId;
  const target = hovered ?? (returning ? s.ui.returning : null);
  const back = hovered === null && returning;
  const found = target === null ? null : tiles.find((t) => t.id === target) ?? null;
  const tv = found && back ? { ...found, hover: { ...found.hover, last: found.dark ? "your borrowed GPU, coming back" : "your borrowed GPU is back" } } : found;
  const segments = layout ? seamSegments(layout.tiles, layout.image, dpr) : [];
  let label: SeamView["label"] = null;
  if (tv && tv.box && layout) {
    const area = input.mosaicArea ?? layout.image;
    const at = placeLabel(tv.box, input.labelSize ?? { w: 230, h: 92 }, area, input.avoid ?? []);
    label = { ...tv.hover, accent: tv.accent, x: at.x, y: at.y };
  }
  return {
    visible: s.ui.pointerOverImage || s.ui.keyId !== null,
    segments,
    lineWidth: 1 / dpr,
    outline: tv && tv.box ? { box: tv.box, color: tv.accent } : null,
    label,
  };
}

export interface StageLabels {
  /** "fcvm" and "EC2" on backplates either side of the seam, top of the image. */
  seam: { half: Half; text: string; x: number; y: number; align: "right" | "left" }[];
  /** One line under each side, side by side, or one under the other when a side is narrow. */
  captions: { half: Half; tag: string; text: string; box: Box; align: "left" | "right" }[];
  /** A side with no GPUs live: a small label where it would be, or null. */
  empty: { half: Half; text: string; x: number; y: number; align: "right" | "left" } | null;
}

/** Least height kept under the image for the captions. */
export const CAPTION_BAND = 44;
/** A side narrower than this puts the captions one under the other, full width. */
export const CAPTION_MIN_W = 260;

export const EMPTY_SIDE: Record<Half, string> = {
  fcvm: "fcvm: no GPUs live right now",
  ec2: "EC2: no GPUs live right now",
};

/** Caption text: 14 px IBM Plex Sans averages about 7.4 px a character; lines are 18 px. */
const CAPTION_CHAR_PX = 7.4;
const CAPTION_LINE_PX = 18;

function captionLines(half: Half, width: number): number {
  return Math.max(1, Math.ceil(((CAPTIONS[half].length + 6) * CAPTION_CHAR_PX) / Math.max(1, width)));
}

/**
 * The captions' boxes for an image and where its fcvm side ends: each under its
 * side, or stacked full width when a side is too narrow; a side with no tiles has none.
 */
function captionBoxes(img: Box, split: number): { half: Half; box: Box }[] {
  const y = img.y + img.h + 6;
  const sides: Half[] = split <= 0 ? ["ec2"] : split >= 1 ? ["fcvm"] : ["fcvm", "ec2"];
  if (sides.length === 1) return [{ half: sides[0]!, box: { x: img.x, y, w: img.w, h: 0 } }];
  const mid = img.x + img.w * split;
  const fw = mid - img.x - 8;
  const ew = img.x + img.w - mid - 8;
  if (Math.min(fw, ew) >= CAPTION_MIN_W) {
    return [
      { half: "fcvm", box: { x: img.x, y, w: fw, h: 0 } },
      { half: "ec2", box: { x: mid + 8, y, w: ew, h: 0 } },
    ];
  }
  const first = captionLines("fcvm", img.w) * CAPTION_LINE_PX;
  return [
    { half: "fcvm", box: { x: img.x, y, w: img.w, h: 0 } },
    { half: "ec2", box: { x: img.x, y: y + first, w: img.w, h: 0 } },
  ];
}

/** Height the captions take under an image of this width. */
function captionBand(imgW: number, split: number): number {
  const boxes = captionBoxes({ x: 0, y: 0, w: imgW, h: 0 }, split);
  const stacked = boxes.length === 2 && boxes[1]!.box.y > boxes[0]!.box.y;
  const lines = boxes.map((b) => captionLines(b.half, b.box.w));
  const total = stacked ? lines.reduce((a, b) => a + b, 0) : Math.max(...lines);
  return Math.max(CAPTION_BAND, total * CAPTION_LINE_PX + 8);
}

function stageLabels(layout: MosaicLayout | null): StageLabels {
  if (!layout) return { seam: [], captions: [], empty: null };
  const img = layout.image;
  const mid = layout.halfLineX;
  const captions = captionBoxes(img, layout.split).map((c) => ({
    half: c.half,
    tag: KIND_TAG[c.half],
    text: CAPTIONS[c.half],
    box: c.box,
    // Under its own side, the EC2 caption keeps to the right edge.
    align: (c.half === "ec2" && c.box.x > img.x ? "right" : "left") as "left" | "right",
  }));
  if (mid === null) {
    // One side has every GPU: it takes the whole width, and the other says it is empty.
    const present: Half = layout.split >= 1 ? "fcvm" : "ec2";
    const absent: Half = present === "fcvm" ? "ec2" : "fcvm";
    return {
      seam: [
        present === "fcvm"
          ? { half: "fcvm", text: "fcvm", x: img.x + 10, y: img.y + 10, align: "left" }
          : { half: "ec2", text: "EC2", x: img.x + img.w - 10, y: img.y + 10, align: "right" },
      ],
      captions,
      empty:
        absent === "ec2"
          ? { half: "ec2", text: EMPTY_SIDE.ec2, x: img.x + img.w - 10, y: img.y + 10, align: "right" }
          : { half: "fcvm", text: EMPTY_SIDE.fcvm, x: img.x + 10, y: img.y + 10, align: "left" },
    };
  }
  return {
    seam: [
      { half: "fcvm", text: "fcvm", x: mid - 8, y: img.y + 10, align: "right" },
      { half: "ec2", text: "EC2", x: mid + 8, y: img.y + 10, align: "left" },
    ],
    captions,
    empty: null,
  };
}

// ---- the whole page ------------------------------------------------------------

export interface Banner {
  kind: "decode" | "connecting" | "offline" | "reclaimed";
  text: string;
}

export interface WallView {
  title: string;
  site: string;
  sentence: string;
  ready: boolean;
  version: number;
  online: boolean;
  banners: Banner[];
  layout: MosaicLayout | null;
  tiles: TileView[];
  score: ScoreView | null;
  labels: StageLabels;
  play: PlayView | null;
  playButton: { label: string; open: boolean };
  pause: { label: string; on: boolean };
  feed: FeedItem[];
  counter: CounterView | null;
  isolation: IsolationView;
  /** Asleep or waking: the poster full size and a card instead of tiles. */
  sleep: SleepView | null;
  /** The recording of the wall, while asleep or waking and until the live image replaces it. */
  reel: ReelView | null;
  focus: FocusView | null;
  own: OwnView | null;
  ownButton: OwnButton | null;
  /** Shown when this viewer's claim was refused. */
  ownRefusal: { text: string; dismiss: string } | null;
  viewers: string;
  help: string;
  rotateHint: string;
  plan: DrawPlan;
  seams: SeamView;
  /** This viewer's borrowed tile is coming back into the image (labelled until it streams). */
  returning: boolean;
  gridLabel: string;
  notices: { key: string; text: string }[];
  /** What the streams cost, for the stats switch. */
  bandwidth: string;
}

/**
 * The tiles the canvas draws now: the shown generation's places and rectangles,
 * each with its latest state when the newest snapshot still has it.
 */
export function drawnTiles(s: ClientState): Tile[] {
  const st = s.accepted;
  if (!st) return [];
  const shown = s.shownLayout;
  if (!shown || shown.gen === st.layout.gen) return st.tiles;
  const now = new Map(st.tiles.map((t) => [t.id, t]));
  return shown.tiles.map((old) => {
    const cur = now.get(old.id);
    return cur ? { ...cur, rect: old.rect } : old;
  });
}

export function pendingIntents(s: ClientState): IntentMsg[] {
  return s.pending.map((p) => p.intent);
}

/**
 * The image's area: the stage less the band its captions sit in, tall enough for
 * the captions at the widths the two sides will have (`split`: where fcvm ends).
 */
export function imageArea(stage: Box, phone: boolean, aspect: number = 16 / 9, split = 0.5): Box {
  if (phone) return stage;
  let band = CAPTION_BAND;
  for (let i = 0; i < 3; i++) band = captionBand(Math.min(stage.w, (stage.h - band) * aspect), split);
  return { x: stage.x, y: stage.y, w: stage.w, h: Math.max(0, stage.h - band) };
}

export function wallView(input: WallInput): WallView {
  const s = input.state;
  const st = s.accepted;
  const srvNow = serverNow(s.clock, input.now);
  const online = s.connection === "open";
  const banners: Banner[] = [];
  if (!input.canDecode) {
    banners.push({ kind: "decode", text: "This browser cannot decode H.264 with WebCodecs, so the picture stays still. Chrome, Edge and Safari 16.4 or later can." });
  }
  if (!st) {
    banners.push({ kind: "connecting", text: s.connection === "closed" ? "Cannot reach the fleet yet. Trying again." : "Connecting to the fleet." });
  } else if (!online) {
    banners.push({ kind: "offline", text: "The coordinator is offline. The picture still streams straight from the VMs; the controls are paused until it is back." });
  }
  if (st) {
    const draining = st.fleet.hosts.filter((h) => h.draining);
    if (draining.length > 0) {
      banners.push({
        kind: "reclaimed",
        text: `AWS reclaimed ${draining.length === 1 ? "an fcvm host" : `${draining.length} fcvm hosts`}. Its squares stay dark while the image is re-tiled onto the GPUs left.`,
      });
    }
  }
  const poster = input.poster ?? null;
  const emptyPlan: DrawPlan = {
    mode: s.ui.mode,
    direct: [],
    aggregated: [],
    tiles: [],
    focus: null,
    image: null,
    dpr: input.dpr,
    darkFill: "#06080B",
    estimatedBps: 0,
    poster,
    paused: s.ui.paused,
    darkFadeMs: input.reducedMotion ? REDUCED_MOTION_DARK_FADE_MS : 0,
    suspended: s.ui.suspended,
    posterFull: false,
    gen: s.accepted?.layout.gen ?? 0,
    atT: 0,
  };
  const area = input.mosaicArea
    ? imageArea(input.mosaicArea, input.phone, s.accepted?.image_aspect, s.accepted ? splitOf(input.canDecode ? drawnTiles(s) : s.accepted.tiles) : 0.5)
    : null;
  const common = {
    title: TITLE,
    site: SITE,
    sentence: SENTENCE,
    version: s.version,
    online,
    banners,
    playButton: { label: "Play", open: s.ui.drawer },
    pause: { label: s.ui.paused ? "Resume" : "Pause", on: s.ui.paused },
    help: "Click a square to look at its VM.",
    rotateHint: "Turn your phone sideways for a bigger picture.",
  };
  if (!st) {
    // Before the first snapshot the poster still goes where the image will be.
    const image = area ? fitContain(area, 16 / 9) : null;
    return {
      ...common,
      ready: false,
      layout: null,
      tiles: [],
      score: null,
      labels: { seam: [], captions: [], empty: null },
      play: null,
      feed: [],
      counter: null,
      isolation: isolationView(null),
      sleep: null,
      reel: null,
      focus: null,
      own: null,
      ownButton: null,
      ownRefusal: null,
      viewers: "",
      plan: { ...emptyPlan, image },
      seams: { visible: false, segments: [], lineWidth: 1 / input.dpr, outline: null, label: null },
      returning: false,
      gridLabel: "",
      notices: [],
      bandwidth: "",
    };
  }
  const pend = pendingIntents(s);
  const sleep = sleepView(st, srvNow);
  // Two tilings while a re-tile settles: the plan is the newest generation (its
  // streams open now), and the overlays follow the one the canvas still draws.
  const shownTiles = input.canDecode ? drawnTiles(s) : st.tiles;
  const planLayout = area ? layoutMosaic(area, st.tiles, input.dpr, st.image_aspect) : null;
  const layout = shownTiles === st.tiles ? planLayout : area ? layoutMosaic(area, shownTiles, input.dpr, st.image_aspect) : null;
  const placedById = new Map<string, PlacedTile>(layout?.tiles.map((p) => [p.id, p]) ?? []);
  const placedPlan = new Map<string, PlacedTile>(planLayout?.tiles.map((p) => [p.id, p]) ?? []);
  // Asleep or waking, a tile that is not rendering yet shows the poster, not a dark square.
  const asleepFix = (tv: TileView) => (sleep && tv.dark ? { ...tv, dark: false, hatch: false } : tv);
  const tiles = shownTiles.map((t) => asleepFix(tileView(t, s, st, placedById.get(t.id)?.box ?? null, srvNow, pend)));
  const planViews = shownTiles === st.tiles ? tiles : st.tiles.map((t) => asleepFix(tileView(t, s, st, placedPlan.get(t.id)?.box ?? null, srvNow, pend)));
  const focusTile = s.ui.focusId === null ? null : st.tiles.find((t) => t.id === s.ui.focusId) ?? null;
  const focus = focusTile ? focusView(s, st, tiles.find((x) => x.id === focusTile.id)!, focusTile, input, srvNow, online) : null;
  const own = ownView(s, st, srvNow, input);
  const covered = focus !== null || own !== null;

  // Direct: each tile asks for the device pixels it covers; with a VM opened
  // or the viewer's own GPU on screen, the others (still streaming, not drawn)
  // ask for the smallest; then small tiles drop to q=lo and the tallest step
  // down until the streams fit the bandwidth budget.
  const opened = focus && focus.placed && !own ? focusTile : null;
  const asks: StreamAsk[] = st.tiles.filter(isLive).map((t) => {
    const focused = opened !== null && opened.id === t.id;
    const height = focused ? focus!.placed!.streamHeight : covered ? STREAM_HEIGHTS[0] : placedPlan.get(t.id)?.streamHeight ?? 144;
    return { id: t.id, rect: t.rect, height, keep: focused };
  });
  const fitted = fitBandwidth(asks, st.image_aspect, input.budgetBps);
  const direct: DrawSource[] = st.tiles.filter(isLive).map((t) => {
    const height = fitted.heights.get(t.id) ?? 144;
    const q = fitted.quality.get(t.id) ?? "std";
    return {
      id: `d:${t.id}`,
      url: frameSocketUrl(t.frame_url!, height, q),
      baseUrl: t.frame_url,
      height,
      box: covered ? (opened?.id === t.id ? focus!.placed!.box : null) : placedPlan.get(t.id)?.box ?? null,
    };
  });
  // Aggregated: each region is one big tile; it asks for the height it covers.
  const aggregated: DrawSource[] = st.aggregates
    .filter((g) => g.frame_url !== null)
    .map((g) => {
      const box = planLayout ? rectOnScreen(planLayout.image, g.rect, input.dpr) : null;
      const height = covered || !box ? STREAM_HEIGHTS[0] : streamHeightFor(box.h * input.dpr);
      return { id: `a:${g.id}`, url: frameSocketUrl(g.frame_url!, height), baseUrl: g.frame_url, height, box: covered ? null : box };
    });
  let focusSource: DrawSource | null = null;
  if (own && own.placed && s.own.claim?.frame_url) {
    const c = s.own.claim;
    focusSource = { id: `o:${c.id}`, url: frameSocketUrl(c.frame_url!, own.placed.streamHeight), baseUrl: c.frame_url, height: own.placed.streamHeight, box: own.placed.box };
  } else if (opened) {
    focusSource = direct.find((d) => d.id === `d:${opened.id}`) ?? null;
  }
  const plan: DrawPlan = {
    ...emptyPlan,
    mode: st.aggregates.some((a) => a.frame_url !== null) ? s.ui.mode : "direct",
    direct,
    aggregated,
    focus: focusSource,
    estimatedBps: fitted.bps,
    image: covered ? null : planLayout?.image ?? null,
    posterFull: sleep !== null,
    gen: st.layout.gen,
    atT: st.layout.at_frame / FRAME_RATE,
    tiles: st.tiles.map((t, i) => {
      const tv = planViews[i]!;
      return { id: t.id, half: t.half, box: covered ? null : placedPlan.get(t.id)?.box ?? null, dark: tv.dark, outline: tv.darkOutline, slowFade: tv.slowFade };
    }),
  };

  const play = playView(s, st, srvNow, pend, plan, input, online);
  const returning = stillReturning(st, s.ui.returning, srvNow);
  return {
    ...common,
    ready: true,
    layout,
    tiles,
    score: scoreView({ st, now: srvNow, pending: pend, online }),
    labels: stageLabels(covered ? null : layout),
    play,
    feed: feedView(st, srvNow, s.you, input.phone ? 1 : 3),
    counter: counterView(st),
    isolation: isolationView(st),
    sleep,
    reel: input.media
      ? reelView({
          base: input.media.base,
          meta: input.media.meta,
          power: st.fleet.power.state,
          reelOn: s.ui.reelOn,
          allLive: s.frames?.allLive ?? false,
          reducedMotion: input.reducedMotion ?? false,
          saveData: input.saveData ?? false,
        })
      : null,
    focus,
    own,
    ownButton: ownButton(st, srvNow, online, s.own.requestId !== null || (s.own.claim !== null && s.own.claim.phase !== "ended")),
    ownRefusal: s.own.refused === null ? null : { text: `No GPU is free right now: ${s.own.refused}. Try again in a minute.`, dismiss: "OK" },
    viewers: fmt.count(st.viewers.count, "person watching", "people watching"),
    plan,
    seams: seamView(s, covered ? null : layout, tiles, input, returning),
    returning,
    bandwidth: `${fmt.count(plan.direct.length, "direct stream", "direct streams")}, about ${fmt.mbps(plan.estimatedBps)} at the renderer's bit rate`,
    notices: s.notices.flatMap((n) => {
      const text = noticeText(n);
      return text === null ? [] : [{ key: n.id, text }];
    }),
    gridLabel: `The image, ${fmt.count(st.tiles.length, "square", "squares")}: fcvm on the left, EC2 on the right. Arrow keys move between squares, Enter opens one.`,
  };
}

/** Notices older than this are dropped. */
export const NOTICE_MS = 8000;

const INTENT_LABEL: Record<string, string> = {
  relaunch_all: "The restart",
  relaunch: "The restart",
  light: "The light",
  time_of_day: "The time of day",
  nudge: "Your choice of stop",
};

/** A refused action, as the page says it. Words and claims have their own lines. */
export function noticeText(n: Notice): string | null {
  if (n.kind !== "refused" || n.intent === "word" || n.intent === "claim_gpu" || n.intent === "claim_camera" || n.intent === "release_gpu") return null;
  const what = INTENT_LABEL[n.intent] ?? n.intent;
  return `${what} was not applied${n.reason ? `: ${n.reason}` : ""}`;
}

/** Whether the page must verify the viewer before sending an action. */
export function needsVerify(s: ClientState): boolean {
  return s.turnstile.sitekey !== null && !s.turnstile.verified;
}

