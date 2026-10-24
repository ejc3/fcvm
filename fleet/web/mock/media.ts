// The mock's recording of the wall: a short generated clip (ffmpeg's test
// source, not the real wall) in the shapes the page reads from /media: an H.264
// MP4, a VP9 WebM where this ffmpeg has libvpx, WebVTT captions and the JSON
// metadata. Made once per directory; the real reel is recorded from the wall.

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { FFMPEG } from "./encoder.ts";

export const REEL_SECONDS = 4;

function run(args: string[]): boolean {
  const r = spawnSync(FFMPEG, ["-hide_banner", "-loglevel", "error", "-y", ...args], { stdio: ["ignore", "ignore", "pipe"] });
  return r.status === 0;
}

/** Make the clip, its captions and metadata in `dir` if they are not there yet (720p by default, the reel's rendition). */
export function ensureReel(dir: string, recordedAt: number, size = "1280x720"): void {
  mkdirSync(dir, { recursive: true });
  const src = ["-f", "lavfi", "-i", `testsrc2=size=${size}:rate=30:duration=${REEL_SECONDS}`];
  const mp4 = path.join(dir, "wall-reel.mp4");
  if (!existsSync(mp4) && !run([...src, "-c:v", "libx264", "-profile:v", "baseline", "-pix_fmt", "yuv420p", "-movflags", "+faststart", mp4])) {
    throw new Error(`mock media: ${FFMPEG} could not make ${mp4}`);
  }
  const webm = path.join(dir, "wall-reel.webm");
  // Optional: an ffmpeg without libvpx leaves the MP4 alone, as a missing alternative.
  // Realtime settings: a few seconds on a small CI runner, not a minute.
  if (!existsSync(webm)) run([...src, "-c:v", "libvpx-vp9", "-b:v", "1M", "-deadline", "realtime", "-cpu-used", "8", "-pix_fmt", "yuv420p", webm]);
  writeFileSync(path.join(dir, "wall-reel.vtt"), `WEBVTT\n\n00:00.000 --> 00:0${REEL_SECONDS}.000\nA test recording standing in for the wall.\n`);
  // The mock clip shows no race, so it carries no measured numbers.
  writeFileSync(path.join(dir, "wall-reel.json"), JSON.stringify({ recorded_at: recordedAt, duration_s: REEL_SECONDS, measured: null }));
}
