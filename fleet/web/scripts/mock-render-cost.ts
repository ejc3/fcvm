// What the mock renderer costs to draw the whole image once, and how many cores it needs
// at a frame rate: node scripts/mock-render-cost.ts [width] [height] [fps]
// (the image is 16:9 inside a window of that size, 4x4 tiles, each with its overlap).
import { deliveredGeometry, fitContain, streamHeightFor, tileGeometry } from "../core/layout.ts";
import { frameCtx, renderI420, type PatternScene } from "../mock/pattern.ts";

const [w, h, fps] = [Number(process.argv[2] ?? 960), Number(process.argv[3] ?? 600), Number(process.argv[4] ?? 10)];
const image = fitContain({ x: 0, y: 0, w, h }, 16 / 9);
const height = streamHeightFor(image.h / 4);
const geos = Array.from({ length: 16 }, (_, i) => {
  const t = tileGeometry({ x: (i % 4) / 4, y: Math.floor(i / 4) / 4, w: 0.25, h: 0.25 }, 16 / 9, height);
  const d = deliveredGeometry(t);
  return { ...t, width: d.width, height: d.height, x0: t.x0 - d.crop.left, y0: t.y0 - d.crop.top };
});
const scene: PatternScene = { epoch_unix_ms: 0, image_aspect: 16 / 9, camera: [], time_of_day: 0.42, words: [], lights: [] };
const px = geos.reduce((a, g) => a + g.width * g.height, 0);
for (let i = 0; i < 3; i++) for (const g of geos) renderI420(g, frameCtx(i / fps, scene));
const t0 = performance.now();
let n = 0;
while (performance.now() - t0 < 3000) {
  for (const g of geos) renderI420(g, frameCtx(n / fps, scene));
  n++;
}
const ms = (performance.now() - t0) / n;
console.log(`${Math.round(image.w)}x${Math.round(image.h)} image, tiles at h=${height}: ${px} px a frame with overlap, ${ms.toFixed(1)} ms on one core, ${((ms * fps) / 1000).toFixed(2)} cores at ${fps} fps`);
