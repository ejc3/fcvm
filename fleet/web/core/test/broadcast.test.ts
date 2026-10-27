import { describe, expect, it } from "vitest";
import {
  addHistory,
  broadcastShown,
  broadcastToggle,
  frameWallMs,
  HISTORY_MS,
  SHOWN_ADVANCE_MAX_MS,
  snapshotAt,
  watchesBroadcast,
  type HistoryEntry,
} from "../broadcast.ts";
import { serverNow } from "../clock.ts";
import { frameSocketUrl, sameStreamAtAnyHeight } from "../net.ts";
import type { Admission, FleetState } from "../protocol.ts";
import { initialState, reduce, type ClientState } from "../store.ts";
import { wallView } from "../view.ts";
import { EPOCH, fleet, snapshotText } from "./fixtures.ts";

const PLAYBACK = "https://playback.example/channel.m3u8?token=jwt";

function admission(over: Partial<Admission> = {}): Admission {
  return {
    seat: "ticket-1",
    locked: false,
    reason: null,
    direct: { value: "v1.9999999999.v-1.mac", expires_at: 9_999_999_999_000 },
    broadcast: { value: PLAYBACK, expires_at: 9_999_999_999_000 },
    seat_free: false,
    capacity: 500,
    ...over,
  };
}

const LATE: Partial<Admission> = {
  seat: null,
  locked: true,
  reason: "Direct streams are full, so you are watching the broadcast.",
  direct: null,
};

function msg(s: ClientState, m: unknown, at = EPOCH): ClientState {
  return reduce(s, { type: "message", text: JSON.stringify(m), at });
}

/** A connection that got `adm`, then snapshots v1..vN, each `stepMs` apart. */
function withSnapshots(adm: Admission, states: FleetState[], stepMs = 1000): ClientState {
  let s = reduce(initialState(), { type: "socket", status: "open", at: EPOCH });
  s = reduce(s, { type: "family", family: "v6", probeMs: 100, via: "race" });
  s = msg(s, { type: "admission", ...adm });
  states.forEach((st, i) => {
    s = reduce(s, { type: "message", text: snapshotText(i + 1, st), at: EPOCH + i * stepMs });
  });
  return s;
}

function entry(at: number, version: number): HistoryEntry {
  return { at, version, state: fleet() };
}

describe("the snapshot history", () => {
  it("keeps HISTORY_MS of snapshots, newest versions only", () => {
    let h: HistoryEntry[] = [];
    h = addHistory(h, entry(0, 1));
    h = addHistory(h, entry(1000, 2));
    h = addHistory(h, entry(HISTORY_MS + 500, 3));
    expect(h.map((x) => x.version)).toEqual([2, 3]);
    // An older or repeated version replaces nothing newer.
    h = addHistory(h, entry(HISTORY_MS + 600, 2));
    expect(h.map((x) => x.version)).toEqual([2]);
  });

  it("names the snapshot in effect at a frame's wall time", () => {
    const h = [entry(1000, 1), entry(2000, 2), entry(3000, 3)];
    expect(snapshotAt(h, 2500)?.version).toBe(2);
    expect(snapshotAt(h, 3000)?.version).toBe(3);
    // Before the oldest kept: the oldest.
    expect(snapshotAt(h, 10)?.version).toBe(1);
    expect(snapshotAt([], 10)).toBeNull();
  });

  it("is kept only when there is a broadcast", () => {
    const states = [fleet(), fleet(), fleet()];
    expect(withSnapshots(admission(), states).history.map((h) => h.version)).toEqual([1, 2, 3]);
    expect(withSnapshots(admission({ broadcast: null }), states).history).toEqual([]);
  });
});

describe("the frame on screen", () => {
  it("advances with the time since it was presented, up to a stall's limit", () => {
    const f = { t: 100, e: EPOCH, at: 5000 };
    expect(frameWallMs(f, 5000)).toBe(EPOCH + 100_000);
    expect(frameWallMs(f, 5100)).toBe(EPOCH + 100_100);
    expect(frameWallMs(f, 9000)).toBe(EPOCH + 100_000 + SHOWN_ADVANCE_MAX_MS);
  });

  it("draws the overlay from the state that was current when the shown frame was drawn", () => {
    // Three snapshots a second apart; the second has a tile dark that the third has back.
    const a = fleet();
    const b = fleet();
    b.tiles[0] = { ...b.tiles[0]!, phase: "restoring" };
    const c = fleet();
    const s = withSnapshots(admission(LATE), [a, b, c]);
    // The frame on screen was drawn 1.5 s after the first snapshot: the second is in effect.
    const local = EPOCH + 2500;
    const wall = serverNow(s.clock, EPOCH) + 1500;
    const frame = { t: (wall - EPOCH) / 1000, e: EPOCH, at: local };
    const { state, now } = broadcastShown(s, frame, local);
    expect(state.version).toBe(2);
    expect(state.accepted!.tiles[0]!.phase).toBe("restoring");
    expect(state.shownLayout!.tiles[0]!.phase).toBe("restoring");
    // "Now" for the overlay is the frame's own time.
    expect(serverNow(s.clock, now)).toBeCloseTo(wall, 6);
    // The live state is untouched.
    expect(s.accepted!.tiles[0]!.phase).not.toBe("restoring");
  });

  it("is the live state until a frame has been presented", () => {
    const s = withSnapshots(admission(LATE), [fleet()]);
    expect(broadcastShown(s, null, EPOCH + 10)).toEqual({ state: s, now: EPOCH + 10 });
  });
});

describe("who watches the broadcast", () => {
  it("is forced for a late viewer, chosen by a seated one, impossible without a broadcast", () => {
    const late = withSnapshots(admission(LATE), [fleet()]);
    expect(watchesBroadcast(late)).toBe(true);
    expect(watchesBroadcast({ ...late, ui: { ...late.ui, mode: "direct" } })).toBe(true);
    const seated = withSnapshots(admission(), [fleet()]);
    expect(watchesBroadcast(seated)).toBe(false);
    expect(watchesBroadcast({ ...seated, ui: { ...seated.ui, mode: "broadcast" } })).toBe(true);
    const none = withSnapshots(admission({ broadcast: null }), [fleet()]);
    expect(watchesBroadcast({ ...none, ui: { ...none.ui, mode: "broadcast" } })).toBe(false);
  });

  it("shows the switch locked on, with its reason, to a late viewer", () => {
    const t = broadcastToggle(withSnapshots(admission(LATE), [fleet()]))!;
    expect(t).toMatchObject({ on: true, disabled: true });
    expect(t.note).toContain("Direct streams are full");
    const free = broadcastToggle(withSnapshots(admission({ ...LATE, seat_free: true }), [fleet()]))!;
    expect(free.note).toContain("reload the page");
    const seated = broadcastToggle(withSnapshots(admission(), [fleet()]))!;
    expect(seated).toMatchObject({ on: false, disabled: false });
    expect(broadcastToggle(withSnapshots(admission({ broadcast: null }), [fleet()]))).toBeNull();
  });
});

function view(s: ClientState) {
  return wallView({ state: s, now: EPOCH, dpr: 1, mosaicArea: { x: 0, y: 0, w: 1600, h: 930 }, canDecode: true, phone: false });
}

describe("the page in broadcast mode", () => {
  it("opens no direct stream for a late viewer, whatever its mode says, and plays the broadcast", () => {
    const late = withSnapshots(admission(LATE), [fleet()]);
    for (const mode of ["direct", "aggregated", "broadcast"] as const) {
      const v = view({ ...late, ui: { ...late.ui, mode } });
      expect(v.plan.direct).toEqual([]);
      expect(v.plan.aggregated).toEqual([]);
      expect(v.plan.suspended).toBe(true);
      expect(v.broadcast?.url).toBe(PLAYBACK);
      expect(v.ownButton).toBeNull();
      expect(v.play!.broadcast).toMatchObject({ on: true, disabled: true });
      expect(v.play!.streams.disabled).toBe(true);
    }
  });

  it("keeps the scene's inputs and drops the VM controls", () => {
    const late = withSnapshots(admission(LATE), [fleet()]);
    const v = view({ ...late, ui: { ...late.ui, focusId: late.accepted!.tiles[0]!.id } });
    expect(v.focus).toBeNull();
    expect(v.play!.word.label).not.toBe("");
    expect(v.play!.light.label).not.toBe("");
    expect(v.play!.disabled).toBe(false);
  });

  it("gives a seated viewer's direct streams its token", () => {
    const seated = withSnapshots(admission(), [fleet()]);
    const v = view({ ...seated, ui: { ...seated.ui, mode: "direct" } });
    expect(v.plan.direct.length).toBeGreaterThan(0);
    for (const d of v.plan.direct) expect(d.url).toContain("admit=v1.9999999999.v-1.mac");
    expect(v.broadcast).toBeNull();
    expect(v.play!.broadcast).toMatchObject({ on: false, disabled: false });
  });
});

describe("frame URLs and the direct token", () => {
  it("carries the token, and a renewed one is still the same stream", () => {
    const a = frameSocketUrl("wss://slot-1.example/frames", 270, "std", "tok-1");
    expect(a).toBe("wss://slot-1.example/frames?h=270&admit=tok-1");
    const b = frameSocketUrl(a, 360, "std", "tok-2");
    expect(b).toBe("wss://slot-1.example/frames?h=360&admit=tok-2");
    expect(sameStreamAtAnyHeight(a, b)).toBe(true);
    expect(frameSocketUrl("wss://x/frames", 180)).toBe("wss://x/frames?h=180");
  });
});

describe("the broadcast's own state on the page", () => {
  const media = { base: "/media", meta: null };
  function asleepThenAwake(s: ClientState): ClientState {
    // The recording went on while the fleet slept; the snapshot now says awake.
    return { ...s, ui: { ...s.ui, reelOn: true } };
  }

  it("lets the recording go once the broadcast presents a frame", () => {
    const late = asleepThenAwake(withSnapshots(admission(LATE), [fleet()]));
    const waiting = wallView({ state: late, now: EPOCH, dpr: 1, mosaicArea: { x: 0, y: 0, w: 1600, h: 930 }, canDecode: true, phone: false, media });
    expect(waiting.reel?.leaving).toBe(false);
    expect(waiting.live).toBe(false);
    const playing = wallView({
      state: late,
      now: EPOCH,
      dpr: 1,
      mosaicArea: { x: 0, y: 0, w: 1600, h: 930 },
      canDecode: true,
      phone: false,
      media,
      broadcastLatencyMs: 1800,
    });
    expect(playing.reel?.leaving).toBe(true);
    expect(playing.live).toBe(true);
  });

  it("tells the player the page is paused", () => {
    const late = withSnapshots(admission(LATE), [fleet()]);
    expect(view(late).broadcast?.paused).toBe(false);
    expect(view({ ...late, ui: { ...late.ui, paused: true } }).broadcast?.paused).toBe(true);
  });
});
