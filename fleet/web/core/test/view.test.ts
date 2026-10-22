import { describe, expect, it } from "vitest";
import * as fmt from "../format.ts";
import { initialState, makeIntent, reduce, type ClientState, type FrameStats } from "../store.ts";
import { relaunchParts, wallView, type WallInput } from "../view.ts";
import type { FleetState } from "../protocol.ts";
import { EPOCH, fleet, snapshotText } from "./fixtures.ts";

function stateWith(st: FleetState, version = 1): ClientState {
  let s = reduce(initialState(), { type: "socket", status: "open", at: EPOCH });
  s = reduce(s, { type: "message", text: snapshotText(version, st), at: EPOCH });
  const tiles: FrameStats["tiles"] = {};
  for (const t of st.tiles) tiles[t.id] = { connected: true, fps: 24, newestT: 100, width: 320, height: 180, behind: false, decodeMs: 2 };
  return reduce(s, { type: "frame-stats", stats: { T: 100, tiles, drawMsP50: 1, drawMsP95: 2, rafMsP50: 16.7, rafMsP95: 17 } });
}

function view(s: ClientState, now: number, over: Partial<WallInput> = {}) {
  return wallView({
    state: s,
    now,
    dpr: 1,
    mosaicArea: { x: 0, y: 0, w: 1600, h: 900 },
    focusArea: null,
    canDecode: true,
    phone: false,
    ...over,
  });
}

describe("formatting", () => {
  it("formats durations the way the race shows them", () => {
    expect(fmt.seconds(2840)).toBe("2.8 s");
    expect(fmt.seconds(58_400)).toBe("58 s");
    expect(fmt.raceClock(4290)).toBe("0:04.2");
    expect(fmt.raceClock(67_950)).toBe("1:07.9");
    expect(fmt.countdown(119_001)).toBe("2:00");
    expect(fmt.utcClock(Date.UTC(2026, 9, 22, 14, 3, 7))).toBe("14:03:07");
    expect(fmt.timeOfDay(6)).toBe("06:00");
    expect(fmt.timeOfDay(18.5)).toBe("18:30");
    expect(fmt.interval(120)).toBe("every 2 minutes");
  });
});

describe("the race", () => {
  it("runs one clock and stamps each half as it finishes", () => {
    const start = EPOCH + 10_000;
    const st = fleet(2, 2, { relaunch_all: { started_at: start, by: "teal otter", fcvm_done_at: null, ec2_done_at: null } });
    for (const t of st.tiles) {
      t.last_relaunch = { clicked_at: start, first_frame_at: null, phases: {} };
      t.phase = t.half === "fcvm" ? "attaching_gpu" : "pending";
    }
    let v = view(stateWith(st), start + 1500);
    expect(v.race!.active).toBe(true);
    expect(v.race!.clock).toBe("0:01.5");
    expect(v.race!.fcvm.back).toBe("0 / 4 back");
    expect(v.race!.ec2.stamp).toBe("still launching");
    expect(v.controls!.relaunchAll).toMatchObject({ disabled: true, note: "started by teal otter" });
    expect(v.tiles.every((t) => t.dark)).toBe(true);
    expect(v.tiles[0]!.darkClock).toBe("1.5 s");

    // fcvm half back at 2.9 s.
    const st2 = structuredClone(st);
    st2.relaunch_all!.fcvm_done_at = start + 2900;
    for (const t of st2.tiles.filter((x) => x.half === "fcvm")) {
      t.phase = "running";
      t.last_relaunch = { clicked_at: start, first_frame_at: start + 2900, phases: { restoring: 400 } };
    }
    v = view(stateWith(st2, 2), start + 4000);
    expect(v.race!.fcvm.back).toBe("4 / 4 back");
    expect(v.race!.fcvm.stamp).toBe("all back at 0:02.9");
    expect(v.race!.ec2.back).toBe("0 / 4 back");
    expect(v.race!.clock).toBe("0:04.0");

    // EC2 back at 61.2 s: the race is over and the clock stops.
    st2.relaunch_all!.ec2_done_at = start + 61_200;
    for (const t of st2.tiles.filter((x) => x.half === "ec2")) {
      t.phase = "running";
      t.last_relaunch = { clicked_at: start, first_frame_at: start + 61_200, phases: {} };
    }
    v = view(stateWith(st2, 3), start + 90_000);
    expect(v.race!.active).toBe(false);
    expect(v.race!.clock).toBe("1:01.2");
    expect(v.race!.fcvm.stamp).toBe("last relaunch all: 2.9 s");
    expect(v.race!.ec2.stamp).toBe("last relaunch all: 61 s");
  });

  it("shows a missing EC2 GPU as dark, waiting for capacity, and leaves it out of the race", () => {
    const st = fleet(2, 2);
    const t = st.tiles.find((x) => x.id === "ec2:4")!;
    t.phase = "waiting_capacity";
    t.slot = null;
    t.gpu = null;
    t.frame_url = null;
    t.last_relaunch = null;
    const v = view(stateWith(st), EPOCH + 1000);
    const tv = v.tiles.find((x) => x.id === "ec2:4")!;
    expect(tv.dark).toBe(true);
    expect(tv.darkPhase).toBe("waiting for capacity");
    expect(tv.darkClock).toBe("");
    expect(v.race!.ec2.renderedBy).toBe("rendered by 3 EC2 GPUs");
    expect(v.plan.tiles.find((x) => x.id === "ec2:4")!.url).toBeNull();
  });
});

describe("controls", () => {
  it("shows who set auto-relaunch and when the next one is due", () => {
    const st = fleet(2, 2, {
      settings: { auto_relaunch_secs: 120 },
      actions: [{ at: EPOCH, by: "quiet heron", name: "set_auto_relaunch", args: null, summary: "auto relaunch every 120 s", outcome: "committed" }],
    });
    const v = view(stateWith(st), EPOCH + 1000 + 1000);
    // Tiles were clicked at EPOCH + 1000: due at EPOCH + 121000.
    expect(v.controls!.auto.selected).toBe("120");
    expect(v.controls!.auto.note).toBe("set by quiet heron · next in 1:59");
  });

  it("shows a pending setting before the coordinator confirms it", () => {
    let s = stateWith(fleet());
    s = reduce(s, { type: "intent-sent", intent: makeIntent("a", "set_auto_relaunch", { secs: 300 }), at: EPOCH });
    expect(view(s, EPOCH).controls!.auto.selected).toBe("300");
  });

  it("reports a word that is being checked, then refused", () => {
    let s = stateWith(fleet());
    s = reduce(s, { type: "intent-sent", intent: makeIntent("w", "word", { text: "forbidden" }), at: EPOCH });
    expect(view(s, EPOCH).controls!.word.status).toBe("Checking “forbidden”");
    s = reduce(s, { type: "message", text: JSON.stringify({ type: "receipt", id: "w", outcome: "refused", reason: "the text check refused it", version: 1 }), at: EPOCH });
    expect(view(s, EPOCH).controls!.word).toMatchObject({ status: "“forbidden” was not added: the text check refused it", statusKind: "refused" });
  });

  it("darkens a tile at once when this viewer relaunches it", () => {
    let s = stateWith(fleet());
    s = reduce(s, { type: "intent-sent", intent: makeIntent("r", "relaunch", { tile: "fcvm:2" }), at: EPOCH });
    const v = view(s, EPOCH);
    expect(v.tiles.find((t) => t.id === "fcvm:2")).toMatchObject({ name: "slot-2", dark: true, darkPhase: "relaunch requested" });
    expect(v.tiles.find((t) => t.id === "fcvm:1")!.dark).toBe(false);
  });
});

describe("seams and hover", () => {
  it("shows boundaries while the pointer is over the image, and outlines the hovered tile", () => {
    let s = stateWith(fleet());
    expect(view(s, EPOCH).seams.visible).toBe(false);
    s = reduce(s, { type: "ui", patch: { pointerOverImage: true, hoverId: "ec2:2" } });
    const v = view(s, EPOCH);
    expect(v.seams.visible).toBe(true);
    expect(v.seams.outline!.color).toBe("#F5A524");
    expect(v.seams.label).toMatchObject({ id: "ec2-2", kind: "EC2", gpu: "NVIDIA T4", last: "last relaunch 58 s" });
    expect(v.seams.lineWidth).toBe(1);
  });

  it("gives keyboard focus the same outline and label", () => {
    const s = reduce(stateWith(fleet()), { type: "ui", patch: { keyId: "fcvm:1" } });
    const v = view(s, EPOCH, { dpr: 2 });
    expect(v.seams.visible).toBe(true);
    expect(v.seams.outline!.color).toBe("#7CC4FF");
    expect(v.seams.lineWidth).toBe(0.5);
  });
});

describe("opened tile", () => {
  it("asks for a larger stream from that tile only, and keeps its rectangle", () => {
    const st = fleet(5, 5);
    const s = reduce(stateWith(st), { type: "ui", patch: { focusId: "fcvm:7" } });
    const v = view(s, EPOCH, { focusArea: { x: 0, y: 0, w: 1280, h: 720 } });
    const t = v.plan.tiles.find((x) => x.id === "fcvm:7")!;
    expect(t.height).toBe(720);
    expect(t.url).toContain("h=720");
    // Same rectangle of the image: a tile of 1/10 x 1/5 keeps that aspect.
    expect(t.box!.w / t.box!.h).toBeCloseTo((0.1 * 16) / 9 / 0.2, 2);
    expect(v.plan.tiles.filter((x) => x.box !== null)).toHaveLength(1);
    expect(v.focus!.bars.map((b) => b.title)).toEqual(["slot-7 (fcvm)", "ec2-7 (EC2)"]);
  });
});

describe("relaunch timings", () => {
  it("orders the parts, splits the GPU out of the restore and leaves out sums", () => {
    expect(relaunchParts({ container_ms: 600, first_frame_ms: 250, gpu_ms: 3100, restore_ms: 4300, teardown_ms: 310, total_ms: 5200, ack_ms: 4200 })).toEqual([
      ["old VM stopped", 310],
      ["snapshot restored", 1200],
      ["GPU attached, driver loaded", 3100],
      ["container started", 600],
      ["first frame", 250],
    ]);
  });
});

describe("the recent actions", () => {
  it("words the coordinator's summaries, leaves out camera moves and refusals", () => {
    const actions = [
      { at: EPOCH + 1, by: "teal otter", name: "relaunch", args: null, summary: "relaunch slot-3", outcome: "committed" },
      { at: EPOCH + 2, by: "teal otter", name: "camera", args: null, summary: "camera", outcome: "committed" },
      { at: EPOCH + 3, by: "quiet heron", name: "word", args: null, summary: "the text check refused it", outcome: "refused" },
      { at: EPOCH + 4, by: "quiet heron", name: "word", args: null, summary: 'word "hello"', outcome: "committed" },
      { at: EPOCH + 5, by: "teal otter", name: "time_of_day", args: null, summary: "time of day 15.5 h", outcome: "committed" },
    ];
    const v = view(stateWith(fleet(2, 2, { actions })), EPOCH + 5000);
    expect(v.ticker.map((t) => t.text)).toEqual([
      "teal otter set time of day 15.5 h",
      'quiet heron wrote "hello"',
      "teal otter relaunched slot-3",
    ]);
  });
});
