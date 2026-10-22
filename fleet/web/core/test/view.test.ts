import { describe, expect, it } from "vitest";
import * as fmt from "../format.ts";
import { initialState, makeIntent, reduce, type ClientState, type FrameStats } from "../store.ts";
import { placeLabel, relaunchParts, sourceIdFor, wallView, type WallInput } from "../view.ts";
import { suggestMode } from "../mode.ts";
import type { FleetState } from "../protocol.ts";
import { EPOCH, fleet, snapshotText } from "./fixtures.ts";

function stateWith(st: FleetState, version = 1): ClientState {
  let s = reduce(initialState(), { type: "socket", status: "open", at: EPOCH });
  s = reduce(s, { type: "message", text: snapshotText(version, st), at: EPOCH });
  const tiles: FrameStats["tiles"] = {};
  for (const t of st.tiles) tiles[`d:${t.id}`] = { connected: true, fps: 24, newestT: 100, width: 320, height: 180, behind: false, decodeMs: 2 };
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
    expect(fmt.timeOfDay(0.25)).toBe("06:00");
    expect(fmt.timeOfDay(18.5 / 24)).toBe("18:30");
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
    expect(v.plan.direct.find((x) => x.id === "d:ec2:4")).toBeUndefined();
    expect(v.plan.tiles.find((x) => x.id === "ec2:4")!.dark).toBe(true);
  });
});

describe("controls", () => {
  it("shows who set auto-relaunch and when the next one is due", () => {
    const st = fleet(2, 2, {
      settings: { auto_relaunch_secs: 120 },
      actions: [{ at: EPOCH, by: "quiet heron", viewer: null, name: "set_auto_relaunch", args: null, summary: "auto relaunch every 120 s", outcome: "committed" }],
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
    const t = v.plan.direct.find((x) => x.id === "d:fcvm:7")!;
    expect(t.height).toBe(720);
    expect(t.url).toContain("h=720");
    // Same rectangle of the image: a tile of 1/10 x 1/5 keeps that aspect.
    expect(t.box!.w / t.box!.h).toBeCloseTo((0.1 * 16) / 9 / 0.2, 2);
    expect(v.plan.direct.filter((x) => x.box !== null)).toHaveLength(1);
    expect(v.plan.focus!.id).toBe("d:fcvm:7");
    // The others keep streaming at the smallest height.
    expect(new Set(v.plan.direct.filter((x) => x.id !== "d:fcvm:7").map((x) => x.height))).toEqual(new Set([72]));
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
      { at: EPOCH + 1, by: "teal otter", viewer: null, name: "relaunch", args: null, summary: "relaunch slot-3", outcome: "committed" },
      { at: EPOCH + 2, by: "teal otter", viewer: null, name: "camera", args: null, summary: "camera", outcome: "committed" },
      { at: EPOCH + 3, by: "quiet heron", viewer: null, name: "word", args: null, summary: "the text check refused it", outcome: "refused" },
      { at: EPOCH + 4, by: "quiet heron", viewer: null, name: "word", args: null, summary: 'word "hello"', outcome: "committed" },
      { at: EPOCH + 5, by: "teal otter", viewer: null, name: "time_of_day", args: null, summary: "time of day 15.5 h", outcome: "committed" },
    ];
    const v = view(stateWith(fleet(2, 2, { actions })), EPOCH + 5000);
    expect(v.ticker.map((t) => t.text)).toEqual([
      "teal otter set time of day 15.5 h",
      'quiet heron wrote "hello"',
      "teal otter relaunched slot-3",
    ]);
  });
});

describe("you", () => {
  it("names this viewer's own actions as you", () => {
    const actions = [
      { at: EPOCH + 1, by: "viewer:v-4", viewer: "viewer:v-4", name: "relaunch", args: { tile: "slot-3" }, summary: "relaunch slot-3", outcome: "committed" },
      { at: EPOCH + 2, by: "viewer:v-7", viewer: "viewer:v-7", name: "relaunch_all", args: {}, summary: "relaunch all (8 tiles)", outcome: "committed" },
      { at: EPOCH + 3, by: "viewer:v-4", viewer: "viewer:v-4", name: "set_auto_relaunch", args: { secs: 120 }, summary: "auto relaunch every 120 s", outcome: "committed" },
    ];
    let s = stateWith(fleet(2, 2, { actions, settings: { auto_relaunch_secs: 120 } }));
    s = reduce(s, { type: "message", text: JSON.stringify({ type: "welcome", viewer: "viewer:v-4" }), at: EPOCH });
    const v = view(s, EPOCH + 5000);
    expect(v.ticker.map((t) => t.text)).toEqual(["you set auto-relaunch to every 2 minutes", "viewer:v-7 relaunched all", "you relaunched slot-3"]);
    expect(v.controls!.auto.note).toMatch(/^set by you · next in /);
  });
});

describe("hover label placement", () => {
  const area = { x: 0, y: 0, w: 1440, h: 800 };
  const card = { x: 20, y: 20, w: 200, h: 125 };
  it("never covers a card when another place is free", () => {
    // A tile just under the fcvm counter card: above would cover the card.
    const tile = { x: 0, y: 200, w: 360, h: 200 };
    const at = placeLabel(tile, { w: 230, h: 92 }, area, [card]);
    const b = { x: at.x, y: at.y, w: 230, h: 92 };
    expect(b.x < card.x + card.w && card.x < b.x + b.w && b.y < card.y + card.h && card.y < b.y + b.h).toBe(false);
    expect(at.x).toBeGreaterThanOrEqual(0);
    expect(at.y + 92).toBeLessThanOrEqual(800);
  });
  it("moves past a card that reaches below the tile it labels (a small screen)", () => {
    const tile = { x: 54, y: 0, w: 229, h: 129 };
    const cards = [{ x: 74, y: 20, w: 200, h: 118 }, { x: 421, y: 20, w: 182, h: 65 }];
    const at = placeLabel(tile, { w: 190, h: 95 }, { x: 54, y: 0, w: 916, h: 515 }, cards);
    const b = { x: at.x, y: at.y, w: 190, h: 95 };
    for (const c of cards) expect(b.x < c.x + c.w && c.x < b.x + b.w && b.y < c.y + c.h && c.y < b.y + b.h).toBe(false);
  });

  it("prefers above the tile when that is free", () => {
    expect(placeLabel({ x: 400, y: 300, w: 300, h: 200 }, { w: 230, h: 92 }, area, [card])).toEqual({ x: 400, y: 202 });
  });
});

describe("streaming modes", () => {
  const aggs = [
    { id: "agg-fcvm", frame_url: "ws://example-host:2/frames/agg-fcvm", rect: { x: 0, y: 0, w: 0.5, h: 1 } },
    { id: "agg-ec2", frame_url: "ws://example-host:2/frames/agg-ec2", rect: { x: 0.5, y: 0, w: 0.5, h: 1 } },
  ];
  it("plans both modes; tiles take their state from the source they are drawn from", () => {
    let s = stateWith(fleet(2, 2, { aggregators: aggs }));
    s = reduce(s, { type: "ui", patch: { mode: "aggregated" } });
    const v = view(s, EPOCH);
    expect(v.plan.mode).toBe("aggregated");
    expect(v.plan.aggregated.map((a) => a.id)).toEqual(["a:agg-fcvm", "a:agg-ec2"]);
    expect(v.plan.aggregated[0]!.box).toEqual({ x: 0, y: 0, w: 800, h: 900 });
    expect(v.plan.direct).toHaveLength(8);
    expect(sourceIdFor(fleet(2, 2).tiles[5]!, "aggregated", aggs)).toBe("a:agg-ec2");
    // The drawer still shows direct until it can switch: the control says so.
    expect(v.controls!.mode.note).toBe("switching to aggregated");
    expect(view(reduce(s, { type: "ui", patch: { shownMode: "aggregated" } }), EPOCH).controls!.mode.note).toBe("");
  });
  it("stays direct when the fleet runs no aggregators", () => {
    const s = reduce(stateWith(fleet(2, 2)), { type: "ui", patch: { mode: "aggregated" } });
    const v = view(s, EPOCH);
    expect(v.plan.mode).toBe("direct");
    expect(v.controls!.mode.options.find((o) => o.value === "aggregated")!.disabled).toBe(true);
  });
  it("suggests aggregated when the link reports less than direct needs, or direct streams run late", () => {
    expect(suggestMode({ aggregators: 2, downlinkMbps: 4, directBps: 30e6, lateShare: null }).mode).toBe("aggregated");
    expect(suggestMode({ aggregators: 2, downlinkMbps: 10, directBps: 30e6, lateShare: null }).mode).toBe("direct");
    expect(suggestMode({ aggregators: 2, downlinkMbps: null, directBps: 30e6, lateShare: 0.25 }).mode).toBe("aggregated");
    expect(suggestMode({ aggregators: 0, downlinkMbps: 1, directBps: 30e6, lateShare: 0.9 }).mode).toBe("direct");
  });
});
