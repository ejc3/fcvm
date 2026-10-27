import { describe, expect, it } from "vitest";
import { FrameLog, LAG_FRAMES_KEPT, sceneAgeMs } from "../lag.ts";

describe("FrameLog", () => {
  it("gives each stage's p50 as local time minus scene time", () => {
    const log = new FrameLog();
    for (let k = 0; k < 30; k++) {
      const t = 100 + k / 30;
      const ms = t * 1000;
      log.arrived(t, ms + 50 + (k % 3));
      log.decoded(t, ms + 53 + (k % 3));
      if (k < 29) log.drawn(t, ms + 80);
    }
    expect(log.summary()).toEqual({ arrive: 51, decoded: 54, drawn: 80, n: 30 });
  });

  it("keeps the first time of each stage and the last LAG_FRAMES_KEPT frames", () => {
    const log = new FrameLog();
    log.arrived(1, 1050);
    log.arrived(1, 1900);
    log.decoded(1, 1060);
    log.decoded(1, 1990);
    log.drawn(1, 1100);
    log.drawn(1, 1200);
    expect(log.summary()).toEqual({ arrive: 50, decoded: 60, drawn: 100, n: 1 });
    for (let k = 0; k < LAG_FRAMES_KEPT + 10; k++) log.arrived(10 + k, (10 + k) * 1000 + 20);
    expect(log.summary()).toEqual({ arrive: 20, decoded: null, drawn: null, n: LAG_FRAMES_KEPT });
  });

  it("has nothing to say before a frame arrives", () => {
    expect(new FrameLog().summary()).toBeNull();
  });

  it("turns a local age into one on the coordinator's clock", () => {
    // Local clock 5 ms behind the coordinator: a frame that arrived 60 ms after its
    // scene time on the local clock is 65 ms old on the coordinator's.
    expect(sceneAgeMs(1_760_000_000_060, 5, 1_760_000_000_000)).toBe(65);
  });
});
