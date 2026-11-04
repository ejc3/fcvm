// The GPU mirror's join loader: the phone's GL state at the start of the copy (the JOIN parts
// GL_STATE, TEXTURE and CHANNEL, parsed by core/mirror/gles-snapshot.ts) rebuilt in the
// replayer (gl.ts), so the processes that were drawing before a viewer joined (SystemUI, the
// launcher, an app) keep drawing on the live records that follow.
//
// Objects are made directly in WebGL2 and put in gl.ts's name maps. A context's settings are put
// back by running gl.ts's own handlers for the calls that set them (glEnable, glBindTexture, ...),
// so each lands in the context's shadow with the key and value a live call makes, and a later
// context switch replays it.
//
// What the snapshot cannot give WebGL2 is counted in JoinGlStats, never thrown.

import {
  attachmentPoint,
  CUBE_FACES,
  DEFAULT_TEXTURE_NAME,
  parseChannel,
  parseGlState,
  parseTexture,
  TEXTURE_UNIT_TARGETS,
  type BufferBinding,
  type ChannelState,
  type FramebufferObject,
  type GlesContext,
  type GlSnapshot,
  type ProgramObject,
  type ShareGroup as SavedShareGroup,
  type TextureBlob,
  type TextureObject,
  type VaoState,
} from "../../../core/mirror/gles-snapshot.ts";
import type { Arg } from "../../../core/mirror/wire.ts";
import type { ColorBuffer, Ctx, Program, ShareGroup } from "./gl.ts";

type GL = WebGL2RenderingContext;

const GL_TEXTURE0 = 0x84c0;
const GL_TEXTURE_2D = 0x0de1;
const GL_TEXTURE_CUBE_MAP = 0x8513;
const GL_TEXTURE_3D = 0x806f;
const GL_TEXTURE_2D_ARRAY = 0x8c1a;
const GL_TEXTURE_EXTERNAL_OES = 0x8d65;
const GL_ARRAY_BUFFER = 0x8892;
const GL_ELEMENT_ARRAY_BUFFER = 0x8893;
const GL_UNIFORM_BUFFER = 0x8a11;
const GL_TRANSFORM_FEEDBACK_BUFFER = 0x8c8e;
const GL_TRANSFORM_FEEDBACK = 0x8e22;
const GL_PIXEL_UNPACK_BUFFER = 0x88ec;
const GL_FRAMEBUFFER = 0x8d40;
const GL_READ_FRAMEBUFFER = 0x8ca8;
const GL_DRAW_FRAMEBUFFER = 0x8ca9;
const GL_RENDERBUFFER = 0x8d41;
const GL_BLEND = 0x0be2;
const GL_PRIMITIVE_RESTART_FIXED_INDEX = 0x8d69;
const GL_LINK_STATUS = 0x8b82;
const GL_ACTIVE_UNIFORM_BLOCKS = 0x8a36;
const GL_FIXED = 0x140c;
const GL_HALF_FLOAT = 0x140b;
const GL_HALF_FLOAT_OES = 0x8d61;
const GL_BGRA_EXT = 0x80e1;
/** The unit the loader binds textures on to fill them, as gl.ts's own uploads do. */
const SCRATCH_UNIT = 15;
const TEXTURE_TARGETS: ReadonlySet<number> = new Set([GL_TEXTURE_2D, GL_TEXTURE_CUBE_MAP, GL_TEXTURE_3D, GL_TEXTURE_2D_ARRAY]);
const hex = (x: number) => `0x${x.toString(16)}`;

/** The JOIN parts the GL state comes in, as GlMirror collected them (each a copy of its record's payload). */
export interface JoinGlParts {
  glState: Uint8Array | null;
  /** By the texture's id in GL_STATE (its host name), the bytes after the id. */
  textures: Map<number, Uint8Array>;
  /** By tap channel id, RenderThread::save's bytes. */
  channels: Map<number, Uint8Array>;
}

/** What the join's GL state rebuilt, and what it could not. */
export interface JoinGlStats {
  contexts: number;
  shareGroups: number;
  textures: number;
  buffers: number;
  renderbuffers: number;
  shaders: number;
  programs: number;
  samplers: number;
  framebuffers: number;
  vertexArrays: number;
  /** EGL images (rcCreateClientImage handles) mapped to their textures. */
  images: number;
  surfaces: number;
  /** Channels made known from their CHANNEL part, with the context they had current. */
  channels: number;
  /** Textures made without contents: no TEXTURE part, an empty one, or levels with no pixels. */
  texturesWithoutPixels: number;
  /** Textures in a compressed format the snapshot has no pixels for, left without storage. */
  compressedTextures: number;
  /** Enabled vertex attributes reading client memory: the live stream sends their data with each draw. */
  clientVertexPointers: number;
  /** Programs the phone linked whose saved sources WebGL2 would not link. */
  programsNotLinked: number;
  /** Everything else not restored, by reason. */
  skipped: Record<string, number>;
  /** Why the GL state was not loaded at all (a part that would not parse), or null. */
  error: string | null;
}

/** GlMirror's side of the loader: its maps, and the paths its live handlers take. */
export interface JoinHost {
  gl: GL;
  /** The color buffers the COLOR_BUFFER parts made, by handle. */
  colorBuffers: ReadonlyMap<number, ColorBuffer>;
  /** EGL client images by rc handle. */
  images: Map<number, WebGLTexture>;
  /** The capabilities WebGL2 takes in glEnable. */
  caps: readonly number[];
  newShare(): ShareGroup;
  /** As rcCreateContext. */
  createContext(handle: number, share: ShareGroup): Ctx;
  /** As rcCreateWindowSurface. */
  createWindowSurface(handle: number, w: number, h: number): void;
  /** As rcSetWindowColorBuffer, with the surface's own pixels taken from the color buffer. */
  setWindowColorBuffer(surface: number, colorBuffer: number): void;
  /** Guest buffer `name` of `share` with its bytes, as a bind to `target` and a glBufferData; null bytes make it without contents. */
  fillBuffer(share: ShareGroup, name: number, target: number, data: Uint8Array | null, usage: number): void;
  /** Guest buffer `name`'s WebGL2 buffer of the type `target` takes (one for ELEMENT_ARRAY_BUFFER is bound there), or null when no such buffer. */
  bufferAt(share: ShareGroup, name: number, target: number): WebGLBuffer | null;
  /** Marks the channel known and makes `context` current on it, as rcMakeCurrent; false when no such context. */
  bindChannel(id: number, context: number, draw: number, read: number): boolean;
  /** Runs the live handler for GL call `name` with `ctx` current. Throws what the handler throws. */
  call(ctx: Ctx, name: string, args: Arg[]): void;
  vaoOf(ctx: Ctx, name: number): WebGLVertexArrayObject;
  fboOf(ctx: Ctx, name: number): WebGLFramebuffer | null;
  /** Pixel bytes as the typed array WebGL2 takes for `type`. */
  view(type: number, d: Uint8Array): ArrayBufferView;
  /** A guest shader made into one WebGL2 compiles. */
  shaderSource(src: string): string;
  /** The loader changed WebGL2 state behind the contexts' shadows. */
  touched(): void;
}

function emptyStats(): JoinGlStats {
  return {
    contexts: 0,
    shareGroups: 0,
    textures: 0,
    buffers: 0,
    renderbuffers: 0,
    shaders: 0,
    programs: 0,
    samplers: 0,
    framebuffers: 0,
    vertexArrays: 0,
    images: 0,
    surfaces: 0,
    channels: 0,
    texturesWithoutPixels: 0,
    compressedTextures: 0,
    clientVertexPointers: 0,
    programsNotLinked: 0,
    skipped: {},
    error: null,
  };
}

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

/**
 * Rebuilds the phone's GL state from the join's GL parts. GL_STATE and the CHANNEL parts are
 * parsed before anything is made, so one that does not parse leaves the mirror as the color
 * buffers left it, with the reason in `error`. A TEXTURE part that does not parse costs that
 * texture its pixels. Never throws.
 */
export function loadGlJoin(host: JoinHost, parts: JoinGlParts): JoinGlStats {
  const stats = emptyStats();
  let snap: GlSnapshot;
  const channels: [number, ChannelState][] = [];
  try {
    if (!parts.glState) throw new Error("END says the GL state was sent, but no GL_STATE part came");
    snap = parseGlState(parts.glState);
    for (const [id, b] of parts.channels) channels.push([id, parseChannel(b)]);
  } catch (e) {
    stats.error = message(e);
    return stats;
  }
  try {
    new Loader(host, snap, parts.textures, stats).run(channels);
  } catch (e) {
    // each object is guarded; this is a defect in the loader itself
    stats.error = `loader: ${message(e)}`;
  }
  host.touched();
  return stats;
}

/** A renderbuffer name that a color buffer took (rcBindRenderbuffer), as gl.ts keeps it. */
const isColorBuffer = (r: unknown): r is ColorBuffer =>
  typeof r === "object" && r !== null && typeof (r as ColorBuffer).handle === "number" && "tex" in r;

// ---- pixel formats ----

interface PixelFormat {
  /** For texImage: sized where WebGL2 has one, else the unsized format (LUMINANCE, ALPHA). */
  internal: number;
  /** For texStorage, 0 when WebGL2 has no sized format for it. */
  sized: number;
  format: number;
  type: number;
  bytesPerPixel: number;
  /** BGRA bytes, swapped to RGBA before the upload. */
  bgra: boolean;
}

/** (format, type) to [WebGL2's sized internal format or 0, bytes per pixel]. */
const FORMATS = new Map<string, [number, number]>([
  ["1908:1401", [0x8058, 4]], // RGBA UNSIGNED_BYTE: RGBA8
  ["1907:1401", [0x8051, 3]], // RGB: RGB8
  ["1907:8363", [0x8d62, 2]], // RGB 5_6_5: RGB565
  ["1908:8033", [0x8056, 2]], // RGBA 4_4_4_4: RGBA4
  ["1908:8034", [0x8057, 2]], // RGBA 5_5_5_1: RGB5_A1
  ["1908:8368", [0x8059, 4]], // RGBA 2_10_10_10_REV: RGB10_A2
  ["1908:1406", [0x8814, 16]], // RGBA32F
  ["1908:140b", [0x881a, 8]], // RGBA16F
  ["1907:1406", [0x8815, 12]], // RGB32F
  ["1907:140b", [0x881b, 6]], // RGB16F
  ["1907:8c3b", [0x8c3a, 4]], // R11F_G11F_B10F
  ["1907:8c3e", [0x8c3d, 4]], // RGB9_E5
  ["1903:1401", [0x8229, 1]], // RED: R8
  ["1903:1406", [0x822e, 4]], // R32F
  ["1903:140b", [0x822d, 2]], // R16F
  ["8227:1401", [0x822b, 2]], // RG: RG8
  ["8227:1406", [0x8230, 8]], // RG32F
  ["8227:140b", [0x822f, 4]], // RG16F
  ["1902:1403", [0x81a5, 2]], // DEPTH_COMPONENT16
  ["1902:1405", [0x81a6, 4]], // DEPTH_COMPONENT24
  ["1902:1406", [0x8cac, 4]], // DEPTH_COMPONENT32F
  ["84f9:84fa", [0x88f0, 4]], // DEPTH24_STENCIL8
  ["1909:1401", [0, 1]], // LUMINANCE
  ["1906:1401", [0, 1]], // ALPHA
  ["190a:1401", [0, 2]], // LUMINANCE_ALPHA
]);

/** Sized formats the format and type alone do not name (sRGB, and 16-bit ones uploaded as bytes), with the (format, type) they take. */
const SIZED_OVERRIDES = new Map<number, string>([
  [0x8c43, "1908:1401"], // SRGB8_ALPHA8
  [0x8c41, "1907:1401"], // SRGB8
  [0x8d62, "1907:1401"], // RGB565
  [0x8056, "1908:1401"], // RGBA4
  [0x8057, "1908:1401"], // RGB5_A1
]);

function pixelFormat(internalFormat: number, format: number, type: number): PixelFormat | null {
  const t = type === GL_HALF_FLOAT_OES ? GL_HALF_FLOAT : type;
  const bgra = format === GL_BGRA_EXT;
  const f = bgra ? 0x1908 : format;
  const key = `${f.toString(16)}:${t.toString(16)}`;
  const known = FORMATS.get(key);
  if (!known || (bgra && t !== 0x1401)) return null;
  const sized = SIZED_OVERRIDES.get(internalFormat) === key ? internalFormat : known[0];
  return { internal: sized || f, sized, format: f, type: t, bytesPerPixel: known[1], bgra };
}

/** The compressed formats GLES and its extensions name. */
function isCompressed(f: number): boolean {
  return (
    (f >= 0x83f0 && f <= 0x83f3) || // S3TC
    (f >= 0x8c00 && f <= 0x8c03) || // PVRTC
    f === 0x8d64 || // ETC1
    (f >= 0x9270 && f <= 0x9279) || // ETC2 and EAC
    (f >= 0x93b0 && f <= 0x93dd) || // ASTC
    (f >= 0x8e8c && f <= 0x8e8f) || // BPTC
    (f >= 0x8dbb && f <= 0x8dbe) // RGTC
  );
}

/** Texture parameters WebGL2 takes, as ints. Swizzles are left out: they are the host's emulation of LUMINANCE and ALPHA, which WebGL2 has. */
const TEX_INT_PARAMS: ReadonlySet<number> = new Set([0x2801, 0x2800, 0x2802, 0x2803, 0x8072, 0x813c, 0x813d, 0x884c, 0x884d]);
/** MIN_LOD and MAX_LOD, saved as ints. */
const TEX_FLOAT_PARAMS: ReadonlySet<number> = new Set([0x813a, 0x813b]);

// ---- uniforms ----

const SAMPLER_TYPES: ReadonlySet<number> = new Set([
  0x8b5e, 0x8b5f, 0x8b60, 0x8b62, 0x8dc1, 0x8dc4, 0x8dc5, 0x8dca, 0x8dcb, 0x8dcc, 0x8dcf, 0x8dd2, 0x8dd3, 0x8dd4, 0x8dd7, 0x8d66,
]);

/** Sets one saved uniform value on the program in use; false for a type WebGL2 has no setter for. */
function setUniform(gl: GL, loc: WebGLUniformLocation | null, type: number, value: Uint8Array): boolean {
  const copy = value.slice();
  const f = new Float32Array(copy.buffer, 0, copy.byteLength >> 2);
  const i = new Int32Array(copy.buffer, 0, copy.byteLength >> 2);
  const u = new Uint32Array(copy.buffer, 0, copy.byteLength >> 2);
  if (SAMPLER_TYPES.has(type)) {
    gl.uniform1iv(loc, i.subarray(0, 1));
    return true;
  }
  switch (type) {
    case 0x1406:
      gl.uniform1fv(loc, f);
      return true;
    case 0x8b50:
      gl.uniform2fv(loc, f);
      return true;
    case 0x8b51:
      gl.uniform3fv(loc, f);
      return true;
    case 0x8b52:
      gl.uniform4fv(loc, f);
      return true;
    case 0x1404: // INT
    case 0x8b56: // BOOL
      gl.uniform1iv(loc, i);
      return true;
    case 0x8b53:
    case 0x8b57:
      gl.uniform2iv(loc, i);
      return true;
    case 0x8b54:
    case 0x8b58:
      gl.uniform3iv(loc, i);
      return true;
    case 0x8b55:
    case 0x8b59:
      gl.uniform4iv(loc, i);
      return true;
    case 0x1405:
      gl.uniform1uiv(loc, u);
      return true;
    case 0x8dc6:
      gl.uniform2uiv(loc, u);
      return true;
    case 0x8dc7:
      gl.uniform3uiv(loc, u);
      return true;
    case 0x8dc8:
      gl.uniform4uiv(loc, u);
      return true;
    // matrices: glGetUniformfv's column-major order, so never transposed
    case 0x8b5a:
      gl.uniformMatrix2fv(loc, false, f);
      return true;
    case 0x8b5b:
      gl.uniformMatrix3fv(loc, false, f);
      return true;
    case 0x8b5c:
      gl.uniformMatrix4fv(loc, false, f);
      return true;
    case 0x8b65:
      gl.uniformMatrix2x3fv(loc, false, f);
      return true;
    case 0x8b66:
      gl.uniformMatrix2x4fv(loc, false, f);
      return true;
    case 0x8b67:
      gl.uniformMatrix3x2fv(loc, false, f);
      return true;
    case 0x8b68:
      gl.uniformMatrix3x4fv(loc, false, f);
      return true;
    case 0x8b69:
      gl.uniformMatrix4x2fv(loc, false, f);
      return true;
    case 0x8b6a:
      gl.uniformMatrix4x3fv(loc, false, f);
      return true;
    default:
      return false;
  }
}

// ---- the loader ----

class Loader {
  private readonly gl: GL;
  /** One WebGL2 texture per host texture name: guest names in any share group that alias one texture share it. */
  private readonly byGlobal = new Map<number, WebGLTexture>();
  /** The live color buffer whose texture a host texture name or EGL image is. */
  private readonly cbByGlobal = new Map<number, ColorBuffer>();
  private readonly cbByImage = new Map<number, ColorBuffer>();
  private readonly blobCache = new Map<number, TextureBlob | null>();
  private readonly host: JoinHost;
  private readonly snap: GlSnapshot;
  private readonly blobs: Map<number, Uint8Array>;
  private readonly stats: JoinGlStats;

  constructor(host: JoinHost, snap: GlSnapshot, blobs: Map<number, Uint8Array>, stats: JoinGlStats) {
    this.host = host;
    this.gl = host.gl;
    this.snap = snap;
    this.blobs = blobs;
    this.stats = stats;
  }

  private skip(reason: string, n = 1): void {
    this.stats.skipped[reason] = (this.stats.skipped[reason] ?? 0) + n;
  }

  /** One object's build: a WebGL2 exception costs that object, counted, and the load goes on. */
  private guard(what: string, f: () => void): void {
    try {
      f();
    } catch {
      this.skip(`${what} failed`);
    }
  }

  run(channels: [number, ChannelState][]): void {
    const { gl, host, snap } = this;
    // uploads read client memory, tightly packed: no context's unpack state is set yet
    gl.bindBuffer(GL_PIXEL_UNPACK_BUFFER, null);
    gl.pixelStorei(0x0cf5, 1);
    gl.pixelStorei(0x0cf2, 0);
    host.touched();

    // a color buffer's pixels came as a COLOR_BUFFER part: its images name that texture
    for (const cb of snap.colorBuffers.values()) {
      const live = host.colorBuffers.get(cb.handle);
      if (!live) continue;
      for (const image of [cb.eglImage, cb.blitEglImage]) {
        if (!image) continue;
        this.cbByImage.set(image, live);
        const global = snap.eglImages.get(image);
        if (global !== undefined) this.cbByGlobal.set(global, live);
      }
    }

    const shares = new Map<number, ShareGroup>();
    for (const [id, saved] of snap.shareGroups) {
      const share = host.newShare();
      shares.set(id, share);
      this.stats.shareGroups++;
      this.buildShare(saved, share, this.elementBuffers(id));
    }

    // client images (rcCreateClientImage); a color buffer's own images are not the guest's
    for (const [image, global] of snap.eglImages) {
      if (this.cbByImage.has(image)) continue;
      const t = this.cbByGlobal.get(global)?.tex ?? this.globalTexture(global, null);
      if (!t) {
        this.skip("EGL image without a texture");
        continue;
      }
      host.images.set(image, t);
      this.stats.images++;
    }

    for (const s of snap.windowSurfaces.values()) {
      this.guard("window surface", () => {
        host.createWindowSurface(s.handle, s.width, s.height);
        this.stats.surfaces++;
        const cb = s.colorBuffer || s.attachedColorBuffer;
        if (!cb) return;
        if (host.colorBuffers.has(cb)) host.setWindowColorBuffer(s.handle, cb);
        else this.skip("window surface's color buffer not in the snapshot");
      });
    }

    const contexts = new Map<number, Ctx>();
    for (const saved of snap.contexts.values()) {
      const share = shares.get(saved.shareGroup) ?? host.newShare();
      shares.set(saved.shareGroup, share);
      contexts.set(saved.handle, host.createContext(saved.handle, share));
      this.stats.contexts++;
    }

    for (const [id, ch] of channels) {
      if (!ch.saved) {
        this.skip("channel whose thread had not finished its snapshot");
        continue;
      }
      if (!host.bindChannel(id, ch.context, ch.drawSurface, ch.readSurface) && ch.context) this.skip("channel's context not in the snapshot");
      this.stats.channels++;
    }

    for (const saved of snap.contexts.values()) {
      const ctx = contexts.get(saved.handle)!;
      if (saved.gles === 1) {
        // the mirror decodes no GLES 1 calls
        if (saved.initialized) this.skip("GLES 1 context state");
        continue;
      }
      if (saved.state && saved.v2) this.restoreContext(saved, ctx);
    }
  }

  /** Buffers a share group's vertex arrays use for indices: WebGL2 fixes a buffer's role at its first bind. */
  private elementBuffers(shareGroup: number): Set<number> {
    const out = new Set<number>();
    for (const c of this.snap.contexts.values()) {
      if (c.shareGroup !== shareGroup || !c.state) continue;
      for (const v of c.state.vaos.values()) if (v.elementArrayBuffer) out.add(v.elementArrayBuffer);
    }
    return out;
  }

  // ---- share groups ----

  private buildShare(saved: SavedShareGroup, share: ShareGroup, elements: Set<number>): void {
    const gl = this.gl;
    for (const [name, b] of saved.buffers) {
      this.guard("buffer", () => {
        // the type a vertex array's index binding needs first; a later bind as the other kind copies it
        this.host.fillBuffer(share, name, elements.has(name) ? GL_ELEMENT_ARRAY_BUFFER : GL_ARRAY_BUFFER, b.size ? b.data : null, b.usage || 0x88e4);
        this.stats.buffers++;
      });
    }

    for (const [name, t] of saved.textures) {
      if (name >= DEFAULT_TEXTURE_NAME) {
        // a target's texture 0 is WebGL2's own default texture, which takes no contents here
        if (this.blobHasPixels(t.globalName)) this.skip("default texture contents");
        continue;
      }
      this.guard("texture", () => {
        const tex = this.cbByImage.get(t.sourceEGLImage)?.tex ?? this.cbByGlobal.get(t.globalName)?.tex ?? this.globalTexture(t.globalName, t);
        if (!tex) return;
        share.textures.set(name, tex);
        this.stats.textures++;
      });
    }

    for (const [name, r] of saved.renderbuffers) {
      this.guard("renderbuffer", () => {
        const rb = gl.createRenderbuffer()!;
        share.renderbuffers.set(name, rb);
        this.stats.renderbuffers++;
        if (!r.width || !r.height) {
          // never given storage, or an EGL image's, which the snapshot does not keep
          this.skip("renderbuffer without storage");
          return;
        }
        gl.bindRenderbuffer(GL_RENDERBUFFER, rb);
        gl.renderbufferStorage(GL_RENDERBUFFER, r.internalFormat, r.width, r.height);
        gl.bindRenderbuffer(GL_RENDERBUFFER, null);
        // the snapshot has no renderbuffer pixels: depth and stencil start cleared
        this.skip("renderbuffer contents");
      });
    }
    if (saved.framebuffers.size) this.skip("share group framebuffers", saved.framebuffers.size);

    for (const [name, s] of saved.shaders) {
      this.guard("shader", () => {
        const sh = gl.createShader(s.type);
        if (!sh) {
          this.skip(`shader type ${hex(s.type)}`);
          return;
        }
        gl.shaderSource(sh, this.host.shaderSource(s.source));
        if (s.compileStatus) gl.compileShader(sh);
        share.shaders.set(name, sh);
        this.stats.shaders++;
      });
    }

    for (const [name, p] of saved.programs) this.guard("program", () => this.buildProgram(share, name, p));

    for (const [name, s] of saved.samplers) {
      this.guard("sampler", () => {
        const sampler = gl.createSampler()!;
        for (const [k, v] of s.params) gl.samplerParameteri(sampler, k, v);
        for (const [k, v] of s.floatParams) gl.samplerParameterf(sampler, k, v);
        share.samplers.set(name, sampler);
        this.stats.samplers++;
      });
    }
    this.host.touched();
  }

  private blob(global: number): TextureBlob | null {
    if (this.blobCache.has(global)) return this.blobCache.get(global)!;
    const bytes = this.blobs.get(global);
    let b: TextureBlob | null = null;
    if (bytes) {
      try {
        b = parseTexture(bytes);
      } catch {
        this.skip("TEXTURE part that does not parse");
      }
    }
    this.blobCache.set(global, b);
    return b;
  }

  private blobHasPixels(global: number): boolean {
    return this.blob(global)?.faces.some((f) => f.some((l) => l.pixels.length > 0)) ?? false;
  }

  /** The texture of a host texture name, made from its TEXTURE part once. */
  private globalTexture(global: number, t: TextureObject | null): WebGLTexture | null {
    const made = this.byGlobal.get(global);
    if (made) return made;
    const blob = this.blob(global);
    if (!t && (!blob || blob.empty)) return null;
    const tex = this.gl.createTexture()!;
    // name 0 is no host texture: nothing to share it with
    if (global) this.byGlobal.set(global, tex);
    this.fillTexture(tex, t, blob && !blob.empty ? blob : null);
    return tex;
  }

  private fillTexture(tex: WebGLTexture, t: TextureObject | null, blob: TextureBlob | null): void {
    const gl = this.gl;
    let target = blob?.target || t?.target || 0;
    if (target === GL_TEXTURE_EXTERNAL_OES) target = GL_TEXTURE_2D;
    // a name bound to no target yet: WebGL2 fixes the target at the first live bind
    if (!target) return;
    if (!TEXTURE_TARGETS.has(target)) {
      this.skip(`texture target ${hex(target)}`);
      return;
    }
    gl.activeTexture(GL_TEXTURE0 + SCRATCH_UNIT);
    gl.bindTexture(target, tex);
    try {
      if (!this.upload(target, t, blob)) this.stats.texturesWithoutPixels++;
      const params = blob?.params.size ? blob.params : (t?.params ?? new Map<number, number>());
      for (const [k, v] of params) {
        if (TEX_INT_PARAMS.has(k)) gl.texParameteri(target, k, v);
        else if (TEX_FLOAT_PARAMS.has(k)) gl.texParameterf(target, k, v);
      }
    } finally {
      gl.bindTexture(target, null);
      this.host.touched();
    }
  }

  /** Storage and pixels for every face and level; false when no level had pixels. */
  private upload(target: number, t: TextureObject | null, blob: TextureBlob | null): boolean {
    const gl = this.gl;
    const layered = target === GL_TEXTURE_3D || target === GL_TEXTURE_2D_ARRAY;
    const faceTarget = (f: number) => (target === GL_TEXTURE_CUBE_MAP ? CUBE_FACES[f]! : target);
    const src = blob ?? (t ? { internalFormat: t.internalFormat, format: t.format, type: t.type, texStorageLevels: t.texStorageLevels } : null);
    if (!src) return false;
    const fmt = pixelFormat(src.internalFormat, src.format, src.type);
    if (!fmt) {
      if (t?.compressed || isCompressed(src.internalFormat) || isCompressed(src.format)) this.stats.compressedTextures++;
      else this.skip(`texture format ${hex(src.format)}/${hex(src.type)}`);
      return true;
    }
    // without a TEXTURE part: level 0 at the size the translator kept, no contents
    const faces = blob?.faces.length
      ? blob.faces
      : t && t.width && t.height
        ? Array.from({ length: target === GL_TEXTURE_CUBE_MAP ? 6 : 1 }, () => [{ width: t.width, height: t.height, depth: Math.max(1, t.depth), pixels: new Uint8Array(0) }])
        : [];
    const level0 = faces[0]?.[0];
    // no level on the phone either: nothing missing
    if (!level0) return true;
    const levels = Math.max(...faces.map((f) => f.length));
    const storage = src.texStorageLevels > 0 && fmt.sized !== 0;
    if (storage) {
      if (layered) gl.texStorage3D(target, levels, fmt.sized, level0.width, level0.height, level0.depth);
      else gl.texStorage2D(target, levels, fmt.sized, level0.width, level0.height);
    }
    let any = false;
    faces.forEach((face, f) =>
      face.forEach((l, level) => {
        const need = l.width * l.height * l.depth * fmt.bytesPerPixel;
        const px = l.pixels.length >= need && need > 0 ? this.pixels(fmt, l.pixels.subarray(0, need)) : null;
        if (px) any = true;
        if (storage) {
          if (!px) return;
          if (layered) gl.texSubImage3D(target, level, 0, 0, 0, l.width, l.height, l.depth, fmt.format, fmt.type, px);
          else gl.texSubImage2D(faceTarget(f), level, 0, 0, l.width, l.height, fmt.format, fmt.type, px);
        } else if (layered) gl.texImage3D(target, level, fmt.internal, l.width, l.height, l.depth, 0, fmt.format, fmt.type, px);
        else gl.texImage2D(faceTarget(f), level, fmt.internal, l.width, l.height, 0, fmt.format, fmt.type, px);
      }),
    );
    return any;
  }

  private pixels(fmt: PixelFormat, px: Uint8Array): ArrayBufferView {
    if (!fmt.bgra) return this.host.view(fmt.type, px);
    const s = px.slice();
    for (let i = 0; i + 3 < s.length; i += 4) {
      const b = s[i]!;
      s[i] = s[i + 2]!;
      s[i + 2] = b;
    }
    return s;
  }

  private buildProgram(share: ShareGroup, name: number, p: ProgramObject): void {
    const gl = this.gl;
    const prog = gl.createProgram()!;
    const program: Program = { p: prog, locations: new Map(), blocks: new Map() };
    share.programs.set(name, program);
    this.stats.programs++;
    if (p.compute.linkedSource) this.skip("compute program");
    else if (p.linkStatus && p.vertex.linkedSource && p.fragment.linkedSource) this.link(program, p);
    else if (p.linkStatus) this.skip("linked program without its sources");
    // the guest's attached shaders and pending attribute bindings, for its next live link
    for (const s of [p.vertex.shader, p.fragment.shader]) {
      const sh = s ? share.shaders.get(s) : undefined;
      if (sh) gl.attachShader(prog, sh);
    }
    for (const [attr, loc] of p.boundAttribLocations) if (!attr.startsWith("gl_")) gl.bindAttribLocation(prog, loc, attr);
  }

  /**
   * Links what the phone's program runs: the sources at its last link, from shaders of the
   * loader's own (the guest's shader objects may have new sources or be gone), with the
   * attribute locations that link gave, then its uniform values and the guest's locations.
   */
  private link(program: Program, p: ProgramObject): void {
    const gl = this.gl;
    const prog = program.p;
    const temps: WebGLShader[] = [];
    for (const [type, src] of [[0x8b31, p.vertex.linkedSource], [0x8b30, p.fragment.linkedSource]] as const) {
      const sh = gl.createShader(type)!;
      gl.shaderSource(sh, this.host.shaderSource(src));
      gl.compileShader(sh);
      gl.attachShader(prog, sh);
      temps.push(sh);
    }
    for (const [attr, loc] of p.linkedAttribLocations) if (!attr.startsWith("gl_")) gl.bindAttribLocation(prog, loc, attr);
    if (p.transformFeedbackVaryings.length) gl.transformFeedbackVaryings(prog, p.transformFeedbackVaryings, p.transformFeedbackBufferMode);
    gl.linkProgram(prog);
    for (const sh of temps) {
      gl.detachShader(prog, sh);
      gl.deleteShader(sh);
    }
    if (gl.getProgramParameter(prog, GL_LINK_STATUS) === false) {
      this.stats.programsNotLinked++;
      return;
    }
    // later glUniform* calls name the guest's locations
    for (const [uniform, loc] of p.uniformLocations) if (loc >= 0) program.locations.set(loc, gl.getUniformLocation(prog, uniform));
    gl.useProgram(prog);
    this.host.touched();
    for (const u of p.uniforms) {
      const loc = gl.getUniformLocation(prog, u.guestName);
      if (loc === null) continue;
      if (!setUniform(gl, loc, u.type, u.value)) this.skip(`uniform type ${hex(u.type)}`);
    }
    gl.useProgram(null);
    // Block indices are the host driver's. WebGL2's driver is assumed to number the blocks the
    // same way, which holds when both order them by name or by declaration.
    const blocks = Number(gl.getProgramParameter(prog, GL_ACTIVE_UNIFORM_BLOCKS)) || 0;
    for (let i = 0; i < blocks; i++) program.blocks.set(i, gl.getActiveUniformBlockName(prog, i) ?? "");
    for (const [index, binding] of p.uniformBlockBindings) {
      if (index < blocks) gl.uniformBlockBinding(prog, index, binding);
      else this.skip("uniform block binding past the program's blocks");
    }
  }

  // ---- contexts ----

  /** A call through gl.ts's handler, so the setting lands in the context's shadow as a live call puts it. */
  private set(ctx: Ctx, name: string, ...args: number[]): void {
    try {
      this.host.call(ctx, name, args);
    } catch {
      this.skip(`${name} failed`);
    }
  }

  private restoreContext(saved: GlesContext, ctx: Ctx): void {
    const gl = this.gl;
    const s = saved.state!;
    const v2 = saved.v2!;
    // objects first, straight into WebGL2; then the context's settings through the handlers
    for (const [name, vao] of s.vaos) this.guard("vertex array", () => this.buildVao(ctx, name, vao));
    for (const fbo of saved.framebuffers.values()) this.guard("framebuffer", () => this.buildFramebuffer(ctx, fbo));
    for (const [name, bindings] of v2.transformFeedbacks) if (name) this.guard("transform feedback", () => this.buildTransformFeedback(ctx, name, bindings));
    gl.bindVertexArray(null);
    gl.bindFramebuffer(GL_FRAMEBUFFER, null);
    gl.bindBuffer(GL_ARRAY_BUFFER, null);
    this.host.touched();

    const caps = this.host.caps;
    for (const [cap, on] of s.enables) {
      // WebGL2 always restarts primitives at the fixed index
      if (cap === GL_PRIMITIVE_RESTART_FIXED_INDEX) continue;
      if (caps.includes(cap)) this.set(ctx, on ? "glEnable" : "glDisable", cap);
      else if (on) this.skip(`capability ${hex(cap)}`);
    }
    const blend = s.blend[0];
    if (blend) {
      if (!s.enables.has(GL_BLEND)) this.set(ctx, blend.enabled ? "glEnable" : "glDisable", GL_BLEND);
      this.set(ctx, "glBlendEquationSeparate", blend.equationRgb, blend.equationAlpha);
      this.set(ctx, "glBlendFuncSeparate", blend.srcRgb, blend.dstRgb, blend.srcAlpha, blend.dstAlpha);
      this.set(ctx, "glColorMask", ...blend.colorMask.map(Number));
      const same = (b: typeof blend) => JSON.stringify(b) === JSON.stringify(blend);
      if (!s.blend.every(same)) this.skip("per draw buffer blend state");
    }
    this.set(ctx, "glCullFace", s.cullFace);
    this.set(ctx, "glFrontFace", s.frontFace);
    this.set(ctx, "glDepthFunc", s.depthFunc);
    this.set(ctx, "glDepthMask", Number(s.depthMask));
    this.set(ctx, "glDepthRangef", s.depthRange[0], s.depthRange[1]);
    this.set(ctx, "glLineWidth", s.lineWidth);
    this.set(ctx, "glPolygonOffset", s.polygonOffset.factor, s.polygonOffset.units);
    this.set(ctx, "glSampleCoverage", s.sampleCoverage.value, Number(s.sampleCoverage.invert));
    for (const [face, st] of [[0x0404, s.stencil.front], [0x0405, s.stencil.back]] as const) {
      this.set(ctx, "glStencilFuncSeparate", face, st.func, st.ref, st.funcMask);
      this.set(ctx, "glStencilOpSeparate", face, st.sfail, st.dpfail, st.dppass);
      this.set(ctx, "glStencilMaskSeparate", face, st.writeMask);
    }
    this.set(ctx, "glClearColor", ...s.clearColor);
    this.set(ctx, "glClearDepthf", s.clearDepth);
    this.set(ctx, "glClearStencil", s.clearStencil);
    for (const [pname, v] of s.pixelStore) this.set(ctx, "glPixelStorei", pname, v);
    // a context that never set them has the size of the surface it was first made current on
    const surface = ctx.draw ?? this.drawSurfaceOf(saved.handle);
    const whole = surface ? { x: 0, y: 0, width: surface.w, height: surface.h } : null;
    const vp = s.viewport ?? whole;
    if (vp) this.set(ctx, "glViewport", vp.x, vp.y, vp.width, vp.height);
    const sc = s.scissor ?? whole;
    if (sc) this.set(ctx, "glScissor", sc.x, sc.y, sc.width, sc.height);

    const b = s.buffers;
    this.set(ctx, "glBindBuffer", GL_ARRAY_BUFFER, b.array);
    for (const [target, name] of [[0x8f36, b.copyRead], [0x8f37, b.copyWrite], [0x88eb, b.pixelPack], [GL_PIXEL_UNPACK_BUFFER, b.pixelUnpack], [GL_UNIFORM_BUFFER, b.uniform], [GL_TRANSFORM_FEEDBACK_BUFFER, b.transformFeedback]] as const)
      if (name) this.set(ctx, "glBindBuffer", target, name);
    const gles31 = [b.atomicCounter, b.dispatchIndirect, b.drawIndirect, b.shaderStorage, b.textureBuffer].filter(Boolean).length;
    if (gles31) this.skip("GLES 3.1 buffer binding", gles31);
    this.indexed(ctx, GL_UNIFORM_BUFFER, s.indexed.uniform);
    this.indexed(ctx, GL_TRANSFORM_FEEDBACK_BUFFER, s.indexed.transformFeedback);
    const indexed31 = [...s.indexed.atomicCounter, ...s.indexed.shaderStorage].filter((x) => x.buffer).length;
    if (indexed31) this.skip("GLES 3.1 indexed buffer binding", indexed31);

    s.textureUnits.forEach((unit, u) =>
      unit.textures.forEach((name, k) => {
        if (!name) return;
        const target = TEXTURE_UNIT_TARGETS[k]!;
        if (!TEXTURE_TARGETS.has(target)) {
          this.skip(`texture binding ${hex(target)}`);
          return;
        }
        this.set(ctx, "glActiveTexture", GL_TEXTURE0 + u);
        this.set(ctx, "glBindTexture", target, name);
      }),
    );
    this.set(ctx, "glActiveTexture", GL_TEXTURE0 + s.activeTexture);
    for (const [unit, name] of v2.samplers) if (name) this.set(ctx, "glBindSampler", unit, name);
    if (v2.program) this.set(ctx, "glUseProgram", v2.program);
    this.set(ctx, "glBindVertexArray", s.boundVao);
    this.set(ctx, "glBindFramebuffer", GL_DRAW_FRAMEBUFFER, s.drawFramebuffer);
    this.set(ctx, "glBindFramebuffer", GL_READ_FRAMEBUFFER, s.readFramebuffer);
    if (s.renderbuffer) this.set(ctx, "glBindRenderbuffer", GL_RENDERBUFFER, s.renderbuffer);

    // Constant attribute values: the translator keeps them in the bound vertex array; WebGL2,
    // like gl.ts's glVertexAttrib* handlers, holds them once for every context.
    const bound = s.vaos.get(s.boundVao);
    bound?.attribs.forEach((a, i) => {
      if (a.attribType !== 2 || a.value.length !== 4) return;
      if (a.isInt) this.skip("integer constant attribute");
      else gl.vertexAttrib4fv(i, a.value);
    });
    if (v2.attribute0Changed) gl.vertexAttrib4fv(0, v2.attribute0);
  }

  private indexed(ctx: Ctx, target: number, bindings: BufferBinding[]): void {
    bindings.forEach((x, i) => {
      if (!x.buffer) return;
      if (x.isBindBase || !x.size) this.set(ctx, "glBindBufferBase", target, i, x.buffer);
      else this.set(ctx, "glBindBufferRange", target, i, x.buffer, x.offset, x.size);
    });
  }

  private drawSurfaceOf(context: number): { w: number; h: number } | null {
    for (const s of this.snap.windowSurfaces.values()) if (s.drawContext === context) return { w: s.width, h: s.height };
    return null;
  }

  private buildVao(ctx: Ctx, name: number, vao: VaoState): void {
    const gl = this.gl;
    const share = ctx.share;
    gl.bindVertexArray(this.host.vaoOf(ctx, name));
    this.stats.vertexArrays++;
    if (vao.elementArrayBuffer) {
      const eb = this.host.bufferAt(share, vao.elementArrayBuffer, GL_ELEMENT_ARRAY_BUFFER);
      if (eb) gl.bindBuffer(GL_ELEMENT_ARRAY_BUFFER, eb);
      else this.skip("element buffer not in the snapshot");
    }
    vao.attribs.forEach((a, i) => {
      // GLES3: the attribute's vertex binding holds its buffer, offset, stride and divisor
      const binding = vao.bindings[a.bindingIndex];
      if (a.attribType === 1) {
        const buf = this.host.bufferAt(share, binding?.buffer || a.buffer, GL_ARRAY_BUFFER);
        if (!buf) {
          if (a.enabled) this.skip("vertex attribute buffer not in the snapshot");
        } else if (a.type === GL_FIXED) {
          this.skip("GL_FIXED vertex attribute");
        } else {
          const offset = (binding ? binding.offset : a.offset) + a.relativeOffset;
          const stride = binding ? binding.stride : a.stride;
          gl.bindBuffer(GL_ARRAY_BUFFER, buf);
          if (a.isInt) gl.vertexAttribIPointer(i, a.size, a.type, stride, offset);
          else gl.vertexAttribPointer(i, a.size, a.type, a.normalize, stride, offset);
        }
      } else if (a.attribType === 0 && a.enabled) {
        this.stats.clientVertexPointers++;
      }
      const divisor = binding?.divisor ?? a.divisor;
      if (divisor) gl.vertexAttribDivisor(i, divisor);
      if (a.enabled) gl.enableVertexAttribArray(i);
    });
  }

  private buildFramebuffer(ctx: Ctx, fbo: FramebufferObject): void {
    const gl = this.gl;
    if (!fbo.name) return;
    gl.bindFramebuffer(GL_FRAMEBUFFER, this.host.fboOf(ctx, fbo.name));
    this.stats.framebuffers++;
    fbo.attachments.forEach((a, slot) => {
      if (!a.target) return;
      const point = attachmentPoint(slot);
      if (a.owned || point === null) {
        // a host object of the translator's, or an attachment point WebGL2 has not
        this.skip("framebuffer attachment");
        return;
      }
      if (a.target === GL_RENDERBUFFER) {
        const r = ctx.share.renderbuffers.get(a.name);
        if (isColorBuffer(r)) gl.framebufferTexture2D(GL_FRAMEBUFFER, point, GL_TEXTURE_2D, r.tex, 0);
        else gl.framebufferRenderbuffer(GL_FRAMEBUFFER, point, GL_RENDERBUFFER, r ?? null);
        return;
      }
      const t = ctx.share.textures.get(a.name) ?? null;
      if (a.target === GL_TEXTURE_3D || a.target === GL_TEXTURE_2D_ARRAY) {
        // the snapshot keeps no attachment level or layer
        gl.framebufferTextureLayer(GL_FRAMEBUFFER, point, t, 0, 0);
        this.skip("layered attachment's level and layer (taken as 0)");
      } else {
        gl.framebufferTexture2D(GL_FRAMEBUFFER, point, a.target === GL_TEXTURE_EXTERNAL_OES ? GL_TEXTURE_2D : a.target, t, 0);
      }
    });
    if (fbo.hasDrawBuffers && fbo.drawBuffers.length) gl.drawBuffers(fbo.drawBuffers);
    if (fbo.readBuffer) gl.readBuffer(fbo.readBuffer);
  }

  private buildTransformFeedback(ctx: Ctx, name: number, bindings: BufferBinding[]): void {
    const gl = this.gl;
    const tf = gl.createTransformFeedback()!;
    ctx.tfbs.set(name, tf);
    gl.bindTransformFeedback(GL_TRANSFORM_FEEDBACK, tf);
    bindings.forEach((x, i) => {
      const buf = x.buffer ? this.host.bufferAt(ctx.share, x.buffer, GL_TRANSFORM_FEEDBACK_BUFFER) : null;
      if (!buf) return;
      if (x.isBindBase || !x.size) gl.bindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, i, buf);
      else gl.bindBufferRange(GL_TRANSFORM_FEEDBACK_BUFFER, i, buf, x.offset, x.size);
    });
    gl.bindTransformFeedback(GL_TRANSFORM_FEEDBACK, null);
  }
}
