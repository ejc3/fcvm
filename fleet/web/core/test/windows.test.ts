import { describe, expect, it } from "vitest";
import {
  desktopInText,
  desktopTimes,
  layoutWindows,
  medianMs,
  restoringText,
  windowsNotice,
  windowsView,
  WINDOWS_ACTION_LABELS,
  type WindowsInput,
} from "../windows.ts";
import { INTENT_NAMES, parseServerMsg, ProtocolError, type FleetState, type Windows, type WindowTile } from "../protocol.ts";
import { initialState, reduce, type ClientState, type Notice } from "../store.ts";
import { EPOCH, fleet, snapshotText } from "./fixtures.ts";

const LABEL = "Windows Server 2025 evaluation, for demonstration only";
const ACTIONS = ["start_menu", "notepad", "explorer", "task_manager", "about", "terminal", "close_all", "reset"];
const NOW = EPOCH + 30_000;

/** `n` sandboxes in the coordinator's layout: one row up to four, then rows of equal length, each 1280 x 720. */
function windowsOf(n: number, over: Partial<Windows> = {}): Windows {
  const rows = Math.ceil(n / 4);
  const cols = Math.ceil(n / rows);
  const proto = fleet(1, 1).tiles[0]!;
  const tiles: WindowTile[] = [];
  for (let i = 0; i < n; i++) {
    tiles.push({
      ...structuredClone(proto),
      id: `win:${i + 1}`,
      half: "fcvm",
      slot: `win-${i + 1}`,
      vm: `fleet-win-${i + 1}`,
      place: null,
      gpu: null,
      rect: { x: (i % cols) / cols, y: Math.floor(i / cols) / rows, w: 1 / cols, h: 1 / rows },
      frame_url: `ws://example-host:1/frames/win-${i + 1}`,
      frame_url_v4: `ws://example-host-v4:1/frames/win-${i + 1}`,
      last_relaunch: { clicked_at: EPOCH, first_frame_at: EPOCH + 2310, phases: { restore_ms: 1500, total_ms: 2310 } },
      actions: ACTIONS,
    });
  }
  return {
    label: LABEL,
    image_aspect: (cols * 1280) / (rows * 720),
    claim_secs: 120,
    borrow: { cap: 2, lent: 0, next_return_at: null },
    image: {
      id: "3f9c0a1b2c4d",
      edition: "Windows Server 2025 Datacenter Evaluation (Desktop Experience)",
      build: "26100.1742",
      installed_at: "2026-11-07T10:00:00Z",
      eval_expires: "2027-05-06T10:00:00+00:00",
      age_days: 12,
      warning: null,
      refused: null,
      gold: "ready",
      gold_error: null,
    },
    last_reset_all: null,
    tiles,
    ...over,
  };
}

function stateWith(windows: Windows | null, you = "v-1", over: Partial<FleetState> = {}): ClientState {
  let s = reduce(initialState(), { type: "socket", status: "open", at: EPOCH });
  s = reduce(s, { type: "family", family: "v6", probeMs: 100, via: "race" });
  s = reduce(s, { type: "message", text: JSON.stringify({ type: "hello", viewer: you, turnstile: null }), at: EPOCH });
  return reduce(s, { type: "message", text: snapshotText(1, { ...fleet(), windows, ...over }), at: EPOCH });
}

const STAGE = { x: 0, y: 150, w: 1000, h: 480 };
const input = (s: ClientState, over: Partial<WindowsInput> = {}): WindowsInput => ({ state: s, now: NOW, dpr: 1, stage: STAGE, narrow: false, reducedMotion: false, canDecode: true, ...over });
const viewOf = (w: Windows | null, over: Partial<WindowsInput> = {}, you = "v-1") => windowsView(input(stateWith(w, you), over));

/** Replace tile `i` of a copy of `w`. */
function with1(w: Windows, i: number, patch: Partial<WindowTile>): Windows {
  return { ...w, tiles: w.tiles.map((t, k) => (k === i ? { ...t, ...patch } : t)) };
}

describe("windows in the snapshot", () => {
  const snap = (state: Record<string, unknown>) => parseServerMsg(JSON.stringify({ type: "snapshot", version: 1, state }));
  const base = (windows: unknown) => ({ ...fleet(), windows }) as unknown as Record<string, unknown>;

  it("reads the coordinator's sandboxes", () => {
    const w = windowsOf(4, {
      last_reset_all: { clicked_at: 1_761_134_400_000, by: "viewer:v-3", sandboxes: 4 },
      borrow: { cap: 2, lent: 1, next_return_at: 1_761_134_525_000 },
    });
    const m = snap(base(w));
    if (m.type !== "snapshot") throw new Error("not a snapshot");
    const got = m.state.windows!;
    expect(got).toEqual(w);
    expect(got.image_aspect).toBeCloseTo(16 / 9 * 4, 9);
    expect(got.tiles[2]!.actions).toEqual(ACTIONS);
  });

  it("has no sandboxes when the coordinator sends none, or says null", () => {
    for (const windows of [undefined, null]) {
      const m = snap(base(windows));
      if (m.type !== "snapshot") throw new Error("not a snapshot");
      expect(m.state.windows).toBeNull();
    }
  });

  it("reads a sandbox's borrow and an image with its warning, refusal and gold failure", () => {
    const w = windowsOf(2);
    w.tiles[1]!.borrowed = { session: "windows-claim-3", viewer: "v-12", since: 1000, until: 121_000 };
    w.image = { ...w.image!, age_days: 171, warning: "old", refused: "refused", gold: "failed", gold_error: "no disk space" };
    const m = snap(base(w));
    if (m.type !== "snapshot") throw new Error("not a snapshot");
    expect(m.state.windows!.tiles[1]!.borrowed).toEqual({ session: "windows-claim-3", viewer: "v-12", since: 1000, until: 121_000 });
    expect(m.state.windows!.image).toMatchObject({ age_days: 171, warning: "old", refused: "refused", gold: "failed", gold_error: "no disk space" });
  });

  type Raw = Record<string, unknown> & { tiles: Record<string, unknown>[]; borrow: Record<string, unknown>; image: Record<string, unknown> };
  const loose = (w: Raw) => w as Record<string, unknown>;
  it.each<[string, (w: Raw) => void, RegExp]>([
    ["no label", (w) => { delete loose(w)["label"]; }, /windows\.label is not a string/],
    ["no aspect", (w) => { delete loose(w)["image_aspect"]; }, /windows\.image_aspect is not a number/],
    ["an aspect of zero", (w) => { w["image_aspect"] = 0; }, /image_aspect is not positive/],
    ["no claim time", (w) => { delete loose(w)["claim_secs"]; }, /windows\.claim_secs is not a number/],
    ["no borrow", (w) => { delete loose(w)["borrow"]; }, /windows\.borrow is not an object/],
    ["a borrow without its cap", (w) => { delete w.borrow["cap"]; }, /borrow\.cap is not a number/],
    ["tiles that are not a list", (w) => { loose(w)["tiles"] = {}; }, /windows\.tiles is not an array/],
    ["a tile outside the image", (w) => { w.tiles[0]!["rect"] = { x: 0.9, y: 0, w: 0.5, h: 1 }; }, /not inside the unit image/],
    ["a sandbox twice", (w) => { w.tiles[1]!["id"] = w.tiles[0]!["id"]; }, /appears twice/],
    ["an image that is not an object", (w) => { loose(w)["image"] = "x"; }, /windows\.image is not an object/],
    ["an image without a build", (w) => { delete w.image["build"]; }, /image\.build is not a string/],
    ["an image age that is text", (w) => { w.image["age_days"] = "12"; }, /image\.age_days is not a number/],
    ["a gold state it does not know", (w) => { w.image["gold"] = "done"; }, /image\.gold is done/],
    ["a reset all without its count", (w) => { w["last_reset_all"] = { clicked_at: 1, by: "viewer:v-1" }; }, /last_reset_all\.sandboxes is not a number/],
  ])("rejects %s", (_, mutate, error) => {
    const w = structuredClone(windowsOf(2)) as unknown as Raw;
    mutate(w);
    const bad = () => snap(base(w));
    expect(bad).toThrow(ProtocolError);
    expect(bad).toThrow(error);
  });

  it("rejects a windows section that is not an object", () => {
    expect(() => snap(base("x"))).toThrow(/windows is not an object/);
    expect(() => snap(base([]))).toThrow(/windows is not an object/);
  });

  it("sends the four intents by the coordinator's names", () => {
    for (const n of ["windows_reset", "claim_windows", "release_windows", "windows_action"]) expect(INTENT_NAMES).toContain(n);
  });
});

describe("a sandbox's words", () => {
  it("says its slot, edition, the label and the time to desktop in two decimals", () => {
    const v = viewOf(windowsOf(4));
    expect(v.cards.map((c) => c.slot)).toEqual(["win-1", "win-2", "win-3", "win-4"]);
    for (const c of v.cards) {
      expect(c.edition).toBe("Windows Server 2025 Datacenter Evaluation (Desktop Experience)");
      expect(c.desktopIn).toBe("Desktop in 2.31 s");
      expect(c.label).toBe(LABEL);
      expect(c.timer).toBeNull();
      expect(c.error).toBeNull();
      expect(c.dark).toBe(false);
    }
  });

  it("rounds the time to desktop to hundredths", () => {
    const t = (first: number | null) => ({ clicked_at: EPOCH, first_frame_at: first === null ? null : EPOCH + first, phases: {} });
    const tile = (first: number | null): WindowTile => ({ ...windowsOf(1).tiles[0]!, last_relaunch: t(first) });
    expect(desktopInText(tile(2304))).toBe("Desktop in 2.30 s");
    expect(desktopInText(tile(2306))).toBe("Desktop in 2.31 s");
    expect(desktopInText(tile(12_000))).toBe("Desktop in 12.00 s");
    expect(desktopInText(tile(null))).toBeNull();
    expect(desktopInText({ ...tile(5), last_relaunch: null })).toBeNull();
  });

  it.each(["restoring", "relaunching", "pending"])("%s: a dark tile with a timer counting from the click, and no time to desktop", (phase) => {
    const w = with1(windowsOf(2), 1, { phase, last_relaunch: { clicked_at: NOW - 1400, first_frame_at: null, phases: {} } });
    const c = viewOf(w).cards[1]!;
    expect(c.dark).toBe(true);
    expect(c.timer).toBe("Restoring 1.4 s");
    expect(c.desktopIn).toBeNull();
    expect(c.label).toBe(LABEL);
    expect(viewOf(w, { now: NOW + 600 }).cards[1]!.timer).toBe("Restoring 2.0 s");
  });

  it("connecting says Connecting; a sandbox with no launch to count from says the word alone", () => {
    const w = with1(windowsOf(2), 0, { phase: "connecting", last_relaunch: { clicked_at: NOW - 2100, first_frame_at: null, phases: {} } });
    expect(viewOf(w).cards[0]!.timer).toBe("Connecting 2.1 s");
    const none = with1(windowsOf(2), 0, { phase: "pending", last_relaunch: null });
    expect(viewOf(none).cards[0]!.timer).toBe("Restoring");
    expect(restoringText(none.tiles[0]!, NOW)).toBe("Restoring");
  });

  it("a failed sandbox is dark and shows its error, the failure's message when there is no error, a fixed line otherwise", () => {
    const w0 = windowsOf(3);
    const w = with1(with1(with1(w0, 0, { phase: "failed", error: "restore: disk full" }), 1, { phase: "failed", error: null, failure: { kind: "relaunch_failed", message: "the VM did not come back", retry_at: null } }), 2, { phase: "failed", error: null });
    const cards = viewOf(w).cards;
    expect(cards.map((c) => [c.dark, c.timer, c.error])).toEqual([
      [true, null, "Failed: restore: disk full"],
      [true, null, "Failed: the VM did not come back"],
      [true, null, "Failed: the sandbox did not come back"],
    ]);
  });
});

describe("the header", () => {
  it("counts the sandboxes up and gives the median and the slowest time to desktop of each one's last reset", () => {
    const first = (ms: number | null): Partial<WindowTile> => ({ last_relaunch: { clicked_at: EPOCH, first_frame_at: ms === null ? null : EPOCH + ms, phases: {} } });
    let w = windowsOf(4);
    w = with1(w, 0, first(2000));
    w = with1(w, 1, first(3100));
    w = with1(w, 2, first(2400));
    w = with1(w, 3, { phase: "restoring", ...first(null) });
    const h = viewOf(w).header!;
    expect(h.up).toBe("3 of 4 sandboxes up");
    expect(h.median).toBe("Median time to desktop 2.40 s");
    expect(h.slowest).toBe("Slowest 3.10 s");
    // an even count: the mean of the two in the middle
    expect(viewOf(with1(w, 3, { phase: "running", ...first(4000) })).header!.median).toBe("Median time to desktop 2.75 s");
    expect(medianMs([5, 1, 3])).toBe(3);
    expect(medianMs([])).toBeNull();
    expect(desktopTimes(w.tiles)).toEqual([2000, 3100, 2400]);
  });

  it("says -- before any sandbox has drawn a desktop, and 1 of 1 sandbox in the singular", () => {
    const w = with1(windowsOf(1), 0, { phase: "pending", last_relaunch: null });
    const h = viewOf(w).header!;
    expect(h.up).toBe("0 of 1 sandbox up");
    expect(h.median).toBe("Median time to desktop --");
    expect(h.slowest).toBe("Slowest --");
  });

  it("gives the last reset all: when, how many, and by whom", () => {
    const at = Date.UTC(2026, 10, 7, 14, 3, 20);
    const line = (by: string, n = 4) => viewOf(windowsOf(4, { last_reset_all: { clicked_at: at, by, sandboxes: n } }), {}, "v-3").header!.lastResetAll;
    expect(line("viewer:v-3")).toBe("Last reset all: 14:03:20 UTC, 4 sandboxes, by you");
    expect(line("viewer:v-9")).toBe("Last reset all: 14:03:20 UTC, 4 sandboxes, by another viewer");
    expect(line("schedule", 1)).toBe("Last reset all: 14:03:20 UTC, 1 sandbox, by schedule");
    expect(viewOf(windowsOf(4)).header!.lastResetAll).toBe("Reset all: not used yet");
  });

  it("describes the image, and gives its warning and refusal as the coordinator worded them", () => {
    const img = windowsOf(2).image!;
    expect(viewOf(windowsOf(2)).header).toMatchObject({
      image: "Windows Server 2025 Datacenter Evaluation (Desktop Experience), build 26100.1742, 12 days old",
      warning: null,
      refused: null,
      gold: null,
    });
    const old = viewOf(windowsOf(2, { image: { ...img, age_days: 151, warning: "The image is 151 days old." } })).header!;
    expect(old.warning).toBe("The image is 151 days old.");
    expect(old.refused).toBeNull();
    const refused = viewOf(windowsOf(2, { image: { ...img, age_days: 1, refused: "No new sandbox starts from this image." } })).header!;
    expect(refused.refused).toBe("No new sandbox starts from this image.");
    expect(refused.image).toContain("1 day old");
  });

  it("says what the gold snapshot is doing while it builds or after it failed, and nothing when it is ready", () => {
    const img = windowsOf(2).image!;
    const gold = (over: Partial<typeof img>) => viewOf(windowsOf(2, { image: { ...img, ...over } })).header!.gold;
    expect(gold({ gold: "ready" })).toBeNull();
    expect(gold({ gold: "building" })).toBe("Building the Windows snapshot. Sandboxes start when it is ready.");
    expect(gold({ gold: "failed", gold_error: "no disk space" })).toBe("The Windows snapshot failed to build: no disk space.");
    expect(gold({ gold: "failed", gold_error: null })).toBe("The Windows snapshot failed to build.");
    expect(viewOf(windowsOf(2, { image: null })).header).toMatchObject({ image: null, gold: null, warning: null, refused: null });
  });

  it("Reset all is on while online and there are sandboxes, off when the socket is closed", () => {
    expect(viewOf(windowsOf(2)).resetAll).toEqual({ label: "Reset all", disabled: false });
    let s = stateWith(windowsOf(2));
    s = reduce(s, { type: "socket", status: "closed", at: EPOCH });
    expect(windowsView(input(s)).resetAll.disabled).toBe(true);
    expect(windowsView(input(s)).banner).toMatch(/coordinator is offline/);
  });
});

describe("taking a sandbox over", () => {
  const held = (viewer: string): Windows => {
    const w = windowsOf(4, { borrow: { cap: 2, lent: 1, next_return_at: NOW + 97_000 } });
    w.tiles[2]!.borrowed = { session: "windows-claim-3", viewer, since: NOW - 23_000, until: NOW + 97_000 };
    return w;
  };

  it("offers Take over on a running sandbox that is free, and gives no panel", () => {
    const v = viewOf(windowsOf(4));
    expect(v.panel).toBeNull();
    for (const c of v.cards) expect(c.take).toEqual({ label: "Take over", disabled: false, title: null });
  });

  it("gives the holder a panel with exactly the tile's actions, the countdown, the label and Give back", () => {
    const v = viewOf(held("v-1"));
    const p = v.panel!;
    expect(p.slot).toBe("win-3");
    expect(p.session).toBe("windows-claim-3");
    expect(p.countdown).toBe("Returns in 97 s");
    expect(p.label).toBe(LABEL);
    expect(p.giveBack).toBe("Give back");
    expect(p.actions.map((a) => [a.name, a.label])).toEqual([
      ["start_menu", "Start menu"],
      ["notepad", "Notepad"],
      ["explorer", "File Explorer"],
      ["task_manager", "Task Manager"],
      ["about", "About Windows"],
      ["terminal", "PowerShell"],
      ["close_all", "Close all"],
      ["reset", "Reset"],
    ]);
    expect(p.actions.every((a) => !a.disabled)).toBe(true);
    expect(Object.keys(WINDOWS_ACTION_LABELS).sort()).toEqual([...ACTIONS].sort());
    // the holder's tile says it is theirs and cannot be reset or taken again; the others cannot be taken while one is held
    const mine = v.cards[2]!;
    expect([mine.mine, mine.lent, mine.lentText, mine.reset.disabled]).toEqual([true, false, "Yours", true]);
    expect(v.cards[0]!.take).toMatchObject({ disabled: true, title: "You are using win-3. Give it back to take another." });
  });

  it("a host that lists fewer actions gives fewer buttons, in its own order, and an unknown name gets a readable label", () => {
    const w = held("v-1");
    w.tiles[2]!.actions = ["notepad", "reset", "screen_saver"];
    expect(viewOf(w).panel!.actions.map((a) => [a.name, a.label])).toEqual([["notepad", "Notepad"], ["reset", "Reset"], ["screen_saver", "Screen saver"]]);
  });

  it("the actions are off while the sandbox is not running or the page is offline", () => {
    const w = held("v-1");
    w.tiles[2]!.phase = "restoring";
    expect(viewOf(w).panel!.actions.every((a) => a.disabled)).toBe(true);
    let s = stateWith(held("v-1"));
    s = reduce(s, { type: "socket", status: "closed", at: EPOCH });
    expect(windowsView(input(s)).panel!.actions.every((a) => a.disabled)).toBe(true);
  });

  it("another viewer's borrow shows no panel to this viewer, only that it is lent and until when", () => {
    const v = viewOf(held("v-12"));
    expect(v.panel).toBeNull();
    const c = v.cards[2]!;
    expect([c.lent, c.mine, c.reset.disabled, c.take.disabled]).toEqual([true, false, true, true]);
    expect(c.lentText).toBe(`Lent to another viewer until ${new Date(NOW + 97_000).toISOString().slice(11, 19)} UTC`);
    // the others stay free
    expect(v.cards[0]!.take.disabled).toBe(false);
  });

  it("the panel is the viewer's by id, with or without the viewer: prefix", () => {
    expect(viewOf(held("viewer:v-1")).panel).not.toBeNull();
    expect(viewOf(held("v-1"), {}, "viewer:v-1").panel).not.toBeNull();
  });

  it("a sandbox that is not running cannot be taken over, and says why", () => {
    const w = with1(windowsOf(2), 1, { phase: "restoring", last_relaunch: { clicked_at: NOW - 100, first_frame_at: null, phases: {} } });
    expect(viewOf(w).cards[1]!.take).toEqual({ label: "Take over", disabled: true, title: "It can be borrowed once it has restored." });
  });

  it("gives no input of any kind: the view carries buttons and text, never a point, a key or a field", () => {
    const v = viewOf(held("v-1"));
    const json = JSON.stringify(v);
    for (const word of ["keyboard", "typing", "tap", "click_at", "\"x\":0.", "text_input"]) expect(json.toLowerCase()).not.toContain(word.toLowerCase());
    expect(v.panel!.note).toMatch(/Fixed actions only/);
  });
});

describe("the label", () => {
  it("is on every tile and on the panel, whatever the phase", () => {
    let w = windowsOf(4);
    w = with1(w, 1, { phase: "restoring", last_relaunch: { clicked_at: NOW - 500, first_frame_at: null, phases: {} } });
    w = with1(w, 2, { phase: "failed", error: "x" });
    w.tiles[3]!.borrowed = { session: "windows-claim-1", viewer: "v-1", since: NOW, until: NOW + 5000 };
    const v = viewOf(w);
    expect(v.cards.map((c) => c.label)).toEqual([LABEL, LABEL, LABEL, LABEL]);
    expect(v.panel!.label).toBe(LABEL);
  });
});

describe("refusals", () => {
  const n = (intent: Notice["intent"], reason: string | null, kind: Notice["kind"] = "refused"): Notice => ({ id: reason ?? "x", kind, intent, reason, args: {}, at: EPOCH });
  it("shows the newest refusal of a Windows intent as the coordinator worded it", () => {
    expect(windowsNotice([n("claim_windows", "one sandbox per client at a time"), n("windows_action", "one action a second; wait 400 ms")])).toBe("one action a second; wait 400 ms");
    expect(windowsNotice([n("windows_reset", "win-2 is lent to a viewer"), n("relaunch", "no")])).toBe("win-2 is lent to a viewer");
    expect(windowsNotice([n("windows_reset", "one reset all per 10 s per client")])).toBe("one reset all per 10 s per client");
  });
  it("ignores what is not a refusal of a Windows intent", () => {
    expect(windowsNotice([n("relaunch", "no"), n("phone_action", "no"), n("windows_reset", null, "applied")])).toBeNull();
    expect(windowsNotice([n("windows_action", null)])).toBe("The coordinator refused it.");
  });
  it("reaches the view from the store", () => {
    let s = stateWith(windowsOf(2));
    s = reduce(s, { type: "intent-sent", intent: { id: "i1", name: "windows_reset", args: { slot: "win-9" } }, at: EPOCH } as never);
    s = reduce(s, { type: "message", text: JSON.stringify({ type: "receipt", id: "i1", outcome: "refused", reason: 'no sandbox "win-9"', version: 2 }), at: EPOCH });
    expect(windowsView(input(s)).notice).toBe('no sandbox "win-9"');
  });
});

describe("what there is to show", () => {
  it("says it is connecting before the first snapshot, and that the fleet has no sandboxes when it has none", () => {
    const s0 = reduce(reduce(initialState(), { type: "socket", status: "connecting", at: EPOCH }), { type: "family", family: "v6", probeMs: 1, via: "race" });
    const v0 = windowsView(input(s0));
    expect(v0.empty).toBe("Connecting to the fleet.");
    expect(v0.cards).toEqual([]);
    const v = viewOf(null);
    expect(v.empty).toBe("This fleet runs no Windows sandboxes.");
    expect(v.header).toBeNull();
    expect(v.plan.direct).toEqual([]);
    expect(viewOf(windowsOf(2)).empty).toBeNull();
  });

  it("says when the browser cannot decode the pictures", () => {
    expect(viewOf(windowsOf(2), { canDecode: false }).banner).toMatch(/cannot decode H\.264/);
  });
});

describe("layout", () => {
  const boxesOf = (n: number, over: Partial<Parameters<typeof layoutWindows>[0]> = {}) => {
    const w = windowsOf(n);
    return layoutWindows({ area: STAGE, tiles: w.tiles, imageAspect: w.image_aspect, narrow: false, panel: false, dpr: 1, ...over });
  };
  const overlap = (a: { x: number; y: number; w: number; h: number }, b: { x: number; y: number; w: number; h: number }) =>
    a.x < b.x + b.w - 1e-6 && b.x < a.x + a.w - 1e-6 && a.y < b.y + b.h - 1e-6 && b.y < a.y + a.h - 1e-6;

  it.each([1, 2, 3, 4, 5, 8, 9, 12])("%s sandboxes: 16:9 desktops of one size, inside the stage, apart, in the snapshot's rows", (n) => {
    const L = boxesOf(n);
    expect(L).toHaveLength(n);
    for (const b of L) {
      expect(b.box.w / b.box.h).toBeCloseTo(16 / 9, 1);
      expect(b.box.w).toBe(L[0]!.box.w);
      expect(b.box.x).toBeGreaterThanOrEqual(STAGE.x - 1e-6);
      expect(b.box.x + b.box.w).toBeLessThanOrEqual(STAGE.x + STAGE.w + 1e-6);
      expect(b.box.y).toBeGreaterThanOrEqual(STAGE.y - 1e-6);
      expect(b.caption.y + b.caption.h).toBeLessThanOrEqual(STAGE.y + STAGE.h + 1e-6);
      expect(b.caption.x).toBe(b.box.x);
      expect(b.caption.y).toBeGreaterThanOrEqual(b.box.y + b.box.h);
    }
    for (let i = 0; i < L.length; i++) for (let j = i + 1; j < L.length; j++) {
      expect(overlap(L[i]!.box, L[j]!.box)).toBe(false);
      expect(overlap(L[i]!.box, L[j]!.caption)).toBe(false);
    }
    expect(new Set(L.map((b) => b.box.y)).size).toBe(Math.ceil(n / 4));
  });

  it("a narrow screen takes two columns, or one row beside an open panel", () => {
    const narrow = { x: 0, y: 150, w: 390, h: 600 };
    expect(new Set(boxesOf(4, { area: narrow, narrow: true }).map((b) => b.box.x)).size).toBe(2);
    expect(new Set(boxesOf(4, { area: narrow, narrow: true }).map((b) => b.box.y)).size).toBe(2);
    expect(new Set(boxesOf(4, { area: narrow, narrow: true, panel: true }).map((b) => b.box.y)).size).toBe(1);
  });

  it("with the panel open the desktops use the room beside it", () => {
    const w = windowsOf(4);
    w.tiles[0]!.borrowed = { session: "windows-claim-1", viewer: "v-1", since: NOW, until: NOW + 100_000 };
    const wide = { x: 0, y: 150, w: 1200, h: 560 };
    const v = viewOf(w, { stage: wide });
    const right = Math.max(...v.cards.map((c) => c.box!.x + c.box!.w));
    expect(right).toBeLessThanOrEqual(1200 - 380 - 32 + 1e-6);
    expect(v.cards.every((c) => !c.compact)).toBe(true);
    const n = viewOf(w, { stage: { x: 0, y: 150, w: 390, h: 640 }, narrow: true });
    expect(n.cards.every((c) => c.compact)).toBe(true);
    expect(Math.max(...n.cards.map((c) => c.box!.y + c.box!.h))).toBeLessThanOrEqual(150 + 320 + 1e-6);
  });

  it("places nothing without a stage", () => {
    const v = viewOf(windowsOf(2), { stage: null });
    expect(v.cards.every((c) => c.box === null && c.caption === null)).toBe(true);
    expect(v.plan.direct).toEqual([]);
  });
});

describe("the draw plan", () => {
  it("streams each running sandbox into its box and draws the others dark", () => {
    const w = with1(windowsOf(4), 1, { phase: "restoring", frame_url: null, frame_url_v4: null, last_relaunch: { clicked_at: NOW - 100, first_frame_at: null, phases: {} } });
    const v = viewOf(w);
    expect(v.plan.mode).toBe("direct");
    expect(v.plan.image).toBeNull();
    expect(v.plan.direct.map((d) => d.id)).toEqual(["d:win:1", "d:win:3", "d:win:4"]);
    const boxOf = new Map(v.cards.map((c) => [`d:${c.id}`, c.box]));
    for (const d of v.plan.direct) expect(d.box).toEqual(boxOf.get(d.id));
    expect(v.plan.tiles.map((t) => [t.id, t.dark])).toEqual([["win:1", false], ["win:2", true], ["win:3", false], ["win:4", false]]);
  });

  it("streams nothing to a viewer who has no direct seat", () => {
    let s = stateWith(windowsOf(2));
    s = reduce(s, { type: "message", text: JSON.stringify({ type: "admission", seat: null, locked: true, reason: "Direct streams are full.", direct: null, broadcast: null, seat_free: false }), at: EPOCH });
    const v = windowsView(input(s));
    expect(v.plan.direct).toEqual([]);
    expect(v.banner).toMatch(/Sandboxes stream only to viewers with a direct seat/);
  });
});

describe("captions", () => {
  it("a caption narrower than a full one is compact, and on a phone the desktops are as large as compact captions allow", () => {
    const w = windowsOf(8);
    const phone = { x: 8, y: 150, w: 374, h: 690 };
    const L = layoutWindows({ area: phone, tiles: w.tiles, imageAspect: w.image_aspect, narrow: true, panel: false, dpr: 3 });
    expect(L.every((b) => b.compact)).toBe(true);
    // two columns of full captions would be 110 px wide desktops, under the full width
    expect(L[0]!.box.w).toBeGreaterThan(100);
    const wide = layoutWindows({ area: STAGE, tiles: windowsOf(4).tiles, imageAspect: windowsOf(4).image_aspect, narrow: false, panel: false, dpr: 1 });
    expect(wide.every((b) => !b.compact)).toBe(true);
  });
});
