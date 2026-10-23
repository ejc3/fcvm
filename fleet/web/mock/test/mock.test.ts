import { afterAll, beforeAll, describe, expect, it } from "vitest";
import WebSocket from "ws";
import { innerRect, parseFrame, parseStreamHeader, splitNals, nalType, NAL_SPS, NAL_IDR } from "../../core/frame.ts";
import { MockCoordinator, halfGrid } from "../coordinator.ts";
import { MockRenderers } from "../renderer.ts";
import { FFMPEG } from "../encoder.ts";
import { tileGeometry } from "../../core/layout.ts";
import { spawnSync } from "node:child_process";
import { expectedRgb, frameCtx, renderI420 } from "../pattern.ts";

let coord: MockCoordinator;
let renderers: MockRenderers;
const FPS = 20;

beforeAll(async () => {
  const epochUnixMs = Math.floor(Date.now() / 1000) * 1000;
  renderers = new MockRenderers({ port: 0, publicHost: "localhost", fps: FPS, threads: 2, maxHeight: 360, epochUnixMs, scene: () => coord.patternScene() });
  await renderers.listen();
  coord = new MockCoordinator({
    port: 0, perHalf: 2, ec2Short: 1, gpusPerHost: 1, fcvmRelaunchMs: 400, ec2RelaunchMs: 900,
    raceEverySecs: 0, firstRaceSecs: 0, cooldownSecs: 1, capacityWaitProb: 0, capacityWaitMs: [0, 0],
    borrowCap: 1, claimSecs: 1, holdSecs: 600, travelSecs: 0.5, turnstileSitekey: null,
    refusedWords: ["forbidden"], checkMs: 20, epochUnixMs, renderers,
  });
  await coord.listen();
});

afterAll(async () => {
  await coord.close();
  await renderers.close();
});

async function admin(path: string, body: unknown): Promise<unknown> {
  const r = await fetch(`http://localhost:${coord.port}${path}`, { method: "POST", body: JSON.stringify(body) });
  expect(r.status, path).toBe(200);
  return r.json();
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Wait for a condition on the mock's state, polling. */
async function until(pred: () => boolean, ms = 5000): Promise<void> {
  const end = Date.now() + ms;
  while (!pred()) {
    if (Date.now() > end) throw new Error("condition not met in time");
    await sleep(20);
  }
}

function live(): Promise<{ ws: WebSocket; msgs: Record<string, unknown>[]; next: (pred: (m: Record<string, unknown>) => boolean) => Promise<Record<string, unknown>> }> {
  return new Promise((resolve) => {
    const ws = new WebSocket(`ws://localhost:${coord.port}/live`);
    const msgs: Record<string, unknown>[] = [];
    const waiters: { pred: (m: Record<string, unknown>) => boolean; res: (m: Record<string, unknown>) => void }[] = [];
    ws.on("message", (d) => {
      const m = JSON.parse(d.toString()) as Record<string, unknown>;
      msgs.push(m);
      for (const w of [...waiters]) if (w.pred(m)) { waiters.splice(waiters.indexOf(w), 1); w.res(m); }
    });
    const next = (pred: (m: Record<string, unknown>) => boolean) =>
      new Promise<Record<string, unknown>>((res) => {
        const hit = msgs.find(pred);
        if (hit) res(hit);
        else waiters.push({ pred, res });
      });
    ws.on("open", () => resolve({ ws, msgs, next }));
  });
}

describe("mock coordinator", () => {
  it("lays out a half on a grid close to 16:9 tiles", () => {
    expect(halfGrid(8)).toEqual({ cols: 2, rows: 4 });
    expect(halfGrid(25)).toEqual({ cols: 5, rows: 5 });
  });

  it("applies a repeated intent id once and answers both with the same receipt", async () => {
    const c = await live();
    const intent = JSON.stringify({ type: "intent", id: "dup-1", name: "word", args: { text: "orbit" } });
    c.ws.send(intent);
    c.ws.send(intent);
    await c.next(() => c.msgs.filter((m) => m["type"] === "receipt").length === 2);
    const receipts = c.msgs.filter((m) => m["type"] === "receipt");
    expect(receipts[0]).toEqual(receipts[1]);
    expect(receipts[0]).toMatchObject({ id: "dup-1", outcome: "committed" });
    expect(coord.appliedCount("dup-1")).toBe(1);
    expect(coord.fleetState().scene.words.filter((w) => w.text === "orbit")).toHaveLength(1);
    c.ws.close();
  });

  it("refuses the test word", async () => {
    const c = await live();
    c.ws.send(JSON.stringify({ type: "intent", id: "w-bad", name: "word", args: { text: "forbidden" } }));
    const r = await c.next((m) => m["type"] === "receipt" && m["id"] === "w-bad");
    expect(r).toMatchObject({ outcome: "refused", reason: "the text check refused it" });
    expect(coord.appliedCount("w-bad")).toBe(0);
    c.ws.close();
  });

  it("answers a ping with its clock", async () => {
    const c = await live();
    c.ws.send(JSON.stringify({ type: "ping", client_unix_ms: 123 }));
    const m = await c.next((x) => x["type"] === "clock");
    expect(typeof m["server_unix_ms"]).toBe("number");
    c.ws.close();
  });

  it("relaunches a tile: dark and refusing frames, then running with timings", async () => {
    const c = await live();
    c.ws.send(JSON.stringify({ type: "intent", id: "r-1", name: "relaunch", args: { tile: "fcvm:1" } }));
    await c.next((m) => m["type"] === "receipt" && m["id"] === "r-1");
    const tile = () => coord.fleetState().tiles.find((t) => t.id === "fcvm:1")!;
    expect(tile().phase).not.toBe("running");
    const refused = await new Promise<boolean>((res) => {
      const ws = new WebSocket(`${tile().frame_url}?h=144`);
      ws.on("unexpected-response", () => res(true));
      ws.on("open", () => { ws.close(); res(false); });
    });
    expect(refused).toBe(true);
    await new Promise((r) => setTimeout(r, 900));
    expect(tile().phase).toBe("running");
    const lr = tile().last_relaunch!;
    expect(lr.first_frame_at! - lr.clicked_at).toBeGreaterThan(250);
    expect(Object.keys(lr.phases).sort()).toEqual(["ack_ms", "container_ms", "first_frame_ms", "gpu_ms", "restore_ms", "teardown_ms", "total_ms"]);
    c.ws.close();
  });

  it("races relaunch all and stamps each half", async () => {
    const c = await live();
    c.ws.send(JSON.stringify({ type: "intent", id: "ra-1", name: "relaunch_all", args: {} }));
    await c.next((m) => m["type"] === "receipt" && m["id"] === "ra-1");
    c.ws.send(JSON.stringify({ type: "intent", id: "ra-2", name: "relaunch_all", args: {} }));
    expect(await c.next((m) => m["type"] === "receipt" && m["id"] === "ra-2")).toMatchObject({ outcome: "refused" });
    await new Promise((r) => setTimeout(r, 1300));
    const ra = coord.fleetState().relaunch_all!;
    expect(ra.fcvm_done_at).not.toBeNull();
    expect(ra.ec2_done_at).not.toBeNull();
    expect(ra.fcvm_done_at!).toBeLessThan(ra.ec2_done_at!);
    c.ws.close();
    await sleep(1000); // the cooldown
  });
});

describe("mock renderer", () => {
  it("speaks the frame protocol: a header, then FCVF frames on the shared clock, keyframe first", async () => {
    const url = `${coord.fleetState().tiles.find((t) => t.slot === "slot-2")!.frame_url}?h=90`;
    const ws = new WebSocket(url);
    const texts: string[] = [];
    const frames: ReturnType<typeof parseFrame>[] = [];
    await new Promise<void>((res) => {
      ws.on("message", (d, bin) => {
        if (!bin) texts.push(d.toString());
        else frames.push(parseFrame(new Uint8Array(d as Buffer)));
        if (frames.length === 6) res();
      });
    });
    ws.close();
    const header = parseStreamHeader(texts[0]!);
    expect(header).not.toBeNull();
    // The tile is 160 x 90 (1/4 x 1/2 of a 16:9 image at 90 high), sent with a
    // margin of 8 px on its inner sides and a crop that says so.
    expect(innerRect(header!)).toMatchObject({ sw: 160, sh: 90 });
    expect(header!.crop.left + header!.crop.right + header!.crop.top + header!.crop.bottom).toBeGreaterThan(0);
    const ok = frames.map((f) => { if (!f.ok) throw new Error(f.error); return f.frame; });
    expect(ok[0]!.keyframe).toBe(true);
    const types = splitNals(ok[0]!.data).map(nalType);
    expect(types).toContain(NAL_SPS);
    expect(types).toContain(NAL_IDR);
    for (let i = 1; i < ok.length; i++) {
      expect(ok[i]!.frame).toBeGreaterThan(ok[i - 1]!.frame);
      expect(ok[i]!.t).toBeCloseTo(ok[i]!.frame / FPS, 9);
    }
    expect([ok[0]!.width, ok[0]!.height]).toEqual([header!.width, header!.height]);
  });
});

describe("mock renderer, height change", () => {
  it("sends only frames drawn at the new height after a height change", async () => {
    // Each drawn frame is held 150 ms before it is encoded, so one drawn at the
    // old height is always in flight when the height changes. It must not reach
    // the new encoder: a raw frame of the wrong size shifts every frame after it.
    const scene = coord.patternScene();
    const r = new MockRenderers({ port: 0, publicHost: "localhost", fps: FPS, threads: 1, maxHeight: 360, epochUnixMs: scene.epoch_unix_ms, scene: () => scene, holdMs: 150 });
    await r.listen();
    const rect = { x: 0.25, y: 0, w: 0.25, h: 0.25 };
    r.setTile("t1", rect);
    const ws = new WebSocket(`${r.frameUrl("t1")}?h=90`);
    const after: { t: number; data: Uint8Array }[] = [];
    let headers = 0;
    let last: ReturnType<typeof parseStreamHeader> = null;
    let sentResize = false;
    await new Promise<void>((res) => {
      ws.on("message", (d, bin) => {
        if (!bin) {
          headers++;
          last = parseStreamHeader(d.toString());
          return;
        }
        const p = parseFrame(new Uint8Array(d as Buffer));
        if (!p.ok) throw new Error(p.error);
        if (!sentResize) {
          sentResize = true;
          ws.send(JSON.stringify({ h: 180 }));
          return;
        }
        if (headers < 2 || p.frame.height !== last!.height) return;
        after.push({ t: p.frame.t, data: p.frame.data });
        if (after.length === 6) res();
      });
    });
    ws.close();
    await r.close();
    expect(headers).toBe(2);
    // Decode them and compare each frame with the picture at its t.
    const stream = Buffer.concat(after.map((f) => Buffer.from(f.data)));
    const out = spawnSync(FFMPEG, ["-hide_banner", "-loglevel", "error", "-f", "h264", "-i", "pipe:0", "-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"], { input: stream, maxBuffer: 1 << 26 });
    expect(out.status).toBe(0);
    // The delivered frame holds the 320 x 180 tile inside its margin.
    const hdr = last!;
    const W = hdr.width;
    const H = hdr.height;
    expect(innerRect(hdr)).toMatchObject({ sw: 320, sh: 180 });
    const frames = Math.floor(out.stdout.length / (W * H * 3));
    expect(frames).toBe(6);
    for (let k = 0; k < frames; k++) {
      const f = frameCtx(after[k]!.t, scene);
      let err = 0;
      let n = 0;
      for (let j = 5; j < H; j += 10) {
        for (let i = 5; i < W; i += 10) {
          const ii = i - hdr.crop.left;
          const jj = j - hdr.crop.top;
          const e = expectedRgb(rect.x + ((ii + 0.5) / 320) * rect.w, rect.y + ((jj + 0.5) / 180) * rect.h, f);
          const o = (k * W * H + j * W + i) * 3;
          err += (Math.abs(out.stdout[o]! - e[0]) + Math.abs(out.stdout[o + 1]! - e[1]) + Math.abs(out.stdout[o + 2]! - e[2])) / 3;
          n++;
        }
      }
      expect(err / n, `frame ${k}`).toBeLessThan(8);
    }
  });
});

describe("pattern", () => {
  it("is a pure function of image position: two halves drawn apart equal the whole", () => {
    const scene = { epoch_unix_ms: 0, image_aspect: 16 / 9, camera: [{ t: 0, x: 0.1, y: 0, zoom: 1.3 }], time_of_day: 0.3, words: [{ text: "hi", pos: { x: 0, y: 0 } }], lights: [] };
    const f = frameCtx(12.35, scene);
    const grid = { fullWidth: 128, fullHeight: 64, x0: 32, y0: 16, height: 16 };
    const whole = renderI420({ ...grid, width: 64 }, f);
    const left = renderI420({ ...grid, width: 32 }, f);
    for (let j = 0; j < 16; j++) for (let i = 0; i < 32; i++) expect(left[j * 32 + i]).toBe(whole[j * 64 + i]);
  });

  it("changes with t, so a tile drawn at another t shows a seam", () => {
    const scene = { epoch_unix_ms: 0, image_aspect: 16 / 9, camera: [], time_of_day: 0.3, words: [], lights: [] };
    const g = { width: 16, height: 16, fullWidth: 160, fullHeight: 160, x0: 0, y0: 0 };
    const a = renderI420(g, frameCtx(10, scene));
    const b = renderI420(g, frameCtx(10.5, scene));
    let diff = 0;
    for (let i = 0; i < a.length; i++) diff += Math.abs(a[i]! - b[i]!);
    expect(diff / a.length).toBeGreaterThan(3);
  });
});

describe("mock coordinator, review findings", () => {
  it("never shows two viewers different state under one version", async () => {
    const a = await live();
    await a.next((m) => m["type"] === "snapshot");
    const id = `tod-${Date.now()}`;
    a.ws.send(JSON.stringify({ type: "intent", id, name: "time_of_day", args: { v: 18.25 } }));
    // B connects while that change waits for its flush.
    await new Promise((r) => setTimeout(r, 2));
    const b = await live();
    await a.next((m) => m["type"] === "receipt" && m["id"] === id);
    await new Promise((r) => setTimeout(r, 100));
    const byVersion = (msgs: Record<string, unknown>[]) => {
      const out = new Map<number, string>();
      for (const m of msgs) {
        if (m["type"] !== "snapshot") continue;
        const st = structuredClone(m["state"]) as { viewers: unknown };
        st.viewers = null; // the viewer count is part of the state too; compare the rest
        out.set(m["version"] as number, JSON.stringify(st));
      }
      return out;
    };
    const va = byVersion(a.msgs);
    const vb = byVersion(b.msgs);
    let shared = 0;
    for (const [v, s] of vb) {
      if (!va.has(v)) continue;
      shared++;
      expect(s, `version ${v}`).toBe(va.get(v));
    }
    expect(shared).toBeGreaterThan(0);
    a.ws.close();
    b.ws.close();
  });

  it("answers a JSON null with an error and keeps serving", async () => {
    const c = await live();
    c.ws.send("null");
    c.ws.send(JSON.stringify({ type: "ping", client_unix_ms: 1 }));
    expect(await c.next((m) => m["type"] === "error")).toMatchObject({ type: "error" });
    expect(await c.next((m) => m["type"] === "clock")).toMatchObject({ type: "clock" });
    c.ws.close();
  });
});

describe("mock aggregates", () => {
  it("lists one aggregate per fcvm host and one for EC2, covering the image, each serving the frame protocol", async () => {
    const st = coord.fleetState();
    expect(st.aggregates.map((a) => a.id)).toEqual(["agg:mock-host-1", "agg:mock-host-2", "agg:ec2"]);
    expect(st.aggregates[0]!.tiles).toEqual(["fcvm:1"]);
    const area = st.aggregates.reduce((s, a) => s + a.rect.w * a.rect.h, 0);
    expect(area).toBeCloseTo(1, 9);
    const ec2 = st.aggregates.find((a) => a.id === "agg:ec2")!;
    const ws = new WebSocket(`${ec2.frame_url}?h=90`);
    const msgs: (string | Uint8Array)[] = [];
    await new Promise<void>((res) => {
      ws.on("message", (d, bin) => {
        msgs.push(bin ? new Uint8Array(d as Buffer) : d.toString());
        if (msgs.length === 3) res();
      });
    });
    ws.close();
    const header = parseStreamHeader(msgs[0] as string)!;
    // The EC2 half asked at 90 is under the encoder's minimum width, so it comes taller.
    const g = tileGeometry(ec2.rect, 16 / 9, 90);
    expect(g.height).toBeGreaterThan(90);
    expect(header.grid).toEqual({ fullWidth: g.fullWidth, fullHeight: g.fullHeight, x0: g.x0, y0: g.y0 });
    const f = parseFrame(msgs[1] as Uint8Array);
    expect(f.ok && f.frame.keyframe).toBe(true);
  });

  it("says hello with the viewer id and records actions with the viewer and intent id", async () => {
    const c = await live();
    const hello = await c.next((m) => m["type"] === "hello");
    expect(hello["viewer"]).toMatch(/^v-\d+$/);
    expect(hello["turnstile"]).toBeUndefined();
    const id = `tod-${Date.now()}`;
    c.ws.send(JSON.stringify({ type: "intent", id, name: "time_of_day", args: { v: 0.75 } }));
    await c.next((m) => m["type"] === "receipt");
    const a = coord.fleetState().actions.at(-1)!;
    expect(a).toMatchObject({ name: "time_of_day", args: { v: 0.75 }, viewer: hello["viewer"], id, outcome: "committed" });
    c.ws.close();
  });
});

describe("mock race, second design review", () => {
  it("runs the owner's cadence, logs it as the schedule, and holds viewers to the cooldown", async () => {
    const c = await live();
    await admin("/mock/schedule", { next_in_ms: 0, cooldown_s: 30 });
    await until(() => coord.fleetState().relaunch_all?.by === "schedule" && coord.fleetState().relaunch_all!.ec2_done_at === null);
    const st = coord.fleetState();
    expect(st.actions.at(-1)).toMatchObject({ viewer: "schedule", name: "relaunch_all", outcome: "committed" });
    expect(st.cooldown_until! - st.relaunch_all!.started_at).toBe(30_000);
    await until(() => coord.fleetState().relaunch_all!.ec2_done_at !== null);
    c.ws.send(JSON.stringify({ type: "intent", id: `cool-${Date.now()}`, name: "relaunch_all", args: {} }));
    const r = await c.next((m) => m["type"] === "receipt");
    expect(r).toMatchObject({ outcome: "refused" });
    expect(String(r["reason"])).toMatch(/^the next restart is allowed in \d+ s$/);
    await admin("/mock/schedule", { next_in_ms: null, cooldown_s: 1, clear_cooldown: true });
    c.ws.close();
  });

  it("keeps a capacity wait out of EC2's time and says how long it was", async () => {
    const c = await live();
    await admin("/mock/schedule", { capacity_wait_ms: 600 });
    c.ws.send(JSON.stringify({ type: "intent", id: `cap-${Date.now()}`, name: "relaunch_all", args: {} }));
    await c.next((m) => m["type"] === "receipt");
    await until(() => coord.fleetState().relaunch_all!.ec2_waiting_since !== null);
    await until(() => coord.fleetState().tiles.find((t) => t.id === "ec2:1")!.phase === "waiting_capacity");
    expect(coord.fleetState().relaunch_all!.ec2_waiting_since).not.toBeNull();
    await until(() => coord.fleetState().relaunch_all!.ec2_done_at !== null);
    const st = coord.fleetState();
    const ra = st.relaunch_all!;
    expect(ra.ec2_capacity_wait_ms).toBe(600);
    expect(ra.ec2_waiting_since).toBeNull();
    const sb = st.scoreboard.ec2!;
    expect(sb.last_ms).toBe(ra.ec2_done_at! - ra.started_at - 600);
    expect(sb.last_capacity_wait_ms).toBe(600);
    expect(sb.measured_on).toBe(1);
    expect(st.scoreboard.fcvm!.last_ms).toBe(ra.fcvm_done_at! - ra.started_at);
    expect(st.scoreboard.fcvm!.measured_on).toBe(2);
    expect(st.tiles.find((t) => t.id === "ec2:1")!.last_relaunch!.phases["capacity_wait_ms"]).toBe(600);
    c.ws.close();
    await sleep(1000);
  });

  it("counts every GPU VM restored, and scrubs only once the scrub has shipped", async () => {
    const before = coord.fleetState().counters;
    expect(coord.fleetState().security).toEqual({ gpu_scrub: false });
    const c = await live();
    c.ws.send(JSON.stringify({ type: "intent", id: `cnt-${Date.now()}`, name: "relaunch", args: { tile: "fcvm:1" } }));
    await c.next((m) => m["type"] === "receipt");
    await until(() => coord.fleetState().counters.restores_total === before.restores_total + 1);
    expect(coord.fleetState().counters.scrubs_total).toBe(before.scrubs_total);
    expect(coord.fleetState().counters.restores_last_hour).toBe(before.restores_last_hour + 1);
    await admin("/mock/schedule", { gpu_scrub: true });
    expect(coord.fleetState().security).toEqual({ gpu_scrub: true });
    c.ws.send(JSON.stringify({ type: "intent", id: `cnt2-${Date.now()}`, name: "relaunch", args: { tile: "fcvm:1" } }));
    await c.next((m) => m["type"] === "receipt" && String(m["id"]).startsWith("cnt2"));
    await until(() => coord.fleetState().counters.restores_total === before.restores_total + 2);
    expect(coord.fleetState().counters.scrubs_total).toBe(before.scrubs_total + 1);
    await admin("/mock/schedule", { gpu_scrub: false });
    c.ws.close();
  });

  it("chooses the next camera stop by the viewers' votes", async () => {
    const c = await live();
    c.ws.send(JSON.stringify({ type: "intent", id: `n-${Date.now()}`, name: "nudge", args: { target: "stars" } }));
    await c.next((m) => m["type"] === "receipt");
    expect(coord.fleetState().nudges.targets.find((t) => t.id === "stars")!.votes).toBe(1);
    await admin("/mock/schedule", { hold_in_ms: 0 });
    await until(() => coord.fleetState().nudges.holding === "stars");
    const keys = coord.patternScene().camera as unknown as { x: number; zoom: number }[];
    expect(keys.at(-1)).toMatchObject({ x: 1.2, zoom: 0.7 });
    expect(coord.fleetState().nudges.targets.every((t) => t.votes === 0)).toBe(true);
    await admin("/mock/schedule", { hold_in_ms: 600_000 });
    c.ws.close();
  });

  it("lends a wall GPU: the tile is borrowed and sits out races, the claim streams, then the tile restores", async () => {
    const c = await live();
    const other = await live();
    c.ws.send(JSON.stringify({ type: "intent", id: `claim-${Date.now()}`, name: "claim_gpu", args: {} }));
    expect(await c.next((m) => m["type"] === "receipt")).toMatchObject({ outcome: "committed" });
    const first = await c.next((m) => m["type"] === "claim");
    const claim = first["claim"] as Record<string, unknown>;
    expect(claim).toMatchObject({ phase: "restoring", frame_url: null, tile: "fcvm:2" });
    const tile = () => coord.fleetState().tiles.find((t) => t.id === "fcvm:2")!;
    expect(tile().phase).toBe("borrowed");
    expect(coord.fleetState().borrow).toMatchObject({ cap: 1, lent: 1 });
    // The cap: a second viewer is told when to try again.
    other.ws.send(JSON.stringify({ type: "intent", id: `claim2-${Date.now()}`, name: "claim_gpu", args: {} }));
    const refused = await other.next((m) => m["type"] === "receipt");
    expect(String(refused["reason"])).toMatch(/^all 1 GPUs we lend are out; the next comes back in \d+ s$/);
    const running = await c.next((m) => m["type"] === "claim" && (m["claim"] as { phase: string }).phase === "running");
    const rc = running["claim"] as { frame_url: string; first_frame_at: number; expires_at: number; clicked_at: number };
    expect(rc.expires_at - rc.first_frame_at).toBe(1000);
    expect(tile().borrowed!.until).toBe(rc.expires_at);
    // A race now leaves the borrowed tile out.
    await admin("/mock/schedule", { next_in_ms: 0 });
    await until(() => coord.fleetState().relaunch_all?.by === "schedule" && coord.fleetState().relaunch_all!.started_at > rc.first_frame_at);
    expect(coord.fleetState().relaunch_all!.fcvm_gpus).toBe(1);
    expect(tile().phase).toBe("borrowed");
    await admin("/mock/schedule", { next_in_ms: null });
    // The claim's own camera.
    const camId = `cc-${Date.now()}`;
    c.ws.send(JSON.stringify({ type: "intent", id: camId, name: "claim_camera", args: { drag: { yaw_deg: 9, pitch_deg: 0, dive: 0 } } }));
    const moved = await c.next((m) => m["type"] === "claim" && ((m["claim"] as { camera: { intent?: string }[] }).camera.some((k) => k.intent === camId)));
    expect(moved).toBeTruthy();
    const ended = await c.next((m) => m["type"] === "claim" && (m["claim"] as { phase: string }).phase === "ended");
    expect((ended["claim"] as { ended: string }).ended).toBe("Your minute is up. The GPU went back to the pool.");
    expect(tile().borrowed).toBeNull();
    expect(["relaunching", "stopping", "restoring", "starting"]).toContain(tile().phase);
    await until(() => tile().phase === "running");
    expect(tile().last_relaunch!.by).toBe("return");
    c.ws.close();
    other.ws.close();
    await until(() => coord.fleetState().relaunch_all!.ec2_done_at !== null);
    await admin("/mock/schedule", { clear_cooldown: true });
  });

  it("asks for Turnstile when it has a site key, refusing intents until verified", async () => {
    await admin("/mock/turnstile", { sitekey: "example-sitekey" });
    const c = await live();
    expect(await c.next((m) => m["type"] === "hello")).toMatchObject({ turnstile: "example-sitekey" });
    c.ws.send(JSON.stringify({ type: "intent", id: `ts-1-${Date.now()}`, name: "time_of_day", args: { v: 0.3 } }));
    expect(await c.next((m) => m["type"] === "receipt")).toMatchObject({ outcome: "refused", reason: "verify first" });
    c.ws.send(JSON.stringify({ type: "verify", token: "wrong" }));
    expect(await c.next((m) => m["type"] === "verified")).toMatchObject({ ok: false });
    c.ws.send(JSON.stringify({ type: "verify", token: "mock-pass" }));
    expect(await c.next((m) => m["type"] === "verified" && m["ok"] === true)).toMatchObject({ ok: true });
    const id = `ts-2-${Date.now()}`;
    c.ws.send(JSON.stringify({ type: "intent", id, name: "time_of_day", args: { v: 0.3 } }));
    expect(await c.next((m) => m["type"] === "receipt" && m["id"] === id)).toMatchObject({ outcome: "committed" });
    await admin("/mock/turnstile", { sitekey: null });
    c.ws.close();
  });

  it("serves the poster as a JPEG any origin can read", async () => {
    const r = await fetch(`http://localhost:${coord.port}/poster.jpg`);
    expect(r.status).toBe(200);
    expect(r.headers.get("content-type")).toBe("image/jpeg");
    expect(r.headers.get("access-control-allow-origin")).toBe("*");
    const b = new Uint8Array(await r.arrayBuffer());
    expect([b[0], b[1]]).toEqual([0xff, 0xd8]);
  });

  it("goes offline: drops /live and refuses it, then takes viewers again", async () => {
    const c = await live();
    const closed = new Promise<void>((res) => c.ws.on("close", () => res()));
    await admin("/mock/offline", { ms: 400 });
    await closed;
    const refused = await new Promise<number>((res) => {
      const ws = new WebSocket(`ws://localhost:${coord.port}/live`);
      ws.on("unexpected-response", (_req, resp) => res(resp.statusCode ?? 0));
      ws.on("open", () => {
        ws.close();
        res(101);
      });
    });
    expect(refused).toBe(503);
    await sleep(450);
    const back = await live();
    expect(await back.next((m) => m["type"] === "hello")).toBeTruthy();
    back.ws.close();
  });

  it("fails a tile with a retry time, and reclaims a host: draining, then no GPU behind its places", async () => {
    await admin("/mock/fault", { fail: "fcvm:1", message: "GPU did not come back", retry_ms: 300 });
    let t = coord.fleetState().tiles.find((x) => x.id === "fcvm:1")!;
    expect(t.phase).toBe("failed");
    expect(t.failure).toMatchObject({ kind: "relaunch_failed", message: "GPU did not come back" });
    expect(t.error).toBe("GPU did not come back");
    await until(() => coord.fleetState().tiles.find((x) => x.id === "fcvm:1")!.phase === "running");
    expect(coord.fleetState().tiles.find((x) => x.id === "fcvm:1")!.failure).toBeNull();
    await admin("/mock/fault", { reclaim: "mock-host-2" });
    t = coord.fleetState().tiles.find((x) => x.id === "fcvm:2")!;
    expect(t.phase).toBe("draining");
    expect(t.failure?.kind).toBe("host_reclaimed");
    expect(coord.fleetState().fleet.hosts.find((h) => h.name === "mock-host-2")!.draining).toBe(true);
    await until(() => coord.fleetState().tiles.find((x) => x.id === "fcvm:2")!.phase === "waiting_capacity");
    t = coord.fleetState().tiles.find((x) => x.id === "fcvm:2")!;
    expect(t.slot).toBeNull();
    expect(coord.fleetState().fleet.hosts.map((h) => h.name)).toEqual(["mock-host-1"]);
    await admin("/mock/fault", { replace_host: "mock-host-2" });
    await until(() => coord.fleetState().tiles.find((x) => x.id === "fcvm:2")!.phase === "running");
  });
});
