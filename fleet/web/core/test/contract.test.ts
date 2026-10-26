// The page reads the coordinator's own snapshot states: every vector in #1146's
// fleet/core/tests/vectors ends with `final_view`, the whole state a browser receives.
// fixtures/coordinator.json holds them (scripts/coordinator-fixture.ts regenerates it).
// A field the page names differently from the coordinator fails here, including one
// the parser would quietly default.
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { INTENT_NAMES, parseServerMsg, type FleetState } from "../protocol.ts";
import { lastRaceLine, wakeMedianMs } from "../view.ts";

type Fixture = { source: { commit: string }; intents: { known: string[]; unknown: string[] }; views: Record<string, Record<string, unknown>> };
const fixture = JSON.parse(readFileSync(new URL("./fixtures/coordinator.json", import.meta.url), "utf8")) as Fixture;
const views = Object.entries(fixture.views);

/** Every leaf's path, array indices kept (`tiles.3.rect.x`). */
function leaves(v: unknown, at = "", out = new Map<string, unknown>()): Map<string, unknown> {
  if (v !== null && typeof v === "object") {
    const entries = Array.isArray(v) ? v.map((x, i) => [String(i), x] as const) : Object.entries(v);
    if (entries.length === 0) out.set(at, v);
    for (const [k, x] of entries) leaves(x, at ? `${at}.${k}` : k, out);
  } else {
    out.set(at, v);
  }
  return out;
}
const shape = (path: string) => path.replace(/\.\d+(?=\.|$)/g, "[]");

/**
 * Fields the parsed state has that the wire does not, each the page's own reading.
 * Anything else in the parsed state must sit at the same path in the coordinator's.
 */
const PAGE_DERIVED: Record<string, string> = {
  "tiles[].gpu.temp_c": "a reading from gpu_stats, kept beside the GPU's name",
  "tiles[].gpu.power_w": "a reading from gpu_stats",
  "tiles[].gpu.util_pct": "a reading from gpu_stats",
  "scene.lights[].color": "linear RGB turned into a CSS colour",
  "relaunch_all.ec2_capacity_wait_ms": "0 when absent",
  "layout.generation": "0 when the coordinator sends no layout",
  "layout.from_frame": "0 when the coordinator sends no layout",
};

function parse(state: Record<string, unknown>): FleetState {
  const m = parseServerMsg(JSON.stringify({ type: "snapshot", version: 1, state }));
  if (m.type !== "snapshot") throw new Error("not a snapshot");
  return m.state;
}

describe(`the coordinator's snapshot states (#1146 at ${fixture.source.commit})`, () => {
  it("has the views to test against", () => {
    expect(views.length).toBeGreaterThanOrEqual(20);
  });

  it.each(views)("%s parses, and every field the page keeps has the coordinator's name and value", (_name, raw) => {
    const parsed = parse(raw);
    const wire = leaves(raw);
    const wireShapes = new Set([...wire.keys()].map(shape));
    // Containers too: a parsed object the wire has under another name is caught by its leaves.
    for (const [path, value] of leaves(parsed)) {
      const s = shape(path);
      if (s in PAGE_DERIVED || [...Object.keys(PAGE_DERIVED)].some((d) => s.startsWith(`${d}.`))) continue;
      // A field the page keeps as null or [] where the wire has nothing is the parser's default.
      if (!wire.has(path) && (value === null || (Array.isArray(value) && value.length === 0))) continue;
      expect(wireShapes.has(s) || wire.has(path), `${path} is not in the coordinator's state`).toBe(true);
      if (wire.has(path)) expect(value, path).toEqual(wire.get(path));
    }
  });

  it("covers the fields the renames were about", () => {
    const all = views.map(([, v]) => v as Record<string, Record<string, unknown>>);
    expect(all.some((v) => (v["layout"]?.["generation"] as number) > 0)).toBe(true);
    expect(all.some((v) => (v["layout"]?.["from_frame"] as number) > 0)).toBe(true);
    expect(all.some((v) => (v["counters"]?.["restores"] as number) > 0)).toBe(true);
    for (const [, raw] of views) {
      const p = parse(raw);
      const w = raw as Record<string, Record<string, unknown>>;
      expect(p.layout.generation).toBe(w["layout"]!["generation"]);
      expect(p.layout.from_frame).toBe(w["layout"]!["from_frame"]);
      expect(p.counters?.restores).toBe(w["counters"]!["restores"]);
      expect(p.counters?.scrubs).toBe(w["counters"]!["scrubs"]);
      expect(p.viewers.active).toBe(w["viewers"]!["active"]);
    }
  });

  it("keeps the races and the wakes, so the strip and the asleep line have measured numbers", () => {
    const sb = parse(fixture.views["15_scoreboard"]!);
    const wire = fixture.views["15_scoreboard"]!["races"] as { id: number; fcvm: { time_ms: number | null } | null; ec2: { time_ms: number | null } | null }[];
    expect(sb.races.map((r) => r.id)).toEqual(wire.map((r) => r.id));
    expect(sb.races.map((r) => r.fcvm?.time_ms ?? null)).toEqual(wire.map((r) => r.fcvm?.time_ms ?? null));
    expect(sb.races.map((r) => r.ec2?.time_ms ?? null)).toEqual(wire.map((r) => r.ec2?.time_ms ?? null));
    // With no scoreboard, the newest race the coordinator kept still gives the strip its line.
    const gen = parse(fixture.views["02_relaunch_generations"]!);
    expect(lastRaceLine({ ...gen, scoreboard: { fcvm: null, ec2: null } })).toBe("Last race: fcvm back in 4.0 s");
    const power = parse(fixture.views["20_power"]!).fleet.power;
    expect(power.wakes).toEqual([{ started_at: 400_000, awake_at: 481_000, gpus: 4 }]);
    expect(wakeMedianMs(power)).toBe(81_000);
  });

  it("the page sends only intents the coordinator knows", () => {
    expect(fixture.intents.known).toContain("restart_now");
    for (const n of INTENT_NAMES) {
      expect(fixture.intents.known, n).toContain(n);
      expect(fixture.intents.unknown, n).not.toContain(n);
    }
  });
});
