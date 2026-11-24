import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import {
  applyDelta,
  botCount,
  sameColumns,
  BOT_DYES,
  decodeBase64,
  DRIVE_HEARTBEAT_MS,
  DRIVE_MIN_GAP_MS,
  driveInputOf,
  Driver,
  FIRST_CHANGE_GIVE_UP_MS,
  HEADER_RECENT_MS,
  MC_CONNECT_DEADLINE_MS,
  forkTarget,
  markersOf,
  MATERIAL_COLORS,
  MATERIALS,
  MC_UNKNOWN,
  minecraftNotice,
  MC_DRAWN_NOTE,
  MC_FLAT_NOTE,
  MC_MAX_COLUMNS,
  MC_MAX_SIDE,
  sameGrid,
  minecraftView,
  FIELD_SHARE,
  parseViewMsg,
  remapOf,
  shadeOf,
  shadeWorld,
  TURN_DEG_PER_S,
  viewFirstMessage,
  worldFromMap,
  wrapYaw,
  type DriveMsg,
  type McPlayer,
  type McTileLive,
  type McWorld,
  type MinecraftInput,
} from "../minecraft.ts";
import { INTENT_NAMES, parseServerMsg, ProtocolError, type Minecraft, type MinecraftTile } from "../protocol.ts";
import { initialState, reduce, type ClientState, type Notice } from "../store.ts";
import { EPOCH, fleet, snapshotText } from "./fixtures.ts";

const NOW = EPOCH + 30_000;
const SERVER = "Pumpkin 3ea7d281 (Java Edition 26.3)";

/** `n` servers in the coordinator's layout: one row of up to four, then rows of equal length, square tiles. */
function minecraftOf(n: number, over: Partial<Minecraft> = {}): Minecraft {
  const rows = Math.ceil(n / 4);
  const cols = Math.ceil(n / rows);
  const proto = fleet(1, 1).tiles[0]!;
  const tiles: MinecraftTile[] = [];
  for (let i = 0; i < n; i++) {
    tiles.push({
      ...structuredClone(proto),
      id: `mc:${i + 1}`,
      half: "fcvm",
      slot: `mc-${i + 1}`,
      vm: `fleet-mc-${i + 1}`,
      place: null,
      gpu: null,
      host: "example-host-1",
      generation: 3,
      rect: { x: (i % cols) / cols, y: Math.floor(i / cols) / rows, w: 1 / cols, h: 1 / rows },
      frame_url: `wss://mc-${i + 1}.example.test/view`,
      frame_url_v4: null,
      last_relaunch: { clicked_at: EPOCH, first_frame_at: EPOCH + 1900, phases: { restore_ms: 410, first_chunk_ms: 1900, total_ms: 1900 } },
      join: `mc-${i + 1}.example.test`,
      forked_from: null,
    });
  }
  return {
    server: SERVER,
    image_aspect: cols / rows,
    claim_secs: 120,
    borrow: { cap: 2, lent: 0, next_return_at: null },
    last_reset_all: null,
    last_fork: null,
    tiles,
    field: null,
    ...over,
  };
}

const TOKEN = "v1.1761135000.v-1.mac";

function stateWith(minecraft: Minecraft | null, you = "v-1", admission: Record<string, unknown> | null = { type: "admission", seat: "s", locked: false, reason: null, direct: { value: TOKEN, expires_at: NOW + 600_000 }, broadcast: null, seat_free: false, capacity: null }): ClientState {
  let s = reduce(initialState(), { type: "socket", status: "open", at: EPOCH });
  s = reduce(s, { type: "family", family: "v6", probeMs: 100, via: "race" });
  s = reduce(s, { type: "message", text: JSON.stringify({ type: "hello", viewer: you, turnstile: null }), at: EPOCH });
  if (admission) s = reduce(s, { type: "message", text: JSON.stringify(admission), at: EPOCH });
  return reduce(s, { type: "message", text: snapshotText(1, { ...fleet(), minecraft }), at: EPOCH });
}

const STAGE = { x: 0, y: 150, w: 1000, h: 520 };
const LIVE = (n: number, tps: number | null = 19.98, viewer = "v-1") =>
  Object.fromEntries(Array.from({ length: n }, (_, i) => [`mc:${i + 1}`, { open: true, viewer, tps, hasMap: true, failures: 0, connectingSince: null }]));
const input = (s: ClientState, over: Partial<MinecraftInput> = {}): MinecraftInput => ({ state: s, now: NOW, dpr: 1, stage: STAGE, narrow: false, touch: false, live: LIVE(8), drive: null, ...over });
const viewOf = (m: Minecraft | null, over: Partial<MinecraftInput> = {}, you = "v-1") => minecraftView(input(stateWith(m, you), over));

function with1(m: Minecraft, i: number, patch: Partial<MinecraftTile>): Minecraft {
  return { ...m, tiles: m.tiles.map((t, k) => (k === i ? { ...t, ...patch } : t)) };
}

describe("minecraft in the snapshot", () => {
  const snap = (state: Record<string, unknown>) => parseServerMsg(JSON.stringify({ type: "snapshot", version: 1, state }));
  const base = (minecraft: unknown) => ({ ...fleet(), minecraft }) as unknown as Record<string, unknown>;

  it("reads the coordinator's servers, the last fork and the last reset all", () => {
    const m = minecraftOf(4, {
      last_reset_all: { clicked_at: 1_761_134_400_000, by: "viewer:v-3", servers: 4 },
      last_fork: { from: "mc-1", to: "mc-2", clicked_at: 1_761_134_410_000, by: "viewer:v-5", first_chunk_at: 1_761_134_412_300, snapshot_ms: 640, restore_ms: 1100, total_ms: 2300 },
      borrow: { cap: 2, lent: 1, next_return_at: 1_761_134_525_000 },
    });
    m.tiles[1]!.forked_from = "mc-1";
    m.tiles[2]!.borrowed = { session: "minecraft-claim-3", viewer: "v-12", since: 1000, until: 121_000 };
    const got = snap(base(m));
    if (got.type !== "snapshot") throw new Error("not a snapshot");
    expect(got.state.minecraft).toEqual(m);
  });

  it("has no servers when the coordinator sends none, or says null; a server name may be null", () => {
    for (const minecraft of [undefined, null]) {
      const got = snap(base(minecraft));
      if (got.type !== "snapshot") throw new Error("not a snapshot");
      expect(got.state.minecraft).toBeNull();
    }
    const m = minecraftOf(1, { server: null });
    const got = snap(base(m));
    if (got.type !== "snapshot") throw new Error("not a snapshot");
    expect(got.state.minecraft!.server).toBeNull();
    expect(got.state.minecraft!.tiles[0]!.join).toBe("mc-1.example.test");
  });

  type Raw = Record<string, unknown> & { tiles: Record<string, unknown>[]; borrow: Record<string, unknown> };
  const loose = (w: Raw) => w as Record<string, unknown>;
  it.each<[string, (w: Raw) => void, RegExp]>([
    ["no aspect", (w) => { delete loose(w)["image_aspect"]; }, /minecraft\.image_aspect is not a number/],
    ["an aspect of zero", (w) => { w["image_aspect"] = 0; }, /image_aspect is not positive/],
    ["no claim time", (w) => { delete loose(w)["claim_secs"]; }, /minecraft\.claim_secs is not a number/],
    ["no borrow", (w) => { delete loose(w)["borrow"]; }, /minecraft\.borrow is not an object/],
    ["a server twice", (w) => { w.tiles[1]!["id"] = w.tiles[0]!["id"]; }, /appears twice/],
    ["a join that is a number", (w) => { w.tiles[0]!["join"] = 7; }, /tiles\[0\]\.join is not a string/],
    ["a reset all without its count", (w) => { w["last_reset_all"] = { clicked_at: 1, by: "viewer:v-1" }; }, /last_reset_all\.servers is not a number/],
    ["a fork without its source", (w) => { w["last_fork"] = { to: "mc-2", clicked_at: 1, by: "x" }; }, /last_fork\.from is not a string/],
  ])("rejects %s", (_, mutate, error) => {
    const w = structuredClone(minecraftOf(2)) as unknown as Raw;
    mutate(w);
    const bad = () => snap(base(w));
    expect(bad).toThrow(ProtocolError);
    expect(bad).toThrow(error);
  });

  it("rejects a minecraft section that is not an object", () => {
    expect(() => snap(base("x"))).toThrow(/minecraft is not an object/);
  });

  it("sends the four intents by the coordinator's names", () => {
    for (const n of ["minecraft_reset", "minecraft_fork", "claim_minecraft", "release_minecraft"]) expect(INTENT_NAMES).toContain(n);
  });
});

describe("materials and colours", () => {
  it("holds the tile's 48 materials in the tile's own order (fleet/minecraft/src/palette.rs)", () => {
    const rust = readFileSync(new URL("../../../minecraft/src/palette.rs", import.meta.url), "utf8");
    const block = /pub const MATERIALS: \[&str; 48\] = \[([\s\S]*?)\];/.exec(rust)![1]!;
    const names = [...block.matchAll(/"([a-z_]+)"/g)].map((m) => m[1]);
    expect(names).toEqual([...MATERIALS]);
  });

  it("has one colour for each, all distinct, and the bots' dyes in fleet/minecraft/src/bots.rs's order", () => {
    expect(MATERIAL_COLORS).toHaveLength(48);
    for (const c of MATERIAL_COLORS) expect(c).toMatch(/^#[0-9a-f]{6}$/);
    expect(new Set(MATERIAL_COLORS).size).toBe(48);
    const rust = readFileSync(new URL("../../../minecraft/src/bots.rs", import.meta.url), "utf8");
    const dyes = [.../pub const COLORS: \[&str; 8\] = \[([^\]]*)\]/.exec(rust)![1]!.matchAll(/"([a-z_]+)"/g)].map((m) => m[1]);
    expect(dyes).toEqual([...BOT_DYES]);
  });

  it("reads as the names say: water blue, grass and leaves green, sand tan, snow near white, the dyes their colour", () => {
    const rgb = (name: string) => {
      const v = Number.parseInt(MATERIAL_COLORS[MATERIALS.indexOf(name as never)]!.slice(1), 16);
      return { r: (v >> 16) & 255, g: (v >> 8) & 255, b: v & 255 };
    };
    for (const name of ["water", "blue", "light_blue"]) expect(rgb(name).b, name).toBeGreaterThan(Math.max(rgb(name).r, rgb(name).g));
    for (const name of ["grass", "leaves", "lime", "green", "plant"]) expect(rgb(name).g, name).toBeGreaterThan(Math.max(rgb(name).r, rgb(name).b));
    for (const name of ["red", "orange", "lava", "brick"]) expect(rgb(name).r, name).toBeGreaterThan(Math.max(rgb(name).g, rgb(name).b));
    const sand = rgb("sand");
    expect(sand.r > sand.g && sand.g > sand.b && sand.b > 100).toBe(true);
    for (const name of ["snow", "white"]) expect(Math.min(rgb(name).r, rgb(name).g, rgb(name).b), name).toBeGreaterThan(230);
    expect(Math.max(rgb("black").r, rgb("black").g, rgb("black").b)).toBeLessThan(40);
  });
});

/** A map message for `cells` ([material, height] per column) on a `w` x `h` region. */
const mapText = (w: number, h: number, cells: [number, number][], seq = 1) =>
  JSON.stringify({ type: "map", seq, region: { x0: -8, z0: -8, w, h }, cells: Buffer.from(cells.flat()).toString("base64") });

describe("the view socket's messages", () => {
  it("decodes base64 as Node does, with or without padding, and refuses anything else", () => {
    for (const len of [0, 1, 2, 3, 4, 5, 31, 256]) {
      const bytes = Uint8Array.from({ length: len }, (_, i) => (i * 37 + 11) & 255);
      const b64 = Buffer.from(bytes).toString("base64");
      expect(decodeBase64(b64)).toEqual(bytes);
      expect(decodeBase64(b64.replace(/=+$/, ""))).toEqual(bytes);
    }
    expect(() => decodeBase64("ab$d")).toThrow(ProtocolError);
    expect(() => decodeBase64("abcde")).toThrow(ProtocolError);
  });

  it("reads hello, map, delta, status and drive_ack, and ignores a type it does not know", () => {
    const hello = parseViewMsg(JSON.stringify({ type: "hello", tile: "mc-2", generation: 7, viewer: "v-12", materials: [...MATERIALS], bots: 3, bot_names: ["fcvm_bot1", "fcvm_bot2", "fcvm_bot3"], driver: null }));
    expect(hello).toMatchObject({ type: "hello", tile: "mc-2", generation: 7, viewer: "v-12", bots: 3, driver: null });
    const map = parseViewMsg(mapText(2, 1, [[2, 130], [1, 126]]));
    expect(map).toMatchObject({ type: "map", seq: 1, region: { x0: -8, z0: -8, w: 2, h: 1 } });
    expect(map?.type === "map" && [...map.cells]).toEqual([2, 130, 1, 126]);
    expect(parseViewMsg(JSON.stringify({ type: "delta", seq: 42, cells: [[3, 46, 140]] }))).toEqual({ type: "delta", seq: 42, cells: [[3, 46, 140]] });
    const st = parseViewMsg(JSON.stringify({ type: "status", t: 5, tps: 19.98, players: [{ name: "fcvm_bot1", x: 1.5, y: 70, z: -2, yaw: 90, bot: true, driven: true }], drive: { seq: 12, moved_at: 6 } }));
    expect(st).toEqual({ type: "status", t: 5, tps: 19.98, players: [{ name: "fcvm_bot1", x: 1.5, y: 70, z: -2, yaw: 90, bot: true, driven: true }], drive: { seq: 12, moved_at: 6 }, day_time: null });
    expect(parseViewMsg(JSON.stringify({ type: "status", t: 5, tps: null, players: [], drive: null }))).toMatchObject({ tps: null, drive: null });
    expect(parseViewMsg(JSON.stringify({ type: "drive_ack", seq: 12, ok: false, reason: "another viewer holds this tile" }))).toEqual({ type: "drive_ack", seq: 12, ok: false, at: null, reason: "another viewer holds this tile" });
    expect(parseViewMsg(JSON.stringify({ type: "later_addition" }))).toBeNull();
  });

  it.each([
    ["not JSON", "{", /not JSON/],
    ["a map whose cells are short", mapText(2, 2, [[1, 1]]), /holds 2 bytes, not 8/],
    ["a map with no region", JSON.stringify({ type: "map", seq: 1, cells: "" }), /map\.region is not an object/],
    ["a map of no columns", JSON.stringify({ type: "map", seq: 1, region: { x0: 0, z0: 0, w: 0, h: 4 }, cells: "" }), /not a grid/],
    ["a delta cell of two numbers", JSON.stringify({ type: "delta", seq: 1, cells: [[1, 2]] }), /delta\.cells\[0\]/],
    ["a player without a position", JSON.stringify({ type: "status", t: 1, tps: 20, players: [{ name: "a", y: 1, z: 1, yaw: 0 }], drive: null }), /players\[0\]\.x is not a number/],
  ])("refuses %s", (_, text, error) => {
    expect(() => parseViewMsg(text)).toThrow(error);
  });
});

describe("a tile's world", () => {
  const world = (): McWorld => {
    const m = parseViewMsg(mapText(3, 2, [[2, 130], [2, 130], [1, 126], [4, 127], [5, 150], [0, 0]], 5));
    if (m?.type !== "map") throw new Error("no map");
    return worldFromMap(m);
  };

  it("applies a delta's columns in place and counts the ones that changed", () => {
    const w = world();
    expect(applyDelta(w, { type: "delta", seq: 6, cells: [[0, 46, 133], [1, 2, 130], [99, 1, 1]] })).toBe(1);
    expect([...w.cells.slice(0, 4)]).toEqual([46, 133, 2, 130]);
    expect(w.seq).toBe(6);
  });

  it("ignores a delta before any map, and one older than the world", () => {
    expect(applyDelta(null, { type: "delta", seq: 9, cells: [[0, 1, 1]] })).toBe(0);
    const w = world();
    expect(applyDelta(w, { type: "delta", seq: 5, cells: [[0, 46, 133]] })).toBe(0);
    expect(w.cells[0]).toBe(2);
  });

  it("shades each column its material's colour, lighter the higher it stands, and an unread column as unknown", () => {
    const w = world();
    const px = shadeWorld(w, null);
    expect(px).toHaveLength(3 * 2 * 4);
    const at = (i: number) => [px[i * 4]!, px[i * 4 + 1]!, px[i * 4 + 2]!];
    const unknown = Number.parseInt(MC_UNKNOWN.slice(1), 16);
    expect(at(5)).toEqual([(unknown >> 16) & 255, (unknown >> 8) & 255, unknown & 255]);
    // water is blue whatever its neighbours
    expect(at(2)[2]).toBeGreaterThan(at(2)[0]!);
    // a stone column high up is lighter than the same stone at sea level
    expect(shadeOf(150)).toBeGreaterThan(shadeOf(126));
    const flat = worldFromMap({ type: "map", seq: 1, region: { x0: 0, z0: 0, w: 1, h: 2 }, cells: Uint8Array.from([5, 126, 5, 150]) });
    const fp = shadeWorld(flat, null);
    expect(fp[4]! + fp[5]! + fp[6]!).toBeGreaterThan(fp[0]! + fp[1]! + fp[2]!);
    expect(px.every((_, i) => i % 4 !== 3 || px[i] === 255)).toBe(true);
  });

  it("colours by the tile's material names, so a tile that lists them in another order is still drawn right", () => {
    const names = [...MATERIALS].reverse();
    const remap = remapOf([...names, "something_new"]);
    expect(remap[names.indexOf("water")]).toBe(MATERIALS.indexOf("water"));
    expect(remap[48]).toBe(0);
    const w = worldFromMap({ type: "map", seq: 1, region: { x0: 0, z0: 0, w: 1, h: 1 }, cells: Uint8Array.from([names.indexOf("water"), 126]) });
    const px = shadeWorld(w, remap);
    expect(px[2]).toBeGreaterThan(px[0]!);
  });
});

describe("players on a tile", () => {
  const region = { x0: -64, z0: -64, w: 128, h: 128 };
  const p = (over: Partial<McPlayer>): McPlayer => ({ name: "fcvm_bot1", x: 0, y: 70, z: 0, yaw: 0, bot: true, driven: false, ...over });
  const names = ["fcvm_bot1", "fcvm_bot2", "fcvm_bot3"];

  it("places each at its column, north up, with a tick toward where it faces", () => {
    const [m] = markersOf(region, [p({ x: 0, z: 0, yaw: 0 })], names, 256, 256);
    expect([m!.x, m!.y]).toEqual([128, 128]);
    // yaw 0 faces +z: down the map
    expect(m!.ty).toBeGreaterThan(m!.y);
    expect(m!.tx).toBeCloseTo(m!.x, 6);
    // yaw 90 faces -x: left
    const [w] = markersOf(region, [p({ yaw: 90 })], names, 256, 256);
    expect(w!.tx).toBeLessThan(w!.x);
    expect(w!.ty).toBeCloseTo(w!.y, 6);
  });

  it("draws each bot in its wool's colour by its number, a person in white, and drops a player off the map", () => {
    const ms = markersOf(region, [p({ name: "fcvm_bot2" }), p({ name: "someone", bot: false }), p({ x: 500 })], names, 256, 256);
    expect(ms.map((m) => m.color)).toEqual([MATERIAL_COLORS[MATERIALS.indexOf("blue")], "#ffffff"]);
    expect(markersOf(region, [p({ driven: true })], names, 256, 256)[0]!.driven).toBe(true);
    // names only where there is room
    expect(markersOf(region, [p({})], names, 90, 90)[0]!.label).toBe(false);
  });
});

describe("the view socket's first message", () => {
  it("is the admission's direct token; with no admission at all the coordinator's viewer; nothing for a late viewer", () => {
    expect(JSON.parse(viewFirstMessage(stateWith(minecraftOf(1)))!)).toEqual({ admit: TOKEN });
    expect(JSON.parse(viewFirstMessage(stateWith(minecraftOf(1), "v-4", null))!)).toEqual({ viewer: "v-4" });
    const late = stateWith(minecraftOf(1), "v-4", { type: "admission", seat: null, locked: true, reason: "Direct streams are full.", direct: null, broadcast: null, seat_free: false });
    expect(viewFirstMessage(late)).toBeNull();
    expect(viewFirstMessage(initialState())).toBeNull();
  });
});

describe("driving", () => {
  it("maps W A S D, space, shift, the arrows, E and Q, and nothing else", () => {
    expect(["KeyW", "KeyA", "KeyS", "KeyD", "Space", "ShiftLeft", "ShiftRight", "ArrowLeft", "ArrowRight", "KeyE", "KeyQ"].map(driveInputOf)).toEqual([
      "forward", "left", "back", "right", "jump", "sprint", "sprint", "turn_left", "turn_right", "place", "dig",
    ]);
    for (const c of ["KeyT", "Enter", "Slash", "ArrowUp", "Digit1"]) expect(driveInputOf(c)).toBeNull();
  });

  it("sends at once on a key change, again every 250 ms while held, and the release", () => {
    const d = new Driver();
    d.observeStatus([{ name: "b", x: 0, y: 70, z: 0, yaw: 30, bot: true, driven: true }], 0);
    d.press("forward", 1000);
    const a = d.tick(1000)!;
    expect(a).toEqual({ type: "drive", seq: 1, keys: ["forward"], yaw: 30, pitch: 45, action: null });
    expect(d.tick(1100)).toBeNull();
    expect(d.nextAt(1100)).toBe(1000 + DRIVE_HEARTBEAT_MS);
    expect(d.tick(1000 + DRIVE_HEARTBEAT_MS)!.seq).toBe(2);
    d.press("sprint", 1300);
    expect(d.tick(1300)!.keys).toEqual(["forward", "sprint"]);
    d.release("forward", 1310);
    d.release("sprint", 1310);
    expect(d.tick(1310)).toBeNull();
    expect(d.nextAt(1310)).toBe(1300 + DRIVE_MIN_GAP_MS);
    expect(d.tick(1340)).toMatchObject({ seq: 4, keys: [] });
    expect(d.nextAt(1400)).toBeNull();
  });

  it("puts each action in a message of its own, in order", () => {
    const d = new Driver();
    d.press("place", 0);
    d.press("dig", 1);
    const got: (DriveMsg | null)[] = [];
    for (let t = 0; t < 200; t += 10) {
      const m = d.tick(t);
      if (m) got.push(m);
    }
    expect(got.map((m) => m!.action)).toEqual(["place", "dig"]);
    expect(got.map((m) => m!.seq)).toEqual([1, 2]);
  });

  it("turns with the arrows at a fixed rate and with a drag, the yaw kept in (-180, 180]", () => {
    const d = new Driver();
    d.press("turn_right", 0);
    d.tick(0);
    const m = d.tick(1000)!;
    expect(m.yaw).toBeCloseTo(TURN_DEG_PER_S, 0);
    d.release("turn_right", 1000);
    d.drag(-100, 1100);
    expect(d.tick(1100)!.yaw).toBeCloseTo(TURN_DEG_PER_S - 40, 0);
    expect(wrapYaw(190)).toBe(-170);
    expect(wrapYaw(-180)).toBe(180);
    expect(wrapYaw(540)).toBe(180);
  });

  it("never sends more than 25 messages in any second, with seq always increasing", () => {
    const d = new Driver();
    const sent: DriveMsg[] = [];
    const sendAt: number[] = [];
    const keys = ["forward", "left", "back", "right", "jump"] as const;
    for (let t = 0; t < 5000; t += 5) {
      if (t % 15 === 0) d.press(keys[(t / 15) % 5]!, t);
      if (t % 20 === 0) d.release(keys[(t / 20) % 5]!, t);
      if (t % 25 === 0) d.drag(3, t);
      const m = d.tick(t);
      if (m) {
        sent.push(m);
        sendAt.push(t);
      }
    }
    for (let i = 0; i < sendAt.length; i++) expect(sendAt.filter((x) => x >= sendAt[i]! && x < sendAt[i]! + 1000).length).toBeLessThanOrEqual(25);
    for (let i = 1; i < sent.length; i++) expect(sent[i]!.seq).toBe(sent[i - 1]!.seq + 1);
    expect(sent.length).toBeGreaterThan(100);
  });

  it("times a key to the first status where the driven bot moved or turned", () => {
    const d = new Driver();
    const bot = (x: number, yaw = 0): McPlayer[] => [{ name: "b", x, y: 70, z: 0, yaw, bot: true, driven: true }];
    d.observeStatus(bot(0), 0);
    d.press("forward", 1000);
    expect(d.observeStatus(bot(0), 1050)).toBeNull();
    expect(d.observeStatus(bot(0.4), 1090)).toBe(90);
    // a second key while one is pending does not restart the clock; after it, a new one starts
    d.release("forward", 1200);
    d.press("turn_left", 2000);
    expect(d.observeStatus(bot(0.4, -5), 2070)).toBe(70);
    expect(d.samples).toEqual([90, 70]);
    // sprint alone moves nothing and is not timed
    d.release("turn_left", 2100);
    d.press("sprint", 3000);
    expect(d.observeStatus(bot(1), 3050)).toBeNull();
  });

  it("times a place or a dig to the first delta, and gives up on a key with no change in 5 s", () => {
    const d = new Driver();
    const near = [{ x: 1, z: 0 }];
    d.observeStatus([{ name: "b", x: 0.5, y: 70, z: 0.5, yaw: 0, bot: true, driven: true }], 50);
    d.press("place", 100);
    expect(d.observeDelta(160, near)).toBe(60);
    d.press("dig", 1000);
    expect(d.observeDelta(1000 + FIRST_CHANGE_GIVE_UP_MS + 1, near)).toBeNull();
    d.press("forward", 9000);
    d.observeStatus([{ name: "b", x: 0, y: 70, z: 0, yaw: 0, bot: true, driven: true }], 9000 + FIRST_CHANGE_GIVE_UP_MS + 10);
    expect(d.samples).toEqual([60]);
  });

  it("does not time a place by a change far from the driven bot, nor a refused one", () => {
    const d = new Driver();
    d.observeStatus([{ name: "b", x: 0.5, y: 70, z: 0.5, yaw: 0, bot: true, driven: true }], 50);
    d.press("place", 100);
    expect(d.observeDelta(150, [{ x: 40, z: 3 }])).toBeNull();
    d.observeAck(false);
    expect(d.observeDelta(200, [{ x: 1, z: 1 }])).toBeNull();
    expect(d.samples).toEqual([]);
  });

  it("starts from the bot's own facing, so the first message does not turn it", () => {
    const d = new Driver();
    d.observeStatus([{ name: "x", x: 0, y: 0, z: 0, yaw: 0, bot: false, driven: false }, { name: "b", x: 0, y: 70, z: 0, yaw: -120, bot: true, driven: true }], 0);
    d.press("forward", 10);
    expect(d.tick(10)!.yaw).toBe(-120);
  });
});

describe("a server's words", () => {
  it("says its slot, its ticks per second from its own socket, and the time from the click to the first chunk", () => {
    const v = viewOf(minecraftOf(4));
    expect(v.cards.map((c) => c.slot)).toEqual(["mc-1", "mc-2", "mc-3", "mc-4"]);
    for (const c of v.cards) {
      expect(c.tps).toBe("20.0 TPS");
      expect(c.upIn).toBe("Up in 1.90 s");
      expect(c.forkedFrom).toBeNull();
      expect(c.join).toBe(`${c.slot}.example.test:25565`);
      expect([c.dark, c.timer, c.error]).toEqual([false, null, null]);
    }
    expect(viewOf(minecraftOf(1), { live: {} }).cards[0]!.tps).toBe("-- TPS");
    expect(viewOf(minecraftOf(1), { live: LIVE(1, 17.04) }).cards[0]!.tps).toBe("17.0 TPS");
  });

  it("a fork's copy says where it came from and how long it took, split into the snapshot and the restore", () => {
    const m = with1(minecraftOf(4, { last_fork: { from: "mc-1", to: "mc-2", clicked_at: EPOCH, by: "viewer:v-1", first_chunk_at: EPOCH + 2300, snapshot_ms: 640, restore_ms: 1100, total_ms: 2300 } }), 1, {
      forked_from: "mc-1",
      last_relaunch: { clicked_at: EPOCH, first_frame_at: EPOCH + 2300, phases: { snapshot_ms: 640, restore_ms: 1100, total_ms: 2300 } },
    });
    const c = viewOf(m).cards[1]!;
    expect(c.forkedFrom).toBe("Forked from mc-1 in 2.30 s");
    expect(c.forkSplit).toBe("snapshot 0.64 s, restore 1.10 s");
    expect(c.upIn).toBe("Up in 2.30 s");
    // without the coordinator's last fork, the agent's phases give the split
    const older = { ...m, last_fork: null };
    expect(viewOf(older).cards[1]!.forkSplit).toBe("snapshot 0.64 s, restore 1.10 s");
  });

  it.each([
    ["restoring", "Restoring 1.4 s"],
    ["relaunching", "Restoring 1.4 s"],
    ["forking", "Forking 1.4 s"],
  ])("%s: a dark tile with a timer from the click, no socket and no ticks", (phase, timer) => {
    const m = with1(minecraftOf(2), 1, { phase, last_relaunch: { clicked_at: NOW - 1400, first_frame_at: null, phases: {} } });
    const c = viewOf(m).cards[1]!;
    expect([c.dark, c.timer, c.upIn, c.socket, c.tps]).toEqual([true, timer, null, null, "-- TPS"]);
    expect(c.fork.disabled).toBe(true);
    expect(c.take.disabled).toBe(true);
  });

  it("a failed server is dark and shows its error", () => {
    const m = with1(minecraftOf(2), 0, { phase: "failed", error: "restore: health not ready in 60 s" });
    expect(viewOf(m).cards[0]!).toMatchObject({ dark: true, timer: null, error: "Failed: restore: health not ready in 60 s" });
  });

  it("opens a socket to each running server with the first message, and a new one for each launch", () => {
    const v = viewOf(minecraftOf(2));
    expect(v.cards[0]!.socket).toEqual({ url: "wss://mc-1.example.test/view", first: JSON.stringify({ admit: TOKEN }), key: "wss://mc-1.example.test/view#3" });
    const next = viewOf(with1(minecraftOf(2), 0, { generation: 4 }));
    expect(next.cards[0]!.socket!.key).not.toBe(v.cards[0]!.socket!.key);
    const none = minecraftView(input(stateWith(minecraftOf(2), "v-1", { type: "admission", seat: null, locked: true, reason: "Direct streams are full.", direct: null, broadcast: null, seat_free: false })));
    expect(none.cards.every((c) => c.socket === null)).toBe(true);
    expect(none.banner).toMatch(/only to viewers with a direct seat/);
  });

  it("opens each socket over the family the page's probe chose, and none before it has", () => {
    const m = minecraftOf(1, {});
    m.tiles[0]!.frame_url_v4 = "wss://mc-1-v4.example.test:8443/view";
    const at = (family: "v4" | "v6" | null) => {
      let s = reduce(initialState(), { type: "socket", status: "open", at: EPOCH });
      if (family !== null) s = reduce(s, { type: "family", family, probeMs: 100, via: "race" });
      s = reduce(s, { type: "message", text: JSON.stringify({ type: "hello", viewer: "v-1", turnstile: null }), at: EPOCH });
      s = reduce(s, { type: "message", text: JSON.stringify({ type: "admission", seat: "s", locked: false, reason: null, direct: { value: TOKEN, expires_at: NOW + 600_000 }, broadcast: null, seat_free: false, capacity: null }), at: EPOCH });
      s = reduce(s, { type: "message", text: snapshotText(1, { ...fleet(), minecraft: m }), at: EPOCH });
      return minecraftView(input(s)).cards[0]!.socket?.url ?? null;
    };
    expect(at("v4")).toBe("wss://mc-1-v4.example.test:8443/view");
    expect(at("v6")).toBe("wss://mc-1.example.test/view");
    expect(at(null)).toBeNull();
    // The probe tries the Minecraft servers' own sockets: without them it never settles.
    const page = readFileSync(new URL("../../app/_wall/Minecraft.tsx", import.meta.url), "utf8");
    // the family is probed over the servers' view sockets, and the page asks for the servers' status rows
    expect(page).toMatch(/new WallClient\(coordUrl, \(st\) => st\.minecraft\?\.tiles \?\? \[\], directUrl, true\)/);
  });
});

describe("the header", () => {
  it("counts the servers up and gives the server, the median and slowest restore, the last fork and the last reset all", () => {
    const first = (ms: number | null): Partial<MinecraftTile> => ({ last_relaunch: { clicked_at: EPOCH, first_frame_at: ms === null ? null : EPOCH + ms, phases: {} } });
    let m = minecraftOf(4);
    m = with1(m, 0, first(1500));
    m = with1(m, 1, first(2600));
    m = with1(m, 2, first(1900));
    m = with1(m, 3, { phase: "restoring", ...first(null) });
    const h = viewOf(m).header!;
    expect(h.up).toBe("3 of 4 servers up");
    expect(h.server).toBe(SERVER);
    expect(h.median).toBe("Median restore to first chunk 1.90 s");
    expect(h.slowest).toBe("Slowest 2.60 s");
    // Nothing to report is no line at all, not "not used yet".
    expect(h.lastFork).toBeNull();
    expect(h.lastResetAll).toBeNull();
    const one = viewOf(with1(minecraftOf(1), 0, { phase: "pending", last_relaunch: null })).header!;
    expect([one.up, one.median, one.slowest]).toEqual(["0 of 1 server up", "Median restore to first chunk --", "Slowest --"]);
  });

  it("gives the last fork with its age, snapshot and restore, and the last reset all by whom, while they are recent", () => {
    const at = NOW - 5 * 60_000;
    const m = minecraftOf(4, {
      last_fork: { from: "mc-1", to: "mc-2", clicked_at: at - 2300, by: "viewer:v-5", first_chunk_at: at, snapshot_ms: 640, restore_ms: 1100, total_ms: 2300 },
      last_reset_all: { clicked_at: at, by: "viewer:v-1", servers: 4 },
    });
    const h = viewOf(m).header!;
    expect(h.lastFork).toBe("Last fork 5 min ago: mc-1 to mc-2 in 2.30 s (snapshot 0.64 s, restore 1.10 s)");
    expect(h.lastResetAll).toBe("Last reset all 5 min ago: 4 servers, by you");
    expect(viewOf(m, {}, "v-9").header!.lastResetAll).toBe("Last reset all 5 min ago: 4 servers, by another viewer");
    // Last night's fork and reset are history: the header drops them.
    const old = NOW - HEADER_RECENT_MS - 1000;
    const stale = viewOf(
      minecraftOf(4, {
        last_fork: { from: "mc-1", to: "mc-2", clicked_at: old - 3890, by: "viewer:v-5", first_chunk_at: old, snapshot_ms: 1090, restore_ms: 2010, total_ms: 3890 },
        last_reset_all: { clicked_at: old, by: "viewer:v-1", servers: 4 },
      }),
    ).header!;
    expect([stale.lastFork, stale.lastResetAll]).toEqual([null, null]);
  });

  it("tells Java Edition players the client and one address to copy when tiles have one, and nothing when none does", () => {
    expect(viewOf(minecraftOf(2)).header!.join).toEqual({ label: "Join with Java Edition 26.3:", address: "mc-1.example.test:25565" });
    const none = minecraftOf(2);
    for (const t of none.tiles) t.join = null;
    expect(viewOf(none).header!.join).toBeNull();
    expect(viewOf(none).cards.every((c) => c.join === null)).toBe(true);
    expect(viewOf(minecraftOf(1, { server: "Some server" })).header!.join).toEqual({ label: "Join with Java Edition:", address: "mc-1.example.test:25565" });
  });

  it("Reset all is on while online, off when the socket is closed", () => {
    expect(viewOf(minecraftOf(2)).resetAll).toEqual({ label: "Reset all", disabled: false });
    const s = reduce(stateWith(minecraftOf(2)), { type: "socket", status: "closed", at: EPOCH });
    const v = minecraftView(input(s));
    expect(v.resetAll.disabled).toBe(true);
    expect(v.cards.every((c) => c.reset.disabled && c.fork.disabled && c.take.disabled)).toBe(true);
    expect(v.banner).toMatch(/^Reconnecting to the coordinator\./);
  });
});

describe("forks", () => {
  it("copies into the next running, unlent server on the source's host, in index order and wrapping", () => {
    const m = minecraftOf(4);
    const t = (k: number) => m.tiles[k]!;
    expect(forkTarget(m.tiles, t(0))!.slot).toBe("mc-2");
    expect(forkTarget(m.tiles, t(3))!.slot).toBe("mc-1");
    t(1).borrowed = { session: "s", viewer: "v-9", until: NOW + 1000 };
    t(2).phase = "restoring";
    expect(forkTarget(m.tiles, t(0))!.slot).toBe("mc-4");
    t(3).host = "example-host-2";
    expect(forkTarget(m.tiles, t(0))).toBeNull();
  });

  it("Fork is off with a reason when no server on the host can take a copy", () => {
    const m = minecraftOf(2);
    m.tiles[1]!.borrowed = { session: "s", viewer: "v-9", until: NOW + 1000 };
    const c = viewOf(m).cards[0]!;
    expect(c.fork).toEqual({ label: "Fork", disabled: true, title: "No other server on its host can take a copy now." });
    expect(viewOf(minecraftOf(2)).cards[0]!.fork).toEqual({ label: "Fork", disabled: false, title: "Copies its world into mc-2." });
  });
});

describe("taking a server over", () => {
  const held = (viewer: string): Minecraft => {
    const m = minecraftOf(4, { borrow: { cap: 2, lent: 1, next_return_at: NOW + 97_000 } });
    m.tiles[2]!.borrowed = { session: "minecraft-claim-3", viewer, since: NOW - 23_000, until: NOW + 97_000 };
    return m;
  };

  it("offers Take over on a running server that is free, and gives no panel", () => {
    const v = viewOf(minecraftOf(4));
    expect(v.panel).toBeNull();
    for (const c of v.cards) expect(c.take).toEqual({ label: "Take over", disabled: false, title: null });
  });

  it("gives the holder a panel: the countdown, the keys, the time to the first change, Give back; driving once the tile's socket acts as the holder", () => {
    const v = viewOf(held("v-1"), { drive: { samples: [80, 120, 95], refused: null } });
    const p = v.panel!;
    expect([p.slot, p.session, p.countdown, p.giveBack]).toEqual(["mc-3", "minecraft-claim-3", "Returns in 97 s", "Give back"]);
    expect(p.keys).toMatch(/^W A S D walk/);
    expect(p.firstChange).toBe("Key to first change: last 95 ms, median 95 ms (3 keys)");
    expect([p.driving, p.waiting, p.pad]).toEqual([true, null, null]);
    expect(viewOf(held("v-1")).panel!.firstChange).toMatch(/^Press a key/);
    const mine = v.cards[2]!;
    expect([mine.mine, mine.lentText, mine.reset.disabled, mine.reset.title]).toEqual([true, "Yours", true, "Yours: give it back and it is reset."]);
    expect(v.cards[0]!.take).toMatchObject({ disabled: true, title: "You are driving mc-3. Give it back to take another." });
  });

  it("keys do not reach the bot until the tile's socket is open and acts as the holder, nor while it restores", () => {
    const connecting = viewOf(held("v-1"), { live: {} }).panel!;
    expect([connecting.driving, connecting.waiting]).toEqual([false, "Connecting to mc-3."]);
    const other = viewOf(held("v-1"), { live: LIVE(4, 20, "v-7") }).panel!;
    expect(other.driving).toBe(false);
    expect(other.waiting).toMatch(/acts as another viewer/);
    const m = held("v-1");
    m.tiles[2]!.phase = "restoring";
    expect(viewOf(m).panel!.waiting).toBe("mc-3 is restoring; the keys reach its bot once it is running.");
  });

  it("a touch layout gets the on-screen buttons", () => {
    const p = viewOf(held("v-1"), { touch: true }).panel!;
    expect(p.pad!.map((b) => b.label)).toEqual(["Turn left", "Forward", "Turn right", "Left", "Back", "Right", "Jump", "Sprint", "Place", "Break"]);
    expect(p.keys).toMatch(/^Hold a button/);
  });

  it("shows the tile's refusal of a drive message", () => {
    expect(viewOf(held("v-1"), { drive: { samples: [], refused: "nobody holds this tile; take it over first" } }).panel!.refused).toBe("nobody holds this tile; take it over first");
  });

  it("another viewer's borrow shows no panel to this viewer, only that it is lent and until when", () => {
    const v = viewOf(held("v-12"));
    expect(v.panel).toBeNull();
    const c = v.cards[2]!;
    expect([c.lent, c.mine, c.reset.disabled, c.take.disabled]).toEqual([true, false, true, true]);
    expect(c.lentText).toBe(`Lent to another viewer until ${new Date(NOW + 97_000).toISOString().slice(11, 19)} UTC`);
    expect(v.cards[0]!.take.disabled).toBe(false);
  });
});

describe("refusals", () => {
  const n = (intent: Notice["intent"], reason: string | null, kind: Notice["kind"] = "refused"): Notice => ({ id: reason ?? "x", kind, intent, reason, args: {}, at: EPOCH });
  it("shows the newest refusal of a Minecraft intent as the coordinator worded it", () => {
    expect(minecraftNotice([n("claim_minecraft", "one server per client at a time"), n("minecraft_fork", "one fork per 10 s per client")])).toBe("one fork per 10 s per client");
    expect(minecraftNotice([n("minecraft_reset", "mc-2 is lent to a viewer"), n("windows_reset", "no")])).toBe("mc-2 is lent to a viewer");
    expect(minecraftNotice([n("release_minecraft", null)])).toBe("The coordinator refused it.");
    expect(minecraftNotice([n("relaunch", "no"), n("minecraft_reset", null, "applied")])).toBeNull();
  });
});

describe("what there is to show, and where", () => {
  it("says it is connecting before the first snapshot, and that the fleet has no servers when it has none", () => {
    const s0 = reduce(reduce(initialState(), { type: "socket", status: "connecting", at: EPOCH }), { type: "family", family: "v6", probeMs: 1, via: "race" });
    expect(minecraftView(input(s0)).empty).toBe("Connecting to the fleet.");
    const v = viewOf(null);
    expect(v.empty).toBe("This fleet runs no Minecraft servers.");
    // none listed before a GPU host is up says nothing about the fleet (core/view.ts noneListedText)
    const waking = stateWith(null);
    const st = waking.accepted!;
    const noHost = reduce(waking, {
      type: "message",
      text: snapshotText(2, { ...st, fleet: { ...st.fleet, hosts: [], power: { ...st.fleet.power, state: "waking", woke_at: EPOCH - 1000 } } }),
      at: EPOCH,
    });
    expect(minecraftView(input(noHost)).empty).toBe("The fleet is waking. The Minecraft servers start when a GPU host is up.");
    expect([v.header, v.cards]).toEqual([null, []]);
  });

  it.each([1, 2, 4, 5, 8])("%s servers: square tiles of one size inside the stage, apart, in the snapshot's rows", (k) => {
    const v = viewOf(minecraftOf(k));
    const boxes = v.cards.map((c) => c.box!);
    for (const b of boxes) {
      expect(b.w).toBe(b.h);
      expect(b.w).toBe(boxes[0]!.w);
      expect(b.x).toBeGreaterThanOrEqual(STAGE.x);
      expect(b.x + b.w).toBeLessThanOrEqual(STAGE.x + STAGE.w + 1e-6);
    }
    for (const c of v.cards) expect(c.caption!.y + c.caption!.h).toBeLessThanOrEqual(STAGE.y + STAGE.h + 1e-6);
    expect(new Set(boxes.map((b) => b.y)).size).toBe(Math.ceil(k / 4));
  });

  it("on a phone with the panel open the held server sits above it, with a compact caption and no buttons", () => {
    const m = minecraftOf(4);
    m.tiles[0]!.borrowed = { session: "minecraft-claim-1", viewer: "v-1", since: NOW, until: NOW + 100_000 };
    const v = viewOf(m, { stage: { x: 8, y: 150, w: 374, h: 640 }, narrow: true, touch: true });
    const shown = v.cards.filter((c) => c.box !== null);
    expect(shown.map((c) => c.slot)).toEqual(["mc-1"]);
    expect(shown.every((c) => c.compact && !c.showActions)).toBe(true);
    expect(Math.max(...shown.map((c) => c.caption!.y + c.caption!.h))).toBeLessThanOrEqual(150 + 320 + 1e-6);
  });
});

describe("a tile whose view is not coming in", () => {
  const closed = (failures: number, since: number | null) => ({ open: false, viewer: null, tps: null, hasMap: false, failures, connectingSince: since });

  it("says so on the tile, never just dark: connecting, then cannot reach with a Retry, then loading", () => {
    const at = (live: McTileLive) => viewOf(minecraftOf(2), { live: { ...LIVE(2), "mc:1": live } as MinecraftInput["live"] }).cards[0]!;
    expect([at(closed(0, NOW - 100)).stream, at(closed(0, NOW - 100)).streamFailed]).toEqual(["Connecting to mc-1.", false]);
    // a connect that failed, or one still pending past the deadline, is a failure the tile names
    expect([at(closed(1, NOW - 100)).stream, at(closed(1, NOW - 100)).streamFailed]).toEqual(["Cannot reach mc-1's view. Trying again.", true]);
    expect(at(closed(0, NOW - MC_CONNECT_DEADLINE_MS)).streamFailed).toBe(true);
    expect(at({ open: true, viewer: "v-1", tps: 20, hasMap: false, failures: 0, connectingSince: null }).stream).toBe("Loading mc-1's world.");
    expect(at({ open: true, viewer: "v-1", tps: 20, hasMap: true, failures: 0, connectingSince: null }).stream).toBeNull();
    // no socket opened for the tile yet: still a line, not a black square
    expect(viewOf(minecraftOf(2), { live: {} }).cards[0]!.stream).toBe("Connecting to mc-1.");
  });

  it("with no socket to open, says why and offers no Retry: there is nothing to retry", () => {
    // no address in the family the page settled on
    const m = minecraftOf(2);
    m.tiles[0]!.frame_url = null;
    const c = viewOf(m, { live: {} }).cards[0]!;
    expect([c.socket, c.stream, c.streamFailed]).toEqual([null, "mc-1 has no IPv6 address yet.", false]);
    // no direct seat: the banner says so, and the tile does not blame the network
    const locked = minecraftView(input(stateWith(minecraftOf(2), "v-1", { type: "admission", seat: null, locked: true, reason: "Direct streams are full.", direct: null, broadcast: null, seat_free: false }), { live: {} })).cards[0]!;
    expect(locked.socket).toBeNull();
    expect(locked.streamFailed).toBe(false);
    expect(locked.stream).not.toMatch(/address/);
  });

  it("cannot be taken over while this page is not connected to it: the turn would run with nothing to drive", () => {
    const c = viewOf(minecraftOf(2), { live: { ...LIVE(2), "mc:2": closed(3, NOW - 9000) } as MinecraftInput["live"] }).cards;
    expect(c[0]!.take.disabled).toBe(false);
    expect(c[1]!.take.disabled).toBe(true);
    expect(c[1]!.take.title).toMatch(/not connected to mc-2/);
  });
});

describe("the held server", () => {
  const holding = () => with1(minecraftOf(4), 1, { borrowed: { session: "minecraft-claim-7", viewer: "v-1", since: NOW - 1000, until: NOW + 119_000 } });

  it("is the one large view beside the panel; the others leave the stage until it is given back", () => {
    const before = viewOf(minecraftOf(4)).cards;
    const v = viewOf(holding());
    const held = v.cards.find((c) => c.slot === "mc-2")!;
    expect(v.panel?.slot).toBe("mc-2");
    expect(held.box!.w).toBeGreaterThan(1.5 * before[1]!.box!.w);
    expect(held.box!.w).toBe(held.box!.h);
    expect(v.cards.filter((c) => c.slot !== "mc-2").every((c) => c.box === null)).toBe(true);
    // it fits the room left of the panel, with its caption under it
    const room = STAGE.w - 360;
    expect(held.box!.x + held.box!.w).toBeLessThanOrEqual(STAGE.x + room);
    expect(held.caption!.y).toBe(held.box!.y + held.box!.h);
  });

  it("closes its panel the moment this page gives it back, before the coordinator says so", () => {
    expect(viewOf(holding()).panel).not.toBeNull();
    const v = viewOf(holding(), { released: "minecraft-claim-7" });
    expect(v.panel).toBeNull();
    expect(v.cards.every((c) => c.box !== null)).toBe(true);
  });

  it("says when its view does not connect, and offers Retry", () => {
    const live = { ...LIVE(4), "mc:2": { open: false, viewer: null, tps: null, hasMap: false, failures: 2, connectingSince: NOW - 4000 } } as MinecraftInput["live"];
    const p = viewOf(holding(), { live }).panel!;
    expect(p.driving).toBe(false);
    expect(p.failed).toBe(true);
    expect(p.waiting).toBe("Lost the connection to mc-2. Trying again; Retry tries now.");
  });
});

describe("the 3D view's words", () => {
  const map = (extra: Record<string, unknown> = {}) => JSON.stringify({ type: "map", seq: 5, region: { x0: 0, z0: 0, w: 2, h: 2 }, cells: Buffer.from([2, 130, 1, 126, 10, 140, 2, 131]).toString("base64"), ...extra });

  it("takes the ground and the clock when a tile sends them, and does without when it does not", () => {
    const plain = parseViewMsg(map())!;
    expect(plain).toMatchObject({ type: "map", ground: null });
    expect(worldFromMap(plain as Extract<typeof plain, { type: "map" }>).ground).toBeNull();
    const m = parseViewMsg(map({ ground: Buffer.from([2, 130, 4, 121, 3, 133, 2, 131]).toString("base64") }))!;
    if (m.type !== "map") throw new Error("not a map");
    const w = worldFromMap(m);
    expect([...w.ground!]).toEqual([2, 130, 4, 121, 3, 133, 2, 131]);
    expect(() => parseViewMsg(map({ ground: Buffer.from([2, 130]).toString("base64") }))).toThrow(ProtocolError);
    // a delta moves the ground with the columns
    const d = parseViewMsg(JSON.stringify({ type: "delta", seq: 6, cells: [], ground: [[1, 5, 119], [9, 1, 1]] }))!;
    if (d.type !== "delta") throw new Error("not a delta");
    expect(applyDelta(w, d)).toBe(1);
    expect([...w.ground!.slice(2, 4)]).toEqual([5, 119]);
    expect(() => parseViewMsg(JSON.stringify({ type: "delta", seq: 7, cells: [], ground: [[1, 5]] }))).toThrow(ProtocolError);
    // a world with no ground ignores a delta's
    const bare = worldFromMap(plain as Extract<typeof plain, { type: "map" }>);
    expect(applyDelta(bare, { type: "delta", seq: 6, cells: [], ground: [[1, 5, 119]] })).toBe(0);
    // the columns that changed, in either layer, once each; a triple that changes nothing is not one
    const b64 = (bytes: number[]) => Buffer.from(bytes).toString("base64");
    const mapOf = (w: number, h: number, bytes: number[], ground = false) => {
      const m = parseViewMsg(JSON.stringify({ type: "map", seq: 1, region: { x0: 0, z0: 0, w, h }, cells: b64(bytes), ...(ground ? { ground: b64(bytes) } : {}) }))!;
      if (m.type !== "map") throw new Error("not a map");
      return worldFromMap(m);
    };
    const wb = mapOf(2, 2, [2, 130, 2, 130, 2, 130, 2, 130], true);
    const cols = new Set<number>();
    expect(applyDelta(wb, { type: "delta", seq: 2, cells: [[0, 46, 131], [1, 2, 130]], ground: [[0, 46, 131], [3, 5, 120]] }, cols)).toBe(3);
    expect([...cols].sort()).toEqual([0, 3]);
    // a delta of the ground alone names its column too
    cols.clear();
    expect(applyDelta(wb, { type: "delta", seq: 3, cells: [], ground: [[2, 5, 121]] }, cols)).toBe(1);
    expect([...cols]).toEqual([2]);
    // column indices mean the same columns only on the same grid
    const again = mapOf(2, 2, [1, 126, 1, 126, 1, 126, 1, 126]);
    const small = mapOf(1, 1, [2, 130]);
    expect([sameColumns(wb, again), sameColumns(wb, small), sameColumns(null, small)]).toEqual([true, false, false]);
    // a person who joined is a player, not one of the tile's bots
    expect(botCount([{ bot: true }, { bot: true }, { bot: true }, { bot: false }])).toBe(3);
    expect(bare.ground).toBeNull();
    const status = (extra: Record<string, unknown>) => parseViewMsg(JSON.stringify({ type: "status", t: 1, tps: 20, players: [], drive: null, ...extra }))!;
    expect(status({ day_time: 13000 })).toMatchObject({ day_time: 13000 });
    expect(status({})).toMatchObject({ day_time: null });
  });

  it("takes only a grid it can build: each side within 16 bits' reach and the whole within the cap, and a delta's indices inside it", () => {
    const grid = (w: number, h: number) => JSON.stringify({ type: "map", seq: 1, region: { x0: 0, z0: 0, w, h }, cells: Buffer.alloc(Math.min(w * h * 2, 16)).toString("base64") });
    // The cap is the tile server's own largest region: whatever side a server is started with,
    // its map is one this page draws.
    const serverSide = /region_side: c\.region\.clamp\((\d+), (\d+)\)/.exec(readFileSync(new URL("../../../minecraft/src/main.rs", import.meta.url), "utf8"))!;
    const most = Number(serverSide[2]);
    expect(most).toBe(512);
    expect([MC_MAX_SIDE, MC_MAX_COLUMNS]).toEqual([1024, most * most]);
    expect(() => parseViewMsg(grid(MC_MAX_SIDE + 1, 1))).toThrow(/not a grid/);
    expect(() => parseViewMsg(grid(1, MC_MAX_SIDE + 1))).toThrow(/not a grid/);
    // a whole map at the cap parses (the server's largest, and the same columns the long way),
    // one column more does not, and a small one does
    const whole = (w: number, h: number) => JSON.stringify({ type: "map", seq: 1, region: { x0: 0, z0: 0, w, h }, cells: Buffer.alloc(w * h * 2).toString("base64") });
    expect(parseViewMsg(whole(most, most))).toMatchObject({ type: "map", region: { w: most, h: most } });
    expect(parseViewMsg(whole(1024, 256))).toMatchObject({ type: "map", region: { w: 1024, h: 256 } });
    expect(() => parseViewMsg(grid(most + 1, most))).toThrow(/not a grid/);
    expect(() => parseViewMsg(grid(1024, 257))).toThrow(/not a grid/);
    expect(parseViewMsg(whole(256, 256))).toMatchObject({ type: "map", region: { w: 256, h: 256 } });
    expect(parseViewMsg(whole(2, 4))).toMatchObject({ type: "map" });
    const delta = (cells: unknown) => JSON.stringify({ type: "delta", seq: 2, cells });
    expect(() => parseViewMsg(delta([[MC_MAX_COLUMNS, 1, 1]]))).toThrow(ProtocolError);
    expect(parseViewMsg(delta([[MC_MAX_COLUMNS - 1, 1, 1]]))).toMatchObject({ type: "delta" });
  });

  it("compares a copy with its source only when both are the same grid of the same materials", () => {
    const w = (x0: number, side = 2): McWorld => ({ region: { x0, z0: 0, w: side, h: side }, cells: new Uint8Array(side * side * 2), seq: 1 });
    const mats = remapOf([...MATERIALS]);
    expect(sameGrid(w(0), mats, w(0), remapOf([...MATERIALS]))).toBe(true);
    expect(sameGrid(w(0), mats, w(8), mats)).toBe(false);
    expect(sameGrid(w(0), mats, w(0, 4), mats)).toBe(false);
    expect(sameGrid(w(0), mats, w(0), remapOf(["unknown", "grass", "water"]))).toBe(false);
    expect(sameGrid(w(0), mats, w(0), null)).toBe(false);
    expect(sameGrid(w(0), null, w(0), null)).toBe(true);
  });

  it("says on the tile that it is a flat map when the browser has no WebGL2, and nothing when it is 3D", () => {
    const flat = viewOf(minecraftOf(2), { renderer: "flat" });
    expect(flat.renderer).toBe("flat");
    expect(flat.cards[0]!.note).toBe(MC_FLAT_NOTE);
    expect(MC_FLAT_NOTE).toBe("Flat map: no WebGL2 in this browser.");
    expect(flat.cards[0]!.ariaLabel).toBe("mc-1, running, flat map");
    const gl = viewOf(minecraftOf(2), { renderer: "3d" });
    expect(gl.renderer).toBe("3d");
    expect(gl.cards[0]!.note).toBeNull();
    expect(gl.cards[0]!.ariaLabel).toBe("mc-1, running");
    // unsaid, the page has not looked yet: nothing claims a flat map
    expect(viewOf(minecraftOf(2)).cards[0]!.note).toBeNull();
    // a 3D view that was lost says so, not that the browser has none
    const lost = viewOf(minecraftOf(2), { renderer: "flat", lost: true }).cards[0]!;
    expect(lost.note).toBe("3D view lost; showing the flat map.");
    expect(lost.ariaLabel).toBe("mc-1, running, flat map");
    // a view the page draws with less detail to keep up says so
    const reduced = viewOf(minecraftOf(2), { renderer: "3d", reduced: true }).cards[0]!;
    expect(reduced.note).toBe("Reduced detail");
    expect(reduced.ariaLabel).toBe("mc-1, running, reduced detail");
    // what is drawn from data and what is filled in, said once, with the 3D view only
    expect(gl.header!.drawn).toBe(MC_DRAWN_NOTE);
    expect(MC_DRAWN_NOTE).toBe("Drawn from each column's top block; trunks are filled in, and so are the forest floor and lake beds of a server that does not send them.");
    expect(flat.header!.drawn).toBeNull();
  });

  it("names a copy's source, counts the columns that differ from its source's world now, and flies the copy while it is taken", () => {
    const forked = with1(minecraftOf(4), 1, { forked_from: "mc-1" });
    const live = { ...LIVE(4), "mc:2": { ...LIVE(4)["mc:2"]!, diff: 4, bots: 3 } };
    const c = viewOf(forked, { live }).cards[1]!;
    expect(c.forkOf).toBe("mc:1");
    expect(c.diff).toBe("4 columns differ");
    expect(c.flight).toBeNull();
    expect(c.ariaLabel).toBe("mc-2, running, 3 bots, copy of mc-1, 4 columns differ");
    expect(viewOf(forked, { live: { ...live, "mc:2": { ...live["mc:2"]!, diff: 1 } } }).cards[1]!.diff).toBe("1 column differs");
    expect(viewOf(forked, { live: { ...live, "mc:2": { ...live["mc:2"]!, diff: 0 } } }).cards[1]!.diff).toBe("same so far");
    // no count yet (the page holds only one of the two worlds), and none for a server that is no copy
    expect(viewOf(forked).cards[1]!.diff).toBeNull();
    expect(viewOf(minecraftOf(4), { live }).cards[1]!.diff).toBeNull();
    expect(viewOf(minecraftOf(4), { live }).cards[1]!.forkOf).toBeNull();
    // while the copy is taken, its picture flies from the source
    const forking = with1(forked, 1, { phase: "forking", last_relaunch: { clicked_at: NOW - 1200, first_frame_at: null, phases: {} } });
    const taking = viewOf(forking, { live, renderer: "3d" }).cards[1]!;
    expect(taking.flight).toEqual({ from: "mc:1", elapsedMs: 1200 });
    expect(taking.diff).toBeNull();
    // the flight is the tile's picture only when it can be drawn: in 3D, with motion, the source on the stage with its world in
    expect(viewOf(forking, { live }).cards[1]!.flight).toBeNull();
    expect(viewOf(forking, { live, renderer: "flat" }).cards[1]!.flight).toBeNull();
    expect(viewOf(forking, { live, renderer: "3d", reducedMotion: true }).cards[1]!.flight).toBeNull();
    expect(viewOf(forking, { live: { ...live, "mc:1": { ...LIVE(4)["mc:1"]!, hasMap: false } }, renderer: "3d" }).cards[1]!.flight).toBeNull();
    expect(viewOf(forking, { live, renderer: "3d", opened: "mc:2" }).cards[1]!.flight).toBeNull();
    // and without it the tile keeps its own timer
    expect(viewOf(forking, { live, renderer: "flat" }).cards[1]!.timer).toBe("Forking 1.2 s");
    // a fork that failed flies nothing: the tile says why
    const failed = viewOf(with1(forking, 1, { phase: "failed", error: "the copy's restore failed" }), { live, renderer: "3d" }).cards[1]!;
    expect(failed.flight).toBeNull();
    expect(failed.error).toContain("the copy's restore failed");
    // a source relaunched since the fork is another world: nothing to compare with
    const relaunched = with1(with1(forked, 1, { last_relaunch: { clicked_at: EPOCH + 1000, first_frame_at: EPOCH + 2000, phases: {} } }), 0, { last_relaunch: { clicked_at: EPOCH + 5000, first_frame_at: EPOCH + 6000, phases: {} } });
    expect(viewOf(relaunched, { live }).cards[1]!.forkOf).toBeNull();
    expect(viewOf(relaunched, { live }).cards[1]!.diff).toBeNull();
    // a source that has left the fleet is no source
    expect(viewOf(with1(minecraftOf(4), 1, { forked_from: "mc-9", phase: "forking" }), { renderer: "3d" }).cards[1]!.flight).toBeNull();
  });

  it("opens one server to the whole stage and puts the rest away, unless this viewer holds one", () => {
    const v = viewOf(minecraftOf(4), { opened: "mc:3" });
    expect(v.opened).toBe("mc-3");
    expect(v.cards.map((c) => c.box !== null)).toEqual([false, false, true, false]);
    const c = v.cards[2]!;
    expect(c.box!.w).toBe(STAGE.h - 104);
    expect(c.box!.w).toBe(c.box!.h);
    expect(c.opened).toBe(true);
    // the opened 3D view says what is drawn and what is filled in; the flat map draws only data
    expect(c.about).toBeNull();
    expect(viewOf(minecraftOf(4), { opened: "mc:3", renderer: "3d" }).cards.map((x) => x.about)).toEqual([null, null, MC_DRAWN_NOTE, null]);
    expect(viewOf(minecraftOf(4), { opened: "mc:3", renderer: "flat" }).cards[2]!.about).toBeNull();
    // every other server keeps its socket: the worlds are there when the wall comes back
    expect(v.cards.every((x) => x.socket !== null)).toBe(true);
    const wall = viewOf(minecraftOf(4));
    expect(wall.opened).toBeNull();
    expect(wall.cards.every((x) => x.box !== null && !x.opened)).toBe(true);
    expect(viewOf(minecraftOf(4), { opened: "mc:9" }).opened).toBeNull();
    const held = with1(minecraftOf(4), 0, { borrowed: { session: "minecraft-claim-1", viewer: "v-1", since: NOW, until: NOW + 60_000 } });
    const h = viewOf(held, { opened: "mc:3" });
    expect(h.opened).toBeNull();
    expect(h.cards.map((x) => x.box !== null)).toEqual([true, false, false, false]);
  });
});

describe("a fleet with a field", () => {
  const N = 512;
  const row = (phase: number, tps = 255, players = 255, age = 65535, launch = 65535) => [phase, tps, players, 0, age & 255, age >> 8, launch & 255, launch >> 8];
  // as the snapshot carries it: each host's view URLs as what they share and what they do not
  const field = {
    servers: N,
    hosts: [{ key: "aaaaaaaaaaaa", servers: Array.from({ length: N }, (_, i) => i + 1), frames: { prefix: "wss://mc-", suffix: "-aaaaaaaa.example.test/view", names: Array.from({ length: N }, (_, i) => String(i + 1)) } }],
  } as unknown as Minecraft["field"];
  const statuses = (rows: number[][] = Array.from({ length: N }, () => row(3, 200, 3, 100, 410))) => ({
    aaaaaaaaaaaa: { type: "minecraft_status" as const, host: "aaaaaaaaaaaa", received_at: NOW, t: NOW, rows: Buffer.from(rows.flat()).toString("base64"), cpus: 192, busy: 100, mem_total_mib: 385_024, mem_used_mib: 204_800, at: NOW },
  });
  const big = (over: Partial<Minecraft> = {}) => minecraftOf(16, { field, ...over });

  it("shows every server as a cell beside six 3D tiles, each with its own view socket and no other", () => {
    const v = viewOf(big(), { statuses: statuses(), renderer: "3d" });
    const f = v.field!;
    expect(f.cells).toHaveLength(N);
    expect(f.shown).toHaveLength(6);
    expect(new Set(f.shown).size).toBe(6);
    // the field takes its share of the stage and the tiles the rest, and they do not overlap
    expect(f.box).toEqual({ x: STAGE.x, y: STAGE.y, w: STAGE.w * FIELD_SHARE, h: STAGE.h });
    // its cells have the room between its words and the rows under them (what one cell is, and what the marks mean), and all 512 fit there
    expect(f.grid).toEqual({ x: STAGE.x, y: STAGE.y + 92, w: STAGE.w * FIELD_SHARE, h: STAGE.h - 92 - 30 - 34 });
    expect(f.layout!.scrolls).toBe(false);
    expect(f.layout!.cell).toBe(16);
    // on a phone the cells keep a size a finger can pick, and scroll
    const phone = viewOf(big(), { statuses: statuses(), renderer: "3d", narrow: true, stage: { x: 0, y: 200, w: 390, h: 520 } }).field!;
    expect(phone.box).toEqual({ x: 0, y: 200, w: 390, h: 198 });
    expect([phone.layout!.cell, phone.layout!.scrolls]).toEqual([9, true]);
    const placed = v.cards.filter((c) => c.box !== null);
    expect(placed.map((c) => c.slot)).toEqual(f.shown.map((n) => `mc-${n}`));
    for (const c of placed) {
      expect(c.box!.x).toBeGreaterThanOrEqual(f.box!.x + f.box!.w);
      expect(c.box!.w).toBe(c.box!.h);
      expect(c.caption!.y + c.caption!.h).toBeLessThanOrEqual(STAGE.y + STAGE.h + 0.5);
    }
    // six sockets, each to its server's own view URL from the field
    const sockets = v.cards.filter((c) => c.socket !== null);
    expect(sockets).toHaveLength(6);
    for (const c of sockets) expect(c.socket!.url).toBe(`wss://${c.slot}-aaaaaaaa.example.test/view`);
    // a server that is not one of the listed sixteen has the buttons every tile has
    const far = placed.find((c) => Number(c.slot.slice(3)) > 16)!;
    expect(far.dark).toBe(false);
    expect([far.reset.disabled, far.fork.disabled]).toEqual([false, false]);
    expect(far.fork.title).toBe("Copies its world into the next server on its host.");
    expect(far.upIn).toBe("Up in 0.41 s");
    // the headline and the header's figures are every server's, from the rows
    expect(f.headline.servers).toBe("512 servers on one host");
    expect(f.headline.perCpu).toBe("5.1 per busy CPU (100.0 of 192 busy)");
    expect(v.header!.up).toBe("512 of 512 servers up");
    expect(v.header!.median).toBe("Median restore to first chunk 0.41 s");
    expect(f.ariaLabel).toBe("512 servers on one host: 512 running");
    // a fleet listed whole has no field
    expect(viewOf(minecraftOf(8)).field).toBeNull();
  });

  it("draws nothing for servers whose host has not reported: no rows is not zero servers up", () => {
    const v = viewOf(big(), { renderer: "3d" });
    expect(v.field!.cells.every((c) => c.phase === "none")).toBe(true);
    // no count at all: "0 of 512 servers up" would be a reading nobody took
    expect(v.field!.headline.states).toBe("not reported yet");
    expect(v.header!.up).toBe("512 servers, not reported yet");
    expect(v.field!.ariaLabel).toBe("512 servers on one host: not reported yet");
    expect(v.header!.median).toBe("Median restore to first chunk --");
    // the tiles are the first servers, which the snapshot lists whole
    const shown = v.cards.filter((c) => c.box !== null);
    expect(shown.map((c) => c.slot)).toEqual(["mc-1", "mc-2", "mc-3", "mc-4", "mc-5", "mc-6"]);
    // a server the snapshot does not list, picked while its host is silent: dark, and it says
    // "Not reported", not "Restoring" (nobody said it is being restored)
    const picked = viewOf(big(), { renderer: "3d", pins: [{ n: 300, at: NOW - 1000 }] }).cards.find((c) => c.slot === "mc-300")!;
    expect([picked.box !== null, picked.dark, picked.timer, picked.ariaLabel]).toEqual([true, true, "Not reported", "mc-300, not reported"]);
    // rows that stopped coming: the same, with how long ago
    const gone = minecraftView(input(stateWith(big()), { statuses: statuses(), renderer: "3d", now: NOW + 9000 }));
    expect(gone.header!.up).toBe("512 servers, no report for 9 s");
    // some servers reported: a count of those, as before
    const some = viewOf(big(), { statuses: statuses(Array.from({ length: N }, (_, i) => (i < 500 ? row(3, 200, 3, 100, 410) : row(0)))), renderer: "3d" });
    expect([some.header!.up, some.field!.headline.states]).toEqual(["500 of 512 servers up", "500 running · 12 not reported"]);
  });

  it("puts a server the viewer picked into one tile, marks it, and moves no other", () => {
    const st = statuses();
    const before = viewOf(big(), { statuses: st, renderer: "3d" }).field!;
    const v = viewOf(big(), { statuses: st, renderer: "3d", sample: before.slots, pins: [{ n: 300, at: NOW - 1000 }] });
    // the tile whose turn was nearest its end takes it; the other five keep their servers
    expect(v.field!.shown.map((n, j) => (n === before.shown[j] ? "same" : n))).toEqual(["same", "same", "same", "same", "same", 300]);
    expect(v.field!.pinned).toEqual([300]);
    const card = v.cards.find((c) => c.slot === "mc-300")!;
    expect([card.box !== null, card.pinned]).toEqual([true, true]);
    expect(v.cards.filter((c) => c.pinned)).toHaveLength(1);
    // with reduced motion the tiles do not rotate: the same six whenever
    const at = (now: number, sample: typeof before.slots) =>
      minecraftView(input(stateWith(big()), { statuses: Object.fromEntries(Object.entries(st).map(([k, x]) => [k, { ...x, at: now }])), renderer: "3d", reducedMotion: true, now, sample })).field!;
    const a = at(NOW, []);
    expect(at(NOW + 60_000, a.slots).shown).toEqual(a.shown);
    // with motion they do, one tile at a time, from the state the page hands back
    let f = before;
    const seen = new Set(f.shown);
    for (let t = NOW; t <= NOW + 30_000; t += 500) {
      const next = minecraftView(input(stateWith(big()), { statuses: Object.fromEntries(Object.entries(st).map(([k, x]) => [k, { ...x, at: t }])), renderer: "3d", now: t, sample: f.slots })).field!;
      expect(next.shown.filter((n, j) => n !== f.shown[j]).length).toBeLessThanOrEqual(1);
      f = next;
      for (const n of f.shown) seen.add(n);
    }
    expect(seen.size).toBeGreaterThan(12);
  });

  it("keeps an opened server open until the viewer closes it, whatever its tile's turn", () => {
    const st = statuses();
    let f = viewOf(big(), { statuses: st, renderer: "3d" });
    // the tile whose turn ends first
    const opened = f.cards.filter((c) => c.box !== null)[5]!;
    for (let t = NOW; t <= NOW + 40_000; t += 500) {
      f = minecraftView(input(stateWith(big()), { statuses: Object.fromEntries(Object.entries(st).map(([k, x]) => [k, { ...x, at: t }])), renderer: "3d", now: t, sample: f.field!.slots, opened: opened.id }));
      expect(f.opened, String(t - NOW)).toBe(opened.slot);
    }
    expect(f.cards.filter((c) => c.box !== null).map((c) => c.slot)).toEqual([opened.slot]);
    // closed: the wall returns with it still on its tile
    const closed = minecraftView(input(stateWith(big()), { statuses: Object.fromEntries(Object.entries(st).map(([k, x]) => [k, { ...x, at: NOW + 40_500 }])), renderer: "3d", now: NOW + 40_500, sample: f.field!.slots }));
    expect(closed.opened).toBeNull();
    expect(closed.field!.shown[5]).toBe(Number(opened.slot.slice(3)));
  });

  it("gives the whole stage to a server this viewer holds or opened, and the field comes back after", () => {
    const st = statuses();
    const held = with1(big(), 2, { borrowed: { session: "minecraft-claim-1", viewer: "v-1", since: NOW, until: NOW + 60_000 } });
    const h = viewOf(held, { statuses: st, renderer: "3d" });
    expect(h.panel).not.toBeNull();
    expect(h.field!.box).toBeNull();
    expect(h.cards.filter((c) => c.box !== null).map((c) => c.slot)).toEqual(["mc-3"]);
    const first = viewOf(big(), { statuses: st, renderer: "3d" });
    const id = first.cards.find((c) => c.box !== null)!.id;
    const o = viewOf(big(), { statuses: st, renderer: "3d", opened: id });
    expect(o.opened).toBe(first.cards.find((c) => c.box !== null)!.slot);
    expect(o.field!.box).toBeNull();
    expect(o.cards.filter((c) => c.box !== null)).toHaveLength(1);
  });

  it("names a server to the coordinator by its own name, never by the page's key for its tile", () => {
    // A field server's tile id is worked out by the page (or made up when it cannot be): an
    // action sent with it could reach another server. Every action sends the server's name.
    const page = readFileSync(new URL("../../app/_wall/Minecraft.tsx", import.meta.url), "utf8");
    const acts = [...page.matchAll(/client\.act\("(minecraft_reset|minecraft_fork|claim_minecraft)", (\{[^}]*\})\)/g)].map((m) => [m[1], m[2]]);
    expect(acts).toEqual([
      ["minecraft_reset", "{ all: true }"],
      ["minecraft_reset", "{ slot: c.slot }"],
      ["minecraft_fork", "{ slot: c.slot }"],
      ["claim_minecraft", "{ slot: c.slot }"],
    ]);
    // and a card's slot is its server's name whatever its id: a field with a made-up key
    const odd = { ...field!, servers: N + 1 };
    const v = viewOf(big({ field: odd }), { statuses: statuses(), renderer: "3d", pins: [{ n: 300, at: NOW - 1000 }] });
    const card = v.cards.find((c) => c.slot === "mc-300")!;
    expect([card.id, card.slot, card.reset.disabled]).toEqual(["field:300", "mc-300", false]);
    expect(new Set(v.cards.map((c) => c.id)).size).toBe(v.cards.length);
  });

  it("counts a reset of every server as its servers come back", () => {
    const rows = Array.from({ length: N }, (_, i) => (i < 300 ? row(3, 200, 3, 2, 400) : i < 400 ? row(1) : row(3, 200, 3, 500, 400)));
    const m = big({ last_reset_all: { clicked_at: NOW - 10_000, by: "viewer:v-1", servers: N } });
    const w = viewOf(m, { statuses: statuses(rows), renderer: "3d" }).field!.wave!;
    expect(w.text).toBe("Reset all: 300 of 512 back · 30.0 a second · 10.0 s");
    expect(viewOf(big(), { statuses: statuses(rows), renderer: "3d" }).field!.wave).toBeNull();
  });
});
