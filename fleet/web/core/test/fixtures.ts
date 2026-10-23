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
    layout: { generation: 1, from_frame: 0 },
    tiles,
    image_aspect: 16 / 9,
    fleet: {
      hosts: [{ name: "example-host", draining: false }],
      gpus: tiles.length,
      n: cols * rows,
      max_tiles: 100,
      power: { state: "awake", asleep_since: null, woke_at: null, hosts: [], last_wake: null },
    },
    borrow: { cap: 2, lent: 0, next_return_at: null },
    failures: [],
    counters: { restores: 1284, scrubs: 0, restores_last_hour: 37 },
    scene: { epoch_unix_ms: EPOCH, camera: [{ t: 0, x: 0, y: 0, zoom: 1 }], time_of_day: 0.5, words: [], lights: [] },
    relaunch_all: null,
    next_race_at: null,
    cooldown_until: null,
    scoreboard: { fcvm: null, ec2: null },
    nudges: null,
    viewers: { count: 2, active: 2 },
    actions: [],
    aggregates: [],
    ...over,
  };
}

export function snapshotText(version: number, state: FleetState): string {
  return JSON.stringify({ type: "snapshot", version, state });
}

/**
 * A fleet with `nf` fcvm and `ne` EC2 GPUs in layout generation `gen`: each
 * side as wide as its share of the GPUs, one row per side, as a re-tile gives it.
 */
export function fleetSplit(nf: number, ne: number, gen: number, over: Partial<FleetState> = {}): FleetState {
  const base = fleet(1, 1);
  const proto = base.tiles[0]!;
  const split = nf / Math.max(1, nf + ne);
  const tiles: Tile[] = [];
  for (const [half, n, x0, w] of [["fcvm", nf, 0, split], ["ec2", ne, split, 1 - split]] as const) {
    for (let i = 0; i < n; i++) {
      tiles.push({
        ...structuredClone(proto),
        id: `${half}:${i + 1}`,
        half,
        slot: half === "fcvm" ? `slot-${i + 1}` : `ec2-${i + 1}`,
        rect: { x: x0 + (i * w) / n, y: 0, w: w / n, h: 1 },
        frame_url: `ws://example-host:1/frames/${half}-${i + 1}`,
      });
    }
  }
  return { ...base, layout: { generation: gen, from_frame: gen * 300 }, tiles, ...over };
}
