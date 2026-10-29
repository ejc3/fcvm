import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import * as fmt from "../format.ts";
import { layoutMosaic, ZOOM_MS } from "../layout.ts";
import {
  ACTION_LABELS,
  BEZEL,
  layoutPhones,
  openedArea,
  PANEL_W,
  phoneLabel,
  phoneNotice,
  phonesView,
  PHONE_ASPECT,
  restoreText,
  zoomProgressAt,
  type PhoneLayout,
  type PhonesInput,
} from "../phones.ts";
import { INTENT_NAMES, parseServerMsg, type Phones, type Tile } from "../protocol.ts";
import { initialState, reduce, type ClientState, type Notice } from "../store.ts";
import { EPOCH, fleet, snapshotText } from "./fixtures.ts";

const ACTIONS = ["switch_screen", "rotate", "dark_mode", "dance", "wave", "gpu_benchmark", "restart_app", "reboot_phone"];

/** `n` phones in the coordinator's layout: one row up to six, then rows of equal length. */
function phonesOf(n: number, over: Partial<Phones> = {}): Phones {
  const cols = n <= 6 ? n : Math.ceil(n / 2);
  const rows = Math.ceil(n / cols);
  const proto = fleet(1, 1).tiles[0]!;
  const tiles: Tile[] = [];
  for (let i = 0; i < n; i++) {
    tiles.push({
      ...structuredClone(proto),
      id: `phone:${i + 1}`,
      half: "fcvm",
      slot: `phone-${i + 1}`,
      vm: `fleet-phone-${i + 1}`,
      place: null,
      rect: { x: (i % cols) / cols, y: Math.floor(i / cols) / rows, w: 1 / cols, h: 1 / rows },
      frame_url: `ws://example-host:1/frames/phone-${i + 1}`,
      frame_url_v4: `ws://example-host-v4:1/frames/phone-${i + 1}`,
      last_relaunch: { clicked_at: EPOCH, first_frame_at: EPOCH + 22_300, phases: { restore_ms: 4600, total_ms: 22_300 } },
    });
  }
  return {
    image_aspect: (cols * 720) / (rows * 1280),
    claim_secs: 120,
    borrow: { cap: 2, lent: 0, next_return_at: null },
    actions: ACTIONS,
    zoomed_out: false,
    zoom_at: null,
    tiles,
    ...over,
  };
}

function stateWith(phones: Phones | null, you = "v-1"): ClientState {
  let s = reduce(initialState(), { type: "socket", status: "open", at: EPOCH });
  s = reduce(s, { type: "family", family: "v6", probeMs: 100, via: "race" });
  s = reduce(s, { type: "message", text: JSON.stringify({ type: "hello", viewer: you, turnstile: null }), at: EPOCH });
  return reduce(s, { type: "message", text: snapshotText(1, { ...fleet(), phones }), at: EPOCH });
}

const STAGE = { x: 0, y: 60, w: 1200, h: 700 };
const input = (s: ClientState, over: Partial<PhonesInput> = {}): PhonesInput => ({
  state: s,
  now: EPOCH + 30_000,
  dpr: 1,
  stage: STAGE,
  progress: 0,
  narrow: false,
  reducedMotion: false,
  openId: null,
  canDecode: true,
  ...over,
});

const layoutOf = (p: Phones, progress: number, over: Partial<Parameters<typeof layoutPhones>[0]> = {}): PhoneLayout =>
  layoutPhones({ area: STAGE, imageAspect: p.image_aspect, tiles: p.tiles, progress, dpr: 1, ...over });

const overlap = (a: { x: number; y: number; w: number; h: number }, b: { x: number; y: number; w: number; h: number }) =>
  a.x < b.x + b.w - 1e-6 && b.x < a.x + a.w - 1e-6 && a.y < b.y + b.h - 1e-6 && b.y < a.y + a.h - 1e-6;

describe("phones in the snapshot", () => {
  const raw = JSON.parse(readFileSync(new URL("./fixtures/coordinator.json", import.meta.url), "utf8")) as { views: Record<string, Record<string, unknown>> };

  it("reads the coordinator's phones", () => {
    const m = parseServerMsg(JSON.stringify({ type: "snapshot", version: 1, state: raw.views["29_phones"] }));
    if (m.type !== "snapshot") throw new Error("not a snapshot");
    const p = m.state.phones!;
    expect(p.image_aspect).toBe(1.125);
    expect(p.claim_secs).toBe(30);
    expect(p.actions).toEqual(ACTIONS);
    expect(p.borrow).toEqual({ cap: 1, lent: 0, next_return_at: null });
    expect(p.tiles.map((t) => [t.id, t.slot, t.phase])).toEqual([["phone:1", "phone-1", "relaunching"], ["phone:2", "phone-2", "running"]]);
    expect(p.tiles[1]!.last_relaunch).toEqual({ clicked_at: 40_000, first_frame_at: 50_000, phases: { restore_ms: 4600, total_ms: 22_500 } });
    // The vector zoomed out at 53 s and back in at 54.5 s.
    expect(p.zoomed_out).toBe(false);
    expect(p.zoom_at).toBe(54_500);
  });

  it("reads the shared zoom and the borrow a phone names", () => {
    const state = structuredClone(raw.views["29_phones"]!) as { phones: Record<string, unknown> & { tiles: Record<string, unknown>[] } };
    state.phones["zoomed_out"] = true;
    state.phones["zoom_at"] = 123_456;
    state.phones.tiles[1]!["borrowed"] = { session: "phone-claim-3", viewer: "v-12", since: 1000, until: 31_000 };
    const m = parseServerMsg(JSON.stringify({ type: "snapshot", version: 1, state }));
    if (m.type !== "snapshot") throw new Error("not a snapshot");
    expect(m.state.phones!.zoomed_out).toBe(true);
    expect(m.state.phones!.zoom_at).toBe(123_456);
    expect(m.state.phones!.tiles[1]!.borrowed).toEqual({ session: "phone-claim-3", viewer: "v-12", since: 1000, until: 31_000 });
  });

  it("has no phones when the coordinator sends none", () => {
    const m = parseServerMsg(JSON.stringify({ type: "snapshot", version: 1, state: raw.views["01_intent_idempotency"] }));
    if (m.type !== "snapshot") throw new Error("not a snapshot");
    expect(m.state.phones).toBeNull();
  });

  it("sends the phone intents by the coordinator's names", () => {
    for (const n of ["claim_phone", "release_phone", "phone_action", "phones_zoom"]) expect(INTENT_NAMES).toContain(n);
  });
});

describe("layout", () => {
  it.each([1, 2, 4, 6])("seamless, %s phones are the stitched picture: shared edges and the image as their union", (n) => {
    const p = phonesOf(n);
    const L = layoutOf(p, 0);
    const mosaic = layoutMosaic(STAGE, p.tiles, 1, p.image_aspect);
    for (const ph of L.phones) {
      expect(ph.box).toEqual(mosaic.tiles.find((t) => t.id === ph.id)!.box);
      expect(ph.bezelOpacity).toBe(0);
      expect(ph.bezel).toEqual(ph.box);
    }
    const boxes = L.phones.map((x) => x.box!);
    for (let i = 0; i < boxes.length; i++) for (let j = i + 1; j < boxes.length; j++) expect(overlap(boxes[i]!, boxes[j]!)).toBe(false);
    const area = boxes.reduce((a, b) => a + b.w * b.h, 0);
    expect(area).toBeCloseTo(L.image.w * L.image.h, 3);
    const sorted = [...boxes].sort((a, b) => a.x - b.x);
    for (let i = 1; i < sorted.length; i++) expect(sorted[i]!.x).toBe(sorted[i - 1]!.x + sorted[i - 1]!.w);
    expect(Math.min(...boxes.map((b) => b.x))).toBe(mosaic.image.x);
    expect(Math.max(...boxes.map((b) => b.x + b.w))).toBe(mosaic.image.x + mosaic.image.w);
  });

  it.each([1, 2, 4, 6, 7, 9, 12])("apart, %s phones sit in bodies that do not touch, with upright screens inside the area", (n) => {
    const p = phonesOf(n);
    const L = layoutOf(p, 1);
    expect(L.phones.every((x) => x.bezelOpacity === 1)).toBe(true);
    for (const ph of L.phones) {
      const b = ph.box!;
      expect(b.w / b.h).toBeCloseTo(PHONE_ASPECT, 6);
      // The screen sits inside its body, by the pads.
      expect(ph.bezel!.x + ph.pad.left).toBeCloseTo(b.x, 6);
      expect(ph.bezel!.y + ph.pad.top).toBeCloseTo(b.y, 6);
      expect(ph.pad.top).toBeCloseTo(b.w * BEZEL.end, 6);
      expect(ph.bezel!.x).toBeGreaterThanOrEqual(STAGE.x - 1e-6);
      expect(ph.bezel!.x + ph.bezel!.w).toBeLessThanOrEqual(STAGE.x + STAGE.w + 1e-6);
      expect(ph.bezel!.y).toBeGreaterThanOrEqual(STAGE.y - 1e-6);
      expect(ph.label!.y + ph.label!.h).toBeLessThanOrEqual(STAGE.y + STAGE.h + 1e-6);
    }
    const bodies = L.phones.map((x) => x.bezel!);
    for (let i = 0; i < bodies.length; i++) for (let j = i + 1; j < bodies.length; j++) expect(overlap(bodies[i]!, bodies[j]!)).toBe(false);
    // The rows are the rectangles' rows: one up to six phones, then two.
    expect(new Set(L.phones.map((x) => Math.round(x.box!.y))).size).toBe(n <= 6 ? 1 : 2);
  });

  it("moves the boxes between the two ends and keeps them apart on the way", () => {
    const p = phonesOf(4);
    const a = layoutOf(p, 0);
    const mid = layoutOf(p, 0.5);
    const z = layoutOf(p, 1);
    const first = (l: PhoneLayout) => l.phones[0]!.box!;
    expect(first(mid).x).toBeGreaterThan(Math.min(first(a).x, first(z).x) - 1e-6);
    expect(mid.phones[0]!.bezelOpacity).toBeGreaterThan(0);
    expect(mid.phones[0]!.bezelOpacity).toBeLessThan(1);
    const bodies = mid.phones.map((x) => x.bezel!);
    for (let i = 0; i < bodies.length; i++) for (let j = i + 1; j < bodies.length; j++) expect(overlap(bodies[i]!, bodies[j]!)).toBe(false);
  });

  it("opens one phone apart, in the room beside the panel, and places no other", () => {
    const p = phonesOf(4);
    const area = openedArea(STAGE, false);
    expect(area.w).toBe(STAGE.w - PANEL_W - 32);
    const L = layoutOf(p, 0, { area, open: "phone:3" });
    const open = L.phones.find((x) => x.id === "phone:3")!;
    expect(L.phones.filter((x) => x.box !== null).map((x) => x.id)).toEqual(["phone:3"]);
    expect(open.bezelOpacity).toBe(1);
    expect(open.box!.w / open.box!.h).toBeCloseTo(PHONE_ASPECT, 6);
    expect(open.bezel!.x + open.bezel!.w).toBeLessThanOrEqual(area.x + area.w + 1e-6);
    expect(open.bezel!.y + open.bezel!.h).toBeLessThanOrEqual(area.y + area.h + 1e-6);
    // Narrow: the top of the stage, above the sheet.
    expect(openedArea(STAGE, true)).toEqual({ x: 0, y: 60, w: 1200, h: 350 });
  });

  it("draws a turned phone at its own aspect, in a body turned with it", () => {
    const p = phonesOf(2);
    const L = layoutOf(p, 1, { aspects: { "phone:1": 16 / 9 } });
    const turned = L.phones[0]!;
    expect(turned.box!.w / turned.box!.h).toBeCloseTo(16 / 9, 6);
    expect(turned.pad.left).toBeGreaterThan(turned.pad.top);
    const bodies = L.phones.map((x) => x.bezel!);
    expect(overlap(bodies[0]!, bodies[1]!)).toBe(false);
  });

  it("keeps a turned phone's aspect in the seamless view, inside its part of the picture", () => {
    const p = phonesOf(2);
    const seamless = layoutOf(p, 0, {});
    const L = layoutOf(p, 0, { aspects: { "phone:1": 16 / 9 } });
    const turned = L.phones[0]!.box!;
    const rect = seamless.phones[0]!.box!;
    expect(turned.w / turned.h).toBeCloseTo(16 / 9, 6);
    expect(turned.x).toBeGreaterThanOrEqual(rect.x - 1e-6);
    expect(turned.x + turned.w).toBeLessThanOrEqual(rect.x + rect.w + 1e-6);
    expect(turned.y).toBeGreaterThanOrEqual(rect.y - 1e-6);
    expect(turned.y + turned.h).toBeLessThanOrEqual(rect.y + rect.h + 1e-6);
    // the upright one keeps its stitched rect
    expect(L.phones[1]!.box).toEqual(seamless.phones[1]!.box);
  });

  it("runs the zoom over the wall's zoom time", () => {
    expect(zoomProgressAt(0, 1, 1000, 1000)).toBe(0);
    expect(zoomProgressAt(0, 1, 1000, 1000 + ZOOM_MS / 2)).toBeCloseTo(0.5, 9);
    expect(zoomProgressAt(0, 1, 1000, 1000 + 5 * ZOOM_MS)).toBe(1);
    expect(zoomProgressAt(1, 0, 1000, 1000 + ZOOM_MS / 4)).toBeCloseTo(0.75, 9);
  });
});

describe("what a phone says about its restore", () => {
  const t = phonesOf(1).tiles[0]!;
  it("gives the time from the click to the first frame", () => {
    expect(restoreText(t)).toEqual({ text: "Phone restored in 22.3 s", restoring: false });
    expect(phoneLabel(t)).toBe("Phone 1 · restored in 22.3 s");
  });
  it("says Restoring while it has not drawn", () => {
    const r: Tile = { ...t, phase: "restoring", last_relaunch: { clicked_at: EPOCH, first_frame_at: null, phases: {} } };
    expect(restoreText(r)).toEqual({ text: "Restoring", restoring: true });
    expect(phoneLabel(r)).toBe("Phone 1 · Restoring");
  });
  it("says nothing for a running phone the coordinator has not timed", () => {
    expect(restoreText({ ...t, last_relaunch: null })).toBeNull();
    expect(phoneLabel({ ...t, last_relaunch: null })).toBe("Phone 1");
  });
});

describe("the draw plan", () => {
  it("streams each placed phone into its own box with no image, and draws the others dark", () => {
    const p = phonesOf(3);
    p.tiles[1] = { ...p.tiles[1]!, phase: "restoring", last_relaunch: { clicked_at: EPOCH, first_frame_at: null, phases: {} } };
    const v = phonesView(input(stateWith(p)));
    const plan = v.plan;
    expect(plan.image).toBeNull();
    expect(plan.mode).toBe("direct");
    expect(plan.aggregated).toEqual([]);
    expect(plan.focus).toBeNull();
    expect(plan.zoom).toBeNull();
    expect(plan.poster).toBeNull();
    expect(plan.posterFull).toBe(false);
    // A phone that is restoring has no stream to open; the other two do, in the viewer's family.
    expect(plan.direct.map((d) => d.id)).toEqual(["d:phone:1", "d:phone:3"]);
    expect(plan.direct[0]!.baseUrl).toBe("ws://example-host:1/frames/phone-1");
    expect(plan.direct[0]!.url).toMatch(/^ws:\/\/example-host:1\/frames\/phone-1\?h=\d+$/);
    expect(plan.direct[0]!.box).toEqual(v.layout!.phones[0]!.box);
    expect(plan.tiles.map((x) => [x.id, x.dark])).toEqual([["phone:1", false], ["phone:2", true], ["phone:3", false]]);
  });

  it("opens each phone stream with the direct seat's admission token, as the wall does", () => {
    const st = stateWith(phonesOf(2));
    const admitted = {
      ...st,
      admission: { seat: "ticket-1", locked: false, reason: null, direct: { value: "v1.9999999999.v-1.mac", expires_at: 9_999_999_999_000 }, broadcast: null, seat_free: false, capacity: 500 },
    } as ClientState;
    const plan = phonesView(input(admitted)).plan;
    expect(plan.direct).toHaveLength(2);
    for (const d of plan.direct) {
      expect(d.admit).toBe("v1.9999999999.v-1.mac");
      // the token is the socket's first message, never in the URL
      expect(new URL(d.url!).searchParams.get("admit")).toBeNull();
    }
  });

  it("waits for the address family, as the wall does", () => {
    let s = reduce(initialState(), { type: "socket", status: "open", at: EPOCH });
    s = reduce(s, { type: "message", text: snapshotText(1, { ...fleet(), phones: phonesOf(2) }), at: EPOCH });
    expect(phonesView(input(s)).plan.direct).toEqual([]);
  });

  it("opens a phone alone: only its stream is asked for", () => {
    const v = phonesView(input(stateWith(phonesOf(4)), { openId: "phone:2" }));
    expect(v.plan.direct.map((d) => d.id)).toEqual(["d:phone:2"]);
    expect(v.plan.tiles.filter((x) => x.box !== null).map((x) => x.id)).toEqual(["phone:2"]);
  });

  it("holds nothing before the stage is measured", () => {
    const v = phonesView(input(stateWith(phonesOf(2)), { stage: null }));
    expect(v.layout).toBeNull();
    expect(v.plan.direct).toEqual([]);
  });
});

describe("the page's view", () => {
  it("says why there is nothing to show", () => {
    expect(phonesView(input(stateWith(null))).empty).toBe("This fleet runs no phones.");
    expect(phonesView(input(initialState())).empty).toBe("Connecting to the fleet.");
  });

  it("follows the shared zoom in the button, not a local choice", () => {
    expect(phonesView(input(stateWith(phonesOf(2)))).zoom).toEqual({ label: "Zoom out", out: false, disabled: false });
    expect(phonesView(input(stateWith(phonesOf(2, { zoomed_out: true })))).zoom).toEqual({ label: "Zoom in", out: true, disabled: false });
  });

  it("gives each phone a card with its label", () => {
    const v = phonesView(input(stateWith(phonesOf(2)), { progress: 1 }));
    expect(v.cards.map((c) => c.label)).toEqual(["Phone 1 · restored in 22.3 s", "Phone 2 · restored in 22.3 s"]);
    expect(v.cards[0]!.ariaLabel).toBe("Phone 1, streaming. Open its details.");
  });

  it("opens no panel for a phone that is not there", () => {
    expect(phonesView(input(stateWith(phonesOf(2)), { openId: "phone:9" })).panel).toBeNull();
  });
});

describe("the panel of an opened phone", () => {
  const panelOf = (phones: Phones, you = "v-1", openId = "phone:1") => phonesView(input(stateWith(phones, you), { openId })).panel!;

  it("shows the rows, the restore and the timeline", () => {
    const p = panelOf(phonesOf(2));
    expect(p.title).toBe("Phone 1");
    expect(p.rows.map((r) => r.term)).toEqual(["GPU", "Place", "VM", "Up for", "Restored in"]);
    expect(Object.fromEntries(p.rows.map((r) => [r.term, r.value]))).toMatchObject({ VM: "fleet-phone-1", "Restored in": "22.3 s", "Up for": "7 s" });
    expect(p.restoreTotal).toBe("22.3 s");
    expect(p.timeline.map((s) => s.label)).toEqual(["snapshot restored"]);
  });

  it("offers a free phone to take over", () => {
    const p = panelOf(phonesOf(2));
    expect(p.borrow).toMatchObject({ kind: "free", button: { label: "Take over this phone", disabled: false } });
  });

  it("holds the button while the phone restores", () => {
    const ph = phonesOf(2);
    ph.tiles[0] = { ...ph.tiles[0]!, phase: "restoring", last_relaunch: { clicked_at: EPOCH, first_frame_at: null, phases: {} } };
    const p = panelOf(ph);
    expect(p.borrow).toMatchObject({ kind: "free", button: { disabled: true } });
    expect(p.state).toBe("Restoring");
    expect(p.restoreNow).toBe("now: restoring the snapshot");
  });

  it("gives the holder the eight actions, the countdown and the way back", () => {
    const ph = phonesOf(2);
    ph.tiles[0] = { ...ph.tiles[0]!, borrowed: { until: EPOCH + 30_000 + 87_400, session: "phone-claim-3", viewer: "v-1", since: EPOCH } };
    const p = panelOf(ph);
    if (p.borrow.kind !== "yours") throw new Error(p.borrow.kind);
    expect(p.borrow.session).toBe("phone-claim-3");
    expect(p.borrow.countdown).toBe("Back in 88 s");
    expect(p.borrow.actions.map((a) => a.label)).toEqual(["Switch screen", "Rotate", "Dark mode", "Dance", "Wave", "GPU benchmark", "Restart app", "Reboot phone"]);
    expect(p.borrow.actions.map((a) => a.name)).toEqual(ACTIONS);
    expect(p.borrow.actions.every((a) => !a.disabled)).toBe(true);
    expect(p.borrow.giveBack).toBe("Give it back");
    expect(Object.keys(ACTION_LABELS)).toEqual(ACTIONS);
  });

  it("offers a browser without WebGL2 the video only, and says why", () => {
    const ph = phonesOf(2);
    ph.tiles[0] = { ...ph.tiles[0]!, borrowed: { until: EPOCH + 60_000, session: "s", viewer: "v-1" } };
    const why = "This browser has no WebGL2, so it can show the video only.";
    const p = phonesView(input(stateWith(ph), { openId: "phone:1", takeover: "video", mirrorUnavailable: why })).panel!;
    if (p.borrow.kind !== "yours") throw new Error(p.borrow.kind);
    expect(p.borrow.views.map((v) => [v.name, v.pressed, v.disabled])).toEqual([
      ["video", true, false],
      ["mirror", false, true],
      ["both", false, true],
    ]);
    expect(p.borrow.viewsNote).toBe(why);
    const able = phonesView(input(stateWith(ph), { openId: "phone:1", takeover: "video" })).panel!;
    if (able.borrow.kind !== "yours") throw new Error(able.borrow.kind);
    expect(able.borrow.views.every((v) => !v.disabled)).toBe(true);
    expect(able.borrow.viewsNote).toBeNull();
  });

  it("recognises the holder by either spelling of the viewer id", () => {
    const ph = phonesOf(1);
    ph.tiles[0] = { ...ph.tiles[0]!, borrowed: { until: EPOCH + 60_000, session: "s", viewer: "viewer:v-1" } };
    expect(panelOf(ph).borrow.kind).toBe("yours");
  });

  it("says who it is lent to, without the actions", () => {
    const ph = phonesOf(2);
    ph.tiles[0] = { ...ph.tiles[0]!, borrowed: { until: EPOCH + 3_723_000, session: "phone-claim-4", viewer: "v-9" } };
    const p = panelOf(ph);
    expect(p.borrow).toEqual({ kind: "lent", text: `Borrowed by another viewer until ${fmt.utcClock(EPOCH + 3_723_000)} UTC` });
    expect(phonesView(input(stateWith(ph), { progress: 1 })).cards[0]).toMatchObject({ lent: true, mine: false });
  });

  it("holds the button on a free phone while the viewer has another", () => {
    const ph = phonesOf(2);
    ph.tiles[1] = { ...ph.tiles[1]!, borrowed: { until: EPOCH + 60_000, session: "s", viewer: "v-1" } };
    const p = panelOf(ph, "v-1", "phone:1");
    expect(p.borrow).toMatchObject({ kind: "free", button: { disabled: true }, note: "You are using Phone 2. Give it back to take another." });
  });

  it("holds the actions while the phone reboots", () => {
    const ph = phonesOf(1);
    ph.tiles[0] = { ...ph.tiles[0]!, phase: "relaunching", borrowed: { until: EPOCH + 60_000, session: "s", viewer: "v-1" }, last_relaunch: { clicked_at: EPOCH, first_frame_at: null, phases: {} } };
    const p = panelOf(ph);
    if (p.borrow.kind !== "yours") throw new Error(p.borrow.kind);
    expect(p.borrow.actions.every((a) => a.disabled)).toBe(true);
  });
});

describe("refusals", () => {
  const n = (intent: Notice["intent"], reason: string | null, kind: Notice["kind"] = "refused"): Notice => ({ id: reason ?? "x", kind, intent, reason, args: {}, at: EPOCH });
  it("shows the newest refusal of a phone intent", () => {
    expect(phoneNotice([n("claim_phone", "one phone per client at a time"), n("phone_action", "one action a second; wait 400 ms")])).toBe("one action a second; wait 400 ms");
  });
  it("ignores other intents and applied receipts", () => {
    expect(phoneNotice([n("relaunch", "no"), n("phone_action", null, "applied")])).toBeNull();
  });
});
