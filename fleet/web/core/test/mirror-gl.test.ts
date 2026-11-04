// The GPU mirror's replay state (app/_wall/mirror/gl.ts) against a recording stand-in for
// WebGL2: what a context switch puts back, and how composition layers are transformed.

import { describe, expect, it } from "vitest";
import { GlMirror, layerTransform } from "../../app/_wall/mirror/gl.ts";
import { encodeCall, encodeRecord, JOIN_PART, RECORD, RecordReader } from "../mirror/wire.ts";
import { channelBlob, GL, glState, textureBlob } from "./gles-writer.ts";

type Call = { name: string; args: unknown[]; ret?: unknown };

/** The binding query of each buffer target, as WebGL2's getParameter takes it. */
const BINDING_OF = new Map<number, number>([
  [0x8894, 0x8892],
  [0x8f36, 0x8f36],
  [0x8f37, 0x8f37],
  [0x88ed, 0x88eb],
  [0x88ef, 0x88ec],
  [0x8a28, 0x8a11],
  [0x8c8f, 0x8c8e],
]);

/**
 * WebGL2 as a log: every call recorded, every created object a fresh tagged object. Buffer
 * bindings are tracked for getParameter (the element array binding per vertex array), and every
 * program reports the uniform blocks `blocks` names, in that order.
 */
function fakeGl(o: { blocks?: string[] } = {}): { gl: WebGL2RenderingContext; calls: Call[] } {
  const calls: Call[] = [];
  let n = 0;
  const canvas = { width: 720, height: 1280 };
  const bound = new Map<number, unknown>();
  const elements = new Map<unknown, unknown>();
  let vao: unknown = null;
  const blocks = o.blocks ?? [];
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
          if (prop === "bindBuffer" && args[0] === 0x8893) elements.set(vao, args[1]);
          else if (prop === "bindBuffer" || prop === "bindBufferBase" || prop === "bindBufferRange") bound.set(args[0] as number, args.at(prop === "bindBuffer" ? 1 : 2));
          if (prop === "bindVertexArray") vao = args[0];
          if (prop === "getUniformBlockIndex") c.ret = o.blocks ? (blocks.indexOf(args[1] as string) >>> 0) : 0;
          if (prop === "getActiveUniformBlockName") c.ret = blocks[args[1] as number] ?? null;
          if (prop === "getProgramParameter" && args[1] === 0x8a36) c.ret = blocks.length;
          if (prop === "getParameter") {
            const p = args[0] as number;
            if (p === 0x8895) return elements.get(vao) ?? null;
            const t = BINDING_OF.get(p);
            return t === undefined ? null : (bound.get(t) ?? null);
          }
          return c.ret;
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

describe("rows and buffer types as gfxstream keeps them", () => {
  it("draws a window surface into a framebuffer of its own and copies it into its color buffer turned over on a flush", () => {
    const { m, calls } = play([
      [1, "rcCreateContext", [1, 0, 3]],
      [1, "reply", [1]],
      [1, "rcCreateColorBuffer", [16, 8, 0x1908]],
      [1, "reply", [9]],
      [1, "rcCreateWindowSurface", [1, 16, 8]],
      [1, "reply", [3]],
      [1, "rcSetWindowColorBuffer", [3, 9]],
      [1, "rcMakeCurrent", [1, 3, 3]],
      [1, "reply", [1]],
      [1, "glClear", [0x4000]],
      [1, "rcFlushWindowColorBuffer", [3]],
    ]);
    expect(m.stats.failed).toEqual({});
    const cbTex = calls.find((c) => c.name === "createTexture")?.ret;
    // the guest's framebuffer 0 has no color buffer attached: what it draws reaches one at a flush
    const attached = calls.slice(0, calls.findIndex((c) => c.name === "clear")).filter((c) => c.name === "framebufferTexture2D").map((c) => c.args[3]);
    expect(attached.length).toBeGreaterThan(0);
    expect(attached).not.toContain(cbTex);
    const blit = calls.filter((c) => c.name === "blitFramebuffer");
    expect(blit.map((c) => c.args.slice(0, 8))).toEqual([[0, 0, 16, 8, 0, 8, 16, 0]]);
  });

  it("binds a buffer filled as vertices to ELEMENT_ARRAY_BUFFER through a WebGL2 buffer of the element type", () => {
    const { m, calls } = play([...contexts(), [1, "glGenBuffers", [1, 4]], [1, "reply", [5]], [1, "glBindBuffer", [0x8892, 5]], [1, "glBindBuffer", [0x8893, 5]]]);
    expect(m.stats.failed).toEqual({});
    const asArray = calls.filter((c) => c.name === "bindBuffer" && c.args[0] === 0x8892 && c.args[1]).at(-1)?.args[1];
    const asElement = calls.filter((c) => c.name === "bindBuffer" && c.args[0] === 0x8893 && c.args[1]).at(-1)?.args[1];
    expect(asArray).toBeTruthy();
    expect(asElement).toBeTruthy();
    expect(asElement).not.toBe(asArray);
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

/** The id of the blit program's `uv` uniform location, from the getUniformLocation call that made it. */
function uvId(calls: { name: string; args: unknown[]; ret?: unknown }[]): number | undefined {
  return (calls.find((c) => c.name === "getUniformLocation" && c.args[1] === "uv")?.ret as { id?: number } | undefined)?.id;
}

describe("the join snapshot", () => {
  /** A stream that starts with a join: HELLO, BEGIN, color buffers, END, then live records. */
  const joined = (parts: Uint8Array[], live: Uint8Array[] = [], fake: { blocks?: string[] } = {}) => {
    const { gl, calls } = fakeGl(fake);
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
    expect(m.stats.join).toEqual({ colorBuffers: 2, bytes: 16 + 32, malformed: 0, ok: true, pauseUs: 61_000, skipped: 0, gl: null });
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

  it("draws every posted color buffer turned over, the join's and a guest repost alike (row 0 is the screen's top)", () => {
    const uvs = (calls: { name: string; args: unknown[] }[]) => calls.filter((c) => c.name === "uniform4f" && c.args[0] && (c.args[0] as { id?: number }).id === uvId(calls)).map((c) => c.args[2]);
    const post = encodeRecord(RECORD.data, 3, encodeCall("rcFBPost", [9]));
    const { calls } = joined([colorBuffer(9, 2, 2, 0), end(0, 1, 9)], [post]);
    // the post's uv uniform: y origin 1 (turned over) for the END post and the guest's repost
    expect(uvs(calls)).toEqual([1, 1]);
  });

  // The GL state parts: gles-writer.ts's scene (context 0x11 in share group 7 with texture 2,
  // texture 5 showing color buffer 0x40, buffer 1, vertex array 9, program 12 with uniform
  // u_color; context 0x22 sharing group 7, never made current; window surface 0x60), texture
  // 2's pixels (host name 101), and channel 3 with context 0x11 current on surface 0x60.
  const cat = (...bs: Uint8Array[]) => {
    const out = new Uint8Array(bs.reduce((a, b) => a + b.length, 0));
    let at = 0;
    for (const b of bs) {
      out.set(b, at);
      at += b.length;
    }
    return out;
  };
  const glParts = (state = glState({ element: 0 })) => [
    colorBuffer(0x40, 4, 4, 0x33),
    encodeRecord(RECORD.join, 0, state, JOIN_PART.glState),
    encodeRecord(RECORD.join, 0, cat(u32(101), textureBlob({ target: GL.TEXTURE_2D, w: 64, h: 32, storage: 0, maxLevel: 0 })), JOIN_PART.texture),
    encodeRecord(RECORD.join, 0, cat(u32(3), channelBlob()), JOIN_PART.channel),
  ];
  const endGl = (flags = 1) => encodeRecord(RECORD.join, 0, u32(0, 1, 0x40, 0, flags), JOIN_PART.end);
  const live = (ch: number, name: string, args: number[]) => encodeRecord(RECORD.data, ch, encodeCall(name, args));

  /** WebGL2's bindings after the first `end` calls: texture per "unit:target", program, vertex array. */
  const boundAt = (calls: Call[], end: number) => {
    let unit = 0;
    const tex = new Map<string, unknown>();
    let program: unknown = null;
    let vao: unknown = null;
    for (const c of calls.slice(0, end)) {
      if (c.name === "activeTexture") unit = (c.args[0] as number) - 0x84c0;
      else if (c.name === "bindTexture") tex.set(`${unit}:${c.args[0]}`, c.args[1]);
      else if (c.name === "useProgram") program = c.args[0];
      else if (c.name === "bindVertexArray") vao = c.args[0];
    }
    return { tex, program, vao };
  };
  /** The scene's objects as the loader made them in WebGL2. */
  const objects = (calls: Call[]) => {
    const upload = calls.findIndex((c) => c.name === "texImage2D" && c.args[3] === 64 && c.args[4] === 32);
    const pointer = calls.findIndex((c) => c.name === "vertexAttribPointer" && JSON.stringify(c.args) === JSON.stringify([0, 3, GL.FLOAT, false, 12, 8]));
    return {
      // the loader fills textures on unit 15
      texture2: upload >= 0 ? boundAt(calls, upload).tex.get(`15:${GL.TEXTURE_2D}`) : undefined,
      // the color buffer part's texture is the first the mirror made
      colorBuffer: calls.find((c) => c.name === "createTexture")?.ret,
      program12: calls.find((c) => c.name === "getUniformLocation" && c.args[1] === "u_color")?.args[0],
      vao9: pointer >= 0 ? boundAt(calls, pointer).vao : undefined,
    };
  };
  const drawAt = (calls: Call[], count: number) => calls.findIndex((c) => c.name === "drawArrays" && c.args[0] === 4 && c.args[2] === count);

  it("draws on a channel the snapshot made current, with the program, textures and vertex array it had bound", () => {
    const { m, calls } = joined([...glParts(), endGl()], [live(3, "glDrawArrays", [4, 0, 3]), live(3, "glUniform4f", [1, 0, 0, 0, 1])]);
    expect(m.stats.failed).toEqual({});
    expect(m.stats.join?.gl).toMatchObject({
      error: null,
      contexts: 3,
      shareGroups: 2,
      channels: 1,
      surfaces: 1,
      textures: 2,
      programs: 1,
      texturesWithoutPixels: 0,
      clientVertexPointers: 1,
      programsNotLinked: 0,
    });
    const o = objects(calls);
    expect(o.texture2).toBeDefined();
    expect(o.program12).toBeDefined();
    expect(o.vao9).toBeDefined();
    const draw = drawAt(calls, 3);
    expect(draw, "the live draw on channel 3 reached WebGL2").toBeGreaterThanOrEqual(0);
    const at = boundAt(calls, draw);
    expect(at.program, "program 12 in use").toBe(o.program12);
    expect(at.vao, "vertex array 9 bound").toBe(o.vao9);
    expect(at.tex.get(`0:${GL.TEXTURE_2D}`), "texture 2 on unit 0").toBe(o.texture2);
    expect(at.tex.get(`1:${GL.TEXTURE_2D}`), "texture 5, color buffer 0x40's, on unit 1").toBe(o.colorBuffer);
    // the saved uniform value, set by name, and the guest's location 1 for later calls
    const set = calls.find((c) => c.name === "uniform4fv");
    expect(Array.from(set?.args[1] as Float32Array)).toEqual([1, 0.5, 0.25, 1]);
    const locations = calls.filter((c) => c.name === "getUniformLocation" && c.args[1] === "u_color").map((c) => c.ret);
    const uniform = calls.filter((c) => c.name === "uniform4f").pop();
    expect(uniform?.args).toEqual([locations[0], 0, 0, 0, 1]);
  });

  it("gives a second context in the share group the same objects", () => {
    const { m, calls } = joined(
      [...glParts(), endGl()],
      [
        encodeRecord(RECORD.open, 5, new Uint8Array(0)),
        live(5, "rcMakeCurrent", [0x22, 0x60, 0x60]),
        live(5, "glActiveTexture", [0x84c0]),
        live(5, "glBindTexture", [GL.TEXTURE_2D, 2]),
        live(5, "glUseProgram", [12]),
        live(5, "glDrawArrays", [4, 0, 6]),
      ],
    );
    expect(m.stats.failed).toEqual({});
    const o = objects(calls);
    const draw = drawAt(calls, 6);
    expect(draw, "the second context's draw reached WebGL2").toBeGreaterThanOrEqual(0);
    const at = boundAt(calls, draw);
    expect(at.tex.get(`0:${GL.TEXTURE_2D}`)).toBe(o.texture2);
    expect(at.program).toBe(o.program12);
    // vertex arrays are per context: not context 0x11's
    expect(at.vao).not.toBe(o.vao9);
  });

  it("keeps the color buffers alone when the GL state does not parse, and the channel unknown", () => {
    const bad = glState({ element: 0 }).subarray(0, 100);
    const { m, calls } = joined([...glParts(bad), endGl()], [live(3, "glDrawArrays", [4, 0, 3])]);
    expect(m.stats.join?.gl?.error).toMatch(/^GL state: /);
    expect(m.stats.join?.gl?.contexts).toBe(0);
    expect(m.stats.posts).toBe(1);
    expect(drawAt(calls, 3)).toBe(-1);
    expect(m.stats.unknownChannelPackets).toBe(1);
  });

  it("loads no GL state from an END that does not say it was sent", () => {
    const { m, calls } = joined([...glParts(), endGl(0)], [live(3, "glDrawArrays", [4, 0, 3])]);
    expect(m.stats.join?.gl).toBeNull();
    expect(drawAt(calls, 3)).toBe(-1);
  });

  /** The last call before `end` that changed WebGL2's generic binding of `target`, and the buffer it left. */
  const genericAt = (calls: Call[], end: number, target: number) => {
    let last: Call | undefined;
    for (const c of calls.slice(0, end)) if (["bindBuffer", "bindBufferBase", "bindBufferRange"].includes(c.name) && c.args[0] === target) last = c;
    return { call: last?.name, buffer: last?.args.at(last.name === "bindBuffer" ? 1 : 2) };
  };
  const UNIFORM = 0x8a11;

  it("restores a context's generic uniform buffer binding after its indexed ones, which set it too", () => {
    // the scene: indexed uniform binding 0 is buffer 1 (glBindBufferRange), the generic one buffer 25
    const { m, calls } = joined([...glParts(), endGl()], [live(3, "glDrawArrays", [4, 0, 3])]);
    expect(m.stats.failed).toEqual({});
    const draw = drawAt(calls, 3);
    expect(draw).toBeGreaterThanOrEqual(0);
    const range = calls.find((c) => c.name === "bindBufferRange" && c.args[0] === UNIFORM);
    expect(range, "the indexed binding was restored").toBeDefined();
    const g = genericAt(calls, draw, UNIFORM);
    expect(g.call, "the generic binding is set last").toBe("bindBuffer");
    expect(g.buffer).not.toBe(range!.args[2]);
    expect(m.shadowOf(0x11)?.get(`buf:${UNIFORM}`)).toMatch(/^glBindBuffer\[35345,25\]/);
    // a saved generic 0 is restored too, not left at the indexed buffer
    const zero = joined([...glParts(glState({ element: 0, generic: Array(11).fill(0) })), endGl()], [live(3, "glDrawArrays", [4, 0, 3])]);
    const g0 = genericAt(zero.calls, drawAt(zero.calls, 3), UNIFORM);
    expect(g0).toEqual({ call: "bindBuffer", buffer: null });
    expect(zero.m.shadowOf(0x11)?.get(`buf:${UNIFORM}`)).toMatch(/^glBindBuffer\[35345,0\]/);
  });

  /** The last glVertexAttrib* call on attribute `index` before call `end`: its name and values. */
  const attribAt = (calls: Call[], end: number, index: number) => {
    const c = calls
      .slice(0, end)
      .filter((c) => c.name.startsWith("vertexAttrib") && !c.name.includes("Pointer") && !c.name.includes("Divisor") && c.args[0] === index)
      .pop();
    return c ? [c.name, ...c.args.slice(1).map((x) => (ArrayBuffer.isView(x) ? Array.from(x as Float32Array) : x)).flat()] : undefined;
  };

  it("restores a constant attribute saved with one, two or three values, the rest as GLES fills them", () => {
    for (const [saved, want] of [
      [[7], [7, 0, 0, 1]],
      [[7, 8], [7, 8, 0, 1]],
      [[7, 8, 9], [7, 8, 9, 1]],
    ] as const) {
      const { m, calls } = joined([...glParts(glState({ element: 0, attrib1: [...saved] })), endGl()], [live(3, "glDrawArrays", [4, 0, 3])]);
      expect(m.stats.failed).toEqual({});
      expect(attribAt(calls, drawAt(calls, 3), 1), `saved ${saved.join(",")}`).toEqual(["vertexAttrib4f", ...want]);
    }
  });

  it("keeps a restored constant attribute in its own context: another context draws with the default", () => {
    const { m, calls } = joined(
      [...glParts(), endGl()],
      [
        encodeRecord(RECORD.open, 5, new Uint8Array(0)),
        live(5, "rcMakeCurrent", [0x22, 0x60, 0x60]),
        live(5, "glDrawArrays", [4, 0, 6]),
        live(3, "glDrawArrays", [4, 0, 3]),
      ],
    );
    expect(m.stats.failed).toEqual({});
    expect(attribAt(calls, drawAt(calls, 6), 1), "context 0x22 never set attribute 1").toEqual(["vertexAttrib4f", 0, 0, 0, 1]);
    expect(attribAt(calls, drawAt(calls, 3), 1), "context 0x11's saved value, put back").toEqual(["vertexAttrib4f", 1, 2, 3, 4]);
  });

  it("applies saved uniform block bindings only to a block it can name, and counts the rest", () => {
    // two blocks: the snapshot gives host indices, not names, and WebGL2 may number them otherwise
    const two = joined([...glParts(glState({ element: 0, blocks: [[0, 2], [1, 5]] })), endGl()], [live(3, "glUniformBlockBinding", [12, 1, 7])], { blocks: ["B", "A"] });
    expect(two.m.stats.failed).toEqual({});
    expect(two.calls.filter((c) => c.name === "uniformBlockBinding")).toEqual([]);
    expect(two.m.stats.join?.gl?.skipped["uniform block binding the snapshot does not name the block of"]).toBe(2);
    expect(two.m.stats.skipped["glUniformBlockBinding of a block not known by name"]).toBe(1);
    // one block on both sides: host index 0 is that block, for the saved binding and a live one
    const one = joined([...glParts(glState({ element: 0, blocks: [[0, 3]] })), endGl()], [live(3, "glUniformBlockBinding", [12, 0, 4])], { blocks: ["U"] });
    expect(one.calls.filter((c) => c.name === "uniformBlockBinding").map((c) => c.args.slice(1))).toEqual([
      [0, 3],
      [0, 4],
    ]);
    // bindings that are all one value need no names
    const same = joined([...glParts(glState({ element: 0, blocks: [[0, 6], [1, 6]] })), endGl()], [], { blocks: ["B", "A"] });
    expect(same.calls.filter((c) => c.name === "uniformBlockBinding").map((c) => c.args.slice(1))).toEqual([
      [0, 6],
      [1, 6],
    ]);
  });

  it("draws nothing from a snapshot that failed or does not match its header", () => {
    const bad = encodeRecord(RECORD.join, 0, new Uint8Array([...u32(4, 2, 2, 1, 1, 1), 0, 0]), JOIN_PART.colorBuffer);
    const { m } = joined([bad, colorBuffer(9, 2, 2, 0), end(1, 0, 9)]);
    expect(m.stats.join?.malformed).toBe(1);
    expect(m.stats.join?.ok).toBe(false);
    expect(m.stats.posts).toBe(0);
  });
});

describe("live state the shadow keeps", () => {
  const UNIFORM = 0x8a11;
  const clears = (calls: Call[]) => calls.map((c, i) => (c.name === "clear" ? i : -1)).filter((i) => i >= 0);

  it("writes a glCopyBufferSubData into the buffer's element copy too, which a vertex array holds without binding it again", () => {
    const { m, calls } = play([
      ...contexts(),
      [1, "glGenBuffers", [2, 8]],
      [1, "reply", [5, 6]],
      [1, "glGenVertexArrays", [1, 4]],
      [1, "reply", [3]],
      [1, "glBindVertexArray", [3]],
      [1, "glBindBuffer", [0x8892, 5]],
      [1, "glBufferData", [0x8892, 4, new Uint8Array([1, 2, 3, 4]), 0x88e4]],
      // buffer 5 becomes the vertex array's index buffer: WebGL2 gives it a buffer of the element type
      [1, "glBindBuffer", [0x8893, 5]],
      [1, "glBindBuffer", [0x8892, 6]],
      [1, "glBufferData", [0x8892, 1, new Uint8Array([9]), 0x88e4]],
      [1, "glBindBuffer", [0x8f36, 6]],
      [1, "glBindBuffer", [0x8f37, 5]],
      [1, "glCopyBufferSubData", [0x8f36, 0x8f37, 0, 1, 1]],
      [1, "glDrawElementsOffset", [4, 3, 0x1401, 0]],
    ]);
    expect(m.stats.failed).toEqual({});
    const element = calls.filter((c) => c.name === "bindBuffer" && c.args[0] === 0x8893 && c.args[1]).at(-1)?.args[1];
    expect(element).toBeTruthy();
    const copy = calls.findIndex((c) => c.name === "copyBufferSubData");
    const draw = calls.findIndex((c) => c.name === "drawElements");
    expect(copy).toBeGreaterThanOrEqual(0);
    expect(draw).toBeGreaterThan(copy);
    // what COPY_WRITE_BUFFER held at each write between the copy and the draw
    let write: unknown = null;
    const wrote: unknown[] = [];
    for (const c of calls.slice(0, draw)) {
      if (c.name === "bindBuffer" && c.args[0] === 0x8f37) write = c.args[1];
      else if (calls.indexOf(c) >= copy && (c.name === "copyBufferSubData" || c.name === "bufferSubData") && c.args[c.name === "bufferSubData" ? 0 : 1] === 0x8f37) wrote.push(write);
    }
    expect(wrote, "the copied byte reached the element buffer the draw reads").toContain(element);
  });

  it("puts a context's generic uniform buffer binding back after its indexed ones on a switch", () => {
    const { m, calls } = play([
      ...contexts(),
      [1, "glGenBuffers", [3, 12]],
      [1, "reply", [5, 6, 7]],
      [1, "glBindBuffer", [UNIFORM, 5]],
      [1, "glBindBufferBase", [UNIFORM, 0, 6]],
      [1, "glBindBuffer", [UNIFORM, 7]],
      [2, "glClear", [0x4000]],
      [1, "glClear", [0x4000]],
    ]);
    expect(m.stats.failed).toEqual({});
    const generic = calls.filter((c) => c.name === "bindBuffer" && c.args[0] === UNIFORM && c.args[1]);
    const seven = generic[1]?.args[1];
    expect(seven).toBeTruthy();
    const last = clears(calls).at(-1)!;
    const lastBind = calls.slice(0, last).filter((c) => ["bindBuffer", "bindBufferBase", "bindBufferRange"].includes(c.name) && c.args[0] === UNIFORM).pop();
    expect(lastBind?.name).toBe("bindBuffer");
    expect(lastBind?.args[1]).toBe(seven);
  });

  it("keeps each context's constant attribute values, and gives a context that set none the default", () => {
    const attribAt = (calls: Call[], end: number) =>
      calls
        .slice(0, end)
        .filter((c) => /^vertexAttrib[1-4I]/.test(c.name) && !c.name.includes("Pointer") && c.args[0] === 1)
        .pop();
    const { m, calls } = play([
      ...contexts(),
      [1, "glVertexAttrib4f", [1, 1, 2, 3, 4]],
      [2, "glClear", [0x4000]],
      [2, "glVertexAttrib2f", [1, 5, 6]],
      [1, "glClear", [0x4000]],
      [2, "glClear", [0x4000]],
    ]);
    expect(m.stats.failed).toEqual({});
    const [a, b, c] = clears(calls);
    expect(attribAt(calls, a!)?.args, "context 2 never set attribute 1").toEqual([1, 0, 0, 0, 1]);
    expect(attribAt(calls, b!), "context 1's own value").toMatchObject({ name: "vertexAttrib4f", args: [1, 1, 2, 3, 4] });
    expect(attribAt(calls, c!), "context 2's own value").toMatchObject({ name: "vertexAttrib2f", args: [1, 5, 6] });
  });

  it("does not pass texture swizzles to WebGL2, which has none, and counts them", () => {
    const { m, calls } = play([...contexts(), [1, "glTexParameteri", [0x0de1, 0x8e42, 0x1903]], [1, "glTexParameteri", [0x0de1, 0x2801, 0x2601]]]);
    expect(m.stats.failed).toEqual({});
    expect(calls.filter((c) => c.name === "texParameteri").map((c) => c.args[1])).toEqual([0x2801]);
    expect(m.stats.skipped["glTexParameteri texture swizzle"]).toBe(1);
  });

  it("draws a solid colour layer with no guest texture on unit 0, where a comparing depth texture would make WebGL2 refuse the draw", () => {
    const layer = new DataView(new ArrayBuffer(56));
    layer.setUint32(0, 0, true);
    layer.setInt32(4, 3, true); // solid colour
    layer.setInt32(16, 16, true);
    layer.setInt32(20, 16, true);
    layer.setInt32(40, 1, true);
    layer.setFloat32(44, 1, true);
    const buf = new Uint8Array([...u32(1, 9, 1), ...new Uint8Array(layer.buffer)]);
    const { m, calls } = play([
      ...contexts(),
      [1, "rcCreateColorBuffer", [16, 16, 0x1908]],
      [1, "reply", [9]],
      [1, "glGenTextures", [1, 4]],
      [1, "reply", [5]],
      [1, "glActiveTexture", [0x84c0]],
      [1, "glBindTexture", [0x0de1, 5]],
      [1, "rcCompose", [buf.length, buf]],
    ]);
    expect(m.stats.failed).toEqual({});
    const draw = calls.findIndex((c) => c.name === "drawArrays");
    expect(draw).toBeGreaterThanOrEqual(0);
    let unit = 0;
    let onUnit0: unknown = "never bound";
    for (const c of calls.slice(0, draw)) {
      if (c.name === "activeTexture") unit = (c.args[0] as number) - 0x84c0;
      else if (c.name === "bindTexture" && unit === 0 && c.args[0] === 0x0de1) onUnit0 = c.args[1];
    }
    expect(onUnit0).toBeNull();
  });
});
