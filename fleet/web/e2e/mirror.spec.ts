// The GPU mirror's replay test (docs/fleet/ARCHITECTURE.md, "The GPU mirror", point 6): a Robot
// Park command stream captured on a gpubox (the phone streamer's GET /mirror, inflated) is fed to
// the mirror on WebGL2 in headless Chrome, and each posted frame whose stamp matches one of the
// phone's own screenshots (adb screencap, taken while the stream was captured) is compared with
// it. Robot Park stamps its frame number into the bottom band of every frame, so a mirror frame
// and a screenshot of the same number show the same scene time.
//
// The fixture is e2e/fixtures/mirror/<name>: tap.gz (the tap stream) and shot-*.png;
// MIRROR_FIXTURE names another directory.

import { readdirSync, readFileSync } from "node:fs";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { expect, test } from "@playwright/test";
import { build } from "vite";
import type { ReplayResult } from "./mirror/replay-entry.ts";

const here = fileURLToPath(new URL(".", import.meta.url));
const fixture = process.env.MIRROR_FIXTURE ?? join(here, "fixtures/mirror/robotpark");

/**
 * The bar a mirror frame clears against the T4's frame of the same stamp. Two GPUs rasterize
 * the same draws differently along edges (4x MSAA resolves, polygon edges) and round lighting
 * by a level or two, so an exact match is not expected; a mirror that draws a call wrong, or
 * misses one, differs over whole objects. Measured on the committed capture, SwiftShader
 * against the T4: 0.41% of pixels more than 16 levels apart and 44.5 dB, in both frames
 * compared. The bar leaves 2.4 times and 6.5 dB of room.
 */
const MAX_OVER16 = Number(process.env.MIRROR_MAX_OVER16 ?? 0.01);
const MIN_PSNR = Number(process.env.MIRROR_MIN_PSNR ?? 38);

let bundle = "";
test.beforeAll(async () => {
  const out = mkdtempSync(join(tmpdir(), "mirror-replay-"));
  await build({
    logLevel: "warn",
    configFile: false,
    build: {
      outDir: out,
      emptyOutDir: true,
      lib: { entry: join(here, "mirror/replay-entry.ts"), formats: ["iife"], name: "mirrorReplayBundle", fileName: () => "replay.js" },
    },
  });
  bundle = readFileSync(join(out, "replay.js"), "utf8");
  rmSync(out, { recursive: true, force: true });
});

test("a captured Robot Park stream replayed on WebGL2 matches the phone's own frames of the same stamp", async ({ page }) => {
  const tap = gunzipSync(readFileSync(join(fixture, "tap.gz")));
  const shots = readdirSync(fixture).filter((f) => /^shot-\d+\.png$/.test(f)).sort();
  expect(shots.length).toBeGreaterThan(0);
  await page.route("http://mirror.test/**", (route) => {
    const name = new URL(route.request().url()).pathname.slice(1);
    const body = name === "tap" ? tap : readFileSync(join(fixture, name));
    return route.fulfill({ body, contentType: name === "tap" ? "application/octet-stream" : "image/png" });
  });
  await page.route("http://mirror.test/", (route) => route.fulfill({ body: "<!doctype html><canvas id=c width=720 height=1280></canvas>", contentType: "text/html" }));
  await page.goto("http://mirror.test/");
  await page.addScriptTag({ content: bundle });
  const result = await page.evaluate(async (names) => {
    // the screenshots, decoded, with the stamp each carries
    const refs = [];
    for (const n of names) {
      const img = new Image();
      img.src = `http://mirror.test/${n}`;
      await img.decode();
      const c = document.createElement("canvas");
      c.width = img.width;
      c.height = img.height;
      const g = c.getContext("2d")!;
      g.drawImage(img, 0, 0);
      const px = g.getImageData(0, 0, c.width, c.height).data;
      const k = window.mirrorReplay.readStamp(px, c.width, c.height);
      if (k !== null) refs.push({ k, w: c.width, h: c.height, px });
    }
    const tapBytes = new Uint8Array(await (await fetch("http://mirror.test/tap")).arrayBuffer());
    const r = window.mirrorReplay.replay(document.getElementById("c") as HTMLCanvasElement, tapBytes, refs);
    return { ...r, refs: refs.map((x) => x.k) };
  }, shots);
  const r = result as ReplayResult & { refs: number[] };
  console.log(JSON.stringify({ ...r, stamped: `${r.stamped.length} frames, ${r.stamped[0]}..${r.stamped[r.stamped.length - 1]}` }));
  expect(r.refs.length, "the screenshots carry Robot Park's stamp").toBe(shots.length);
  expect(r.failed, "no call failed in WebGL2").toEqual({});
  expect(r.posts).toBeGreaterThan(30);
  expect(r.missing, "every screenshot's frame was posted by the mirror").toEqual([]);
  for (const m of r.matches) {
    expect(m.over16, `frame ${m.k}: share of pixels more than 16 levels off`).toBeLessThanOrEqual(MAX_OVER16);
    expect(m.psnr, `frame ${m.k}: PSNR`).toBeGreaterThanOrEqual(MIN_PSNR);
  }
});
