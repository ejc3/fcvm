import { describe, expect, it } from "vitest";
import * as fmt from "../format.ts";
import { initialState, makeIntent, reduce, type ClientState, type FrameStats } from "../store.ts";
import {
  BORROW_FADE_MS,
  TEXT_STYLES,
  wordArgs,
  REPOSITORY_URL,
  HUD_BAND,
  SEAM_HOLD_MS,
  TOAST_MS,
  isolationView,
  REDUCED_MOTION_DARK_FADE_MS,
  needsVerify,
  placeLabel,
  ZOOM_DIM_FAR,
  ZOOM_DIM_NEAR,
  zoomInsetsFor,
  familyLine,
  pointerTile,
  relaunchParts,
  sourceIdFor,
  wallView,
  type WallInput,
} from "../view.ts";
import { suggestMode } from "../mode.ts";
import { END_CARD_MS } from "../race.ts";
import { boxesMeet, DETAIL_TILE_PX, layoutMosaic, tileAt as tileAtPoint, tileGeometry, zoomTarget } from "../layout.ts";
import { familyProbeTargets } from "../net.ts";
import { parseReelMeta, reelView } from "../reel.ts";
import type { Aggregate, Claim, FleetState, Nudges, Power, RelaunchAll } from "../protocol.ts";
import { EPOCH, fleet, fleetSplit, snapshotText } from "./fixtures.ts";

function stateWith(st: FleetState, version = 1): ClientState {
  let s = reduce(initialState(), { type: "socket", status: "open", at: EPOCH });
  s = reduce(s, { type: "family", family: "v6", probeMs: 180, via: "race" });
  s = reduce(s, { type: "message", text: snapshotText(version, st), at: EPOCH });
  const tiles: FrameStats["tiles"] = {};
  for (const t of st.tiles) tiles[`d:${t.id}`] = { connected: true, fps: 24, newestT: 100, width: 320, height: 180, behind: false, decodeMs: 2, lag: null };
  return reduce(s, { type: "frame-stats", stats: { at: EPOCH, T: 100, tiles, drawMsP50: 1, drawMsP95: 2, rafMsP50: 16.7, rafMsP95: 17, allLive: true, live: [], mode: "direct" } });
}

function msg(s: ClientState, m: unknown, at = EPOCH): ClientState {
  return reduce(s, { type: "message", text: JSON.stringify(m), at });
}

function view(s: ClientState, now: number, over: Partial<WallInput> = {}) {
  return wallView({
    state: s,
    now,
    dpr: 1,
    mosaicArea: { x: 0, y: 0, w: 1600, h: 930 },
    canDecode: true,
    phone: false,
    ...over,
  });
}

function race(start: number, over: Partial<RelaunchAll> = {}): RelaunchAll {
  return {
    started_at: start,
    by: "v-3",
    fcvm_done_at: null,
    ec2_done_at: null,
    fcvm_pending: 4,
    ec2_pending: 4,
    fcvm_gpus: 4,
    ec2_gpus: 4,
    ec2_capacity_wait_ms: 0,
    ec2_waiting_since: null,
    fcvm_ms: null,
    ec2_ms: null,
    ...over,
  };
}

const SCORES = {
  fcvm: { median_ms: 3000, last_ms: 2900, races: 12, measured_on: 4, last_capacity_wait_ms: 0 },
  ec2: { median_ms: 50_000, last_ms: 49_000, races: 12, measured_on: 4, last_capacity_wait_ms: 21_000 },
};

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
  });
});

describe("the scoreboard at rest", () => {
  it("draws both medians on one axis with the last race marked, says what they were measured on, and keeps EC2's capacity wait apart", () => {
    const v = view(stateWith(fleet(2, 2, { scoreboard: SCORES, next_race_at: EPOCH + 61_000 })), EPOCH + 20_000);
    const sc = v.score!;
    expect(sc.racing).toBe(false);
    // Axis: EC2's median plus the wait drawn after it, 71 s.
    expect(sc.fcvm.solid).toBeCloseTo(3000 / 71_000, 6);
    expect(sc.ec2.solid).toBeCloseTo(50_000 / 71_000, 6);
    expect(sc.ec2.hatch).toBeCloseTo(21_000 / 71_000, 6);
    expect(sc.ec2.hatchFirst).toBe(false);
    expect(sc.fcvm.hatch).toBe(0);
    expect(sc.fcvm.lastMark).toBeCloseTo(2900 / 71_000, 6);
    expect(sc.fcvm).toMatchObject({ value: "3.0 s", valueLabel: "median of 12 restarts", line: "last restart 2.9 s", measured: "measured on 4 GPUs" });
    expect(sc.ec2).toMatchObject({ value: "50 s", hatchLabel: "last restart: AWS had no spot GPU for 21 s, not counted" });
    expect(sc.countdown).toMatchObject({ label: "auto in", value: "0:41", racing: false, show: true });
  });

  it("shows no number before the coordinator has measured one", () => {
    const sc = view(stateWith(fleet()), EPOCH).score!;
    expect(sc.fcvm).toMatchObject({ value: "--", line: "no restart measured yet", measured: "", solid: 0 });
    // Nothing scheduled: nothing shown, not a dash.
    expect(sc.countdown).toMatchObject({ show: false, note: "none scheduled" });
    // No race measured: the strip falls back to each half's newest single restart, never a dash.
    expect(view(stateWith(fleet()), EPOCH).lastRace).toBe("Last single restarts: fcvm VM 1 back in 2.8 s · EC2 VM 1 back in 58 s");
  });
});

describe("the race", () => {
  it("counts per half, freezes fcvm's bar once it is back while EC2's grows, and announces each half", () => {
    const start = EPOCH + 10_000;
    const st = fleet(2, 2, { scoreboard: SCORES, relaunch_all: race(start) });
    for (const t of st.tiles) {
      t.last_relaunch = { clicked_at: start, first_frame_at: null, phases: {} };
      t.phase = t.half === "fcvm" ? "restoring" : "launching";
    }
    let v = view(stateWith(st), start + 1500);
    expect(v.score!.racing).toBe(true);
    expect(v.score!.countdown).toMatchObject({ label: "Restart running", value: "0:01.5" });
    expect(v.score!.fcvm).toMatchObject({ value: "0:01.5", live: true, line: "0 of 4 GPUs streaming again" });
    expect(v.score!.button).toMatchObject({ label: "Restarting now", disabled: true });
    // No per-tile clocks: each dark tile shows how far its restart is.
    const tv = v.tiles.find((t) => t.id === "fcvm:1")!;
    expect(tv.dark).toBe(true);
    expect(tv.glyph).toEqual({ kind: "progress", steps: 4, done: 2 });
    expect(tv.phase).toBe("restoring the snapshot");
    expect(v.score!.announce).toBe("");

    const st2 = structuredClone(st);
    st2.relaunch_all = race(start, { fcvm_done_at: start + 2900, fcvm_pending: 0 });
    for (const t of st2.tiles.filter((x) => x.half === "fcvm")) {
      t.phase = "running";
      t.last_relaunch = { clicked_at: start, first_frame_at: start + 2900, phases: {} };
    }
    const a = view(stateWith(st2, 2), start + 20_000).score!;
    const b = view(stateWith(st2, 2), start + 40_000).score!;
    expect(a.fcvm).toMatchObject({ value: "2.9 s", done: true, line: "all 4 GPUs streaming again" });
    // fcvm's bar stays 2.9 s long (its share shrinks only because the axis grows past EC2's median).
    expect(a.fcvm.solid * 50_000 * 1.0).toBeCloseTo(2900 * (50_000 / Math.max(71_000, 20_000)), 0);
    expect(b.ec2.solid).toBeGreaterThan(a.ec2.solid);
    expect(b.fcvm.solid).toBeCloseTo(a.fcvm.solid, 9);
    expect(a.announce).toBe("fcvm: all 4 GPUs streaming again 2.9 s after the restart.");

    st2.relaunch_all = race(start, { fcvm_done_at: start + 2900, ec2_done_at: start + 61_000, fcvm_pending: 0, ec2_pending: 0, ec2_capacity_wait_ms: 12_000 });
    v = view(stateWith(st2, 3), start + 70_000);
    expect(v.score!.racing).toBe(false);
    expect(v.score!.announce).toBe(
      "fcvm: all 4 GPUs streaming again 2.9 s after the restart. EC2: all 4 GPUs streaming again 49 s after the restart, not counting 12 s when AWS had no spot GPU.",
    );
  });

  it("draws EC2's wait for spot capacity hatched, first, and keeps it out of the count", () => {
    const start = EPOCH + 10_000;
    const st = fleet(2, 2, { relaunch_all: race(start, { ec2_waiting_since: start + 500 }) });
    for (const t of st.tiles.filter((x) => x.half === "ec2")) t.phase = "waiting_capacity";
    const v = view(stateWith(st), start + 20_500);
    const e = v.score!.ec2;
    expect(e.hatchFirst).toBe(true);
    expect(e.value).toBe("0:00.5");
    expect(e.hatch / e.solid).toBeCloseTo(20_000 / 500, 6);
    expect(e.hatchLabel).toBe("AWS had no spot GPU: waiting 0:20, not counted");
    const tv = v.tiles.find((t) => t.id === "ec2:1")!;
    expect(tv).toMatchObject({ dark: true, hatch: true, phase: "AWS had no spot GPU" });
  });
});

describe("the one button", () => {
  it("names the GPUs it restarts, waits out the cooldown, and pauses while the coordinator is offline", () => {
    const st = fleet(2, 2, { cooldown_until: EPOCH + 23_000 });
    expect(view(stateWith(st), EPOCH).score!.button).toEqual({ label: "Restart all 8 GPUs", disabled: true, note: "available again in 0:23", hidden: false });
    expect(view(stateWith(st), EPOCH + 23_000).score!.button).toEqual({ label: "Restart all 8 GPUs", disabled: false, note: "", hidden: false });
    const off = reduce(stateWith(st), { type: "socket", status: "closed", at: EPOCH });
    const v = view(off, EPOCH + 30_000);
    expect(v.score!.button).toMatchObject({ disabled: true, note: "paused while the coordinator is offline" });
    expect(v.banners.map((b) => b.kind)).toEqual(["offline"]);
    expect(v.banners[0]!.text).toBe("The coordinator is offline. The picture still streams straight from the VMs; the controls are paused until it is back.");
    // The streams are still planned: the picture keeps playing.
    expect(v.plan.direct.length).toBe(8);
    expect(v.play!.disabled).toBe(true);
  });
});

describe("failure states", () => {
  it("labels a failed tile with its error and retry time, and says when AWS reclaimed a host", () => {
    const st = fleet(2, 2);
    const f = st.tiles.find((t) => t.id === "fcvm:2")!;
    f.phase = "failed";
    f.failure = { kind: "relaunch_failed", message: "GPU did not come back", retry_at: EPOCH + 12_000 };
    let v = view(stateWith(st), EPOCH);
    expect(v.tiles.find((t) => t.id === "fcvm:2")).toMatchObject({ dark: true, phase: "failed: GPU did not come back", detail: "retrying in 0:12", glyph: { kind: "failed" } });
    expect(v.banners.find((b) => b.kind === "reclaimed")).toBeUndefined();
    // The coordinator takes a reclaimed host's tiles out in the same event and keeps the reason in `failures`.
    const text = "AWS reclaimed an fcvm host. The image is re-tiled onto the GPUs left.";
    v = view(stateWith({ ...st, failures: [{ kind: "host_reclaimed", host: "example-host", since: EPOCH }] }), EPOCH);
    expect(v.banners.find((b) => b.kind === "reclaimed")!.text).toBe(text);
    // Or only the host marked draining.
    v = view(stateWith({ ...st, fleet: { ...st.fleet, hosts: [{ name: "example-host", draining: true }] } }), EPOCH);
    expect(v.banners.find((b) => b.kind === "reclaimed")!.text).toBe(text);
    // Two reported hosts, one of them also draining: counted once each.
    v = view(stateWith({ ...st, failures: [{ kind: "host_reclaimed", host: "a", since: EPOCH }, { kind: "host_reclaimed", host: "b", since: EPOCH }, { kind: "coordinator_degraded", host: null, since: EPOCH }], fleet: { ...st.fleet, hosts: [{ name: "a", draining: true }] } }), EPOCH);
    expect(v.banners.find((b) => b.kind === "reclaimed")!.text).toBe("AWS reclaimed 2 fcvm hosts. The image is re-tiled onto the GPUs left.");
  });

  it("draws the race on the picture: a clock per half, a clock per dark tile, and each relit tile's time", () => {
    const start = EPOCH + 10_000;
    const st = fleet(2, 2, { relaunch_all: race(start, { fcvm_done_at: start + 3100, fcvm_pending: 0 }) });
    for (const t of st.tiles) {
      if (t.half === "fcvm") {
        t.last_relaunch = { clicked_at: start, first_frame_at: start + (t.id === "fcvm:1" ? 1500 : 3100), phases: {} };
      } else {
        t.last_relaunch = { clicked_at: start, first_frame_at: null, phases: {} };
        t.phase = t.id === "ec2:1" ? "pending" : "starting";
      }
    }
    const v = view(stateWith(st), start + 5000);
    const [f, e] = v.race!.clocks;
    expect(f).toMatchObject({ half: "fcvm", value: "3.1 s", state: "done", sub: "all 4 GPUs back" });
    expect(e).toMatchObject({ half: "ec2", value: "0:05.0", state: "counting", sub: "+1 s and counting", hatch: "" });
    // EC2's slowest tile is still getting its instance.
    expect(e!.steps.map((x) => `${x.label}:${x.state}`)).toEqual(["instance:now", "OS:todo", "driver:todo", "app:todo"]);
    // Each clock sits low in its own half.
    const L = v.layout!;
    expect(f!.x).toBeLessThan(L.halfLineX!);
    expect(e!.x).toBeGreaterThan(L.halfLineX!);
    expect(f!.y).toBeGreaterThan(L.image.y + L.image.h / 2);
    // A dark tile counts; a relit one shows its own time, big for three seconds, then in its corner.
    const dark = v.tiles.find((t) => t.id === "ec2:2")!;
    expect(dark).toMatchObject({ dark: true, clock: "0:05.0", chip: null });
    expect(v.tiles.find((t) => t.id === "fcvm:1")!.chip).toEqual({ text: "1.5 s", fresh: false });
    expect(view(stateWith(st), start + 4000).tiles.find((t) => t.id === "fcvm:2")!.chip).toEqual({ text: "3.1 s", fresh: true });
    expect(v.race!.end).toBeNull();
    // AWS without a spot GPU: EC2's clock stops counting and says how long it did not count.
    const waiting = fleet(2, 2, { relaunch_all: race(start, { ec2_capacity_wait_ms: 21_000 }) });
    expect(view(stateWith(waiting), start + 30_000).race!.clocks[1]).toMatchObject({ value: "0:09.0", hatch: "not counting 21 s AWS spent finding a spot GPU" });
    // fcvm back while EC2's counted time is still under fcvm's: no "+0 s", just how many are back.
    const behind = fleet(2, 2, { relaunch_all: race(start, { fcvm_done_at: start + 3100, fcvm_pending: 0, ec2_capacity_wait_ms: 4000 }) });
    expect(view(stateWith(behind), start + 5000).race!.clocks[1]).toMatchObject({ value: "0:01.0", sub: "0 of 4 GPUs back" });
  });

  it("sizes each half's clock to its own half and keeps the whole card on the image", () => {
    const start = EPOCH + 10_000;
    const area = { mosaicArea: { x: 0, y: 0, w: 1600, h: 900 } };
    // Even halves on a 1600 px image: 9% of the width, 144 px, centred in each half.
    const even = view(stateWith(fleet(2, 2, { relaunch_all: race(start) })), start + 2000, area).race!.clocks;
    expect(even.map((c) => Math.round(c.size))).toEqual([144, 144]);
    expect(even[0]!.x).toBeCloseTo(400, 0);
    // One fcvm GPU against fifteen EC2: the fcvm half is 100 px. Its figure shrinks to the
    // floor and its card moves in from the left edge instead of hanging off it.
    const st = fleetSplit(1, 15, 1);
    st.relaunch_all = race(start, { fcvm_gpus: 1, fcvm_pending: 1, ec2_gpus: 15, ec2_pending: 15 });
    const v = view(stateWith(st), start + 2000, area);
    const L = v.layout!;
    const f = v.race!.clocks.find((c) => c.half === "fcvm")!;
    expect(L.halfLineX! - L.image.x).toBeCloseTo(100, 0);
    expect(f.size).toBeLessThan(56);
    for (const c of v.race!.clocks) {
      expect(c.x - c.width / 2, c.half).toBeGreaterThanOrEqual(L.image.x);
      expect(c.x + c.width / 2, c.half).toBeLessThanOrEqual(L.image.x + L.image.w);
    }
  });

  it("ends a race with one card in the coordinator's numbers, then hands over to the strip's line", () => {
    const start = EPOCH + 10_000;
    const st = fleet(2, 2, { relaunch_all: race(start, { fcvm_done_at: start + 3100, ec2_done_at: start + 49_000, fcvm_pending: 0, ec2_pending: 0, ec2_capacity_wait_ms: 4000 }), scoreboard: SCORES });
    const v = view(stateWith(st), start + 50_000);
    expect(v.race).toEqual({ label: "", clocks: [], end: "fcvm back in 3.1 s · EC2 back in 45 s · 15× faster" });
    expect(v.tiles.every((t) => t.chip === null && t.clock === "")).toBe(true);
    expect(view(stateWith(st), start + 49_000 + END_CARD_MS).race).toBeNull();
    // The coordinator leaves each tile's own wait out, so its time can be longer than the half's
    // wall time less the longest wait: a tile back at 60 s without waiting, another at 50 s after
    // a 30 s wait, is 60 s, not 30 s. The card, the announcement and the half's frozen clock say its number.
    const measured = fleet(2, 2, { relaunch_all: race(start, { fcvm_done_at: start + 3000, ec2_done_at: start + 60_000, fcvm_pending: 0, ec2_pending: 0, ec2_capacity_wait_ms: 30_000, fcvm_ms: 3000, ec2_ms: 60_000 }) });
    expect(view(stateWith(measured), start + 61_000).race!.end).toBe("fcvm back in 3.0 s · EC2 back in 60 s · 20× faster");
    expect(view(stateWith(measured), start + 61_000).score!.announce).toContain("EC2: all 4 GPUs streaming again 60 s after the restart");
    // Without relaunch_all's own figure, the matching race in races[] gives it.
    const kept = fleet(2, 2, {
      relaunch_all: race(start, { fcvm_done_at: start + 3000, ec2_done_at: start + 60_000, fcvm_pending: 0, ec2_pending: 0, ec2_capacity_wait_ms: 30_000 }),
      races: [{ id: 9, started_at: start, by: "v-3", fcvm: { done_at: start + 3000, time_ms: 3000, capacity_wait_ms: 0 }, ec2: { done_at: start + 60_000, time_ms: 60_000, capacity_wait_ms: 30_000 } }],
    });
    expect(view(stateWith(kept), start + 61_000).race!.end).toBe("fcvm back in 3.0 s · EC2 back in 60 s · 20× faster");
    // And a half that is back mid-race freezes at the coordinator's number.
    const half = fleet(2, 2, { relaunch_all: race(start, { fcvm_done_at: start + 3400, fcvm_pending: 0, fcvm_ms: 3100 }) });
    expect(view(stateWith(half), start + 5000).race!.clocks[0]).toMatchObject({ value: "3.1 s", state: "done" });
  });

  it("reads the last race from races[] when the scoreboard has none, then from single restarts", () => {
    const st = fleet(2, 2, {
      races: [
        { id: 3, started_at: EPOCH - 90_000, by: "schedule", fcvm: { done_at: EPOCH - 86_900, time_ms: 3100, capacity_wait_ms: 0 }, ec2: { done_at: EPOCH - 40_000, time_ms: 50_000, capacity_wait_ms: 0 } },
        { id: 4, started_at: EPOCH - 9000, by: "v-2", fcvm: { done_at: null, time_ms: null, capacity_wait_ms: 0 }, ec2: null },
      ],
    });
    expect(view(stateWith(st), EPOCH).lastRace).toBe("Last race: fcvm back in 3.1 s · EC2 back in 50 s · 16× faster");
    expect(view(stateWith(fleet(2, 2, { scoreboard: SCORES, races: st.races })), EPOCH).lastRace).toBe("Last race: fcvm back in 2.9 s · EC2 back in 49 s · 17× faster");
  });

  it("shows a place with no GPU behind it plainly, and leaves it out of the race", () => {
    const st = fleet(2, 2);
    const t = st.tiles.find((x) => x.id === "fcvm:4")!;
    t.phase = "waiting_capacity";
    t.slot = null;
    t.gpu = null;
    t.frame_url = null;
    const v = view(stateWith(st), EPOCH + 1000);
    expect(v.tiles.find((x) => x.id === "fcvm:4")).toMatchObject({ dark: true, hatch: false, phase: "no GPU here yet" });
    expect(v.score!.button.label).toBe("Restart all 7 GPUs");
    expect(v.plan.direct.find((x) => x.id === "d:fcvm:4")).toBeUndefined();
  });
});

describe("a borrowed tile", () => {
  it("leaves the image slowly as a visitor's private GPU, says when it returns, and sits out the race", () => {
    const start = EPOCH + 10_000;
    const st = fleet(2, 2, { relaunch_all: race(start, { fcvm_gpus: 3, fcvm_pending: 3 }), borrow: { cap: 2, lent: 1, next_return_at: start + 42_000 } });
    const t = st.tiles.find((x) => x.id === "fcvm:4")!;
    t.phase = "borrowed";
    t.borrowed = { until: start + 42_000 };
    const v = view(stateWith(st), start);
    const tv = v.tiles.find((x) => x.id === "fcvm:4")!;
    expect(tv).toMatchObject({ dark: true, slowFade: true, phase: "a visitor's private GPU", detail: "sitting out this race · back in 0:42", glyph: { kind: "borrowed" } });
    expect(v.plan.tiles.find((x) => x.id === "fcvm:4")!.slowFade).toBe(true);
    expect(v.score!.fcvm.line).toBe("0 of 3 GPUs streaming again · 1 sits out, lent to a visitor");
    expect(BORROW_FADE_MS).toBeGreaterThanOrEqual(1000);
  });

  it("shows why the button is off when every GPU we lend is out, and when to try again", () => {
    const v = view(stateWith(fleet(2, 2, { borrow: { cap: 2, lent: 2, next_return_at: EPOCH + 41_000 } })), EPOCH);
    expect(v.ownButton).toEqual({ label: "Get your own GPU", disabled: true, note: "all 2 GPUs we lend are out; try again in 0:41" });
    const free = view(stateWith(fleet(2, 2, { borrow: { cap: 2, lent: 1, next_return_at: EPOCH + 41_000 } })), EPOCH);
    expect(free.ownButton).toMatchObject({ disabled: false, note: "borrow one of the wall's GPUs for a minute (1 of 2 free)" });
  });

  it("labels the viewer's borrowed tile while it comes back, until it has streamed a while", () => {
    const st = fleet(2, 2);
    const t = st.tiles.find((x) => x.id === "fcvm:4")!;
    t.phase = "restoring";
    let s = reduce(stateWith(st), { type: "ui", patch: { returning: "fcvm:4" } });
    let v = view(s, EPOCH + 5000);
    expect(v.returning).toBe(true);
    expect(v.seams.label).toMatchObject({ id: "fcvm VM 4", last: "your borrowed GPU, coming back" });
    const st2 = structuredClone(st);
    const t2 = st2.tiles.find((x) => x.id === "fcvm:4")!;
    t2.phase = "running";
    t2.last_relaunch = { clicked_at: EPOCH + 5000, first_frame_at: EPOCH + 8000, phases: {} };
    s = msg(s, { type: "snapshot", version: 2, state: st2 });
    v = view(s, EPOCH + 9000);
    expect(v.seams.label).toMatchObject({ last: "your borrowed GPU is back" });
    expect(view(s, EPOCH + 13_000).returning).toBe(false);
  });

  it("stops labelling the borrowed tile when it is not coming back (its host was reclaimed)", () => {
    const st = fleet(2, 2);
    const t = st.tiles.find((x) => x.id === "fcvm:4")!;
    t.phase = "draining";
    t.failure = { kind: "host_reclaimed", message: "AWS reclaimed the spot host", retry_at: null };
    const s = reduce(stateWith(st), { type: "ui", patch: { returning: "fcvm:4" } });
    expect(view(s, EPOCH + 5000).returning).toBe(false);
  });
});

describe("your own GPU", () => {
  const claim = (over: Partial<Claim>): Claim => ({
    id: "claim-1",
    tile: "fcvm:4",
    phase: "restoring",
    frame_url: null,
    frame_url_v4: null,
    clicked_at: EPOCH + 1000,
    first_frame_at: null,
    expires_at: null,
    gpu: "Tesla T4",
    phases: {},
    camera: [{ t: 0, x: 0, y: 0, zoom: 1 }],
    ended: null,
    ...over,
  });

  it("times the click to the first frame, then counts the minute down, then says it is over", () => {
    let s = reduce(stateWith(fleet()), { type: "intent-sent", intent: makeIntent("own", "claim_gpu"), at: EPOCH + 1000 });
    let v = view(s, EPOCH + 1700, { ownArea: { x: 0, y: 0, w: 1600, h: 900 } });
    expect(v.own).toMatchObject({ status: "Asking for a GPU", timerRunning: true, timer: "0:00.7", countdown: "1:00" });
    expect(v.plan.image).toBeNull();
    s = msg(s, { type: "receipt", id: "own", outcome: "committed", version: 2 });
    s = msg(s, { type: "claim", claim: claim({}) });
    expect(view(s, EPOCH + 2500).own!.status).toBe("Restoring a VM for you");
    s = msg(s, { type: "claim", claim: claim({ phase: "running", frame_url: "ws://example-host:2/frames/claim-1", first_frame_at: EPOCH + 3900, expires_at: EPOCH + 63_900, phases: { restore_ms: 1800, gpu_ms: 600, first_frame_ms: 300, total_ms: 2900 } }) });
    v = view(s, EPOCH + 13_900, { ownArea: { x: 0, y: 0, w: 1600, h: 900 } });
    expect(v.own).toMatchObject({ status: "Your GPU is streaming", timerRunning: false, timer: "2.9 s", countdownLabel: "Back to the pool in", countdown: "0:50" });
    expect(v.plan.focus).toMatchObject({ id: "o:claim-1", box: { x: 0, y: 0, w: 1600, h: 900 } });
    // The whole image on one GPU: 768 x 432, the tallest stream height within MAX_TILE_PX.
    expect(v.plan.focus!.url).toBe("ws://example-host:2/frames/claim-1?h=432");
    s = msg(s, { type: "claim", claim: claim({ phase: "ended", first_frame_at: EPOCH + 3900, ended: "Your minute is up. The GPU went back to the pool." }) });
    v = view(s, EPOCH + 64_000);
    expect(v.own).toMatchObject({ ended: true, status: "Your minute is up. The GPU went back to the pool.", countdown: "" });
  });

  it("says plainly when the coordinator refuses", () => {
    let s = reduce(stateWith(fleet()), { type: "intent-sent", intent: makeIntent("own", "claim_gpu"), at: EPOCH });
    s = msg(s, { type: "receipt", id: "own", outcome: "refused", reason: "all 2 GPUs we lend are out; the next comes back in 41 s", version: 2 });
    const v = view(s, EPOCH);
    expect(v.own).toBeNull();
    expect(v.ownRefusal!.text).toBe("No GPU is free right now: all 2 GPUs we lend are out; the next comes back in 41 s. Try again in a minute.");
    expect(v.notices).toEqual([]);
  });

  it("ignores a claim message it did not ask for", () => {
    const s = msg(stateWith(fleet()), { type: "claim", claim: claim({}) });
    expect(s.own.claim).toBeNull();
  });
});

describe("Turnstile", () => {
  it("needs a check only when hello carries a site key, and again on every new connection", () => {
    let s = stateWith(fleet());
    expect(needsVerify(s)).toBe(false);
    s = msg(s, { type: "hello", viewer: "v-3", turnstile: "example-sitekey" });
    expect(needsVerify(s)).toBe(true);
    expect(view(s, EPOCH).play!.verify).toMatchObject({ needed: true });
    s = reduce(s, { type: "queue", action: { name: "restart_now", args: {} } });
    s = reduce(s, { type: "verify-sent" });
    expect(view(s, EPOCH).play!.verify.text).toBe("Checking");
    s = msg(s, { type: "verified", ok: false, reason: "the check did not pass" });
    expect(view(s, EPOCH).play!.verify.error).toBe("the check did not pass");
    s = msg(s, { type: "verified", ok: true });
    expect(needsVerify(s)).toBe(false);
    expect(s.turnstile.queued).toEqual([{ name: "restart_now", args: {} }]);
    s = msg(s, { type: "hello", viewer: "v-9", turnstile: "example-sitekey" });
    expect(needsVerify(s)).toBe(true);
  });
});

/** The directed camera holding on the ringed planet, the vote open for the next stop. */
function nudges(over: Partial<Nudges> = {}): Nudges {
  return {
    targets: [
      { id: "moon", label: "the moon", votes: 0, arrive_at: EPOCH + 20_000 },
      { id: "stars", label: "the star field", votes: 1, arrive_at: EPOCH + 12_000 },
    ],
    holding: "planet",
    holding_label: "the ringed planet",
    next_hold_at: EPOCH + 12_000,
    next: null,
    next_label: null,
    arrive_next_at: null,
    leading: "stars",
    opened_at: EPOCH - 1_000,
    closes_at: EPOCH + 5_000,
    ...over,
  };
}

/** The page after this viewer picked `target` and the coordinator answered. */
function picked(n: Nudges, target: string, receipt: Record<string, unknown> = { outcome: "committed" }): ClientState {
  let s = msg(stateWith(fleet(2, 2, { nudges: n })), { type: "hello", viewer: "v-3", turnstile: null });
  s = reduce(s, { type: "intent-sent", intent: makeIntent("n", "nudge", { target }), at: EPOCH });
  return msg(s, { type: "receipt", id: "n", version: 2, ...receipt });
}

describe("a stop choice says what the camera will do", () => {
  it("is next up when it leads the open vote", () => {
    expect(view(picked(nudges(), "stars"), EPOCH).notices).toEqual([{ key: "n", text: "Next up: your pick, the star field, in 0:12" }]);
  });

  it("comes after the hold already decided", () => {
    const n = nudges({ holding: null, next_hold_at: null, next: "planet", next_label: "the ringed planet", arrive_next_at: EPOCH + 9_000 });
    n.targets[1]!.arrive_at = EPOCH + 41_000;
    expect(view(picked(n, "stars"), EPOCH).notices[0]!.text).toBe("Your pick, the star field, comes after the ringed planet, in 0:41");
  });

  it("is next up when it is the hold just decided", () => {
    const n = nudges({ next: "stars", next_label: "the star field", arrive_next_at: EPOCH + 20_000 });
    expect(view(picked(n, "stars"), EPOCH).notices[0]!.text).toBe("Next up: the star field, in 0:20");
  });

  it("trails the leader until the vote closes", () => {
    const n = nudges({ leading: "moon" });
    n.targets[0]!.votes = 2;
    expect(view(picked(n, "stars"), EPOCH).notices[0]!.text).toBe(
      "Your vote for the star field is in; the moon leads 2 to 1. The vote closes in 0:05.",
    );
  });

  it("says where the camera goes when a pick is refused", () => {
    const s = picked(nudges(), "comet", { outcome: "refused", reason: "\"comet\" is not offered" });
    expect(view(s, EPOCH).notices[0]!.text).toBe("That stop is not offered now. Next up: the star field, in 0:12");
    const verify = picked(nudges(), "stars", { outcome: "refused", reason: "verify first" });
    expect(view(verify, EPOCH).notices[0]!.text).toBe("Your pick did not count: verify first. Next up: the star field, in 0:12");
  });

  it("dates a pick by when the coordinator applied it, not when its receipt arrived", () => {
    // Applied 100 ms before the vote it was in closed; the receipt came after the next one opened.
    const n = nudges({ next: "planet", next_label: "the ringed planet", arrive_next_at: EPOCH + 9_000, opened_at: EPOCH, leading: "moon" });
    let s = msg(stateWith(fleet(2, 2, { nudges: n, actions: [{ at: EPOCH - 100, viewer: "v-3", id: "n", name: "nudge", args: { target: "stars" }, summary: "nudge stars", outcome: "committed" }] })), { type: "hello", viewer: "v-3", turnstile: null });
    s = reduce(s, { type: "intent-sent", intent: makeIntent("n", "nudge", { target: "stars" }), at: EPOCH - 150 });
    s = msg(s, { type: "receipt", id: "n", version: 2, outcome: "committed" }, EPOCH);
    expect(view(s, EPOCH).notices[0]!.text).toBe("The vote went to the ringed planet. Pick again for the stop after it.");
  });

  it("speaks only for the latest pick", () => {
    let s = picked(nudges(), "moon");
    s = reduce(s, { type: "intent-sent", intent: makeIntent("n2", "nudge", { target: "stars" }), at: EPOCH + 1 });
    s = msg(s, { type: "receipt", id: "n2", version: 3, outcome: "committed" }, EPOCH + 1);
    expect(view(s, EPOCH + 1).notices).toEqual([{ key: "n2", text: "Next up: your pick, the star field, in 0:12" }]);
  });

  it("tells a pick to come back to the held stop from one already reached", () => {
    // The vote after the decided hold can offer the stop the camera is still holding.
    const n = nudges({ holding: "stars", holding_label: "the star field", next: "planet", next_label: "the ringed planet", arrive_next_at: EPOCH + 9_000 });
    n.targets[1]!.arrive_at = EPOCH + 41_000;
    expect(view(picked(n, "stars"), EPOCH).notices[0]!.text).toBe("Your pick, the star field, comes after the ringed planet, in 0:41");
  });

  it("says the camera is there once the pick is held", () => {
    const n = nudges({ holding: "stars", holding_label: "the star field" });
    n.targets = n.targets.filter((x) => x.id !== "stars");
    expect(view(picked(n, "stars"), EPOCH).notices[0]!.text).toBe("Your pick, the star field: the camera is there now");
  });
});

describe("the Play drawer", () => {
  it("reports a word that is being checked, then refused", () => {
    let s = stateWith(fleet());
    s = reduce(s, { type: "intent-sent", intent: makeIntent("w", "word", { text: "forbidden" }), at: EPOCH });
    expect(view(s, EPOCH).play!.word.status).toBe("Checking “forbidden”");
    s = msg(s, { type: "receipt", id: "w", outcome: "refused", reason: "the text check refused it", version: 1 });
    expect(view(s, EPOCH).play!.word).toMatchObject({ status: "“forbidden” was not added: the text check refused it", statusKind: "refused" });
  });

  it("lists the camera's next stops with votes and marks this viewer's choice", () => {
    const st = fleet(2, 2, {
      nudges: nudges({ targets: [{ id: "planet", label: "the ringed planet", votes: 0, arrive_at: null }, { id: "stars", label: "the star field", votes: 2, arrive_at: null }] }),
    });
    let s = msg(stateWith(st), { type: "hello", viewer: "v-3", turnstile: null });
    s = reduce(s, { type: "intent-sent", intent: makeIntent("n", "nudge", { target: "stars" }), at: EPOCH });
    const n = view(s, EPOCH).play!.nudge!;
    expect(n.note).toBe("holding on the ringed planet · next stop in 0:12");
    expect(n.options).toEqual([
      { id: "planet", label: "the ringed planet", votes: "0 votes", holding: true, chosen: false },
      { id: "stars", label: "the star field", votes: "2 votes", holding: false, chosen: true },
    ]);
  });
});

describe("tiles", () => {
  it("darkens a tile at once when this viewer restarts it", () => {
    let s = stateWith(fleet());
    s = reduce(s, { type: "intent-sent", intent: makeIntent("r", "relaunch", { tile: "fcvm:2" }), at: EPOCH });
    const v = view(s, EPOCH);
    expect(v.tiles.find((t) => t.id === "fcvm:2")).toMatchObject({ name: "fcvm VM 2", dark: true, phase: "restart sent" });
    expect(v.tiles.find((t) => t.id === "fcvm:1")!.dark).toBe(false);
  });

  it("fades a dark-out in when the viewer asks for less motion", () => {
    expect(view(stateWith(fleet()), EPOCH).plan.darkFadeMs).toBe(0);
    expect(view(stateWith(fleet()), EPOCH, { reducedMotion: true }).plan.darkFadeMs).toBe(REDUCED_MOTION_DARK_FADE_MS);
  });

  it("holds the picture when paused", () => {
    const s = reduce(stateWith(fleet()), { type: "ui", patch: { paused: true } });
    const v = view(s, EPOCH);
    expect(v.plan.paused).toBe(true);
    expect(v.pause).toEqual({ label: "Resume", on: true });
  });
});

describe("the stage", () => {
  it("fills the stage with the image, contain-fit and centred, and tags each half on the image", () => {
    const v = view(stateWith(fleet()), EPOCH);
    const img = v.layout!.image;
    // 1600 x 930: the full width, 900 tall, centred with 15 px over and under.
    expect(img).toEqual({ x: 0, y: 15, w: 1600, h: 900 });
    expect(v.labels.sides.map((l) => [l.half, l.tag, l.text, l.align])).toEqual([
      ["fcvm", "FCVM", "restored from a snapshot", "right"],
      ["ec2", "EC2", "launched from an AMI", "left"],
    ]);
    // On the image, beside the line between the halves, under the row of controls.
    for (const l of v.labels.sides) {
      expect(l.y).toBeGreaterThanOrEqual(HUD_BAND);
      expect(l.y).toBeLessThan(img.y + img.h / 4);
    }
    expect(v.labels.sides[0]!.x).toBeLessThan(v.layout!.halfLineX!);
    expect(v.labels.sides[1]!.x).toBeGreaterThan(v.layout!.halfLineX!);
    expect(v.sentence).toBe("Each square is its own virtual machine with its own NVIDIA GPU. Restart them all and watch which side comes back.");
  });

  it("shortens a half's tag to its name when that half is too narrow for the whole tag", () => {
    // 960 wide, one fcvm GPU against seven EC2: the fcvm half is 120 px.
    const v = view(stateWith(fleetSplit(1, 7, 1)), EPOCH, { mosaicArea: { x: 0, y: 0, w: 960, h: 540 } });
    const [f, e] = v.labels.sides;
    expect(v.layout!.halfLineX!).toBeLessThan(200);
    expect(f).toMatchObject({ half: "fcvm", tag: "FCVM", text: "" });
    expect(e).toMatchObject({ half: "ec2", tag: "EC2", text: "launched from an AMI" });
    // A phone held upright: both halves are under 320 px.
    const phone = view(stateWith(fleet()), EPOCH, { mosaicArea: { x: 0, y: 0, w: 390, h: 219 }, phone: true });
    expect(phone.labels.sides.map((x) => x.text)).toEqual(["", ""]);
  });

  it("says what draws the image in one line, from live numbers", () => {
    const v = view(stateWith(fleet()), EPOCH, { dpr: 2 });
    expect(v.spec).toBe("3200×1800 · 8 × NVIDIA T4 · 30 fps · path traced");
    expect(view(stateWith(fleet(2, 2, { scoreboard: SCORES })), EPOCH).lastRace).toBe("Last race: fcvm back in 2.9 s · EC2 back in 49 s · 17× faster");
  });

  it("puts the poster where the image goes before the first snapshot", () => {
    const v = view(initialState(), EPOCH, { poster: "http://example-host:1/poster.jpg" });
    expect(v.plan.poster).toBe("http://example-host:1/poster.jpg");
    const img = v.plan.image!;
    expect(img.w / img.h).toBeCloseTo(16 / 9, 6);
    // The whole stage, as the image will be once the snapshot comes.
    expect(img).toEqual({ x: 0, y: 15, w: 1600, h: 900 });
  });
});

describe("seams and hover", () => {
  it("shows boundaries while the pointer moves over the image, and outlines the hovered tile", () => {
    let s = stateWith(fleet());
    expect(view(s, EPOCH).seams.visible).toBe(false);
    s = reduce(s, { type: "ui", patch: { pointerOverImage: true, hoverId: "ec2:2", pointerMovedAt: EPOCH } });
    const v = view(s, EPOCH + 1000);
    expect(v.seams.visible).toBe(true);
    expect(v.seams.outline!.color).toBe("#F5A524");
    expect(v.seams.label).toMatchObject({ id: "EC2 VM 2", kind: "EC2", gpu: "NVIDIA T4", last: "last restart 58 s" });
    expect(v.seams.lineWidth).toBe(1);
    // Full-bleed the pointer is always over the image: once it rests, the seams and the outline go.
    const rest = view(s, EPOCH + SEAM_HOLD_MS);
    expect(rest.seams.visible).toBe(false);
    expect(rest.seams.outline).toBeNull();
    // A long press holds them for as long as it is held.
    expect(view(reduce(s, { type: "ui", patch: { longPress: true } }), EPOCH + 60_000).seams.visible).toBe(true);
  });

  describe("the hover card", () => {
    const label = (id: string, edit: (t: ReturnType<typeof fleet>["tiles"][number]) => void) => {
      const st = fleet();
      edit(st.tiles.find((t) => t.id === id)!);
      const s = reduce(stateWith(st), { type: "ui", patch: { pointerOverImage: true, hoverId: id, pointerMovedAt: EPOCH } });
      return view(s, EPOCH + 1000).seams.label!;
    };
    const lines = (l: { gpu: string; host: string; last: string }) => [l.gpu, l.host, l.last];

    it("names a live tile's GPU, place and last launch from what the coordinator reported", () => {
      const l = label("ec2:2", (t) => {
        t.gpu = { name: "Tesla T4", temp_c: 40, power_w: 50, util_pct: 90 };
        t.place = "EC2 in us-east-1";
        t.host = "i-example-host";
        t.last_relaunch = { clicked_at: EPOCH, first_frame_at: EPOCH + 58_000, phases: {} };
      });
      expect(lines(l)).toEqual(["Tesla T4", "EC2 in us-east-1", "last restart 58 s"]);
    });

    it("never shows the agent's own host name, which can be an instance id", () => {
      const l = label("fcvm:1", (t) => {
        t.host = "i-example-host";
        t.place = null;
      });
      expect(JSON.stringify(l)).not.toContain("i-example-host");
    });

    it("never says \"yet\" on a live tile, whatever the coordinator left out", () => {
      for (const id of ["fcvm:1", "ec2:2"]) {
        const l = label(id, (t) => {
          t.gpu = null;
          t.place = null;
          t.last_relaunch = null;
        });
        expect(lines(l).join(" | ")).not.toMatch(/\byet\b/);
        expect(lines(l)).toEqual(["GPU model not reported", "location not reported", "launch time not recorded"]);
      }
    });

    it("says what a tile that is not up is waiting for", () => {
      const dark = (id: string, phase: string) =>
        label(id, (t) => {
          t.phase = phase;
          t.gpu = null;
          t.place = null;
          t.last_relaunch = null;
          t.frame_url = null;
          t.frame_url_v4 = null;
        });
      expect(lines(dark("ec2:2", "waiting_capacity"))).toEqual(["waiting for EC2 capacity", "waiting for EC2 capacity", "no launch recorded"]);
      expect(lines(dark("ec2:2", "launching"))).toEqual(["waiting for the instance to launch", "waiting for the instance to launch", "no launch recorded"]);
      expect(lines(dark("fcvm:1", "waiting_capacity"))[0]).toBe("waiting for a free GPU");
      expect(lines(dark("fcvm:1", "relaunching"))[0]).toBe("waiting for the VM to restore");
    });
  });

  it("gives keyboard focus the same outline and label", () => {
    const s = reduce(stateWith(fleet()), { type: "ui", patch: { keyId: "fcvm:1" } });
    const v = view(s, EPOCH, { dpr: 2 });
    expect(v.seams.visible).toBe(true);
    expect(v.seams.outline!.color).toBe("#7CC4FF");
    expect(v.seams.lineWidth).toBe(0.5);
  });
});

describe("one VM opened", () => {
  const AREA = { x: 0, y: 0, w: 1600, h: 930 };
  const opened = (st: FleetState, id: string, over: Partial<WallInput> = {}) => view(reduce(stateWith(st), { type: "ui", patch: { focusId: id } }), EPOCH, over);

  it("zooms the image until that VM fills the stage at its own aspect, with every other tile in place around it", () => {
    const st = fleet(5, 5);
    const v = opened(st, "fcvm:13");
    const t = st.tiles.find((x) => x.id === "fcvm:13")!;
    const z = v.plan.zoom!;
    const target = zoomTarget(AREA, t.rect, 16 / 9, zoomInsetsFor(false, 0));
    expect(Math.abs(z.box!.h - target.h)).toBeLessThanOrEqual(1);
    expect(z.box!.w / z.box!.h).toBeCloseTo((t.rect.w * 16) / 9 / t.rect.h, 2);
    // The image is the zoomed one: the opened VM's rectangle of it is its box.
    const img = v.plan.image!;
    expect(Math.abs(img.x + t.rect.x * img.w - z.box!.x)).toBeLessThanOrEqual(1);
    expect(Math.abs(img.w * t.rect.w - z.box!.w)).toBeLessThanOrEqual(1);
    // The opened VM is drawn on its own over the mosaic; its mosaic entry streams undrawn.
    expect(z.source).toMatchObject({ id: "d:fcvm:13", box: z.box });
    expect(v.plan.direct.find((x) => x.id === "d:fcvm:13")!.box).toBeNull();
    expect(v.plan.focus).toBeNull();
    // Every tile on screen is drawn where its wall rectangle puts it in the zoomed image, the rest not at all.
    for (const d of v.plan.direct.filter((x) => x.id !== "d:fcvm:13")) {
      const r = st.tiles.find((x) => `d:${x.id}` === d.id)!.rect;
      const b = { x: img.x + r.x * img.w, y: img.y + r.y * img.h, w: r.w * img.w, h: r.h * img.h };
      if (boxesMeet(b, AREA)) {
        expect(d.box, d.id).not.toBeNull();
        expect(Math.abs(d.box!.x - b.x), d.id).toBeLessThanOrEqual(1);
        expect(Math.abs(d.box!.y - b.y), d.id).toBeLessThanOrEqual(1);
      } else expect(d.box, d.id).toBeNull();
    }
    const left = v.plan.direct.find((x) => x.id === "d:fcvm:12")!.box!;
    expect(left.x + left.w).toBe(z.box!.x);
    // Dimmed around it, fully once open; nothing of the wall's overlays is drawn over the zoom.
    expect([z.near, z.far]).toEqual([ZOOM_DIM_NEAR, ZOOM_DIM_FAR]);
    expect(v.zoom!.dim).toBe(1);
    expect(v.race).toBeNull();
    expect(v.seams.segments).toEqual([]);
    expect(v.focus!.placed!.box).toEqual(z.box);
  });

  it("asks that VM for the renderer's detail view, the dimmed tiles on screen for their wall height at q=lo, and the rest for the smallest", () => {
    const st = fleet(5, 5);
    const wall = view(stateWith(st), EPOCH, { dpr: 2 });
    const v = opened(st, "fcvm:13", { dpr: 2 });
    const t = st.tiles.find((x) => x.id === "fcvm:13")!;
    const z = v.plan.zoom!;
    // 810 CSS px tall on a 2x screen, a tile 0.89 as wide as tall: 1080, at most the detail view's pixels.
    expect(z.source!.height).toBe(1080);
    expect(z.source!.url).toBe("ws://example-host:1/frames/fcvm-13?h=1080");
    const g = tileGeometry(t.rect, 16 / 9, z.source!.height);
    expect(g.width * g.height).toBeLessThanOrEqual(DETAIL_TILE_PX);
    expect(v.focus!.placed!.streamHeight).toBe(1080);
    for (const d of v.plan.direct.filter((x) => x.id !== "d:fcvm:13")) {
      const onWall = wall.plan.direct.find((x) => x.id === d.id)!;
      if (d.box) {
        expect(d.height, d.id).toBe(onWall.height);
        expect(d.url, d.id).toContain("q=lo");
      } else expect(d.height, d.id).toBe(72);
    }
    // A 16:9 tile on a large 2x screen: 720, not 1080.
    const flat = fleet(1, 2);
    const fv = opened(flat, "fcvm:1", { dpr: 2, mosaicArea: { x: 0, y: 0, w: 2560, h: 1440 } });
    expect(fv.plan.zoom!.source!.height).toBe(720);
    // A small window asks only for what the box covers.
    const small = opened(st, "fcvm:13", { mosaicArea: { x: 0, y: 0, w: 800, h: 500 } });
    expect(small.plan.zoom!.source!.height).toBe(432);
  });

  it("keeps the opened VM at its detail height inside the bandwidth budget: the dimmed tiles give way", () => {
    const st = fleet(5, 5);
    const v = opened(st, "fcvm:13", { dpr: 2, budgetBps: 8_000_000 });
    expect(v.plan.zoom!.source!.height).toBe(1080);
    const shown = v.plan.direct.filter((x) => x.box !== null);
    expect(shown.length).toBeGreaterThan(0);
    for (const d of shown) expect(d.url, d.id).toContain("q=lo");
    const wall = view(stateWith(st), EPOCH, { dpr: 2 });
    expect(Math.max(...shown.map((d) => d.height))).toBeLessThan(Math.max(...wall.plan.direct.map((d) => d.height)));
  });

  it("follows the page's zoom step: half way in, half dimmed; closing, the wall's heights come back while it shrinks", () => {
    const st = fleet(5, 5);
    const s = reduce(stateWith(st), { type: "ui", patch: { focusId: "fcvm:13" } });
    const wallImage = layoutMosaic(AREA, st.tiles, 1).image;
    const full = view(s, EPOCH).plan.image!;
    const half = view(s, EPOCH, { zoom: { from: null, to: "fcvm:13", progress: 0.5 } });
    expect(half.plan.zoom!.near).toBeCloseTo(ZOOM_DIM_NEAR / 2, 9);
    expect(half.plan.image!.w).toBeGreaterThan(wallImage.w);
    expect(half.plan.image!.w).toBeLessThan(full.w);
    // While it opens, the streams already ask for where it ends.
    expect(half.plan.zoom!.source!.height).toBe(view(s, EPOCH).plan.zoom!.source!.height);
    const closed = reduce(s, { type: "ui", patch: { focusId: null } });
    const out = view(closed, EPOCH, { zoom: { from: "fcvm:13", to: null, progress: 0.5 } });
    expect(out.focus).toBeNull();
    expect(out.zoom!.focus!.id).toBe("fcvm:13");
    expect(out.plan.zoom!.box).toEqual(out.zoom!.focus!.box);
    const wall = view(closed, EPOCH);
    expect(out.plan.direct.map((d) => d.url)).toEqual(wall.plan.direct.map((d) => d.url));
    expect(wall.plan.zoom).toBeNull();
    expect(wall.plan.image).toEqual(wallImage);
  });

  it("shows its restart timeline and live GPU readings", () => {
    const v = opened(fleet(5, 5), "fcvm:7");
    expect(v.focus!.timeline.map((x) => [x.label, x.value])).toEqual([
      ["old VM stopped", "0.30 s"],
      ["snapshot restored", "0.40 s"],
      ["GPU reset and driver loaded", "1.20 s"],
      ["renderer started", "0.60 s"],
      ["first frame", "0.30 s"],
    ]);
    expect(v.focus!.timelineTotal).toBe("2.8 s");
    expect(v.focus!.readings.map((r) => [r.label, r.value])).toEqual([["Temperature", "61 °C"], ["Power", "70 W"], ["GPU busy", "98%"]]);
  });

  it("on a phone fills the viewport's width, not the 16:9 stage band", () => {
    const st = fleet(1, 2);
    const v = opened(st, "fcvm:1", { phone: true, mosaicArea: { x: 0, y: 0, w: 390, h: 219 }, zoomArea: { x: 0, y: 0, w: 390, h: 844 } });
    expect(v.plan.zoom!.box!.w).toBe(390);
    expect(v.plan.zoom!.box!.y).toBeGreaterThanOrEqual(56);
  });

  it("keeps GPU readings as a short history for the sparklines", () => {
    let s = stateWith(fleet());
    const st = fleet();
    st.tiles[0]!.gpu!.temp_c = 70;
    s = msg(s, { type: "snapshot", version: 2, state: st }, EPOCH + 2000);
    expect(s.gpuHistory["fcvm:1"]!.map((x) => x.temp_c)).toEqual([61, 70]);
    const r = view(reduce(s, { type: "ui", patch: { focusId: "fcvm:1" } }), EPOCH + 2000).focus!.readings[0]!;
    expect(r.points).toBe("0.0,22.0 100.0,2.0");
  });
});

describe("restart timings", () => {
  it("orders the parts, splits the GPU out of the restore, puts a capacity wait first and leaves out sums", () => {
    expect(relaunchParts({ container_ms: 600, first_frame_ms: 250, gpu_ms: 3100, restore_ms: 4300, teardown_ms: 310, total_ms: 5200, ack_ms: 4200 }).map(([, l, ms]) => [l, ms])).toEqual([
      ["old VM stopped", 310],
      ["snapshot restored", 1200],
      ["GPU reset and driver loaded", 3100],
      ["renderer started", 600],
      ["first frame", 250],
    ]);
    expect(relaunchParts({ launch_ms: 9000, capacity_wait_ms: 20_000 }).map(([k]) => k)).toEqual(["capacity_wait_ms", "launch_ms"]);
  });
});

describe("the activity feed", () => {
  it("names this viewer as you, others by number, and the owner's cadence (not an intent) from the race", () => {
    const actions = [
      { at: EPOCH + 1, viewer: "v-4", id: "a", name: "relaunch", args: { tile: "fcvm:3" }, summary: "relaunch slot-3", outcome: "committed" },
      { at: EPOCH + 2, viewer: "v-7", id: "b", name: "restart_now", args: {}, summary: "race 1", outcome: "committed" },
      { at: EPOCH + 3, viewer: "v-4", id: "c", name: "word", args: {}, summary: "the text check refused it", outcome: "refused" },
      { at: EPOCH + 5, viewer: "v-4", id: "e", name: "light", args: { pos: [0, 1, 2], color: [1, 1, 1] }, summary: "light", outcome: "committed" },
    ];
    const race = { started_at: EPOCH + 4, by: "schedule", fcvm_done_at: null, ec2_done_at: null, fcvm_pending: 2, ec2_pending: 2, fcvm_gpus: 2, ec2_gpus: 2, ec2_capacity_wait_ms: 0, ec2_waiting_since: null, fcvm_ms: null, ec2_ms: null };
    let s = stateWith(fleet(2, 2, { actions, relaunch_all: race }));
    s = msg(s, { type: "hello", viewer: "v-4" });
    const v = view(s, EPOCH + 5000);
    expect(v.feed.map((t) => t.text)).toEqual(["you dropped a light", "the scheduled restart of every GPU began", "viewer 7 restarted every GPU", "you restarted fcvm VM 3"]);
    // On the image, only the newest, and only for a few seconds.
    expect(view(s, EPOCH + 1000).toast).toMatchObject({ text: "you dropped a light" });
    expect(view(s, EPOCH + 5 + TOAST_MS).toast).toBeNull();
    // A race a viewer started is in `actions` already: no second line for it.
    const byViewer = view(msg(stateWith(fleet(2, 2, { actions, relaunch_all: { ...race, by: "viewer:v-7" } })), { type: "hello", viewer: "v-4" }), EPOCH + 5000);
    expect(byViewer.feed.map((t) => t.text)).toEqual(["you dropped a light", "viewer 7 restarted every GPU", "you restarted fcvm VM 3"]);
  });
});

describe("hover label placement", () => {
  const area = { x: 0, y: 0, w: 1440, h: 800 };
  const card = { x: 20, y: 20, w: 200, h: 125 };
  it("never covers a card when another place is free", () => {
    const tile = { x: 0, y: 200, w: 360, h: 200 };
    const at = placeLabel(tile, { w: 230, h: 92 }, area, [card]);
    const b = { x: at.x, y: at.y, w: 230, h: 92 };
    expect(b.x < card.x + card.w && card.x < b.x + b.w && b.y < card.y + card.h && card.y < b.y + b.h).toBe(false);
  });
  it("moves past a box that reaches below the tile it labels", () => {
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
    { id: "agg:example-host", half: "fcvm" as const, host: "example-host", frame_url: "ws://example-host:2/frames/agg-a", frame_url_v4: null, rect: { x: 0, y: 0, w: 0.5, h: 1 }, tiles: ["fcvm:1", "fcvm:2", "fcvm:3", "fcvm:4"] },
    { id: "agg:ec2", half: "ec2" as const, host: null, frame_url: "ws://example-host:2/frames/agg-ec2", frame_url_v4: null, rect: { x: 0.5, y: 0, w: 0.5, h: 1 }, tiles: ["ec2:1", "ec2:2", "ec2:3", "ec2:4"] },
  ];
  it("starts aggregated, plans both modes, and offers direct as every VM's own stream", () => {
    const s = stateWith(fleet(2, 2, { aggregates: aggs }));
    expect(s.ui.mode).toBe("aggregated");
    const v = view(s, EPOCH);
    expect(v.plan.mode).toBe("aggregated");
    expect(v.plan.aggregated.map((a) => a.id)).toEqual(["a:agg:example-host", "a:agg:ec2"]);
    expect(v.plan.direct).toHaveLength(8);
    expect(sourceIdFor(fleet(2, 2).tiles[5]!, "aggregated", aggs)).toBe("a:agg:ec2");
    expect(v.play!.streams).toMatchObject({ label: "See every VM's own stream", on: false, disabled: false });
    const d = view(reduce(s, { type: "ui", patch: { mode: "direct" } }), EPOCH);
    expect(d.play!.streams.on).toBe(true);
    expect(d.play!.streams.note).toMatch(/^switching\. 8 streams straight from the VMs/);
  });
  it("stays direct when the fleet runs no aggregates", () => {
    const v = view(stateWith(fleet(2, 2)), EPOCH);
    expect(v.plan.mode).toBe("direct");
    expect(v.play!.streams).toMatchObject({ on: true, disabled: true });
  });
  it("suggests aggregated when the link reports less than direct needs, or direct streams run late", () => {
    expect(suggestMode({ aggregators: 2, downlinkMbps: 4, directBps: 30e6, lateShare: null }).mode).toBe("aggregated");
    expect(suggestMode({ aggregators: 2, downlinkMbps: 10, directBps: 30e6, lateShare: null }).mode).toBe("direct");
    expect(suggestMode({ aggregators: 2, downlinkMbps: null, directBps: 30e6, lateShare: 0.25 }).mode).toBe("aggregated");
    expect(suggestMode({ aggregators: 0, downlinkMbps: 1, directBps: 30e6, lateShare: 0.9 }).mode).toBe("direct");
  });
});

describe("counters and isolation", () => {
  it("counts GPU VMs restored, and scrubs only once an agent has reported one", () => {
    expect(fmt.thousands(1284)).toBe("1,284");
    expect(fmt.thousands(1_234_567)).toBe("1,234,567");
    expect(fmt.thousands(12)).toBe("12");
    let v = view(stateWith(fleet()), EPOCH);
    expect(v.counter).toEqual({ label: "GPU VMs restored", value: "1,284", total: 1284, lastHour: "37 in the last hour", scrubs: "", wake: "" });
    v = view(stateWith(fleet(2, 2, { counters: { restores: 1284, restores_last_hour: 37, scrubs: 1201 } })), EPOCH);
    expect(v.counter!.scrubs).toBe("GPU memory scrubs: 1,201");
    expect(view(stateWith(fleet(2, 2, { counters: null })), EPOCH).counter).toBeNull();
  });

  it("says how a square is isolated, the scrub only once one was reported, with one link to the repository", () => {
    const off = isolationView(fleet());
    expect(off.sentences).toEqual([
      "Each square is its own virtual machine with its own Linux kernel.",
      "Its GPU is handed to that virtual machine alone by the hardware's IOMMU (VFIO passthrough), not shared through a proxy in the host kernel.",
      "A restart throws the old virtual machine away, restores a fresh one from a snapshot, and attaches the GPU as it restores.",
    ]);
    expect(off.link).toEqual({ label: "The fcvm repository", href: REPOSITORY_URL });
    expect(REPOSITORY_URL).toBe("https://github.com/ejc3/fcvm");
    const on = isolationView(fleet(2, 2, { counters: { restores: 1284, restores_last_hour: 37, scrubs: 1 } }));
    expect(on.sentences.at(-1)).toBe("GPU memory is cleared before a GPU moves to the next virtual machine.");
    expect(isolationView(null).sentences).toHaveLength(3);
  });
});

describe("asleep and waking", () => {
  const power = (p: Partial<Power>): Power => ({ state: "awake", asleep_since: null, woke_at: null, hosts: [], last_wake: null, wakes: [], ...p });
  // As the coordinator: asleep, nothing is placed; waking, tiles arrive as their GPUs join.
  const sleeping = (p: Partial<Power>, over: Partial<FleetState> = {}) => {
    const st = fleet(2, 2, over);
    st.fleet.power = power(p);
    if (st.fleet.power.state === "asleep") {
      st.tiles = [];
      st.aggregates = [];
    }
    return st;
  };

  it("formats dates and minutes the way the page shows them", () => {
    expect(fmt.utcDate(Date.UTC(2026, 9, 22, 23, 14, 59))).toBe("22 Oct 2026, 23:14 UTC");
    expect(fmt.utcDate(Date.UTC(2024, 1, 29, 0, 5))).toBe("29 Feb 2024, 00:05 UTC");
    expect(fmt.utcDate(Date.UTC(1999, 11, 31, 9, 0))).toBe("31 Dec 1999, 09:00 UTC");
    expect(fmt.minSec(127_900)).toBe("2:07");
  });

  it("asleep: no dark squares over the poster, the poster full size, the scores dated, the buttons waiting", () => {
    const st = sleeping(
      { state: "asleep", asleep_since: EPOCH - 600_000 },
      { scoreboard: SCORES, relaunch_all: race(Date.UTC(2026, 9, 22, 23, 14), { fcvm_done_at: 1, ec2_done_at: 2 }) },
    );
    const v = view(stateWith(st), EPOCH);
    expect(v.sleep).toEqual({ state: "asleep", title: "Asleep.", line: "The GPUs wake when someone watches.", clock: "", steps: [] });
    // With wakes measured: how many GPUs, and the median wake.
    const wakes = [81_000, 98_000, 120_000].map((ms, i) => ({ started_at: i * 1e6, awake_at: i * 1e6 + ms, gpus: 16 }));
    const st2 = sleeping({ state: "asleep", asleep_since: EPOCH - 600_000, wakes, last_wake: wakes[2]! });
    expect(view(stateWith(st2), EPOCH).sleep!.line).toBe("16 GPUs wake in about 1:38 when you stay.");
    expect(v.tiles.every((t) => !t.dark)).toBe(true);
    expect(v.plan.tiles.every((t) => !t.dark)).toBe(true);
    expect(v.plan.posterFull).toBe(true);
    expect(v.plan.direct).toEqual([]);
    expect(v.score!.title).toBe("Restart time: click to the last GPU's first frame · last measured 22 Oct 2026, 23:14 UTC");
    expect(v.score!.fcvm.value).toBe("3.0 s");
    // No tiles are placed, so no count: "Restart all 0 GPUs" would be wrong.
    expect(v.score!.button).toEqual({ label: "Restart all GPUs", disabled: true, note: "works once the GPUs are awake", hidden: false });
    expect(v.ownButton).toEqual({ label: "Get your own GPU", disabled: true, note: "works once the GPUs are awake" });
    expect(v.score!.countdown).toMatchObject({ label: "Restarts resume once the GPUs are awake", show: false });
  });

  it("waking: a clock since the wake began and each real phase as every host reaches it", () => {
    const start = EPOCH;
    const st = sleeping({
      state: "waking",
      woke_at: start,
      hosts: [
        { host: "h1", phases: [{ phase: "instance_pending", at: start + 300 }, { phase: "booted", at: start + 41_200 }] },
        { host: "h2", phases: [{ phase: "instance_pending", at: start + 500 }] },
      ],
    });
    const v = view(stateWith(st), start + 63_400);
    expect(v.sleep!.title).toBe("WAKING THE GPUs");
    expect(v.sleep!.clock).toBe("1:03.4");
    expect(v.sleep!.steps.map((x) => [x.label, x.state, x.value])).toEqual([
      ["instances requested", "done", "0:00.5"],
      ["hosts booted", "partial", "1 of 2 hosts"],
      ["snapshot store ready", "waiting", ""],
      ["agents connected", "waiting", ""],
      ["tiles rendering", "waiting", ""],
    ]);
    // No progress bar: nothing on the card that is not a reported phase.
    expect(Object.keys(v.sleep!)).toEqual(["state", "title", "line", "clock", "steps"]);
  });

  it("reads a fleet that does not scale to zero as awake, and shows the last wake once, as measured", () => {
    let s = stateWith(fleet());
    expect(s.accepted!.fleet.power.state).toBe("awake");
    expect(view(s, EPOCH).sleep).toBeNull();
    const st = fleet();
    st.fleet.power = power({ last_wake: { started_at: Date.UTC(2026, 9, 22, 23, 0), awake_at: Date.UTC(2026, 9, 22, 23, 2, 7, 600), gpus: 128 } });
    s = stateWith(st);
    expect(view(s, EPOCH).counter!.wake).toBe("Last wake: 128 GPUs from zero in 2:07 (22 Oct 2026, 23:00 UTC)");
  });
});

describe("the recording", () => {
  const meta = parseReelMeta({ recorded_at: Date.UTC(2026, 9, 21, 18, 30), duration_s: 42, measured: { gpus: 16, fcvm_ms: 3100, ec2_ms: 52_000 } });
  const base = { base: "/media", meta, power: "waking", reelOn: true, allLive: false, reducedMotion: false, saveData: false };

  it("plays the MP4 with the WebM as the alternative and captions, with a badge and caption from its metadata", () => {
    const r = reelView(base);
    expect(r.sources).toEqual([{ src: "/media/wall-reel.mp4", type: "video/mp4" }, { src: "/media/wall-reel.webm", type: "video/webm" }]);
    expect(r.track.src).toBe("/media/wall-reel.vtt");
    expect(r).toMatchObject({ show: true, leaving: false, autoplay: true });
    expect(r.badge).toBe("◉ RECORDING · 21 Oct 2026, 18:30 UTC");
    expect(r.caption).toBe("In this recording 16 GPUs restart: fcvm is back in 3.1 s, EC2 in 52 s.");
    expect(reelView({ ...base, power: "asleep" }).badge).toBe("◉ RECORDING · 21 Oct 2026, 18:30 UTC");
    // Once the live image is whole, the badge says so as the recording fades.
    expect(reelView({ ...base, power: "awake", allLive: true })).toMatchObject({ leaving: true, badge: "● LIVE" });
    expect(reelView({ ...base, base: "https://example-host/media/" }).sources[0]!.src).toBe("https://example-host/media/wall-reel.mp4");
  });

  it("does not start by itself with less motion or data saving, and says nothing it was not told", () => {
    expect(reelView({ ...base, reducedMotion: true })).toMatchObject({ autoplay: false, manualNote: "Not started by itself, as your device asks for less motion." });
    expect(reelView({ ...base, saveData: true })).toMatchObject({ autoplay: false, manualNote: "Not started by itself, as your browser asks to save data." });
    expect(reelView({ ...base, meta: null })).toMatchObject({ badge: "◉ RECORDING", caption: "" });
    expect(parseReelMeta({ duration_s: 4 })).toBeNull();
    expect(parseReelMeta("x")).toBeNull();
  });

  it("does not leave on a drawer report that predates a region the newest snapshot opened", () => {
    // The wake's last snapshot both says "awake" and gives the last region its frame URL;
    // the drawer's report, every 500 ms, still describes the image without it.
    const st = fleet();
    let s = stateWith(st);
    s = reduce(s, { type: "ui", patch: { reelOn: true, mode: "direct" } });
    const media = { media: { base: "/media", meta: null } };
    const v0 = view(s, EPOCH, media);
    const keys = v0.plan.direct.filter((x) => x.box !== null && x.url !== null).map((x) => `${x.id} ${x.baseUrl ?? ""}`);
    expect(keys.length).toBeGreaterThan(1);
    // The report carries the mode it describes, so the reader needs no other record of it.
    const report = (live: string[]) => reduce(s, { type: "frame-stats", stats: { ...s.frames!, allLive: true, live, mode: "direct" } });
    expect(view(report(keys.slice(1)), EPOCH, media).reel!.leaving).toBe(false);
    expect(view(report(keys), EPOCH, media).reel!.leaving).toBe(true);
  });

  it("leaves only once the fleet is awake and every region of the live image has a frame", () => {
    expect(reelView({ ...base, power: "awake", allLive: false })).toMatchObject({ show: true, leaving: false });
    expect(reelView({ ...base, power: "waking", allLive: true })).toMatchObject({ show: true, leaving: false });
    expect(reelView({ ...base, power: "awake", allLive: true })).toMatchObject({ show: true, leaving: true });
    expect(reelView({ ...base, power: "awake", reelOn: false }).show).toBe(false);
  });
});

describe("re-tiling: each side as wide as the GPUs it has", () => {
  const snap = (s: ClientState, version: number, st: FleetState) => msg(s, { type: "snapshot", version, state: st });
  const shownIds = (v: ReturnType<typeof view>) => v.tiles.map((t) => t.id).sort();
  const planIds = (v: ReturnType<typeof view>) => v.plan.direct.map((d) => d.id.slice(2)).sort();

  // A join and a leave on each side: [fcvm before, ec2 before, fcvm after, ec2 after].
  for (const [nf0, ne0, nf1, ne1, what] of [
    [4, 4, 5, 4, "a GPU joins the fcvm side"],
    [4, 4, 3, 4, "a GPU leaves the fcvm side"],
    [4, 4, 4, 5, "a GPU joins the EC2 side"],
    [4, 4, 4, 3, "a GPU leaves the EC2 side"],
  ] as const) {
    it(`${what}: the plan opens the new tiling at once, the overlays keep the drawn one until the drawer swaps`, () => {
      let s = stateWith(fleetSplit(nf0, ne0, 1));
      const before = view(s, EPOCH);
      expect(before.layout!.split).toBeCloseTo(nf0 / (nf0 + ne0), 9);
      s = snap(s, 2, fleetSplit(nf1, ne1, 2));
      let v = view(s, EPOCH);
      // The plan is generation 2 from the start, with its first frame.
      expect(v.plan.gen).toBe(2);
      expect(v.plan.atT).toBe(600 / 30);
      expect(planIds(v)).toEqual(fleetSplit(nf1, ne1, 2).tiles.map((t) => t.id).sort());
      // The overlays (seams, hover, keyboard grid, dark tiles) stay on generation 1 until it is drawn.
      expect(shownIds(v)).toEqual(shownIds(before));
      expect(v.layout!.split).toBeCloseTo(nf0 / (nf0 + ne0), 9);
      expect(v.tiles.find((t) => t.id === "fcvm:1")!.box).toEqual(before.tiles.find((t) => t.id === "fcvm:1")!.box);
      // A gen-shown for another generation changes nothing.
      expect(reduce(s, { type: "gen-shown", gen: 1 }).shownLayout!.gen).toBe(1);
      s = reduce(s, { type: "gen-shown", gen: 2 });
      v = view(s, EPOCH);
      expect(shownIds(v)).toEqual(planIds(v));
      expect(v.layout!.split).toBeCloseTo(nf1 / (nf1 + ne1), 9);
      expect(v.layout!.halfLineX).toBe(Math.round(v.layout!.image.x + v.layout!.image.w * (nf1 / (nf1 + ne1))));
      expect(v.seams.segments.length).toBeGreaterThan(0);
    });
  }

  it("the pointer targets the tile drawn there, not the one of a tiling still waiting for its streams", () => {
    const drawn = fleetSplit(8, 7, 1);
    let s = snap(stateWith(drawn), 2, fleetSplit(4, 7, 2));
    // (0.45, 0.5) is fcvm:7 in the drawn tiling and an EC2 tile in the waiting one.
    expect(tileAtPoint(drawn.tiles, 0.45, 0.5)!.id).toBe("fcvm:7");
    expect(tileAtPoint(fleetSplit(4, 7, 2).tiles, 0.45, 0.5)!.half).toBe("ec2");
    expect(pointerTile(s, true, 0.45, 0.5)!.id).toBe("fcvm:7");
    s = reduce(s, { type: "gen-shown", gen: 2 });
    expect(pointerTile(s, true, 0.45, 0.5)!.half).toBe("ec2");
    // A browser that cannot decode draws no frames, so its overlays and pointer follow the newest snapshot.
    expect(pointerTile(snap(stateWith(drawn), 2, fleetSplit(4, 7, 2)), false, 0.45, 0.5)!.half).toBe("ec2");
  });

  it("keeps the latest state of a tile that stays, on its drawn rectangle", () => {
    let s = stateWith(fleetSplit(4, 4, 1));
    const next = fleetSplit(5, 4, 2);
    next.tiles.find((t) => t.id === "fcvm:2")!.phase = "restoring";
    s = snap(s, 2, next);
    const v = view(s, EPOCH);
    const tv = v.tiles.find((t) => t.id === "fcvm:2")!;
    expect(tv.dark).toBe(true);
    expect(tv.box!.w).toBeCloseTo(v.layout!.image.w / 8, 0);
  });

  it("gives one side the full width when the other has no GPUs, and says that side is empty", () => {
    for (const [nf, ne, empty] of [[6, 0, "ec2"], [0, 6, "fcvm"]] as const) {
      const v = view(stateWith(fleetSplit(nf, ne, 1)), EPOCH);
      expect(v.layout!.halfLineX).toBeNull();
      expect(v.layout!.split).toBe(nf === 0 ? 0 : 1);
      const xs = v.tiles.map((t) => t.box!);
      expect(Math.min(...xs.map((b) => b.x))).toBeCloseTo(v.layout!.image.x, 0);
      expect(Math.max(...xs.map((b) => b.x + b.w))).toBeCloseTo(v.layout!.image.x + v.layout!.image.w, 0);
      expect(v.labels.empty).toMatchObject({ half: empty, text: empty === "ec2" ? "EC2: no GPUs live right now" : "fcvm: no GPUs live right now" });
      expect(v.labels.sides.map((c) => c.half)).toEqual([nf === 0 ? "ec2" : "fcvm"]);
    }
    // Waking, a side not placed yet is still coming: no "empty" label (it would sit on the recording's badge).
    const waking = fleetSplit(0, 6, 1);
    waking.fleet.power = { state: "waking", asleep_since: null, woke_at: EPOCH - 5000, hosts: [{ host: "h1", phases: [{ phase: "instance_pending", at: EPOCH - 5000 }] }], last_wake: null, wakes: [] };
    expect(view(stateWith(waking), EPOCH).labels.empty).toBeNull();
    // And with nothing placed (asleep), the halves stay even.
    const asleep = fleetSplit(0, 0, 1);
    asleep.fleet.power = { state: "asleep", asleep_since: EPOCH - 5000, woke_at: null, hosts: [], last_wake: null, wakes: [] };
    const av = view(stateWith(asleep), EPOCH);
    expect(av.layout!.split).toBe(0.5);
    expect(av.labels.empty).toBeNull();
    const both = view(stateWith(fleetSplit(10, 2, 1)), EPOCH);
    expect(both.labels.empty).toBeNull();
    expect(both.labels.sides.map((c) => c.half)).toEqual(["fcvm", "ec2"]);
  });

  it("a generation change mid-magnify: the opened VM's stream follows its new rectangle, and a VM that left closes", () => {
    const area = { mosaicArea: { x: 0, y: 0, w: 1400, h: 800 } };
    let s = reduce(stateWith(fleetSplit(4, 4, 1)), { type: "ui", patch: { focusId: "fcvm:2" } });
    let v = view(s, EPOCH, area);
    const r1 = fleetSplit(4, 4, 1).tiles.find((t) => t.id === "fcvm:2")!.rect;
    expect(v.plan.zoom!.box!.w / v.plan.zoom!.box!.h).toBeCloseTo((r1.w * 16) / 9 / r1.h, 1);
    s = snap(s, 2, fleetSplit(2, 4, 2));
    v = view(s, EPOCH, area);
    const r2 = fleetSplit(2, 4, 2).tiles.find((t) => t.id === "fcvm:2")!.rect;
    expect(v.focus!.title).toBe("fcvm VM 2");
    expect(v.plan.gen).toBe(2);
    expect(v.plan.zoom!.box!.w / v.plan.zoom!.box!.h).toBeCloseTo((r2.w * 16) / 9 / r2.h, 1);
    // The VM leaves in the next generation: nothing is opened any more.
    s = snap(s, 3, fleetSplit(1, 4, 3));
    v = view(s, EPOCH, area);
    expect(v.focus).toBeNull();
    expect(v.plan.zoom).toBeNull();
  });

  it("Aggregated mode takes the new regions with the new generation", () => {
    const agg = (st: FleetState) => [
      { id: "agg:example-host", half: "fcvm" as const, host: "example-host", rect: { x: 0, y: 0, w: st.tiles.filter((t) => t.half === "fcvm").reduce((a, t) => Math.max(a, t.rect.x + t.rect.w), 0), h: 1 }, frame_url: "ws://example-host:2/frames/agg-a", frame_url_v4: null, tiles: st.tiles.filter((t) => t.half === "fcvm").map((t) => t.id) },
    ];
    const g1 = fleetSplit(4, 4, 1);
    g1.aggregates = agg(g1);
    const g2 = fleetSplit(6, 2, 2);
    g2.aggregates = agg(g2);
    let s = reduce(stateWith(g1), { type: "ui", patch: { mode: "aggregated" } });
    s = snap(s, 2, g2);
    const v = view(s, EPOCH);
    expect(v.plan.aggregated[0]!.box!.w / v.layout!.image.w).toBeCloseTo(0.75, 2);
  });
});

describe("the style of a message", () => {
  it("offers Lockup, Skywriting and Landmark with thumbnails, Lockup picked until the viewer picks another", () => {
    let s = stateWith(fleet());
    let st = view(s, EPOCH).play!.word.style;
    expect(st.options.map((o) => [o.value, o.label, o.selected])).toEqual([
      ["lockup", "Lockup", true],
      ["skywriting", "Skywriting", false],
      ["landmark", "Landmark", false],
    ]);
    expect(st.options.every((o) => o.thumb.startsWith("/text-styles/") && o.alt.length > 0)).toBe(true);
    s = reduce(s, { type: "ui", patch: { textStyle: "skywriting" } });
    st = view(s, EPOCH).play!.word.style;
    expect(st.options.filter((o) => o.selected).map((o) => o.value)).toEqual(["skywriting"]);
  });

  it("puts the choice in the word intent", () => {
    for (const { value } of TEXT_STYLES) {
      const intent = makeIntent("w", "word", wordArgs("hello", value));
      expect(intent.args).toEqual({ text: "hello", text_style: value });
    }
    expect(JSON.parse(JSON.stringify(makeIntent("w", "word", wordArgs("hi", "landmark"))))).toEqual({ type: "intent", id: "w", name: "word", args: { text: "hi", text_style: "landmark" } });
  });
});

describe("one address family for every tile", () => {
  const hostOf = (u: string) => new URL(u).hostname;
  const withFamily = (st: FleetState, family: "v6" | "v4" | null) => {
    let s = stateWith(st);
    if (family === null) s = { ...s, net: { family: null, probeMs: null, via: null } };
    else s = { ...s, net: { family, probeMs: 180, via: "race" } };
    return reduce(s, { type: "ui", patch: { mode: "direct" } });
  };

  it("streams both halves over IPv4 when the probe chose IPv4, and over IPv6 when it chose IPv6", () => {
    for (const [family, host] of [["v4", "example-host-v4"], ["v6", "example-host"]] as const) {
      const v = view(withFamily(fleet(2, 2), family), EPOCH);
      expect(v.plan.direct.length).toBe(8);
      for (const half of ["fcvm", "ec2"]) {
        const urls = v.plan.direct.filter((d) => d.id.startsWith(`d:${half}:`)).map((d) => d.url!);
        expect(urls.length, half).toBe(4);
        expect(new Set(urls.map(hostOf)), `${family} ${half}`).toEqual(new Set([host]));
      }
    }
  });

  it("opens no tile stream until the probe answers, and does not paint the tiles as waiting meanwhile", () => {
    const v = view(withFamily(fleet(2, 2), null), EPOCH);
    expect(v.plan.direct).toEqual([]);
    expect(v.tiles.filter((t) => t.dark)).toEqual([]);
  });

  it("a tile whose URLs are null shows no stream and does not hold the others", () => {
    const st = fleet(2, 2);
    st.tiles[1]!.frame_url = null;
    st.tiles[1]!.frame_url_v4 = null;
    for (const family of ["v6", "v4"] as const) {
      const v = view(withFamily(st, family), EPOCH);
      expect(v.plan.direct.map((d) => d.id)).not.toContain(`d:${st.tiles[1]!.id}`);
      expect(v.plan.direct.length).toBe(7);
    }
  });

  it("says in the stats drawer which family is in use", () => {
    expect(familyLine({ family: null, probeMs: null, via: null })).toBe("Address family: checking");
    expect(familyLine({ family: "v6", probeMs: 183.4, via: "race" })).toBe("Address family: IPv6 for every tile (connected first, in 183 ms)");
    expect(familyLine({ family: "v4", probeMs: 431.2, via: "race" })).toBe("Address family: IPv4 for every tile (connected first, in 431 ms)");
    expect(familyLine({ family: "v6", probeMs: null, via: "one-family" })).toBe("Address family: IPv6 for every tile (the tiles have no IPv4 address)");
    expect(familyLine({ family: "v4", probeMs: null, via: "one-family" })).toBe("Address family: IPv4 for every tile (the tiles have no IPv6 address)");
    expect(familyLine({ family: "v4", probeMs: null, via: "no-answer" })).toBe("Address family: IPv4 for every tile (neither family connected in 3 tries of 1.5 s)");
  });
});

describe("the family probe's targets", () => {
  const tile = (phase: string, v6: string | null, v4: string | null) => ({ phase, frame_url: v6, frame_url_v4: v4 });

  it("tries up to three running tiles' URLs in each family, skipping tiles without one", () => {
    const tiles = [tile("running", "wss://a/frames", "wss://a4/frames"), tile("restoring", "wss://b/frames", null), tile("running", null, null), tile("running", "wss://c/frames", "wss://c4/frames"), tile("running", "wss://d/frames", null), tile("running", "wss://e/frames", null)];
    expect(familyProbeTargets(tiles)).toEqual({ urls: ["wss://a/frames", "wss://c/frames", "wss://d/frames"], v4Urls: ["wss://a4/frames", "wss://c4/frames"], v4Only: false, v6Only: false });
  });

  it("waits while no tile has an address, and settles without a probe when only one family has URLs", () => {
    expect(familyProbeTargets([tile("running", null, null)])).toEqual({ urls: [], v4Urls: [], v4Only: false, v6Only: false });
    expect(familyProbeTargets([tile("running", null, "wss://a4/frames")])).toEqual({ urls: [], v4Urls: ["wss://a4/frames"], v4Only: true, v6Only: false });
    // A coordinator that publishes no IPv4 names (staging before #1229's DNS): IPv6 is the only family.
    expect(familyProbeTargets([tile("running", "wss://a/frames", null), tile("running", "wss://b/frames", null)])).toEqual({ urls: ["wss://a/frames", "wss://b/frames"], v4Urls: [], v4Only: false, v6Only: true });
  });

  it("is chosen once per page load", () => {
    let s = initialState();
    s = reduce(s, { type: "family", family: "v6", probeMs: 180, via: "race" });
    expect(reduce(s, { type: "family", family: "v4", probeMs: null, via: "no-answer" }).net).toEqual({ family: "v6", probeMs: 180, via: "race" });
  });
});

describe("the family choice covers every stream the page opens", () => {
  const agg = (v6: string | null, v4: string | null): Aggregate => ({ id: "agg:example-host", half: "fcvm", host: "example-host", rect: { x: 0, y: 0, w: 0.5, h: 1 }, frame_url: v6, frame_url_v4: v4, tiles: ["fcvm:1", "fcvm:2", "fcvm:3", "fcvm:4"] });
  const at = (aggregates: Aggregate[], family: "v6" | "v4") => {
    const s = stateWith(fleet(2, 2, { aggregates }));
    return reduce({ ...s, net: { family, probeMs: 180, via: "race" } }, { type: "ui", patch: { mode: "aggregated" } });
  };

  it("keeps an IPv4 viewer direct when the aggregates serve only IPv6", () => {
    const v = view(at([agg("ws://example-host:2/frames/agg-a", null)], "v4"), EPOCH);
    expect(v.plan.aggregated).toEqual([]);
    expect(v.plan.mode).toBe("direct");
    expect(v.plan.direct.length).toBe(8);
    expect(new Set(v.plan.direct.map((d) => new URL(d.url!).hostname))).toEqual(new Set(["example-host-v4"]));
  });

  it("stays direct when one serving aggregate has no URL in the viewer's family, so no half goes undrawn", () => {
    const other: Aggregate = { ...agg("ws://example-host:2/frames/agg-ec2", null), id: "agg:ec2", half: "ec2", host: null, rect: { x: 0.5, y: 0, w: 0.5, h: 1 }, tiles: ["ec2:1", "ec2:2", "ec2:3", "ec2:4"] };
    const v = view(at([agg("ws://example-host:2/frames/agg-a", "ws://example-host-v4:2/frames/agg-a"), other], "v4"), EPOCH);
    expect(v.plan.mode).toBe("direct");
    // An aggregate that is not serving yet (no URL in either family, as during a wake) does not count against it.
    const waking: Aggregate = { ...other, frame_url: null, frame_url_v4: null };
    expect(view(at([agg("ws://example-host:2/frames/agg-a", "ws://example-host-v4:2/frames/agg-a"), waking], "v4"), EPOCH).plan.mode).toBe("aggregated");
  });

  it("streams an aggregate over the viewer's family when it has a URL in it", () => {
    const v4 = view(at([agg("ws://example-host:2/frames/agg-a", "ws://example-host-v4:2/frames/agg-a")], "v4"), EPOCH);
    expect(v4.plan.mode).toBe("aggregated");
    expect(v4.plan.aggregated.map((a) => new URL(a.url!).hostname)).toEqual(["example-host-v4"]);
    const v6 = view(at([agg("ws://example-host:2/frames/agg-a", "ws://example-host-v4:2/frames/agg-a")], "v6"), EPOCH);
    expect(v6.plan.aggregated.map((a) => new URL(a.url!).hostname)).toEqual(["example-host"]);
  });

  it("opens a borrowed GPU's stream over the viewer's family, and none when it has no URL in it", () => {
    const claimMsg = (v4: string | null) => ({ type: "claim", claim: { id: "claim-1", tile: "fcvm:4", phase: "running", frame_url: "ws://example-host:2/frames/claim-1", frame_url_v4: v4, clicked_at: EPOCH, first_frame_at: EPOCH + 2900, expires_at: EPOCH + 62_900, gpu: "Tesla T4", phases: {}, camera: [], ended: null } });
    const own = (family: "v6" | "v4", v4: string | null) => {
      let s = stateWith(fleet(2, 2));
      s = { ...s, net: { family, probeMs: 180, via: "race" } };
      s = reduce(s, { type: "intent-sent", intent: makeIntent("own", "claim_gpu"), at: EPOCH });
      s = msg(s, claimMsg(v4));
      return view(s, EPOCH, { ownArea: { x: 0, y: 0, w: 1600, h: 900 } }).plan.focus;
    };
    expect(new URL(own("v4", "ws://example-host-v4:2/frames/claim-1")!.url!).hostname).toBe("example-host-v4");
    expect(new URL(own("v6", "ws://example-host-v4:2/frames/claim-1")!.url!).hostname).toBe("example-host");
    expect(own("v4", null)).toBeNull();
  });
});

describe("frame age on a tile's card", () => {
  // The drawer reports every 500 ms; the card is drawn every 100 ms.
  function reported(lag: FrameStats["tiles"][string]["lag"], at: number, newestT: number): ClientState {
    const st = fleet(1, 1);
    let s = stateWith(st);
    const tiles: FrameStats["tiles"] = {};
    for (const t of st.tiles) tiles[`d:${t.id}`] = { connected: true, fps: 30, newestT, width: 320, height: 180, behind: false, decodeMs: 2, lag };
    s = reduce(s, { type: "frame-stats", stats: { at, T: newestT, tiles, drawMsP50: 1, drawMsP95: 2, rafMsP50: 16.7, rafMsP95: 17, allLive: true, live: [], mode: "direct" } });
    return s;
  }

  it("is the newest frame's age when the drawer reported it, not that age plus how old the report is", () => {
    // Newest frame at t = 100 s, reported 80 ms after it; the card is drawn 400 ms after the report.
    const at = EPOCH + 100_080;
    const v = view(reported(null, at, 100), at + 400);
    expect(v.tiles[0]!.stats[1]).toBe("frame age 80 ms");
  });

  it("is the drawn age when the drawer has drawn frames, with where it went", () => {
    const at = EPOCH + 100_080;
    // Local minus scene ms, as core/lag.ts records them (the clock offset is 0 here).
    const lag = { arrive: EPOCH + 55, decoded: EPOCH + 58, drawn: EPOCH + 90, n: 60 };
    const v = view(reported(lag, at, 100), at + 400);
    expect(v.tiles[0]!.stats[1]).toBe("frame age 90 ms");
    expect(v.tiles[0]!.stats[2]).toBe("arrives 55 ms · decoded 58 ms · drawn 90 ms");
  });
});
