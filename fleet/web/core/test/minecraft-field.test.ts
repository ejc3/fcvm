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
  PIN_MS,
  resetWave,
  sampleOf,
  SAMPLE_MS,
  STATUS_STALE_MS,
  statusRows,
  type FieldCell,
  type FieldStatus,
} from "../minecraft-field.ts";
import { parseServerMsg, ProtocolError, type MinecraftField } from "../protocol.ts";

const NOW = 1_000_000;
/** A row: phase, tps x 10 (or 255), players (or 255), age s, launch ms (65535: none). */
const row = (phase: number, tps = 255, players = 255, age = 65535, launch = 65535) => [phase, tps, players, 0, age & 255, age >> 8, launch & 255, launch >> 8];
const status = (rows: number[][], over: Partial<FieldStatus> = {}): FieldStatus => ({
  type: "minecraft_status",
  host: "aaaaaaaaaaaa",
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
      ["mc-1", "mc:1", "running", 20, 3, 125, 412],
      ["mc-2", "mc:2", "restoring", null, null, null, null],
      ["mc-3", "mc:3", "none", null, null, null, null],
      // running, but its tile did not answer: no tick rate, not zero
      ["mc-4", "mc:4", "running", null, null, null, null],
    ]);
    expect(cells[0]!.frame).toBe("ws://[2001:db8::2]:8080/view");
    // no rows yet, rows too old, rows for another count of servers, rows that are not base64: every server is not reported
    const cases: Record<string, FieldStatus>[] = [{}, { aaaaaaaaaaaa: status([row(3), row(3), row(3), row(3)], { at: NOW - STATUS_STALE_MS - 1 }) }, { aaaaaaaaaaaa: status([row(3), row(3), row(3)]) }, { aaaaaaaaaaaa: status([], { rows: "***" }) }];
    for (const statuses of cases) expect(fieldCells(f, statuses, NOW).map((c) => c.phase)).toEqual(["none", "none", "none", "none"]);
    expect(statusRows({ rows: Buffer.from(row(3)).toString("base64") }, 1)).toHaveLength(8);
    expect(statusRows({ rows: Buffer.from(row(3)).toString("base64") }, 2)).toBeNull();
  });

  it("orders servers by number across hosts, and gives each the id the coordinator gives its tile", () => {
    const f: MinecraftField = {
      servers: 4,
      hosts: [
        { key: "bbbbbbbbbbbb", servers: [3, 4], frames: ["x3", null], frames_v4: [null, "y4"] },
        { key: "aaaaaaaaaaaa", servers: [1, 7], frames: ["x1", "x7"], frames_v4: [null, null] },
      ],
    };
    const cells = fieldCells(f, { bbbbbbbbbbbb: status([row(3, 198), row(4)], { host: "bbbbbbbbbbbb" }) }, NOW);
    expect(cells.map((c) => [c.n, c.id, c.phase, c.frame, c.frameV4])).toEqual([
      [1, "mc:1", "none", "x1", null],
      [3, "mc:2", "running", "x3", null],
      [4, "mc:3", "failed", null, "y4"],
      [7, "mc:4", "none", "x7", null],
    ]);
  });

  it("says what a cell is, and colours it by its state and its tick rate", () => {
    const [ok, slow, silent, none] = fieldCells(field(4), { aaaaaaaaaaaa: status([row(3, 200, 3, 252, 410), row(3, 120, 3, 9, 700), row(3), row(0)]) }, NOW);
    expect(cellText(ok!)).toBe("mc-1 · running · 20.0 TPS · 3 players · up 4 min 12 s · restored in 0.41 s");
    expect(cellText(slow!)).toBe("mc-2 · running · 12.0 TPS · 3 players · up 9 s · restored in 0.70 s");
    expect(cellText(silent!)).toBe("mc-3 · running");
    expect(cellText(none!)).toBe("mc-4 · not reported");
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

describe("which servers the 3D tiles show", () => {
  it("shows six different running servers, each for a while, the tiles cutting one after another", () => {
    const cells = running(512);
    const at = (t: number) => sampleOf(cells, FIELD_TILES, t);
    const a = at(0);
    expect(a).toHaveLength(6);
    expect(new Set(a).size).toBe(6);
    // within a tile's hold nothing changes more than one tile at a time
    let cuts = 0;
    for (let t = 100; t <= SAMPLE_MS * 4; t += 100) {
      const before = at(t - 100);
      const now = at(t);
      const changed = now.filter((n, j) => n !== before[j]).length;
      expect(changed, String(t)).toBeLessThanOrEqual(1);
      expect(new Set(now).size, String(t)).toBe(6);
      cuts += changed;
    }
    // every tile cut about four times in four holds
    expect(cuts).toBeGreaterThanOrEqual(20);
    // over a while, many servers get shown
    const seen = new Set<number>();
    for (let t = 0; t < SAMPLE_MS * 40; t += SAMPLE_MS / 6) for (const n of at(t)) seen.add(n);
    expect(seen.size).toBeGreaterThan(150);
  });

  it("shows only running servers while there are enough, and the rest when there are not", () => {
    const f = field(8);
    const cells = fieldCells(f, { aaaaaaaaaaaa: status([row(1), row(3, 200), row(1), row(3, 200), row(4), row(0), row(3, 200), row(1)]) }, NOW);
    const got = sampleOf(cells, 6, 12_345);
    expect(got.slice(0, 3).sort()).toEqual([2, 4, 7]);
    expect(got).toHaveLength(6);
    expect(new Set(got).size).toBe(6);
    expect(sampleOf(cells.slice(0, 2), 6, 0)).toHaveLength(2);
    expect(sampleOf([], 6, 0)).toEqual([]);
  });

  it("gives a server a viewer picked the first tile for a minute, the newest pick first", () => {
    const cells = running(64);
    const got = sampleOf(cells, 6, 100_000, [{ n: 40, at: 90_000 }, { n: 41, at: 95_000 }, { n: 999, at: 99_000 }]);
    expect(got.slice(0, 2)).toEqual([41, 40]);
    expect(new Set(got).size).toBe(6);
    // a pick that has had its minute gives its tile back
    expect(sampleOf(cells, 6, 90_000 + PIN_MS, [{ n: 40, at: 90_000 }])).toEqual(sampleOf(cells, 6, 90_000 + PIN_MS));
    expect(sampleOf(cells, 6, 90_000 + PIN_MS - 1, [{ n: 40, at: 90_000 }])[0]).toBe(40);
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
    // no rows at all
    const none = fieldHeadline(f, fieldCells(f, {}, NOW), {}, NOW);
    expect(none).toEqual({ servers: "512 servers on one host", states: "0 running · 512 not reported", perCpu: null, perGib: null, tps: null });
  });
});

describe("a reset of every server", () => {
  const cellsAt = (ages: (number | null)[]) => fieldCells(field(ages.length), { aaaaaaaaaaaa: status(ages.map((a) => (a === null ? row(1) : row(3, 200, 3, a, 400)))) }, NOW);

  it("counts the servers back as they come, with the rate and the time, then says the whole of it", () => {
    const clicked = 50_000;
    // 10 s in: two are back (their launches are younger than the click), one restoring, one not reset yet
    const during = resetWave(cellsAt([3, 8, null, 400]), clicked, clicked + 10_000, null)!;
    expect([during.back, during.total, during.done]).toEqual([2, 4, false]);
    expect(during.text).toBe("Reset all: 2 of 4 back · 0.2 a second · 10.0 s");
    // all back: the page keeps when it first saw that
    const done = resetWave(cellsAt([13, 18, 5, 2]), clicked, clicked + 20_000, null)!;
    expect(done.done).toBe(true);
    expect(done.text).toBe("Reset all: 4 servers back in 20.0 s (0.2 a second)");
    const later = resetWave(cellsAt([43, 48, 35, 32]), clicked, clicked + 50_000, clicked + 20_000)!;
    expect(later.text).toBe("Reset all: 4 servers back in 20.0 s (0.2 a second)");
    // a minute after the last server is back it is history
    expect(resetWave(cellsAt([83, 88, 75, 72]), clicked, clicked + 20_000 + 60_001, clicked + 20_000)).toBeNull();
    expect(resetWave([], clicked, clicked + 1000, null)).toBeNull();
    expect(resetWave(cellsAt([1]), clicked, clicked - 1, null)).toBeNull();
  });
});

describe("the field in the protocol", () => {
  it("reads the snapshot's field and the status message, and refuses a field that does not add up", () => {
    const msg = parseServerMsg(JSON.stringify({ type: "minecraft_status", host: "aaaaaaaaaaaa", t: 7, rows: "AwDIAw==", cpus: 192 }));
    expect(msg).toEqual({ type: "minecraft_status", host: "aaaaaaaaaaaa", t: 7, rows: "AwDIAw==", cpus: 192, busy: null, mem_total_mib: null, mem_used_mib: null });
    expect(() => parseServerMsg(JSON.stringify({ type: "minecraft_status", host: "a", rows: "" }))).toThrow(ProtocolError);
  });
});
