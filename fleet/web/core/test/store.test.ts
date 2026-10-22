import { describe, expect, it } from "vitest";
import { parseServerMsg, ProtocolError } from "../protocol.ts";
import { initialState, intentsToResend, makeIntent, reduce, cameraTargets, type ClientState } from "../store.ts";
import { EPOCH, fleet, snapshotText } from "./fixtures.ts";

function feed(s: ClientState, text: string, at = EPOCH): ClientState {
  return reduce(s, { type: "message", text, at });
}

describe("protocol parsing", () => {
  it("reads a snapshot", () => {
    const m = parseServerMsg(snapshotText(3, fleet()));
    expect(m.type).toBe("snapshot");
    if (m.type === "snapshot") expect(m.state.tiles).toHaveLength(8);
  });

  it("accepts a GPU given as a plain name", () => {
    const st = fleet() as unknown as { tiles: Record<string, unknown>[] };
    st.tiles[0]!["gpu"] = "NVIDIA T4";
    const m = parseServerMsg(JSON.stringify({ type: "snapshot", version: 1, state: st }));
    if (m.type !== "snapshot") throw new Error("not a snapshot");
    expect(m.state.tiles[0]!.gpu).toEqual({ name: "NVIDIA T4", temp_c: null, power_w: null, util_pct: null });
  });

  it.each([
    ["not JSON", "{"],
    ["unknown type", JSON.stringify({ type: "nope" })],
    ["a rect outside the image", JSON.stringify({ type: "snapshot", version: 1, state: { ...fleet(), tiles: [{ ...fleet().tiles[0], rect: { x: 0.9, y: 0, w: 0.2, h: 1 } }] } })],
    ["a duplicate tile id", JSON.stringify({ type: "snapshot", version: 1, state: { ...fleet(), tiles: [fleet().tiles[0], fleet().tiles[0]] } })],
    ["a half that is neither", JSON.stringify({ type: "snapshot", version: 1, state: { ...fleet(), tiles: [{ ...fleet().tiles[0], half: "gcp" }] } })],
    ["a receipt with no id", JSON.stringify({ type: "receipt", outcome: "applied", version: 2 })],
  ])("refuses %s", (_name, text) => {
    expect(() => parseServerMsg(text)).toThrow(ProtocolError);
  });

  it("counts a refused message and keeps the state", () => {
    let s = feed(initialState(), snapshotText(1, fleet()));
    s = feed(s, "{not json");
    expect(s.protocolErrors).toBe(1);
    expect(s.version).toBe(1);
  });
});

describe("snapshots", () => {
  it("drops a snapshot older than the last accepted, and a repeat of it", () => {
    let s = feed(initialState(), snapshotText(5, fleet()));
    const older = fleet(2, 2, { viewers: { count: 99 } });
    s = feed(s, snapshotText(4, older));
    expect(s.version).toBe(5);
    expect(s.accepted!.viewers.count).toBe(2);
    s = feed(s, snapshotText(5, older));
    expect(s.accepted!.viewers.count).toBe(2);
    s = feed(s, snapshotText(6, older));
    expect(s.accepted!.viewers.count).toBe(99);
  });
});

describe("intents and receipts", () => {
  it("keeps an intent pending until its receipt and its version both arrive", () => {
    let s = feed(initialState(), snapshotText(1, fleet()));
    s = reduce(s, { type: "intent-sent", intent: makeIntent("a", "relaunch_all"), at: EPOCH });
    expect(s.pending).toHaveLength(1);
    s = feed(s, JSON.stringify({ type: "receipt", id: "a", outcome: "applied", version: 2 }));
    expect(s.pending).toHaveLength(1); // receipt seen, snapshot 2 not yet
    s = feed(s, snapshotText(2, fleet()));
    expect(s.pending).toHaveLength(0);
  });

  it("settles at once when the receipt's version is already accepted", () => {
    let s = feed(initialState(), snapshotText(3, fleet()));
    s = reduce(s, { type: "intent-sent", intent: makeIntent("a", "relaunch_all"), at: EPOCH });
    s = feed(s, JSON.stringify({ type: "receipt", id: "a", outcome: "applied", version: 3 }));
    expect(s.pending).toHaveLength(0);
  });

  it("handles a repeated receipt once", () => {
    let s = feed(initialState(), snapshotText(1, fleet()));
    s = reduce(s, { type: "intent-sent", intent: makeIntent("w", "word", { text: "orbit" }), at: EPOCH });
    const r = JSON.stringify({ type: "receipt", id: "w", outcome: "applied", version: 2 });
    s = feed(s, r);
    s = feed(s, r);
    expect(s.notices.filter((n) => n.id === "w")).toHaveLength(1);
  });

  it("does not record a second send of the same id", () => {
    let s = initialState();
    const i = makeIntent("x", "relaunch", { tile: "slot-1" });
    s = reduce(s, { type: "intent-sent", intent: i, at: EPOCH });
    s = reduce(s, { type: "intent-sent", intent: i, at: EPOCH + 5 });
    expect(s.pending).toHaveLength(1);
  });

  it("drops a refused intent and says why", () => {
    let s = feed(initialState(), snapshotText(1, fleet()));
    s = reduce(s, { type: "intent-sent", intent: makeIntent("w", "word", { text: "forbidden" }), at: EPOCH });
    s = feed(s, JSON.stringify({ type: "receipt", id: "w", outcome: "refused", reason: "the text check refused it", version: 1 }));
    expect(s.pending).toHaveLength(0);
    expect(s.notices[0]).toMatchObject({ kind: "refused", reason: "the text check refused it" });
  });

  it("resends only the intents with no receipt after a reconnect", () => {
    let s = feed(initialState(), snapshotText(1, fleet()));
    s = reduce(s, { type: "intent-sent", intent: makeIntent("a", "relaunch_all"), at: EPOCH });
    s = reduce(s, { type: "intent-sent", intent: makeIntent("b", "word", { text: "x" }), at: EPOCH });
    s = feed(s, JSON.stringify({ type: "receipt", id: "a", outcome: "applied", version: 2 }));
    expect(intentsToResend(s).map((i) => i.id)).toEqual(["b"]);
  });
});

describe("input to pixels", () => {
  it("measures from the input to the first draw at the camera key's t", () => {
    let s = feed(initialState(), snapshotText(1, fleet()));
    s = reduce(s, { type: "intent-sent", intent: makeIntent("c", "camera", { drag: { dx: 0.1, dy: 0 } }), at: EPOCH + 100 });
    expect(cameraTargets(s)).toEqual([]);
    const st = fleet();
    st.scene.camera = [{ t: 10, x: 0 }, { t: 10.05, x: 0.1, intent: "c" }];
    s = feed(s, snapshotText(2, st));
    expect(cameraTargets(s)).toEqual([{ id: "c", tEff: 10.05 }]);
    s = reduce(s, { type: "camera-drawn", id: "c", drawnAt: EPOCH + 283 });
    expect(s.lastInputLatencyMs).toBe(183);
    expect(cameraTargets(s)).toEqual([]);
  });
});

describe("the coordinator's documented snapshot (fleet/PROTOCOL.md)", () => {
  // Shapes as the coordinator writes them: hosts as a list, GPU readings apart from
  // the GPU, lights as world arrays, timings as <part>_ms, actions with summaries.
  const doc = {
    type: "snapshot",
    version: 42,
    state: {
      tiles: [
        {
          id: "fcvm:1", half: "fcvm", rect: { x: 0, y: 0, w: 0.25, h: 0.5 }, phase: "running", slot: "slot-1",
          frame_url: "ws://example-host:1/frames", gpu: { sbdf: "0000:01:00.0", name: "Tesla T4" }, host: "example-host",
          vm: "fleet-slot-1", generation: 3, fps: 30.0, gpu_stats: { temp_c: 61, power_w: 70.5, util_pct: 97, mem_used_mb: 900 }, error: null,
          last_relaunch: { clicked_at: 1761134400000, by: "viewer:v-4", first_frame_at: 1761134405200,
            phases: { container_ms: 600, first_frame_ms: 250, gpu_ms: 3100, restore_ms: 4300, teardown_ms: 310, total_ms: 5200 } },
        },
        { id: "ec2:1", half: "ec2", rect: { x: 0.5, y: 0, w: 0.25, h: 0.5 }, phase: "waiting_capacity", slot: null, frame_url: null, gpu: null, host: null, vm: null, generation: null, fps: null, gpu_stats: null, error: null, last_relaunch: null },
      ],
      fleet: { hosts: [{ name: "example-host", arch: "x86_64", gpus: [{ sbdf: "0000:01:00.0", name: "Tesla T4" }], draining: false }], gpus: 8, fcvm_gpus: 8, ec2_instances: 0, n: 8, max_tiles: 100, standby: [] },
      scene: { epoch_unix_ms: 1761134400000, camera: [{ t: 0, pos: [0, 2, 0], look: [0, 1.5, -10], fov_deg: 60 }], time_of_day: 0.3, words: [{ text: "hello", t: 12.5 }], lights: [{ pos: [1, 2, 3], color: [1, 0.5, 0], t: 14.0 }] },
      image_aspect: 3.5555555555555554,
      settings: { auto_relaunch_secs: null },
      relaunch_all: { started_at: 1761134400000, by: "viewer:v-4", fcvm_done_at: null, ec2_done_at: null, fcvm_last: null, ec2_last: null, fcvm_pending: 8, ec2_pending: 0 },
      viewers: { count: 3 },
      actions: [{ at: 1761134400000, by: "viewer:v-4", name: "relaunch_all", summary: "relaunch all (8 tiles)", outcome: "committed" }],
    },
  };

  it("reads it", () => {
    const m = parseServerMsg(JSON.stringify(doc));
    if (m.type !== "snapshot") throw new Error("not a snapshot");
    const st = m.state;
    expect(st.image_aspect).toBeCloseTo(32 / 9);
    expect(st.fleet.hosts).toBe(1);
    expect(st.tiles[0]).toMatchObject({ slot: "slot-1", fps: 30, gpu: { name: "Tesla T4", temp_c: 61, power_w: 70.5, util_pct: 97 } });
    expect(st.tiles[1]).toMatchObject({ phase: "waiting_capacity", slot: null, gpu: null });
    expect(st.scene.lights[0]!.color).toBe("#ffbc00");
    expect(st.scene.camera[0]).toEqual({ t: 0, pos: [0, 2, 0], look: [0, 1.5, -10], fov_deg: 60 });
    expect(st.actions[0]).toMatchObject({ summary: "relaunch all (8 tiles)", outcome: "committed", args: null });
  });

  it("takes a committed receipt as applied and an error message as the coordinator's error", () => {
    let s = feed(initialState(), JSON.stringify(doc));
    s = reduce(s, { type: "intent-sent", intent: makeIntent("a", "relaunch", { tile: "fcvm:1" }), at: EPOCH });
    s = feed(s, JSON.stringify({ type: "receipt", id: "a", outcome: "committed", version: 43 }));
    expect(s.pending[0]!.receipt).not.toBeNull();
    expect(s.notices).toHaveLength(0);
    s = feed(s, JSON.stringify({ type: "error", reason: "message is not JSON: x" }));
    expect(s.serverError).toBe("message is not JSON: x");
    expect(s.protocolErrors).toBe(0);
  });

  it("times a camera move from untagged keys: the first key newer than the page held", () => {
    let s = feed(initialState(), JSON.stringify(doc));
    s = reduce(s, { type: "intent-sent", intent: makeIntent("c", "camera", { drag: { yaw_deg: 5, pitch_deg: 0, dive: 0 } }), at: EPOCH });
    s = feed(s, JSON.stringify({ type: "receipt", id: "c", outcome: "committed", version: 43 }));
    expect(cameraTargets(s)).toEqual([]);
    const next = structuredClone(doc);
    next.version = 43;
    next.state.scene.camera = [...doc.state.scene.camera, { t: 20, pos: [0, 2, 0], look: [1, 1.5, -10], fov_deg: 60 }, { t: 20.3, pos: [0, 2, 0], look: [2, 1.5, -10], fov_deg: 60 }];
    s = feed(s, JSON.stringify(next));
    expect(cameraTargets(s)).toEqual([{ id: "c", tEff: 20 }]);
  });
});
