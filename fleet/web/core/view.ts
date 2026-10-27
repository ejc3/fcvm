// View models: everything the page draws, every word it shows, every number it
// prints. React and the frame drawer read these and compute nothing themselves.

import { serverNow } from "./clock.ts";
import * as fmt from "./format.ts";
import {
  DETAIL_TILE_PX,
  STREAM_HEIGHTS,
  boxesMeet,
  fitContain,
  layoutMosaic,
  layoutZoom,
  rectOnScreen,
  snap,
  streamHeightFor,
  seamSegments,
  tileAspectOf,
  tileAt,
  zoomTarget,
  type Box,
  type MosaicLayout,
  type PlacedTile,
  type Segment,
  type ZoomInsets,
  type ZoomLayout,
  type ZoomStep,
} from "./layout.ts";
import { FAMILY_PROBE_ATTEMPTS, FAMILY_PROBE_MS, frameSocketUrl, streamUrl, type Family } from "./net.ts";
import { fitBandwidth, type StreamAsk } from "./bandwidth.ts";
import { suggestMode } from "./mode.ts";
import { FRAME_RATE, sameViewer, WAKE_PHASES, type Action, type Aggregate, type FleetState, type Half, type IntentMsg, type Nudges, type Rect, type Tile } from "./protocol.ts";
import { KIND_TAG, NOT_AWAKE_NOTE, raceOnImage, ratioText, runningRace, scoreView, type RaceOnImage, type ScoreView } from "./race.ts";
import { sceneUnixMs } from "./scene.ts";
import { reelView, type ReelMeta, type ReelView } from "./reel.ts";
import { ownAutopilot, type ClientState, type FrameStats, type GpuSample, type Notice, type StreamMode, type TileFrameStats } from "./store.ts";
import { sceneAgeMs } from "./lag.ts";

export const SITE = "fcvm.dev";
export const TITLE = "Every GPU, live";
export const SENTENCE = "Each square is its own virtual machine with its own NVIDIA GPU. Restart them all and watch which side comes back.";
/** The tag on each half of the image: who it is, and how its VMs start. */
export const SIDE_TAGS: Record<Half, { tag: string; text: string }> = {
  fcvm: { tag: "FCVM", text: "restored from a snapshot" },
  ec2: { tag: "EC2", text: "launched from an AMI" },
};
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
  waiting_capacity: "AWS had no spot GPU",
  failed: "failed",
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
  /**
   * The zoom between the wall and an opened VM as the page animates it. Absent, an opened VM
   * (ui.focusId) is shown fully zoomed.
   */
  zoom?: ZoomStep | null;
  /** Where an opened VM may fill: the viewport (mosaicArea when absent). */
  zoomArea?: Box | null;
  /** Room an opened VM leaves for the page's controls (zoomInsetsFor(phone, 0) when absent). */
  zoomInsets?: ZoomInsets;
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
  kind: "progress" | "failed" | "capacity" | "empty" | "borrowed";
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
  /** While a race runs and this tile is still dark: its own clock since the race began, or "". */
  clock: string;
  /** Once it has relit in the running race: its measured time, big for CHIP_FRESH_MS, then in its corner. */
  chip: { text: string; fresh: boolean } | null;
}

/** A relit tile's time stays big this long before it moves to the tile's corner. */
export const CHIP_FRESH_MS = 3000;

const STEPS: Record<Half, Record<string, number>> = {
  fcvm: { relaunching: 0, requested: 0, stopping: 1, restoring: 2, starting: 3 },
  ec2: { relaunching: 0, requested: 0, stopping: 0, waiting_capacity: 0, pending: 1, starting: 2 },
};
const STEP_COUNT: Record<Half, number> = { fcvm: 4, ec2: 3 };

function relaunchMs(t: Tile): number | null {
  const lr = t.last_relaunch;
  if (!lr || lr.first_frame_at === null) return null;
  return lr.first_frame_at - lr.clicked_at;
}

function pendingRelaunch(pending: readonly IntentMsg[], t: Tile): boolean {
  return pending.some((i) => (i.name === "relaunch" && i.args["tile"] === t.id) || i.name === "restart_now");
}

/** Running with a renderer to connect to, over either family (the overlays follow this). */
function isLive(t: Tile): boolean {
  return t.phase === "running" && (t.frame_url !== null || t.frame_url_v4 !== null);
}

function glyphFor(t: Tile, phase: string): Glyph {
  const steps = STEP_COUNT[t.half];
  if (phase === "borrowed" || (t.borrowed !== null && phase !== "running")) return { kind: "borrowed", steps, done: 0 };
  if (phase === "failed" || t.failure) return { kind: "failed", steps, done: 0 };
  if (phase === "waiting_capacity") return { kind: t.half === "ec2" ? "capacity" : "empty", steps, done: 0 };
  const step = STEPS[t.half][phase];
  return { kind: "progress", steps, done: step ?? 0 };
}

/** What a tile that is not up is waiting for, in a phrase that follows "waiting for". */
function waitingFor(t: Tile): string {
  if (t.phase === "waiting_capacity") return t.half === "ec2" ? "EC2 capacity" : "a free GPU";
  return t.half === "ec2" ? "the instance to launch" : "the VM to restore";
}

/**
 * The hover card's three lines. A line with nothing to report says why: a tile that is up
 * says the coordinator has not named it, one that is not up says what it waits for. The
 * host line is the tile's public place, never the agent's own host name.
 */
export function hoverLabel(t: Tile, name: string, rms: number | null): HoverLabel {
  const up = t.phase === "running";
  const wait = `waiting for ${waitingFor(t)}`;
  return {
    id: name,
    kind: KIND_TAG[t.half],
    gpu: t.gpu?.name ?? (up ? "GPU model not reported" : wait),
    host: t.place ?? (up ? "location not reported" : wait),
    last: rms !== null ? `last restart ${fmt.seconds(rms)}` : up ? "launch time not recorded" : "no launch recorded",
  };
}

/**
 * A tile's frame ages on the coordinator's clock, ms: when its frames arrived, came out of
 * the decoder and were first drawn in step (p50 of the drawer's recent frames), and the
 * age shown on its card: the drawn age, or before any draw, the newest frame's age when
 * the drawer reported (not now: the report can be up to 500 ms old).
 */
export function frameAges(
  fs: TileFrameStats,
  s: ClientState,
  epochUnixMs: number,
): { shown: number; arrive: number | null; decoded: number | null; drawn: number | null } {
  const off = s.clock.offsetMs ?? 0;
  const lag = fs.lag;
  const at = (x: number | null | undefined) => (x === null || x === undefined ? null : sceneAgeMs(x, off, epochUnixMs));
  const drawn = at(lag?.drawn);
  const newest = fs.newestT === null || s.frames === null ? null : serverNow(s.clock, s.frames.at) - sceneUnixMs(epochUnixMs, fs.newestT);
  return { shown: drawn ?? newest ?? 0, arrive: at(lag?.arrive), decoded: at(lag?.decoded), drawn };
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
  if (glyph.kind === "failed") {
    phaseLine = `failed: ${t.failure?.message || t.error || "no reason given"}`;
    const retry = t.failure?.retry_at ?? null;
    detail = retry === null ? "" : retry > srvNow ? `retrying in ${fmt.countdown(retry - srvNow)}` : "retrying now";
  }

  const hover = hoverLabel(t, name, rms);

  const stats: string[] = [];
  if (fs && fs.newestT !== null) {
    stats.push(`${Math.round(fs.fps)} fps · ${fs.height}p`);
    const ages = frameAges(fs, s, st.scene.epoch_unix_ms);
    stats.push(`frame age ${fmt.millis(Math.max(0, ages.shown))}`);
    if (ages.arrive !== null && ages.decoded !== null && ages.drawn !== null) {
      stats.push(`arrives ${fmt.millis(Math.max(0, ages.arrive))} · decoded ${fmt.millis(Math.max(0, ages.decoded))} · drawn ${fmt.millis(Math.max(0, ages.drawn))}`);
    }
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

  // In a running race: a dark tile counts, a relit one shows when it came back.
  const ra = runningRace(st);
  const ff = t.last_relaunch?.first_frame_at ?? null;
  const clock = ra !== null && dark && glyph.kind === "progress" ? fmt.raceClock(Math.max(0, srvNow - ra.started_at)) : "";
  const chip = ra !== null && !dark && ff !== null && ff >= ra.started_at ? { text: fmt.seconds(ff - ra.started_at), fresh: srvNow - ff < CHIP_FRESH_MS } : null;

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
    darkOutline: glyph.kind === "failed" ? "#B04A3E" : COLORS[t.half].darkOutline,
    slowFade: glyph.kind === "borrowed",
    behind: fs?.behind ?? false,
    ariaLabel,
    hover,
    stats,
    clock,
    chip,
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
    value: fmt.thousands(c.restores),
    total: c.restores,
    lastHour: `${fmt.thousands(c.restores_last_hour)} in the last hour`,
    scrubs: scrubShipped(st) ? `GPU memory scrubs: ${fmt.thousands(c.scrubs)}` : "",
  };
}

/** The page speaks of the GPU memory scrub only once an agent has reported one. */
function scrubShipped(st: FleetState | null): boolean {
  return (st?.counters?.scrubs ?? 0) > 0;
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
 * the restore). The scrub sentence appears only once the coordinator has counted
 * a scrub an agent reported (`counters.scrubs`).
 */
export function isolationView(st: FleetState | null): IsolationView {
  const sentences = [
    "Each square is its own virtual machine with its own Linux kernel.",
    "Its GPU is handed to that virtual machine alone by the hardware's IOMMU (VFIO passthrough), not shared through a proxy in the host kernel.",
    "A restart throws the old virtual machine away, restores a fresh one from a snapshot, and attaches the GPU as it restores.",
  ];
  if (scrubShipped(st)) sentences.push("GPU memory is cleared before a GPU moves to the next virtual machine.");
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

/** The median of the coordinator's measured wakes (`wakes`, else `last_wake`), in ms, or null. */
export function wakeMedianMs(p: FleetState["fleet"]["power"]): number | null {
  const ms = (p.wakes.length > 0 ? p.wakes : p.last_wake ? [p.last_wake] : []).map((w) => w.awake_at - w.started_at).sort((a, b) => a - b);
  if (ms.length === 0) return null;
  const m = Math.floor(ms.length / 2);
  return ms.length % 2 === 1 ? ms[m]! : (ms[m - 1]! + ms[m]!) / 2;
}

function sleepView(st: FleetState, srvNow: number): SleepView | null {
  const p = st.fleet.power;
  if (p.state === "awake") return null;
  const gpus = p.last_wake?.gpus ?? p.wakes.at(-1)?.gpus ?? 0;
  if (p.state === "asleep") {
    const median = wakeMedianMs(p);
    return {
      state: "asleep",
      title: "Asleep.",
      line:
        median === null || gpus === 0
          ? "The GPUs wake when someone watches."
          : `${fmt.count(gpus, "GPU", "GPUs")} wake in about ${fmt.minSec(median)} when you stay.`,
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
    title: gpus > 0 ? `WAKING ${fmt.count(gpus, "GPU", "GPUs")}` : "WAKING THE GPUs",
    line: "The fleet scaled to zero while nobody was watching. Each step is reported by the hosts as it completes.",
    clock: fmt.raceClock(srvNow - start),
    steps,
  };
}

// ---- the Play drawer -----------------------------------------------------------

export const WORD_MAX = 28;

/** How a viewer's message is drawn in the sky. The wire values are the coordinator's `text_style`. */
export type TextStyle = "lockup" | "skywriting" | "landmark";

export const TEXT_STYLES: readonly { value: TextStyle; label: string; thumb: string; alt: string }[] = [
  { value: "lockup", label: "Lockup", thumb: "/text-styles/lockup.jpg", alt: "The message set as type above the clock, upper left" },
  { value: "skywriting", label: "Skywriting", thumb: "/text-styles/skywriting.jpg", alt: "The message written in smoke in the sky" },
  { value: "landmark", label: "Landmark", thumb: "/text-styles/landmark.jpg", alt: "The message in big letters standing on a ridge" },
];

/** The arguments of a word intent: the text and the style its sender picked. */
export function wordArgs(text: string, style: TextStyle): { text: string; text_style: TextStyle } {
  return { text, text_style: style };
}

export interface PlayView {
  title: string;
  close: string;
  nudge: {
    title: string;
    note: string;
    options: { id: string; label: string; votes: string; holding: boolean; chosen: boolean }[];
    disabled: boolean;
  } | null;
  word: {
    label: string;
    placeholder: string;
    submit: string;
    maxLength: number;
    status: string;
    statusKind: "idle" | "pending" | "applied" | "refused";
    /** The three ways to draw it, one picked (Lockup unless the viewer chose another). */
    style: { legend: string; options: { value: TextStyle; label: string; thumb: string; alt: string; selected: boolean }[] };
  };
  light: { label: string; on: boolean };
  streams: { label: string; on: boolean; note: string; disabled: boolean };
  /** `family`: the stats drawer's line on the address family every tile streams over. */
  stats: { label: string; on: boolean; family: string };
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

  let nudge: PlayView["nudge"] = null;
  const nd = st.nudges;
  if (nd) {
    // This viewer's pick in the open vote (a pick before it opened was counted in an earlier one).
    const mine = lastMine(st, s.you, "nudge", nd.opened_at);
    const pendingNudge = [...pend].reverse().find((i) => i.name === "nudge");
    const chosen = pendingNudge ? String(pendingNudge.args["target"] ?? "") : mine ? String(mine.args?.["target"] ?? "") : null;
    const holding = nd.holding_label ?? nd.targets.find((x) => x.id === nd.holding)?.label ?? null;
    const next = nd.next_hold_at === null ? "" : `next stop in ${fmt.countdown(nd.next_hold_at - srvNow)}`;
    nudge = {
      title: nd.next !== null && nd.next_label !== null ? `Where the camera stops after ${nd.next_label}` : "Where the camera stops next",
      note: [holding !== null ? `holding on ${holding}` : "flying", next].filter(Boolean).join(" · "),
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

  const hasAgg = aggregatedAvailable(st.aggregates, s.net.family);
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
    title: "Direct the scene",
    close: "Close",
    nudge,
    word: {
      label: "Write a word into the world",
      placeholder: "a word",
      submit: "Write",
      maxLength: WORD_MAX,
      status: wordStatus,
      statusKind,
      style: { legend: "How it is drawn", options: TEXT_STYLES.map((o) => ({ ...o, selected: o.value === s.ui.textStyle })) },
    },
    light: { label: s.ui.lightMode ? "Click the picture to drop it" : "Drop a light", on: s.ui.lightMode },
    streams: { label: "See every VM's own stream", on: direct, note: streamsNote, disabled: !hasAgg },
    stats: { label: "Show stream stats", on: s.ui.stats, family: familyLine(s.net) },
    verify: {
      needed: ts.sitekey !== null && !ts.verified,
      title: "One check first",
      text: ts.verifying ? "Checking" : "One check before your first action, so the fleet stays for people.",
      error: ts.error ?? "",
    },
    disabled: !online,
  };
}

/** This viewer's latest committed action of a kind at or after a time. */
function lastMine(st: FleetState, you: string | null, name: string, since: number | null): Action | null {
  let best: Action | null = null;
  for (const a of st.actions) {
    if (a.name !== name || a.outcome === "refused" || !sameViewer(a.viewer, you)) continue;
    if (since !== null && a.at < since) continue;
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
    case "restart_now":
      return `${who} restarted every GPU`;
    case "relaunch": {
      const t = st.tiles.find((x) => x.id === args["tile"] || x.slot === args["tile"]);
      return `${who} restarted ${t ? tileName(t) : "a VM"}`;
    }
    case "word":
      return typeof args["text"] === "string" ? `${who} wrote “${args["text"]}”` : `${who} wrote a word`;
    case "light":
      return `${who} dropped a light`;
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

/** How long the newest action stays on screen as a toast. */
export const TOAST_MS = 4000;

/** The newest action, for TOAST_MS after it happened, or null. */
function toastView(st: FleetState, srvNow: number, you: string | null): FeedItem | null {
  const newest = feedView(st, srvNow, you, 1)[0];
  if (!newest) return null;
  const age = srvNow - newest.at;
  return age >= -1000 && age < TOAST_MS ? { key: newest.key, text: newest.text, when: newest.when } : null;
}

function feedView(st: FleetState, srvNow: number, you: string | null, n: number): (FeedItem & { at: number })[] {
  const items: { at: number; key: string; text: string }[] = [];
  for (const a of st.actions) {
    const text = actionText(a, st, you);
    if (text !== null) items.push({ at: a.at, key: a.id ?? `${a.at}-${items.length}`, text });
  }
  // The owner's cadence is not an intent, so it is not in `actions`: the race says who started it.
  const ra = st.relaunch_all;
  if (ra !== null && ra.by === "schedule") items.push({ at: ra.started_at, key: `race-${ra.started_at}`, text: "the scheduled restart of every GPU began" });
  return items
    .sort((a, b) => b.at - a.at)
    .slice(0, n)
    .map((x) => ({ key: x.key, text: x.text, at: x.at, when: fmt.ago(srvNow - x.at) }));
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
  /** Where the VM is drawn at this step of the zoom, and the height its stream is asked for. */
  placed: { box: Box; streamHeight: number } | null;
}

/** The renderer's own resolution for one tile: what the viewer's own GPU asks for. */
export const NATIVE_TILE_HEIGHT = 720;

/** How dark the picture around an opened VM is: next to it, and at the far corners. */
export const ZOOM_DIM_NEAR = 0.55;
export const ZOOM_DIM_FAR = 0.82;
/** The side sheet's width on a wide screen, CSS pixels. */
export const ZOOM_SHEET_W = 380;

/**
 * Room an opened VM leaves around it. On a phone it is full width, under a bar and above the
 * bottom sheet's handle; elsewhere a margin all round wide enough to show the tiles next to it,
 * with the bar over the top, and clear of a side sheet `sheet` wide when that costs no size.
 */
export function zoomInsetsFor(phone: boolean, sheet: number): ZoomInsets {
  return phone ? { top: 56, right: 0, bottom: 76, left: 0, sheet: 0 } : { top: 72, right: 56, bottom: 48, left: 56, sheet };
}

function sparkline(samples: readonly GpuSample[], key: "temp_c" | "power_w" | "util_pct"): string {
  const vals = samples.map((x) => x[key]).filter((v): v is number => v !== null);
  if (vals.length < 2) return "";
  const lo = Math.min(...vals);
  const hi = Math.max(...vals);
  const span = hi - lo || 1;
  return vals.map((v, i) => `${((i / (vals.length - 1)) * 100).toFixed(1)},${(22 - ((v - lo) / span) * 20).toFixed(1)}`).join(" ");
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

function focusView(
  s: ClientState,
  tv: TileView,
  t: Tile,
  input: WallInput,
  srvNow: number,
  online: boolean,
  placed: { box: Box; streamHeight: number } | null,
): FocusView {
  const fs = s.frames?.tiles[`d:${t.id}`];
  const lr = t.last_relaunch;
  const { steps, total } = lr ? timelineOf(lr.phases) : { steps: [], total: 0 };
  const rms = relaunchMs(t);
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
    const scale = (placed.box.h * input.dpr) / fs.height;
    streamTag = `${fs.height}p · ${Math.round(fs.fps)} fps · ${Math.abs(scale - 1) < 0.005 ? "native size" : `shown at ${Math.round(scale * 100)}%`}`;
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
      { term: "Host", value: t.place ?? "none yet" },
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
  /** The autopilot control, or null before a claim arrives and after it ends. */
  autopilot: OwnAutopilot | null;
}

/** The pause and play control of the viewer's own camera. */
export interface OwnAutopilot {
  on: boolean;
  /** The button's text and accessible name: what pressing it does. */
  button: string;
  /** The state shown next to it. */
  state: string;
  /** The key that toggles it. */
  key: string;
}

/** Toggles the autopilot on the viewer's own GPU. */
export const AUTOPILOT_KEY = " ";

function ownAutopilotView(on: boolean | null): OwnAutopilot | null {
  if (on === null) return null;
  return on
    ? { on, button: "Pause", state: "Autopilot", key: "Space" }
    : { on, button: "Play", state: "Paused", key: "Space" };
}

export interface OwnButton {
  label: string;
  note: string;
  disabled: boolean;
}

export const OWN_GPU_SECONDS = 60;

/** The button to borrow a GPU, or null: this fleet lends none, so there is nothing to offer. */
function ownButton(st: FleetState, srvNow: number, online: boolean, busy: boolean): OwnButton | null {
  const b = st.borrow;
  const label = "Get your own GPU";
  if (b === null || b.cap === 0) return null;
  if (st.fleet.power.state !== "awake") return { label, note: NOT_AWAKE_NOTE, disabled: true };
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
    return { box: b, streamHeight: Math.min(NATIVE_TILE_HEIGHT, streamHeightFor(b.h * input.dpr, st.image_aspect)) };
  })() : null;
  const left = c?.expires_at != null ? c.expires_at - srvNow : null;
  const autopilot = ownAutopilot(s);
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
    help:
      autopilot === true
        ? "Flying on autopilot. Drag, scroll, an arrow key or plus or minus takes the camera; Space pauses."
        : "Drag to turn the camera, scroll to move closer. Arrow keys and plus or minus work too; Space flies again.",
    latency: s.lastInputLatencyMs === null ? "move the camera to measure input to pixels" : `input to pixels: ${fmt.millis(s.lastInputLatencyMs)}`,
    gpu: c?.gpu ?? "",
    steps: c ? timelineOf(c.phases).steps : [],
    placed,
    autopilot: ended ? null : ownAutopilotView(autopilot),
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

/** What the drawer adds to the mosaic while a VM is opened. */
export interface ZoomDraw {
  /** The opened VM's box; it is not dimmed. */
  box: Box | null;
  /** Its own stream, drawn over the mosaic in that box (in Aggregated mode, over its aggregate). */
  source: DrawSource | null;
  /** Black over the rest of the image: `near` at the opened box's edge, rising to `far` at the stage's corners. */
  near: number;
  far: number;
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
  /** The viewer's own GPU: that stream, drawn alone. */
  focus: DrawSource | null;
  /** A VM opened: the image is zoomed so it fills the stage, and the rest is dimmed around it. */
  zoom: ZoomDraw | null;
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

const RESTORING_PHASES = new Set(["borrowed", "relaunching", "stopping", "restoring", "starting", "pending"]);

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

/** Seams show for this long after the pointer last moved over the image. */
export const SEAM_HOLD_MS = 1500;

function seamView(s: ClientState, layout: MosaicLayout | null, tiles: TileView[], input: WallInput, returning: boolean): SeamView {
  const dpr = input.dpr;
  // Full-bleed, the pointer is always over the image: the seams and the hovered tile's
  // outline show only while it moves (or for a long press, or the keyboard).
  const moving = s.ui.pointerOverImage && (s.ui.longPress || (s.ui.pointerMovedAt !== null && input.now - s.ui.pointerMovedAt < SEAM_HOLD_MS));
  const visible = moving || s.ui.keyId !== null;
  const hovered = s.ui.keyId ?? (moving ? s.ui.hoverId : null);
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
    visible,
    segments,
    lineWidth: 1 / dpr,
    outline: tv && tv.box ? { box: tv.box, color: tv.accent } : null,
    label,
  };
}

export interface SideTag {
  half: Half;
  tag: string;
  text: string;
  x: number;
  y: number;
  align: "right" | "left";
}

export interface StageLabels {
  /** Each half's tag, drawn on the image at its top: beside the line between the halves, or at the image's edge. */
  sides: SideTag[];
  /** A side with no GPUs live: a small label where it would be, or null. */
  empty: { half: Half; text: string; x: number; y: number; align: "right" | "left" } | null;
}

/**
 * A half narrower than this is tagged with its name alone: the longer tag, 14 px uppercase,
 * is about 290 px wide, and it must fit between the divider and the image's edge.
 */
export const SIDE_TAG_FULL_W = 320;

/** The desktop's top row of controls is this tall; labels drawn on the image start below it. */
export const HUD_BAND = 72;

export const EMPTY_SIDE: Record<Half, string> = {
  fcvm: "fcvm: no GPUs live right now",
  ec2: "EC2: no GPUs live right now",
};

/** `awake`: a side with no GPUs is only "empty" once the fleet is up; asleep or waking it is still to come. */
function stageLabels(layout: MosaicLayout | null, awake: boolean, phone: boolean): StageLabels {
  if (!layout) return { sides: [], empty: null };
  const img = layout.image;
  const mid = layout.halfLineX;
  const y = phone ? img.y + 8 : Math.max(img.y + 12, HUD_BAND);
  // A half too narrow for its whole tag (a phone held upright, or a side with few GPUs) says only whose half it is.
  const widthOf = (half: Half) => (mid === null ? img.w : half === "fcvm" ? mid - img.x : img.x + img.w - mid);
  const side = (half: Half, x: number, align: "left" | "right"): SideTag => ({
    half,
    tag: SIDE_TAGS[half].tag,
    text: widthOf(half) < SIDE_TAG_FULL_W ? "" : SIDE_TAGS[half].text,
    x,
    y,
    align,
  });
  if (mid === null) {
    // One side has every GPU: it takes the whole width, and the other says it is empty.
    const present: Half = layout.split >= 1 ? "fcvm" : "ec2";
    const absent: Half = present === "fcvm" ? "ec2" : "fcvm";
    const ey = y + 32;
    return {
      sides: [present === "fcvm" ? side("fcvm", img.x + 12, "left") : side("ec2", img.x + img.w - 12, "right")],
      empty: !awake
        ? null
        : absent === "ec2"
          ? { half: "ec2", text: EMPTY_SIDE.ec2, x: img.x + img.w - 12, y: ey, align: "right" }
          : { half: "fcvm", text: EMPTY_SIDE.fcvm, x: img.x + 12, y: ey, align: "left" },
    };
  }
  return { sides: [side("fcvm", mid - 12, "right"), side("ec2", mid + 12, "left")], empty: null };
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
  /** The phone's way into the Play drawer: a text link under its one button. */
  playButton: { label: string; open: boolean; phoneLabel: string };
  pause: { label: string; on: boolean };
  /** The newest action, briefly. */
  toast: FeedItem | null;
  /** The last few actions, newest first, for the drawer's About section. */
  feed: FeedItem[];
  counter: CounterView | null;
  isolation: IsolationView;
  /** Asleep or waking: the poster full size and a card instead of tiles. */
  sleep: SleepView | null;
  /** The recording of the wall, while asleep or waking and until the live image replaces it. */
  reel: ReelView | null;
  focus: FocusView | null;
  /** The image zoomed around an opened VM (also while it zooms back out), or null. */
  zoom: ZoomLayout | null;
  own: OwnView | null;
  /** Null when this fleet lends no GPUs: the button is not shown. */
  ownButton: OwnButton | null;
  /** Shown when this viewer's claim was refused. */
  ownRefusal: { text: string; dismiss: string } | null;
  viewers: string;
  /** A one-time hint the first time the pointer is over the image. */
  hint: string;
  rotateHint: string;
  /** The live image is whole and the fleet awake. */
  live: boolean;
  /** "2560×1440 · 15 × NVIDIA T4 · 30 fps · path traced", or "". */
  spec: string;
  /** The last race in one line, or "". */
  lastRace: string;
  /** The race drawn on the image: a clock per half while it runs, then the result. */
  race: RaceOnImage | null;
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
 * The tiles the overlays follow: the drawn generation while a newer one waits for its
 * streams. A browser that cannot decode draws no frames, so it follows the newest.
 */
export function shownTilesOf(s: ClientState, canDecode: boolean): Tile[] {
  return canDecode ? drawnTiles(s) : s.accepted?.tiles ?? [];
}

/**
 * The live image is whole: the drawer says every region it draws has shown a frame, and
 * its report covers every region of the newest plan. The drawer reports every 500 ms, and
 * a wake's last snapshot both says "awake" and opens the last region's stream, so the
 * report alone would let the recording leave over a region with no frame yet.
 */
export function imageWhole(plan: DrawPlan, frames: FrameStats | null): boolean {
  if (!frames?.allLive) return false;
  const live = new Set(frames.live);
  const drawn = (frames.mode === "direct" ? plan.direct : plan.aggregated).filter((x) => x.box !== null && x.url !== null);
  return drawn.length > 0 && drawn.every((x) => live.has(`${x.id} ${x.baseUrl ?? ""}`));
}

/** The tile under an image point, as the pointer targets it: the one drawn there. */
export function pointerTile(s: ClientState, canDecode: boolean, u: number, v: number): Tile | null {
  return tileAt(shownTilesOf(s, canDecode), u, v);
}

/**
 * The tiles the canvas draws now: the shown generation's places and rectangles,
 * each with its latest state when the newest snapshot still has it.
 */
export function drawnTiles(s: ClientState): Tile[] {
  const st = s.accepted;
  if (!st) return [];
  const shown = s.shownLayout;
  if (!shown || shown.gen === st.layout.generation) return st.tiles;
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
 * The image's area: the whole stage. The image is contain-fitted in it, and the bands a
 * different aspect leaves (top and bottom, or the sides) hold the controls; nothing is
 * reserved under the image any more.
 */
export function imageArea(stage: Box): Box {
  return stage;
}

/** The line under the image that says what is drawing it: "2560×1440 · 15 × NVIDIA T4 · 30 fps · path traced". */
export function specLine(st: FleetState, s: ClientState, image: Box | null, dpr: number): string {
  if (!image || st.fleet.power.state !== "awake") return "";
  const live = st.tiles.filter(isLive);
  if (live.length === 0) return "";
  const names = new Map<string, number>();
  for (const t of live) if (t.gpu) names.set(t.gpu.name, (names.get(t.gpu.name) ?? 0) + 1);
  const gpu = [...names.entries()].sort((a, b) => b[1] - a[1])[0]?.[0] ?? "GPU";
  // The renderers' own rate where they report it; else what this page receives.
  const reported = live.map((t) => t.fps).filter((x): x is number => x !== null && x > 0);
  const fps = (reported.length > 0 ? reported : Object.values(s.frames?.tiles ?? {}).filter((x) => x.newestT !== null && x.fps > 0).map((x) => x.fps)).sort((a, b) => a - b);
  const parts = [`${Math.round(image.w * dpr)}×${Math.round(image.h * dpr)}`, `${live.length} × ${gpu}`];
  if (fps.length > 0) parts.push(`${Math.round(fps[Math.floor(fps.length / 2)]!)} fps`);
  parts.push("path traced");
  return parts.join(" · ");
}

/**
 * The last race in one line, so the strip never shows dashes: the scoreboard, else the
 * newest race the coordinator kept (`races`), else each half's newest single restart,
 * else "". Asleep or waking, dated.
 */
export function lastRaceLine(st: FleetState): string {
  const asleep = st.fleet.power.state !== "awake";
  const line = (f: number | null, e: number | null, at: number | null): string => {
    const parts: string[] = [];
    if (f !== null) parts.push(`fcvm back in ${fmt.seconds(f)}`);
    if (e !== null) parts.push(`EC2 back in ${fmt.seconds(e)}`);
    if (f !== null && e !== null) parts.push(`${ratioText(f, e)} faster`);
    return `Last race: ${parts.join(" · ")}${asleep && at !== null ? ` · measured ${fmt.utcDate(at)}` : ""}`;
  };
  const { fcvm, ec2 } = st.scoreboard;
  if (fcvm !== null || ec2 !== null) return line(fcvm?.last_ms ?? null, ec2?.last_ms ?? null, st.relaunch_all?.started_at ?? null);
  const race = [...st.races].reverse().find((r) => r.fcvm?.time_ms != null || r.ec2?.time_ms != null);
  if (race) return line(race.fcvm?.time_ms ?? null, race.ec2?.time_ms ?? null, race.started_at);
  const single = (h: Half): string | null => {
    let best: Tile | null = null;
    for (const t of st.tiles) {
      if (t.half !== h || relaunchMs(t) === null) continue;
      if (best === null || t.last_relaunch!.first_frame_at! > best.last_relaunch!.first_frame_at!) best = t;
    }
    return best === null ? null : `${tileName(best)} back in ${fmt.seconds(relaunchMs(best)!)}`;
  };
  const singles = [single("fcvm"), single("ec2")].filter((x): x is string => x !== null);
  return singles.length > 0 ? `Last single restarts: ${singles.join(" · ")}` : "";
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
    // A reclaimed host's tiles leave the tiling at once; `failures` keeps it for a minute.
    const reclaimed = new Set([
      ...st.failures.filter((f) => f.kind === "host_reclaimed").map((f, i) => f.host ?? `#${i}`),
      ...st.fleet.hosts.filter((h) => h.draining).map((h) => h.name),
    ]);
    if (reclaimed.size > 0) {
      banners.push({
        kind: "reclaimed",
        text: `AWS reclaimed ${reclaimed.size === 1 ? "an fcvm host" : `${reclaimed.size} fcvm hosts`}. The image is re-tiled onto the GPUs left.`,
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
    zoom: null,
    image: null,
    dpr: input.dpr,
    darkFill: "#06080B",
    estimatedBps: 0,
    poster,
    paused: s.ui.paused,
    darkFadeMs: input.reducedMotion ? REDUCED_MOTION_DARK_FADE_MS : 0,
    suspended: s.ui.suspended,
    posterFull: false,
    gen: s.accepted?.layout.generation ?? 0,
    atT: 0,
  };
  const area = input.mosaicArea
    ? imageArea(input.mosaicArea)
    : null;
  const common = {
    title: TITLE,
    site: SITE,
    sentence: SENTENCE,
    version: s.version,
    online,
    banners,
    playButton: { label: "Direct the scene", open: s.ui.drawer, phoneLabel: "Direct the scene: write a word in the sky, drop a light" },
    pause: { label: s.ui.paused ? "Resume" : "Pause", on: s.ui.paused },
    hint: "Click a square to look at its VM.",
    rotateHint: "Turn sideways for full screen",
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
      labels: { sides: [], empty: null },
      play: null,
      toast: null,
      feed: [],
      counter: null,
      isolation: isolationView(null),
      sleep: null,
      reel: null,
      focus: null,
      zoom: null,
      own: null,
      ownButton: null,
      ownRefusal: null,
      viewers: "",
      live: false,
      spec: "",
      lastRace: "",
      race: null,
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
  const shownTiles = shownTilesOf(s, input.canDecode);
  const planLayout = area ? layoutMosaic(area, st.tiles, input.dpr, st.image_aspect) : null;
  const layout = shownTiles === st.tiles ? planLayout : area ? layoutMosaic(area, shownTiles, input.dpr, st.image_aspect) : null;
  const placedById = new Map<string, PlacedTile>(layout?.tiles.map((p) => [p.id, p]) ?? []);
  const placedPlan = new Map<string, PlacedTile>(planLayout?.tiles.map((p) => [p.id, p]) ?? []);
  // Asleep or waking, a tile that is not rendering yet shows the poster, not a dark square.
  const asleepFix = (tv: TileView) => (sleep && tv.dark ? { ...tv, dark: false, hatch: false } : tv);
  const tiles = shownTiles.map((t) => asleepFix(tileView(t, s, st, placedById.get(t.id)?.box ?? null, srvNow, pend)));
  const planViews = shownTiles === st.tiles ? tiles : st.tiles.map((t) => asleepFix(tileView(t, s, st, placedPlan.get(t.id)?.box ?? null, srvNow, pend)));
  const focusTile = s.ui.focusId === null ? null : st.tiles.find((t) => t.id === s.ui.focusId) ?? null;
  const own = ownView(s, st, srvNow, input);

  // A VM opened: the whole image zooms until that VM fills the stage, and the tiles around it
  // stay in place, dimmed. The page animates `zoom`; without it an opened VM is fully zoomed.
  const insets = input.zoomInsets ?? zoomInsetsFor(input.phone, 0);
  const step: ZoomStep | null = input.zoom !== undefined && input.zoom !== null ? input.zoom : focusTile ? { from: null, to: focusTile.id, progress: 1 } : null;
  const zArea = input.zoomArea ?? area;
  const zoom = !own && zArea && planLayout && step ? layoutZoom(planLayout.image, zArea, st.tiles, step, input.dpr, st.image_aspect, insets) : null;
  // Where the zoom ends while it opens: what the streams are asked for (a height change keeps each
  // stream and its frames, so they can follow the target and not every step of the animation).
  const opening = zoom && focusTile && zoom.focus?.id === focusTile.id ? focusTile : null;
  const target = opening && zArea && planLayout ? layoutZoom(planLayout.image, zArea, st.tiles, { from: opening.id, to: opening.id, progress: 1 }, input.dpr, st.image_aspect, insets) : null;
  const detailHeight = opening && zArea ? streamHeightFor(zoomTarget(zArea, opening.rect, st.image_aspect, insets).h * input.dpr, tileAspectOf(opening.rect, st.image_aspect), DETAIL_TILE_PX) : null;
  const zoomBox = (id: string): Box | null => {
    const b = zoom?.layout.tiles.find((p) => p.id === id)?.box ?? null;
    return b && zArea && boxesMeet(b, zArea) ? b : null;
  };
  const focusPlaced = zoom?.focus && focusTile && zoom.focus.id === focusTile.id ? { box: zoom.focus.box, streamHeight: detailHeight ?? zoom.focus.streamHeight } : null;
  const focus = focusTile ? focusView(s, tiles.find((x) => x.id === focusTile.id)!, focusTile, input, srvNow, online, focusPlaced) : null;
  const covered = focus !== null || own !== null || zoom !== null;

  // Direct: each tile asks for the device pixels it covers. With the viewer's own GPU on screen
  // the others (still streaming, not drawn) ask for the smallest. While a VM opens it asks for its
  // detail height and is kept out of the budget; the tiles still on screen around it keep their
  // wall height and start at q=lo (they are dimmed), and the ones the zoom pushed off screen ask
  // for the smallest. Then everything but the kept one drops to q=lo and steps down until the
  // streams fit the bandwidth budget.
  const asks: StreamAsk[] = st.tiles.filter(isLive).map((t) => {
    const wall = placedPlan.get(t.id)?.streamHeight ?? 144;
    if (own) return { id: t.id, rect: t.rect, height: STREAM_HEIGHTS[0], keep: false };
    if (opening && target) {
      if (t.id === opening.id) return { id: t.id, rect: t.rect, height: detailHeight!, keep: true };
      const b = target.layout.tiles.find((p) => p.id === t.id)?.box;
      return b && zArea && boxesMeet(b, zArea) ? { id: t.id, rect: t.rect, height: wall, keep: false, dim: true } : { id: t.id, rect: t.rect, height: STREAM_HEIGHTS[0], keep: false };
    }
    return { id: t.id, rect: t.rect, height: wall, keep: false };
  });
  const fitted = fitBandwidth(asks, st.image_aspect, input.budgetBps);
  // Every tile streams over the one family the probe chose; until it answers, none does.
  const family = s.net.family;
  const zoomed = zoom?.focus ?? null;
  const direct: DrawSource[] = st.tiles.filter((t) => isLive(t) && streamUrl(t, family) !== null).map((t) => {
    const height = fitted.heights.get(t.id) ?? 144;
    const q = fitted.quality.get(t.id) ?? "std";
    const base = streamUrl(t, family)!;
    // Zoomed, the opened VM is drawn on its own over the mosaic (plan.zoom.source), so its entry here keeps streaming undrawn.
    const box = own ? null : zoom ? (zoomed?.id === t.id ? null : zoomBox(t.id)) : placedPlan.get(t.id)?.box ?? null;
    return { id: `d:${t.id}`, url: frameSocketUrl(base, height, q), baseUrl: base, height, box };
  });
  // Aggregated: each region is one big tile; it asks for the height it covers on the wall. Only the
  // aggregates with a URL in this viewer's family stream.
  const aggregated: DrawSource[] = st.aggregates
    .filter((g) => streamUrl(g, family) !== null)
    .map((g) => {
      const box = planLayout ? rectOnScreen(planLayout.image, g.rect, input.dpr) : null;
      const height = own || !box ? STREAM_HEIGHTS[0] : streamHeightFor(box.h * input.dpr);
      const base = streamUrl(g, family)!;
      const zb = zoom ? rectOnScreen(zoom.layout.image, g.rect, input.dpr) : null;
      const drawn = own ? null : zoom ? (zb && zArea && boxesMeet(zb, zArea) ? zb : null) : box;
      return { id: `a:${g.id}`, url: frameSocketUrl(base, height), baseUrl: base, height, box: drawn };
    });
  let focusSource: DrawSource | null = null;
  const claimUrl = s.own.claim ? streamUrl(s.own.claim, family) : null;
  if (own && own.placed && s.own.claim && claimUrl) {
    const c = s.own.claim;
    focusSource = { id: `o:${c.id}`, url: frameSocketUrl(claimUrl, own.placed.streamHeight), baseUrl: claimUrl, height: own.placed.streamHeight, box: own.placed.box };
  }
  let zoomDraw: ZoomDraw | null = null;
  if (zoom) {
    const src = zoomed ? direct.find((d) => d.id === `d:${zoomed.id}`) : undefined;
    zoomDraw = {
      box: zoomed?.box ?? null,
      source: src && zoomed ? { ...src, box: zoomed.box } : null,
      near: ZOOM_DIM_NEAR * zoom.dim,
      far: ZOOM_DIM_FAR * zoom.dim,
    };
  }
  const plan: DrawPlan = {
    ...emptyPlan,
    // Aggregated only where the serving aggregates serve this viewer's family; otherwise direct.
    mode: aggregatedAvailable(st.aggregates, family) ? s.ui.mode : "direct",
    direct,
    aggregated,
    focus: focusSource,
    zoom: zoomDraw,
    estimatedBps: fitted.bps,
    image: own ? null : zoom ? zoom.layout.image : planLayout?.image ?? null,
    posterFull: sleep !== null,
    gen: st.layout.generation,
    atT: st.layout.from_frame / FRAME_RATE,
    tiles: st.tiles.map((t, i) => {
      const tv = planViews[i]!;
      const box = own ? null : zoom ? zoomBox(t.id) : placedPlan.get(t.id)?.box ?? null;
      return { id: t.id, half: t.half, box, dark: tv.dark, outline: tv.darkOutline, slowFade: tv.slowFade };
    }),
  };

  const play = playView(s, st, srvNow, pend, plan, input, online);
  const returning = stillReturning(st, s.ui.returning, srvNow);
  const latestNudge = [...s.notices].reverse().find((n) => n.intent === "nudge")?.id ?? null;
  return {
    ...common,
    ready: true,
    layout,
    tiles,
    score: scoreView({ st, now: srvNow, pending: pend, online }),
    labels: stageLabels(covered ? null : layout, st.fleet.power.state === "awake", input.phone),
    play,
    toast: toastView(st, srvNow, s.you),
    feed: feedView(st, srvNow, s.you, 5),
    counter: counterView(st),
    isolation: isolationView(st),
    sleep,
    reel: input.media
      ? reelView({
          base: input.media.base,
          meta: input.media.meta,
          power: st.fleet.power.state,
          reelOn: s.ui.reelOn,
          allLive: imageWhole(plan, s.frames),
          reducedMotion: input.reducedMotion ?? false,
          saveData: input.saveData ?? false,
        })
      : null,
    focus,
    zoom,
    own,
    ownButton: ownButton(st, srvNow, online, s.own.requestId !== null || (s.own.claim !== null && s.own.claim.phase !== "ended")),
    ownRefusal: s.own.refused === null ? null : { text: `No GPU is free right now: ${s.own.refused}. Try again in a minute.`, dismiss: "OK" },
    viewers: fmt.count(st.viewers.active, "person watching", "people watching"),
    live: st.fleet.power.state === "awake" && imageWhole(plan, s.frames),
    spec: specLine(st, s, planLayout?.image ?? null, input.dpr),
    lastRace: lastRaceLine(st),
    race: covered ? null : raceOnImage(st, srvNow, layout?.image ?? null, layout?.split ?? 0.5),
    plan,
    seams: seamView(s, covered ? null : layout, tiles, input, returning),
    returning,
    bandwidth: `${fmt.count(plan.direct.length, "direct stream", "direct streams")}, about ${fmt.mbps(plan.estimatedBps)} at the renderer's bit rate`,
    notices: s.notices.flatMap((n) => {
      // Only the latest stop choice speaks: a later pick replaced the earlier one's vote.
      if (n.intent === "nudge" && n.id !== latestNudge) return [];
      // A pick is dated by when the coordinator applied it, which says which vote it was in.
      const applied = st.actions.find((a) => a.id === n.id && a.name === n.intent);
      const text = noticeText(n, { nudges: st.nudges, srvNow, srvAt: applied?.at ?? serverNow(s.clock, n.at) });
      return text === null ? [] : [{ key: n.id, text }];
    }),
    gridLabel: `The image, ${fmt.count(st.tiles.length, "square", "squares")}: fcvm on the left, EC2 on the right. Arrow keys move between squares, Enter opens one.`,
  };
}

/** Notices older than this are dropped. */
export const NOTICE_MS = 8000;

const INTENT_LABEL: Record<string, string> = {
  restart_now: "The restart",
  relaunch: "The restart",
  light: "The light",
  nudge: "Your choice of stop",
};

/** What a notice needs from the fleet: the directed camera, the server time now and at the notice. */
export interface NoticeContext {
  nudges: Nudges | null;
  srvNow: number;
  srvAt: number;
}

/**
 * A notice, as the page says it: a refused action, and what a stop choice will do. Words and
 * claims have their own lines.
 */
export function noticeText(n: Notice, ctx: NoticeContext | null = null): string | null {
  if (n.intent === "nudge" && ctx?.nudges) return stopChoiceText(n, ctx.nudges, ctx.srvNow, ctx.srvAt);
  if (n.kind !== "refused" || n.intent === "word" || n.intent === "claim_gpu" || n.intent === "claim_camera" || n.intent === "release_gpu") return null;
  const what = INTENT_LABEL[n.intent] ?? n.intent;
  return `${what} was not applied${n.reason ? `: ${n.reason}` : ""}`;
}

/**
 * What a viewer's stop choice will do, from the open vote now: the vote is always open, for
 * the first hold not yet decided, so a counted pick is next up, comes after the hold already
 * decided, or trails the leader until the vote closes.
 */
function stopChoiceText(n: Notice, nd: Nudges, srvNow: number, srvAt: number): string {
  const target = String(n.args["target"] ?? "");
  const label = (id: string | null) => nd.targets.find((x) => x.id === id)?.label ?? null;
  const inT = (at: number | null) => (at === null ? "" : `, in ${fmt.countdown(at - srvNow)}`);
  const leader = nd.targets.find((x) => x.id === nd.leading) ?? null;
  const nextUp =
    nd.next !== null
      ? `Next up: ${nd.next_label ?? "the next stop"}${inT(nd.arrive_next_at)}`
      : leader
        ? `Next up: ${leader.label}${inT(leader.arrive_at)}`
        : "";
  const mine = nd.targets.find((x) => x.id === target) ?? null;
  if (n.kind === "refused") {
    const why = mine === null && target !== nd.next ? "That stop is not offered now." : `Your pick did not count${n.reason ? `: ${n.reason}` : ""}.`;
    return [why, nextUp].filter(Boolean).join(" ");
  }
  if (target === nd.next) return nextUp;
  // Reached, unless the open vote offers it again (a return after the hold already decided).
  if (target === nd.holding && mine === null) return `Your pick, ${nd.holding_label ?? "this stop"}: the camera is there now`;
  // The vote this pick was in has closed and chose another stop.
  if (mine === null || (nd.opened_at !== null && srvAt < nd.opened_at)) {
    return `The vote went to ${nd.next_label ?? label(nd.leading) ?? "another stop"}. Pick again for the stop after it.`;
  }
  if (target === nd.leading) {
    return nd.next === null
      ? `Next up: your pick, ${mine.label}${inT(mine.arrive_at)}`
      : `Your pick, ${mine.label}, comes after ${nd.next_label ?? "the next stop"}${inT(mine.arrive_at)}`;
  }
  const closes = nd.closes_at === null ? "" : ` The vote closes in ${fmt.countdown(nd.closes_at - srvNow)}.`;
  return `Your vote for ${mine.label} is in; ${leader ? `${leader.label} leads ${leader.votes} to ${mine.votes}` : "another stop leads"}.${closes}`;
}

/** Whether the page must verify the viewer before sending an action. */
export function needsVerify(s: ClientState): boolean {
  return s.turnstile.sitekey !== null && !s.turnstile.verified;
}


/**
 * Aggregated mode is available to a viewer when an aggregate serves its family and every
 * aggregate that is serving at all (a URL in either family) serves its family: otherwise a
 * half drawn only by an aggregate the viewer cannot open would stay dark. An aggregate not
 * serving yet (no URL, as during a wake) does not count against it.
 */
export function aggregatedAvailable(aggregates: readonly Aggregate[], family: Family | null): boolean {
  const serving = aggregates.filter((a) => a.frame_url !== null || a.frame_url_v4 !== null);
  return serving.length > 0 && serving.every((a) => streamUrl(a, family) !== null);
}

/** The stats drawer's line on the address family the tiles stream over. */
export function familyLine(net: ClientState["net"]): string {
  if (net.family === null) return "Address family: checking";
  const name = net.family === "v6" ? "IPv6" : "IPv4";
  if (net.via === "race" && net.probeMs !== null)
    return `Address family: ${name} for every tile (connected first, in ${Math.round(net.probeMs)} ms)`;
  if (net.via === "no-answer")
    return `Address family: IPv4 for every tile (neither family connected in ${FAMILY_PROBE_ATTEMPTS} tries of ${(FAMILY_PROBE_MS / 1000).toFixed(1)} s)`;
  return `Address family: ${name} for every tile (the tiles have no ${net.family === "v6" ? "IPv4" : "IPv6"} address)`;
}
