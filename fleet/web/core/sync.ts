// Showing every tile at one common t.
//
// Each tile keeps a short buffer of decoded frames, each stamped with its scene
// time t. Renderers share a clock and a frame rate, so their frames land on the
// same t values. On every draw the page picks one T and shows each tile's frame at
// exactly T. T is the latest frame time every keeping-up tile holds, so the image
// waits for the slowest of them; when none is held by all, it holds where it is
// while a keeping-up tile catches up to it, and otherwise it is the frame time the
// most keeping-up tiles hold (the latest, on a tie). A tile with no frame at T
// shows its newest frame from before T dimmed (it is behind), or nothing if all
// its frames are after T (it just joined). Nothing is ever drawn from two
// different times side by side undimmed, so there is no torn seam.
//
// A keeping-up tile is one at most MAX_LAG_S behind the newest frame, or, if it was
// in step on the previous draw, MAX_LAG_S + HOLD_EXTRA_S: a tile whose decode
// stalls for a moment holds the image instead of flipping between dimmed and in
// step (the staging wall's fcvm half strobed that way, its frames 0.13 s later than
// the EC2 half's and one decode in a hundred taking twice as long).

/** Frames within this many seconds of each other are the same frame time. */
export const SAME_T_EPS = 0.0005;
/** A tile more than this many seconds behind the newest tile stops holding the others back. */
export const MAX_LAG_S = 0.35;
/** How much further a tile that was in step may fall behind before it stops holding the others. */
export const HOLD_EXTRA_S = 0.2;
/**
 * Frames a tile keeps decoded and waiting: enough at 30 fps for the tiles that keep up
 * to still hold the frame a tile MAX_LAG_S + HOLD_EXTRA_S behind them is showing.
 */
export const BUFFER_FRAMES = 18;
/**
 * A stream's frames only move forward in scene time. A frame this many seconds older than
 * the newest one a tile holds means its renderer's clock moved back: it drew against its
 * default scene (epoch 0, so scene times near 1.8e9 s) until its scene arrived, or it was
 * given a scene with a later epoch.
 */
export const CLOCK_BACK_S = 5;

/**
 * Whether a frame at `t` is on an earlier clock than the frames held (`ts`, ascending): the
 * held frames are then dropped. Kept, they stay the newest the tile holds and every frame on
 * the new clock is evicted as the oldest, so the tile never shows a frame again.
 */
export function clockMovedBack(ts: readonly number[], t: number): boolean {
  return ts.length > 0 && t < ts[ts.length - 1]! - CLOCK_BACK_S;
}

/**
 * Add a decoded frame to a tile's held frames (ascending t; a repeated t replaces the old
 * frame) and keep at most BUFFER_FRAMES, letting go of the oldest that `keeps` does not
 * pin. Returns the frames let go, for the caller to close.
 */
export function holdFrame<H extends { t: number }>(frames: H[], f: H, keeps: (h: H) => boolean): H[] {
  const gone = clockMovedBack(frames.map((h) => h.t), f.t) ? frames.splice(0) : [];
  const at = frames.findIndex((h) => h.t >= f.t - SAME_T_EPS);
  if (at >= 0 && Math.abs(frames[at]!.t - f.t) <= SAME_T_EPS) {
    gone.push(frames[at]!);
    frames[at] = f;
  } else if (at >= 0) {
    frames.splice(at, 0, f);
  } else {
    frames.push(f);
  }
  while (frames.length > BUFFER_FRAMES) {
    const i = frames.findIndex((h) => !keeps(h));
    if (i < 0) break;
    gone.push(frames.splice(i, 1)[0]!);
  }
  return gone;
}

export interface TileBuffer {
  id: string;
  /** Scene times of the frames held, ascending. */
  ts: readonly number[];
}

export type TileShow =
  | { kind: "sync"; t: number }
  | { kind: "behind"; t: number }
  | { kind: "waiting" }
  | { kind: "empty" };

export interface SyncChoice {
  /** The common time drawn, or null when no tile has a frame. */
  T: number | null;
  show: Map<string, TileShow>;
}

function has(ts: readonly number[], t: number): boolean {
  for (const x of ts) if (Math.abs(x - t) <= SAME_T_EPS) return true;
  return false;
}

/**
 * Choose the common time. `prevT` keeps T from going backwards when a tile is
 * dropped from the set; `maxLag` bounds how far one slow tile can hold the image,
 * and a tile in `inStep` (in step on the previous draw) may lag HOLD_EXTRA_S more.
 */
export function chooseCommonT(
  buffers: readonly TileBuffer[],
  prevT: number | null,
  maxLag: number = MAX_LAG_S,
  maxT: number = Infinity,
  inStep: ReadonlySet<string> = new Set(),
): SyncChoice {
  const show = new Map<string, TileShow>();
  // Frames after maxT are not chosen yet (a mode switch holds the image back
  // to where the incoming streams are); they stay in the buffers.
  const capped = maxT === Infinity ? buffers : buffers.map((b) => ({ id: b.id, ts: b.ts.filter((t) => t <= maxT + SAME_T_EPS) }));
  const live = capped.filter((b) => b.ts.length > 0);
  for (const b of capped) if (b.ts.length === 0) show.set(b.id, { kind: buffers.find((x) => x.id === b.id)!.ts.length > 0 ? "waiting" : "empty" });
  if (live.length === 0) return { T: prevT, show };

  const newest = (b: TileBuffer) => b.ts[b.ts.length - 1]!;
  const newestMax = Math.max(...live.map(newest));
  const notBefore = prevT === null ? -Infinity : prevT - SAME_T_EPS;
  const lagOf = (b: TileBuffer) => maxLag + (inStep.has(b.id) ? HOLD_EXTRA_S : 0);
  const candidates = live.filter((b) => newest(b) >= newestMax - lagOf(b));
  // Frame times before this are not chosen: too old for any keeping-up tile, or before prevT.
  const floor = Math.max(newestMax - maxLag - HOLD_EXTRA_S, notBefore);

  // How many keeping-up tiles hold each frame time. Counting over every tile's
  // times keeps the choice independent of tile order.
  const counts = new Map<number, { t: number; n: number }>();
  for (const c of candidates) {
    for (const t of c.ts) {
      if (t < floor) continue;
      const key = Math.round(t / SAME_T_EPS / 2);
      const e = counts.get(key);
      if (e) e.n++;
      else counts.set(key, { t, n: 1 });
    }
  }
  let T: number | null = prevT;
  let all: number | null = null;
  for (const e of counts.values()) if (e.n === candidates.length && (all === null || e.t > all)) all = e.t;
  const catchingUp = prevT !== null && candidates.some((c) => newest(c) < prevT - SAME_T_EPS);
  const othersHoldPrev = prevT !== null && candidates.every((c) => newest(c) < prevT - SAME_T_EPS || has(c.ts, prevT));
  if (all !== null) {
    // The latest frame time every keeping-up tile holds: the image waits for the slowest.
    T = all;
  } else if (catchingUp && othersHoldPrev) {
    // A keeping-up tile is behind the picture on screen and the others still hold it:
    // hold the picture until that tile reaches it, rather than leaving it dimmed.
    T = prevT;
  } else {
    // No frame time all of them hold: the one the most hold, the latest of those on a
    // tie, so a slow tile whose frames the others no longer hold loses to them
    // instead of holding them dark.
    let best: { t: number; n: number } | null = null;
    for (const e of counts.values()) if (best === null || e.n > best.n || (e.n === best.n && e.t > best.t)) best = e;
    if (best !== null) T = best.t;
  }
  if (prevT !== null && T !== null && T < prevT) T = prevT;

  for (const b of live) {
    if (T === null) {
      show.set(b.id, { kind: "waiting" });
      continue;
    }
    if (has(b.ts, T)) {
      show.set(b.id, { kind: "sync", t: T });
      continue;
    }
    // Its newest frame from before T, if it has one: shown, but dimmed.
    let before: number | null = null;
    for (const t of b.ts) if (t < T - SAME_T_EPS) before = t;
    show.set(b.id, before === null ? { kind: "waiting" } : { kind: "behind", t: before });
  }
  return { T, show };
}

/** Frames older than this t can be released: nothing will draw them again. */
export function releaseBefore(choice: SyncChoice, id: string): number | null {
  const s = choice.show.get(id);
  if (!s) return null;
  if (s.kind === "sync" || s.kind === "behind") return s.t - SAME_T_EPS;
  return null;
}

/**
 * A frozen choice, given a frame for each tile whose entry names none (empty or
 * waiting) and whose stream now holds one: a stream that closed and reopened under a
 * paused picture. The tile takes its newest frame at or before T, dimmed unless it
 * is at T. A reopened stream seldom holds one that old, so otherwise it takes its
 * oldest frame after T, dimmed. T and every other entry stay as they were: choosing
 * the whole wall again would move a picture the viewer paused. Null when nothing
 * changes. A tile the choice does not name is not added.
 */
export function acquireFrozen(choice: SyncChoice, buffers: readonly TileBuffer[]): SyncChoice | null {
  if (choice.T === null) return null;
  let show: Map<string, TileShow> | null = null;
  for (const b of buffers) {
    const was = choice.show.get(b.id);
    if (!was || (was.kind !== "empty" && was.kind !== "waiting") || b.ts.length === 0) continue;
    let at: number | null = null;
    for (const t of b.ts) if (t <= choice.T + SAME_T_EPS) at = t;
    show ??= new Map(choice.show);
    if (at !== null) show.set(b.id, Math.abs(at - choice.T) <= SAME_T_EPS ? { kind: "sync", t: choice.T } : { kind: "behind", t: at });
    else show.set(b.id, { kind: "behind", t: b.ts[0]! });
  }
  return show === null ? null : { T: choice.T, show };
}
