import { afterAll, beforeAll, describe, expect, it } from "vitest";
import WebSocket from "ws";
import { parseFrame, parseStreamHeader, splitNals, nalType, NAL_SPS, NAL_IDR } from "../../core/frame.ts";
import { MockCoordinator, halfGrid } from "../coordinator.ts";
import { MockRenderers } from "../renderer.ts";
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
    port: 0, perHalf: 2, ec2Short: 1, fcvmRelaunchMs: 400, ec2RelaunchMs: 900,
    refusedWords: ["forbidden"], checkMs: 20, epochUnixMs, renderers,
  });
  await coord.listen();
});

afterAll(async () => {
  await coord.close();
  await renderers.close();
});

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
    expect(header!.height).toBe(90);
    const ok = frames.map((f) => { if (!f.ok) throw new Error(f.error); return f.frame; });
    expect(ok[0]!.keyframe).toBe(true);
    const types = splitNals(ok[0]!.data).map(nalType);
    expect(types).toContain(NAL_SPS);
    expect(types).toContain(NAL_IDR);
    for (let i = 1; i < ok.length; i++) {
      expect(ok[i]!.frame).toBeGreaterThan(ok[i - 1]!.frame);
      expect(ok[i]!.t).toBeCloseTo(ok[i]!.frame / FPS, 9);
    }
    expect(ok[0]!.width).toBe(160); // 1/4 x 1/2 of a 16:9 image at 90 high
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
    let sentResize = false;
    await new Promise<void>((res) => {
      ws.on("message", (d, bin) => {
        if (!bin) {
          headers++;
          return;
        }
        const p = parseFrame(new Uint8Array(d as Buffer));
        if (!p.ok) throw new Error(p.error);
        if (!sentResize) {
          sentResize = true;
          ws.send(JSON.stringify({ h: 180 }));
          return;
        }
        if (p.frame.height !== 180) return;
        after.push({ t: p.frame.t, data: p.frame.data });
        if (after.length === 6) res();
      });
    });
    ws.close();
    await r.close();
    expect(headers).toBe(2);
    // Decode them and compare each frame with the picture at its t.
    const stream = Buffer.concat(after.map((f) => Buffer.from(f.data)));
    const out = spawnSync("ffmpeg", ["-hide_banner", "-loglevel", "error", "-f", "h264", "-i", "pipe:0", "-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"], { input: stream, maxBuffer: 1 << 26 });
    expect(out.status).toBe(0);
    const W = 320;
    const H = 180;
    const frames = Math.floor(out.stdout.length / (W * H * 3));
    expect(frames).toBe(6);
    for (let k = 0; k < frames; k++) {
      const f = frameCtx(after[k]!.t, scene);
      let err = 0;
      let n = 0;
      for (let j = 5; j < H; j += 10) {
        for (let i = 5; i < W; i += 10) {
          const e = expectedRgb(rect.x + ((i + 0.5) / W) * rect.w, rect.y + ((j + 0.5) / H) * rect.h, f);
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
    const scene = { epoch_unix_ms: 0, image_aspect: 16 / 9, camera: [{ t: 0, x: 0.1, y: 0, zoom: 1.3 }], time_of_day: 7, words: [{ text: "hi", pos: { x: 0, y: 0 } }], lights: [] };
    const f = frameCtx(12.35, scene);
    const whole = renderI420({ x: 0.25, y: 0.25, w: 0.5, h: 0.25 }, 64, 16, f);
    const left = renderI420({ x: 0.25, y: 0.25, w: 0.25, h: 0.25 }, 32, 16, f);
    for (let j = 0; j < 16; j++) for (let i = 0; i < 32; i++) expect(left[j * 32 + i]).toBe(whole[j * 64 + i]);
  });

  it("changes with t, so a tile drawn at another t shows a seam", () => {
    const scene = { epoch_unix_ms: 0, image_aspect: 16 / 9, camera: [], time_of_day: 7, words: [], lights: [] };
    const a = renderI420({ x: 0, y: 0, w: 0.1, h: 0.1 }, 16, 16, frameCtx(10, scene));
    const b = renderI420({ x: 0, y: 0, w: 0.1, h: 0.1 }, 16, 16, frameCtx(10.5, scene));
    let diff = 0;
    for (let i = 0; i < a.length; i++) diff += Math.abs(a[i]! - b[i]!);
    expect(diff / a.length).toBeGreaterThan(3);
  });
});
