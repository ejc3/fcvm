// Scene helpers shared by the page and the mock renderer: camera interpolation,
// scene time, and turning a drag into a camera intent.

/** Scene seconds for a coordinator time. */
export function sceneT(epochUnixMs: number, serverUnixMs: number): number {
  return (serverUnixMs - epochUnixMs) / 1000;
}

/** Coordinator Unix ms for a scene t. */
export function sceneUnixMs(epochUnixMs: number, t: number): number {
  return epochUnixMs + t * 1000;
}

/**
 * The camera at t: every numeric field linearly interpolated between the keys on
 * either side, held at the first key before it and the last key after it.
 */
export function cameraAt(keys: readonly { t: number }[], t: number): Record<string, number> {
  const out: Record<string, number> = {};
  if (keys.length === 0) return out;
  const sorted = [...keys].sort((a, b) => a.t - b.t);
  let i = 0;
  while (i < sorted.length && sorted[i]!.t <= t) i++;
  const a = sorted[Math.max(0, i - 1)]!;
  const b = sorted[Math.min(sorted.length - 1, i)]!;
  const span = b.t - a.t;
  const f = span > 0 ? Math.min(1, Math.max(0, (t - a.t) / span)) : 0;
  const bv = b as Record<string, unknown>;
  for (const [k, va] of Object.entries(a)) {
    if (k === "t" || typeof va !== "number") continue;
    const vb = bv[k];
    out[k] = typeof vb === "number" ? va + (vb - va) * f : va;
  }
  return out;
}

/** Drag deltas gathered between sends: dx, dy in fractions of the image, dive in world units. */
export interface DragAccumulator {
  dx: number;
  dy: number;
  dive: number;
  lastSentAt: number | null;
}

/** A drag across the whole image's width turns the camera this far. */
export const YAW_DEG_PER_IMAGE = 90;
/** A drag down the whole image's height tilts it this far. */
export const PITCH_DEG_PER_IMAGE = 50;
/** One wheel notch (deltaY 100) dives this many world units. */
export const DIVE_PER_WHEEL_100 = 1;

/** Camera intents go out at most this often while a drag runs. */
export const DRAG_SEND_INTERVAL_MS = 50;

export function emptyDrag(): DragAccumulator {
  return { dx: 0, dy: 0, dive: 0, lastSentAt: null };
}

export function addDrag(acc: DragAccumulator, dx: number, dy: number, dive = 0): DragAccumulator {
  return { ...acc, dx: acc.dx + dx, dy: acc.dy + dy, dive: acc.dive + dive };
}

/** A wheel's deltaY as dive units: scrolling up dives in. */
export function wheelDive(deltaY: number): number {
  return (-deltaY / 100) * DIVE_PER_WHEEL_100;
}

export function dragIsEmpty(acc: DragAccumulator): boolean {
  return Math.abs(acc.dx) < 1e-6 && Math.abs(acc.dy) < 1e-6 && Math.abs(acc.dive) < 1e-6;
}

/**
 * Whether to send now, and the camera intent's args (the coordinator's
 * `{drag: {yaw_deg, pitch_deg, dive}}`). A send resets the deltas. The drag
 * grabs the world: dragging right turns the camera left (negative yaw), dragging
 * down tilts it up (positive pitch).
 */
export function takeDrag(
  acc: DragAccumulator,
  now: number,
  force = false,
): { acc: DragAccumulator; args: Record<string, unknown> | null } {
  if (dragIsEmpty(acc)) return { acc, args: null };
  if (!force && acc.lastSentAt !== null && now - acc.lastSentAt < DRAG_SEND_INTERVAL_MS) return { acc, args: null };
  const args = {
    drag: {
      yaw_deg: round6(-acc.dx * YAW_DEG_PER_IMAGE),
      pitch_deg: round6(acc.dy * PITCH_DEG_PER_IMAGE),
      dive: round6(acc.dive),
    },
  };
  return { acc: { dx: 0, dy: 0, dive: 0, lastSentAt: now }, args };
}

function round6(x: number): number {
  return Math.round(x * 1e6) / 1e6;
}

/** Pointer travel under which a press counts as a click, not a drag (CSS px). */
export const CLICK_SLOP_PX = 5;
/** A touch held this long without moving shows the pressed tile's seams. */
export const LONG_PRESS_MS = 450;

/** Colours of dropped lights, linear RGB in 0..1, taken in turn. */
export const LIGHT_COLORS: readonly [number, number, number][] = [
  [1, 0.8, 0.45],
  [0.35, 0.8, 1],
  [1, 0.42, 0.7],
  [0.5, 1, 0.45],
  [1, 0.62, 0.2],
];
