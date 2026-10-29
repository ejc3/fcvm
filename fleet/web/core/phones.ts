// Phones mode (`?mode=phones`): the phones' own image, drawn seamless or zoomed out into
// bezels, and the panel of one opened phone. Pure: the page measures the stage and passes
// it in, and draws what comes back (PROTOCOL.md, "Phones mode").

import {
  easeZoom,
  fitContain,
  lerpBox,
  rectOnScreen,
  snap,
  streamHeightFor,
  ZOOM_MS,
  type Box,
} from "./layout.ts";
import { streamBitrate } from "./bandwidth.ts";
import * as fmt from "./format.ts";
import { frameSocketUrl, streamUrl } from "./net.ts";
import { sameViewer, type Phones, type Tile } from "./protocol.ts";
import { serverNow } from "./clock.ts";
import type { ClientState, Notice } from "./store.ts";
import {
  COLORS,
  phaseText,
  REDUCED_MOTION_DARK_FADE_MS,
  timelineOf,
  verifyView,
  type DrawPlan,
  type DrawSource,
  type DrawTile,
  type TimelineStep,
} from "./view.ts";

export const PHONES_TITLE = "Phones";
export const PHONES_SENTENCE = "Each phone is an Android emulator in its own virtual machine, drawing on its own NVIDIA GPU. Zoom out to see the phones apart.";
export const NO_PHONES = "This fleet runs no phones.";

/** A phone's screen, width over height (720 x 1280). */
export const PHONE_ASPECT = 720 / 1280;

/** The phone body around the screen, as fractions of the screen's short side. */
export const BEZEL = {
  /** Beside the screen (the long edges). */
  side: 0.06,
  /** Above and below it (the short edges, where the speaker and the camera sit). */
  end: 0.1,
  /** Between two bodies. */
  gap: 0.14,
  /** The body's outer corner radius. */
  radius: 0.12,
} as const;

/** Room under a phone's body for its label, CSS pixels. */
export const LABEL_H = 44;
/** The panel beside an opened phone on a wide screen, and the space left between. */
export const PANEL_W = 380;
export const PANEL_GAP = 16;
/** The share of a narrow screen's stage an opened phone gets; the sheet takes the rest. */
export const NARROW_PHONE_SHARE = 0.5;

/** Distance (CSS px) the two ends of one axis may differ and still be the same row or column. */
const SAME = 1e-6;

export interface Pad {
  top: number;
  right: number;
  bottom: number;
  left: number;
}

/** One phone placed: where its screen is drawn and the body around it. A null box: not drawn. */
export interface PhoneBox {
  id: string;
  box: Box | null;
  /** The body: `box` grown by `pad`. Equal to `box` with no pad at progress 0. */
  bezel: Box | null;
  pad: Pad;
  /** The body's outer corner radius, CSS px. */
  radius: number;
  /** 0 seamless, 1 apart. */
  bezelOpacity: number;
  /** Under the body. */
  label: Box | null;
}

export interface PhoneLayout {
  /** The phones' whole image when stitched. */
  image: Box;
  phones: PhoneBox[];
  /** Eased progress, 0 (seamless) to 1 (apart). */
  eased: number;
}

export interface PhoneLayoutInput {
  area: Box;
  imageAspect: number;
  tiles: readonly Tile[];
  /** Linear progress of the zoom, 0 seamless to 1 apart. */
  progress: number;
  dpr: number;
  /** One phone shown on its own, apart, whatever the progress; the others are not placed. */
  open?: string | null;
  /** A phone's screen width over height when it is not the upright 9:16 (a turned phone). */
  aspects?: Readonly<Record<string, number>>;
  labelH?: number;
}

const NO_PAD: Pad = { top: 0, right: 0, bottom: 0, left: 0 };

/** Pads and body size for a screen of `aspect` whose short side is `u`. */
function bodyOf(u: number, aspect: number): { pad: Pad; w: number; h: number; sw: number; sh: number } {
  const landscape = aspect > 1;
  const sw = landscape ? u * aspect : u;
  const sh = landscape ? u : u / aspect;
  const pad: Pad = landscape
    ? { top: u * BEZEL.side, bottom: u * BEZEL.side, left: u * BEZEL.end, right: u * BEZEL.end }
    : { top: u * BEZEL.end, bottom: u * BEZEL.end, left: u * BEZEL.side, right: u * BEZEL.side };
  return { pad, sw, sh, w: sw + pad.left + pad.right, h: sh + pad.top + pad.bottom };
}

/** The rows of a set of rectangles: each tile's row and its place in it, and the longest row. */
function gridOf(tiles: readonly Tile[]): { rows: number; cols: number; at: Map<string, { row: number; col: number; len: number }> } {
  const ys: number[] = [];
  for (const t of [...tiles].sort((a, b) => a.rect.y - b.rect.y)) if (!ys.some((y) => Math.abs(y - t.rect.y) < SAME)) ys.push(t.rect.y);
  const rowOf = (t: Tile) => ys.findIndex((y) => Math.abs(y - t.rect.y) < SAME);
  const byRow = ys.map((_, r) => tiles.filter((t) => rowOf(t) === r).sort((a, b) => a.rect.x - b.rect.x));
  const at = new Map<string, { row: number; col: number; len: number }>();
  byRow.forEach((row, r) => row.forEach((t, c) => at.set(t.id, { row: r, col: c, len: row.length })));
  return { rows: ys.length, cols: Math.max(0, ...byRow.map((r) => r.length)), at };
}

/**
 * Every phone's screen and body at a zoom progress. At 0 the screens are the phones' image
 * fitted in the area, each phone at its rectangle: neighbours share edges exactly, as the
 * wall's tiles do. At 1 each phone sits in a body of its own, in the same rows and columns, with
 * gaps between the bodies and a label under each. In between the boxes move linearly (the
 * progress eased), the body grows from nothing and fades in.
 */
export function layoutPhones(inp: PhoneLayoutInput): PhoneLayout {
  const { area, tiles, dpr } = inp;
  const labelH = inp.labelH ?? LABEL_H;
  const e = inp.open != null ? 1 : easeZoom(inp.progress);
  const fitted = fitContain(area, inp.imageAspect);
  const image: Box = {
    x: snap(fitted.x, dpr),
    y: snap(fitted.y, dpr),
    w: snap(fitted.x + fitted.w, dpr) - snap(fitted.x, dpr),
    h: snap(fitted.y + fitted.h, dpr) - snap(fitted.y, dpr),
  };
  const aspectOf = (id: string) => {
    const a = inp.aspects?.[id];
    return a !== undefined && a > 0 && Number.isFinite(a) ? a : PHONE_ASPECT;
  };
  /** The body of a phone fitted in a cell of `cell` (label room already taken off), centred. */
  const fitBody = (id: string, cell: Box) => {
    const one = bodyOf(1, aspectOf(id));
    const f = Math.max(0, Math.min(cell.w / one.w, cell.h / one.h));
    const b = bodyOf(f, aspectOf(id));
    const bezel: Box = { x: cell.x + (cell.w - b.w) / 2, y: cell.y + (cell.h - b.h) / 2, w: b.w, h: b.h };
    const box: Box = { x: bezel.x + b.pad.left, y: bezel.y + b.pad.top, w: b.sw, h: b.sh };
    return { box, bezel, pad: b.pad, u: f };
  };
  /**
   * Where a phone sits in the seamless view: its part of the picture, or, for a turned phone
   * (its stream's orientation is not its rect's), its whole screen fitted inside that part and
   * centred, since a turned phone draws its whole screen.
   */
  const seamlessBox = (id: string, rect: Box | null): Box | null => {
    const a = inp.aspects?.[id];
    if (rect === null || rect.h <= 0 || a === undefined || !(a > 0) || !Number.isFinite(a)) return rect;
    if (rect.w / rect.h > 1 === a > 1) return rect;
    const h = Math.min(rect.h, rect.w / a);
    const w = h * a;
    return { x: rect.x + (rect.w - w) / 2, y: rect.y + (rect.h - h) / 2, w, h };
  };
  const place = (id: string, apart: Box | null, stitched: Box | null, label: Box | null): PhoneBox => {
    if (apart === null) return { id, box: null, bezel: null, pad: NO_PAD, radius: 0, bezelOpacity: 0, label: null };
    // A body that grows from the screen's own edge: its pad follows the box it surrounds.
    const box = stitched === null || e >= 1 ? apart : lerpBox(stitched, apart, e);
    const u = Math.min(box.w, box.h) * e;
    const landscape = box.w > box.h;
    const pad: Pad = landscape
      ? { top: u * BEZEL.side, bottom: u * BEZEL.side, left: u * BEZEL.end, right: u * BEZEL.end }
      : { top: u * BEZEL.end, bottom: u * BEZEL.end, left: u * BEZEL.side, right: u * BEZEL.side };
    const bezel: Box = { x: box.x - pad.left, y: box.y - pad.top, w: box.w + pad.left + pad.right, h: box.h + pad.top + pad.bottom };
    return { id, box, bezel, pad, radius: u * BEZEL.radius, bezelOpacity: e, label };
  };

  if (inp.open != null) {
    const cell: Box = { x: area.x, y: area.y, w: area.w, h: Math.max(0, area.h - labelH) };
    return {
      image,
      eased: 1,
      phones: tiles.map((t) => {
        if (t.id !== inp.open) return place(t.id, null, null, null);
        const fit = fitBody(t.id, cell);
        const label: Box = { x: fit.bezel.x, y: fit.bezel.y + fit.bezel.h + 6, w: fit.bezel.w, h: labelH - 6 };
        return place(t.id, fit.box, null, label);
      }),
    };
  }

  const grid = gridOf(tiles);
  // The cell of the widest row and the tallest column: every body the same size.
  const gapUnits = BEZEL.gap;
  const one = bodyOf(1, PHONE_ASPECT);
  const widthPerU = grid.cols * one.w + (grid.cols - 1) * gapUnits;
  const heightPerU = grid.rows * one.h + (grid.rows - 1) * gapUnits;
  const u = grid.cols === 0 ? 0 : Math.max(0, Math.min(area.w / widthPerU, (area.h - grid.rows * labelH) / heightPerU));
  const cellW = one.w * u;
  const cellH = one.h * u;
  const gap = gapUnits * u;
  const blockH = grid.rows * (cellH + labelH) + Math.max(0, grid.rows - 1) * gap;
  const top = area.y + (area.h - blockH) / 2;
  return {
    image,
    eased: e,
    phones: tiles.map((t) => {
      const g = grid.at.get(t.id)!;
      const rowW = g.len * cellW + (g.len - 1) * gap;
      const cell: Box = {
        x: area.x + (area.w - rowW) / 2 + g.col * (cellW + gap),
        y: top + g.row * (cellH + labelH + gap),
        w: cellW,
        h: cellH,
      };
      const fit = fitBody(t.id, cell);
      const label: Box = { x: cell.x, y: cell.y + cell.h + 6, w: cell.w, h: labelH - 6 };
      return place(t.id, fit.box, seamlessBox(t.id, rectOnScreen(fitted, t.rect, dpr)), label);
    }),
  };
}

/** Where an opened phone goes: the stage without the panel on its right, or the top of the stage above a bottom sheet. */
export function openedArea(stage: Box, narrow: boolean): Box {
  if (narrow) return { x: stage.x, y: stage.y, w: stage.w, h: Math.round(stage.h * NARROW_PHONE_SHARE) };
  return { x: stage.x, y: stage.y, w: Math.max(0, stage.w - PANEL_W - 2 * PANEL_GAP), h: stage.h };
}

/** The zoom's linear progress `now` ms after it started moving from `from` to `to` (0 or 1). */
export function zoomProgressAt(from: number, to: number, startedAt: number, now: number): number {
  const k = Math.min(1, Math.max(0, (now - startedAt) / ZOOM_MS));
  return from + (to - from) * k;
}

// ---- words ---------------------------------------------------------------------

/** "Phone 2" for `phone:2`. */
export function phoneName(t: { id: string }): string {
  const k = /:(\d+)$/.exec(t.id)?.[1];
  return k === undefined ? t.id : `Phone ${k}`;
}

const tenths = (ms: number) => `${(Math.max(0, ms) / 1000).toFixed(1)} s`;

/**
 * How a phone says it was restored: "Phone restored in 4.6 s" from the click to the app's first
 * frame, "Restoring" while it is not running and has not drawn one, nothing before the
 * coordinator has timed a launch.
 */
export function restoreText(t: Tile): { text: string; restoring: boolean } | null {
  const lr = t.last_relaunch;
  if (lr !== null && lr.first_frame_at !== null) return { text: `Phone restored in ${tenths(lr.first_frame_at - lr.clicked_at)}`, restoring: false };
  if (t.phase !== "running") return { text: "Restoring", restoring: true };
  return null;
}

/** The short line under a phone: "Phone 2 · restored in 22.3 s". */
export function phoneLabel(t: Tile): string {
  const lr = t.last_relaunch;
  if (lr !== null && lr.first_frame_at !== null) return `${phoneName(t)} · restored in ${tenths(lr.first_frame_at - lr.clicked_at)}`;
  return t.phase !== "running" ? `${phoneName(t)} · Restoring` : phoneName(t);
}

export const ACTION_LABELS: Record<string, string> = {
  switch_screen: "Switch screen",
  rotate: "Rotate",
  dark_mode: "Dark mode",
  dance: "Dance",
  wave: "Wave",
  gpu_benchmark: "GPU benchmark",
  restart_app: "Restart app",
  reboot_phone: "Reboot phone",
};

const PHONE_INTENTS = new Set(["claim_phone", "release_phone", "phone_action", "phones_zoom"]);

/** The newest refusal of a phone intent still on screen, as the reason the coordinator gave. */
export function phoneNotice(notices: readonly Notice[]): string | null {
  for (let i = notices.length - 1; i >= 0; i--) {
    const n = notices[i]!;
    if (n.kind === "refused" && PHONE_INTENTS.has(n.intent)) return n.reason ?? "The coordinator refused it.";
  }
  return null;
}

// ---- the draw plan -------------------------------------------------------------------

function isLive(t: Tile): boolean {
  return t.phase === "running" && (t.frame_url !== null || t.frame_url_v4 !== null);
}

export interface PhonesPlanInput {
  state: ClientState;
  phones: Phones;
  layout: PhoneLayout;
  dpr: number;
  reducedMotion: boolean;
}

/**
 * What the frame drawer gets: one direct stream per phone placed, each filling its own box (the
 * plan has no image, so the drawer does not seat frames by the stream's grid), and a phone that
 * is not running drawn dark. A phone that is not placed (another is opened) streams nothing.
 */
export function phonesPlan(inp: PhonesPlanInput): DrawPlan {
  const { state: s, phones, layout, dpr } = inp;
  const boxOf = new Map(layout.phones.map((p) => [p.id, p.box]));
  const family = s.net.family;
  // phone streams take the direct seat's token as the wall's do (the streamer checks it)
  const admit = s.admission?.direct?.value ?? null;
  let bps = 0;
  const direct: DrawSource[] = [];
  for (const t of phones.tiles) {
    const box = boxOf.get(t.id) ?? null;
    const base = streamUrl(t, family);
    if (box === null || base === null || !isLive(t)) continue;
    const height = streamHeightFor(box.h * dpr, box.w / box.h);
    bps += streamBitrate(t.rect, phones.image_aspect, height);
    direct.push({ id: `d:${t.id}`, url: frameSocketUrl(base, height, "std"), baseUrl: base, height, box, admit });
  }
  const tiles: DrawTile[] = phones.tiles.map((t) => ({
    id: t.id,
    half: t.half,
    box: boxOf.get(t.id) ?? null,
    dark: t.phase !== "running",
    outline: COLORS.fcvm.darkOutline,
    slowFade: false,
  }));
  return {
    mode: "direct",
    direct,
    aggregated: [],
    tiles,
    focus: null,
    zoom: null,
    image: null,
    dpr,
    darkFill: "#06080B",
    estimatedBps: bps,
    poster: null,
    paused: s.ui.paused,
    darkFadeMs: inp.reducedMotion ? REDUCED_MOTION_DARK_FADE_MS : 0,
    suspended: s.ui.suspended,
    posterFull: false,
    gen: 0,
    atT: 0,
  };
}

// ---- the view -----------------------------------------------------------------------

export interface PhoneCard {
  id: string;
  name: string;
  /** Under the phone when apart. */
  label: string;
  box: Box | null;
  bezel: Box | null;
  pad: Pad;
  radius: number;
  bezelOpacity: number;
  labelBox: Box | null;
  dark: boolean;
  ariaLabel: string;
  /** This viewer holds it. */
  mine: boolean;
  /** Someone else holds it. */
  lent: boolean;
}

export type PhoneBorrowView =
  | { kind: "free"; button: { label: string; disabled: boolean }; note: string }
  | { kind: "yours"; session: string; countdown: string; actions: { name: string; label: string; disabled: boolean }[]; giveBack: string }
  | { kind: "lent"; text: string };

export interface PhonePanel {
  id: string;
  title: string;
  state: string;
  rows: { term: string; value: string }[];
  restoreTitle: string;
  restoreTotal: string;
  restoreNote: string;
  /** While it restores: where it is now. */
  restoreNow: string;
  timeline: TimelineStep[];
  borrow: PhoneBorrowView;
  back: string;
}

export interface PhonesInput {
  state: ClientState;
  /** Local Unix ms. */
  now: number;
  dpr: number;
  /** Where the phones may go: the page under its header, measured. */
  stage: Box | null;
  /** Linear progress of the zoom (0 seamless, 1 apart). */
  progress: number;
  narrow: boolean;
  reducedMotion: boolean;
  /** The phone this viewer has opened. */
  openId: string | null;
  canDecode: boolean;
}

export interface PhonesView {
  title: string;
  sentence: string;
  online: boolean;
  /** Why there is nothing to show, when there is nothing. */
  empty: string | null;
  banner: string | null;
  zoom: { label: string; out: boolean; disabled: boolean };
  layout: PhoneLayout | null;
  cards: PhoneCard[];
  panel: PhonePanel | null;
  verify: ReturnType<typeof verifyView>;
  /** A refusal the viewer should read. */
  notice: string | null;
  plan: DrawPlan;
}

const BLANK_PLAN = (s: ClientState, dpr: number): DrawPlan => ({
  mode: "direct",
  direct: [],
  aggregated: [],
  tiles: [],
  focus: null,
  zoom: null,
  image: null,
  dpr,
  darkFill: "#06080B",
  estimatedBps: 0,
  poster: null,
  paused: false,
  darkFadeMs: 0,
  suspended: s.ui.suspended,
  posterFull: false,
  gen: 0,
  atT: 0,
});

export function phonesView(inp: PhonesInput): PhonesView {
  const s = inp.state;
  const phones = s.accepted?.phones ?? null;
  const online = s.connection === "open";
  const srvNow = serverNow(s.clock, inp.now);
  const common = {
    title: PHONES_TITLE,
    sentence: PHONES_SENTENCE,
    online,
    verify: verifyView(s),
    notice: phoneNotice(s.notices),
  };
  const zoomOut = phones?.zoomed_out === true;
  const zoom = { label: zoomOut ? "Zoom in" : "Zoom out", out: zoomOut, disabled: !online || phones === null };
  const banner = !inp.canDecode
    ? "This browser cannot decode H.264 with WebCodecs, so the picture stays still. Chrome, Edge and Safari 16.4 or later can."
    : s.accepted !== null && !online
      ? "The coordinator is offline. The picture still streams straight from the phones; the controls are paused until it is back."
      : null;
  if (phones === null || s.accepted === null) {
    return {
      ...common,
      empty: s.accepted === null ? (s.connection === "closed" ? "Cannot reach the fleet yet. Trying again." : "Connecting to the fleet.") : NO_PHONES,
      banner,
      zoom,
      layout: null,
      cards: [],
      panel: null,
      plan: BLANK_PLAN(s, inp.dpr),
    };
  }
  const open = inp.openId !== null && phones.tiles.some((t) => t.id === inp.openId) ? inp.openId : null;
  const stage = inp.stage;
  // A turned phone draws its screen at the other aspect: the drawer reports each stream's size.
  const aspects: Record<string, number> = {};
  for (const t of phones.tiles) {
    const fs = s.frames?.tiles[`d:${t.id}`];
    if (fs && fs.width > 0 && fs.height > 0 && fs.newestT !== null) aspects[t.id] = Math.round((fs.width / fs.height) * 1000) / 1000;
  }
  const layout = stage
    ? layoutPhones({
        area: open !== null ? openedArea(stage, inp.narrow) : stage,
        imageAspect: phones.image_aspect,
        tiles: phones.tiles,
        progress: inp.progress,
        dpr: inp.dpr,
        open,
        aspects,
      })
    : null;
  const placed = new Map(layout?.phones.map((p) => [p.id, p]) ?? []);
  const mine = (t: Tile) => t.borrowed !== null && sameViewer(t.borrowed.viewer ?? null, s.you);
  const cards: PhoneCard[] = phones.tiles.map((t) => {
    const p = placed.get(t.id);
    const lentToOther = t.borrowed !== null && !mine(t);
    const state = t.phase === "running" ? "streaming" : phaseText(t.phase);
    return {
      id: t.id,
      name: phoneName(t),
      label: phoneLabel(t),
      box: p?.box ?? null,
      bezel: p?.bezel ?? null,
      pad: p?.pad ?? NO_PAD,
      radius: p?.radius ?? 0,
      bezelOpacity: p?.bezelOpacity ?? 0,
      labelBox: p?.label ?? null,
      dark: t.phase !== "running",
      ariaLabel: `${phoneName(t)}, ${state}${mine(t) ? ", yours" : lentToOther ? ", borrowed" : ""}. Open its details.`,
      mine: mine(t),
      lent: lentToOther,
    };
  });
  const openTile = open === null ? null : phones.tiles.find((t) => t.id === open)!;
  return {
    ...common,
    empty: null,
    banner,
    zoom,
    layout,
    cards,
    panel: openTile === null ? null : panelOf(s, phones, openTile, srvNow, online),
    plan: layout ? phonesPlan({ state: s, phones, layout, dpr: inp.dpr, reducedMotion: inp.reducedMotion }) : BLANK_PLAN(s, inp.dpr),
  };
}

function panelOf(s: ClientState, phones: Phones, t: Tile, srvNow: number, online: boolean): PhonePanel {
  const lr = t.last_relaunch;
  const { steps, total } = lr ? timelineOf(lr.phases) : { steps: [], total: 0 };
  const running = t.phase === "running";
  const restoring = !running || (lr !== null && lr.first_frame_at === null);
  const restoredMs = lr !== null && lr.first_frame_at !== null ? lr.first_frame_at - lr.clicked_at : null;
  const restored = restoreText(t);
  const b = t.borrowed;
  const youHold = phones.tiles.find((x) => x.borrowed !== null && sameViewer(x.borrowed.viewer ?? null, s.you)) ?? null;
  let borrow: PhoneBorrowView;
  if (b !== null && sameViewer(b.viewer ?? null, s.you) && b.session !== undefined) {
    const left = Math.max(0, Math.ceil((b.until - srvNow) / 1000));
    borrow = {
      kind: "yours",
      session: b.session,
      countdown: `Back in ${left} s`,
      actions: phones.actions.map((name) => ({ name, label: ACTION_LABELS[name] ?? name.replace(/_/g, " "), disabled: !online || !running })),
      giveBack: "Give it back",
    };
  } else if (b !== null) {
    borrow = { kind: "lent", text: `Borrowed by another viewer until ${fmt.utcClock(b.until)} UTC` };
  } else if (youHold !== null) {
    borrow = { kind: "free", button: { label: "Take over this phone", disabled: true }, note: `You are using ${phoneName(youHold)}. Give it back to take another.` };
  } else if (!running) {
    borrow = { kind: "free", button: { label: "Take over this phone", disabled: true }, note: "It can be borrowed once it has restored." };
  } else {
    borrow = {
      kind: "free",
      button: { label: "Take over this phone", disabled: !online },
      note: `Fixed actions only, for ${fmt.span(phones.claim_secs * 1000)}. ${phones.borrow.lent} of ${phones.borrow.cap} lent now.`,
    };
  }
  const g = t.gpu;
  return {
    id: t.id,
    title: phoneName(t),
    state: running ? (restored && !restored.restoring ? restored.text : "streaming") : restored?.text ?? phaseText(t.phase),
    rows: [
      { term: "GPU", value: g?.name ?? "none yet" },
      { term: "Place", value: t.place ?? "none yet" },
      { term: "VM", value: t.vm ?? "none yet" },
      { term: "Up for", value: lr?.first_frame_at != null && running ? fmt.span(srvNow - lr.first_frame_at) : "--" },
      { term: "Restored in", value: restoredMs === null ? "--" : tenths(restoredMs) },
      ...(t.error ? [{ term: "Error", value: t.error }] : []),
    ],
    restoreTitle: restoring && lr !== null ? "This restore" : "Last restore",
    restoreTotal: restoredMs !== null ? tenths(restoredMs) : restoring && lr ? fmt.raceClock(srvNow - lr.clicked_at) : "--",
    restoreNote: "From the click to the app's first frame, measured by the coordinator.",
    restoreNow: restoring ? `now: ${phaseText(t.phase)}` : "",
    timeline: steps.map((x) => ({ ...x, fraction: total > 0 ? x.fraction : 0 })),
    borrow,
    back: "Back to the phones",
  };
}
