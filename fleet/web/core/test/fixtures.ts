import type { FleetState, Tile } from "../protocol.ts";

export const EPOCH = 1_760_000_000_000;

/** A fleet of `perHalf` tiles each side, cols x rows per half. */
export function fleet(cols = 2, rows = 2, over: Partial<FleetState> = {}): FleetState {
  const tiles: Tile[] = [];
  for (const half of ["fcvm", "ec2"] as const) {
    for (let r = 0; r < rows; r++) {
      for (let c = 0; c < cols; c++) {
        const n = tiles.filter((t) => t.half === half).length + 1;
        tiles.push({
          id: `${half}:${n}`,
          half,
          rect: { x: (half === "fcvm" ? 0 : 0.5) + (c * 0.5) / cols, y: r / rows, w: 0.5 / cols, h: 1 / rows },
          phase: "running",
          slot: half === "fcvm" ? `slot-${n}` : `ec2-${n}`,
          vm: half === "fcvm" ? `fleet-slot-${n}` : `fleet-ec2-${n}`,
          generation: 2,
          fps: 30,
          error: null,
          failure: null,
          borrowed: null,
          frame_url: `ws://example-host:1/frames/${half}-${n}`,
          gpu: { name: "NVIDIA T4", temp_c: 61, power_w: 70.4, util_pct: 98 },
          host: half === "fcvm" ? "g4dn.metal · example-region" : "g4dn.xlarge · example-region",
          last_relaunch: {
            clicked_at: EPOCH + 1000,
            first_frame_at: EPOCH + 1000 + (half === "fcvm" ? 2800 : 58_000),
            phases:
            half === "fcvm"
              ? { teardown_ms: 300, restore_ms: 1600, gpu_ms: 1200, ack_ms: 1560, container_ms: 600, first_frame_ms: 300, total_ms: 2800 }
              : { launch_ms: 14000, boot_ms: 36000, container_ms: 5000, first_frame_ms: 3000, total_ms: 58000 },
          },
        });
      }
    }
  }
  return {
    tiles,
    image_aspect: 16 / 9,
    fleet: { hosts: [{ name: "example-host", draining: false }], gpus: tiles.length, n: cols * rows, max_tiles: 100 },
    borrow: { cap: 2, lent: 0, next_return_at: null },
    counters: { restores_total: 1284, scrubs_total: 1201, restores_last_hour: 37 },
    security: { gpu_scrub: false },
    scene: { epoch_unix_ms: EPOCH, camera: [{ t: 0, x: 0, y: 0, zoom: 1 }], time_of_day: 0.5, words: [], lights: [] },
    relaunch_all: null,
    next_race_at: null,
    cooldown_until: null,
    scoreboard: { fcvm: null, ec2: null },
    nudges: null,
    viewers: { count: 2 },
    actions: [],
    aggregates: [],
    ...over,
  };
}

export function snapshotText(version: number, state: FleetState): string {
  return JSON.stringify({ type: "snapshot", version, state });
}
