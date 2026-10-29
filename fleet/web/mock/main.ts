// Start the mock coordinator and mock renderers.
//
//   FLEET_MOCK_COORD_PORT    coordinator port (required; 0 asks the kernel)
//   FLEET_MOCK_RENDER_PORT   renderer port (required; 0 asks the kernel)
//   FLEET_MOCK_HOST          host name the browser uses for frame_url (localhost)
//   FLEET_MOCK_PUBLIC_RENDER_PORT  the port frame_url names, when a proxy sits in front (the renderer port)
//   FLEET_MOCK_FCVM_GPUS     fcvm GPUs live (8); each side is as wide as its share
//   FLEET_MOCK_EC2_GPUS      EC2 GPUs live (7)
//   FLEET_MOCK_GPUS_PER_HOST GPUs per fcvm host (8)
//   FLEET_MOCK_FCVM_MS       typical fcvm restart (3000; each takes 0.87 to 1.13 of it)
//   FLEET_MOCK_EC2_MS        typical EC2 restart (52000; each takes 0.67 to 1.33 of it)
//   FLEET_MOCK_RACE_EVERY_S  the owner's cadence: restart every GPU this often (300; 0 for none)
//   FLEET_MOCK_FIRST_RACE_S  the first scheduled restart, after start (20)
//   FLEET_MOCK_COOLDOWN_S    after a restart of every GPU, "restart now" waits this long (60)
//   FLEET_MOCK_WAIT_PERCENT  chance in percent that a race's EC2 half waits for spot capacity (25)
//   FLEET_MOCK_BORROW_CAP    wall GPUs lent to visitors at most at once (2)
//   FLEET_MOCK_CLAIM_S       how long a claimed GPU lasts (60)
//   FLEET_MOCK_HOLD_S        how long the directed camera holds on a stop (20)
//   FLEET_MOCK_TURNSTILE     a Turnstile site key: actions need the check (unset: none)
//   FLEET_MOCK_GPU_SCRUB     1: the GPU memory scrub has shipped (counted, and said on the page)
//   FLEET_MOCK_SLEEP_AFTER_S with no visible viewer this long the fleet scales to zero (60; 0: never)
//   FLEET_MOCK_WAKE_MS       how long a wake takes, first instance to every tile rendering (120000)
//   FLEET_MOCK_MEDIA_DIR     where the generated recording is kept (node_modules/.cache/fleet-mock-media)
//   FLEET_MOCK_PHONES        phones for phones mode (0: none, and no `phones` in the snapshot)
//   FLEET_MOCK_PHONE_CAP     phones lent at once at most (2)
//   FLEET_MOCK_PHONE_CLAIM_S how long a borrowed phone lasts (120)
//   FLEET_MOCK_PHONE_COOLDOWN_S  after a phone comes back, its viewer waits this long to borrow again (60)
//   FLEET_MOCK_PHONE_RESTORE_MS  a phone's restore after a reboot or a return (2000)
//   FLEET_MOCK_PHONE_ZOOM_GAP_MS a viewer's zoom is refused sooner than this after its last (2000)
//   FLEET_MOCK_FPS           frames per second, a divisor of 30 (30): frames land on t = k/30 as the renderer's do
//   FLEET_MOCK_THREADS       render threads (12)
//   FLEET_MOCK_MAX_HEIGHT    tallest stream it will encode (1080)
//   FLEET_MOCK_REFUSED       comma-separated words the fake text check refuses (forbidden)
//   FLEET_FFMPEG             the ffmpeg binary, with libx264 (ffmpeg on the PATH)
//
// Prints one JSON line with the bound ports when ready.

import { MockCoordinator } from "./coordinator.ts";
import { MockRenderers } from "./renderer.ts";
import { ensureReel } from "./media.ts";

function env(name: string, fallback?: string): string {
  const v = process.env[name] ?? fallback;
  if (v === undefined) {
    console.error(`${name} is not set. Claim a port block and set it (see fleet/web/README.md).`);
    process.exit(2);
  }
  return v;
}

function int(name: string, fallback?: number): number {
  const v = Number(env(name, fallback === undefined ? undefined : String(fallback)));
  if (!Number.isInteger(v) || v < 0) {
    console.error(`${name} must be a whole number`);
    process.exit(2);
  }
  return v;
}

function fps(): number {
  const v = int("FLEET_MOCK_FPS", 30);
  if (v === 0 || 30 % v !== 0) {
    console.error("FLEET_MOCK_FPS must divide 30, so frames fall on the renderer's t = k/30 grid");
    process.exit(2);
  }
  return v;
}

export async function startMock() {
  const epochUnixMs = Math.floor(Date.now() / 1000) * 1000;
  const mediaDir = env("FLEET_MOCK_MEDIA_DIR", "node_modules/.cache/fleet-mock-media");
  ensureReel(mediaDir, epochUnixMs);
  let coord: MockCoordinator | null = null;
  const renderers = new MockRenderers({
    port: int("FLEET_MOCK_RENDER_PORT"),
    publicHost: env("FLEET_MOCK_HOST", "localhost"),
    publicHostV4: env("FLEET_MOCK_HOST_V4", "127.0.0.1"),
    ...(process.env["FLEET_MOCK_PUBLIC_RENDER_PORT"] ? { publicPort: int("FLEET_MOCK_PUBLIC_RENDER_PORT") } : {}),
    fps: fps(),
    threads: int("FLEET_MOCK_THREADS", 12),
    maxHeight: int("FLEET_MOCK_MAX_HEIGHT", 1080),
    epochUnixMs,
    scene: () => coord!.patternScene(),
  });
  await renderers.listen();
  coord = new MockCoordinator({
    port: int("FLEET_MOCK_COORD_PORT"),
    fcvmGpus: int("FLEET_MOCK_FCVM_GPUS", 8),
    ec2Gpus: int("FLEET_MOCK_EC2_GPUS", 7),
    gpusPerHost: Math.max(1, int("FLEET_MOCK_GPUS_PER_HOST", 8)),
    fcvmRelaunchMs: int("FLEET_MOCK_FCVM_MS", 3000),
    ec2RelaunchMs: int("FLEET_MOCK_EC2_MS", 52000),
    raceEverySecs: int("FLEET_MOCK_RACE_EVERY_S", 300),
    firstRaceSecs: int("FLEET_MOCK_FIRST_RACE_S", 20),
    cooldownSecs: int("FLEET_MOCK_COOLDOWN_S", 60),
    capacityWaitProb: int("FLEET_MOCK_WAIT_PERCENT", 25) / 100,
    capacityWaitMs: [10_000, 30_000],
    borrowCap: int("FLEET_MOCK_BORROW_CAP", 2),
    claimSecs: int("FLEET_MOCK_CLAIM_S", 60),
    holdSecs: int("FLEET_MOCK_HOLD_S", 20),
    travelSecs: 4,
    turnstileSitekey: process.env.FLEET_MOCK_TURNSTILE || null,
    gpuScrub: process.env.FLEET_MOCK_GPU_SCRUB === "1",
    sleepAfterSecs: int("FLEET_MOCK_SLEEP_AFTER_S", 60),
    wakeMs: int("FLEET_MOCK_WAKE_MS", 120_000),
    mediaDir,
    ...(int("FLEET_MOCK_PHONES", 0) > 0
      ? {
          phones: {
            count: int("FLEET_MOCK_PHONES", 0),
            cap: int("FLEET_MOCK_PHONE_CAP", 2),
            claimSecs: int("FLEET_MOCK_PHONE_CLAIM_S", 120),
            cooldownSecs: int("FLEET_MOCK_PHONE_COOLDOWN_S", 60),
            restoreMs: int("FLEET_MOCK_PHONE_RESTORE_MS", 2000),
            zoomGapMs: int("FLEET_MOCK_PHONE_ZOOM_GAP_MS", 2000),
          },
        }
      : {}),
    refusedWords: env("FLEET_MOCK_REFUSED", "forbidden").split(",").filter(Boolean),
    checkMs: 250,
    epochUnixMs,
    renderers,
  });
  await coord.listen();
  return { coord, renderers };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const { coord, renderers } = await startMock();
  console.log(JSON.stringify({ ready: true, coord: coord.port, render: renderers.port }));
  const stop = async () => {
    await coord.close();
    await renderers.close();
    process.exit(0);
  };
  process.on("SIGINT", () => void stop());
  process.on("SIGTERM", () => void stop());
}
