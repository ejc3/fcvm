// The /boot-race page renders only a summary that says what every number is. These tests are the build's guard:
// app/boot-race/page.tsx parses the same file at build time with the same function, so a missing summary, a
// missing campaign field or a lane without its configuration fails `next build` as it fails here.
import { existsSync, readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { laneTime, parseBootRaceSummary } from "../boot-race.ts";

const PATH = new URL("../../app/boot-race/summary.json", import.meta.url);

describe("boot-race summary", () => {
  it("is committed, and every lane in it names its configuration", () => {
    expect(existsSync(PATH), "fleet/web/app/boot-race/summary.json is missing").toBe(true);
    const s = parseBootRaceSummary(JSON.parse(readFileSync(PATH, "utf8")));
    expect(s.recorded).toBe(true);
    expect(s.lanes.length).toBeGreaterThan(0);
    for (const lane of s.lanes) expect(lane.config.length).toBeGreaterThan(0);
  });

  it("refuses a missing summary", () => {
    expect(() => parseBootRaceSummary(undefined)).toThrow(/the summary is missing/);
  });

  it("refuses a lane without its configuration", () => {
    const s = JSON.parse(readFileSync(PATH, "utf8"));
    delete s.lanes[0].config;
    expect(() => parseBootRaceSummary(s)).toThrow(/has no config/);
  });

  it("refuses results not marked as recorded", () => {
    const s = JSON.parse(readFileSync(PATH, "utf8"));
    s.recorded = false;
    expect(() => parseBootRaceSummary(s)).toThrow(/not marked as recorded/);
  });

  it("writes times as the published page does", () => {
    expect([laneTime(63.66), laneTime(129.9), laneTime(1491.5), laneTime(16827.3)]).toEqual(["63.7 ms", "130 ms", "1.49 s", "16.8 s"]);
  });
});
