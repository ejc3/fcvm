// The GPU mirror's replay state (app/_wall/mirror/gl.ts) against a recording stand-in for
// WebGL2: what a context switch puts back, and how composition layers are transformed.

import { describe, expect, it } from "vitest";
import { GlMirror, layerTransform } from "../../app/_wall/mirror/gl.ts";
import { encodeCall, encodeRecord, JOIN_PART, RECORD, RecordReader } from "../mirror/wire.ts";

type Call = { name: string; args: unknown[]; ret?: unknown };

/** WebGL2 as a log: every call recorded, every created object a fresh tagged object. */
function fakeGl(): { gl: WebGL2RenderingContext; calls: Call[] } {
  const calls: Call[] = [];
  let n = 0;
  const canvas = { width: 720, height: 1280 };
  const gl = new Proxy(
    {},
    {
      get(_t, prop: string) {
        if (prop === "canvas") return canvas;
        if (prop === "drawingBufferWidth") return canvas.width;
        if (prop === "drawingBufferHeight") return canvas.height;
        if (/^[A-Z_0-9]+$/.test(prop)) return 0;
        return (...args: unknown[]) => {
          const c: Call = { name: prop, args };
          calls.push(c);
          if (prop.startsWith("create") || prop === "getUniformLocation") c.ret = { obj: prop, id: ++n };
          if (prop === "getUniformBlockIndex") c.ret = 0;
          return prop === "getParameter" ? null : c.ret;
        };
      },
    },
  ) as unknown as WebGL2RenderingContext;
  return { gl, calls };
}

(globalThis as Record<string, unknown>).WebGLRenderbuffer ??= class {
  readonly stand_in = true;
};

const u32 = (...xs: number[]) => new Uint8Array(new Uint32Array(xs).buffer);

/** A stream on channels the copy saw open, fed to a fresh mirror. */
function play(steps: ([number, string, (number | bigint | Uint8Array)[]] | [number, "reply" | "free", number[]])[]) {
  const { gl, calls } = fakeGl();
  const m = new GlMirror(gl, () => 0);
  const recs: Uint8Array[] = [encodeRecord(RECORD.hello, 0, u32(0), 1)];
  for (const ch of new Set(steps.filter((s) => s[1] !== "free").map((s) => s[0]))) recs.push(encodeRecord(RECORD.open, ch, new Uint8Array(0)));
  for (const [ch, name, args] of steps) {
    if (name === "free") recs.push(encodeRecord(RECORD.free, 0, new Uint8Array(0), args[0] as number));
    else if (name === "reply") recs.push(encodeRecord(RECORD.reply, ch, u32(...(args as number[])), 4 * args.length));
    else recs.push(encodeRecord(RECORD.data, ch, encodeCall(name, args as (number | bigint | Uint8Array)[])));
  }
  const bytes = new Uint8Array(recs.reduce((a, r) => a + r.length, 0));
  let at = 0;
  for (const r of recs) {
    bytes.set(r, at);
    at += r.length;
  }
  for (const r of new RecordReader().push(bytes)) m.feed(r);
  return { m, calls };
}

/** The last call named `name` (with `first` as its first argument, when given) before the last `before`. */
function lastBefore(calls: Call[], name: string, before: string, first?: unknown): Call | undefined {
  const end = calls.map((c) => c.name).lastIndexOf(before);
  return calls
    .slice(0, end)
    .filter((c) => c.name === name && (first === undefined || c.args[0] === first))
    .pop();
}

const BLEND = 0x0be2;
const contexts = (shareB = 0): ([number, string, number[]] | [number, "reply", number[]])[] => [
  [1, "rcCreateContext", [1, 0, 3]],
  [1, "reply", [1]],
  [2, "rcCreateContext", [1, shareB, 3]],
  [2, "reply", [2]],
  [1, "rcMakeCurrent", [1, 0, 0]],
  [1, "reply", [1]],
  [2, "rcMakeCurrent", [2, 0, 0]],
  [2, "reply", [1]],
];

describe("the mirror's context switch", () => {
  it("puts back one context's enable when another disabled the same capability", () => {
    const { m, calls } = play([...contexts(), [1, "glEnable", [BLEND]], [2, "glDisable", [BLEND]], [1, "glClear", [0x4000]]]);
    expect(m.stats.failed).toEqual({});
    const caps = calls.filter((c) => (c.name === "enable" || c.name === "disable") && c.args[0] === BLEND).map((c) => c.name);
    expect(caps.at(-1), `the switch back to context 1 left BLEND ${caps.at(-1)}d: ${caps.join(", ")}`).toBe("enable");
  });

  it("gives a context that never set a value the default, after the mirror's own drawing", () => {
    const { calls } = play([
      ...contexts(),
      [1, "glDepthMask", [0]],
      // the mirror makes the color buffer with GL of its own, between the contexts' calls
      [1, "rcCreateColorBuffer", [16, 16, 0x1908]],
      [1, "reply", [9]],
      [2, "glClear", [0x4000]],
    ]);
    expect(lastBefore(calls, "depthMask", "clear")?.args).toEqual([true]);
  });

  it("binds a texture name's current object when it is put back, after a color buffer took the name", () => {
    const { calls } = play([
      ...contexts(1),
      [1, "rcCreateColorBuffer", [16, 16, 0x1908]],
      [1, "reply", [9]],
      [1, "glGenTextures", [1, 4]],
      [1, "reply", [5]],
      [1, "glBindTexture", [0x0de1, 5]],
      [1, "rcBindTexture", [9]],
      [2, "glClear", [0x4000]],
      [1, "glClear", [0x4000]],
    ]);
    // the color buffer's texture is the first the mirror made (rcCreateColorBuffer), name 5's own the second
    const textures = calls.filter((c) => c.name === "createTexture").map((c) => c.ret);
    expect(textures.length).toBeGreaterThanOrEqual(2);
    expect(lastBefore(calls, "bindTexture", "clear", 0x0de1)?.args[1]).toBe(textures[0]);
  });
});

describe("color buffers the host destroys", () => {
  const made = (calls: Call[]) => calls.filter((c) => c.name === "createTexture").map((c) => c.ret);
  const deleted = (calls: Call[]) => calls.filter((c) => c.name === "deleteTexture").map((c) => c.args[0]);

  it("lets go of a color buffer when the host frees it, not when the guest closes it", () => {
    const { m, calls } = play([
      ...contexts(),
      [1, "rcCreateColorBuffer", [16, 16, 0x1908]],
      [1, "reply", [9]],
      // the refcount pipe still holds it: the host keeps it, and so does the mirror
      [1, "rcCloseColorBuffer", [9]],
      [1, "rcFBPost", [9]],
    ]);
    expect(deleted(calls)).toEqual([]);
    expect(m.stats.posts).toBe(1);
    const freed = play([
      ...contexts(),
      [1, "rcCreateColorBuffer", [16, 16, 0x1908]],
      [1, "reply", [9]],
      [0, "free", [9]],
      [1, "rcFBPost", [9]],
    ]);
    expect(deleted(freed.calls)).toEqual([made(freed.calls)[0]]);
    expect(freed.m.stats.freed).toBe(1);
    // the handle names nothing now
    expect(freed.m.stats.posts).toBe(0);
  });

  it("keeps a freed color buffer's texture while a guest texture draws with it, until that texture is deleted", () => {
    const { m, calls } = play([
      ...contexts(),
      [1, "rcCreateColorBuffer", [16, 16, 0x1908]],
      [1, "reply", [9]],
      [1, "glGenTextures", [1, 4]],
      [1, "reply", [5]],
      [1, "glBindTexture", [0x0de1, 5]],
      [1, "rcBindTexture", [9]],
      [0, "free", [9]],
    ]);
    expect(m.stats.freed).toBe(1);
    // the buffer's texture stays (name 5's own, which rcBindTexture replaced, goes)
    expect(deleted(calls)).not.toContain(made(calls)[0]);
    const after = play([
      ...contexts(),
      [1, "rcCreateColorBuffer", [16, 16, 0x1908]],
      [1, "reply", [9]],
      [1, "glGenTextures", [1, 4]],
      [1, "reply", [5]],
      [1, "glBindTexture", [0x0de1, 5]],
      [1, "rcBindTexture", [9]],
      [0, "free", [9]],
      [1, "glDeleteTextures", [1, u32(5)]],
    ]);
    expect(deleted(after.calls)).toContain(made(after.calls)[0]);
  });
});

describe("color buffers the host destroys, shared and reused", () => {
  const deleted = (calls: Call[]) => calls.filter((c) => c.name === "deleteTexture").map((c) => c.args[0]);
  const made = (calls: Call[]) => calls.filter((c) => c.name === "createTexture").map((c) => c.ret);
  const bound = (name: number): ([number, string, number[]] | [number, "reply", number[]])[] => [
    [1, "glBindTexture", [0x0de1, name]],
    [1, "rcBindTexture", [9]],
  ];

  it("keeps a freed buffer's texture until the last guest texture drawing with it is deleted", () => {
    const steps: ([number, string, (number | Uint8Array)[]] | [number, "reply" | "free", number[]])[] = [
      ...contexts(),
      [1, "rcCreateColorBuffer", [16, 16, 0x1908]],
      [1, "reply", [9]],
      [1, "glGenTextures", [2, 4]],
      [1, "reply", [5, 6]],
      ...bound(5),
      ...bound(6),
      [0, "free", [9]],
      [1, "glDeleteTextures", [1, u32(5)]],
    ];
    const one = play(steps);
    expect(deleted(one.calls)).not.toContain(made(one.calls)[0]);
    const both = play([...steps, [1, "glDeleteTextures", [1, u32(6)]]]);
    expect(deleted(both.calls)).toContain(made(both.calls)[0]);
  });

  it("keeps a freed buffer's texture while a client image made from it lives, and lets it go with the image", () => {
    const steps: ([number, string, (number | Uint8Array)[]] | [number, "reply" | "free", number[]])[] = [
      ...contexts(),
      [1, "rcCreateColorBuffer", [16, 16, 0x1908]],
      [1, "reply", [9]],
      [1, "glGenTextures", [1, 4]],
      [1, "reply", [5]],
      ...bound(5),
      // an EGLImage of texture 5, then the name goes and the host frees the buffer
      [1, "rcCreateClientImage", [1, 0x30b1, 5]],
      [1, "reply", [77]],
      [1, "glDeleteTextures", [1, u32(5)]],
      [0, "free", [9]],
    ];
    const live = play(steps);
    expect(deleted(live.calls)).not.toContain(made(live.calls)[0]);
    const gone = play([...steps, [1, "rcDestroyClientImage", [77]]]);
    expect(deleted(gone.calls)).toContain(made(gone.calls)[0]);
  });

  // Every owner that can outlive a FREE, and every way it lets go: the texture stays until
  // the last one does, then goes.
  type Step = [number, string, (number | Uint8Array)[]] | [number, "reply" | "free", number[]];
  const two: Step[] = [
    ...contexts(),
    [1, "rcCreateColorBuffer", [16, 16, 0x1908]],
    [1, "reply", [9]],
    [1, "rcCreateColorBuffer", [16, 16, 0x1908]],
    [1, "reply", [10]],
  ];
  const owners: { owner: string; hold: Step[]; lets: Record<string, Step[]> }[] = [
    {
      owner: "a window surface",
      hold: [[1, "rcCreateWindowSurface", [1, 16, 16]], [1, "reply", [3]], [1, "rcSetWindowColorBuffer", [3, 9]]],
      lets: { "is destroyed": [[1, "rcDestroyWindowSurface", [3]]], "takes another buffer": [[1, "rcSetWindowColorBuffer", [3, 10]]] },
    },
    {
      owner: "a renderbuffer",
      hold: [[1, "glGenRenderbuffers", [1, 4]], [1, "reply", [6]], [1, "glBindRenderbuffer", [0x8d41, 6]], [1, "rcBindRenderbuffer", [9]]],
      lets: { "is deleted": [[1, "glDeleteRenderbuffers", [1, u32(6)]]], "takes another buffer": [[1, "rcBindRenderbuffer", [10]]] },
    },
    {
      owner: "a guest texture name",
      hold: [[1, "glGenTextures", [1, 4]], [1, "reply", [5]], ...bound(5)],
      lets: { "is deleted": [[1, "glDeleteTextures", [1, u32(5)]]], "takes another buffer": [[1, "rcBindTexture", [10]]] },
    },
  ];
  for (const { owner, hold, lets } of owners)
    for (const [how, release] of Object.entries(lets))
      it(`keeps a freed buffer's texture while ${owner} draws with it, and lets it go when it ${how}`, () => {
        const held = play([...two, ...hold, [0, "free", [9]]]);
        expect(deleted(held.calls), "deleted while held").not.toContain(made(held.calls)[0]);
        const after = play([...two, ...hold, [0, "free", [9]], ...release]);
        expect(deleted(after.calls), "kept after the last owner let go").toContain(made(after.calls)[0]);
        expect(deleted(after.calls), "the other buffer's texture").not.toContain(made(after.calls)[1]);
      });

  it("does not free a buffer the guest made again with the same handle before the old one's FREE arrived", () => {
    // The tap copies a call's bytes before the host runs it, and the host frees the old
    // buffer with that handle while it runs the create: the old buffer's FREE follows the
    // replacement's create on the wire.
    const { m, calls } = play([
      ...contexts(),
      [1, "rcCreateColorBufferWithHandle", [16, 16, 0x1908, 9]],
      [1, "rcCreateColorBufferWithHandle", [32, 32, 0x1908, 9]],
      [0, "free", [9]],
      [1, "rcFBPost", [9]],
    ]);
    expect(m.stats.posts).toBe(1);
    // the old buffer's texture went, the replacement's stays
    expect(deleted(calls)).toEqual([made(calls)[0]]);
    expect(deleted(calls)).not.toContain(made(calls)[1]);
  });
});

describe("composition layers", () => {
  // gfxstream's host/gl/texture_draw.cpp kVertices: for each transform, the texture coordinate
  // at the quad's corners (+1,-1), (+1,+1), (-1,+1), (-1,-1)
  const corners: [number, number][] = [
    [1, -1],
    [1, 1],
    [-1, 1],
    [-1, -1],
  ];
  const table: Record<number, [number, number][]> = {
    0: [[1, 0], [1, 1], [0, 1], [0, 0]],
    4: [[1, 1], [0, 1], [0, 0], [1, 0]],
    3: [[0, 1], [0, 0], [1, 0], [1, 1]],
    7: [[0, 0], [1, 0], [1, 1], [0, 1]],
    1: [[0, 0], [0, 1], [1, 1], [1, 0]],
    2: [[1, 1], [1, 0], [0, 0], [0, 1]],
    5: [[0, 1], [1, 1], [1, 0], [0, 0]],
    6: [[1, 0], [0, 0], [0, 1], [1, 1]],
  };
  it("samples each HWC transform's texture coordinate at each corner, as gfxstream draws it", () => {
    for (const [t, coords] of Object.entries(table)) {
      const [m00, m01, m10, m11, ox, oy] = layerTransform(Number(t));
      corners.forEach(([px, py], i) => {
        const q = [px * 0.5 + 0.5, py * 0.5 + 0.5] as const;
        const got = [m00 * q[0] + m01 * q[1] + ox, m10 * q[0] + m11 * q[1] + oy];
        expect(got, `transform ${t}, corner ${i}`).toEqual(coords[i]);
      });
    }
  });

  it("draws device layers and solid colours only, as the phone's composer does", () => {
    // a compose of two layers onto color buffer 9: a client layer (skipped) and a device layer
    const layer = (cb: number, mode: number) => {
      const b = new DataView(new ArrayBuffer(56));
      b.setUint32(0, cb, true);
      b.setInt32(4, mode, true);
      b.setInt32(16, 16, true); // right
      b.setInt32(20, 16, true); // bottom
      b.setFloat32(32, 16, true);
      b.setFloat32(36, 16, true);
      b.setInt32(40, 2, true); // premultiplied
      b.setFloat32(44, 1, true);
      return new Uint8Array(b.buffer);
    };
    const head = new Uint8Array(new Uint32Array([1, 9, 2]).buffer);
    const buf = new Uint8Array([...head, ...layer(9, 1), ...layer(9, 2)]);
    const { calls } = play([
      [1, "rcCreateColorBuffer", [16, 16, 0x1908]],
      [1, "reply", [9]],
      [1, "rcCompose", [buf.length, buf]],
    ]);
    expect(calls.filter((c) => c.name === "drawArrays")).toHaveLength(2); // one layer, then the post
  });
});

describe("the join snapshot", () => {
  /** A stream that starts with a join: HELLO, BEGIN, color buffers, END, then live records. */
  const joined = (parts: Uint8Array[], live: Uint8Array[] = []) => {
    const { gl, calls } = fakeGl();
    const m = new GlMirror(gl, () => 0);
    const recs = [encodeRecord(RECORD.hello, 0, u32(1, 3), 1), encodeRecord(RECORD.join, 0, u32(1), JOIN_PART.begin), ...parts, ...live];
    const bytes = new Uint8Array(recs.reduce((a, r) => a + r.length, 0));
    let at = 0;
    for (const r of recs) {
      bytes.set(r, at);
      at += r.length;
    }
    for (const r of new RecordReader().push(bytes)) m.feed(r);
    return { m, calls };
  };
  const colorBuffer = (handle: number, w: number, h: number, fill: number, refcount = 1) =>
    encodeRecord(RECORD.join, 0, new Uint8Array([...u32(handle, w, h, 1, refcount, 1), ...new Uint8Array(w * h * 4).fill(fill)]), JOIN_PART.colorBuffer);
  const end = (status: number, pauseUs: number, lastPosted: number) => encodeRecord(RECORD.join, 0, u32(status, pauseUs, lastPosted, 0), JOIN_PART.end);

  it("makes every color buffer with its pixels and draws the screen the phone posted last", () => {
    const { m, calls } = joined([colorBuffer(4, 2, 2, 0x11), colorBuffer(9, 4, 2, 0x22), end(0, 61_000, 9)]);
    expect(m.stats.failed).toEqual({});
    expect(m.stats.join).toEqual({ colorBuffers: 2, bytes: 16 + 32, malformed: 0, ok: true, pauseUs: 61_000, skipped: 0 });
    const uploads = calls.filter((c) => c.name === "texSubImage2D");
    expect(uploads.map((c) => [c.args[4], c.args[5], (c.args[8] as Uint8Array)[0]])).toEqual([
      [2, 2, 0x11],
      [4, 2, 0x22],
    ]);
    // the post: the screen drawn at once, at the posted buffer's size
    expect(m.stats.posts).toBe(1);
    expect(calls.filter((c) => c.name === "drawArrays")).toHaveLength(1);
  });

  it("composes a layer the snapshot made, as a live color buffer", () => {
    const layer = new DataView(new ArrayBuffer(56));
    layer.setUint32(0, 4, true);
    layer.setInt32(4, 2, true); // device
    layer.setInt32(16, 2, true);
    layer.setInt32(20, 2, true);
    layer.setFloat32(32, 2, true);
    layer.setFloat32(36, 2, true);
    layer.setInt32(40, 1, true); // no blending
    layer.setFloat32(44, 1, true);
    const buf = new Uint8Array([...u32(1, 9, 1), ...new Uint8Array(layer.buffer)]);
    // channel 3 is older than the copy: renderControl still decodes on it
    const live = [encodeRecord(RECORD.data, 3, encodeCall("rcCompose", [buf.length, buf]))];
    const { m, calls } = joined([colorBuffer(4, 2, 2, 0x11), colorBuffer(9, 2, 2, 0), end(0, 1, 9)], live);
    expect(m.stats.failed).toEqual({});
    // the END's post, then the compose's layer and its post
    expect(m.stats.posts).toBe(2);
    expect(calls.filter((c) => c.name === "drawArrays")).toHaveLength(3);
  });

  it("draws nothing from a snapshot that failed or does not match its header", () => {
    const bad = encodeRecord(RECORD.join, 0, new Uint8Array([...u32(4, 2, 2, 1, 1, 1), 0, 0]), JOIN_PART.colorBuffer);
    const { m } = joined([bad, colorBuffer(9, 2, 2, 0), end(1, 0, 9)]);
    expect(m.stats.join?.malformed).toBe(1);
    expect(m.stats.join?.ok).toBe(false);
    expect(m.stats.posts).toBe(0);
  });
});
