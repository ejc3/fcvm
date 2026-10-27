// Where every tile goes on the screen, and what height to ask its renderer for.
//
// The whole image has the aspect the coordinator names (image_aspect). It is fitted inside the area the page gives
// it (contain, centred), and each tile's rectangle of the image becomes a screen
// rectangle. Edges are snapped to device pixels from the image's own edges, so two
// tiles that share an edge in the image share it on screen: no gap, no overlap.

import { DEFAULT_IMAGE_ASPECT, type Rect, type Tile } from "./protocol.ts";

/** Heights a page asks a renderer for. A renderer encodes few distinct heights. */
export const STREAM_HEIGHTS = [72, 90, 108, 144, 180, 216, 270, 360, 432, 540, 720, 1080] as const;

export interface Box {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** Fit a rectangle of `aspect` inside `area`, centred. */
export function fitContain(area: Box, aspect: number): Box {
  if (area.w <= 0 || area.h <= 0) return { x: area.x, y: area.y, w: 0, h: 0 };
  let w = area.w;
  let h = w / aspect;
  if (h > area.h) {
    h = area.h;
    w = h * aspect;
  }
  return { x: area.x + (area.w - w) / 2, y: area.y + (area.h - h) / 2, w, h };
}

/** Snap a CSS-pixel coordinate to the nearest device pixel. */
export function snap(v: number, dpr: number): number {
  return Math.round(v * dpr) / dpr;
}

/**
 * An edge as a fraction of the image, rounded so that one tile's `x + w` and its
 * neighbour's `x` (equal in exact arithmetic, a few ulps apart in floating
 * point) snap to the same device pixel.
 */
function edge(f: number): number {
  return Math.round(f * 1e9) / 1e9;
}

/** The screen box of an image rectangle, edges snapped to device pixels. */
export function rectOnScreen(image: Box, r: Rect, dpr: number): Box {
  const x0 = snap(image.x + edge(r.x) * image.w, dpr);
  const y0 = snap(image.y + edge(r.y) * image.h, dpr);
  const x1 = snap(image.x + edge(r.x + r.w) * image.w, dpr);
  const y1 = snap(image.y + edge(r.y + r.h) * image.h, dpr);
  return { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
}

/**
 * The most pixels one tile's stream may ask for. A renderer draws at the largest height any
 * subscriber asks, within the pixel-samples per frame its GPU fits at 30 fps
 * (fleet/renderer/README.md, "Grid-wide budget": 0.4 to 0.5 M for a terrain tile on a T4),
 * so a larger tile drops under one sample per pixel while the camera moves. One viewer on a
 * big screen must not ask for more and blur the picture every other viewer sees.
 */
export const MAX_TILE_PX = 0.5e6;

/**
 * The smallest stream height that covers `devicePixels`, capped at the largest. With
 * `tileAspect` (the tile's width over its height on screen) it is also capped so the tile is at
 * most `maxPx` (MAX_TILE_PX unless given): h <= sqrt(maxPx / tileAspect), snapped down to a stream height.
 */
export function streamHeightFor(devicePixels: number, tileAspect?: number, maxPx: number = MAX_TILE_PX): number {
  let h: number = STREAM_HEIGHTS[STREAM_HEIGHTS.length - 1]!;
  for (const x of STREAM_HEIGHTS) {
    if (x >= devicePixels - 0.5) {
      h = x;
      break;
    }
  }
  if (tileAspect === undefined || !(tileAspect > 0)) return h;
  const cap = Math.sqrt(maxPx / tileAspect);
  if (h <= cap) return h;
  let best: number = STREAM_HEIGHTS[0];
  for (const x of STREAM_HEIGHTS) if (x <= cap) best = x;
  return best;
}

/** A tile's width over its height, from its rectangle of an image of `imageAspect`. */
export function tileAspectOf(rect: Rect, imageAspect: number): number {
  return (rect.w * imageAspect) / rect.h;
}

export interface PlacedTile {
  id: string;
  box: Box;
  /** The height, in device pixels, the page asks this tile's renderer for. */
  streamHeight: number;
}

export interface MosaicLayout {
  image: Box;
  tiles: PlacedTile[];
  /** x of the line between the halves, in CSS pixels, or null when one side has no tiles. */
  halfLineX: number | null;
  /** Where the fcvm side ends, as a fraction of the image width (0: no fcvm tiles, 1: no EC2 tiles). */
  split: number;
}

/**
 * Where the fcvm side ends: each side's width follows the GPUs it has, so the
 * line sits at the right edge of the fcvm tiles (0 with none, 1 with no EC2 tiles).
 */
export function splitOf(tiles: readonly Tile[]): number {
  const fcvm = tiles.filter((t) => t.half === "fcvm");
  const ec2 = tiles.filter((t) => t.half === "ec2");
  // No GPU on either side (asleep): nothing is placed, and the halves stay even.
  if (fcvm.length === 0 && ec2.length === 0) return 0.5;
  if (fcvm.length === 0) return 0;
  if (ec2.length === 0) return 1;
  return Math.max(...fcvm.map((t) => t.rect.x + t.rect.w));
}

export function layoutMosaic(area: Box, tiles: readonly Tile[], dpr: number, aspect: number = DEFAULT_IMAGE_ASPECT): MosaicLayout {
  return layoutImage(fitContain(area, aspect), tiles, dpr, aspect);
}

/** Every tile placed in an image already sized and positioned (the wall's, or a zoomed one larger than the screen). */
export function layoutImage(image: Box, tiles: readonly Tile[], dpr: number, aspect: number = DEFAULT_IMAGE_ASPECT): MosaicLayout {
  const split = splitOf(tiles);
  const snapped: Box = {
    x: snap(image.x, dpr),
    y: snap(image.y, dpr),
    w: snap(image.x + image.w, dpr) - snap(image.x, dpr),
    h: snap(image.y + image.h, dpr) - snap(image.y, dpr),
  };
  return {
    image: snapped,
    tiles: tiles.map((t) => {
      const box = rectOnScreen(image, t.rect, dpr);
      return { id: t.id, box, streamHeight: streamHeightFor(box.h * dpr, tileAspectOf(t.rect, aspect)) };
    }),
    halfLineX: split > 0 && split < 1 ? snap(image.x + image.w * split, dpr) : null,
    split,
  };
}

// ---- one tile opened: the image zoomed until that tile fills the stage ------------------

/**
 * The most pixels an opened tile's stream may ask for. The renderer README ("Measured", round 7)
 * runs the detail view, one wall tile at 1080 x 1080, at 30 fps on a T4: it draws a quarter of
 * the pixels each frame (checker stride 4) and accumulates the rest. So an opened tile asks up to
 * that many pixels: 1080 for a square tile, 720 for a 16:9 one (1280 x 720, the whole-image
 * 720p row of the same table, also 30 fps).
 */
export const DETAIL_TILE_PX = 1080 * 1080;

/** Room an opened tile leaves around it, in CSS pixels. */
export interface ZoomInsets {
  top: number;
  right: number;
  bottom: number;
  left: number;
  /** Width of a side sheet over the right edge, which the tile moves clear of when it can without shrinking. */
  sheet: number;
}

/**
 * The box an opened tile fills: its own aspect, as large as fits in the area less the insets.
 * It moves left to clear a side sheet when there is room, and is never made smaller for one.
 */
export function zoomTarget(area: Box, rect: Rect, imageAspect: number, insets: ZoomInsets): Box {
  const inner: Box = {
    x: area.x + insets.left,
    y: area.y + insets.top,
    w: Math.max(0, area.w - insets.left - insets.right),
    h: Math.max(0, area.h - insets.top - insets.bottom),
  };
  const fit = fitContain(inner, tileAspectOf(rect, imageAspect));
  const clear = area.x + area.w - insets.sheet - fit.w;
  return { ...fit, x: Math.max(inner.x, Math.min(fit.x, clear)) };
}

/** Whether an opened tile, at full size, lies clear of the side sheet: then the sheet starts open. */
export function sheetClears(area: Box, rect: Rect, imageAspect: number, insets: ZoomInsets): boolean {
  const z = zoomTarget(area, rect, imageAspect, insets);
  return z.x + z.w <= area.x + area.w - insets.sheet + 0.5;
}

/** The image box that puts the tile with rectangle `rect` exactly on `target`. */
export function imageBehind(target: Box, rect: Rect): Box {
  const w = target.w / rect.w;
  const h = target.h / rect.h;
  return { x: target.x - rect.x * w, y: target.y - rect.y * h, w, h };
}

export function lerpBox(a: Box, b: Box, p: number): Box {
  return { x: a.x + (b.x - a.x) * p, y: a.y + (b.y - a.y) * p, w: a.w + (b.w - a.w) * p, h: a.h + (b.h - a.h) * p };
}

/** How long the zoom between the wall and a tile (or from one tile to another) takes. */
export const ZOOM_MS = 420;

/** Cubic ease in and out of the zoom's linear progress. */
export function easeZoom(p: number): number {
  const x = Math.min(1, Math.max(0, p));
  return x < 0.5 ? 4 * x * x * x : 1 - (-2 * x + 2) ** 3 / 2;
}

/** The zoom as the page animates it: from the wall or a tile, to the wall or a tile, `progress` of the way (linear time). */
export interface ZoomStep {
  from: string | null;
  to: string | null;
  progress: number;
}

export interface ZoomLayout {
  /** The image and every tile at this step of the zoom; most of the image lies off screen. */
  layout: MosaicLayout;
  /** The tile zoomed into (while closing, the one zoomed out of) and its box now. */
  focus: PlacedTile | null;
  /** How far the picture around it has dimmed, 0 (the wall) to 1 (fully open). */
  dim: number;
}

/**
 * The image and tiles at a step of the zoom. The image box moves (eased) from where the `from`
 * side puts it to where the `to` side does: the wall's image box for null, else the box that
 * puts that tile on its zoomTarget in `area`. Null when neither side is a tile of `tiles`.
 */
export function layoutZoom(
  wall: Box,
  area: Box,
  tiles: readonly Tile[],
  step: ZoomStep,
  dpr: number,
  aspect: number,
  insets: ZoomInsets,
): ZoomLayout | null {
  const from = step.from === null ? null : tiles.find((t) => t.id === step.from) ?? null;
  const to = step.to === null ? null : tiles.find((t) => t.id === step.to) ?? null;
  if (!from && !to) return null;
  const imageFor = (t: Tile | null) => (t ? imageBehind(zoomTarget(area, t.rect, aspect, insets), t.rect) : wall);
  const e = easeZoom(step.progress);
  const layout = layoutImage(lerpBox(imageFor(from), imageFor(to), e), tiles, dpr, aspect);
  const opened = to ?? from!;
  const focus = layout.tiles.find((p) => p.id === opened.id) ?? null;
  const dim = (from ? 1 : 0) + ((to ? 1 : 0) - (from ? 1 : 0)) * e;
  return { layout, focus, dim };
}

/** Whether two boxes overlap by more than a hairline. */
export function boxesMeet(a: Box, b: Box): boolean {
  return a.x < b.x + b.w - 0.5 && b.x < a.x + a.w - 0.5 && a.y < b.y + b.h - 0.5 && b.y < a.y + a.h - 0.5;
}

/** NVENC's smallest H.264 frame on a T4: nothing narrower than 145 or shorter than 49 px. */
export const MIN_STREAM_W = 146;
export const MIN_STREAM_H = 50;

export interface TileGeometry {
  width: number;
  height: number;
  fullWidth: number;
  fullHeight: number;
  x0: number;
  y0: number;
}

/**
 * The renderer's tile geometry (fleet/PROTOCOL.md, "Tile geometry"): the
 * whole-image pixel grid at a requested height and this tile's part of it. A
 * height whose tile would be under the encoder's minimum is raised to the
 * smallest even height that fits, so the stream can be larger than asked.
 */
export function tileGeometry(rect: Rect, imageAspect: number, requested: number): TileGeometry {
  const at = (H: number): TileGeometry => {
    const fullHeight = Math.round(H / rect.h);
    const fullWidth = Math.round(fullHeight * imageAspect);
    const w = Math.round(rect.w * fullWidth);
    return { width: w + (w % 2), height: H, fullWidth, fullHeight, x0: Math.round(rect.x * fullWidth), y0: Math.round(rect.y * fullHeight) };
  };
  let H = Math.min(2160, Math.max(16, Math.round(requested))) & ~1;
  let g = at(H);
  while ((g.width < MIN_STREAM_W || g.height < MIN_STREAM_H) && H < 2160) {
    H += 2;
    g = at(H);
  }
  return g;
}

/** A box in device pixels: x, y, width, height. */
export type DeviceBox = [number, number, number, number];

export function deviceBox(b: Box, dpr: number): DeviceBox {
  const x = Math.round(b.x * dpr);
  const y = Math.round(b.y * dpr);
  return [x, y, Math.round((b.x + b.w) * dpr) - x, Math.round((b.y + b.h) * dpr) - y];
}

/** A stream's place in the whole image's pixel grid, from its header. */
export interface StreamGrid {
  fullWidth: number;
  fullHeight: number;
  x0: number;
  y0: number;
}

/** The stream's inner rectangle (w x h pixels of the whole-image grid) placed in the image, in device pixels. */
export function gridBox(image: Box, g: StreamGrid, w: number, h: number, dpr: number): DeviceBox {
  const x0 = Math.round((image.x + (g.x0 / g.fullWidth) * image.w) * dpr);
  const y0 = Math.round((image.y + (g.y0 / g.fullHeight) * image.h) * dpr);
  const x1 = Math.round((image.x + ((g.x0 + w) / g.fullWidth) * image.w) * dpr);
  const y1 = Math.round((image.y + ((g.y0 + h) / g.fullHeight) * image.h) * dpr);
  return [x0, y0, x1 - x0, y1 - y0];
}

/**
 * Whether a frame whose inner rectangle is `inner` (pixels of the whole-image grid
 * `g`) was rendered for the place `box` has in `image`: the rectangle it draws at
 * is the planned one, give or take the renderer's rounding.
 */
export function frameFitsBox(image: Box, g: StreamGrid, inner: { sw: number; sh: number }, box: Box, dpr: number): boolean {
  const [x, y, w, h] = gridBox(image, g, inner.sw, inner.sh, dpr);
  const [bx, by, bw, bh] = deviceBox(box, dpr);
  // The renderer's rounding (an even width, a minimum size) moves an edge by up to a pixel of its
  // own grid, and a width or height compares two edges: two grid pixels, which are
  // image.w * dpr / fullWidth device pixels wide. Never under the screen-space floor.
  const tolX = Math.max(2, Math.round(dpr * 2), Math.ceil((2 * image.w * dpr) / g.fullWidth));
  const tolY = Math.max(2, Math.round(dpr * 2), Math.ceil((2 * image.h * dpr) / g.fullHeight));
  return Math.abs(x - bx) <= tolX && Math.abs(y - by) <= tolY && Math.abs(w - bw) <= tolX && Math.abs(h - bh) <= tolY;
}

/** A point on screen in image fractions, or null when it is outside the image. */
export function screenToImage(image: Box, px: number, py: number): { u: number; v: number } | null {
  if (image.w <= 0 || image.h <= 0) return null;
  const u = (px - image.x) / image.w;
  const v = (py - image.y) / image.h;
  if (u < 0 || u >= 1 || v < 0 || v >= 1) return null;
  return { u, v };
}

/** The tile whose rectangle holds an image point. */
export function tileAt(tiles: readonly Tile[], u: number, v: number): Tile | null {
  for (const t of tiles) {
    const r = t.rect;
    if (u >= r.x && u < r.x + r.w && v >= r.y && v < r.y + r.h) return t;
  }
  return null;
}

export type Direction = "left" | "right" | "up" | "down";

/**
 * The tile next to `from` in a direction: the nearest tile whose centre lies on
 * that side and overlaps it across the direction of travel. Null at the edge.
 */
export function neighbour(tiles: readonly Tile[], from: Tile, dir: Direction): Tile | null {
  const c = { x: from.rect.x + from.rect.w / 2, y: from.rect.y + from.rect.h / 2 };
  let best: Tile | null = null;
  let bestD = Infinity;
  for (const t of tiles) {
    if (t.id === from.id) continue;
    const tc = { x: t.rect.x + t.rect.w / 2, y: t.rect.y + t.rect.h / 2 };
    const dx = tc.x - c.x;
    const dy = tc.y - c.y;
    const along = dir === "left" ? -dx : dir === "right" ? dx : dir === "up" ? -dy : dy;
    if (along <= 1e-9) continue;
    const overlap =
      dir === "left" || dir === "right"
        ? Math.min(t.rect.y + t.rect.h, from.rect.y + from.rect.h) - Math.max(t.rect.y, from.rect.y)
        : Math.min(t.rect.x + t.rect.w, from.rect.x + from.rect.w) - Math.max(t.rect.x, from.rect.x);
    if (overlap <= 1e-9) continue;
    const across = dir === "left" || dir === "right" ? Math.abs(dy) : Math.abs(dx);
    const d = along + across * 2;
    if (d < bestD) {
      bestD = d;
      best = t;
    }
  }
  return best;
}

/** Tiles in reading order: top to bottom, then left to right. */
export function readingOrder(tiles: readonly Tile[]): Tile[] {
  return [...tiles].sort((a, b) => a.rect.y - b.rect.y || a.rect.x - b.rect.x);
}

export interface Segment {
  x0: number;
  y0: number;
  x1: number;
  y1: number;
}

/**
 * Every internal tile boundary as screen segments, each once, placed on device
 * pixel centres so a 1-device-pixel line is crisp.
 */
export function seamSegments(placed: readonly PlacedTile[], image: Box, dpr: number): Segment[] {
  const half = 0.5 / dpr;
  const seen = new Set<string>();
  const out: Segment[] = [];
  const add = (s: Segment) => {
    const key = `${s.x0.toFixed(3)},${s.y0.toFixed(3)},${s.x1.toFixed(3)},${s.y1.toFixed(3)}`;
    if (seen.has(key)) return;
    seen.add(key);
    out.push(s);
  };
  const right = image.x + image.w;
  const bottom = image.y + image.h;
  for (const p of placed) {
    const b = p.box;
    // Right and bottom edges of each tile, unless they are the image's own edge.
    if (b.x + b.w < right - half) add({ x0: b.x + b.w - half, y0: b.y, x1: b.x + b.w - half, y1: b.y + b.h });
    if (b.y + b.h < bottom - half) add({ x0: b.x, y0: b.y + b.h - half, x1: b.x + b.w, y1: b.y + b.h - half });
  }
  return out;
}

/** Pixels of neighbouring image a stream carries on each inner side (the renderer's MARGIN). */
export const STREAM_MARGIN = 16;

export interface Delivered {
  /** The delivered picture, overlap included. */
  width: number;
  height: number;
  crop: { left: number; top: number; right: number; bottom: number };
}

/**
 * The renderer's delivered picture for a tile: the tile plus up to MARGIN pixels
 * on every side that lies inside the image, none past the image's own edges, one
 * side a pixel short where that keeps the size even.
 */
export function deliveredGeometry(g: TileGeometry, margin: number = STREAM_MARGIN): Delivered {
  let left = Math.min(margin, g.x0);
  let right = Math.min(margin, Math.max(0, g.fullWidth - (g.x0 + g.width)));
  let top = Math.min(margin, g.y0);
  let bottom = Math.min(margin, Math.max(0, g.fullHeight - (g.y0 + g.height)));
  if ((g.width + left + right) % 2 === 1) {
    if (right > 0) right--;
    else left--;
  }
  if ((g.height + top + bottom) % 2 === 1) {
    if (bottom > 0) bottom--;
    else top--;
  }
  return { width: g.width + left + right, height: g.height + top + bottom, crop: { left, top, right, bottom } };
}
