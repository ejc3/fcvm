// Showing every tile at one common t.
//
// Each tile keeps a short buffer of decoded frames, each stamped with its scene
// time t. Renderers share a clock and a frame rate, so their frames land on the
// same t values. On every draw the page picks one T, the frame time the most
// keeping-up tiles hold (the latest, on a tie), and shows each tile's frame at
// exactly T. A tile with no frame at T
// shows its newest frame from before T dimmed (it is behind), or nothing if all
// its frames are after T (it just joined). Nothing is ever drawn from two
// different times side by side undimmed, so there is no torn seam.

/** Frames within this many seconds of each other are the same frame time. */
export const SAME_T_EPS = 0.0005;
/** A tile more than this many seconds behind the newest tile stops holding the others back. */
export const MAX_LAG_S = 0.35;
/** Frames a tile keeps decoded and waiting. */
export const BUFFER_FRAMES = 10;

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
 * dropped from the set; `maxLag` bounds how far one slow tile can hold the image.
 */
export function chooseCommonT(
  buffers: readonly TileBuffer[],
  prevT: number | null,
  maxLag: number = MAX_LAG_S,
  maxT: number = Infinity,
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
  const floor = Math.max(newestMax - maxLag, prevT === null ? -Infinity : prevT - SAME_T_EPS);
  const candidates = live.filter((b) => newest(b) >= floor);

  // The frame time held by the most keeping-up tiles, the latest of those on a
  // tie. Counting over every tile's times keeps the choice independent of tile
  // order, and a slow tile whose frames the others no longer hold loses to them
  // instead of holding them dark.
  let T: number | null = prevT;
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
  let best: { t: number; n: number } | null = null;
  for (const e of counts.values()) if (best === null || e.n > best.n || (e.n === best.n && e.t > best.t)) best = e;
  if (best !== null) T = best.t;
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
