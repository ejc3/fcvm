// The mock's potato game on fake timers: the ladder it plays, the rules of the two intents, and
// that it sends only what the coordinator's PotatoView has.
import { readFileSync } from "node:fs";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MockPotato, type MockPotatoOptions } from "../potato.ts";

type State = ReturnType<MockPotato["state"]>;

const OPTS: MockPotatoOptions = { players: 8, budgetMs: 700, startFloorMs: 300, restartAfterMs: 2000, sleepMs: 350, wakeMs: 350, slowWakeMs: 1500, slowWakeChance: 0, seed: 7 };

let game: MockPotato;
const reply = { refuse: (r: string) => ({ outcome: "refused", reason: r }), committed: (s: string) => ({ outcome: "committed", reason: s }) };
const start = (over: Partial<MockPotatoOptions> = {}) => {
  game = new MockPotato({ ...OPTS, ...over }, { now: () => Date.now(), later: (ms, fn) => setTimeout(fn, ms), markDirty: () => 0 });
};
/** Advance until `pred` holds, in 10 ms steps, at most `ms`. */
function until(pred: (s: State) => boolean, ms = 120_000): State {
  for (let t = 0; t <= ms; t += 10) {
    const s = game.state();
    if (pred(s)) return s;
    vi.advanceTimersByTime(10);
  }
  throw new Error("condition not met in time");
}

beforeEach(() => {
  vi.useFakeTimers({ now: 1_760_000_000_000 });
});
afterEach(() => {
  game.close();
  vi.useRealTimers();
});

describe("mock potato", () => {
  it("plays the ladder: holds shrink a tenth a lap, colours step each catch, a late catcher drops the ball, and the next size starts", () => {
    start();
    expect(game.state().round).toBeNull();
    expect(game.state().next_round_at).toBe(Date.now() + 1000);
    const r1 = until((s) => s.round?.seq === 0).round!;
    expect([r1.size, r1.ring, r1.start_hold_ms, r1.hold_ms, r1.turnaround_ms]).toEqual([2, [0, 1], 700, 700, 700]);
    const colour0 = game.state().players.find((p) => p.player === 0)!.colour;
    const lap1 = until((s) => (s.round?.seq ?? 0) >= 2).round!;
    expect(lap1.hold_ms).toBe(630);
    expect(lap1.lap).toBe(1);
    const p0 = game.state().players.find((p) => p.player === 0)!;
    expect(p0.colour).toBe((colour0 + p0.counter - 1) % 12);
    // A ring of 2 has one hold to sleep and wake in: it drops once the hold is under about 600 ms.
    const dropped = until((s) => s.round === null && s.last_drop !== null);
    const d = dropped.last_drop!;
    expect(d.reason).toBe("not_listening");
    expect(["asleep", "sleeping", "waking"]).toContain(d.stage);
    expect(d.hold_ms).toBeLessThan(630);
    expect(d.detail).toMatch(/^player \d was (asleep|sleeping|waking) when player \d threw/);
    const rung2 = dropped.ladder.find((g) => g.size === 2)!;
    expect(rung2.rounds).toBe(1);
    expect(rung2.broke).toEqual(d);
    // the record is the hold of the last lap completed: the one before the lap that dropped
    expect(d.lap).toBeGreaterThanOrEqual(1);
    let h = 700;
    for (let k = 0; k < d.lap - 1; k++) h -= Math.floor(h / 10);
    expect(rung2.record_hold_ms).toBe(h);
    expect(dropped.size).toBe(4);
    expect(dropped.next_round_at).toBe(d.at + 2000);
    expect(dropped.records.longest_round).toMatchObject({ size: 2, value: d.seq });
    expect(dropped.records.fastest_cycle_ms).not.toBeNull();
    const r2 = until((s) => s.round?.size === 4).round!;
    // max(700 / 3, 300)
    expect(r2.start_hold_ms).toBe(300);
  });

  it("reports when a dropped ball's waking catcher started accepting", () => {
    start();
    until((s) => s.round?.seq === 0);
    const s = until((x) => x.last_drop !== null && x.last_drop.stage === "waking" && x.last_drop.listening_after_ms !== null, 300_000);
    const d = s.last_drop!;
    expect(d.listening_after_ms).toBeGreaterThan(0);
    expect(s.ladder.find((g) => g.broke?.round === d.round)!.broke!.listening_after_ms).toBe(d.listening_after_ms);
  });

  it("skips sizes above the host's players and goes back to 2 after the largest", () => {
    start({ players: 4 });
    until((s) => s.round?.size === 4, 300_000);
    const back = until((s) => s.round?.size === 2 && s.round.id === 3, 300_000);
    expect(back.ladder.filter((g) => g.rounds > 0).map((g) => g.size)).toEqual([2, 4]);
  });

  it("refuses Faster without a round, takes one per viewer every 2 s, and cuts the hold by a fifth", () => {
    start();
    expect(game.apply("v-1", "potato_faster", reply)).toEqual({ outcome: "refused", reason: "no round is being played" });
    until((s) => s.round?.seq === 0);
    expect(game.apply("v-1", "potato_faster", reply)).toEqual({ outcome: "committed", reason: "faster" });
    expect(game.state().round).toMatchObject({ pace_ms: 560, hold_ms: 560, turnaround_ms: 560 });
    expect(game.apply("v-1", "potato_faster", reply)).toEqual({ outcome: "refused", reason: "one faster every 2 s" });
    expect(game.apply("v-2", "potato_faster", reply)).toEqual({ outcome: "committed", reason: "faster" });
    expect(game.state().round!.pace_ms).toBe(448);
    expect(game.apply("v-1", "anything_else", reply)).toBeUndefined();
  });

  it("refuses Restart in a round's first 10 s and twice in 10 s from one viewer; a restart starts the size again without a record", () => {
    start({ budgetMs: 20_000, startFloorMs: 20_000 });
    const r = until((s) => s.round?.seq === 0).round!;
    expect(game.apply("v-1", "potato_restart", reply)).toEqual({ outcome: "refused", reason: "the round has only just started" });
    vi.advanceTimersByTime(r.started_at + 10_000 - Date.now());
    expect(game.apply("v-1", "potato_restart", reply)).toEqual({ outcome: "committed", reason: "restart" });
    const s = game.state();
    expect([s.round, s.next_round_at, s.last_drop]).toEqual([null, Date.now() + 1000, null]);
    expect(s.ladder.every((g) => g.rounds === 0 && g.broke === null)).toBe(true);
    expect(game.apply("v-1", "potato_restart", reply)).toEqual({ outcome: "refused", reason: "one restart every 10 s" });
    expect(until((x) => x.round !== null).round!.size).toBe(2);
  });

  it("sends only what the coordinator's potato view has", () => {
    // The coordinator's vector ends between rounds with no players (core/test/fixtures/coordinator.json,
    // 33_potato), so the round and the players are held to PROTOCOL.md's example instead.
    const fixture = JSON.parse(readFileSync(new URL("../../core/test/fixtures/coordinator.json", import.meta.url), "utf8")) as { views: Record<string, { potato?: unknown }> };
    const protocol = readFileSync(new URL("../../../PROTOCOL.md", import.meta.url), "utf8");
    const example = /```json\n(\{"size": 4, "colours": 12[\s\S]*?)\n```/.exec(protocol)![1]!.replace('"last_drop": {...}', '"last_drop": null');
    const shapes = (v: unknown, at = "", out = new Set<string>()): Set<string> => {
      if (v !== null && typeof v === "object") {
        const arr = Array.isArray(v);
        for (const [k, x] of arr ? v.map((y, i) => [i, y] as const) : Object.entries(v)) shapes(x, arr ? `${at}[]` : at ? `${at}.${k}` : String(k), out);
        if (!arr) out.add(at);
      } else out.add(at);
      return out;
    };
    const coordinator = shapes(fixture.views["33_potato"]!.potato);
    shapes(JSON.parse(example), "", coordinator);
    // The stage maps' keys are whatever the agent measured: the coordinator passes them on.
    const free = (p: string) => /\.(sleep|wake)_stages\./.test(p);
    // The density page's fields (fleet POTATO-FIELDS.md, which the host-side PRs add to the coordinator's view and PROTOCOL.md).
    const pending = (p: string) =>
      /^(confined|cpus)$/.test(p) ||
      /^round\.(pace|best_pace|laps|cpus_in_use|cpus_peak|asleep|lead|slowest_wake_ms|cpus_busy|cpu_slots)$/.test(p) ||
      /^round\.(laps|cpu_slots)\[\]/.test(p) ||
      /^players\[\]\.(cpu_wait_ms|ready_margin_ms)$/.test(p) ||
      /^ladder\[\]\.(record_pace|record_pace_at)$/.test(p) ||
      /(last_drop|broke)\.(cause|cause_ms|pace)$/.test(p);
    start({ slowWakeChance: 0.2 });
    const mock = new Set<string>();
    for (let i = 0; i < 3000; i++) {
      shapes(game.state(), "", mock);
      vi.advanceTimersByTime(20);
    }
    expect(mock.has("round.flight.from")).toBe(true);
    expect(mock.has("last_drop.listening_after_ms")).toBe(true);
    expect([...mock].filter((p) => p !== "" && !coordinator.has(p) && !free(p) && !pending(p)).sort()).toEqual([]);
    expect([...coordinator].filter((p) => p !== "" && !mock.has(p) && !free(p)).sort()).toEqual([]);
  });

  it("plays by the allotment: no more machines than CPUs not asleep, wakes that wait, measured pace, rungs from K, causes and a record", () => {
    start({ cpus: 3, slowWakeChance: 0.25 });
    expect(game.state().ladder.map((g) => g.size)).toEqual([6, 8]);
    expect([game.state().cpus, game.state().confined]).toEqual([3, true]);
    let peak = 0;
    let waited = false;
    let laps = 0;
    const causes = new Set<string>();
    for (let i = 0; i < 9000; i++) {
      const st = game.state();
      const held = st.players.filter((p) => p.phase !== "asleep").length;
      peak = Math.max(peak, held);
      if (st.players.some((p) => p.cpu_wait_ms !== null && p.cpu_wait_ms !== undefined)) waited = true;
      laps = Math.max(laps, st.round?.laps.length ?? 0);
      const c = st.last_drop?.cause;
      if (c) causes.add(c);
      vi.advanceTimersByTime(20);
    }
    // phase "sleeping" counts as held too: the state shows a thrower as sleeping until its save is done
    expect(peak).toBeLessThanOrEqual(3);
    expect(waited).toBe(true);
    expect(laps).toBeGreaterThan(0);
    const st = game.state();
    const paced = st.ladder.filter((g) => g.record_pace !== null);
    expect(paced.length).toBeGreaterThan(0);
    expect(paced[0]!.record_pace).toBeGreaterThan(0);
    expect(causes.size).toBeGreaterThan(0);
    for (const c of causes) expect(["waking", "cpu_wait", "saving"]).toContain(c);
  });
});
