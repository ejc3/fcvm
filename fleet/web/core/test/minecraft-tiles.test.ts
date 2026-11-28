// The tile sockets' glue (app/_wall/minecraft-tiles.ts) over a stand-in WebSocket: what it tells
// the page of a delta.
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { McTiles } from "../../app/_wall/minecraft-tiles.ts";

class FakeSocket {
  static last: FakeSocket | null = null;
  onopen: (() => void) | null = null;
  onmessage: ((e: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  readyState = 1;
  sent: string[] = [];
  readonly url: string;
  constructor(url: string) {
    this.url = url;
    FakeSocket.last = this;
  }
  send(s: string): void {
    this.sent.push(s);
  }
  close(): void {}
  say(m: unknown): void {
    this.onmessage?.({ data: JSON.stringify(m) });
  }
}

describe("a tile's delta, as the page is told of it", () => {
  const real = globalThis.WebSocket;
  beforeEach(() => {
    (globalThis as { WebSocket: unknown }).WebSocket = FakeSocket;
  });
  afterEach(() => {
    (globalThis as { WebSocket: unknown }).WebSocket = real;
  });

  it("names the columns that changed, a ground-only change too, and no column that did not", () => {
    const tiles = new McTiles();
    const told: { x: number; z: number }[][] = [];
    tiles.events = { status: () => {}, ack: () => {}, delta: (_id, _at, columns) => told.push([...columns]) };
    tiles.sync([{ id: "mc:1", socket: { url: "wss://tile.example.test/view", first: "{}", key: "k" } }]);
    const ws = FakeSocket.last!;
    ws.onopen!();
    // a 4 by 2 grid from (-8, -8): every column material 1 at height 64, the ground under it 2 at 63
    const flat = (m: number, h: number) => Buffer.from(Array.from({ length: 8 }, () => [m, h]).flat()).toString("base64");
    ws.say({ type: "map", seq: 1, region: { x0: -8, z0: -8, w: 4, h: 2 }, cells: flat(1, 64), ground: flat(2, 63) });
    // only the ground of column 5 changes (a dig under a column whose top stays): that column is named
    ws.say({ type: "delta", seq: 2, cells: [], ground: [[5, 3, 62]] });
    expect(told).toEqual([[{ x: -7, z: -7 }]]);
    // column 2's top changes; column 6 is sent as it already is, and index 99 is outside the grid: one column
    ws.say({ type: "delta", seq: 3, cells: [[2, 7, 65], [6, 1, 64], [99, 7, 65]] });
    expect(told[1]).toEqual([{ x: -6, z: -8 }]);
    tiles.sync([]);
  });
});
