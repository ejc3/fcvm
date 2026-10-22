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

// ---- the renderer's camera: positions in km, [x, altitude, z] ----------------

export type Vec3 = [number, number, number];

export interface Pose {
  pos: Vec3;
  look: Vec3;
  fovDeg: number;
}

function vec(v: unknown): Vec3 | null {
  return Array.isArray(v) && v.length === 3 && v.every((x) => typeof x === "number") ? (v as Vec3) : null;
}

/**
 * The camera at t from keys `{t, pos, look, fov_deg}`, interpolated linearly.
 * The renderer follows a Catmull-Rom spline through the same keys, so between
 * keys this is close, at a key or in a hold it is exact. Null when no key has a
 * pose.
 */
export function poseAt(keys: readonly { t: number }[], t: number): Pose | null {
  const ks = keys
    .map((k) => {
      const r = k as Record<string, unknown>;
      const pos = vec(r["pos"]);
      const look = vec(r["look"]);
      const fov = typeof r["fov_deg"] === "number" ? (r["fov_deg"] as number) : null;
      return pos && look && fov !== null ? { t: k.t, pos, look, fov } : null;
    })
    .filter((k): k is { t: number; pos: Vec3; look: Vec3; fov: number } => k !== null)
    .sort((a, b) => a.t - b.t);
  if (ks.length === 0) return null;
  let i = 0;
  while (i < ks.length && ks[i]!.t <= t) i++;
  const a = ks[Math.max(0, i - 1)]!;
  const b = ks[Math.min(ks.length - 1, i)]!;
  const span = b.t - a.t;
  const f = span > 0 ? Math.min(1, Math.max(0, (t - a.t) / span)) : 0;
  const mix = (p: Vec3, q: Vec3): Vec3 => [p[0] + (q[0] - p[0]) * f, p[1] + (q[1] - p[1]) * f, p[2] + (q[2] - p[2]) * f];
  return { pos: mix(a.pos, b.pos), look: mix(a.look, b.look), fovDeg: a.fov + (b.fov - a.fov) * f };
}

function sub(a: Vec3, b: Vec3): Vec3 {
  return [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
}
function cross(a: Vec3, b: Vec3): Vec3 {
  return [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
}
function dot(a: Vec3, b: Vec3): number {
  return a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
}
function norm(a: Vec3): Vec3 {
  const l = Math.hypot(a[0], a[1], a[2]) || 1;
  return [a[0] / l, a[1] / l, a[2] / l];
}

/** The camera's forward, right and up unit vectors (world up is +altitude). */
function basis(p: Pose): { f: Vec3; r: Vec3; u: Vec3 } {
  const f = norm(sub(p.look, p.pos));
  const r = norm(cross(f, [0, 1, 0]));
  const u = cross(r, f);
  return { f, r, u };
}

/** The unit ray through an image point (u, v in 0..1, v down) for a pose. */
export function pixelRay(p: Pose, u: number, v: number, imageAspect: number): Vec3 {
  const { f, r, u: up } = basis(p);
  const th = Math.tan(((p.fovDeg / 2) * Math.PI) / 180);
  const sx = (2 * u - 1) * th * imageAspect;
  const sy = (1 - 2 * v) * th;
  return norm([f[0] + r[0] * sx + up[0] * sy, f[1] + r[1] * sx + up[1] * sy, f[2] + r[2] * sx + up[2] * sy]);
}

/** Where a world point appears on the image, or null behind the camera. */
export function projectPoint(p: Pose, w: Vec3, imageAspect: number): { u: number; v: number } | null {
  const { f, r, u: up } = basis(p);
  const d = sub(w, p.pos);
  const z = dot(d, f);
  if (z <= 1e-9) return null;
  const th = Math.tan(((p.fovDeg / 2) * Math.PI) / 180);
  return { u: (dot(d, r) / z / (th * imageAspect) + 1) / 2, v: (1 - dot(d, up) / z / th) / 2 };
}

/** How far ahead of the camera a dropped light is placed, km. */
export const LIGHT_DISTANCE_KM = 2;

/** The world position for a light dropped on an image point at scene time t. */
export function lightPosition(keys: readonly { t: number }[], t: number, u: number, v: number, imageAspect: number): Vec3 | null {
  const p = poseAt(keys, t);
  if (!p) return null;
  const d = pixelRay(p, u, v, imageAspect);
  const r = (x: number) => Math.round(x * 1e6) / 1e6;
  return [r(p.pos[0] + d[0] * LIGHT_DISTANCE_KM), r(p.pos[1] + d[1] * LIGHT_DISTANCE_KM), r(p.pos[2] + d[2] * LIGHT_DISTANCE_KM)];
}
