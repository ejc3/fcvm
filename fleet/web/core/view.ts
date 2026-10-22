// View models: everything the page draws, every word it shows, every number it
// prints. React and the frame drawer read these and compute nothing themselves.

import { serverNow } from "./clock.ts";
import * as fmt from "./format.ts";
import {
  STREAM_HEIGHTS,
  layoutFocus,
  layoutMosaic,
  rectOnScreen,
  streamHeightFor,
  seamSegments,
  type Box,
  type MosaicLayout,
  type Segment,
} from "./layout.ts";
import { frameSocketUrl, urlHost } from "./net.ts";
import { fitBandwidth, type StreamAsk } from "./bandwidth.ts";
import { suggestMode } from "./mode.ts";
import type { Action, FleetState, Half, IntentMsg, Rect, Tile } from "./protocol.ts";
import { sceneUnixMs } from "./scene.ts";
import type { ClientState, Notice, StreamMode } from "./store.ts";

export const SITE = "fcvm.dev";
export const TITLE = "Every GPU, live";
export const NO_FLEET = "FLEET_COORD_URL is not set, so this page has no fleet to show.";

export const COLORS = {
  fcvm: { accent: "#7CC4FF", soft: "#CFE8FF", darkOutline: "#1E3A52" },
  ec2: { accent: "#F5A524", soft: "#FFE2B0", darkOutline: "#4A3410" },
} as const;

export const KIND_TAG: Record<Half, string> = { fcvm: "fcvm", ec2: "EC2" };
export const KIND_NAME: Record<Half, string> = { fcvm: "fcvm sandbox", ec2: "EC2 instance" };
const HALF_PLURAL: Record<Half, string> = { fcvm: "fcvm sandboxes", ec2: "EC2 instances" };

export const AUTO_OPTIONS: readonly (number | null)[] = [null, 60, 120, 300, 900];

const PHASE_TEXT: Record<string, string> = {
  running: "running",
  stopping: "stopping the old VM",
  restoring: "restoring from the snapshot",
  attaching_gpu: "attaching the GPU",
  starting: "starting the renderer",
  terminating: "terminating",
  launching: "launching an instance",
  pending: "assigned, waiting for its host",
  relaunching: "relaunching",
  waiting_capacity: "waiting for capacity",
  waiting_for_capacity: "waiting for capacity",
  draining: "host reclaimed, draining",
  failed: "failed",
  requested: "relaunch requested",
};

/**
 * The parts of a relaunch, in the order they run, with the label each gets. The
 * coordinator reports `<part>_ms`; `restore_ms` contains `gpu_ms`, so the bar
 * shows the restore without the GPU and the GPU after it. `total_ms` and
 * `ack_ms` are sums or other clocks and are not parts.
 */
const PART_ORDER: [string, string][] = [
  ["teardown_ms", "old VM stopped"],
  ["restore_ms", "snapshot restored"],
  ["gpu_ms", "GPU attached, driver loaded"],
  ["launch_ms", "launch requested and accepted"],
  ["ready_ms", "instance boot to first frame"],
  ["container_ms", "container started"],
  ["first_frame_ms", "first frame"],
];
const NOT_PARTS = new Set(["total_ms", "ack_ms"]);

/** The parts of a relaunch's timings, in order, as [label, ms]. */
export function relaunchParts(phases: Record<string, number>): [string, number][] {
  const out: [string, number][] = [];
  const restore = phases["restore_ms"];
  const gpu = phases["gpu_ms"];
  for (const [k, label] of PART_ORDER) {
    let ms = phases[k];
    if (ms === undefined) continue;
    if (k === "restore_ms" && gpu !== undefined && restore !== undefined) ms = Math.max(0, restore - gpu);
    out.push([label, ms]);
  }
  for (const [k, ms] of Object.entries(phases)) {
    if (NOT_PARTS.has(k) || PART_ORDER.some(([p]) => p === k)) continue;
    out.push([k.replace(/_ms$/, "").replace(/_/g, " "), ms]);
  }
  return out;
}

export function phaseText(phase: string): string {
  return PHASE_TEXT[phase] ?? phase.replace(/_/g, " ");
}

function isWaiting(phase: string): boolean {
  return phase === "waiting_capacity" || phase === "waiting_for_capacity";
}

/** The name a viewer reads for a tile: the slot behind it, or its place. */
export function tileName(t: Tile): string {
  return t.slot ?? t.id;
}

export interface Viewport {
  w: number;
  h: number;
  dpr: number;
}

export interface WallInput {
  state: ClientState;
  /** Local Unix ms. */
  now: number;
  dpr: number;
  /** Where the image may go, measured by the page. */
  mosaicArea: Box | null;
  /** Where an opened tile is drawn, measured by the page. */
  focusArea: Box | null;
  /** WebCodecs H.264 is available. */
  canDecode: boolean;
  phone: boolean;
  /** The browser's downlink estimate (Mbit/s), when it gives one. */
  downlinkMbps?: number | null;
  /** Share of drawn tiles behind the common t lately, in direct mode. */
  lateShare?: number | null;
  /** Boxes the hover label must not cover (the race and counter cards), measured by the page. */
  avoid?: Box[];
  /** The hover label's measured size. */
  labelSize?: { w: number; h: number };
  /** The direct streams' bandwidth budget, bits per second (BANDWIDTH_BUDGET_BPS when absent). */
  budgetBps?: number;
}

// ---- tiles -------------------------------------------------------------------

export interface HoverLabel {
  id: string;
  kind: string;
  gpu: string;
  host: string;
  last: string;
}

export interface TileView {
  id: string;
  /** The slot behind it, or its place when there is none. */
  name: string;
  half: Half;
  box: Box | null;
  accent: string;
  /** Drawn dark, with its phase and clock over it. */
  dark: boolean;
  darkClock: string;
  darkPhase: string;
  darkOutline: string;
  /** Shown dimmed: behind the common time. */
  behind: boolean;
  ariaLabel: string;
  hover: HoverLabel;
  stats: string[];
  /** Short line for the phone card and the focus view: "running · 2.8 s last relaunch". */
  status: string;
  lastShort: string;
}

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
  const fs = s.frames?.tiles[sourceIdFor(t, s.ui.shownMode, st.aggregators)];
  const waitingForFrames = isLive(t) && !requested && (!fs || fs.newestT === null);
  const dark = !isLive(t) || requested || waitingForFrames;
  const accent = COLORS[t.half].accent;
  const rms = relaunchMs(t);
  const lastShort = rms === null ? "" : fmt.seconds(rms);

  let darkPhase = phaseText(phase);
  let darkClock = "";
  const clockRuns = !isWaiting(t.phase) && t.phase !== "failed" && !requested;
  if (dark && clockRuns && t.last_relaunch && t.phase !== "running") {
    darkClock = fmt.seconds(srvNow - t.last_relaunch.clicked_at);
  }
  if (waitingForFrames) {
    darkPhase = "connecting";
    darkClock = lastShort;
  }

  const gpuName = t.gpu?.name ?? "no GPU yet";
  const name = tileName(t);
  const hover: HoverLabel = {
    id: name,
    kind: KIND_TAG[t.half],
    gpu: gpuName,
    host: t.host ?? "no host yet",
    last: rms === null ? "no relaunch yet" : `last relaunch ${fmt.seconds(rms)}`,
  };

  const stats: string[] = [];
  if (fs && fs.newestT !== null) {
    stats.push(`${Math.round(fs.fps)} fps · ${fs.height}p`);
    const age = srvNow - sceneUnixMs(st.scene.epoch_unix_ms, fs.newestT);
    stats.push(`frame age ${fmt.millis(Math.max(0, age))}`);
  } else {
    stats.push(dark ? phaseText(phase) : "no frames");
  }
  if (t.gpu) {
    const parts = [
      t.gpu.temp_c === null ? null : `${Math.round(t.gpu.temp_c)} °C`,
      t.gpu.power_w === null ? null : `${Math.round(t.gpu.power_w)} W`,
      t.gpu.util_pct === null ? null : `${Math.round(t.gpu.util_pct)}%`,
    ].filter((x): x is string => x !== null);
    if (parts.length > 0) stats.push(parts.join(" · "));
  }
  const up = t.last_relaunch?.first_frame_at ?? null;
  stats.push(up === null ? name : `${name} · up ${fmt.span(srvNow - up)}`);

  const status = dark ? phaseText(phase) : rms === null ? "running" : `running · ${fmt.seconds(rms)} last relaunch`;
  const ariaLabel = `${name}, ${KIND_NAME[t.half]}, ${dark ? phaseText(phase) : "running"}${
    rms === null ? "" : `, last relaunch ${fmt.seconds(rms)}`
  }. Open it large.`;

  return {
    id: t.id,
    name,
    half: t.half,
    box,
    accent,
    dark,
    darkClock,
    darkPhase,
    darkOutline: COLORS[t.half].darkOutline,
    behind: fs?.behind ?? false,
    ariaLabel,
    hover,
    stats,
    status,
    lastShort,
  };
}

// ---- the race ----------------------------------------------------------------

export interface HalfView {
  half: Half;
  accent: string;
  soft: string;
  title: string;
  renderedBy: string;
  back: string;
  backFraction: number;
  stamp: string;
  median: string;
}

export interface RaceView {
  active: boolean;
  caption: string;
  clock: string;
  by: string;
  fcvm: HalfView;
  ec2: HalfView;
}

function halfTiles(st: FleetState, h: Half): Tile[] {
  return st.tiles.filter((t) => t.half === h);
}

function halfView(st: FleetState, h: Half): HalfView {
  const tiles = halfTiles(st, h);
  const withGpu = tiles.filter((t) => !isWaiting(t.phase) && t.gpu !== null);
  const ra = st.relaunch_all;
  const durations = tiles.map(relaunchMs).filter((x): x is number => x !== null);
  const med = fmt.median(durations);
  const base = {
    half: h,
    accent: COLORS[h].accent,
    soft: COLORS[h].soft,
    title: HALF_PLURAL[h],
    renderedBy: `rendered by ${fmt.count(withGpu.length, `${KIND_TAG[h]} GPU`, `${KIND_TAG[h]} GPUs`)}`,
    median: med === null ? "no relaunches yet" : `median relaunch ${fmt.seconds(med)}`,
  };
  if (!ra) {
    const up = tiles.filter(isLive).length;
    return {
      ...base,
      back: `${up} / ${tiles.length} up`,
      backFraction: tiles.length === 0 ? 0 : up / tiles.length,
      stamp: "no relaunch all yet",
    };
  }
  const racing = withGpu;
  const back = racing.filter(
    (t) => isLive(t) && t.last_relaunch !== null && t.last_relaunch.first_frame_at !== null &&
      t.last_relaunch.first_frame_at >= ra.started_at,
  ).length;
  const doneAt = h === "fcvm" ? ra.fcvm_done_at : ra.ec2_done_at;
  const raceOver = ra.fcvm_done_at !== null && ra.ec2_done_at !== null;
  let stamp: string;
  if (doneAt !== null) {
    stamp = raceOver
      ? `last relaunch all: ${fmt.seconds(doneAt - ra.started_at)}`
      : `all back at ${fmt.raceClock(doneAt - ra.started_at)}`;
  } else if (back === 0) {
    stamp = h === "fcvm" ? "restoring" : "still launching";
  } else {
    stamp = "healing";
  }
  return {
    ...base,
    back: `${back} / ${racing.length} back`,
    backFraction: racing.length === 0 ? 0 : back / racing.length,
    stamp,
  };
}

function raceView(st: FleetState, srvNow: number): RaceView {
  const ra = st.relaunch_all;
  const fcvm = halfView(st, "fcvm");
  const ec2 = halfView(st, "ec2");
  if (!ra) return { active: false, caption: "relaunch all to start the race", clock: "--", by: "", fcvm, ec2 };
  const over = ra.fcvm_done_at !== null && ra.ec2_done_at !== null;
  const end = over ? Math.max(ra.fcvm_done_at!, ra.ec2_done_at!) : srvNow;
  return {
    active: !over,
    caption: over ? "last relaunch all, both halves back" : "since Relaunch all",
    clock: fmt.raceClock(end - ra.started_at),
    by: ra.by === "" ? "" : `started by ${ra.by}`,
    fcvm,
    ec2,
  };
}

// ---- controls ----------------------------------------------------------------

export interface Option {
  value: string;
  label: string;
}

export interface ModeView {
  label: string;
  options: { value: StreamMode; label: string; disabled: boolean }[];
  selected: StreamMode;
  /** "switching to aggregated" while the drawer still shows the other mode. */
  note: string;
  suggestion: { mode: StreamMode; text: string };
  detail: string;
}

export interface ControlsView {
  site: string;
  mode: ModeView;
  relaunchAll: { label: string; disabled: boolean; note: string };
  auto: { label: string; options: Option[]; selected: string; note: string; pending: boolean };
  stats: { label: string; on: boolean };
  light: { label: string; on: boolean };
  word: { label: string; placeholder: string; submit: string; maxLength: number; status: string; statusKind: "idle" | "pending" | "applied" | "refused" };
  timeOfDay: { label: string; value: number; min: number; max: number; step: number; valueText: string; pending: boolean };
  viewers: string;
  help: string;
  latency: string;
  latencyMeasured: boolean;
}

export const WORD_MAX = 28;

function autoValue(secs: number | null): string {
  return secs === null ? "off" : String(secs);
}

export function autoFromValue(v: string): number | null {
  if (v === "off") return null;
  const n = Number(v);
  return Number.isFinite(n) && n > 0 ? n : null;
}

function autoLabel(secs: number | null): string {
  if (secs === null) return "Off";
  const s = fmt.interval(secs);
  return s.charAt(0).toUpperCase() + s.slice(1);
}

function lastAction(st: FleetState, name: string): Action | null {
  let best: Action | null = null;
  for (const a of st.actions) if (a.name === name && a.outcome !== "refused" && (best === null || a.at >= best.at)) best = a;
  return best;
}

function modeView(s: ClientState, st: FleetState, plan: DrawPlan, input: WallInput): ModeView {
  const has = st.aggregators.length > 0;
  const sug = suggestMode({
    aggregators: st.aggregators.length,
    downlinkMbps: input.downlinkMbps ?? null,
    directBps: plan.estimatedBps,
    lateShare: input.lateShare ?? null,
  });
  const name = (m: StreamMode) => (m === "direct" ? "Direct" : "Aggregated");
  const selected = has ? s.ui.mode : "direct";
  const detail =
    selected === "direct"
      ? `${fmt.count(plan.direct.length, "stream", "streams")}, one per tile, about ${fmt.mbps(plan.estimatedBps)}`
      : `${fmt.count(plan.aggregated.length, "stream", "streams")}, each a joined region`;
  return {
    label: "Streams",
    options: [
      { value: "direct", label: "Direct", disabled: false },
      { value: "aggregated", label: "Aggregated", disabled: !has },
    ],
    selected,
    note: selected !== s.ui.shownMode ? `switching to ${name(selected).toLowerCase()}` : "",
    suggestion: { mode: sug.mode, text: `${name(sug.mode)} suggested: ${sug.reason}` },
    detail,
  };
}

function controlsView(s: ClientState, st: FleetState, srvNow: number, pendingIntents: IntentMsg[]): Omit<ControlsView, "mode"> {
  const ra = st.relaunch_all;
  const raceOn = ra !== null && (ra.fcvm_done_at === null || ra.ec2_done_at === null);
  const pendingAll = pendingIntents.some((i) => i.name === "relaunch_all");
  const gpus = st.tiles.filter((t) => t.gpu !== null).length;
  const relaunchAll = raceOn
    ? { label: "Relaunch all in progress", disabled: true, note: ra!.by === "" ? "" : `started by ${ra!.by}` }
    : pendingAll
      ? { label: "Relaunch all requested", disabled: true, note: "" }
      : { label: `Relaunch all ${fmt.count(gpus, "GPU", "GPUs")}`, disabled: gpus === 0, note: "" };

  const pendingAuto = [...pendingIntents].reverse().find((i) => i.name === "set_auto_relaunch");
  const secs = pendingAuto
    ? (typeof pendingAuto.args["secs"] === "number" ? (pendingAuto.args["secs"] as number) : null)
    : st.settings.auto_relaunch_secs;
  const opts = [...AUTO_OPTIONS];
  if (secs !== null && !opts.includes(secs)) opts.push(secs);
  let note = "";
  if (st.settings.auto_relaunch_secs !== null && !pendingAuto) {
    const last = lastAction(st, "set_auto_relaunch");
    const setBy = last === null ? undefined : s.you !== null && (last.viewer === s.you || last.by === s.you) ? "you" : last.by;
    // A tile never relaunched is due at once.
    const due = st.tiles
      .filter((t) => t.gpu !== null)
      .map((t) => (t.last_relaunch === null ? srvNow : t.last_relaunch.clicked_at + st.settings.auto_relaunch_secs! * 1000));
    const next = due.length === 0 ? null : Math.min(...due);
    const parts = [setBy ? `set by ${setBy}` : null, next === null ? null : `next in ${fmt.countdown(next - srvNow)}`];
    note = parts.filter((p) => p !== null).join(" · ");
  } else if (pendingAuto) {
    note = "sending";
  }

  const pendingWord = pendingIntents.find((i) => i.name === "word");
  const lastWordNotice = [...s.notices].reverse().find((n) => n.intent === "word");
  let wordStatus = "";
  let statusKind: ControlsView["word"]["statusKind"] = "idle";
  if (pendingWord) {
    wordStatus = `Checking “${String(pendingWord.args["text"] ?? "")}”`;
    statusKind = "pending";
  } else if (lastWordNotice) {
    const text = String(lastWordNotice.args["text"] ?? "");
    if (lastWordNotice.kind === "applied") {
      wordStatus = `“${text}” is in the scene`;
      statusKind = "applied";
    } else {
      wordStatus = `“${text}” was not added${lastWordNotice.reason ? `: ${lastWordNotice.reason}` : ""}`;
      statusKind = "refused";
    }
  }

  const pendingTod = [...pendingIntents].reverse().find((i) => i.name === "time_of_day");
  const tod = pendingTod && typeof pendingTod.args["v"] === "number" ? (pendingTod.args["v"] as number) : st.scene.time_of_day;

  const latency =
    s.lastInputLatencyMs === null
      ? "drag the image to measure input to pixels"
      : `input to pixels: ${fmt.millis(s.lastInputLatencyMs)}`;

  return {
    site: SITE,
    relaunchAll,
    auto: {
      label: "Auto-relaunch, shared",
      options: opts.map((o) => ({ value: autoValue(o), label: autoLabel(o) })),
      selected: autoValue(secs),
      note,
      pending: pendingAuto !== undefined,
    },
    stats: { label: "Stats", on: s.ui.stats },
    light: { label: s.ui.lightMode ? "Click the image to drop it" : "Drop a light", on: s.ui.lightMode },
    word: {
      label: "Write a word into the world",
      placeholder: "a word",
      submit: "Write",
      maxLength: WORD_MAX,
      status: wordStatus,
      statusKind,
    },
    timeOfDay: { label: "Time of day", value: tod, min: 0, max: 1 - 1 / 96, step: 1 / 96, valueText: fmt.timeOfDay(tod), pending: pendingTod !== undefined },
    viewers: `${st.viewers.count} watching`,
    help: "Drag to steer the camera together, scroll to dive. Click a square to open it.",
    latency,
    latencyMeasured: s.lastInputLatencyMs !== null,
  };
}

// ---- actions ticker ----------------------------------------------------------

function actionText(a: Action, you: string | null): string | null {
  if (a.outcome === "refused") return null;
  const mine = you !== null && (a.viewer === you || a.by === you);
  const who = mine ? "you" : a.by === "" ? "someone" : a.by;
  const args = a.args;
  if (args === null) {
    // The coordinator sends a summary instead of the arguments: "relaunch slot-3",
    // "word \"hello\"", "time of day 15.5 h".
    const summary = a.summary ?? "";
    const detail = summary.replace(/^\S+\s*/, "");
    switch (a.name) {
      case "camera":
        return null;
      case "relaunch":
        return `${who} relaunched ${detail}`;
      case "relaunch_all":
        return `${who} relaunched all`;
      case "word":
        return `${who} wrote ${detail}`;
      case "light":
        return `${who} dropped a light`;
      case "set_auto_relaunch":
      case "time_of_day":
        return `${who} set ${summary}`;
      default:
        return summary === "" ? null : `${who}: ${summary}`;
    }
  }
  switch (a.name) {
    case "relaunch":
      return `${who} relaunched ${String(args["tile"] ?? "a tile")}`;
    case "relaunch_all":
      return `${who} relaunched all`;
    case "set_auto_relaunch": {
      const secs = typeof args["secs"] === "number" ? (args["secs"] as number) : null;
      return secs === null ? `${who} turned auto-relaunch off` : `${who} set auto-relaunch to ${fmt.interval(secs)}`;
    }
    case "word":
      return `${who} wrote “${String(args["text"] ?? "")}”`;
    case "light":
      return `${who} dropped a light`;
    case "time_of_day":
      return typeof args["v"] === "number" ? `${who} set the time of day to ${fmt.timeOfDay(args["v"] as number)}` : null;
    case "camera":
      return null;
    default:
      return a.summary === null ? null : `${who}: ${a.summary}`;
  }
}

export interface TickerItem {
  key: string;
  text: string;
  when: string;
}

function tickerView(st: FleetState, srvNow: number, you: string | null): TickerItem[] {
  const out: TickerItem[] = [];
  const sorted = [...st.actions].sort((a, b) => b.at - a.at);
  for (const a of sorted) {
    const text = actionText(a, you);
    if (text === null) continue;
    out.push({ key: `${a.at}-${a.by}-${a.name}-${out.length}`, text, when: fmt.ago(srvNow - a.at) });
    if (out.length === 4) break;
  }
  return out;
}

// ---- focus (one tile opened) --------------------------------------------------

export interface PhaseSegment {
  label: string;
  value: string;
  fraction: number;
  shade: number;
}

export interface PhaseBar {
  title: string;
  accent: string;
  total: string;
  segments: PhaseSegment[];
}

export interface FocusView {
  id: string;
  accent: string;
  title: string;
  state: string;
  nav: string;
  back: string;
  streamTag: string;
  sourceTag: string;
  caption: string;
  rows: { term: string; value: string }[];
  relaunch: { label: string; disabled: boolean };
  lastTitle: string;
  lastValue: string;
  lastNote: string;
  bars: PhaseBar[];
  placed: { box: Box; streamHeight: number } | null;
}

function partsTotal(t: Tile): number {
  const lr = t.last_relaunch;
  if (!lr) return 0;
  return relaunchMs(t) ?? relaunchParts(lr.phases).reduce((a, [, ms]) => a + ms, 0);
}

function phaseBar(t: Tile, axisMs: number): PhaseBar | null {
  const lr = t.last_relaunch;
  if (!lr) return null;
  const parts = relaunchParts(lr.phases);
  if (parts.length === 0) return null;
  return {
    title: `${tileName(t)} (${KIND_TAG[t.half]})`,
    accent: COLORS[t.half].accent,
    total: fmt.seconds(partsTotal(t)),
    segments: parts.map(([label, ms], i) => ({
      label: `${KIND_TAG[t.half]}: ${label}`,
      value: ms < 10_000 ? `${(ms / 1000).toFixed(2)} s` : fmt.seconds(ms),
      fraction: axisMs > 0 ? ms / axisMs : 0,
      shade: i,
    })),
  };
}

/** The tile in the other half at the same place in its half's grid. */
export function mirrorTile(tiles: readonly Tile[], t: Tile): Tile | null {
  const dx = t.half === "fcvm" ? 0.5 : -0.5;
  let best: Tile | null = null;
  let bestD = Infinity;
  for (const o of tiles) {
    if (o.half === t.half) continue;
    const d = Math.abs(o.rect.x - (t.rect.x + dx)) + Math.abs(o.rect.y - t.rect.y);
    if (d < bestD) {
      bestD = d;
      best = o;
    }
  }
  return best;
}

function focusView(s: ClientState, st: FleetState, tv: TileView, t: Tile, input: WallInput): FocusView {
  const fs = s.frames?.tiles[`d:${t.id}`];
  const mirror = mirrorTile(st.tiles, t);
  const pair = [t, mirror].filter((x): x is Tile => x !== null);
  const axis = Math.max(1, ...pair.map(partsTotal));
  const bars = pair.map((x) => phaseBar(x, axis)).filter((b): b is PhaseBar => b !== null);
  const host = t.frame_url ? urlHost(t.frame_url) : null;
  const rms = relaunchMs(t);
  const placed = input.focusArea ? layoutFocus(input.focusArea, t, input.dpr, st.image_aspect) : null;
  return {
    id: t.id,
    accent: tv.accent,
    title: tv.name,
    state: tv.dark ? tv.darkPhase : "running",
    nav: `${SITE} / ${tv.name}`,
    back: "Back to the wall",
    streamTag: fs && fs.newestT !== null ? `${fs.height}p · ${Math.round(fs.fps)} fps` : "no frames yet",
    sourceTag: "H.264 straight from the VM",
    caption:
      "The same rectangle of the image, drawn larger: the GPU spends its budget on more pixels of the same area. Frames come from the VM's own address; nothing relays them.",
    rows: [
      { term: "Kind", value: KIND_NAME[t.half] },
      { term: "GPU", value: t.gpu?.name ?? "none yet" },
      { term: "Host", value: t.host ?? "none yet" },
      { term: "Address", value: host ?? "none yet" },
      { term: "State", value: tv.dark ? tv.darkPhase : "running" },
      ...(t.error ? [{ term: "Error", value: t.error }] : []),
    ],
    relaunch: { label: `Relaunch ${tv.name}`, disabled: t.gpu === null },
    lastTitle: "Last relaunch",
    lastValue: rms === null ? "--" : fmt.seconds(rms),
    lastNote: "Click to first new frame, both kinds on one time axis.",
    bars,
    placed,
  };
}

// ---- phone header --------------------------------------------------------------

export interface PhoneView {
  up: { label: string; value: string };
  fcvm: { label: string; value: string };
  ec2: { label: string; value: string };
}

function phoneView(st: FleetState): PhoneView {
  const med = (h: Half) => fmt.median(halfTiles(st, h).map(relaunchMs).filter((x): x is number => x !== null));
  const mf = med("fcvm");
  const me = med("ec2");
  return {
    up: { label: "Up", value: `${st.tiles.filter(isLive).length}/${st.tiles.length}` },
    fcvm: { label: "fcvm relaunch", value: mf === null ? "--" : fmt.seconds(mf) },
    ec2: { label: "EC2 relaunch", value: me === null ? "--" : fmt.seconds(me) },
  };
}

// ---- what the frame drawer gets -------------------------------------------------

/** One frame stream the drawer may open: a tile's renderer, or an aggregator. */
export interface DrawSource {
  /** `d:<tile id>` for a tile's own stream, `a:<aggregator id>` for an aggregator. */
  id: string;
  /** Frame socket URL with h (and q), or null: do not connect. */
  url: string | null;
  baseUrl: string | null;
  height: number;
  /** Where it is drawn in the mosaic, or null: keep streaming, do not draw. */
  box: Box | null;
}

/** A tile's own state, drawn the same way whatever stream its pixels came from. */
export interface DrawTile {
  id: string;
  half: Half;
  box: Box | null;
  /** Drawn dark over any frame (a relaunch, no GPU, no frames yet). */
  dark: boolean;
  /** Outline of a dark tile, in its half's colour. */
  outline: string;
}

export interface DrawPlan {
  /** The mode this viewer wants; the drawer switches to it when it can do so without a gap. */
  mode: StreamMode;
  direct: DrawSource[];
  aggregated: DrawSource[];
  tiles: DrawTile[];
  /** With a tile opened: its direct source, drawn alone. */
  focus: DrawSource | null;
  image: Box | null;
  dpr: number;
  darkFill: string;
  /** What the direct streams cost at the renderer's bit rate, bits per second. */
  estimatedBps: number;
}

/** The source a tile's pixels come from in a mode: its own stream, or the aggregator holding its centre. */
export function sourceIdFor(t: Tile, mode: StreamMode, aggregators: readonly { id: string; rect: Rect }[]): string {
  if (mode === "direct") return `d:${t.id}`;
  const cx = t.rect.x + t.rect.w / 2;
  const cy = t.rect.y + t.rect.h / 2;
  const a = aggregators.find((g) => cx >= g.rect.x && cx < g.rect.x + g.rect.w && cy >= g.rect.y && cy < g.rect.y + g.rect.h);
  return a ? `a:${a.id}` : `d:${t.id}`;
}

// ---- seams --------------------------------------------------------------------

export interface SeamView {
  /** Fade every boundary in (the pointer is over the image, or a press or key focus). */
  visible: boolean;
  segments: Segment[];
  /** One device pixel, in CSS pixels. */
  lineWidth: number;
  outline: { box: Box; color: string } | null;
  /** The label's top-left corner, placed off the cards and inside the image's area. */
  label: (HoverLabel & { accent: string; x: number; y: number }) | null;
}

function overlaps(a: Box, b: Box): boolean {
  return a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
}

/**
 * Where the hover label goes: the first of above, below, beside and inside the
 * tile that stays inside the area and covers none of the boxes to avoid (the
 * race and counter cards). Failing all, the one covering the least of them.
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
  // Then the same places pushed just past whatever card they hit.
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

function seamView(s: ClientState, layout: MosaicLayout | null, tiles: TileView[], input: WallInput): SeamView {
  const dpr = input.dpr;
  const target = s.ui.hoverId ?? s.ui.keyId;
  const tv = target === null ? null : tiles.find((t) => t.id === target) ?? null;
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

// ---- the whole page ------------------------------------------------------------

export interface WallView {
  title: string;
  site: string;
  ready: boolean;
  version: number;
  banner: string | null;
  layout: MosaicLayout | null;
  tiles: TileView[];
  race: RaceView | null;
  controls: ControlsView | null;
  ticker: TickerItem[];
  focus: FocusView | null;
  phone: PhoneView | null;
  plan: DrawPlan;
  seams: SeamView;
  gridLabel: string;
  notices: { key: string; text: string }[];
  /** What the streams cost, for the stats switch. */
  bandwidth: string;
}

export function pendingIntents(s: ClientState): IntentMsg[] {
  return s.pending.map((p) => p.intent);
}

export function wallView(input: WallInput): WallView {
  const s = input.state;
  const st = s.accepted;
  const srvNow = serverNow(s.clock, input.now);
  let banner: string | null = null;
  if (!input.canDecode) {
    banner = "This browser cannot decode H.264 with WebCodecs, so the tiles stay dark. Chrome, Edge and Safari 16.4 or later can.";
  } else if (s.connection === "closed") {
    banner = "Lost the connection to the fleet. Reconnecting.";
  } else if (!st) {
    banner = "Connecting to the fleet.";
  }
  const emptyPlan: DrawPlan = { mode: "direct", direct: [], aggregated: [], tiles: [], focus: null, image: null, dpr: input.dpr, darkFill: "#06080B", estimatedBps: 0 };
  if (!st) {
    return {
      title: TITLE,
      site: SITE,
      ready: false,
      version: s.version,
      banner,
      layout: null,
      tiles: [],
      race: null,
      controls: null,
      ticker: [],
      focus: null,
      phone: null,
      plan: emptyPlan,
      seams: { visible: false, segments: [], lineWidth: 1 / input.dpr, outline: null, label: null },
      gridLabel: "",
      notices: [],
      bandwidth: "",
    };
  }
  const pend = pendingIntents(s);
  const layout = input.mosaicArea ? layoutMosaic(input.mosaicArea, st.tiles, input.dpr, st.image_aspect) : null;
  const placedById = new Map(layout?.tiles.map((p) => [p.id, p]) ?? []);
  const tiles = st.tiles.map((t) => tileView(t, s, st, placedById.get(t.id)?.box ?? null, srvNow, pend));
  const focusTile = s.ui.focusId === null ? null : st.tiles.find((t) => t.id === s.ui.focusId) ?? null;
  const focus = focusTile
    ? focusView(s, st, tiles.find((x) => x.id === focusTile.id)!, focusTile, input)
    : null;

  // Direct: each tile asks for the device pixels it covers; with a tile
  // opened, the others (still streaming, not drawn) ask for the smallest; then
  // small tiles drop to q=lo and the tallest step down until the streams fit
  // the bandwidth budget.
  const opened = focus && focus.placed ? focusTile : null;
  const asks: StreamAsk[] = st.tiles.filter(isLive).map((t) => {
    const focused = opened !== null && opened.id === t.id;
    const height = focused ? focus!.placed!.streamHeight : focus ? STREAM_HEIGHTS[0] : placedById.get(t.id)?.streamHeight ?? 144;
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
      box: focus ? (opened?.id === t.id ? focus.placed!.box : null) : placedById.get(t.id)?.box ?? null,
    };
  });
  // Aggregated: each aggregator is one big tile; it asks for the height its
  // region covers, like a tile.
  const aggregated: DrawSource[] = st.aggregators
    .filter((g) => g.frame_url !== null)
    .map((g) => {
      const box = layout ? rectOnScreen(layout.image, g.rect, input.dpr) : null;
      const height = focus || !box ? STREAM_HEIGHTS[0] : streamHeightFor(box.h * input.dpr);
      return { id: `a:${g.id}`, url: frameSocketUrl(g.frame_url!, height), baseUrl: g.frame_url, height, box: focus ? null : box };
    });
  const plan: DrawPlan = {
    ...emptyPlan,
    mode: st.aggregators.length > 0 ? s.ui.mode : "direct",
    direct,
    aggregated,
    focus: opened ? direct.find((d) => d.id === `d:${opened.id}`) ?? null : null,
    estimatedBps: fitted.bps,
    image: focus ? null : layout?.image ?? null,
    tiles: st.tiles.map((t, i) => {
      const tv = tiles[i]!;
      return {
        id: t.id,
        half: t.half,
        box: focus ? null : placedById.get(t.id)?.box ?? null,
        dark: tv.dark && !(tv.darkPhase === "connecting"),
        outline: COLORS[t.half].darkOutline,
      };
    }),
  };

  return {
    title: TITLE,
    site: SITE,
    ready: true,
    version: s.version,
    banner,
    layout,
    tiles,
    race: raceView(st, srvNow),
    controls: { ...controlsView(s, st, srvNow, pend), mode: modeView(s, st, plan, input) },
    ticker: tickerView(st, srvNow, s.you),
    focus,
    phone: input.phone ? phoneView(st) : null,
    plan,
    seams: seamView(s, focus ? null : layout, tiles, input),
    bandwidth: `${fmt.count(plan.direct.length, "direct stream", "direct streams")}, about ${fmt.mbps(plan.estimatedBps)} at the renderer's bit rate`,
    notices: s.notices.flatMap((n) => {
      const text = noticeText(n);
      return text === null ? [] : [{ key: n.id, text }];
    }),
    gridLabel: `The image, ${fmt.count(st.tiles.length, "tile", "tiles")}: fcvm on the left, EC2 on the right. Arrow keys move between tiles, Enter opens one.`,
  };
}

/** Notices older than this are dropped. */
export const NOTICE_MS = 8000;

const INTENT_LABEL: Record<string, string> = {
  relaunch_all: "Relaunch all",
  set_auto_relaunch: "Auto-relaunch",
  light: "The light",
  time_of_day: "The time of day",
  camera: "The camera move",
};

/** A refused action, as the page says it. Words have their own status line. */
export function noticeText(n: Notice): string | null {
  if (n.kind !== "refused" || n.intent === "word") return null;
  const what = n.intent === "relaunch" ? `Relaunch ${String(n.args["tile"] ?? "")}` : INTENT_LABEL[n.intent] ?? n.intent;
  return `${what} was not applied${n.reason ? `: ${n.reason}` : ""}`;
}
