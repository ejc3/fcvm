// A render thread: draws one tile's rectangle for one t and returns I420.
import { parentPort } from "node:worker_threads";
import type { TileGeometry } from "../core/layout.ts";
import { frameCtx, renderI420, type PatternScene } from "./pattern.ts";

export interface RenderJob {
  job: number;
  geometry: TileGeometry;
  /** Image rectangles to draw black (an aggregator's tiles that are not running). */
  black: { x: number; y: number; w: number; h: number }[];
  /** The t whose picture is drawn (the stamp can differ when a fault is injected). */
  drawT: number;
  scene: PatternScene;
}

parentPort!.on("message", (j: RenderJob) => {
  const out = renderI420(j.geometry, frameCtx(j.drawT, j.scene), j.black);
  parentPort!.postMessage({ job: j.job, data: out }, [out.buffer as ArrayBuffer]);
});
