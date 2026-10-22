// Start the mock coordinator and mock renderers.
//
//   FLEET_MOCK_COORD_PORT    coordinator port (required; 0 asks the kernel)
//   FLEET_MOCK_RENDER_PORT   renderer port (required; 0 asks the kernel)
//   FLEET_MOCK_HOST          host name the browser uses for frame_url (localhost)
//   FLEET_MOCK_TILES         tiles per half (8, so 16 in all)
//   FLEET_MOCK_EC2_SHORT     EC2 tiles waiting for capacity (1)
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
//   FLEET_MOCK_FPS           frames per second, a divisor of 30 (30): frames land on t = k/30 as the renderer's do
//   FLEET_MOCK_THREADS       render threads (12)
//   FLEET_MOCK_MAX_HEIGHT    tallest stream it will encode (1080)
//   FLEET_MOCK_REFUSED       comma-separated words the fake text check refuses (forbidden)
//   FLEET_FFMPEG             the ffmpeg binary, with libx264 (ffmpeg on the PATH)
//
// Prints one JSON line with the bound ports when ready.

import { MockCoordinator } from "./coordinator.ts";
import { MockRenderers } from "./renderer.ts";

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
  let coord: MockCoordinator | null = null;
  const renderers = new MockRenderers({
    port: int("FLEET_MOCK_RENDER_PORT"),
    publicHost: env("FLEET_MOCK_HOST", "localhost"),
    fps: fps(),
    threads: int("FLEET_MOCK_THREADS", 12),
    maxHeight: int("FLEET_MOCK_MAX_HEIGHT", 1080),
    epochUnixMs,
    scene: () => coord!.patternScene(),
  });
  await renderers.listen();
  coord = new MockCoordinator({
    port: int("FLEET_MOCK_COORD_PORT"),
    perHalf: int("FLEET_MOCK_TILES", 8),
    ec2Short: int("FLEET_MOCK_EC2_SHORT", 1),
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
