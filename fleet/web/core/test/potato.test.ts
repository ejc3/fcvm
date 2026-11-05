import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import {
  arcMs,
  arcPoint,
  ballAnchor,
  ballOf,
  brokeText,
  dropCard,
  dropHeadline,
  ladderOf,
  LINE_MIN_TILE,
  MAX_TILE,
  NO_HOST,
  NO_POTATO,
  paceOf,
  potatoColour,
  potatoFasterButton,
  potatoHue,
  potatoNotice,
  potatoRestartButton,
  potatoView,
  recordsOf,
  ringGeometry,
  type PotatoInput,
} from "../potato.ts";
import { parseServerMsg, ProtocolError, type Potato, type PotatoDrop, type PotatoRound } from "../protocol.ts";
import { initialState, reduce, type ClientState } from "../store.ts";
import { EPOCH, fleet, snapshotText } from "./fixtures.ts";

const T = EPOCH + 100_000;

/** PROTOCOL.md's example `state.potato`, with its `last_drop` written out. */
const BROKE: PotatoDrop = {
  reason: "not_listening",
  player: 1,
  thrower: 0,
  seq: 11,
  lap: 5,
  hold_ms: 1182,
  stage: "waking",
  stage_ms: 90.0,
  listening_after_ms: 84.0,
  sleep_stages: { prepare_ms: 17.6, pause_ms: 0.5, save_ms: 74.5, disk_ms: 114.7, stop_ms: 106.7 },
  wake_stages: { restore_ms: 352.0, ack_ms: 201.0, listen_ms: 355.3 },
  at: T - 30_000,
  round: 7,
  size: 2,
  detail: "player 1 was waking when player 0 threw (1 tries in 100 ms: connection timed out)",
};

function round(over: Partial<PotatoRound> = {}): PotatoRound {
  return {
    id: 8,
    size: 4,
    ring: [0, 1, 2, 3],
    started_at: T - 4000,
    seq: 6,
    lap: 1,
    hold_ms: 599,
    start_hold_ms: 666,
    min_hold_ms: 2,
    flight_ms: 100,
    pace_ms: null,
    turnaround_ms: 1797,
    holder: 2,
    last_catch_at: T - 900,
    flight: { from: 2, to: 3, at: T - 300 },
    best_lap_hold_ms: 666,
    ...over,
  };
}

function potato(over: Partial<Potato> = {}): Potato {
  return {
    size: 4,
    colours: 12,
    capacity: 96,
    next_round_at: null,
    ladder: [
      { size: 2, record_hold_ms: 1458, record_at: T - 30_000, rounds: 3, broke: BROKE },
      { size: 4, record_hold_ms: null, record_at: null, broke: null, rounds: 0 },
      { size: 8, record_hold_ms: null, record_at: null, broke: null, rounds: 0 },
    ],
    round: round(),
    players: [
      { player: 0, colour: 5, counter: 31, phase: "asleep", since: T - 600, last_sleep_ms: 330.1, last_wake_ms: 355.3 },
      { player: 1, colour: 6, counter: 30, phase: "waking", since: T - 200, last_sleep_ms: 340, last_wake_ms: 400 },
      { player: 2, colour: 7, counter: 31, phase: "awake", since: T - 5000, last_sleep_ms: 300, last_wake_ms: 360 },
      { player: 3, colour: 11, counter: 30, phase: "failed", since: T - 50, last_sleep_ms: null, last_wake_ms: null },
    ],
    records: {
      fastest_cycle_ms: { value: 650.0, size: 2, player: 0, at: T - 30_000 },
      longest_round: { value: 151.0, size: 64, player: null, at: T - 30_000 },
    },
    last_drop: BROKE,
    ...over,
  };
}

function stateWith(p: Potato | null, you = "v-1", actions: ReturnType<typeof fleet>["actions"] = []): ClientState {
  let s = reduce(initialState(), { type: "socket", status: "open", at: T });
  s = reduce(s, { type: "message", text: JSON.stringify({ type: "hello", viewer: you, turnstile: null }), at: T });
  return reduce(s, { type: "message", text: snapshotText(1, { ...fleet(), actions, potato: p }), at: T });
}

const STAGE = { x: 0, y: 0, w: 600, h: 600 };
const input = (s: ClientState, over: Partial<PotatoInput> = {}): PotatoInput => ({ state: s, now: T, stage: STAGE, reducedMotion: false, ...over });
const dist = (a: { x: number; y: number }, b: { x: number; y: number }) => Math.hypot(a.x - b.x, a.y - b.y);

describe("potato in the snapshot", () => {
  const raw = JSON.parse(readFileSync(new URL("./fixtures/coordinator.json", import.meta.url), "utf8")) as { views: Record<string, Record<string, unknown>> };
  const parse = (state: unknown) => {
    const m = parseServerMsg(JSON.stringify({ type: "snapshot", version: 1, state }));
    if (m.type !== "snapshot") throw new Error("not a snapshot");
    return m.state;
  };

  it("is null where the coordinator sends none", () => {
    expect(raw.views["01_intent_idempotency"]!["potato"]).toBeUndefined();
    expect(parse(raw.views["01_intent_idempotency"]).potato).toBeNull();
  });

  it("reads the coordinator's potato vector: the ladder, the drop with its listening time, the records", () => {
    const p = parse(raw.views["33_potato"]).potato!;
    expect(p.size).toBe(2);
    expect(p.colours).toBe(12);
    expect(p.capacity).toBe(0);
    expect(p.round).toBeNull();
    expect(p.ladder.map((r) => [r.size, r.record_hold_ms, r.rounds])).toEqual([[2, 2000, 1], [4, null, 1], [8, null, 0], [16, null, 0], [32, null, 0], [64, null, 0], [96, null, 0]]);
    expect(p.ladder[0]!.broke).toMatchObject({ reason: "proof", player: 1, stage: "proof", seq: 3 });
    expect(p.last_drop).toMatchObject({ reason: "not_listening", player: 1, thrower: 0, stage: "waking", stage_ms: 90, listening_after_ms: 84, sleep_stages: { save_ms: 61 }, wake_stages: {} });
    expect(p.records.fastest_cycle_ms).toEqual({ value: 650, size: 2, player: 0, at: 4750 });
    expect(p.records.longest_round).toEqual({ value: 3, size: 2, player: null, at: 7901 });
  });

  it("reads PROTOCOL.md's example with a round in play", () => {
    const st = parse({ ...raw.views["01_intent_idempotency"], potato: potato() });
    expect(st.potato).toEqual(potato());
  });

  it("refuses a ring that names a player twice, and colours that are not a positive whole number", () => {
    expect(() => parse({ ...raw.views["01_intent_idempotency"], potato: potato({ round: round({ ring: [0, 1, 1, 3] }) }) })).toThrow(ProtocolError);
    expect(() => parse({ ...raw.views["01_intent_idempotency"], potato: potato({ colours: 0 }) })).toThrow(/colours/);
  });
});

describe("the ring", () => {
  for (const [n, area] of [
    [2, STAGE],
    [8, STAGE],
    [96, STAGE],
    [96, { x: 0, y: 0, w: 358, h: 358 }],
  ] as const) {
    it(`places ${n} tiles on a circle in ${area.w}x${area.h}: the first at the top, clockwise, none touching, all inside`, () => {
      const g = ringGeometry(area, n);
      expect(g.places).toHaveLength(n);
      expect(g.tile).toBeGreaterThan(0);
      expect(g.tile).toBeLessThanOrEqual(MAX_TILE);
      expect(g.places[0]!.x).toBeCloseTo(g.centre.x, 6);
      expect(g.places[0]!.y).toBeLessThan(g.centre.y);
      // clockwise on screen: the second place is right of the first (a ring of two has it straight below)
      if (n > 2) expect(g.places[1]!.x).toBeGreaterThan(g.places[0]!.x);
      else expect(g.places[1]!.y).toBeGreaterThan(g.centre.y);
      for (const p of g.places) {
        expect(dist(p, g.centre)).toBeCloseTo(g.radius, 6);
        expect(p.x - g.tile / 2).toBeGreaterThanOrEqual(area.x);
        expect(p.y - g.tile / 2).toBeGreaterThanOrEqual(area.y);
        expect(p.x + g.tile / 2).toBeLessThanOrEqual(area.x + area.w);
        expect(p.y + g.tile / 2).toBeLessThanOrEqual(area.y + area.h);
      }
      // neighbouring squares do not overlap: their centres are a tile apart along x or y
      for (let i = 0; i < n; i++) {
        const a = g.places[i]!;
        const b = g.places[(i + 1) % n]!;
        expect(Math.max(Math.abs(a.x - b.x), Math.abs(a.y - b.y)), `places ${i} and ${(i + 1) % n}`).toBeGreaterThan(g.tile);
      }
    });
  }

  it("draws each player's tile with its colour, phase, number and line", () => {
    const v = potatoView(input(stateWith(potato())));
    const tiles = v.ring!.tiles;
    expect(tiles.map((t) => t.player)).toEqual([0, 1, 2, 3]);
    expect(tiles.map((t) => t.phase)).toEqual(["asleep", "waking", "awake", "failed"]);
    expect(tiles.map((t) => t.colour)).toEqual([potatoColour(5, 12), potatoColour(6, 12), potatoColour(7, 12), potatoColour(11, 12)]);
    expect(tiles.map((t) => t.number)).toEqual(["0", "1", "2", "3"]);
    expect(tiles[0]!.box.w).toBeGreaterThanOrEqual(LINE_MIN_TILE);
    expect(tiles.map((t) => t.line)).toEqual(["asleep", "waking 200 ms", "has the ball", "failed"]);
    expect(tiles.map((t) => t.holder)).toEqual([false, false, true, false]);
    // waking: 200 ms of a last wake of 400
    expect(tiles[1]!.wake).toEqual({ fraction: 0.5, text: "200 ms" });
    expect(tiles[1]!.ariaLabel).toBe("Player 1, waking for 200 ms; its last wake took 400 ms, 30 catches");
    expect(tiles[0]!.ariaLabel).toBe("Player 0, asleep for 600 ms, 31 catches");
    expect(tiles[2]!.ariaLabel).toBe("Player 2, awake, 31 catches, holding the ball");
    expect(v.ring!.caption).toBe("Ring of 4: player 2 has the ball, catch 7");
  });

  it("shows a player that threw as going to sleep until its sleep is done", () => {
    const p = potato();
    p.players[2] = { ...p.players[2]!, phase: "sleeping", since: T - 120 };
    const t = potatoView(input(stateWith(p))).ring!.tiles[2]!;
    expect(t.phase).toBe("sleeping");
    expect(t.line).toBe("going to sleep");
    expect(t.ariaLabel).toContain("going to sleep for 120 ms");
  });

  it("caps the wake bar at its end, has no bar without a last wake, and shows a ring member that has not said hello as not started", () => {
    const p = potato({
      players: [
        { player: 0, colour: 0, counter: 0, phase: "waking", since: T - 900, last_sleep_ms: null, last_wake_ms: 400 },
        { player: 1, colour: 0, counter: 0, phase: "waking", since: T - 100, last_sleep_ms: null, last_wake_ms: null },
      ],
    });
    const tiles = potatoView(input(stateWith(p))).ring!.tiles;
    expect(tiles[0]!.wake).toEqual({ fraction: 1, text: "900 ms" });
    expect(tiles[1]!.wake).toEqual({ fraction: 0, text: "100 ms" });
    expect(tiles.slice(2).map((t) => [t.phase, t.line])).toEqual([["absent", "not started"], ["absent", "not started"]]);
  });

  it("leaves numbers and lines out of tiles too small to hold 14 px text", () => {
    const players = Array.from({ length: 96 }, (_, i) => i);
    const p = potato({ size: 96, round: round({ size: 96, ring: players, holder: 5, flight: null }), players: [] });
    const tiles = potatoView(input(stateWith(p), { stage: { x: 0, y: 0, w: 358, h: 358 } })).ring!.tiles;
    expect(tiles).toHaveLength(96);
    expect(tiles.every((t) => t.number === null && t.line === null)).toBe(true);
  });
});

describe("colours", () => {
  it("spaces `colours` hues evenly, red first, and wraps an index", () => {
    expect([0, 1, 3, 6, 11].map((k) => potatoHue(k, 12))).toEqual([0, 30, 90, 180, 330]);
    expect(potatoHue(13, 12)).toBe(30);
    expect(potatoHue(-1, 12)).toBe(330);
    expect(potatoHue(2, 3)).toBe(240);
    expect(potatoColour(4, 12)).toBe("hsl(120 72% 52%)");
  });

  it("steps a tile's colour with its player's colour index", () => {
    const at = (c: number) => potatoView(input(stateWith(potato({ players: [{ player: 0, colour: c, counter: 1, phase: "awake", since: T, last_sleep_ms: null, last_wake_ms: null }] })))).ring!.tiles[0]!.colour;
    expect(new Set([0, 1, 2, 3].map(at)).size).toBe(4);
    expect(at(12)).toBe(at(0));
  });
});

describe("the ball", () => {
  const g = ringGeometry(STAGE, 4);

  it("arcs from the thrower to the catcher over half the hold, bowed away from the centre", () => {
    const r = round({ flight: { from: 2, to: 3, at: T }, hold_ms: 400 });
    expect(arcMs(400)).toBe(200);
    expect(arcMs(5000)).toBe(300);
    expect(arcMs(20)).toBe(60);
    const start = ballOf(r, g, T, false)!;
    expect(start.at).toEqual(ballAnchor(g, 2));
    expect(start.flying).toBe(true);
    const mid = ballOf(r, g, T + 100, false)!;
    const chordMid = { x: (ballAnchor(g, 2).x + ballAnchor(g, 3).x) / 2, y: (ballAnchor(g, 2).y + ballAnchor(g, 3).y) / 2 };
    expect(dist(mid.at, g.centre)).toBeGreaterThan(dist(chordMid, g.centre) + 10);
    const end = ballOf(r, g, T + 200, false)!;
    expect(end.at.x).toBeCloseTo(ballAnchor(g, 3).x, 6);
    expect(end.at.y).toBeCloseTo(ballAnchor(g, 3).y, 6);
    expect(end.flying).toBe(false);
    // later still and not caught: it waits at the catcher
    expect(ballOf(r, g, T + 5000, false)!.at).toEqual(end.at);
  });

  it("arcs into the holder from the player before it after a catch, then rests on the holder", () => {
    const r = round({ flight: null, holder: 3, seq: 7, last_catch_at: T, hold_ms: 400 });
    const a = ballOf(r, g, T, false)!;
    expect(a.at).toEqual(ballAnchor(g, 2));
    expect(ballOf(r, g, T + 100, false)!.flying).toBe(true);
    const rest = ballOf(r, g, T + 2000, false)!;
    expect(rest.at).toEqual(ballAnchor(g, 3));
    expect(rest.flying).toBe(false);
  });

  it("starts on the first catcher with no arc, is absent before the first catch, and jumps with reduced motion", () => {
    expect(ballOf(round({ flight: null, holder: 0, seq: 0, last_catch_at: T }), g, T, false)!.at).toEqual(ballAnchor(g, 0));
    expect(ballOf(round({ flight: null, holder: null, seq: null, last_catch_at: null }), g, T, false)).toBeNull();
    const r = round({ flight: { from: 2, to: 3, at: T }, hold_ms: 400 });
    expect(ballOf(r, g, T, true)!.at.x).toBeCloseTo(ballAnchor(g, 3).x, 6);
  });

  it("rests on the tile's corner, so the player's number stays in view", () => {
    for (let i = 0; i < 4; i++) {
      const a = ballAnchor(g, i);
      const p = g.places[i]!;
      // inside the tile, and more than a ball's radius from its centre, where the number is
      expect(Math.abs(a.x - p.x)).toBeLessThan(g.tile / 2);
      expect(Math.abs(a.y - p.y)).toBeLessThan(g.tile / 2);
      expect(dist(a, p)).toBeGreaterThan(Math.min(28, g.tile * 0.4) / 2 + 7);
    }
  });

  it("bows a throw across the centre (a ring of two) to one side, not through the middle", () => {
    const g2 = ringGeometry(STAGE, 2);
    const mid = arcPoint(g2.places[0]!, g2.places[1]!, g2.centre, 0.5);
    expect(dist(mid, g2.centre)).toBeGreaterThan(g2.radius * 0.3);
  });

  it("lies inside the ring beside the catcher that missed it while the next round waits", () => {
    const p = potato({ round: null, size: 4, next_round_at: T + 3000, last_drop: { ...BROKE, size: 2, player: 1 } });
    const ring = potatoView(input(stateWith(p))).ring!;
    expect(ring.tiles.map((t) => t.player)).toEqual([0, 1]);
    expect(ring.ball!.dropped).toBe(true);
    expect(dist(ring.ball!.at, ring.geometry.centre)).toBeLessThan(ring.geometry.radius);
    expect(ring.caption).toBe("Ring of 2: the ball dropped");
  });
});

describe("the pace", () => {
  it("meters each player's time to sleep and wake against the slowest sleep and wake in the ring", () => {
    const v = paceOf(potato(), round());
    expect(v.hold).toBe("599 ms");
    expect(v.where).toBe("Lap 2 · ring of 4 · catch 7");
    expect(v.meter.turnaroundText).toBe("1797 ms for each player to sleep and wake (3 holds)");
    // players 0, 1 and 2 have both: 685.4, 740 and 660 ms
    expect(v.meter.needText).toBe("slowest sleep and wake in the ring: 740 ms");
    expect(v.meter.short).toBe(false);
    expect(v.meter.turnaround).toBeCloseTo(1797 / (1797 * 1.15), 6);
    expect(v.meter.need).toBeCloseTo(740 / (1797 * 1.15), 6);
    expect(v.pace).toBeNull();
    expect(v.start).toBe("Started at 666 ms, a tenth shorter each lap; floor 2 ms");
  });

  it("says when the hold leaves a player less than it needs, and the viewers' cap", () => {
    const v = paceOf(potato(), round({ hold_ms: 200, turnaround_ms: 600, pace_ms: 200, seq: null }));
    expect(v.meter.short).toBe(true);
    expect(v.meter.need).toBeCloseTo(1 / 1.15, 6);
    expect(v.pace).toBe("Faster: holds capped at 200 ms");
    expect(v.where).toBe("Lap 2 · ring of 4 · no catch yet");
  });
});

describe("drops, the ladder and the records", () => {
  it("names the player and the late stage with its milliseconds", () => {
    expect(dropHeadline(BROKE)).toBe("Player 1 was still waking, 90 ms into its wake, when the ball dropped.");
    expect(dropHeadline({ ...BROKE, stage: "asleep", stage_ms: 412.4 })).toBe("Player 1 was still asleep when the ball dropped; it had slept 412 ms.");
    expect(dropHeadline({ ...BROKE, stage: "sleeping", stage_ms: 30 })).toBe("Player 1 was still going to sleep, 30 ms in, when the ball dropped.");
    expect(dropHeadline({ ...BROKE, reason: "proof", stage: "proof" })).toBe("Player 1's colour, count or chain was not the step of its last catch.");
    expect(dropHeadline({ ...BROKE, reason: "wake_failed", stage: "wake" })).toBe("Player 1's wake failed.");
    expect(dropHeadline({ ...BROKE, reason: "deadline", stage: "report" })).toBe("No catch was reported in time; player 1 was next.");
    expect(dropHeadline({ ...BROKE, reason: "host_gone", player: null })).toBe("The host running the players disconnected.");
    expect(brokeText(BROKE)).toBe("player 1 still waking (90 ms)");
    expect(brokeText({ ...BROKE, reason: "sleep_failed" })).toBe("player 1's sleep failed");
  });

  it("shows the drop card while the next round is pending, with when the catcher started accepting", () => {
    const p = potato({ round: null, size: 4, next_round_at: T + 2500 });
    const v = potatoView(input(stateWith(p)));
    expect(v.drop).toEqual({
      title: "The ball dropped",
      headline: "Player 1 was still waking, 90 ms into its wake, when the ball dropped.",
      listening: "It started accepting 84 ms after the ball dropped.",
      context: "Ring of 2, lap 6, at a hold of 1182 ms, after 11 catches.",
      stages: ["Its last sleep: prepare 17.6 ms, pause 0.5 ms, save 74.5 ms, disk 114.7 ms, stop 106.7 ms", "Its last wake: restore 352 ms, ack 201 ms, listen 355.3 ms"],
      next: "Next: a ring of 4 in 3 s",
      detail: BROKE.detail,
    });
    expect(dropCard(p, { ...BROKE, listening_after_ms: null, sleep_stages: {}, wake_stages: {} }, T).listening).toBeNull();
    // Not during a round, nor when no next round is due.
    expect(potatoView(input(stateWith(potato()))).drop).toBeNull();
    expect(potatoView(input(stateWith(potato({ round: null, next_round_at: null })))).drop).toBeNull();
  });

  it("fills the ladder: each size's record, what broke it, its rounds, the current size and sizes the host skips", () => {
    const l = ladderOf(potato({ capacity: 4 }));
    expect(l.head).toEqual({ players: "Ring", record: "Record hold", broke: "What ended its last round", rounds: "Rounds" });
    expect(l.rows.map((r) => [r.players, r.record, r.broke, r.rounds, r.current, r.skipped])).toEqual([
      ["2", "1458 ms", "player 1 still waking (90 ms), lap 6 at 1182 ms", "3", false, false],
      ["4", "--", "", "0", true, false],
      ["8", "--", "skipped: the host runs 4 players", "0", false, true],
    ]);
    const tried = ladderOf(potato({ ladder: [{ size: 2, record_hold_ms: null, record_at: null, broke: { ...BROKE, lap: 0 }, rounds: 1 }] }));
    expect(tried.rows[0]!.record).toBe("no lap completed");
  });

  it("states the records", () => {
    expect(recordsOf(potato()).map((r) => [r.label, r.value, r.note])).toEqual([
      ["Fastest sleep and wake", "650 ms", "player 0 in a ring of 2"],
      ["Longest round", "151 catches", "a ring of 64"],
    ]);
    expect(recordsOf(potato({ records: { fastest_cycle_ms: null, longest_round: null } })).map((r) => [r.value, r.note])).toEqual([
      ["--", "none yet"],
      ["--", "none yet"],
    ]);
  });
});

describe("Faster and Restart", () => {
  const committed = (name: string, at: number, viewer = "v-1") => ({ at, viewer, id: `${name}-${at}`, name, args: {}, summary: name, outcome: "committed" });

  it("Faster says what it cuts the hold to, and is off without a round, at the floor, while pending, and for 2 s after this viewer's last", () => {
    expect(potatoFasterButton(stateWith(potato()), potato(), T)).toEqual({ label: "Faster", disabled: false, note: "cuts the hold to 480 ms" });
    expect(potatoFasterButton(stateWith(potato()), potato({ round: null }), T)).toEqual({ label: "Faster", disabled: true, note: "works while a round is being played" });
    const floor = potato({ round: round({ hold_ms: 2 }) });
    expect(potatoFasterButton(stateWith(floor), floor, T).note).toBe("the hold is at its floor");
    const mine = stateWith(potato(), "v-1", [committed("potato_faster", T - 600)]);
    expect(potatoFasterButton(mine, potato(), T)).toEqual({ label: "Faster", disabled: true, note: "again in 2 s" });
    expect(potatoFasterButton(mine, potato(), T + 1000).note).toBe("again in 1 s");
    expect(potatoFasterButton(mine, potato(), T + 1399).disabled).toBe(true);
    expect(potatoFasterButton(mine, potato(), T + 1400).disabled).toBe(false);
    // another viewer's faster does not hold this one back
    const theirs = stateWith(potato(), "v-1", [committed("potato_faster", T - 600, "v-2")]);
    expect(potatoFasterButton(theirs, potato(), T).disabled).toBe(false);
    const pending = reduce(stateWith(potato()), { type: "intent-sent", intent: { type: "intent", id: "f1", name: "potato_faster", args: {} }, at: T });
    expect(potatoFasterButton(pending, potato(), T)).toEqual({ label: "Faster requested", disabled: true, note: "" });
  });

  it("Restart waits 10 s into a round and 10 s after this viewer's last, and says which ring it starts again", () => {
    const fresh = potato({ round: round({ started_at: T - 4000 }) });
    expect(potatoRestartButton(stateWith(fresh), fresh, T)).toEqual({ label: "Restart", disabled: true, note: "possible 10 s into a round, in 6 s" });
    const old = potato({ round: round({ started_at: T - 60_000 }) });
    expect(potatoRestartButton(stateWith(old), old, T)).toEqual({ label: "Restart", disabled: false, note: "starts the ring of 4 again" });
    const mine = stateWith(old, "v-1", [committed("potato_restart", T - 1000)]);
    expect(potatoRestartButton(mine, old, T)).toEqual({ label: "Restart", disabled: true, note: "again in 9 s" });
    const gone = potato({ round: null, capacity: 0 });
    expect(potatoRestartButton(stateWith(gone), gone, T).note).toBe("works while a host runs players");
  });

  it("both pause while the coordinator is offline", () => {
    const s = reduce(stateWith(potato()), { type: "socket", status: "closed", at: T });
    const v = potatoView(input(s));
    expect([v.faster.disabled, v.faster.note, v.restart.disabled]).toEqual([true, "paused while the coordinator is offline", true]);
    expect(v.banner).toBe("The coordinator is offline. The game is shown as it last was; the buttons wait until it is back.");
  });

  it("puts a refusal on screen as the coordinator gave it", () => {
    let s = reduce(stateWith(potato()), { type: "intent-sent", intent: { type: "intent", id: "r1", name: "potato_restart", args: {} }, at: T });
    s = reduce(s, { type: "message", text: JSON.stringify({ type: "receipt", id: "r1", outcome: "refused", reason: "the round has only just started", version: 2 }), at: T });
    expect(potatoNotice(s.notices)).toBe("Restart was not applied: the round has only just started");
    expect(potatoView(input(s)).notice).toBe("Restart was not applied: the round has only just started");
    expect(s.pending).toEqual([]);
  });
});

describe("the view", () => {
  it("says what it is waiting for when there is no game", () => {
    expect(potatoView(input(initialState())).empty).toBe("Connecting to the fleet.");
    expect(potatoView(input(reduce(initialState(), { type: "socket", status: "closed", at: T }))).empty).toBe("Cannot reach the fleet yet. Trying again.");
    const none = potatoView(input(stateWith(null)));
    expect(none.empty).toBe(NO_POTATO);
    expect([none.ring, none.ladder, none.faster.disabled]).toEqual([null, null, true]);
    // a host that left: the ladder and the records stay, the ring goes
    const gone = potatoView(input(stateWith(potato({ round: null, capacity: 0, next_round_at: null }))));
    expect(gone.empty).toBe(NO_HOST);
    expect(gone.ring).toBeNull();
    expect(gone.ladder!.rows).toHaveLength(3);
  });

  it("draws the next ring between rounds with no drop pending, and no ring without a measured stage", () => {
    const v = potatoView(input(stateWith(potato({ round: null, size: 8, last_drop: null, next_round_at: T + 1000 }))));
    expect(v.ring!.tiles).toHaveLength(8);
    expect(v.ring!.caption).toBe("Next: a ring of 8");
    expect(v.ring!.ball).toBeNull();
    expect(v.pace).toBeNull();
    expect(potatoView(input(stateWith(potato()), { stage: null })).ring).toBeNull();
  });

  it("reads the time on the coordinator's clock", () => {
    // the page's clock runs 5 s behind the coordinator's
    let s = stateWith(potato());
    s = reduce(s, { type: "ping-sent", at: T - 5000 });
    s = reduce(s, { type: "message", text: JSON.stringify({ type: "clock", server_unix_ms: T, client_unix_ms: T - 5000 }), at: T - 5000 });
    const tiles = potatoView(input(s, { now: T - 5000 })).ring!.tiles;
    expect(tiles[1]!.wake!.text).toBe("200 ms");
  });
});
