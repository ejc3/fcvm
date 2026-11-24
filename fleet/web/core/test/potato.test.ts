import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import {
  arcMs,
  arcPoint,
  ballAnchor,
  ballOf,
  causeOf,
  chartOf,
  cpuSlotsOf,
  densityOf,
  arcOf,
  fmtMs,
  ladderBarsOf,
  perCpu,
  slotsLayout,
  centreClear,
  dropCard,
  dropHeadline,
  LINE_MIN_TILE,
  MAX_TILE,
  NO_HOST,
  NO_POTATO,
  potatoColour,
  potatoFasterButton,
  potatoHue,
  potatoNotice,
  potatoRestartButton,
  potatoView,
  ringGeometry,
  type PotatoInput,
  tileReadout,
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
  cause: null,
  cause_ms: null,
  pace: null,
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
    pace: null,
    best_pace: null,
    laps: [],
    cpus_in_use: null,
    cpus_peak: null,
    asleep: null,
    lead: null,
    slowest_wake_ms: null,
    cpus_busy: null,
    cpu_slots: null,
    ...over,
  };
}

function potato(over: Partial<Potato> = {}): Potato {
  return {
    size: 4,
    colours: 12,
    capacity: 96,
    cpus: null,
    confined: null,
    next_round_at: null,
    ladder: [
      { size: 2, record_hold_ms: 1458, record_at: T - 30_000, record_pace: null, record_pace_at: null, record_prev_pace: null, rounds: 3, broke: BROKE },
      { size: 4, record_hold_ms: null, record_at: null, record_pace: null, record_pace_at: null, record_prev_pace: null, broke: null, rounds: 0 },
      { size: 8, record_hold_ms: null, record_at: null, record_pace: null, record_pace_at: null, record_prev_pace: null, broke: null, rounds: 0 },
    ],
    round: round(),
    players: [
      { player: 0, colour: 5, counter: 31, phase: "asleep", since: T - 600, last_sleep_ms: 330.1, last_wake_ms: 355.3, cpu_wait_ms: null, ready_margin_ms: null },
      { player: 1, colour: 6, counter: 30, phase: "waking", since: T - 200, last_sleep_ms: 340, last_wake_ms: 400, cpu_wait_ms: null, ready_margin_ms: null },
      { player: 2, colour: 7, counter: 31, phase: "awake", since: T - 5000, last_sleep_ms: 300, last_wake_ms: 360, cpu_wait_ms: null, ready_margin_ms: null },
      { player: 3, colour: 11, counter: 30, phase: "failed", since: T - 50, last_sleep_ms: null, last_wake_ms: null, cpu_wait_ms: null, ready_margin_ms: null },
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

  it("reads the density fields when the coordinator sends them, and nulls when it does not", () => {
    const full = dense({}, { cpu_slots: [{ player: 2, phase: "ball" }] });
    expect(parse({ ...raw.views["01_intent_idempotency"], potato: full }).potato).toEqual(full);
    // a coordinator from before the density page: none of the new keys at all
    const bare = JSON.parse(JSON.stringify(potato())) as Record<string, unknown>;
    const drop = (o: Record<string, unknown>, keys: string[]) => Object.fromEntries(Object.entries(o).filter(([k]) => !keys.includes(k)));
    const old = { ...drop(bare, ["cpus", "confined"]), round: drop(bare["round"] as Record<string, unknown>, ["pace", "best_pace", "laps", "cpus_in_use", "cpus_peak", "asleep", "lead", "slowest_wake_ms", "cpus_busy", "cpu_slots"]) };
    const got = parse({ ...raw.views["01_intent_idempotency"], potato: old }).potato!;
    expect([got.cpus, got.confined, got.round!.pace, got.round!.laps, got.round!.cpu_slots, got.ladder[0]!.record_pace]).toEqual([null, null, null, [], null, null]);
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
    expect(tiles.map((t) => t.readout)).toEqual([
      { head: "Player 0 · asleep 600 ms", detail: "31 catches · last sleep 330 ms · last wake 355 ms" },
      { head: "Player 1 · waking 200 ms", detail: "30 catches · last sleep 340 ms · last wake 400 ms" },
      { head: "Player 2 · has the ball", detail: "31 catches · last sleep 300 ms · last wake 360 ms" },
      { head: "Player 3 · failed", detail: "30 catches" },
    ]);
    expect(v.ring!.caption).toBe("Ring of 4: player 2 has the ball, catch 7");
  });

  it("reads a player that has not started without a detail line", () => {
    expect(tileReadout(5, "not started", false, null)).toEqual({ head: "Player 5 · not started", detail: "" });
  });

  it("shows a player that threw as going to sleep until its sleep is done", () => {
    const p = potato();
    p.players[2] = { ...p.players[2]!, phase: "sleeping", since: T - 120 };
    const t = potatoView(input(stateWith(p))).ring!.tiles[2]!;
    expect(t.phase).toBe("sleeping");
    expect(t.line).toBe("going to sleep");
    expect(t.readout.head).toBe("Player 2 · going to sleep 120 ms · has the ball");
    expect(t.ariaLabel).toBe(`${t.readout.head}. ${t.readout.detail}`);
  });

  it("caps the wake bar at its end, has no bar without a last wake, and shows a ring member that has not said hello as not started", () => {
    const p = potato({
      players: [
        { player: 0, colour: 0, counter: 0, phase: "waking", since: T - 900, last_sleep_ms: null, last_wake_ms: 400, cpu_wait_ms: null, ready_margin_ms: null },
        { player: 1, colour: 0, counter: 0, phase: "waking", since: T - 100, last_sleep_ms: null, last_wake_ms: null, cpu_wait_ms: null, ready_margin_ms: null },
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
    const at = (c: number) => potatoView(input(stateWith(potato({ players: [{ player: 0, colour: c, counter: 1, phase: "awake", since: T, last_sleep_ms: null, last_wake_ms: null, cpu_wait_ms: null, ready_margin_ms: null }] })))).ring!.tiles[0]!.colour;
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

/** A ring of 8 on 4 CPUs mid-round: machines 0 and 1 asleep, 2 holds the ball, 3 and 4 are ready, 5 is waking, 6 is saving, 7 waits for the ball's next lap. */
function dense(over: Partial<Potato> = {}, rOver: Partial<PotatoRound> = {}): Potato {
  const mk = (player: number, phase: string, o: object = {}) => ({ player, colour: player % 12, counter: 5, phase, since: T - 100, last_sleep_ms: 300, last_wake_ms: 310, cpu_wait_ms: null, ready_margin_ms: null, ...o });
  return potato({
    cpus: 4,
    confined: true,
    size: 8,
    capacity: 8,
    round: round({
      size: 8,
      ring: [0, 1, 2, 3, 4, 5, 6, 7],
      holder: 2,
      seq: 9,
      lap: 1,
      pace: 5.24,
      best_pace: 5.3,
      asleep: 4,
      slowest_wake_ms: 310,
      cpus_in_use: 4,
      laps: [
        { lap: 0, pace: 2.5, hold_ms: 400, cpus_peak: 2, cpus_busy: null },
        { lap: 1, pace: 5, hold_ms: 200, cpus_peak: 3, cpus_busy: null },
      ],
      ...rOver,
    }),
    players: [mk(0, "asleep"), mk(1, "asleep"), mk(2, "awake"), mk(3, "awake"), mk(4, "asleep"), mk(5, "waking"), mk(6, "sleeping"), mk(7, "asleep")],
    ladder: [
      { size: 8, record_hold_ms: null, record_at: null, record_pace: 5.3, record_pace_at: T - 1000, record_prev_pace: null, broke: null, rounds: 2 },
      { size: 16, record_hold_ms: null, record_at: null, record_pace: null, record_pace_at: null, record_prev_pace: null, broke: null, rounds: 0 },
    ],
    ...over,
  });
}

describe("the density figures", () => {
  it("states machines per CPU, whole or to one decimal", () => {
    expect(perCpu(32, 8)).toBe("4");
    expect(perCpu(8, 3)).toBe("2.7");
    expect(perCpu(96, 8)).toBe("12");
  });

  it("puts machines per CPU in the large type, with the machines and CPUs, percent asleep, pace and the slowest wake", () => {
    const d = densityOf(dense(), dense().round!);
    expect(d.heading).toBe("Machines per CPU");
    expect(d.big).toBe("2");
    expect(d.machines).toBe("8 machines · 4 CPUs");
    expect(d.where).toBe("ring of 8 · lap 2");
    expect(d.figures).toEqual([
      { key: "asleep", value: "50%", label: "asleep" },
      { key: "pace", value: "5.2", label: "catches / s" },
      { key: "slowest", value: "310 ms", label: "slowest wake" },
      { key: "fastest", value: "650 ms", label: "fastest sleep and wake" },
    ]);
    // the record's pill needs the record before it: none is sent, so none is drawn
    expect(d.record).toBeNull();
    const rung = (prev: number | null) => dense({ ladder: [{ size: 8, record_hold_ms: null, record_at: null, record_pace: 5.3, record_pace_at: T - 1000, record_prev_pace: prev, broke: null, rounds: 2 }] });
    expect(densityOf(rung(4.94), rung(4.94).round!).record).toEqual({ was: "4.9" });
    expect(densityOf(rung(null), rung(null).round!).record).toBeNull();
    const earlier = dense({ ladder: [{ size: 8, record_hold_ms: null, record_at: null, record_pace: 5.3, record_pace_at: T - 90_000, record_prev_pace: 4.9, broke: null, rounds: 2 }] });
    expect(densityOf(earlier, earlier.round!).record).toBeNull();
  });

  it("says CPU slots, not CPUs, when the agent only caps the count", () => {
    const p = dense({ confined: false });
    expect(densityOf(p, p.round!).machines).toBe("8 machines · 4 CPU slots");
    expect(densityOf(p, p.round!).heading).toBe("Machines per CPU slot");
    expect(cpuSlotsOf(p, p.round!)!.heading).toBe("4 of 4 CPU slots in use");
  });

  it("draws no figure whose field is absent, and the machine count alone with no allotment (an old coordinator or agent)", () => {
    const old = potato();
    const d = densityOf(old, old.round!);
    expect(d.heading).toBe("Machines");
    expect(d.big).toBe("4");
    expect(d.machines).toBe("");
    expect(d.figures).toEqual([{ key: "fastest", value: "650 ms", label: "fastest sleep and wake" }]);
    expect(d.record).toBeNull();
    const paceOnly = potato({ round: round({ pace: 3.14159 }) });
    expect(densityOf(paceOnly, paceOnly.round!).figures.map((f) => f.key)).toEqual(["pace", "fastest"]);
    expect(cpuSlotsOf(old, old.round!)).toBeNull();
    expect(chartOf(old, old.round!)).toBeNull();
  });

  it("formats durations in seconds from a second up", () => {
    expect(fmtMs(310.4)).toBe("310 ms");
    expect(fmtMs(1254)).toBe("1.25 s");
  });
});

describe("the CPU slots", () => {
  it("holds a chip for each machine not asleep, grouped ball, ready, waking, saving, then the free ones", () => {
    const p = dense();
    const v = cpuSlotsOf(p, p.round!)!;
    // 2 has the ball, 3 is ready, 5 is waking, 6 is saving: four machines on four CPUs
    expect(v.heading).toBe("4 of 4 CPUs in use");
    expect(v.groups.map((g) => [g.key, g.label, g.slots.map((x) => x.number)])).toEqual([
      ["ball", "ball", ["2"]],
      ["ready", "ready", ["3"]],
      ["waking", "waking", ["5"]],
      ["saving", "saving", ["6"]],
    ]);
    expect(v.groups[0]!.slots[0]!.colour).toBe(potatoColour(2, 12));
    expect(v.asleep).toBe("4 of 8 asleep");
  });

  it("shows the free CPUs after the others, and does not count a sleeping machine", () => {
    const p = dense({ cpus: 6 });
    const v = cpuSlotsOf(p, p.round!)!;
    expect(v.heading).toBe("4 of 6 CPUs in use");
    expect(v.groups.at(-1)).toMatchObject({ key: "free", label: "free" });
    expect(v.groups.at(-1)!.slots).toHaveLength(2);
    expect(v.groups.at(-1)!.slots.every((x) => x.player === null && x.number === null)).toBe(true);
  });

  it("shows the CPU the ring really burned beside the count, when the agent measures it", () => {
    const p = dense({}, { cpus_busy: 2.14 });
    expect(cpuSlotsOf(p, p.round!)!.busy).toBe("2.1 busy");
    expect(cpuSlotsOf(dense(), dense().round!)!.busy).toBeNull();
    expect(cpuSlotsOf(p, p.round!)!.k).toBe(4);
  });

  it("takes the agent's slot list when it sends one, in its phases, and counts booting as waking", () => {
    const p = dense({}, { cpu_slots: [{ player: 4, phase: "ball" }, { player: 1, phase: "ready" }, { player: 0, phase: "booting" }] });
    const v = cpuSlotsOf(p, p.round!)!;
    expect(v.heading).toBe("3 of 4 CPUs in use");
    expect(v.groups.map((g) => [g.key, g.slots.map((x) => x.number)])).toEqual([
      ["ball", ["4"]],
      ["ready", ["1"]],
      ["waking", ["0"]],
      ["free", [null]],
    ]);
  });
});

describe("the arc from the ball to the furthest machine being woken", () => {
  it("spans the ring places from the ball to the farthest waking machine ahead of it", () => {
    const p = dense();
    // the ball is at index 2, the waking machine at 5: three ahead
    expect(arcOf(p.round!, p)).toEqual({ fromIndex: 2, toIndex: 5, label: "woken 3 ahead", ahead: 3 });
    const wrap = dense({}, { holder: 6 });
    // from 6 round the ring to 5: seven ahead
    expect(arcOf(wrap.round!, wrap)!.ahead).toBe(7);
  });

  it("is absent with nobody waking or no ball, and is drawn inside the ring clockwise on the view", () => {
    const quiet = dense();
    quiet.players = quiet.players.map((x) => (x.phase === "waking" ? { ...x, phase: "awake" } : x));
    expect(arcOf(quiet.round!, quiet)).toBeNull();
    const none = dense({}, { holder: null });
    expect(arcOf(none.round!, none)).toBeNull();
    const v = potatoView(input(stateWith(dense())));
    const arc = v.ring!.arc!;
    expect(arc.label).toBe("woken 3 ahead");
    expect(arc.radius).toBeLessThan(v.ring!.geometry.radius);
    expect(arc.large).toBe(false);
  });
});

describe("the arc's label and the slots' room", () => {
  const clear = (a: { x: number; y: number; w: number; h: number }, b: { x: number; y: number; w: number; h: number }) => a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y;
  it("keeps the label clear of every tile and of the box left for the slots, at every holder and span", () => {
    const stage = { x: 0, y: 0, w: 1000, h: 760 };
    for (const n of [8, 16, 32]) {
      for (let holder = 0; holder < n; holder += 3) {
        for (const ahead of [1, 3, Math.floor(n / 2) - 1]) {
          const ring = Array.from({ length: n }, (_, i) => i);
          const p = dense({ size: n }, { size: n, ring, holder, seq: 5 });
          p.players = ring.map((i) => ({ ...p.players[0]!, player: i, colour: i % 12, phase: i === (holder + ahead) % n ? "waking" : "asleep" }));
          const v = potatoView(input(stateWith(p), { stage }));
          const arc = v.ring!.arc!;
          const label = { x: arc.labelAt.x - 32, y: arc.labelAt.y - 18, w: 64, h: 36 };
          for (const t of v.ring!.tiles) expect(clear(label, t.box), `n=${n} holder=${holder} ahead=${ahead}`).toBe(true);
          const lay = slotsLayout(v.ring!.geometry, 8);
          if (lay.form !== "below") {
            const c = v.ring!.geometry.centre;
            expect(clear(label, { x: c.x - lay.w / 2, y: c.y - lay.h / 2, w: lay.w, h: lay.h }), `the label is outside the ${lay.form} slots`).toBe(true);
          }
        }
      }
    }
  });
});

describe("where the CPU slots go", () => {
  const geo = (w: number, h: number, n: number) => ringGeometry({ x: 0, y: 0, w, h }, n);
  it("labels them in the centre on a laptop, with the largest chip that fits, between 24 and 40 px", () => {
    // 1440x900: the ring's stage is about 880x710; 1280x800: about 800x610
    for (const n of [16, 32]) {
      const l = slotsLayout(geo(880, 710, n), 8);
      expect(l.form, `1440x900 ring of ${n}`).toBe("full");
      expect(l.chip).toBeGreaterThanOrEqual(24);
      expect(l.chip).toBeLessThanOrEqual(40);
    }
    expect(slotsLayout(geo(800, 610, 32), 8).form).toBe("full");
    // the interior of a ring of 16 on 1280x800 is 428 px across, and the arc's label takes about 70 of it on each side
    expect(slotsLayout(geo(800, 610, 16), 8).form).not.toBe("full");
    expect(slotsLayout(geo(1000, 760, 32), 8).chip).toBe(40);
  });

  it("drops the labels, then the centre, as the ring shrinks, and never packs the box where it would cross the arc", () => {
    const forms = [700, 600, 520, 460, 400, 358].map((w) => slotsLayout(geo(w, w, 32), 8).form);
    // never the labelled form again once it has been dropped
    expect(forms.join()).toMatch(/^(full,)*(compact,)*(below,?)*$/);
    expect(forms.at(-1)).toBe("below");
    const g = geo(880, 710, 32);
    const l = slotsLayout(g, 8);
    expect(centreClear(g, l.w, l.h)).toBe(true);
    expect(centreClear(g, 2 * g.radius, 20)).toBe(false);
  });
});

describe("the chart and the ladder's bars", () => {
  it("plots CPUs in use against pace for the round's laps, with the allotment as the line", () => {
    const p = dense();
    const c = chartOf(p, p.round!)!;
    expect(c.points).toEqual([{ pace: 2.5, cpus: 2 }, { pace: 5, cpus: 3 }]);
    expect(c.now).toEqual({ pace: 5.24, cpus: 4 });
    expect([c.k, c.cpusMax, c.kLabel]).toEqual([4, 4, "no CPU for the next wake"]);
    // the axis runs 1.6 times past the furthest point, or on to the rung's record
    expect(c.paceMax).toBe(9);
  });

  it("has no chart before a lap reports its peak, or when the laps carry none", () => {
    const early = dense({}, { laps: [] });
    expect(chartOf(early, early.round!)).toBeNull();
    const old = dense({}, { laps: [{ lap: 0, pace: 2, hold_ms: 100, cpus_peak: null, cpus_busy: null }] });
    expect(chartOf(old, old.round!)).toBeNull();
  });

  it("draws one bar per rung, long by the fastest pace held, with the current rung marked", () => {
    const l = ladderBarsOf(dense({ ladder: [
      { size: 16, record_hold_ms: null, record_at: null, record_pace: 12.1, record_pace_at: T, record_prev_pace: null, broke: null, rounds: 1 },
      { size: 32, record_hold_ms: null, record_at: null, record_pace: 6.05, record_pace_at: T, record_prev_pace: null, broke: null, rounds: 1 },
      { size: 64, record_hold_ms: null, record_at: null, record_pace: null, record_pace_at: null, record_prev_pace: null, broke: null, rounds: 0 },
    ], size: 32, capacity: 96 }));
    expect(l.title).toBe("Fastest pace held on 4 CPUs");
    expect(l.rows.map((r) => [r.size, r.label, r.value, r.length === null ? null : Math.round(r.length * 100) / 100, r.current])).toEqual([
      [16, "4 per CPU", "12.1", 1, false],
      [32, "8 per CPU", "6.1", 0.5, true],
      [64, "16 per CPU", "none yet", null, false],
    ]);
  });

  it("falls back to the machine count and the old hold record when there is no allotment or no pace", () => {
    const l = ladderBarsOf(potato({ capacity: 4, ladder: [
      { size: 2, record_hold_ms: 1458, record_at: T, record_pace: null, record_pace_at: null, record_prev_pace: null, broke: null, rounds: 3 },
      { size: 4, record_hold_ms: null, record_at: null, record_pace: null, record_pace_at: null, record_prev_pace: null, broke: null, rounds: 0 },
      { size: 8, record_hold_ms: null, record_at: null, record_pace: null, record_pace_at: null, record_prev_pace: null, broke: null, rounds: 0 },
    ] }));
    expect(l.title).toBe("Fastest pace held");
    expect(l.rows.map((r) => [r.label, r.value, r.length, r.skipped])).toEqual([
      ["2 machines", "1.46 s hold", null, false],
      ["4 machines", "none yet", null, false],
      ["8 machines", "skipped", null, true],
    ]);
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
  });

  it("names each measured cause for the card and the late square", () => {
    expect(causeOf({ ...BROKE, cause: "waking", cause_ms: 84 })).toEqual({ tile: "84 ms late", line: "Still waking when the ball arrived; it accepted 84 ms later.", big: { value: "84 ms", word: "late" } });
    expect(causeOf({ ...BROKE, cause: "waking", cause_ms: null })).toMatchObject({ tile: "still waking", big: null });
    expect(causeOf({ ...BROKE, cause: "cpu_wait", cause_ms: 131 })).toMatchObject({ tile: "waited 131 ms for a CPU", line: "Its wake had waited 131 ms for a CPU.", big: { value: "131 ms", word: "waiting for a CPU" } });
    expect(causeOf({ ...BROKE, cause: "saving", cause_ms: 40 })).toMatchObject({ tile: "still saving", line: "Still saving, 40 ms in, when the ball arrived." });
    expect(causeOf({ ...BROKE, cause: "proof" })).toMatchObject({ tile: "proof failed" });
    expect(causeOf({ ...BROKE, cause: null })).toBeNull();
  });

  it("outlines the late square with its cause on the ring, and uses the old sentence when no cause is measured", () => {
    const withCause = dense({ round: null, next_round_at: T + 2500, last_drop: { ...BROKE, size: 8, player: 5, cause: "waking", cause_ms: 84 } });
    const v = potatoView(input(stateWith(withCause)));
    expect(v.ring!.tiles.filter((t) => t.late !== null).map((t) => [t.player, t.late])).toEqual([[5, "84 ms late"]]);
    expect(v.drop!.big).toEqual({ value: "84 ms", word: "late" });
    expect(v.drop!.headline).toBe("Still waking when the ball arrived; it accepted 84 ms later.");
    const plain = potatoView(input(stateWith(dense({ round: null, next_round_at: T + 2500, last_drop: { ...BROKE, size: 8, player: 5 } }))));
    expect(plain.ring!.tiles.every((t) => t.late === null)).toBe(true);
    expect(plain.drop!.big).toBeNull();
    expect(plain.drop!.headline).toBe("Player 5 was still waking, 90 ms into its wake, when the ball dropped.");
  });

  it("shows the drop card while the next round is pending", () => {
    const p = potato({ round: null, size: 4, next_round_at: T + 2500 });
    const v = potatoView(input(stateWith(p)));
    expect(v.drop).toEqual({
      title: "The ball dropped",
      headline: "Player 1 was still waking, 90 ms into its wake, when the ball dropped.",
      big: null,
      context: "Ring of 2 · lap 6",
      stages: ["Its last sleep: prepare 17.6 ms, pause 0.5 ms, save 74.5 ms, disk 114.7 ms, stop 106.7 ms", "Its last wake: restore 352 ms, ack 201 ms, listen 355.3 ms"],
      next: "Next: a ring of 4 in 3 s",
      detail: BROKE.detail,
    });
    expect(dropCard(p, { ...BROKE, pace: 8.41 }, T).context).toBe("Ring of 2 · lap 6 · 8.4 catches / s");
    // Not during a round, nor when no next round is due.
    expect(potatoView(input(stateWith(potato()))).drop).toBeNull();
    expect(potatoView(input(stateWith(potato({ round: null, next_round_at: null })))).drop).toBeNull();
  });

});

describe("Faster and Restart", () => {
  const committed = (name: string, at: number, viewer = "v-1") => ({ at, viewer, id: `${name}-${at}`, name, args: {}, summary: name, outcome: "committed" });

  it("Faster has no note when it is ready, and is off without a round, at the floor, while pending, and for 2 s after this viewer's last", () => {
    expect(potatoFasterButton(stateWith(potato()), potato(), T)).toEqual({ label: "Faster", disabled: false, note: "" });
    expect(potatoFasterButton(stateWith(potato()), potato({ round: null }), T)).toEqual({ label: "Faster", disabled: true, note: "works while a round is being played" });
    const floor = potato({ round: round({ hold_ms: 2 }) });
    expect(potatoFasterButton(stateWith(floor), floor, T).note).toBe("as fast as it goes");
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

  it("Restart waits 10 s into a round and 10 s after this viewer's last, and names the ring it plays again", () => {
    const fresh = potato({ round: round({ started_at: T - 4000 }) });
    expect(potatoRestartButton(stateWith(fresh), fresh, T)).toEqual({ label: "Restart", disabled: true, note: "possible 10 s into a round, in 6 s" });
    const old = potato({ round: round({ started_at: T - 60_000 }) });
    expect(potatoRestartButton(stateWith(old), old, T)).toEqual({ label: "Restart", disabled: false, note: "ring of 4 again" });
    const mine = stateWith(old, "v-1", [committed("potato_restart", T - 1000)]);
    expect(potatoRestartButton(mine, old, T)).toEqual({ label: "Restart", disabled: true, note: "again in 9 s" });
    const gone = potato({ round: null, capacity: 0 });
    expect(potatoRestartButton(stateWith(gone), gone, T).note).toBe("works while a host runs players");
  });

  it("both pause while the coordinator is offline", () => {
    const s = reduce(stateWith(potato()), { type: "socket", status: "closed", at: T });
    const v = potatoView(input(s));
    expect([v.faster.disabled, v.faster.note, v.restart.disabled]).toEqual([true, "paused while the coordinator is offline", true]);
    expect(v.banner).toBe("Reconnecting to the coordinator. The game is shown as it last was; the buttons wait until it is back.");
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
    expect(v.density).toBeNull();
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
