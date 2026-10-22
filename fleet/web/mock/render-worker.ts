// A render thread: draws one tile's rectangle for one t and returns I420.
import { parentPort } from "node:worker_threads";
import { frameCtx, renderI420, type PatternScene, type TileRect } from "./pattern.ts";

export interface RenderJob {
  job: number;
  rect: TileRect;
  width: number;
  height: number;
  /** The t whose picture is drawn (the stamp can differ when a fault is injected). */
  drawT: number;
  scene: PatternScene;
}

parentPort!.on("message", (j: RenderJob) => {
  const out = renderI420(j.rect, j.width, j.height, frameCtx(j.drawT, j.scene));
  parentPort!.postMessage({ job: j.job, data: out }, [out.buffer as ArrayBuffer]);
});
