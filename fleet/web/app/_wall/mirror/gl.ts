// The GPU mirror's renderer: the phone's GL command stream (core/mirror/wire.ts) replayed on one
// WebGL2 context. The phone's T4 answers every call; this only replays, reading object names
// from the server's replies (docs/fleet/ARCHITECTURE.md, "The GPU mirror").
//
// WebGL2 has no share groups, so every guest context lives in the one WebGL2 context: objects
// are shared by construction, names are kept per share group, and each guest context's state
// is a shadow that is put back into WebGL2 when a channel's context differs from the last one
// that drew (`Shadow`). A guest context's default framebuffer is its draw surface's
// framebuffer, and its vertex array 0 is a vertex array of its own.

import { decodeCall, JOIN_PART, joinColorBuffer, joinEnd, PacketSplitter, RECORD, type Arg, type Call, type TapRecord } from "../../../core/mirror/wire.ts";
import { RateMeter } from "../../../core/meter.ts";

type GL = WebGL2RenderingContext;

const GL_TEXTURE_2D = 0x0de1;
const GL_TEXTURE_EXTERNAL_OES = 0x8d65;
const GL_TEXTURE0 = 0x84c0;
const GL_ARRAY_BUFFER = 0x8892;
const GL_ELEMENT_ARRAY_BUFFER = 0x8893;
const GL_FRAMEBUFFER = 0x8d40;
const GL_READ_FRAMEBUFFER = 0x8ca8;
const GL_DRAW_FRAMEBUFFER = 0x8ca9;
const GL_RENDERBUFFER = 0x8d41;
const GL_UNIFORM_BUFFER = 0x8a11;
const GL_PRIMITIVE_RESTART_FIXED_INDEX = 0x8d69;
const CAPS = [0x0be2, 0x0b44, 0x0b71, 0x0bd0, 0x8037, 0x809e, 0x80a0, 0x0c11, 0x0b90, 0x8c89];
const TEXTURE_UNITS = 16;
const RASTER = /^gl(Draw|Clear$|ClearBuffer|BlitFramebuffer)/;

/** What the mirror has drawn, for the panel's readout and the tests. */
export interface MirrorStats {
  records: number;
  dataBytes: number;
  replies: number;
  calls: number;
  posts: number;
  /** Calls by name the mirror does not draw (it skips each and carries on). */
  skipped: Record<string, number>;
  /** Calls that failed in WebGL2, by name. */
  failed: Record<string, number>;
  /** Packets on channels whose state predates the copy, which the mirror cannot draw. */
  unknownChannelPackets: number;
  overflowed: boolean;
  /** Color buffers the host destroyed (FREE records) that the mirror let go of. */
  freed: number;
  /** The join snapshot the stream began with (JOIN records), or null for a stream without one. */
  join: JoinStats | null;
}

export interface JoinStats {
  colorBuffers: number;
  /** Pixel bytes the snapshot carried. */
  bytes: number;
  /** Parts the mirror could not read (a size that does not match its header). */
  malformed: number;
  /** From the END part: whether the snapshot is whole, the phone's pause for it, and the color buffers it could not read. */
  ok: boolean | null;
  pauseUs: number | null;
  skipped: number | null;
}

interface ShareGroup {
  /** Tells share groups apart in the state shadow: one name in two groups is two objects. */
  id: number;
  textures: Map<number, WebGLTexture>;
  buffers: Map<number, WebGLBuffer>;
  renderbuffers: Map<number, WebGLRenderbuffer | ColorBuffer>;
  samplers: Map<number, WebGLSampler>;
  shaders: Map<number, WebGLShader>;
  programs: Map<number, Program>;
}

interface Program {
  p: WebGLProgram;
  locations: Map<number, WebGLUniformLocation | null>;
  blocks: Map<number, string>;
}

interface Surface {
  fbo: WebGLFramebuffer;
  depth: WebGLRenderbuffer;
  w: number;
  h: number;
  cb: ColorBuffer | null;
}

export interface ColorBuffer {
  handle: number;
  tex: WebGLTexture;
  w: number;
  h: number;
}

/** One guest context: its share group, its stand-ins for vertex array 0 and its framebuffers, and its state. */
interface Ctx {
  handle: number;
  share: ShareGroup;
  vao0: WebGLVertexArrayObject;
  vaos: Map<number, WebGLVertexArrayObject>;
  fbos: Map<number, WebGLFramebuffer>;
  tfbs: Map<number, WebGLTransformFeedback>;
  queries: Map<number, WebGLQuery>;
  draw: Surface | null;
  read: Surface | null;
  shadow: Shadow;
  /** Guest names bound now, for the calls that act on "the bound one". */
  boundFbo: { draw: number; read: number };
  boundRbo: number;
  boundTex: Map<string, number>;
  activeUnit: number;
  program: number;
  /** The guest's vertex array bound now (0: the context's own stand-in). */
  vao: number;
  arrayBuffer: number;
  scratch: Map<number, WebGLBuffer>;
}

/** One setting of a guest context: how to put it into WebGL2, and its value. */
interface Setting {
  value: string;
  apply: () => void;
}

/** A guest context's settable state, as the guest last set it. */
class Shadow {
  readonly settings = new Map<string, Setting>();
  set(key: string, value: string, apply: () => void): void {
    this.settings.set(key, { value, apply });
  }
}

interface Channel {
  id: number;
  /** Opened after the copy started (or loaded from a join snapshot): its state is known. */
  known: boolean;
  split: PacketSplitter;
  ctx: Ctx | null;
  /** Calls answered by a reply not yet seen, oldest first. */
  waiting: { call: Call; then: ((reply: Uint8Array) => void) | null }[];
}

type Handler = (c: Channel, a: Arg[], call: Call) => void;
type ReplyHandler = (c: Channel, a: Arg[], reply: Uint8Array) => void;

const u32s = (b: Uint8Array) => new Uint32Array(b.buffer.slice(b.byteOffset, b.byteOffset + (b.byteLength & ~3)));
const num = (a: Arg | undefined) => (typeof a === "number" ? a : typeof a === "bigint" ? Number(a) : 0);
const bytes = (a: Arg | undefined) => (a instanceof Uint8Array ? a : new Uint8Array(0));
const text = (b: Uint8Array) => new TextDecoder().decode(b[b.length - 1] === 0 ? b.subarray(0, b.length - 1) : b);

/**
 * A composition layer's HWC transform as the affine map of its quad's corner q (0..1, x right,
 * y with the display's top at 0) to the texture coordinate it samples, before the crop:
 * [m00, m01, m10, m11, ox, oy], coord = (m00 q.x + m01 q.y + ox, m10 q.x + m11 q.y + oy).
 * gfxstream's TextureDraw draws each transform from a vertex table (host/gl/texture_draw.cpp,
 * kVertices); these are the same eight maps. FLIP_H 1, FLIP_V 2, ROT_90 4, ROT_180 3,
 * ROT_270 7, FLIP_H_ROT_90 5, FLIP_V_ROT_90 6; anything else draws untransformed, as there.
 */
export function layerTransform(t: number): [number, number, number, number, number, number] {
  switch (t) {
    case 4:
      return [0, -1, 1, 0, 1, 0]; // (1 - y, x)
    case 3:
      return [-1, 0, 0, -1, 1, 1]; // (1 - x, 1 - y)
    case 7:
      return [0, 1, -1, 0, 0, 1]; // (y, 1 - x)
    case 1:
      return [-1, 0, 0, 1, 1, 0]; // (1 - x, y)
    case 2:
      return [1, 0, 0, -1, 0, 1]; // (x, 1 - y)
    case 5:
      return [0, 1, 1, 0, 0, 0]; // (y, x)
    case 6:
      return [0, -1, -1, 0, 1, 1]; // (1 - y, 1 - x)
    default:
      return [1, 0, 0, 1, 0, 0];
  }
}

/** A guest shader in GLSL ES that WebGL2 compiles: external samplers become 2D samplers. */
export function webglShaderSource(src: string): string {
  return src
    .replace(/^[ \t]*#extension[ \t]+GL_OES_EGL_image_external(_essl3)?[^\n]*$/gm, "")
    .replace(/\bsamplerExternalOES\b/g, "sampler2D");
}

export class GlMirror {
  readonly stats: MirrorStats = {
    records: 0,
    dataBytes: 0,
    replies: 0,
    calls: 0,
    posts: 0,
    skipped: {},
    failed: {},
    unknownChannelPackets: 0,
    overflowed: false,
    freed: 0,
    join: null,
  };
  /** Replies the phone's GPU sent per second: the round trips the server answered. */
  readonly replyRate = new RateMeter();
  /** Frames posted to the screen per second. */
  readonly postRate = new RateMeter();
  /** Called with each color buffer posted to the screen (after it is drawn to the canvas). */
  onPost: ((cb: ColorBuffer, timeUs: number) => void) | null = null;
  /**
   * The replay test's fast-forward: every call but those that rasterize (draws, clears, blits).
   * Only for a stream whose frames are each drawn whole, as Robot Park's are; the page never sets it.
   */
  skipDraws = false;
  /** The phone's time of the record being fed (tap microseconds). */
  timeUs = 0;

  private readonly channels = new Map<number, Channel>();
  private readonly contexts = new Map<number, Ctx>();
  private readonly colorBuffers = new Map<number, ColorBuffer>();
  private readonly surfaces = new Map<number, Surface>();
  /** Per handle, FREE records still to come for buffers a create with that handle replaced. */
  private readonly staleFrees = new Map<number, number>();
  private readonly images = new Map<number, WebGLTexture>();
  private current: Ctx | null = null;
  /**
   * The mirror's own drawing (composition, posts, uploads) changed GL state behind the
   * contexts' shadows: the next context to draw puts all of its state back, and every other
   * setting a context ever made back to its default.
   */
  private dirty = false;
  /** Every setting key any context has made, for the defaults a dirty switch restores. */
  private readonly allKeys = new Set<string>();
  private nextShareId = 1;
  private readonly handlers = new Map<string, Handler>();
  private readonly replyHandlers = new Map<string, ReplyHandler>();
  private blit: {
    prog: WebGLProgram;
    vao: WebGLVertexArrayObject;
    uSrc: WebGLUniformLocation | null;
    uRect: WebGLUniformLocation | null;
    uUv: WebGLUniformLocation | null;
    uColor: WebGLUniformLocation | null;
    uMode: WebGLUniformLocation | null;
    uXm: WebGLUniformLocation | null;
    uXo: WebGLUniformLocation | null;
  } | null = null;
  private readonly nowMs: () => number;
  private readonly gl: GL;

  constructor(gl: GL, nowMs: () => number) {
    this.gl = gl;
    this.nowMs = nowMs;
    this.install();
  }

  /** One tap record. */
  feed(r: TapRecord): void {
    this.stats.records++;
    this.timeUs = r.timeUs;
    if (r.type === RECORD.hello) {
      for (const id of u32s(r.payload.subarray(4))) this.channel(id, false);
      return;
    }
    if (r.type === RECORD.open) {
      this.channels.set(r.channel, { id: r.channel, known: r.aux === 0, split: new PacketSplitter(), ctx: null, waiting: [] });
      return;
    }
    if (r.type === RECORD.close) {
      this.channels.delete(r.channel);
      return;
    }
    if (r.type === RECORD.overflow) {
      this.stats.overflowed = true;
      return;
    }
    if (r.type === RECORD.free) {
      this.hostFreed(r.aux);
      return;
    }
    if (r.type === RECORD.join) {
      this.join(r);
      return;
    }
    const c = this.channel(r.channel, false);
    if (r.type === RECORD.reply) {
      this.stats.replies++;
      this.replyRate.add(this.nowMs(), 1);
      const w = c.waiting.shift();
      if (w?.then) this.guard(w.call.name, () => w.then!(r.payload));
      return;
    }
    if (r.type !== RECORD.data) return;
    this.stats.dataBytes += r.payload.length;
    let packets: Uint8Array[];
    try {
      packets = c.split.push(r.payload);
    } catch {
      // not at a packet boundary: a channel older than the copy whose first bytes were lost
      c.split = new PacketSplitter();
      c.known = false;
      return;
    }
    for (const p of packets) this.packet(c, p);
  }

  /**
   * One part of the join snapshot: the phone's state when the copy started. Its color buffers
   * (every layer the composer can draw, and the screen) are made here with their pixels, and
   * the screen is drawn at its END, so a phone that draws nothing new is shown at once.
   */
  private join(r: TapRecord): void {
    const j = (this.stats.join ??= { colorBuffers: 0, bytes: 0, malformed: 0, ok: null, pauseUs: null, skipped: null });
    if (r.aux === JOIN_PART.colorBuffer) {
      const p = joinColorBuffer(r.payload);
      if (!p) {
        j.malformed++;
        return;
      }
      // a snapshot names each handle once; a repeat replaces the first
      this.freeColorBuffer(p.handle);
      const cb = this.newColorBuffer(p.handle, p.width, p.height);
      this.withTexture(cb.tex, () => this.upload(0, 0, p.width, p.height, 0x1908, 0x1401, p.pixels));
      j.colorBuffers++;
      j.bytes += p.pixels.length;
    } else if (r.aux === JOIN_PART.end) {
      const e = joinEnd(r.payload);
      if (!e) {
        j.malformed++;
        return;
      }
      j.ok = e.status === 0;
      j.pauseUs = e.pauseUs;
      j.skipped = e.skipped;
      if (j.ok && e.lastPosted) this.post(e.lastPosted);
    }
  }

  private channel(id: number, known: boolean): Channel {
    let c = this.channels.get(id);
    if (!c) {
      c = { id, known, split: new PacketSplitter(), ctx: null, waiting: [] };
      this.channels.set(id, c);
    }
    return c;
  }

  private guard(name: string, f: () => void): void {
    try {
      f();
    } catch {
      this.stats.failed[name] = (this.stats.failed[name] ?? 0) + 1;
    }
  }

  private packet(c: Channel, p: Uint8Array): void {
    const d = decodeCall(p);
    if ("unknown" in d) {
      this.skip(`op ${d.op}`);
      return;
    }
    this.stats.calls++;
    // renderControl decodes on any channel; GL needs the channel's context, which an older
    // channel's state does not give
    const rc = d.name.startsWith("rc");
    if (!rc && (!c.known || !c.ctx)) {
      this.stats.unknownChannelPackets++;
      if (d.replies) c.waiting.push({ call: d, then: null });
      return;
    }
    if (!rc) this.makeCurrent(c.ctx!);
    const onReply = this.replyHandlers.get(d.name);
    if (d.replies) c.waiting.push({ call: d, then: onReply ? (reply) => onReply(c, d.args, reply) : null });
    const h = this.handlers.get(d.name);
    if (this.skipDraws && RASTER.test(d.name)) return;
    if (h) this.guard(d.name, () => h(c, d.args, d));
    else if (!onReply) this.skip(d.name);
  }

  private skip(name: string): void {
    this.stats.skipped[name] = (this.stats.skipped[name] ?? 0) + 1;
  }

  // ---- contexts ------------------------------------------------------------------------

  private newShare(): ShareGroup {
    return {
      id: this.nextShareId++,
      textures: new Map(),
      buffers: new Map(),
      renderbuffers: new Map(),
      samplers: new Map(),
      shaders: new Map(),
      programs: new Map(),
    };
  }

  /** Puts `ctx`'s state into WebGL2: every setting that differs from what is there now. */
  private makeCurrent(ctx: Ctx): void {
    if (this.current === ctx && !this.dirty) return;
    // after the mirror's own drawing nothing in GL is any context's: compare with nothing
    const prev = this.dirty ? null : this.current;
    this.current = ctx;
    this.dirty = false;
    const gl = this.gl;
    const next = ctx.shadow.settings;
    // settings this context never made go back to their defaults: those the previous context
    // made, or with no previous context to trust, every one any context made
    for (const k of prev ? prev.shadow.settings.keys() : this.allKeys) if (!next.has(k)) this.defaultOf(k)?.();
    let unit: Setting | undefined;
    for (const [k, s] of next) {
      if (k === "activeTexture") {
        unit = s;
        continue;
      }
      if (prev?.shadow.settings.get(k)?.value === s.value) continue;
      s.apply();
    }
    // per-context objects that WebGL2 holds globally
    gl.bindVertexArray(this.vaoOf(ctx, ctx.vao));
    gl.bindFramebuffer(GL_DRAW_FRAMEBUFFER, this.fboOf(ctx, ctx.boundFbo.draw, true));
    gl.bindFramebuffer(GL_READ_FRAMEBUFFER, this.fboOf(ctx, ctx.boundFbo.read, false));
    if (unit) unit.apply();
    else gl.activeTexture(GL_TEXTURE0);
  }

  private defaultOf(key: string): (() => void) | null {
    const gl = this.gl;
    const [kind, a, b] = key.split(":");
    switch (kind) {
      case "cap":
        return () => (Number(a) === 0x0bd0 ? gl.enable(0x0bd0) : gl.disable(Number(a)));
      case "buf":
        return () => gl.bindBuffer(Number(a), null);
      case "tex":
        return () => {
          gl.activeTexture(GL_TEXTURE0 + Number(a));
          gl.bindTexture(Number(b), null);
        };
      case "sampler":
        return () => gl.bindSampler(Number(a), null);
      case "program":
        return () => gl.useProgram(null);
      case "rbo":
        return () => gl.bindRenderbuffer(GL_RENDERBUFFER, null);
      case "blendColor":
        return () => gl.blendColor(0, 0, 0, 0);
      case "blendEq":
        return () => gl.blendEquation(0x8006);
      case "blendFunc":
        return () => gl.blendFunc(1, 0);
      case "colorMask":
        return () => gl.colorMask(true, true, true, true);
      case "cullFace":
        return () => gl.cullFace(0x0405);
      case "depthFunc":
        return () => gl.depthFunc(0x0201);
      case "depthMask":
        return () => gl.depthMask(true);
      case "depthRange":
        return () => gl.depthRange(0, 1);
      case "frontFace":
        return () => gl.frontFace(0x0901);
      case "lineWidth":
        return () => gl.lineWidth(1);
      case "polygonOffset":
        return () => gl.polygonOffset(0, 0);
      case "sampleCoverage":
        return () => gl.sampleCoverage(1, false);
      case "stencilFunc":
        return () => gl.stencilFuncSeparate(Number(a), 0x0207, 0, 0xffffffff);
      case "stencilOp":
        return () => gl.stencilOpSeparate(Number(a), 0x1e00, 0x1e00, 0x1e00);
      case "stencilMask":
        return () => gl.stencilMaskSeparate(Number(a), 0xffffffff);
      case "clearColor":
        return () => gl.clearColor(0, 0, 0, 0);
      case "clearDepth":
        return () => gl.clearDepth(1);
      case "clearStencil":
        return () => gl.clearStencil(0);
      case "pixelStore":
        return () => gl.pixelStorei(Number(a), Number(a) === 0x0cf5 || Number(a) === 0x0d05 ? 4 : 0);
      case "ubo":
        return () => gl.bindBufferBase(GL_UNIFORM_BUFFER, Number(a), null);
      case "tfb":
        return () => gl.bindTransformFeedback(0x8e22, null);
      default:
        // scissor and viewport are always set by a context before it draws; vao and fbo are
        // put back by makeCurrent
        return null;
    }
  }

  private vaoOf(ctx: Ctx, name: number): WebGLVertexArrayObject {
    if (name === 0) return ctx.vao0;
    let v = ctx.vaos.get(name);
    if (!v) {
      v = this.gl.createVertexArray()!;
      ctx.vaos.set(name, v);
    }
    return v;
  }

  private fboOf(ctx: Ctx, name: number, draw: boolean): WebGLFramebuffer | null {
    if (name === 0) return (draw ? ctx.draw : ctx.read)?.fbo ?? null;
    let f = ctx.fbos.get(name);
    if (!f) {
      f = this.gl.createFramebuffer()!;
      ctx.fbos.set(name, f);
    }
    return f;
  }

  private tex(ctx: Ctx, name: number): WebGLTexture | null {
    if (name === 0) return null;
    let t = ctx.share.textures.get(name);
    if (!t) {
      t = this.gl.createTexture()!;
      ctx.share.textures.set(name, t);
    }
    return t;
  }

  private buf(ctx: Ctx, name: number): WebGLBuffer | null {
    if (name === 0) return null;
    let b = ctx.share.buffers.get(name);
    if (!b) {
      b = this.gl.createBuffer()!;
      ctx.share.buffers.set(name, b);
    }
    return b;
  }

  private prog(ctx: Ctx, name: number): Program | null {
    return name === 0 ? null : (ctx.share.programs.get(name) ?? null);
  }

  /** WebGL2 has no external target: it is a 2D texture here, as the shaders are rewritten. */
  private target(t: number): number {
    return t === GL_TEXTURE_EXTERNAL_OES ? GL_TEXTURE_2D : t;
  }

  // ---- handlers ------------------------------------------------------------------------

  private on(name: string, h: Handler): void {
    this.handlers.set(name, h);
  }

  private onReply(name: string, h: ReplyHandler): void {
    this.replyHandlers.set(name, h);
  }

  /** A call that only sets state: applied now and kept in the context's shadow. */
  private setting(name: string, key: (a: Arg[], ctx: Ctx) => string, apply: (a: Arg[], ctx: Ctx) => void): void {
    this.on(name, (c, a) => {
      const ctx = c.ctx!;
      const k = key(a, ctx);
      const run = () => apply(a, ctx);
      run();
      this.shadowSet(ctx, k, name, a, run);
    });
  }

  /**
   * Keeps a setting in the context's shadow. Its value names the call and the share group as
   * well as the arguments: glEnable(BLEND) and glDisable(BLEND) are one key with two values,
   * and buffer 1 of two share groups is two buffers.
   */
  private shadowSet(ctx: Ctx, key: string, call: string, args: readonly (Arg | number)[], apply: () => void): void {
    const v = args.map((x) => (x instanceof Uint8Array ? Array.from(x) : typeof x === "bigint" ? x.toString() : x));
    ctx.shadow.set(key, `${call}${JSON.stringify(v)}@${ctx.share.id}`, apply);
    this.allKeys.add(key);
  }

  /** For the tests: the state shadow's key and value set (key -> value) of a context. */
  shadowOf(handle: number): Map<string, string> | null {
    const c = this.contexts.get(handle);
    return c ? new Map([...c.shadow.settings].map(([k, s]) => [k, s.value])) : null;
  }

  private install(): void {
    const gl = this.gl;
    const n = num;

    // renderControl: contexts, surfaces, color buffers, composition, posts
    this.onReply("rcCreateContext", (_c, a, r) => {
      const handle = new DataView(r.buffer, r.byteOffset).getUint32(0, true);
      const share = this.contexts.get(n(a[1]))?.share ?? this.newShare();
      this.contexts.set(handle, {
        handle,
        share,
        vao0: gl.createVertexArray()!,
        vaos: new Map(),
        fbos: new Map(),
        tfbs: new Map(),
        queries: new Map(),
        draw: null,
        read: null,
        shadow: new Shadow(),
        boundFbo: { draw: 0, read: 0 },
        boundRbo: 0,
        boundTex: new Map(),
        activeUnit: 0,
        program: 0,
        vao: 0,
        arrayBuffer: 0,
        scratch: new Map(),
      });
    });
    this.on("rcDestroyContext", (_c, a) => {
      const ctx = this.contexts.get(n(a[0]));
      if (ctx && this.current === ctx) this.current = null;
      this.contexts.delete(n(a[0]));
    });
    const makeCurrent: Handler = (c, a) => {
      const ctx = this.contexts.get(n(a[0])) ?? null;
      c.ctx = ctx;
      if (ctx) {
        ctx.draw = this.surfaces.get(n(a[1])) ?? null;
        ctx.read = this.surfaces.get(n(a[2])) ?? null;
        // a context made current again on another surface rebinds its framebuffer 0
        if (this.current === ctx) this.dirty = true;
      }
    };
    this.on("rcMakeCurrent", makeCurrent);
    this.on("rcMakeCurrentAsync", makeCurrent);
    this.onReply("rcCreateWindowSurface", (_c, a, r) => {
      const handle = new DataView(r.buffer, r.byteOffset).getUint32(0, true);
      const w = n(a[1]);
      const h = n(a[2]);
      const fbo = gl.createFramebuffer()!;
      const depth = gl.createRenderbuffer()!;
      gl.bindRenderbuffer(GL_RENDERBUFFER, depth);
      gl.renderbufferStorage(GL_RENDERBUFFER, 0x88f0, Math.max(1, w), Math.max(1, h));
      this.restoreRbo();
      this.surfaces.set(handle, { fbo, depth, w, h, cb: null });
    });
    this.on("rcDestroyWindowSurface", (_c, a) => {
      const s = this.surfaces.get(n(a[0]));
      if (!s) return;
      this.surfaces.delete(n(a[0]));
      gl.deleteFramebuffer(s.fbo);
      gl.deleteRenderbuffer(s.depth);
      this.release(s.cb?.tex);
    });
    this.on("rcSetWindowColorBuffer", (_c, a) => {
      const s = this.surfaces.get(n(a[0]));
      const cb = this.colorBuffers.get(n(a[1]));
      if (!s || !cb) return;
      const old = s.cb;
      s.cb = cb;
      this.withFbo(s.fbo, () => {
        gl.framebufferTexture2D(GL_FRAMEBUFFER, 0x8ce0, GL_TEXTURE_2D, cb.tex, 0);
        if (s.w === cb.w && s.h === cb.h) gl.framebufferRenderbuffer(GL_FRAMEBUFFER, 0x821a, GL_RENDERBUFFER, s.depth);
      });
      if (old !== cb) this.release(old?.tex);
    });
    // the surface draws straight into its color buffer: nothing to copy on a flush
    for (const f of ["rcFlushWindowColorBuffer", "rcFlushWindowColorBufferAsync", "rcFlushWindowColorBufferAsyncWithFrameNumber"]) this.on(f, () => {});
    const newColorBuffer = (handle: number, w: number, h: number) => void this.newColorBuffer(handle, w, h);
    this.onReply("rcCreateColorBuffer", (_c, a, r) => newColorBuffer(new DataView(r.buffer, r.byteOffset).getUint32(0, true), n(a[0]), n(a[1])));
    this.onReply("rcCreateColorBufferDMA", (_c, a, r) => newColorBuffer(new DataView(r.buffer, r.byteOffset).getUint32(0, true), n(a[0]), n(a[1])));
    // The guest names the handle. The tap copies the call before the host runs it, and the host
    // destroys an old buffer with that handle while it runs it, so the old buffer's FREE comes
    // after this create: that one FREE is the old buffer's.
    this.on("rcCreateColorBufferWithHandle", (_c, a) => {
      const handle = n(a[3]);
      if (this.colorBuffers.has(handle)) {
        this.freeColorBuffer(handle);
        this.staleFrees.set(handle, (this.staleFrees.get(handle) ?? 0) + 1);
      }
      newColorBuffer(handle, n(a[0]), n(a[1]));
    });
    // A color buffer lives until the host destroys it (a FREE record): the guest's open and
    // close calls are only part of its count (the refcount pipe and process cleanup are the
    // rest), and the host may keep a closed one for a second in case it is opened again.
    for (const f of ["rcOpenColorBuffer", "rcOpenColorBuffer2", "rcCloseColorBuffer"]) this.on(f, () => {});
    this.on("rcUpdateColorBuffer", (_c, a) => {
      const cb = this.colorBuffers.get(n(a[0]));
      if (!cb) return;
      this.withTexture(cb.tex, () => this.upload(n(a[1]), n(a[2]), n(a[3]), n(a[4]), n(a[5]), n(a[6]), bytes(a[7])));
    });
    this.on("rcBindTexture", (c, a) => {
      const ctx = c.ctx;
      const cb = this.colorBuffers.get(n(a[0]));
      if (!ctx || !cb) return;
      // the guest's bound texture now names the color buffer (an EGLImage of a gralloc buffer)
      const unit = ctx.activeUnit;
      for (const t of [GL_TEXTURE_2D, GL_TEXTURE_EXTERNAL_OES]) {
        const name = ctx.boundTex.get(`${unit}:${t}`);
        if (name) {
          const old = ctx.share.textures.get(name);
          ctx.share.textures.set(name, cb.tex);
          if (old !== cb.tex) this.release(old);
          this.makeCurrent(ctx);
          gl.bindTexture(GL_TEXTURE_2D, cb.tex);
        }
      }
    });
    this.on("rcBindRenderbuffer", (c, a) => {
      const ctx = c.ctx;
      const cb = this.colorBuffers.get(n(a[0]));
      if (!ctx || !cb || !ctx.boundRbo) return;
      const old = ctx.share.renderbuffers.get(ctx.boundRbo);
      ctx.share.renderbuffers.set(ctx.boundRbo, cb);
      if (old instanceof WebGLRenderbuffer) gl.deleteRenderbuffer(old);
      else if (old !== cb) this.release(old?.tex);
    });
    this.onReply("rcCreateClientImage", (c, a, r) => {
      const ctx = this.contexts.get(n(a[0])) ?? c.ctx;
      const t = ctx?.share.textures.get(n(a[2]));
      if (t) this.images.set(new DataView(r.buffer, r.byteOffset).getUint32(0, true), t);
    });
    this.on("rcDestroyClientImage", (_c, a) => {
      const t = this.images.get(n(a[0]));
      this.images.delete(n(a[0]));
      // the image may have been the last thing drawing with a freed buffer's texture
      this.release(t);
    });
    this.on("rcFBPost", (_c, a, call) => this.post(n(a[0]), call));
    const compose = (post: boolean): Handler => (_c, a, call) => this.compose(bytes(a[1]), post, call);
    this.on("rcCompose", compose(true));
    this.on("rcComposeAsync", compose(true));
    this.on("rcComposeWithoutPost", compose(false));
    this.on("rcComposeAsyncWithoutPost", compose(false));
    for (const f of [
      "rcSelectChecksumHelper",
      "rcSetPuid",
      "rcCreateSyncKHR",
      "rcClientWaitSyncKHR",
      "rcWaitSyncKHR",
      "rcDestroySyncKHR",
      "rcDestroySyncKHRAsync",
      "rcFBSetSwapInterval",
      "rcSetProcessMetadata",
      "rcColorBufferCacheFlush",
      "rcIsSyncSignaled",
      "rcSetDisplayPose",
      "rcSetDisplayPoseDpi",
      "rcSetDisplayColorBuffer",
      "rcSetDisplayPowerMode",
      "rcSetDisplayColorTransform",
      "rcSetTracingForPuid",
      "rcSetColorBufferVulkanMode",
      "rcSetColorBufferVulkanMode2",
    ])
      this.on(f, () => {});

    // GL: objects named by the server's replies
    const gen = (name: string, make: (ctx: Ctx, id: number) => void) =>
      this.onReply(name, (c, _a, r) => {
        if (!c.ctx) return;
        for (const id of u32s(r)) if (id) make(c.ctx, id);
      });
    gen("glGenTextures", (ctx, id) => this.tex(ctx, id));
    gen("glGenBuffers", (ctx, id) => this.buf(ctx, id));
    gen("glGenRenderbuffers", (ctx, id) => ctx.share.renderbuffers.set(id, gl.createRenderbuffer()!));
    gen("glGenFramebuffers", (ctx, id) => this.fboOf(ctx, id, true));
    gen("glGenVertexArrays", (ctx, id) => this.vaoOf(ctx, id));
    gen("glGenVertexArraysOES", (ctx, id) => this.vaoOf(ctx, id));
    gen("glGenSamplers", (ctx, id) => ctx.share.samplers.set(id, gl.createSampler()!));
    gen("glGenTransformFeedbacks", (ctx, id) => ctx.tfbs.set(id, gl.createTransformFeedback()!));
    gen("glGenQueries", (ctx, id) => ctx.queries.set(id, gl.createQuery()!));
    this.onReply("glCreateShader", (c, a, r) => {
      const id = new DataView(r.buffer, r.byteOffset).getUint32(0, true);
      const s = gl.createShader(n(a[0]));
      if (c.ctx && s && id) c.ctx.share.shaders.set(id, s);
    });
    this.onReply("glCreateProgram", (c, _a, r) => {
      const id = new DataView(r.buffer, r.byteOffset).getUint32(0, true);
      const p = gl.createProgram();
      if (c.ctx && p && id) c.ctx.share.programs.set(id, { p, locations: new Map(), blocks: new Map() });
    });
    this.onReply("glGetUniformLocation", (c, a, r) => {
      const p = c.ctx && this.prog(c.ctx, n(a[0]));
      const loc = new DataView(r.buffer, r.byteOffset).getInt32(0, true);
      if (p && loc >= 0) p.locations.set(loc, gl.getUniformLocation(p.p, text(bytes(a[1]))));
    });
    this.onReply("glGetUniformBlockIndex", (c, a, r) => {
      const p = c.ctx && this.prog(c.ctx, n(a[0]));
      const idx = new DataView(r.buffer, r.byteOffset).getUint32(0, true);
      if (p) p.blocks.set(idx, text(bytes(a[1])));
    });
    const del = (name: string, f: (ctx: Ctx, id: number) => void) =>
      this.on(name, (c, a) => {
        for (const id of u32s(bytes(a[1]))) if (id) f(c.ctx!, id);
      });
    del("glDeleteTextures", (ctx, id) => {
      const t = ctx.share.textures.get(id);
      ctx.share.textures.delete(id);
      // a texture that names a color buffer is the color buffer's, which outlives it, and
      // another name may draw with it too
      this.release(t);
    });
    del("glDeleteBuffers", (ctx, id) => {
      gl.deleteBuffer(ctx.share.buffers.get(id) ?? null);
      ctx.share.buffers.delete(id);
    });
    del("glDeleteRenderbuffers", (ctx, id) => {
      const r = ctx.share.renderbuffers.get(id);
      ctx.share.renderbuffers.delete(id);
      if (r instanceof WebGLRenderbuffer) gl.deleteRenderbuffer(r);
      else this.release(r?.tex);
    });
    del("glDeleteFramebuffers", (ctx, id) => {
      gl.deleteFramebuffer(ctx.fbos.get(id) ?? null);
      ctx.fbos.delete(id);
      if (ctx.boundFbo.draw === id) ctx.boundFbo.draw = 0;
      if (ctx.boundFbo.read === id) ctx.boundFbo.read = 0;
    });
    del("glDeleteVertexArrays", (ctx, id) => {
      gl.deleteVertexArray(ctx.vaos.get(id) ?? null);
      ctx.vaos.delete(id);
    });
    del("glDeleteVertexArraysOES", (ctx, id) => {
      gl.deleteVertexArray(ctx.vaos.get(id) ?? null);
      ctx.vaos.delete(id);
    });
    del("glDeleteSamplers", (ctx, id) => {
      gl.deleteSampler(ctx.share.samplers.get(id) ?? null);
      ctx.share.samplers.delete(id);
    });
    del("glDeleteQueries", (ctx, id) => {
      gl.deleteQuery(ctx.queries.get(id) ?? null);
      ctx.queries.delete(id);
    });
    this.on("glDeleteShader", (c, a) => {
      const s = c.ctx!.share.shaders.get(n(a[0]));
      if (s) gl.deleteShader(s);
      c.ctx!.share.shaders.delete(n(a[0]));
    });
    this.on("glDeleteProgram", (c, a) => {
      const p = c.ctx!.share.programs.get(n(a[0]));
      if (p) gl.deleteProgram(p.p);
      c.ctx!.share.programs.delete(n(a[0]));
    });

    // shaders and programs
    this.on("glShaderString", (c, a) => {
      const s = c.ctx!.share.shaders.get(n(a[0]));
      if (s) gl.shaderSource(s, webglShaderSource(text(bytes(a[1]))));
    });
    this.on("glCompileShader", (c, a) => {
      const s = c.ctx!.share.shaders.get(n(a[0]));
      if (s) gl.compileShader(s);
    });
    this.on("glAttachShader", (c, a) => {
      const p = this.prog(c.ctx!, n(a[0]));
      const s = c.ctx!.share.shaders.get(n(a[1]));
      if (p && s) gl.attachShader(p.p, s);
    });
    this.on("glDetachShader", (c, a) => {
      const p = this.prog(c.ctx!, n(a[0]));
      const s = c.ctx!.share.shaders.get(n(a[1]));
      if (p && s) gl.detachShader(p.p, s);
    });
    this.on("glBindAttribLocation", (c, a) => {
      const p = this.prog(c.ctx!, n(a[0]));
      if (p) gl.bindAttribLocation(p.p, n(a[1]), text(bytes(a[2])));
    });
    this.on("glLinkProgram", (c, a) => {
      const p = this.prog(c.ctx!, n(a[0]));
      if (!p) return;
      gl.linkProgram(p.p);
      // locations were the old link's; the guest asks again
      p.locations.clear();
    });
    this.on("glTransformFeedbackVaryingsAEMU", (c, a) => {
      const p = this.prog(c.ctx!, n(a[0]));
      if (p) gl.transformFeedbackVaryings(p.p, text(bytes(a[2])).split(";").filter(Boolean), n(a[4]));
    });
    this.setting("glUseProgram", () => "program", (a, ctx) => {
      ctx.program = n(a[0]);
      gl.useProgram(this.prog(ctx, n(a[0]))?.p ?? null);
    });
    this.on("glUniformBlockBinding", (c, a) => {
      const p = this.prog(c.ctx!, n(a[0]));
      const name = p?.blocks.get(n(a[1]));
      if (p && name !== undefined) gl.uniformBlockBinding(p.p, gl.getUniformBlockIndex(p.p, name), n(a[2]));
    });
    const loc = (c: Channel, l: Arg | undefined) => this.prog(c.ctx!, c.ctx!.program)?.locations.get(n(l)) ?? null;
    const floats = (b: Arg | undefined) => {
      const x = bytes(b);
      return new Float32Array(x.buffer.slice(x.byteOffset, x.byteOffset + (x.byteLength & ~3)));
    };
    const ints = (b: Arg | undefined) => {
      const x = bytes(b);
      return new Int32Array(x.buffer.slice(x.byteOffset, x.byteOffset + (x.byteLength & ~3)));
    };
    this.on("glUniform1f", (c, a) => gl.uniform1f(loc(c, a[0]), n(a[1])));
    this.on("glUniform2f", (c, a) => gl.uniform2f(loc(c, a[0]), n(a[1]), n(a[2])));
    this.on("glUniform3f", (c, a) => gl.uniform3f(loc(c, a[0]), n(a[1]), n(a[2]), n(a[3])));
    this.on("glUniform4f", (c, a) => gl.uniform4f(loc(c, a[0]), n(a[1]), n(a[2]), n(a[3]), n(a[4])));
    this.on("glUniform1i", (c, a) => gl.uniform1i(loc(c, a[0]), n(a[1])));
    this.on("glUniform2i", (c, a) => gl.uniform2i(loc(c, a[0]), n(a[1]), n(a[2])));
    this.on("glUniform3i", (c, a) => gl.uniform3i(loc(c, a[0]), n(a[1]), n(a[2]), n(a[3])));
    this.on("glUniform4i", (c, a) => gl.uniform4i(loc(c, a[0]), n(a[1]), n(a[2]), n(a[3]), n(a[4])));
    this.on("glUniform1ui", (c, a) => gl.uniform1ui(loc(c, a[0]), n(a[1])));
    this.on("glUniform2ui", (c, a) => gl.uniform2ui(loc(c, a[0]), n(a[1]), n(a[2])));
    this.on("glUniform3ui", (c, a) => gl.uniform3ui(loc(c, a[0]), n(a[1]), n(a[2]), n(a[3])));
    this.on("glUniform4ui", (c, a) => gl.uniform4ui(loc(c, a[0]), n(a[1]), n(a[2]), n(a[3]), n(a[4])));
    this.on("glUniform1fv", (c, a) => gl.uniform1fv(loc(c, a[0]), floats(a[2])));
    this.on("glUniform2fv", (c, a) => gl.uniform2fv(loc(c, a[0]), floats(a[2])));
    this.on("glUniform3fv", (c, a) => gl.uniform3fv(loc(c, a[0]), floats(a[2])));
    this.on("glUniform4fv", (c, a) => gl.uniform4fv(loc(c, a[0]), floats(a[2])));
    this.on("glUniform1iv", (c, a) => gl.uniform1iv(loc(c, a[0]), ints(a[2])));
    this.on("glUniform2iv", (c, a) => gl.uniform2iv(loc(c, a[0]), ints(a[2])));
    this.on("glUniform3iv", (c, a) => gl.uniform3iv(loc(c, a[0]), ints(a[2])));
    this.on("glUniform4iv", (c, a) => gl.uniform4iv(loc(c, a[0]), ints(a[2])));
    this.on("glUniformMatrix2fv", (c, a) => gl.uniformMatrix2fv(loc(c, a[0]), !!n(a[2]), floats(a[3])));
    this.on("glUniformMatrix3fv", (c, a) => gl.uniformMatrix3fv(loc(c, a[0]), !!n(a[2]), floats(a[3])));
    this.on("glUniformMatrix4fv", (c, a) => gl.uniformMatrix4fv(loc(c, a[0]), !!n(a[2]), floats(a[3])));

    // bindings
    this.setting("glBindBuffer", (a) => (n(a[0]) === GL_ELEMENT_ARRAY_BUFFER ? "vaoState" : `buf:${n(a[0])}`), (a, ctx) => {
      if (n(a[0]) === GL_ARRAY_BUFFER) ctx.arrayBuffer = n(a[1]);
      gl.bindBuffer(n(a[0]), this.buf(ctx, n(a[1])));
    });
    // the element array binding is the vertex array's state, not the context's: never replayed
    this.on("glBindBuffer", ((prev) => (c: Channel, a: Arg[], call: Call) => {
      if (n(a[0]) === GL_ELEMENT_ARRAY_BUFFER) gl.bindBuffer(GL_ELEMENT_ARRAY_BUFFER, this.buf(c.ctx!, n(a[1])));
      else prev(c, a, call);
    })(this.handlers.get("glBindBuffer")!));
    this.setting("glBindBufferBase", (a) => (n(a[0]) === GL_UNIFORM_BUFFER ? `ubo:${n(a[1])}` : `tfbuf:${n(a[1])}`), (a, ctx) =>
      gl.bindBufferBase(n(a[0]), n(a[1]), this.buf(ctx, n(a[2]))),
    );
    this.setting("glBindBufferRange", (a) => (n(a[0]) === GL_UNIFORM_BUFFER ? `ubo:${n(a[1])}` : `tfbuf:${n(a[1])}`), (a, ctx) =>
      gl.bindBufferRange(n(a[0]), n(a[1]), this.buf(ctx, n(a[2])), n(a[3]), n(a[4])),
    );
    this.setting("glActiveTexture", () => "activeTexture", (a, ctx) => {
      ctx.activeUnit = n(a[0]) - GL_TEXTURE0;
      gl.activeTexture(n(a[0]));
    });
    this.setting("glBindTexture", (a, ctx) => `tex:${ctx.activeUnit}:${this.target(n(a[0]))}`, (a, ctx) => {
      ctx.boundTex.set(`${ctx.activeUnit}:${n(a[0])}`, n(a[1]));
      gl.bindTexture(this.target(n(a[0])), this.tex(ctx, n(a[1])));
    });
    // replaying a unit's binding needs that unit active; the key's unit names it
    this.on("glBindTexture", ((prev) => (c: Channel, a: Arg[], call: Call) => {
      prev(c, a, call);
      const ctx = c.ctx!;
      const unit = ctx.activeUnit;
      const name = n(a[1]);
      const target = this.target(n(a[0]));
      // the name's object is looked up when the binding is put back: an EGL image or a color
      // buffer may have replaced it since (rcBindTexture, glEGLImageTargetTexture2DOES)
      this.shadowSet(ctx, `tex:${unit}:${target}`, "glBindTexture", [unit, target, name], () => {
        gl.activeTexture(GL_TEXTURE0 + unit);
        gl.bindTexture(target, this.tex(ctx, name));
        gl.activeTexture(GL_TEXTURE0 + ctx.activeUnit);
      });
    })(this.handlers.get("glBindTexture")!));
    this.setting("glBindSampler", (a) => `sampler:${n(a[0])}`, (a, ctx) => gl.bindSampler(n(a[0]), ctx.share.samplers.get(n(a[1])) ?? null));
    // the vertex array is put back by makeCurrent from the context's own record
    for (const f of ["glBindVertexArray", "glBindVertexArrayOES"])
      this.on(f, (c, a) => {
        c.ctx!.vao = n(a[0]);
        gl.bindVertexArray(this.vaoOf(c.ctx!, n(a[0])));
      });
    this.on("glBindFramebuffer", (c, a) => {
      const ctx = c.ctx!;
      const t = n(a[0]);
      if (t === GL_FRAMEBUFFER || t === GL_DRAW_FRAMEBUFFER) ctx.boundFbo.draw = n(a[1]);
      if (t === GL_FRAMEBUFFER || t === GL_READ_FRAMEBUFFER) ctx.boundFbo.read = n(a[1]);
      if (t === GL_FRAMEBUFFER) gl.bindFramebuffer(GL_FRAMEBUFFER, this.fboOf(ctx, n(a[1]), true));
      else gl.bindFramebuffer(t, this.fboOf(ctx, n(a[1]), t === GL_DRAW_FRAMEBUFFER));
    });
    this.setting("glBindRenderbuffer", () => "rbo", (a, ctx) => {
      ctx.boundRbo = n(a[1]);
      const r = ctx.share.renderbuffers.get(n(a[1]));
      gl.bindRenderbuffer(GL_RENDERBUFFER, r instanceof WebGLRenderbuffer ? r : null);
    });
    this.setting("glBindTransformFeedback", () => "tfb", (a, ctx) => gl.bindTransformFeedback(n(a[0]), ctx.tfbs.get(n(a[1])) ?? null));

    // fixed-function state
    this.setting("glEnable", (a) => `cap:${n(a[0])}`, (a) => (n(a[0]) === GL_PRIMITIVE_RESTART_FIXED_INDEX ? undefined : gl.enable(n(a[0]))));
    this.setting("glDisable", (a) => `cap:${n(a[0])}`, (a) => (n(a[0]) === GL_PRIMITIVE_RESTART_FIXED_INDEX ? undefined : gl.disable(n(a[0]))));
    this.setting("glBlendColor", () => "blendColor", (a) => gl.blendColor(n(a[0]), n(a[1]), n(a[2]), n(a[3])));
    this.setting("glBlendEquation", () => "blendEq", (a) => gl.blendEquation(n(a[0])));
    this.setting("glBlendEquationSeparate", () => "blendEq", (a) => gl.blendEquationSeparate(n(a[0]), n(a[1])));
    this.setting("glBlendFunc", () => "blendFunc", (a) => gl.blendFunc(n(a[0]), n(a[1])));
    this.setting("glBlendFuncSeparate", () => "blendFunc", (a) => gl.blendFuncSeparate(n(a[0]), n(a[1]), n(a[2]), n(a[3])));
    this.setting("glColorMask", () => "colorMask", (a) => gl.colorMask(!!n(a[0]), !!n(a[1]), !!n(a[2]), !!n(a[3])));
    this.setting("glCullFace", () => "cullFace", (a) => gl.cullFace(n(a[0])));
    this.setting("glDepthFunc", () => "depthFunc", (a) => gl.depthFunc(n(a[0])));
    this.setting("glDepthMask", () => "depthMask", (a) => gl.depthMask(!!n(a[0])));
    this.setting("glDepthRangef", () => "depthRange", (a) => gl.depthRange(n(a[0]), n(a[1])));
    this.setting("glFrontFace", () => "frontFace", (a) => gl.frontFace(n(a[0])));
    this.setting("glLineWidth", () => "lineWidth", (a) => gl.lineWidth(n(a[0])));
    this.setting("glPolygonOffset", () => "polygonOffset", (a) => gl.polygonOffset(n(a[0]), n(a[1])));
    this.setting("glSampleCoverage", () => "sampleCoverage", (a) => gl.sampleCoverage(n(a[0]), !!n(a[1])));
    this.setting("glScissor", () => "scissor", (a) => gl.scissor(n(a[0]), n(a[1]), n(a[2]), n(a[3])));
    this.setting("glViewport", () => "viewport", (a) => gl.viewport(n(a[0]), n(a[1]), n(a[2]), n(a[3])));
    this.on("glStencilFunc", (c, a) => this.stencil(c.ctx!, "Func", 0x0408, [n(a[0]), n(a[1]), n(a[2])]));
    this.on("glStencilFuncSeparate", (c, a) => this.stencil(c.ctx!, "Func", n(a[0]), [n(a[1]), n(a[2]), n(a[3])]));
    this.on("glStencilOp", (c, a) => this.stencil(c.ctx!, "Op", 0x0408, [n(a[0]), n(a[1]), n(a[2])]));
    this.on("glStencilOpSeparate", (c, a) => this.stencil(c.ctx!, "Op", n(a[0]), [n(a[1]), n(a[2]), n(a[3])]));
    this.on("glStencilMask", (c, a) => this.stencil(c.ctx!, "Mask", 0x0408, [n(a[0])]));
    this.on("glStencilMaskSeparate", (c, a) => this.stencil(c.ctx!, "Mask", n(a[0]), [n(a[1])]));
    this.setting("glClearColor", () => "clearColor", (a) => gl.clearColor(n(a[0]), n(a[1]), n(a[2]), n(a[3])));
    this.setting("glClearDepthf", () => "clearDepth", (a) => gl.clearDepth(n(a[0])));
    this.setting("glClearStencil", () => "clearStencil", (a) => gl.clearStencil(n(a[0])));
    this.setting("glPixelStorei", (a) => `pixelStore:${n(a[0])}`, (a) => gl.pixelStorei(n(a[0]), n(a[1])));
    this.setting("glHint", (a) => `hint:${n(a[0])}`, (a) => (n(a[0]) === 0x8192 ? gl.hint(n(a[0]), n(a[1])) : undefined));
    this.on("glVertexAttrib1f", (_c, a) => gl.vertexAttrib1f(n(a[0]), n(a[1])));
    this.on("glVertexAttrib2f", (_c, a) => gl.vertexAttrib2f(n(a[0]), n(a[1]), n(a[2])));
    this.on("glVertexAttrib3f", (_c, a) => gl.vertexAttrib3f(n(a[0]), n(a[1]), n(a[2]), n(a[3])));
    this.on("glVertexAttrib4f", (_c, a) => gl.vertexAttrib4f(n(a[0]), n(a[1]), n(a[2]), n(a[3]), n(a[4])));
    this.on("glVertexAttrib4fv", (_c, a) => gl.vertexAttrib4fv(n(a[0]), floats(a[1])));

    // vertex arrays: buffer offsets, or client data sent with the call
    this.on("glEnableVertexAttribArray", (_c, a) => gl.enableVertexAttribArray(n(a[0])));
    this.on("glDisableVertexAttribArray", (_c, a) => gl.disableVertexAttribArray(n(a[0])));
    this.on("glVertexAttribDivisor", (_c, a) => gl.vertexAttribDivisor(n(a[0]), n(a[1])));
    this.on("glVertexAttribPointerOffset", (_c, a) => gl.vertexAttribPointer(n(a[0]), n(a[1]), n(a[2]), !!n(a[3]), n(a[4]), n(a[5])));
    this.on("glVertexAttribIPointerOffsetAEMU", (_c, a) => gl.vertexAttribIPointer(n(a[0]), n(a[1]), n(a[2]), n(a[3]), n(a[4])));
    this.on("glVertexAttribPointerData", (c, a) => {
      this.clientArray(c.ctx!, n(a[0]), bytes(a[5]));
      gl.vertexAttribPointer(n(a[0]), n(a[1]), n(a[2]), !!n(a[3]), n(a[4]), 0);
      gl.bindBuffer(GL_ARRAY_BUFFER, this.buf(c.ctx!, c.ctx!.arrayBuffer));
    });
    this.on("glVertexAttribIPointerDataAEMU", (c, a) => {
      this.clientArray(c.ctx!, n(a[0]), bytes(a[4]));
      gl.vertexAttribIPointer(n(a[0]), n(a[1]), n(a[2]), n(a[3]), 0);
      gl.bindBuffer(GL_ARRAY_BUFFER, this.buf(c.ctx!, c.ctx!.arrayBuffer));
    });

    // buffers
    this.on("glBufferData", (_c, a) => {
      const d = a[2];
      if (d instanceof Uint8Array && d.length) gl.bufferData(n(a[0]), d, n(a[3]));
      else gl.bufferData(n(a[0]), n(a[1]), n(a[3]));
    });
    this.on("glBufferDataSyncAEMU", (_c, a) => {
      const d = a[2];
      if (d instanceof Uint8Array && d.length) gl.bufferData(n(a[0]), d, n(a[3]));
      else gl.bufferData(n(a[0]), n(a[1]), n(a[3]));
    });
    this.on("glBufferSubData", (_c, a) => gl.bufferSubData(n(a[0]), n(a[1]), bytes(a[3])));
    this.on("glCopyBufferSubData", (_c, a) => gl.copyBufferSubData(n(a[0]), n(a[1]), n(a[2]), n(a[3]), n(a[4])));
    // a write map's bytes come back with the unmap or the flush
    for (const f of ["glUnmapBufferAEMU", "glUnmapBufferAsyncAEMU"])
      this.on(f, (_c, a) => {
        const d = bytes(a[4]);
        if (n(a[3]) & 0x0002 && d.length) gl.bufferSubData(n(a[0]), n(a[1]), d);
      });
    for (const f of ["glFlushMappedBufferRangeAEMU", "glFlushMappedBufferRangeAEMU2"])
      this.on(f, (_c, a) => {
        const d = bytes(a[4]);
        if (d.length) gl.bufferSubData(n(a[0]), n(a[1]), d);
      });
    this.on("glMapBufferRangeAEMU", () => {});

    // textures
    this.on("glTexImage2D", (_c, a) => {
      const d = a[8];
      gl.texImage2D(this.target(n(a[0])), n(a[1]), n(a[2]), n(a[3]), n(a[4]), n(a[5]), n(a[6]), n(a[7]), d instanceof Uint8Array && d.length ? this.view(n(a[7]), d) : null);
    });
    this.on("glTexImage2DOffsetAEMU", (_c, a) => gl.texImage2D(this.target(n(a[0])), n(a[1]), n(a[2]), n(a[3]), n(a[4]), n(a[5]), n(a[6]), n(a[7]), n(a[8])));
    this.on("glTexSubImage2D", (_c, a) => gl.texSubImage2D(this.target(n(a[0])), n(a[1]), n(a[2]), n(a[3]), n(a[4]), n(a[5]), n(a[6]), n(a[7]), this.view(n(a[7]), bytes(a[8]))));
    this.on("glTexSubImage2DOffsetAEMU", (_c, a) => gl.texSubImage2D(this.target(n(a[0])), n(a[1]), n(a[2]), n(a[3]), n(a[4]), n(a[5]), n(a[6]), n(a[7]), n(a[8])));
    this.on("glTexImage3D", (_c, a) => {
      const d = a[9];
      gl.texImage3D(n(a[0]), n(a[1]), n(a[2]), n(a[3]), n(a[4]), n(a[5]), n(a[6]), n(a[7]), n(a[8]), d instanceof Uint8Array && d.length ? this.view(n(a[8]), d) : null);
    });
    this.on("glTexSubImage3D", (_c, a) => gl.texSubImage3D(n(a[0]), n(a[1]), n(a[2]), n(a[3]), n(a[4]), n(a[5]), n(a[6]), n(a[7]), n(a[8]), n(a[9]), this.view(n(a[9]), bytes(a[10]))));
    this.on("glTexStorage2D", (_c, a) => gl.texStorage2D(this.target(n(a[0])), n(a[1]), n(a[2]), n(a[3]), n(a[4])));
    this.on("glTexStorage3D", (_c, a) => gl.texStorage3D(n(a[0]), n(a[1]), n(a[2]), n(a[3]), n(a[4]), n(a[5])));
    this.on("glCompressedTexImage2D", (_c, a) => gl.compressedTexImage2D(this.target(n(a[0])), n(a[1]), n(a[2]), n(a[3]), n(a[4]), n(a[5]), bytes(a[7])));
    this.on("glCompressedTexSubImage2D", (_c, a) => gl.compressedTexSubImage2D(this.target(n(a[0])), n(a[1]), n(a[2]), n(a[3]), n(a[4]), n(a[5]), n(a[6]), bytes(a[8])));
    this.on("glCopyTexImage2D", (_c, a) => gl.copyTexImage2D(this.target(n(a[0])), n(a[1]), n(a[2]), n(a[3]), n(a[4]), n(a[5]), n(a[6]), n(a[7])));
    this.on("glCopyTexSubImage2D", (_c, a) => gl.copyTexSubImage2D(this.target(n(a[0])), n(a[1]), n(a[2]), n(a[3]), n(a[4]), n(a[5]), n(a[6]), n(a[7])));
    this.on("glGenerateMipmap", (_c, a) => gl.generateMipmap(this.target(n(a[0]))));
    this.on("glTexParameteri", (_c, a) => gl.texParameteri(this.target(n(a[0])), n(a[1]), n(a[2])));
    this.on("glTexParameterf", (_c, a) => gl.texParameterf(this.target(n(a[0])), n(a[1]), n(a[2])));
    this.on("glTexParameteriv", (_c, a) => gl.texParameteri(this.target(n(a[0])), n(a[1]), ints(a[2])[0] ?? 0));
    this.on("glTexParameterfv", (_c, a) => gl.texParameterf(this.target(n(a[0])), n(a[1]), floats(a[2])[0] ?? 0));
    this.on("glSamplerParameteri", (c, a) => gl.samplerParameteri(c.ctx!.share.samplers.get(n(a[0]))!, n(a[1]), n(a[2])));
    this.on("glSamplerParameterf", (c, a) => gl.samplerParameterf(c.ctx!.share.samplers.get(n(a[0]))!, n(a[1]), n(a[2])));
    this.on("glEGLImageTargetTexture2DOES", (c, a) => {
      const t = this.images.get(n(a[1]));
      const ctx = c.ctx!;
      const name = ctx.boundTex.get(`${ctx.activeUnit}:${n(a[0])}`);
      if (t && name) {
        ctx.share.textures.set(name, t);
        gl.bindTexture(GL_TEXTURE_2D, t);
      }
    });

    // framebuffers and renderbuffers
    this.on("glFramebufferTexture2D", (c, a) =>
      gl.framebufferTexture2D(n(a[0]), n(a[1]), this.target(n(a[2])), this.tex(c.ctx!, n(a[3])), n(a[4])),
    );
    this.on("glFramebufferTextureLayer", (c, a) => gl.framebufferTextureLayer(n(a[0]), n(a[1]), this.tex(c.ctx!, n(a[2])), n(a[3]), n(a[4])));
    this.on("glFramebufferRenderbuffer", (c, a) => {
      const r = c.ctx!.share.renderbuffers.get(n(a[3]));
      if (r && !(r instanceof WebGLRenderbuffer)) gl.framebufferTexture2D(n(a[0]), n(a[1]), GL_TEXTURE_2D, r.tex, 0);
      else gl.framebufferRenderbuffer(n(a[0]), n(a[1]), n(a[2]), r ?? null);
    });
    this.on("glRenderbufferStorage", (_c, a) => gl.renderbufferStorage(n(a[0]), n(a[1]), n(a[2]), n(a[3])));
    this.on("glRenderbufferStorageMultisample", (_c, a) => gl.renderbufferStorageMultisample(n(a[0]), n(a[1]), n(a[2]), n(a[3]), n(a[4])));
    this.on("glDrawBuffers", (c, a) => {
      const bufs = Array.from(u32s(bytes(a[1])));
      // framebuffer 0 is a surface's framebuffer here: its color is attachment 0
      gl.drawBuffers(c.ctx!.boundFbo.draw === 0 ? bufs.map((b) => (b === 0x0405 ? 0x8ce0 : b)) : bufs);
    });
    this.on("glReadBuffer", (c, a) => gl.readBuffer(c.ctx!.boundFbo.read === 0 && n(a[0]) === 0x0405 ? 0x8ce0 : n(a[0])));
    this.on("glBlitFramebuffer", (_c, a) => gl.blitFramebuffer(n(a[0]), n(a[1]), n(a[2]), n(a[3]), n(a[4]), n(a[5]), n(a[6]), n(a[7]), n(a[8]), n(a[9])));
    this.on("glInvalidateFramebuffer", (c, a) => {
      const att = Array.from(u32s(bytes(a[2])));
      gl.invalidateFramebuffer(n(a[0]), this.fboAttachments(c.ctx!, n(a[0]), att));
    });
    this.on("glDiscardFramebufferEXT", (c, a) => {
      const att = Array.from(u32s(bytes(a[2])));
      gl.invalidateFramebuffer(n(a[0]), this.fboAttachments(c.ctx!, n(a[0]), att));
    });

    // drawing
    this.on("glClear", (_c, a) => gl.clear(n(a[0])));
    this.on("glClearBufferfv", (_c, a) => gl.clearBufferfv(n(a[0]), n(a[1]), floats(a[2])));
    this.on("glClearBufferiv", (_c, a) => gl.clearBufferiv(n(a[0]), n(a[1]), ints(a[2])));
    this.on("glClearBufferuiv", (_c, a) => gl.clearBufferuiv(n(a[0]), n(a[1]), new Uint32Array(ints(a[2]).buffer)));
    this.on("glClearBufferfi", (_c, a) => gl.clearBufferfi(n(a[0]), n(a[1]), n(a[2]), n(a[3])));
    for (const f of ["glDrawArrays", "glDrawArraysNullAEMU"]) this.on(f, (_c, a) => gl.drawArrays(n(a[0]), n(a[1]), n(a[2])));
    this.on("glDrawArraysInstanced", (_c, a) => gl.drawArraysInstanced(n(a[0]), n(a[1]), n(a[2]), n(a[3])));
    for (const f of ["glDrawElementsOffset", "glDrawElementsOffsetNullAEMU"]) this.on(f, (_c, a) => gl.drawElements(n(a[0]), n(a[1]), n(a[2]), n(a[3])));
    this.on("glDrawElementsInstancedOffsetAEMU", (_c, a) => gl.drawElementsInstanced(n(a[0]), n(a[1]), n(a[2]), n(a[3]), n(a[4])));
    this.on("glDrawRangeElementsOffsetAEMU", (_c, a) => gl.drawRangeElements(n(a[0]), n(a[1]), n(a[2]), n(a[3]), n(a[4]), n(a[5])));
    for (const f of ["glDrawElementsData", "glDrawElementsDataNullAEMU"])
      this.on(f, (c, a) => this.clientIndices(c.ctx!, bytes(a[3]), () => gl.drawElements(n(a[0]), n(a[1]), n(a[2]), 0)));
    this.on("glDrawElementsInstancedDataAEMU", (c, a) =>
      this.clientIndices(c.ctx!, bytes(a[3]), () => gl.drawElementsInstanced(n(a[0]), n(a[1]), n(a[2]), 0, n(a[4]))),
    );
    this.on("glDrawRangeElementsDataAEMU", (c, a) =>
      this.clientIndices(c.ctx!, bytes(a[5]), () => gl.drawRangeElements(n(a[0]), n(a[1]), n(a[2]), n(a[3]), n(a[4]), 0)),
    );
    this.on("glBeginTransformFeedback", (_c, a) => gl.beginTransformFeedback(n(a[0])));
    this.on("glEndTransformFeedback", () => gl.endTransformFeedback());

    // answered by the phone, drawing nothing: the mirror reads no state back
    for (const f of [
      "glFinish",
      "glFlush",
      "glGetError",
      "glGetIntegerv",
      "glGetFloatv",
      "glGetBooleanv",
      "glGetString",
      "glGetStringi",
      "glGetShaderiv",
      "glGetProgramiv",
      "glGetShaderInfoLog",
      "glGetProgramInfoLog",
      "glGetActiveUniform",
      "glGetActiveAttrib",
      "glGetAttribLocation",
      "glGetUniformIndicesAEMU",
      "glGetActiveUniformsiv",
      "glGetActiveUniformBlockiv",
      "glGetActiveUniformBlockName",
      "glGetFramebufferAttachmentParameteriv",
      "glGetRenderbufferParameteriv",
      "glGetTexParameteriv",
      "glGetBufferParameteriv",
      "glGetShaderPrecisionFormat",
      "glGetInternalformativ",
      "glCheckFramebufferStatus",
      "glIsTexture",
      "glIsBuffer",
      "glIsProgram",
      "glIsShader",
      "glIsFramebuffer",
      "glIsRenderbuffer",
      "glReadPixels",
      "glReadPixelsOffsetAEMU",
      "glFenceSyncAEMU",
      "glClientWaitSyncAEMU",
      "glWaitSyncAEMU",
      "glDeleteSyncAEMU",
      "glIsSyncAEMU",
      "glGetSyncivAEMU",
      "glGetFragDataLocation",
      "glGetQueryObjectuiv",
      "glBeginQuery",
      "glEndQuery",
      "glGetGraphicsResetStatusEXT",
      "glValidateProgram",
      "glReleaseShaderCompiler",
    ])
      if (!this.handlers.has(f)) this.on(f, () => {});
  }

  private newColorBuffer(handle: number, w: number, h: number): ColorBuffer {
    const gl = this.gl;
    const tex = gl.createTexture()!;
    this.withTexture(tex, () => {
      gl.texStorage2D(GL_TEXTURE_2D, 1, 0x8058, Math.max(1, w), Math.max(1, h));
      gl.texParameteri(GL_TEXTURE_2D, 0x2801, 0x2601);
      gl.texParameteri(GL_TEXTURE_2D, 0x2800, 0x2601);
      gl.texParameteri(GL_TEXTURE_2D, 0x2802, 0x812f);
      gl.texParameteri(GL_TEXTURE_2D, 0x2803, 0x812f);
    });
    const cb = { handle, tex, w, h };
    this.colorBuffers.set(handle, cb);
    return cb;
  }

  /**
   * The host destroyed color buffer `handle`: its handle names nothing now. Its texture goes
   * too, unless a guest texture name, renderbuffer or window surface still draws with it (an
   * EGLImage outlives the buffer it was made from); glDeleteTextures then deletes it with the
   * name.
   */
  private freeColorBuffer(handle: number): void {
    const cb = this.colorBuffers.get(handle);
    if (!cb) return;
    this.colorBuffers.delete(handle);
    this.stats.freed++;
    this.release(cb.tex);
  }

  /**
   * One owner of `t` let go of it: a FREE, a deleted texture name, renderbuffer, window surface
   * or client image, or one of those given another buffer. The texture goes with its last owner.
   */
  private release(t: WebGLTexture | null | undefined): void {
    if (t && !this.texInUse(t)) this.gl.deleteTexture(t);
  }

  /** A FREE record: the host destroyed the buffer, unless this FREE is a replaced buffer's. */
  private hostFreed(handle: number): void {
    const stale = this.staleFrees.get(handle) ?? 0;
    if (stale > 0) {
      if (stale === 1) this.staleFrees.delete(handle);
      else this.staleFrees.set(handle, stale - 1);
      return;
    }
    this.freeColorBuffer(handle);
  }

  /** Whether a live color buffer, a client image, a guest texture name, a renderbuffer or a window surface still draws with `t`. */
  private texInUse(t: WebGLTexture): boolean {
    for (const cb of this.colorBuffers.values()) if (cb.tex === t) return true;
    for (const x of this.images.values()) if (x === t) return true;
    for (const share of new Set([...this.contexts.values()].map((c) => c.share))) {
      for (const x of share.textures.values()) if (x === t) return true;
      for (const r of share.renderbuffers.values()) if (!(r instanceof WebGLRenderbuffer) && r.tex === t) return true;
    }
    for (const s of this.surfaces.values()) if (s.cb?.tex === t) return true;
    return false;
  }

  private stencil(ctx: Ctx, kind: "Func" | "Op" | "Mask", face: number, v: number[]): void {
    const gl = this.gl;
    for (const f of face === 0x0408 ? [0x0404, 0x0405] : [face]) {
      const run = () =>
        kind === "Func"
          ? gl.stencilFuncSeparate(f, v[0]!, v[1]!, v[2]!)
          : kind === "Op"
            ? gl.stencilOpSeparate(f, v[0]!, v[1]!, v[2]!)
            : gl.stencilMaskSeparate(f, v[0]!);
      run();
      this.shadowSet(ctx, `stencil${kind}:${f}`, `stencil${kind}`, v, run);
    }
  }

  /** Framebuffer 0's attachments named as the surface framebuffer's. */
  private fboAttachments(ctx: Ctx, target: number, att: number[]): number[] {
    const zero = target === GL_READ_FRAMEBUFFER ? ctx.boundFbo.read === 0 : ctx.boundFbo.draw === 0;
    if (!zero) return att;
    return att.map((x) => (x === 0x1800 ? 0x8ce0 : x === 0x1801 || x === 0x1802 ? 0x821a : x));
  }

  /** Client-side vertex data sent with the call: into a buffer of the attribute's own. */
  private clientArray(ctx: Ctx, index: number, data: Uint8Array): void {
    const gl = this.gl;
    let b = ctx.scratch.get(index);
    if (!b) {
      b = gl.createBuffer()!;
      ctx.scratch.set(index, b);
    }
    gl.bindBuffer(GL_ARRAY_BUFFER, b);
    gl.bufferData(GL_ARRAY_BUFFER, data, 0x88e0);
  }

  /** Client-side indices sent with the call: drawn from a scratch element buffer, then the vertex array's own put back. */
  private clientIndices(ctx: Ctx, data: Uint8Array, draw: () => void): void {
    const gl = this.gl;
    const prev = gl.getParameter(0x8895) as WebGLBuffer | null;
    let b = ctx.scratch.get(-1);
    if (!b) {
      b = gl.createBuffer()!;
      ctx.scratch.set(-1, b);
    }
    gl.bindBuffer(GL_ELEMENT_ARRAY_BUFFER, b);
    gl.bufferData(GL_ELEMENT_ARRAY_BUFFER, data, 0x88e0);
    draw();
    gl.bindBuffer(GL_ELEMENT_ARRAY_BUFFER, prev);
  }

  private view(type: number, d: Uint8Array): ArrayBufferView {
    const copy = d.slice();
    switch (type) {
      case 0x1406: // FLOAT
        return new Float32Array(copy.buffer, 0, copy.byteLength >> 2);
      case 0x8363: // UNSIGNED_SHORT_5_6_5
      case 0x8033:
      case 0x8034:
      case 0x1403:
      case 0x1402:
      case 0x140b: // HALF_FLOAT
        return new Uint16Array(copy.buffer, 0, copy.byteLength >> 1);
      case 0x1405:
      case 0x1404:
      case 0x8368:
      case 0x8c3b:
      case 0x8c3e:
      case 0x84fa:
        return new Uint32Array(copy.buffer, 0, copy.byteLength >> 2);
      default:
        return copy;
    }
  }

  private upload(x: number, y: number, w: number, h: number, format: number, type: number, px: Uint8Array): void {
    const gl = this.gl;
    gl.pixelStorei(0x0cf5, 1);
    gl.pixelStorei(0x0cf2, 0);
    // gralloc's BGRA and RGB565 arrive as the guest's bytes; WebGL2 takes RGBA and RGB 565
    if (format === 0x80e1) {
      const s = px.slice();
      for (let i = 0; i + 3 < s.length; i += 4) {
        const b = s[i]!;
        s[i] = s[i + 2]!;
        s[i + 2] = b;
      }
      gl.texSubImage2D(GL_TEXTURE_2D, 0, x, y, w, h, 0x1908, 0x1401, s);
    } else if (format === 0x1907 && type === 0x8363) {
      gl.texSubImage2D(GL_TEXTURE_2D, 0, x, y, w, h, 0x1907, 0x8363, this.view(type, px));
    } else {
      gl.texSubImage2D(GL_TEXTURE_2D, 0, x, y, w, h, 0x1908, 0x1401, px);
    }
    // the guest's unpack state is put back on the next context switch
    this.dirty = true;
  }

  private withTexture(t: WebGLTexture, f: () => void): void {
    const gl = this.gl;
    gl.activeTexture(GL_TEXTURE0 + TEXTURE_UNITS - 1);
    gl.bindTexture(GL_TEXTURE_2D, t);
    f();
    gl.bindTexture(GL_TEXTURE_2D, null);
    this.dirty = true;
  }

  private withFbo(f: WebGLFramebuffer, g: () => void): void {
    this.gl.bindFramebuffer(GL_FRAMEBUFFER, f);
    g();
    this.dirty = true;
  }

  private restoreRbo(): void {
    this.gl.bindRenderbuffer(GL_RENDERBUFFER, null);
    this.dirty = true;
  }

  // ---- composition and posts -------------------------------------------------------------

  private quad() {
    if (this.blit) return this.blit;
    const gl = this.gl;
    const vs = `#version 300 es
in vec2 p; uniform vec4 rect; uniform vec4 uv; uniform mat2 xm; uniform vec2 xo; out vec2 t;
void main() { vec2 q = p * 0.5 + 0.5; t = uv.xy + (xm * q + xo) * uv.zw; gl_Position = vec4(rect.xy + q * rect.zw, 0.0, 1.0) * vec4(2.0, 2.0, 1.0, 1.0) - vec4(1.0, 1.0, 0.0, 0.0); }`;
    const fs = `#version 300 es
precision highp float; in vec2 t; uniform sampler2D src; uniform vec4 color; uniform int mode; out vec4 o;
void main() { o = mode == 1 ? color : texture(src, t) * color.a; }`;
    // mode 1: a solid colour, already multiplied by the layer's alpha; else the texture times it
    const sh = (type: number, src: string) => {
      const s = gl.createShader(type)!;
      gl.shaderSource(s, src);
      gl.compileShader(s);
      return s;
    };
    const prog = gl.createProgram()!;
    gl.attachShader(prog, sh(0x8b31, vs));
    gl.attachShader(prog, sh(0x8b30, fs));
    gl.bindAttribLocation(prog, 0, "p");
    gl.linkProgram(prog);
    const vao = gl.createVertexArray()!;
    gl.bindVertexArray(vao);
    const b = gl.createBuffer()!;
    gl.bindBuffer(GL_ARRAY_BUFFER, b);
    gl.bufferData(GL_ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), 0x88e4);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 2, 0x1406, false, 0, 0);
    this.blit = {
      prog,
      vao,
      uSrc: gl.getUniformLocation(prog, "src"),
      uRect: gl.getUniformLocation(prog, "rect"),
      uUv: gl.getUniformLocation(prog, "uv"),
      uColor: gl.getUniformLocation(prog, "color"),
      uMode: gl.getUniformLocation(prog, "mode"),
      uXm: gl.getUniformLocation(prog, "xm"),
      uXo: gl.getUniformLocation(prog, "xo"),
    };
    return this.blit;
  }

  /** Draws with the mirror's own state; the guest context's state is put back on its next call. */
  private ownState(): void {
    const gl = this.gl;
    for (const c of CAPS) if (c !== 0x0bd0) gl.disable(c);
    gl.colorMask(true, true, true, true);
    gl.bindSampler(0, null);
    gl.activeTexture(GL_TEXTURE0);
    gl.pixelStorei(0x0cf5, 4);
    this.dirty = true;
  }

  /** HWC's host composition (rcCompose): each layer drawn into the target color buffer. */
  private compose(buf: Uint8Array, post: boolean, call: Call): void {
    const gl = this.gl;
    const v = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
    const version = v.getUint32(0, true);
    let at = 4;
    if (version >= 2) at += 4; // displayId
    const handle = v.getUint32(at, true);
    const layers = v.getUint32(at + 4, true);
    at += 8;
    let target = this.colorBuffers.get(handle);
    if (!target) {
      // HWC's own targets are made at boot, before the copy started: one the size of the layers'
      // union stands in for it
      let w = 0;
      let h = 0;
      for (let i = 0, o = at; i < layers && o + 56 <= buf.length; i++, o += 56) {
        w = Math.max(w, v.getInt32(o + 16, true));
        h = Math.max(h, v.getInt32(o + 20, true));
      }
      if (w <= 0 || h <= 0) return;
      target = this.newColorBuffer(handle, w, h);
    }
    const q = this.quad();
    const fbo = gl.createFramebuffer()!;
    this.ownState();
    gl.bindFramebuffer(GL_FRAMEBUFFER, fbo);
    gl.framebufferTexture2D(GL_FRAMEBUFFER, 0x8ce0, GL_TEXTURE_2D, target.tex, 0);
    gl.viewport(0, 0, target.w, target.h);
    gl.clearColor(0, 0, 0, 1);
    gl.clear(0x4000);
    gl.useProgram(q.prog);
    gl.bindVertexArray(q.vao);
    gl.uniform1i(q.uSrc, 0);
    gl.enable(0x0be2);
    for (let i = 0; i < layers && at + 56 <= buf.length; i++, at += 56) {
      const cb = this.colorBuffers.get(v.getUint32(at, true));
      const mode = v.getInt32(at + 4, true);
      const [l, t, r, b] = [0, 1, 2, 3].map((k) => v.getInt32(at + 8 + 4 * k, true)) as [number, number, number, number];
      const [cl, ct, cr, cbm] = [0, 1, 2, 3].map((k) => v.getFloat32(at + 24 + 4 * k, true)) as [number, number, number, number];
      const blend = v.getInt32(at + 40, true);
      const alpha = v.getFloat32(at + 44, true);
      const color = [0, 1, 2, 3].map((k) => v.getUint8(at + 48 + k) / 255) as [number, number, number, number];
      const transform = v.getInt32(at + 52, true);
      // As gfxstream's TextureDraw::drawLayer: device layers and solid colours only, blending
      // none (1) or premultiplied (2); anything else the phone's composer skips too.
      if ((mode !== 2 && mode !== 3) || (blend !== 1 && blend !== 2)) continue;
      if (mode === 2 && !cb) continue;
      if (blend === 1) gl.disable(0x0be2);
      else {
        gl.enable(0x0be2);
        gl.blendFunc(1, 0x0303);
      }
      gl.uniform4f(q.uRect, l / target.w, t / target.h, (r - l) / target.w, (b - t) / target.h);
      const [m00, m01, m10, m11, ox, oy] = layerTransform(transform);
      // column-major: xm * q = (m00 q.x + m01 q.y, m10 q.x + m11 q.y)
      gl.uniformMatrix2fv(q.uXm, false, [m00, m10, m01, m11]);
      gl.uniform2f(q.uXo, ox, oy);
      if (mode === 3) {
        gl.uniform1i(q.uMode, 1);
        gl.uniform4f(q.uColor, color[0] * alpha, color[1] * alpha, color[2] * alpha, color[3] * alpha);
      } else {
        gl.uniform1i(q.uMode, 0);
        gl.uniform4f(q.uColor, 1, 1, 1, alpha);
        gl.uniform4f(q.uUv, cl / cb!.w, ct / cb!.h, (cr - cl) / cb!.w, (cbm - ct) / cb!.h);
        gl.bindTexture(GL_TEXTURE_2D, cb!.tex);
      }
      gl.drawArrays(0x0005, 0, 4);
    }
    gl.disable(0x0be2);
    gl.bindFramebuffer(GL_FRAMEBUFFER, null);
    gl.deleteFramebuffer(fbo);
    if (post) this.post(target.handle, call);
  }

  /** The screen: the posted color buffer, drawn to the canvas. */
  private post(handle: number, _call?: Call): void {
    const cb = this.colorBuffers.get(handle);
    if (!cb) return;
    const gl = this.gl;
    // the canvas takes the screen's size (a turned phone posts the other shape)
    const canvas = gl.canvas;
    if (canvas.width !== cb.w || canvas.height !== cb.h) {
      canvas.width = cb.w;
      canvas.height = cb.h;
    }
    const q = this.quad();
    this.ownState();
    gl.bindFramebuffer(GL_FRAMEBUFFER, null);
    gl.viewport(0, 0, gl.drawingBufferWidth, gl.drawingBufferHeight);
    gl.useProgram(q.prog);
    gl.bindVertexArray(q.vao);
    gl.uniform1i(q.uSrc, 0);
    gl.uniform1i(q.uMode, 0);
    gl.uniform4f(q.uColor, 1, 1, 1, 1);
    gl.uniform4f(q.uRect, 0, 0, 1, 1);
    gl.uniform4f(q.uUv, 0, 0, 1, 1);
    gl.uniformMatrix2fv(q.uXm, false, [1, 0, 0, 1]);
    gl.uniform2f(q.uXo, 0, 0);
    gl.bindTexture(GL_TEXTURE_2D, cb.tex);
    gl.drawArrays(0x0005, 0, 4);
    this.stats.posts++;
    this.postRate.add(this.nowMs(), 1);
    this.onPost?.(cb, this.timeUs);
  }
}
