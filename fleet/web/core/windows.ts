// Windows mode (`?mode=windows`): disposable Windows sandboxes, each a desktop drawn from its own
// stream, and the panel of the one sandbox this viewer holds. Pure: the page measures the stage
// and passes it in with the time, and draws what comes back (PROTOCOL.md, "Windows mode").
// Nothing here describes a click or a key on a desktop: a viewer resets a sandbox, borrows one
// and presses fixed actions, and the intents carry no point and no text.

import { snap, type Box } from "./layout.ts";
import * as fmt from "./format.ts";
import { serverNow } from "./clock.ts";
import { sameViewer, type Windows, type WindowTile } from "./protocol.ts";
import type { ClientState, Notice } from "./store.ts";
import { BLANK_PLAN, directPlan, gridOf, openedArea, tenths } from "./phones.ts";
import { phaseText, sleepView, verifyView, type DrawPlan, type SleepView } from "./view.ts";

export const WINDOWS_TITLE = "Windows";
export const WINDOWS_SENTENCE = "Each sandbox is a Windows desktop in its own virtual machine, restored from one snapshot. Click a desktop to throw it away and restore a new one.";
export const NO_WINDOWS = "This fleet runs no Windows sandboxes.";

/** Between two tiles, CSS pixels. */
export const WINDOWS_TILE_GAP = 12;
/** Room under a tile for its slot, edition, time to desktop and label, and the same with the panel open on a narrow screen. */
export const WINDOWS_CAPTION_H = 96;
export const WINDOWS_CAPTION_H_NARROW = 120;
export const WINDOWS_CAPTION_H_COMPACT = 104;
/** A caption narrower than this is compact: its words smaller, its edition left out. */
export const WINDOWS_CAPTION_FULL_MIN_W = 170;
/** One action a second from a holder (PROTOCOL.md, "Windows mode": Actions). */
export const WINDOWS_ACTION_INTERVAL_MS = 1000;

export const WINDOWS_ACTION_LABELS: Record<string, string> = {
  start_menu: "Start menu",
  notepad: "Notepad",
  explorer: "File Explorer",
  task_manager: "Task Manager",
  about: "About Windows",
  terminal: "PowerShell",
  close_all: "Close all",
  reset: "Reset",
};

const WINDOWS_INTENTS = new Set(["windows_reset", "claim_windows", "release_windows", "windows_action"]);

/** The newest refusal of a Windows intent still on screen, as the reason the coordinator gave. */
export function windowsNotice(notices: readonly Notice[]): string | null {
  for (let i = notices.length - 1; i >= 0; i--) {
    const n = notices[i]!;
    if (n.kind === "refused" && WINDOWS_INTENTS.has(n.intent)) return n.reason ?? "The coordinator refused it.";
  }
  return null;
}

// ---- layout --------------------------------------------------------------------------

export interface WindowBox {
  id: string;
  /** Where the desktop is drawn. */
  box: Box;
  /** Under it. */
  caption: Box;
  /** The caption is drawn compact (`WINDOWS_CAPTION_FULL_MIN_W`). */
  compact: boolean;
}

export interface WindowsLayoutInput {
  area: Box;
  tiles: readonly WindowTile[];
  imageAspect: number;
  /** A phone-sized screen: two columns, or one row with the panel open. */
  narrow: boolean;
  /** The take-over panel is open. */
  panel: boolean;
  dpr: number;
}

/** The sandboxes' own tile shape from the snapshot: one tile's rectangle in the image of `imageAspect`. */
export function windowTileAspect(tiles: readonly WindowTile[], imageAspect: number): number {
  const r = tiles[0]?.rect;
  const a = r ? (r.w * imageAspect) / r.h : 0;
  return a > 0 && Number.isFinite(a) ? a : 16 / 9;
}

/**
 * Every desktop's box and the caption under it. The rows and columns are the snapshot's (one row
 * of up to four, then rows of equal length); a narrow screen takes two columns, or rows of up to
 * four while the panel is open. Each cell is as large as the area allows with the caption's room
 * taken off, and the block is centred across and starts at the top. A cell narrower than
 * `WINDOWS_CAPTION_FULL_MIN_W` gets a compact caption, and on a narrow screen then the columns
 * that make the desktops largest.
 */
export function layoutWindows(inp: WindowsLayoutInput): WindowBox[] {
  const { area, dpr, tiles } = inp;
  if (tiles.length === 0) return [];
  const sorted = [...tiles].sort((a, b) => a.rect.y - b.rect.y || a.rect.x - b.rect.x);
  const n = sorted.length;
  let compact = inp.narrow && inp.panel;
  let capH = compact ? WINDOWS_CAPTION_H_COMPACT : inp.narrow ? WINDOWS_CAPTION_H_NARROW : WINDOWS_CAPTION_H;
  const aspect = windowTileAspect(tiles, inp.imageAspect);
  /** The cell width, in whole device pixels, that `cols` columns allow. */
  const cellFor = (cols: number) => {
    const rows = Math.ceil(n / cols);
    const availH = area.h - rows * capH - (rows - 1) * WINDOWS_TILE_GAP;
    return Math.max(0, Math.floor(Math.min((area.w - (cols - 1) * WINDOWS_TILE_GAP) / cols, (availH / rows) * aspect) * dpr) / dpr);
  };
  /** The columns, `min` or more, that make the desktops largest; `from` on a tie. */
  const largest = (from: number, min = 1) => {
    let best = from;
    for (let c = min; c <= n; c++) if (cellFor(c) > cellFor(best)) best = c;
    return best;
  };
  const rectCols = Math.max(1, gridOf(sorted).cols);
  let cols = inp.narrow ? (inp.panel ? Math.min(4, n) : Math.min(2, n)) : rectCols;
  // Beside the open panel the room is narrow: take the columns that make the desktops largest.
  if (inp.panel && !inp.narrow) cols = largest(cols);
  if (!compact && cellFor(cols) < WINDOWS_CAPTION_FULL_MIN_W) {
    compact = true;
    capH = WINDOWS_CAPTION_H_COMPACT;
  }
  // A wide screen keeps the snapshot's rows; a phone whose cells are under a full caption's
  // width takes the columns that make the desktops largest. Above the open panel it keeps rows
  // of at least four, so a short room gets one long row rather than rows of no height.
  if (inp.narrow && cellFor(cols) < WINDOWS_CAPTION_FULL_MIN_W) cols = largest(cols, inp.panel ? cols : 1);
  const cellW = cellFor(cols);
  const cellH = snap(cellW / aspect, dpr);
  const top = area.y;
  return sorted.map((t, i) => {
    const row = Math.floor(i / cols);
    const len = Math.min(cols, n - row * cols);
    const rowW = len * cellW + (len - 1) * WINDOWS_TILE_GAP;
    const x = area.x + (area.w - rowW) / 2 + (i % cols) * (cellW + WINDOWS_TILE_GAP);
    const y = top + row * (cellH + capH + WINDOWS_TILE_GAP);
    const box: Box = { x: snap(x, dpr), y: snap(y, dpr), w: cellW, h: cellH };
    return { id: t.id, box, caption: { x: box.x, y: box.y + box.h + 4, w: box.w, h: capH - 4 }, compact };
  });
}

// ---- words ---------------------------------------------------------------------------

/** "win-3" for the sandbox on slot `win-3`. */
export function sandboxName(t: { id: string; slot: string | null }): string {
  return t.slot ?? t.id;
}

/** "2.31 s": the time from a click to a desktop, two decimals. */
export function hundredthsText(ms: number): string {
  return `${(Math.max(0, ms) / 1000).toFixed(2)} s`;
}

const TRANSIENT = new Set(["restoring", "connecting", "relaunching", "pending", "requested", "stopping", "starting"]);

/** Whether a sandbox is between a reset and its first desktop frame. */
export function isRestoring(t: { phase: string }): boolean {
  return TRANSIENT.has(t.phase);
}

/**
 * The dark tile's running timer: "Restoring 1.4 s" counting from the click that asked for the
 * sandbox, "Connecting 2.1 s" once the sandbox has started. Without a launch to count from, the
 * word alone.
 */
export function restoringText(t: WindowTile, srvNow: number): string {
  const word = t.phase === "connecting" ? "Connecting" : "Restoring";
  const lr = t.last_relaunch;
  return lr === null ? word : `${word} ${tenths(srvNow - lr.clicked_at)}`;
}

/** "Desktop in 2.31 s" from the click to the first frame of the desktop, or null before one. */
export function desktopInText(t: WindowTile): string | null {
  const lr = t.last_relaunch;
  return lr !== null && lr.first_frame_at !== null ? `Desktop in ${hundredthsText(lr.first_frame_at - lr.clicked_at)}` : null;
}

/** What a failed sandbox says. */
export function failedText(t: WindowTile): string {
  return `Failed: ${t.error ?? t.failure?.message ?? "the sandbox did not come back"}`;
}

/** Each running sandbox's last time to desktop, ms: those whose last reset has reached its first frame. */
export function desktopTimes(tiles: readonly WindowTile[]): number[] {
  const out: number[] = [];
  for (const t of tiles) {
    const lr = t.last_relaunch;
    if (lr !== null && lr.first_frame_at !== null) out.push(Math.max(0, lr.first_frame_at - lr.clicked_at));
  }
  return out;
}

export function medianMs(v: readonly number[]): number | null {
  if (v.length === 0) return null;
  const s = [...v].sort((a, b) => a - b);
  const mid = Math.floor(s.length / 2);
  return s.length % 2 === 1 ? s[mid]! : (s[mid - 1]! + s[mid]!) / 2;
}

function actionLabel(name: string): string {
  return WINDOWS_ACTION_LABELS[name] ?? name.replace(/_/g, " ").replace(/^./, (c) => c.toUpperCase());
}

// ---- the view ------------------------------------------------------------------------

export interface WindowCard {
  id: string;
  slot: string;
  edition: string | null;
  /** "Desktop in 2.31 s" once the sandbox has drawn its desktop. */
  desktopIn: string | null;
  /** The dark tile's running timer, while the sandbox restores. */
  timer: string | null;
  /** A failed sandbox's reason. */
  error: string | null;
  /** "Windows Server 2025 evaluation, for demonstration only". */
  label: string;
  box: Box | null;
  caption: Box | null;
  dark: boolean;
  /** Smaller words and no edition, for a narrow caption (`WINDOWS_CAPTION_FULL_MIN_W`). */
  compact: boolean;
  /** Whether the caption draws Take over (a compact caption leaves it out while the panel is open). */
  showTake: boolean;
  /** Clicking the tile resets this sandbox. */
  reset: { label: string; disabled: boolean };
  take: { label: string; disabled: boolean; title: string | null };
  /** "Lent to another viewer until 12:03:20 UTC" or "Yours". */
  lentText: string | null;
  mine: boolean;
  lent: boolean;
  ariaLabel: string;
}

export interface WindowsHeader {
  up: string;
  median: string;
  slowest: string;
  lastResetAll: string;
  /** The image's edition, build and age. */
  image: string | null;
  warning: string | null;
  refused: string | null;
  /** While the gold snapshot builds or after it failed. */
  gold: string | null;
}

export interface WindowsPanel {
  id: string;
  slot: string;
  title: string;
  session: string;
  countdown: string;
  label: string;
  note: string;
  actions: { name: string; label: string; disabled: boolean }[];
  giveBack: string;
}

export interface WindowsInput {
  state: ClientState;
  /** Local Unix ms. */
  now: number;
  dpr: number;
  /** Where the sandboxes may go: the page under its header, measured. */
  stage: Box | null;
  narrow: boolean;
  reducedMotion: boolean;
  canDecode: boolean;
}

export interface WindowsView {
  title: string;
  sentence: string;
  online: boolean;
  /** Why there is nothing to show, when there is nothing. */
  empty: string | null;
  banner: string | null;
  header: WindowsHeader | null;
  resetAll: { label: string; disabled: boolean };
  cards: WindowCard[];
  panel: WindowsPanel | null;
  verify: ReturnType<typeof verifyView>;
  /** A refusal the viewer should read. */
  notice: string | null;
  plan: DrawPlan;
  sleep: SleepView | null;
}

function seatsFull(reason: string | null, seatFree: boolean): string {
  const why = reason ?? "Direct streams are full.";
  return seatFree
    ? `${why} A direct seat is free now: reload the page to take it and see the sandboxes.`
    : `${why} Sandboxes stream only to viewers with a direct seat, so they stay dark on this page until one is free.`;
}

function who(by: string, you: string | null): string {
  if (sameViewer(by, you)) return "you";
  return by.startsWith("viewer:") ? "another viewer" : by.replace(/_/g, " ");
}

function headerOf(w: Windows, you: string | null): WindowsHeader {
  const times = desktopTimes(w.tiles);
  const median = medianMs(times);
  const up = w.tiles.filter((t) => t.phase === "running").length;
  const img = w.image;
  const days = img === null ? 0 : img.age_days;
  return {
    up: `${up} of ${w.tiles.length} ${w.tiles.length === 1 ? "sandbox" : "sandboxes"} up`,
    median: `Median time to desktop ${median === null ? "--" : hundredthsText(median)}`,
    slowest: `Slowest ${times.length === 0 ? "--" : hundredthsText(Math.max(...times))}`,
    lastResetAll:
      w.last_reset_all === null
        ? "Reset all: not used yet"
        : `Last reset all: ${fmt.utcClock(w.last_reset_all.clicked_at)} UTC, ${w.last_reset_all.sandboxes} ${w.last_reset_all.sandboxes === 1 ? "sandbox" : "sandboxes"}, by ${who(w.last_reset_all.by, you)}`,
    image: img === null ? null : `${img.edition}, build ${img.build}, ${days} ${days === 1 ? "day" : "days"} old`,
    warning: img?.warning ?? null,
    refused: img?.refused ?? null,
    gold:
      img === null || img.gold === "ready"
        ? null
        : img.gold === "building"
          ? "Building the Windows snapshot. Sandboxes start when it is ready."
          : `The Windows snapshot failed to build${img.gold_error ? `: ${img.gold_error}` : ""}.`,
  };
}

export function windowsView(inp: WindowsInput): WindowsView {
  const s = inp.state;
  const w = s.accepted?.windows ?? null;
  const online = s.connection === "open";
  const srvNow = serverNow(s.clock, inp.now);
  const common = {
    title: WINDOWS_TITLE,
    sentence: WINDOWS_SENTENCE,
    online,
    verify: verifyView(s),
    notice: windowsNotice(s.notices),
    sleep: s.accepted === null ? null : sleepView(s.accepted, srvNow, online, s.pending.some((p) => p.intent.name === "wake")),
  };
  const banner = !inp.canDecode
    ? "This browser cannot decode H.264 with WebCodecs, so the picture stays still. Chrome, Edge and Safari 16.4 or later can."
    : s.admission?.locked === true
      ? seatsFull(s.admission.reason ?? null, s.admission.seat_free)
      : s.accepted !== null && !online
        ? "The coordinator is offline. The picture still streams straight from the sandboxes; the controls are paused until it is back."
        : null;
  if (w === null || s.accepted === null) {
    return {
      ...common,
      empty: s.accepted === null ? (s.connection === "closed" ? "Cannot reach the fleet yet. Trying again." : "Connecting to the fleet.") : NO_WINDOWS,
      banner,
      header: null,
      resetAll: { label: "Reset all", disabled: true },
      cards: [],
      panel: null,
      plan: BLANK_PLAN(s, inp.dpr),
    };
  }
  const mine = (t: WindowTile) => t.borrowed !== null && sameViewer(t.borrowed.viewer ?? null, s.you);
  const held = w.tiles.find((t) => mine(t) && t.borrowed?.session !== undefined) ?? null;
  const panelOpen = held !== null;
  const area = inp.stage === null ? null : panelOpen ? openedArea(inp.stage, inp.narrow) : inp.stage;
  const placed = new Map(area === null ? [] : layoutWindows({ area, tiles: w.tiles, imageAspect: w.image_aspect, narrow: inp.narrow, panel: panelOpen, dpr: inp.dpr }).map((b) => [b.id, b]));
  const cards: WindowCard[] = w.tiles.map((t) => {
    const p = placed.get(t.id);
    const slot = sandboxName(t);
    const running = t.phase === "running";
    const restoring = isRestoring(t);
    const lentToOther = t.borrowed !== null && !mine(t);
    const state = running ? "streaming" : phaseText(t.phase);
    let takeTitle: string | null = null;
    if (held !== null && !mine(t)) takeTitle = `You are using ${sandboxName(held)}. Give it back to take another.`;
    else if (lentToOther) takeTitle = "Lent to another viewer.";
    else if (!running) takeTitle = "It can be borrowed once it has restored.";
    return {
      id: t.id,
      slot,
      edition: w.image?.edition ?? null,
      desktopIn: desktopInText(t),
      timer: restoring ? restoringText(t, srvNow) : null,
      error: t.phase === "failed" ? failedText(t) : null,
      label: w.label,
      box: p?.box ?? null,
      caption: p?.caption ?? null,
      dark: !running,
      compact: p?.compact ?? false,
      // With its panel open, this viewer takes nothing more.
      showTake: !(p?.compact === true && panelOpen),
      reset: { label: `Reset ${slot}`, disabled: !online || t.borrowed !== null },
      take: { label: "Take over", disabled: !online || !running || t.borrowed !== null || held !== null, title: takeTitle },
      lentText: mine(t) ? "Yours" : lentToOther ? `Lent to another viewer until ${fmt.utcClock(t.borrowed!.until)} UTC` : null,
      mine: mine(t),
      lent: lentToOther,
      ariaLabel: `${slot}, ${state}${mine(t) ? ", your sandbox" : lentToOther ? ", lent" : ""}. ${t.borrowed !== null ? "It cannot be reset while it is lent." : "Click to reset it."}`,
    };
  });
  let panel: WindowsPanel | null = null;
  if (held !== null) {
    const b = held.borrowed!;
    const left = Math.max(0, Math.ceil((b.until - srvNow) / 1000));
    const running = held.phase === "running";
    panel = {
      id: held.id,
      slot: sandboxName(held),
      title: sandboxName(held),
      session: b.session!,
      countdown: `Returns in ${left} s`,
      label: w.label,
      note: "Fixed actions only: nothing you type or point at reaches Windows. The sandbox is reset when you give it back.",
      actions: held.actions.map((name) => ({ name, label: actionLabel(name), disabled: !online || !running })),
      giveBack: "Give back",
    };
  }
  const boxes = new Map([...placed].map(([id, b]) => [id, b.box]));
  return {
    ...common,
    empty: null,
    banner,
    header: headerOf(w, s.you),
    resetAll: { label: "Reset all", disabled: !online || w.tiles.length === 0 },
    cards,
    panel,
    plan: directPlan({ state: s, tiles: w.tiles, imageAspect: w.image_aspect, boxes, dpr: inp.dpr, reducedMotion: inp.reducedMotion }),
  };
}
