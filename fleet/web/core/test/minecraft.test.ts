import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import {
  applyDelta,
  BOT_DYES,
  decodeBase64,
  DRIVE_HEARTBEAT_MS,
  DRIVE_MIN_GAP_MS,
  driveInputOf,
  Driver,
  FIRST_CHANGE_GIVE_UP_MS,
  forkTarget,
  markersOf,
  MATERIAL_COLORS,
  MATERIALS,
  MC_UNKNOWN,
  minecraftNotice,
  minecraftView,
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
  Object.fromEntries(Array.from({ length: n }, (_, i) => [`mc:${i + 1}`, { open: true, viewer, tps, hasMap: true }]));
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
    expect(st).toEqual({ type: "status", t: 5, tps: 19.98, players: [{ name: "fcvm_bot1", x: 1.5, y: 70, z: -2, yaw: 90, bot: true, driven: true }], drive: { seq: 12, moved_at: 6 } });
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
    d.press("place", 100);
    expect(d.observeDelta(160)).toBe(60);
    d.press("dig", 1000);
    expect(d.observeDelta(1000 + FIRST_CHANGE_GIVE_UP_MS + 1)).toBeNull();
    d.press("forward", 9000);
    d.observeStatus([{ name: "b", x: 0, y: 70, z: 0, yaw: 0, bot: true, driven: true }], 9000 + FIRST_CHANGE_GIVE_UP_MS + 10);
    expect(d.samples).toEqual([60]);
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
    expect(h.lastFork).toBe("Fork: not used yet");
    expect(h.lastResetAll).toBe("Reset all: not used yet");
    const one = viewOf(with1(minecraftOf(1), 0, { phase: "pending", last_relaunch: null })).header!;
    expect([one.up, one.median, one.slowest]).toEqual(["0 of 1 server up", "Median restore to first chunk --", "Slowest --"]);
  });

  it("gives the last fork with its snapshot and restore, and the last reset all by whom", () => {
    const at = Date.UTC(2026, 10, 8, 9, 5, 0);
    const m = minecraftOf(4, {
      last_fork: { from: "mc-1", to: "mc-2", clicked_at: at, by: "viewer:v-5", first_chunk_at: at + 2300, snapshot_ms: 640, restore_ms: 1100, total_ms: 2300 },
      last_reset_all: { clicked_at: at, by: "viewer:v-1", servers: 4 },
    });
    const h = viewOf(m).header!;
    expect(h.lastFork).toBe("Last fork: mc-1 to mc-2 in 2.30 s (snapshot 0.64 s, restore 1.10 s)");
    expect(h.lastResetAll).toBe("Last reset all: 09:05:00 UTC, 4 servers, by you");
    expect(viewOf(m, {}, "v-9").header!.lastResetAll).toBe("Last reset all: 09:05:00 UTC, 4 servers, by another viewer");
  });

  it("tells Java Edition players an address and the port when tiles have one, and nothing when none does", () => {
    expect(viewOf(minecraftOf(2)).header!.join).toBe("Join from Minecraft: Java Edition 26.3 at mc-1.example.test, port 25565. Every tile has its own address, under it.");
    const none = minecraftOf(2);
    for (const t of none.tiles) t.join = null;
    expect(viewOf(none).header!.join).toBeNull();
    expect(viewOf(none).cards.every((c) => c.join === null)).toBe(true);
    expect(viewOf(minecraftOf(1, { server: "Some server" })).header!.join).toBe("Join from Minecraft: Java Edition at mc-1.example.test, port 25565. Every tile has its own address, under it.");
  });

  it("Reset all is on while online, off when the socket is closed", () => {
    expect(viewOf(minecraftOf(2)).resetAll).toEqual({ label: "Reset all", disabled: false });
    const s = reduce(stateWith(minecraftOf(2)), { type: "socket", status: "closed", at: EPOCH });
    const v = minecraftView(input(s));
    expect(v.resetAll.disabled).toBe(true);
    expect(v.cards.every((c) => c.reset.disabled && c.fork.disabled && c.take.disabled)).toBe(true);
    expect(v.banner).toMatch(/coordinator is offline/);
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
    expect([connecting.driving, connecting.waiting]).toEqual([false, "Connecting to the server."]);
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

  it("on a phone with the panel open the tiles sit above it, with compact captions and no buttons", () => {
    const m = minecraftOf(4);
    m.tiles[0]!.borrowed = { session: "minecraft-claim-1", viewer: "v-1", since: NOW, until: NOW + 100_000 };
    const v = viewOf(m, { stage: { x: 8, y: 150, w: 374, h: 640 }, narrow: true, touch: true });
    expect(v.cards.every((c) => c.compact && !c.showActions)).toBe(true);
    expect(Math.max(...v.cards.map((c) => c.caption!.y + c.caption!.h))).toBeLessThanOrEqual(150 + 320 + 1e-6);
  });
});
