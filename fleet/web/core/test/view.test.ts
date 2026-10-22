import { describe, expect, it } from "vitest";
import * as fmt from "../format.ts";
import { initialState, makeIntent, reduce, type ClientState, type FrameStats } from "../store.ts";
import {
  BORROW_FADE_MS,
  REDUCED_MOTION_DARK_FADE_MS,
  needsVerify,
  nativePlacement,
  placeLabel,
  relaunchParts,
  sourceIdFor,
  wallView,
  type WallInput,
} from "../view.ts";
import { suggestMode } from "../mode.ts";
import type { Claim, FleetState, RelaunchAll } from "../protocol.ts";
import { EPOCH, fleet, snapshotText } from "./fixtures.ts";

function stateWith(st: FleetState, version = 1): ClientState {
  let s = reduce(initialState(), { type: "socket", status: "open", at: EPOCH });
  s = reduce(s, { type: "message", text: snapshotText(version, st), at: EPOCH });
  const tiles: FrameStats["tiles"] = {};
  for (const t of st.tiles) tiles[`d:${t.id}`] = { connected: true, fps: 24, newestT: 100, width: 320, height: 180, behind: false, decodeMs: 2 };
  return reduce(s, { type: "frame-stats", stats: { T: 100, tiles, drawMsP50: 1, drawMsP95: 2, rafMsP50: 16.7, rafMsP95: 17 } });
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
    focusArea: null,
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
    expect(sc.countdown).toMatchObject({ label: "Next restart in", value: "0:41", racing: false });
  });

  it("shows no number before the coordinator has measured one", () => {
    const sc = view(stateWith(fleet()), EPOCH).score!;
    expect(sc.fcvm).toMatchObject({ value: "--", line: "no restart measured yet", measured: "", solid: 0 });
    expect(sc.countdown).toMatchObject({ value: "--", note: "none scheduled" });
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
    expect(view(stateWith(st), EPOCH).score!.button).toEqual({ label: "Restart all 8 GPUs", disabled: true, note: "available again in 0:23" });
    expect(view(stateWith(st), EPOCH + 23_000).score!.button).toEqual({ label: "Restart all 8 GPUs", disabled: false, note: "" });
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
  it("labels a failed tile with its error and retry time, and a reclaimed host's tiles", () => {
    const st = fleet(2, 2);
    const f = st.tiles.find((t) => t.id === "fcvm:2")!;
    f.phase = "failed";
    f.failure = { kind: "relaunch_failed", message: "GPU did not come back", retry_at: EPOCH + 12_000 };
    const r = st.tiles.find((t) => t.id === "fcvm:3")!;
    r.phase = "draining";
    r.failure = { kind: "host_reclaimed", message: "AWS reclaimed the spot host", retry_at: null };
    st.fleet.hosts = [{ name: "example-host", draining: true }];
    const v = view(stateWith(st), EPOCH);
    expect(v.tiles.find((t) => t.id === "fcvm:2")).toMatchObject({ dark: true, phase: "failed: GPU did not come back", detail: "retrying in 0:12", glyph: { kind: "failed" } });
    expect(v.tiles.find((t) => t.id === "fcvm:3")).toMatchObject({ dark: true, phase: "host reclaimed by AWS", detail: "re-tiling", glyph: { kind: "reclaimed" } });
    expect(v.banners.find((b) => b.kind === "reclaimed")!.text).toBe("AWS reclaimed an fcvm host. Its squares stay dark while the image is re-tiled onto the GPUs left.");
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
    expect(v.plan.focus!.url).toBe("ws://example-host:2/frames/claim-1?h=720");
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
    s = reduce(s, { type: "queue", action: { name: "relaunch_all", args: {} } });
    s = reduce(s, { type: "verify-sent" });
    expect(view(s, EPOCH).play!.verify.text).toBe("Checking");
    s = msg(s, { type: "verified", ok: false, reason: "the check did not pass" });
    expect(view(s, EPOCH).play!.verify.error).toBe("the check did not pass");
    s = msg(s, { type: "verified", ok: true });
    expect(needsVerify(s)).toBe(false);
    expect(s.turnstile.queued).toEqual([{ name: "relaunch_all", args: {} }]);
    s = msg(s, { type: "hello", viewer: "v-9", turnstile: "example-sitekey" });
    expect(needsVerify(s)).toBe(true);
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
      nudges: { targets: [{ id: "planet", label: "the ringed planet", votes: 0 }, { id: "stars", label: "the star field", votes: 2 }], holding: "planet", next_hold_at: EPOCH + 12_000 },
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
  it("keeps a band under the image for one caption per half, and labels the seam", () => {
    const v = view(stateWith(fleet()), EPOCH);
    const img = v.layout!.image;
    expect(img.y + img.h).toBeLessThanOrEqual(900);
    expect(v.labels.captions.map((c) => c.text)).toEqual([
      "a fresh VM restored from a snapshot on a running host, GPU reset",
      "a fresh EC2 instance launched from an AMI",
    ]);
    expect(v.labels.captions[0]!.box.y).toBeGreaterThan(img.y + img.h);
    expect(v.labels.seam.map((l) => [l.text, l.align])).toEqual([["fcvm", "right"], ["EC2", "left"]]);
    expect(v.sentence).toBe("Each square is its own virtual machine with its own NVIDIA GPU. Restart them all and watch which side comes back.");
  });

  it("puts the poster where the image goes before the first snapshot", () => {
    const v = view(initialState(), EPOCH, { poster: "http://example-host:1/poster.jpg" });
    expect(v.plan.poster).toBe("http://example-host:1/poster.jpg");
    const img = v.plan.image!;
    expect(img.w / img.h).toBeCloseTo(16 / 9, 6);
    // Above the band the captions take, as the image will be once the snapshot comes.
    expect(img.y + img.h).toBeLessThanOrEqual(930 - 44);
    expect(img.h).toBeGreaterThan(800);
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
    expect(v.seams.label).toMatchObject({ id: "EC2 VM 2", kind: "EC2", gpu: "NVIDIA T4", last: "last restart 58 s" });
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

describe("one VM opened", () => {
  it("asks that VM alone for its native-size stream, at one device pixel per pixel", () => {
    const st = fleet(5, 5);
    const s = reduce(stateWith(st), { type: "ui", patch: { focusId: "fcvm:7" } });
    const v = view(s, EPOCH, { focusArea: { x: 0, y: 0, w: 1400, h: 800 } });
    const t = v.plan.direct.find((x) => x.id === "d:fcvm:7")!;
    expect(t.height).toBe(720);
    expect(t.box!.h).toBe(720);
    expect(t.box!.w / t.box!.h).toBeCloseTo((0.1 * 16) / 9 / 0.2, 2);
    expect(v.plan.focus!.id).toBe("d:fcvm:7");
    expect(new Set(v.plan.direct.filter((x) => x.id !== "d:fcvm:7").map((x) => x.height))).toEqual(new Set([72]));
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

  it("shrinks a native stream only to fit, and on a 2x screen asks for the same pixels at half the CSS size", () => {
    expect(nativePlacement({ x: 0, y: 0, w: 600, h: 300 }, { x: 0, y: 0, w: 0.25, h: 0.5 }, 16 / 9, 1).box.h).toBe(300);
    const hi = nativePlacement({ x: 0, y: 0, w: 1400, h: 800 }, { x: 0, y: 0, w: 0.25, h: 0.5 }, 16 / 9, 2);
    expect(hi.box.h).toBe(360);
    expect(hi.streamHeight).toBe(720);
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
  it("names this viewer as you, others by number, and the owner's cadence as the schedule", () => {
    const actions = [
      { at: EPOCH + 1, viewer: "v-4", id: "a", name: "relaunch", args: { tile: "fcvm:3" }, summary: "relaunch slot-3", outcome: "committed" },
      { at: EPOCH + 2, viewer: "v-7", id: "b", name: "relaunch_all", args: {}, summary: "relaunch all (8 tiles)", outcome: "committed" },
      { at: EPOCH + 3, viewer: "v-4", id: "c", name: "word", args: {}, summary: "the text check refused it", outcome: "refused" },
      { at: EPOCH + 4, viewer: "schedule", id: "d", name: "relaunch_all", args: {}, summary: "relaunch all (8 tiles)", outcome: "committed" },
      { at: EPOCH + 5, viewer: "v-4", id: "e", name: "time_of_day", args: { v: 0.6 }, summary: "time of day", outcome: "committed" },
    ];
    let s = stateWith(fleet(2, 2, { actions }));
    s = msg(s, { type: "hello", viewer: "v-4" });
    const v = view(s, EPOCH + 5000);
    expect(v.feed.map((t) => t.text)).toEqual(["you set the time of day to 14:24", "the scheduled restart of every GPU began", "viewer 7 restarted every GPU"]);
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
    { id: "agg:example-host", half: "fcvm" as const, host: "example-host", frame_url: "ws://example-host:2/frames/agg-a", rect: { x: 0, y: 0, w: 0.5, h: 1 }, tiles: ["fcvm:1", "fcvm:2", "fcvm:3", "fcvm:4"] },
    { id: "agg:ec2", half: "ec2" as const, host: null, frame_url: "ws://example-host:2/frames/agg-ec2", rect: { x: 0.5, y: 0, w: 0.5, h: 1 }, tiles: ["ec2:1", "ec2:2", "ec2:3", "ec2:4"] },
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
