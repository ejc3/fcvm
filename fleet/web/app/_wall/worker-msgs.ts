// Messages between the page and the frame worker. The worker owns the frame
// sockets, the decoders and the canvas; the page owns everything else.

import type { DrawPlan, FrameStats, StreamMode } from "../../core/index.ts";

export interface DrawSettings {
  /** Page background under the image (mosaic: black; an opened tile: the panel colour). */
  background: string;
  /** Dim factor for a tile that fell behind the common t. */
  dimAlpha: number;
  fadeInMs: number;
}

export type ToWorker =
  | { type: "init"; canvas: OffscreenCanvas; width: number; height: number; dpr: number; mainTicks: boolean }
  | { type: "resize"; width: number; height: number; dpr: number }
  | { type: "plan"; plan: DrawPlan; settings: DrawSettings }
  | { type: "watch"; targets: { id: string; tEff: number }[] }
  | { type: "tick" }
  | { type: "capture"; req: number; points: { x: number; y: number }[] }
  /** Start recording each draw (on), or stop and send what was recorded (off). */
  | { type: "trace"; req: number; on: boolean };

export interface CaptureResult {
  req: number;
  /** The common t drawn in the captured frame. */
  T: number | null;
  /** Per tile: what was drawn (sync at T, behind, waiting, dark). */
  shown: Record<string, string>;
  /** RGBA at each requested point. */
  pixels: [number, number, number, number][];
  /** The mode drawn. */
  mode: StreamMode;
}

export interface BenchStats {
  /** Decode latency (decode call to output), ms. */
  decodeMsP50: number;
  decodeMsP95: number;
  decodedPerSec: number;
  /** Frame bytes received over all streams since the worker started. */
  bytesReceived: number;
  drawsPerSec: number;
  tilesSynced: number;
  tilesBehind: number;
  tilesDrawn: number;
}

/** One draw, recorded for a test. */
export interface TraceEntry {
  T: number | null;
  mode: StreamMode;
  /** Regions drawn without a frame. */
  missing: number;
  /** Tiles drawn part way into their dark-out fade. */
  darkIn: number;
  /** The poster was drawn under the streams. */
  poster: boolean;
  /** The picture from before a suspension was shown (held, or under the live one fading in). */
  held: boolean;
  /** The layout generation drawn. */
  gen: number;
}

export type FromWorker =
  | { type: "stats"; stats: FrameStats; bench: BenchStats }
  | { type: "camera-drawn"; id: string; drawnAt: number }
  | { type: "capture"; result: CaptureResult }
  | { type: "error"; message: string }
  /** The worker has no requestAnimationFrame: the page sends it one tick per display frame. */
  | { type: "need-ticks" }
  /** The drawer now shows this mode (a switch completed). */
  | { type: "mode-shown"; mode: StreamMode }
  | { type: "trace"; req: number; entries: TraceEntry[] }
  /** The canvas shows something (the poster or a frame): the page's own poster can go. */
  | { type: "painted" }
  /** The drawer swapped to this layout generation. */
  | { type: "gen-shown"; gen: number };
