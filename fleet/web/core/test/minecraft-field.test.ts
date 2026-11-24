import { describe, expect, it } from "vitest";
import {
  cellAt,
  cellBox,
  cellColor,
  youngLift,
  YOUNG_LIFT,
  cellText,
  CELL_NONE,
  CELL_RUNNING,
  CELL_SILENT,
  fieldCells,
  fieldHeadline,
  fieldLayout,
  FIELD_MIN_CELL,
  FIELD_TILES,
  BACK_HOLD_S,
  moveCursor,
  PIN_MS,
  resetWave,
  SAMPLE_MS,
  scrollTo,
  stepSample,
  unreported,
  type SampleSlot,
  STATUS_STALE_MS,
  statusRows,
  type FieldCell,
  type FieldStatus,
} from "../minecraft-field.ts";
import { parseServerMsg, ProtocolError, type MinecraftField } from "../protocol.ts";
import { McField } from "../../app/_wall/minecraft-field.ts";

const NOW = 1_000_000;
/** A row: phase, tps x 10 (or 255), players (or 255), age s, launch ms (65535: none). */
const row = (phase: number, tps = 255, players = 255, age = 65535, launch = 65535) => [phase, tps, players, 0, age & 255, age >> 8, launch & 255, launch >> 8];
const status = (rows: number[][], over: Partial<FieldStatus> = {}): FieldStatus => ({
  type: "minecraft_status",
  host: "aaaaaaaaaaaa",
  received_at: NOW,
  t: NOW,
  rows: Buffer.from(rows.flat()).toString("base64"),
  cpus: 192,
  busy: 100,
  mem_total_mib: 385_024,
  mem_used_mib: 204_800,
  at: NOW,
  ...over,
});
const field = (n: number, key = "aaaaaaaaaaaa"): MinecraftField => ({
  servers: n,
  hosts: [{ key, servers: Array.from({ length: n }, (_, i) => i + 1), frames: Array.from({ length: n }, (_, i) => `ws://[2001:db8::${(i + 2).toString(16)}]:8080/view`), frames_v4: Array.from({ length: n }, () => null) }],
});
const running = (n: number): FieldCell[] => fieldCells(field(n), { aaaaaaaaaaaa: status(Array.from({ length: n }, () => row(3, 200, 3, 100, 410))) }, NOW);

describe("the field's cells", () => {
  it("reads each server's row, and draws nothing for what its host did not read", () => {
    const f = field(4);
    const cells = fieldCells(f, { aaaaaaaaaaaa: status([row(3, 200, 3, 125, 412), row(1), row(0), row(3)]) }, NOW);
    expect(cells.map((c) => [c.slot, c.id, c.phase, c.tps, c.players, c.ageS, c.launchMs])).toEqual([
      ["mc-1", "field:1", "running", 20, 3, 125, 412],
      ["mc-2", "field:2", "restoring", null, null, null, null],
      ["mc-3", "field:3", "none", null, null, null, null],
      // running, but its tile did not answer: no tick rate, not zero
      ["mc-4", "field:4", "running", null, null, null, null],
    ]);
    expect(cells[0]!.frame).toBe("ws://[2001:db8::2]:8080/view");
    // no rows yet, rows too old, rows for another count of servers, rows that are not base64: every server is not reported
    const cases: Record<string, FieldStatus>[] = [{}, { aaaaaaaaaaaa: status([row(3), row(3), row(3), row(3)], { at: NOW - STATUS_STALE_MS - 1 }) }, { aaaaaaaaaaaa: status([row(3), row(3), row(3)]) }, { aaaaaaaaaaaa: status([], { rows: "***" }) }];
    for (const statuses of cases) expect(fieldCells(f, statuses, NOW).map((c) => c.phase)).toEqual(["none", "none", "none", "none"]);
    expect(statusRows({ rows: Buffer.from(row(3)).toString("base64") }, 1)).toHaveLength(8);
    expect(statusRows({ rows: Buffer.from(row(3)).toString("base64") }, 2)).toBeNull();
  });

  it("orders servers by number across hosts, each under a key that does not change", () => {
    const f: MinecraftField = {
      servers: 4,
      hosts: [
        { key: "bbbbbbbbbbbb", servers: [3, 4], frames: ["x3", null], frames_v4: [null, "y4"] },
        { key: "aaaaaaaaaaaa", servers: [1, 7], frames: ["x1", "x7"], frames_v4: [null, null] },
      ],
    };
    const listed = [{ id: "mc:1", slot: "mc-1" }];
    const cells = fieldCells(f, { bbbbbbbbbbbb: status([row(3, 198), row(4)], { host: "bbbbbbbbbbbb" }) }, NOW, listed);
    // a listed tile keeps its own id; every other server has the page's key, from its number
    expect(cells.map((c) => [c.n, c.id, c.phase, c.frame, c.frameV4])).toEqual([
      [1, "mc:1", "none", "x1", null],
      [3, "field:3", "running", "x3", null],
      [4, "field:4", "failed", null, "y4"],
      [7, "field:7", "none", "x7", null],
    ]);
    // The keys do not move when the field stops naming every server (one with no host yet is
    // counted and listed by no host), or names them all again: a tile whose key moved would be
    // closed and reopened, and an opened server put back in the wall.
    const keys = (field: MinecraftField) => fieldCells(field, {}, NOW, listed).map((c) => c.id);
    expect(keys({ ...f, servers: 5 })).toEqual(keys(f));
    const fewer: MinecraftField = { servers: 4, hosts: [f.hosts[0]!, { ...f.hosts[1]!, servers: [1], frames: ["x1"], frames_v4: [null] }] };
    expect(keys(fewer)).toEqual(["mc:1", "field:3", "field:4"]);
    // a listed tile whose id is not its place, and no two servers under one key
    const odd = fieldCells(f, {}, NOW, [{ id: "mc:9", slot: "mc-3" }]).map((c) => c.id);
    expect(odd).toEqual(["field:1", "mc:9", "field:4", "field:7"]);
  });

  it("says what a cell is, and colours it by its state and its tick rate", () => {
    const [ok, slow, silent, none] = fieldCells(field(4), { aaaaaaaaaaaa: status([row(3, 200, 3, 252, 410), row(3, 120, 3, 9, 700), row(3), row(0)]) }, NOW);
    expect(cellText(ok!)).toBe("mc-1 · running · 20.0 TPS · 3 players · up 4 min 12 s · restored in 0.41 s");
    expect(cellText(slow!)).toBe("mc-2 · running · 12.0 TPS · 3 players · up 9 s · restored in 0.70 s");
    expect(cellText(silent!)).toBe("mc-3 · running");
    expect(cellText(none!)).toBe("mc-4 · not reported");
    // a phone's row has room for the name, the state and the rate
    expect([cellText(ok!, true), cellText(silent!, true), cellText(none!, true)]).toEqual(["mc-1 · running · 20.0 TPS", "mc-3 · running", "mc-4 · not reported"]);
    expect(cellColor(ok!)).toEqual(CELL_RUNNING);
    expect(cellColor(silent!)).toEqual(CELL_SILENT);
    expect(cellColor(none!)).toEqual(CELL_NONE);
    // a slow server is not the colour of one that keeps up
    expect(cellColor(slow!)).not.toEqual(CELL_RUNNING);
    expect(cellColor(slow!)[0]).toBeGreaterThan(cellColor(ok!)[0]);
    // a server just back is lifted toward white and fades over its first minute; an age that was not read lifts nothing
    expect(youngLift({ ...ok!, ageS: 0 })).toBeCloseTo(YOUNG_LIFT);
    expect(youngLift({ ...ok!, ageS: 30 })).toBeCloseTo(YOUNG_LIFT / 2);
    expect([youngLift({ ...ok!, ageS: 60 }), youngLift({ ...ok!, ageS: null }), youngLift({ ...ok!, phase: "restoring", ageS: 0 })]).toEqual([0, 0, 0]);
  });
});

describe("the field's layout", () => {
  it("fits hundreds of cells on a laptop without scrolling, with the largest whole cell that fits", () => {
    for (const n of [64, 256, 512]) {
      const l = fieldLayout({ w: 520, h: 520 }, n);
      expect(l.scrolls, String(n)).toBe(false);
      expect(l.cols * l.rows).toBeGreaterThanOrEqual(n);
      expect(l.h).toBeLessThanOrEqual(520);
      // one size larger would not fit
      const bigger = l.cell + 1;
      expect(Math.ceil(n / Math.floor(520 / bigger)) * bigger > 520 || bigger > 28, String(n)).toBe(true);
    }
    expect(fieldLayout({ w: 520, h: 520 }, 512).cell).toBe(22);
    expect(fieldLayout({ w: 520, h: 520 }, 8).cell).toBe(28);
  });

  it("scrolls on a phone rather than shrink a cell below what a finger can pick", () => {
    const l = fieldLayout({ w: 390, h: 90 }, 512);
    expect(fieldLayout({ w: 390, h: 120 }, 512).scrolls).toBe(false);
    expect([l.cell, l.scrolls]).toEqual([FIELD_MIN_CELL, true]);
    expect(l.w).toBeLessThanOrEqual(390);
    expect(l.h).toBe(Math.ceil(512 / l.cols) * FIELD_MIN_CELL);
  });

  it("finds the cell under a point, and none beyond the last", () => {
    const l = fieldLayout({ w: 100, h: 100 }, 7);
    expect([l.cell, l.cols]).toEqual([28, 3]);
    expect(cellAt(l, 7, 0, 0)).toBe(0);
    expect(cellAt(l, 7, 29, 29)).toBe(4);
    expect(cellAt(l, 7, 83, 0)).toBe(2);
    // the grid is 84 wide: beyond it, and beyond the seventh cell, there is nothing
    expect(cellAt(l, 7, 84, 0)).toBeNull();
    expect(cellAt(l, 7, 0, 56)).toBe(6);
    expect(cellAt(l, 7, 30, 56)).toBeNull();
    expect(cellAt(l, 7, -1, 0)).toBeNull();
    expect(cellBox(l, 4)).toEqual({ x: 28, y: 28, w: 27, h: 27 });
  });
});

describe("the field's painter", () => {
  /** A canvas that counts what is drawn on it. */
  const canvas = () => {
    const ops: string[] = [];
    const ctx = { setTransform() {}, clearRect() {}, fillRect: () => ops.push("fill"), strokeRect: () => ops.push("stroke"), fillStyle: "", strokeStyle: "", lineWidth: 0 };
    return { width: 0, height: 0, getContext: () => ctx, ops } as unknown as HTMLCanvasElement & { ops: string[] };
  };

  it("paints a canvas it has not painted, though nothing in the field changed", () => {
    const painter = new McField();
    const paint = { layout: fieldLayout({ w: 400, h: 300 }, 64), cells: running(64), shown: [1, 2], pinned: [2], cursor: null, dpr: 1, reducedMotion: true };
    const a = canvas();
    expect(painter.paint(a, paint, 0)).toBe(true);
    // nothing changed: nothing is painted, with motion reduced or not
    expect(painter.paint(a, paint, 1000)).toBe(false);
    // The field left the page (a server had the stage) and came back on a new canvas, which
    // is blank: the same field is painted on it at once.
    const b = canvas();
    expect(painter.paint(b, paint, 2000)).toBe(true);
    expect(b.ops.filter((o) => o === "fill")).toHaveLength(64);
    // the two servers in 3D tiles are ringed
    expect(b.ops.filter((o) => o === "stroke")).toHaveLength(2);
    expect([painter.paints, painter.lastCells]).toEqual([2, 64]);
    expect(painter.paint(b, paint, 3000)).toBe(false);
    // a pick changes a ring's colour: painted again
    expect(painter.paint(b, { ...paint, pinned: [] }, 4000)).toBe(true);
  });
});

describe("the field's cursor", () => {
  it("moves one cell or one row, and stays put at every edge", () => {
    // 23 cells, 5 to a row: four whole rows and a last row of three
    const l = { cell: 10, cols: 5, rows: 5, w: 50, h: 50, scrolls: false };
    const go = (from: number | null, key: "ArrowLeft" | "ArrowRight" | "ArrowUp" | "ArrowDown") => moveCursor(l, 23, from, key);
    expect([go(null, "ArrowRight"), go(null, "ArrowUp")]).toEqual([0, 0]);
    expect([go(7, "ArrowLeft"), go(7, "ArrowRight"), go(7, "ArrowUp"), go(7, "ArrowDown")]).toEqual([6, 8, 2, 12]);
    // the first row has no row above it: the cursor does not jump to the first cell
    expect([go(3, "ArrowUp"), go(0, "ArrowUp"), go(0, "ArrowLeft")]).toEqual([3, 0, 0]);
    // the last row is short: down from above its gap stays, down onto it goes, and past the last cell stays
    expect([go(19, "ArrowDown"), go(17, "ArrowDown"), go(22, "ArrowDown"), go(22, "ArrowRight")]).toEqual([19, 22, 22, 22]);
    expect(moveCursor(l, 0, null, "ArrowRight")).toBeNull();
  });

  it("scrolls a field just far enough to show the cursor's cell whole", () => {
    const l = { cell: 10, cols: 5, rows: 20, w: 50, h: 200, scrolls: true };
    // 60 px in view from 40: rows 4 to 9
    expect(scrollTo(l, 5 * 5, 40, 60)).toBe(40);
    expect(scrollTo(l, 2 * 5, 40, 60)).toBe(20);
    expect(scrollTo(l, 12 * 5 + 3, 40, 60)).toBe(70);
    expect(scrollTo(l, 0, 0, 60)).toBe(0);
  });
});

describe("which servers the 3D tiles show", () => {
  const ns = (slots: readonly SampleSlot[]) => slots.map((x) => x.n);
  /** The cells with some servers' rows replaced. */
  const withRows = (n: number, over: Record<number, number[]>) => fieldCells(field(n), { aaaaaaaaaaaa: status(Array.from({ length: n }, (_, i) => over[i + 1] ?? row(3, 200, 3, 100, 410))) }, NOW);
  /** Step the sample every 100 ms from `from` to `to`, and return it with how many tiles changed on the way. */
  const run = (start: SampleSlot[], cellsAt: (t: number) => FieldCell[], from: number, to: number, opts = {}) => {
    let slots = start;
    let cuts = 0;
    let most = 0;
    for (let t = from; t <= to; t += 100) {
      const next = stepSample(slots, cellsAt(t), FIELD_TILES, t, opts);
      const changed = next.filter((x, j) => x.n !== slots[j]?.n).length;
      if (slots.length > 0) {
        cuts += changed;
        most = Math.max(most, changed);
      }
      slots = next;
    }
    return { slots, cuts, most };
  };

  it("shows six different running servers, each for a while, the tiles cutting one after another", () => {
    const cells = running(512);
    const a = stepSample([], cells, FIELD_TILES, 0);
    expect(a).toHaveLength(6);
    expect(new Set(ns(a)).size).toBe(6);
    // stepping again at the same moment changes nothing
    expect(stepSample(a, cells, FIELD_TILES, 0)).toEqual(a);
    const four = run(a, () => cells, 100, SAMPLE_MS * 4);
    // never two tiles at once, never two tiles on one server, and every tile cut about four times in four holds
    expect(four.most).toBe(1);
    expect(new Set(ns(four.slots)).size).toBe(6);
    expect(four.cuts).toBeGreaterThanOrEqual(20);
    expect(four.cuts).toBeLessThanOrEqual(26);
    // over a while, many servers get shown
    const seen = new Set<number>();
    let slots = a;
    for (let t = 0; t < SAMPLE_MS * 40; t += 500) {
      slots = stepSample(slots, cells, FIELD_TILES, t);
      for (const n of ns(slots)) seen.add(n);
    }
    expect(seen.size).toBeGreaterThan(150);
  });

  it("a reset of one server moves no other tile, and its own tile stays on it until it is back", () => {
    const t0 = 50_000;
    const a = stepSample([], running(512), FIELD_TILES, t0);
    // a server on no tile is reset: nothing moves (an index into the list of running servers would shift for every tile)
    const off = Array.from({ length: 512 }, (_, i) => i + 1).find((n) => !ns(a).includes(n))!;
    expect(stepSample(a, withRows(512, { [off]: row(1) }), FIELD_TILES, t0 + 50)).toEqual(a);
    // the server on the tile whose turn ends last is reset, and stays restoring past the end of that turn
    const mine = a[0]!.n;
    const restoring = withRows(512, { [mine]: row(1) });
    const during = run(a, () => restoring, t0 + 100, t0 + SAMPLE_MS + 3000);
    expect(during.slots[0]!.n).toBe(mine);
    // the other tiles went on with their own turns meanwhile, one at a time
    expect(during.most).toBe(1);
    expect(during.cuts).toBeGreaterThanOrEqual(5);
    // it comes back: its tile shows it for BACK_HOLD_S more, then takes its turn
    const t1 = t0 + SAMPLE_MS + 3100;
    const justBack = withRows(512, { [mine]: row(3, 200, 3, BACK_HOLD_S - 1, 400) });
    expect(stepSample(during.slots, justBack, FIELD_TILES, t1)[0]!.n).toBe(mine);
    const back = withRows(512, { [mine]: row(3, 200, 3, BACK_HOLD_S, 400) });
    const after = run(during.slots, () => back, t1, t1 + SAMPLE_MS);
    expect(after.slots[0]!.n).not.toBe(mine);
  });

  it("a reset of every server keeps every tile on its server until that one is back", () => {
    const t0 = 80_000;
    const a = stepSample([], running(64), FIELD_TILES, t0);
    const all = fieldCells(field(64), { aaaaaaaaaaaa: status(Array.from({ length: 64 }, () => row(1))) }, NOW);
    const during = run(a, () => all, t0 + 100, t0 + SAMPLE_MS * 3);
    expect(during.cuts).toBe(0);
    expect(ns(during.slots)).toEqual(ns(a));
  });

  it("shows only running servers while there are enough, and the rest when there are not", () => {
    const f = field(8);
    const cells = fieldCells(f, { aaaaaaaaaaaa: status([row(1), row(3, 200), row(1), row(3, 200), row(4), row(0), row(3, 200), row(1)]) }, NOW);
    const got = ns(stepSample([], cells, 6, 12_345));
    expect(got.slice(0, 3).sort()).toEqual([2, 4, 7]);
    // then the ones being restored, before a failed or an unreported one
    expect(got.slice(3).sort()).toEqual([1, 3, 8]);
    expect(new Set(got).size).toBe(6);
    expect(stepSample([], cells.slice(0, 2), 6, 0)).toHaveLength(2);
    expect(stepSample([], [], 6, 0)).toEqual([]);
    // a tile whose server left the field takes another at once; the others keep theirs
    const a = stepSample([], running(64), 6, 1000);
    const gone = running(64).filter((c) => c.n !== a[2]!.n);
    const b = stepSample(a, gone, 6, 1100);
    expect(b.map((x, j) => x.n === a[j]!.n)).toEqual([true, true, false, true, true, true]);
    expect(new Set(ns(b)).size).toBe(6);
  });

  it("gives a picked server the tile whose turn is nearest, for a minute or until it is unpinned, and moves no other", () => {
    const cells = running(64);
    const t0 = 100_000;
    const a = stepSample([], cells, 6, t0);
    const pick = [40, 41].filter((n) => !ns(a).includes(n))[0]!;
    const pins = [{ n: pick, at: t0 + 10 }];
    const b = stepSample(a, cells, 6, t0 + 10, { pins });
    // the last tile's turn was the nearest to its end
    expect(b.map((x, j) => (x.n === a[j]!.n ? "same" : x.n))).toEqual(["same", "same", "same", "same", "same", pick]);
    expect(b[5]!.pin).toBe(true);
    // it keeps the tile while the others cut around it
    const held = run(b, () => cells, t0 + 100, t0 + PIN_MS - 100, { pins });
    expect(held.slots[5]!.n).toBe(pick);
    expect(held.cuts).toBeGreaterThan(10);
    // its minute over, the tile goes back to the rotation
    const after = run(held.slots, () => cells, t0 + PIN_MS - 100, t0 + PIN_MS + 3000, { pins });
    expect(ns(after.slots)).not.toContain(pick);
    // unpinned: back to the rotation at once, without waiting for the minute
    const unpinned = run(b, () => cells, t0 + 100, t0 + 2500, {});
    expect(ns(unpinned.slots)).not.toContain(pick);
    // a pick already on a tile stays where it is; one that is no server is ignored
    const there = stepSample(a, cells, 6, t0 + 10, { pins: [{ n: a[1]!.n, at: t0 + 10 }, { n: 999, at: t0 + 10 }] });
    expect(ns(there)).toEqual(ns(a));
    expect(there[1]!.pin).toBe(true);
  });

  it("with reduced motion no tile cuts by itself, and a pick still gives its tile back", () => {
    const cells = running(64);
    const a = stepSample([], cells, 6, 0, { still: true });
    const later = run(a, () => cells, 100, SAMPLE_MS * 5, { still: true });
    expect(later.cuts).toBe(0);
    const pick = [40, 41, 42, 43, 44, 45, 46].find((n) => !ns(a).includes(n))!;
    const pins = [{ n: pick, at: SAMPLE_MS * 5 }];
    const pinned = stepSample(later.slots, cells, 6, SAMPLE_MS * 5, { still: true, pins });
    expect(ns(pinned)).toContain(pick);
    const kept = run(pinned, () => cells, SAMPLE_MS * 5 + 100, SAMPLE_MS * 5 + PIN_MS - 100, { still: true, pins });
    expect(ns(kept.slots)).toContain(pick);
    const over = run(kept.slots, () => cells, SAMPLE_MS * 5 + PIN_MS, SAMPLE_MS * 5 + PIN_MS + 2000, { still: true, pins });
    expect(ns(over.slots)).not.toContain(pick);
    // only that tile changed
    expect(over.slots.filter((x, j) => x.n !== pinned[j]!.n)).toHaveLength(1);
    // a pick whose minute ran out while its server was being restored keeps the tile until it is back, then gives it up
    const t2 = SAMPLE_MS * 5 + PIN_MS;
    const waiting = run(kept.slots, () => withRows(64, { [pick]: row(1) }), t2, t2 + 3000, { still: true, pins });
    expect(ns(waiting.slots)).toContain(pick);
    const back = run(waiting.slots, () => cells, t2 + 3100, t2 + 6000, { still: true, pins });
    expect(ns(back.slots)).not.toContain(pick);
  });

  it("keeps the opened server on its tile until it is closed, then for a turn more", () => {
    const cells = running(64);
    const a = stepSample([], cells, 6, 0);
    // the tile whose turn ends first: opened, it does not cut
    const n = a[5]!.n;
    const open = run(a, () => cells, 100, SAMPLE_MS * 4, { held: n });
    expect(open.slots[5]!.n).toBe(n);
    // closed: it stays for a turn, not cut under the viewer the moment the wall returns
    const closed = run(open.slots, () => cells, SAMPLE_MS * 4 + 100, SAMPLE_MS * 5 - 200, {});
    expect(closed.slots[5]!.n).toBe(n);
    const gone = run(closed.slots, () => cells, SAMPLE_MS * 5, SAMPLE_MS * 5 + 2000, {});
    expect(gone.slots[5]!.n).not.toBe(n);
  });
});

describe("the headline", () => {
  it("counts the servers and says how many a busy CPU and a GiB in use carry, from the host's readings only", () => {
    const f = field(512);
    const rows = Array.from({ length: 512 }, (_, i) => (i < 500 ? row(3, i === 0 ? 191 : 200, 3, 100, 400) : i < 510 ? row(1) : row(0)));
    const statuses = { aaaaaaaaaaaa: status(rows) };
    const h = fieldHeadline(f, fieldCells(f, statuses, NOW), statuses, NOW);
    expect(h).toEqual({
      servers: "512 servers on one host",
      states: "500 running · 10 restoring · 2 not reported",
      perCpu: "5.0 per busy CPU (100.0 of 192 busy)",
      perGib: "2.5 per GiB in use (200 GiB)",
      tps: "20.0 TPS median, 19.1 lowest",
    });
    // a host that did not say its CPUs or its memory: no ratio, never one from a zero
    const bare = { aaaaaaaaaaaa: status(rows, { busy: null, mem_used_mib: null }) };
    const b = fieldHeadline(f, fieldCells(f, bare, NOW), bare, NOW);
    expect([b.perCpu, b.perGib]).toEqual([null, null]);
    expect(b.states).toBe("500 running · 10 restoring · 2 not reported");
    // no rows at all: no count, never "0 running" that nobody read
    const none = fieldHeadline(f, fieldCells(f, {}, NOW), {}, NOW);
    expect(none).toEqual({ servers: "512 servers on one host", states: "not reported yet", perCpu: null, perGib: null, tps: null });
    expect(unreported(f, fieldCells(f, {}, NOW), {}, NOW)).toBe("not reported yet");
    // rows that stopped coming: how long ago, by the page's clock
    const later = NOW + STATUS_STALE_MS + 3200;
    expect(fieldHeadline(f, fieldCells(f, statuses, later), statuses, later).states).toBe("no report for 8 s");
    // every server restoring is a count that was read: zero running is said
    const down = { aaaaaaaaaaaa: status(Array.from({ length: 512 }, () => row(1))) };
    expect(fieldHeadline(f, fieldCells(f, down, NOW), down, NOW).states).toBe("0 running · 512 restoring");
    expect(unreported(f, fieldCells(f, down, NOW), down, NOW)).toBeNull();
  });
});

describe("a reset of every server", () => {
  const CLICKED = NOW - 100_000;
  /** Cells from ages (null: restoring; "f": failed; "n": not reported), their rows received `seen` ms after the click. */
  const cellsAt = (ages: (number | null | "f" | "n")[], seen: number | null) =>
    fieldCells(field(ages.length), { aaaaaaaaaaaa: status(ages.map((a) => (a === null ? row(1) : a === "f" ? row(4) : a === "n" ? row(0) : row(3, 200, 3, a, 400))), { received_at: seen === null ? null : CLICKED + seen }) }, NOW);
  const wave = (ages: (number | null | "f" | "n")[], seen: number | null, nowAfter: number, servers = ages.length) => resetWave(cellsAt(ages, seen), { clicked_at: CLICKED, servers }, CLICKED + nowAfter);

  it("counts the servers back as they come, with the rate and the time, then says how long the whole took", () => {
    // 10 s in: two are back (their launches' first chunks came after the click), one restoring, one not reset yet
    const during = wave([3, 8, null, 400], 10_000, 10_200)!;
    expect([during.back, during.total, during.done]).toEqual([2, 4, false]);
    expect(during.text).toBe("Reset all: 2 of 4 back · 0.2 a second · 10.2 s");
    // all back: the last came 18 s after the click, as its row's age says
    const done = wave([13, 18, 5, 2], 20_000, 20_300)!;
    expect(done.done).toBe(true);
    expect(done.text).toBe("Reset all: 4 servers back in 18 s (0.2 a second)");
    // the time stands still as the rows age
    expect(wave([43, 48, 35, 32], 50_000, 50_100)!.text).toBe("Reset all: 4 servers back in 18 s (0.2 a second)");
    // a minute after the last server is back it is history
    expect(wave([71, 76, 63, 60], 78_000, 78_000)).not.toBeNull();
    expect(wave([72, 77, 64, 61], 79_000, 79_000)).toBeNull();
    expect(resetWave([], { clicked_at: CLICKED, servers: 0 }, CLICKED + 1000)).toBeNull();
    expect(wave([1], 500, -1)).toBeNull();
  });

  it("says the same on a page opened long after: the time is the rows', not when this page looked", () => {
    // a reset that took 20 s, first seen by this page 40 s after the click
    const late = wave([20, 25, 30, 38], 40_000, 40_400)!;
    expect(late.text).toBe("Reset all: 4 servers back in 20 s (0.2 a second)");
    // from a coordinator that does not say when a row came there is no time to give: none is given
    expect(wave([20, 25, 30, 38], null, 40_400)!.text).toBe("Reset all: 4 servers back");
  });

  it("keeps the time it showed once the reset was over, though the rows' whole seconds would move it", () => {
    // The last server's first chunk came 21.45 s after the click. Its age is cut to a whole
    // second when its host reads it, a moment that moves a little from report to report:
    // read 21.49 s in, the rows say 21 s; read 21.51 s in, 22 s.
    const a = wave([10, 12], 31_490, 31_600)!;
    const b = wave([11, 13], 32_510, 32_600)!;
    expect([a.tookS, b.tookS]).toEqual([21, 22]);
    expect([a.settled, a.text]).toEqual([true, "Reset all: 2 servers back in 21 s (0.1 a second)"]);
    expect(a.frozen).toEqual({ clicked_at: CLICKED, back: 2, total: 2, done: true, tookS: 21, at: CLICKED + 21_490 });
    // the page hands back what it showed, and that stands
    const stood = resetWave(cellsAt([11, 13], 32_510), { clicked_at: CLICKED, servers: 2 }, CLICKED + 32_600, a.frozen)!;
    expect([stood.text, stood.frozen]).toEqual([a.text, a.frozen]);
    // while the reset runs nothing is kept: the clock is the coordinator's
    const running = resetWave(cellsAt([3, null], 10_000), { clicked_at: CLICKED, servers: 2 }, CLICKED + 10_200)!;
    expect([running.settled, running.tookS, running.frozen, running.text]).toEqual([false, null, null, "Reset all: 1 of 2 back · 0.1 a second · 10.2 s"]);
    // what was kept for another reset is not this one's
    const other = resetWave(cellsAt([3, null], 10_000), { clicked_at: CLICKED, servers: 2 }, CLICKED + 10_200, { ...a.frozen!, clicked_at: CLICKED - 5 })!;
    expect(other.text).toBe(running.text);
  });

  it("does not come back when one server is reset minutes after it finished", () => {
    // A reset of four that took 36 s. Five minutes after the click one of the four is reset by
    // itself and is back: its row is a launch after the click, as every row is.
    const later = [264, 270, 280, 2] as const;
    // the page that watched kept the finished reset: its minute is long over, and it stays over
    const kept = { clicked_at: CLICKED, back: 4, total: 4, done: true, tookS: 36, at: CLICKED + 36_000 };
    expect(resetWave(cellsAt([...later], 300_000), { clicked_at: CLICKED, servers: 4 }, CLICKED + 300_500, kept)).toBeNull();
    // and through its minute what it showed did not move, whatever came back meanwhile
    expect(resetWave(cellsAt([44, 50, 60, 2], 80_000), { clicked_at: CLICKED, servers: 4 }, CLICKED + 80_500, kept)!.text).toBe("Reset all: 4 servers back in 36 s (0.1 a second)");
    // a page opened only now has the rows alone: the late launch came more than a minute after
    // the one before it, so it is not the reset's, and the reset's last server is long back
    expect(wave([...later], 300_000, 300_500)).toBeNull();
    // the same while that one server is still being restored
    expect(wave([264, 270, 280, null], 299_000, 299_500)).toBeNull();
  });

  it("does not count a server outside the reset that relaunches later", () => {
    // Five servers, the reset sent to four of them (one was lent), and one of the four failed:
    // three of four back, and it stops there.
    const stopped = wave([14, 19, 10, "f", 900], 21_000, 21_300, 4)!;
    expect([stopped.done, stopped.text]).toEqual([false, "Reset all: 3 of 4 back after 11 s"]);
    // The lent server is given back and relaunched two minutes on. With what the page kept,
    // and from the rows alone, it is not the reset's fourth: the reset never reads as whole.
    const rows = cellsAt([134, 139, 130, "f", 3], 141_000);
    expect(resetWave(rows, { clicked_at: CLICKED, servers: 4 }, CLICKED + 141_300, stopped.frozen)).toBeNull();
    expect(resetWave(rows, { clicked_at: CLICKED, servers: 4 }, CLICKED + 141_300)).toBeNull();
    // 40 s on, inside the kept line's minute: still three of four
    const soon = cellsAt([54, 59, 50, "f", 3], 61_000);
    expect(resetWave(soon, { clicked_at: CLICKED, servers: 4 }, CLICKED + 61_300, stopped.frozen)!.text).toBe("Reset all: 3 of 4 back after 11 s");
  });

  it("counts against the servers the reset was sent to, and stops when the rest are not coming", () => {
    // five servers, one lent: the reset was sent to four, and four back is all of it
    expect(wave([13, 18, 5, 2, 900], 20_000, 20_300, 4)!.text).toBe("Reset all: 4 servers back in 18 s (0.2 a second)");
    // one of the four failed and nothing is being restored: 10 s after the last one back, it says so and the clock stops
    const going = wave([13, 18, 9, "f"], 20_000, 20_300)!;
    expect(going.text).toBe("Reset all: 3 of 4 back · 0.1 a second · 20.3 s");
    const stopped = wave([14, 19, 10, "f"], 21_000, 21_300)!;
    expect([stopped.done, stopped.text]).toEqual([false, "Reset all: 3 of 4 back after 11 s"]);
    expect(wave([44, 49, 40, "f"], 51_000, 51_300)!.text).toBe("Reset all: 3 of 4 back after 11 s");
    expect(wave([65, 70, 61, "f"], 72_000, 72_300)).toBeNull();
    // while any server is still being restored the count goes on, however long since the last one back
    expect(wave([40, 45, 36, null], 47_000, 47_300)!.text).toBe("Reset all: 3 of 4 back · 0.1 a second · 47.3 s");
    // servers on a silent host: the same end, and never a count of them as back
    expect(wave([14, 19, 10, "n"], 21_000, 21_300)!.text).toBe("Reset all: 3 of 4 back after 11 s");
    // more launches after the click than the reset's own (a single reset since): never more than its total
    expect(wave([13, 18, 5, 2, 1], 20_000, 20_300, 4)!.back).toBe(4);
  });
});

describe("the field in the protocol", () => {
  it("reads the snapshot's field and the status message, and refuses a field that does not add up", () => {
    const msg = parseServerMsg(JSON.stringify({ type: "minecraft_status", host: "aaaaaaaaaaaa", t: 7, rows: "AwDIAw==", cpus: 192 }));
    // from a coordinator that does not stamp its rows: no receive time, never a made-up one
    expect(msg).toEqual({ type: "minecraft_status", host: "aaaaaaaaaaaa", received_at: null, t: 7, rows: "AwDIAw==", cpus: 192, busy: null, mem_total_mib: null, mem_used_mib: null });
    expect(parseServerMsg(JSON.stringify({ type: "minecraft_status", host: "aaaaaaaaaaaa", received_at: 9, t: 7, rows: "" }))).toMatchObject({ received_at: 9 });
    expect(() => parseServerMsg(JSON.stringify({ type: "minecraft_status", host: "a", rows: "" }))).toThrow(ProtocolError);
  });
});
