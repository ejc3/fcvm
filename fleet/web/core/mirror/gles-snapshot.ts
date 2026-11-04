// gfxstream's GLES snapshot stream as plain data, for the GPU mirror's join: the GL_STATE,
// TEXTURE and CHANNEL parts of a JOIN (ejc3/gfxstream host/mirror_tap.h,
// RendererImpl::saveMirrorJoin in host/renderer_impl.cpp). The browser rebuilds the phone's GL
// state on WebGL2 from what this returns. Portable: no DOM, no WebGL, no clock.
//
// Encoding (host/include/render-utils/stream.h): putBe16/32/64 are big-endian; putFloat and raw
// struct or array writes are host order, little-endian on every host gfxstream runs on. No
// record carries a length, so every record is parsed, kept or not, to stay aligned. Collections
// come from unordered maps, so their order means nothing. Cites are to the gfxstream tree, with
// GLC = host/gl/glestranslator/common/gles_context.cpp.
//
// GL_STATE is FrameBuffer::onSave's stream as the join calls it: GL emulation present, the
// translator's save entry points present, and Vulkan snapshots off (the join sends no GL state
// with them on), so the stream has no Vulkan records and ends at its magic. It is onSave's own
// output, so the U8 mStopped that RendererImpl::save puts first is not there. (The format notes
// in .scratch/gles-save-format.md predate the join's onSave call; where they and the source
// differ, the source is followed and the comment says so.)

/** kFramebufferSnapshotVersion (host/include/render-utils/snapshot_operations.h). */
export const GLES_SNAPSHOT_VERSION = 6;
const SNAPSHOT_MAGIC = 0xc0ffeeee;
const COLOR_BUFFER_MAGIC = 0xcafeface;

/** GLESApi (host/common/include/gfxstream/host/gl_enums.h). */
export const GLES_API = { cm: 1, gles2: 2, gles3_0: 3, gles3_1: 4, gles3_2: 5 } as const;

/** The texture targets of a texture unit, in the order the stream holds them (TextureTarget). */
export const TEXTURE_UNIT_TARGETS = [0x0de1, 0x8513, 0x8c1a, 0x806f, 0x9100, 0x8c2a] as const;

/** A share group's "texture 0" of target k (TEXTURE_UNIT_TARGETS order) is keyed 2^32 + k. */
export const DEFAULT_TEXTURE_NAME = 0x1_0000_0000;

/** The cube map faces in a texture blob's order. */
export const CUBE_FACES = [0x8515, 0x8516, 0x8517, 0x8518, 0x8519, 0x851a] as const;

/** The attachment point of framebuffer slot i (framebuffer_data.cpp attachmentPointIndex); slot 19 is any other. */
export function attachmentPoint(slot: number): number | null {
  if (slot === 0) return 0x8ce0;
  if (slot === 1) return 0x8d00;
  if (slot === 2) return 0x8d20;
  if (slot === 3) return 0x821a;
  if (slot >= 4 && slot <= 18) return 0x8ce1 + slot - 4;
  return null;
}

// ---- data model ----

export interface BufferBinding {
  buffer: number;
  /** offset, size and stride are 64-bit in the translator and truncated to 32 bits in the stream. */
  offset: number;
  size: number;
  stride: number;
  divisor: number;
  isBindBase: boolean;
}

/** GLESpointer: one vertex attribute's format and source (common/gles_pointer.cpp onSave). */
export interface VertexAttrib {
  size: number;
  type: number;
  stride: number;
  enabled: boolean;
  normalize: boolean;
  /** 0 client array, 1 buffer, 2 constant value. */
  attribType: number;
  buffer: number;
  /** For a client array: the bytes the last draw copied, often empty. Null otherwise. */
  clientData: Uint8Array | null;
  offset: number;
  isInt: boolean;
  divisor: number;
  /** The vertex binding (VaoState.bindings index) that holds this attribute's buffer, offset, stride and divisor. */
  bindingIndex: number;
  relativeOffset: number;
  /** The constant value when attribType is 2. */
  value: number[];
}

export interface VaoState {
  elementArrayBuffer: number;
  /** 16 attributes (kMaxVertexAttributes). */
  attribs: VertexAttrib[];
  /** GLES1's legacy arrays by array type; null for GLES2+ VAOs. */
  legacyArrays: Map<number, VertexAttrib> | null;
  bindings: BufferBinding[];
  bufferBacked: boolean;
  everBound: boolean;
}

export interface BlendState {
  enabled: boolean;
  equationRgb: number;
  equationAlpha: number;
  srcRgb: number;
  dstRgb: number;
  srcAlpha: number;
  dstAlpha: number;
  colorMask: [boolean, boolean, boolean, boolean];
}

export interface StencilFace {
  func: number;
  ref: number;
  funcMask: number;
  writeMask: number;
  sfail: number;
  dpfail: number;
  dppass: number;
}

export interface TextureUnit {
  /** Guest texture name per TEXTURE_UNIT_TARGETS entry; 0 is the share group's default texture. */
  textures: number[];
  /** GLES1's per-target enables, same order. */
  enabled: boolean[];
}

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** GLEScontext state, written once the context has been made current (GLC onSave). */
export interface ContextState {
  /** Unit index (GL_TEXTUREi - GL_TEXTURE0). */
  activeTexture: number;
  /** By guest VAO name; 0 is the default VAO. */
  vaos: Map<number, VaoState>;
  boundVao: number;
  buffers: {
    array: number;
    elementArray: number;
    copyRead: number;
    copyWrite: number;
    pixelPack: number;
    pixelUnpack: number;
    transformFeedback: number;
    uniform: number;
    atomicCounter: number;
    dispatchIndirect: number;
    drawIndirect: number;
    shaderStorage: number;
    textureBuffer: number;
  };
  indexed: {
    transformFeedback: BufferBinding[];
    uniform: BufferBinding[];
    atomicCounter: BufferBinding[];
    shaderStorage: BufferBinding[];
  };
  /** Null until glViewport was called. */
  viewport: Rect | null;
  /** Null until glScissor was called. */
  scissor: Rect | null;
  polygonOffset: { factor: number; units: number };
  /** Capabilities set with glEnable/glDisable, by GLenum. */
  enables: Map<number, boolean>;
  /** One per draw buffer when indexed blending is offered, else one. */
  blend: BlendState[];
  pixelStore: Map<number, number>;
  cullFace: number;
  frontFace: number;
  depthFunc: number;
  depthMask: boolean;
  depthRange: [number, number];
  lineWidth: number;
  sampleCoverage: { value: number; invert: boolean };
  stencil: { front: StencilFace; back: StencilFace };
  clearColor: [number, number, number, number];
  clearDepth: number;
  clearStencil: number;
  glError: number;
  maxTexUnits: number;
  maxUsedTexUnit: number;
  textureUnits: TextureUnit[];
  renderbuffer: number;
  drawFramebuffer: number;
  readFramebuffer: number;
  defaultDrawBuffer: number;
  defaultReadBuffer: number;
}

/** GLESv2Context's tail (gles_v2/gles_v2_context.cpp onSave). */
export interface V2State {
  attribute0: [number, number, number, number];
  attribute0Changed: boolean;
  /** Floats of the core-profile attribute 0 emulation array (skipped). */
  attribute0ArrayLength: number;
  attribute0NeedsDisable: boolean;
  program: number;
  /** Guest sampler name by texture unit. */
  samplers: Map<number, number>;
  /** By guest name; 0 is the default transform feedback on GLES3. Indexed buffer bindings only. */
  transformFeedbacks: Map<number, BufferBinding[]>;
}

export interface GlesContext {
  /** rcCreateContext's handle. */
  handle: number;
  /** GLESApi: 1 GLES1, 2 GLES2, 3..5 GLES 3.0..3.2. */
  api: number;
  gles: 1 | 2 | 3;
  /** False for a context never made current; state is then null. */
  initialized: boolean;
  glesMajor: number;
  glesMinor: number;
  state: ContextState | null;
  /** Framebuffer objects are per context, by guest name. */
  framebuffers: Map<number, FramebufferObject>;
  /** Null for GLES1, whose tail is parsed and dropped. */
  v2: V2State | null;
  eglConfigId: number;
  shareGroup: number;
}

export interface TextureObject {
  /** GL_TEXTURE_CUBE_MAP for cube maps, 0 for a texture never bound with a target. */
  target: number;
  width: number;
  height: number;
  depth: number;
  border: number;
  internalFormat: number;
  format: number;
  type: number;
  /** The EGL image the texture shows (a color buffer's eglImage for gralloc buffers), 0 for none. */
  sourceEGLImage: number;
  hasStorage: boolean;
  wasBound: boolean;
  requiresAutoMipmap: boolean;
  compressed: boolean;
  compressedFormat: number;
  cropRect: [number, number, number, number];
  /** 0 for a mutable texture. */
  texStorageLevels: number;
  /** The host's texture name: the key of the TEXTURE parts, and shared by aliases of one texture. */
  globalName: number;
  /** Only the parameters set with glTexParameterf*, as ints; the blob holds the full list. */
  params: Map<number, number>;
}

export interface BufferObject {
  size: number;
  usage: number;
  /** A view into the GL_STATE bytes. */
  data: Uint8Array;
  wasBound: boolean;
}

export interface RenderbufferObject {
  attachedFramebuffer: number;
  attachedPoint: number;
  width: number;
  height: number;
  internalFormat: number;
  hostInternalFormat: number;
  everBound: boolean;
}

export interface FramebufferAttachment {
  /** The texture target (cube face included) or GL_RENDERBUFFER; 0 when the slot is empty. */
  target: number;
  /** Guest texture or renderbuffer name, unless owned (a host object). */
  name: number;
  /** NamedObjectType of the attached object: 3 for a renderbuffer, 0 for a texture or none. */
  objectType: number;
  owned: boolean;
}

export interface FramebufferObject {
  name: number;
  /** 20 slots; attachmentPoint(slot) gives each one's GLenum. */
  attachments: FramebufferAttachment[];
  dirty: boolean;
  hasBeenBound: boolean;
  hasDrawBuffers: boolean;
  drawBuffers: number[];
  readBuffer: number;
}

export interface ShaderObject {
  type: number;
  /** The guest's ESSL as given to glShaderSource. */
  source: string;
  infoLog: string;
  /** Guest programs the shader is attached to. */
  programs: number[];
  compileStatus: boolean;
  deleteStatus: boolean;
}

export interface UniformValue {
  /** The host driver's location: not the guest's; match by guestName. */
  hostLocation: number;
  count: number;
  transpose: boolean;
  type: number;
  /** One element of the uniform's type in host order (floats, ints; samplers as ints). */
  value: Uint8Array;
  guestName: string;
}

export interface AttachedShader {
  /** Guest shader name, 0 for none. */
  shader: number;
  /** The guest's source at the last link: what the program runs. */
  linkedSource: string;
}

export interface ProgramObject {
  /** glBindAttribLocation calls not yet linked. */
  boundAttribLocations: Map<string, number>;
  linkedAttribLocations: Map<string, number>;
  uniforms: UniformValue[];
  /** Uniform block binding by the host's block index. */
  uniformBlockBindings: Map<number, number>;
  transformFeedbackVaryings: string[];
  transformFeedbackBufferMode: number;
  vertex: AttachedShader;
  fragment: AttachedShader;
  compute: AttachedShader;
  infoLog: string;
  validateStatus: boolean;
  linkStatus: boolean;
  inUse: boolean;
  deleteStatus: boolean;
  glesMajor: number;
  glesMinor: number;
  /** Uniform locations the guest was given, by guest uniform name. */
  uniformLocations: Map<string, number>;
}

export interface SamplerObject {
  params: Map<number, number>;
  floatParams: Map<number, number>;
}

/** One share group's objects, each map by guest name. */
export interface ShareGroup {
  id: number;
  buffers: Map<number, BufferObject>;
  /** Default textures are keyed DEFAULT_TEXTURE_NAME + target index. */
  textures: Map<number, TextureObject>;
  renderbuffers: Map<number, RenderbufferObject>;
  /** Empty in practice: framebuffers live in each context. */
  framebuffers: Map<number, FramebufferObject>;
  shaders: Map<number, ShaderObject>;
  programs: Map<number, ProgramObject>;
  samplers: Map<number, SamplerObject>;
}

export interface ColorBufferState {
  handle: number;
  width: number;
  height: number;
  /** GfxstreamFormat. */
  format: number;
  /** The EGL image of the color buffer's texture: the key into GlSnapshot.eglImages. */
  eglImage: number;
  blitEglImage: number;
  refcount: number;
  opened: boolean;
}

export interface WindowSurface {
  handle: number;
  attachedColorBuffer: number;
  /** Context handles current with this surface on some render thread, 0 for none. */
  readContext: number;
  drawContext: number;
  width: number;
  height: number;
  eglConfigId: number;
  colorBuffer: number;
}

export interface ProcOwned {
  puid: bigint;
  handles: number[];
}

export interface GlSnapshot {
  version: number;
  /** Host texture names whose blobs come as TEXTURE parts. */
  textureIds: number[];
  /** Global texture name by EGL image id. */
  eglImages: Map<number, number>;
  framebufferWidth: number;
  framebufferHeight: number;
  dpr: number;
  /** By rc handle, in stream order. */
  contexts: Map<number, GlesContext>;
  /** By share group id (GlesContext.shareGroup). */
  shareGroups: Map<number, ShareGroup>;
  colorBuffers: Map<number, ColorBufferState>;
  lastPostedColorBuffer: number;
  windowSurfaces: Map<number, WindowSurface>;
  procOwned: { windowSurfaces: ProcOwned[]; colorBuffers: ProcOwned[]; eglImages: ProcOwned[]; contexts: ProcOwned[] };
}

export interface TextureLevel {
  width: number;
  height: number;
  /** Layers for 3D and 2D array textures, else 1. */
  depth: number;
  /** Tightly packed rows of format/type, GL's row 0 first; a view into the part. Empty: no content. */
  pixels: Uint8Array;
}

export interface TextureBlob {
  /** True when the saver wrote nothing (the global name had no saveable texture). */
  empty: boolean;
  target: number;
  width: number;
  height: number;
  depth: number;
  format: number;
  internalFormat: number;
  type: number;
  border: number;
  texStorageLevels: number;
  maxMipmapLevel: number;
  /** faces[face][level]: six faces (CUBE_FACES order) for a cube map, else one; empty for an unsupported target. */
  faces: TextureLevel[][];
  /** Parameters read back from the host, as ints. Empty for an unsupported target. */
  params: Map<number, number>;
}

export interface ChannelState {
  /** False when the thread had not finished its snapshot (RenderThread::save wrote 0). */
  saved: boolean;
  context: number;
  drawSurface: number;
  readSurface: number;
  /** Contexts and window surfaces this thread created outside any process (puid 0). */
  contexts: number[];
  windowSurfaces: number[];
  puid: bigint;
  /** Bytes the decoder held unconsumed, then unread and unsent stream bytes. */
  pendingReadBytes: number;
  unreadStreamBytes: number;
  pendingWriteBytes: number;
  checksumVersion: number;
  /** The Vulkan context id, null without Vulkan. */
  vkContextId: number | null;
}

// ---- reader ----

class Reader {
  pos = 0;
  private readonly v: DataView;
  private readonly b: Uint8Array;
  private readonly what: string;

  constructor(b: Uint8Array, what: string) {
    this.b = b;
    this.what = what;
    this.v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  }

  get left(): number {
    return this.b.length - this.pos;
  }

  fail(msg: string, at = this.pos): never {
    throw new Error(`${this.what}: ${msg} at byte ${at}`);
  }

  private take(n: number, field: string): number {
    if (n > this.left) this.fail(`${field}: needs ${n} bytes, ${this.left} left`);
    const at = this.pos;
    this.pos += n;
    return at;
  }

  u8(f: string): number {
    return this.v.getUint8(this.take(1, f));
  }
  bool(f: string): boolean {
    return this.u8(f) !== 0;
  }
  be16(f: string): number {
    return this.v.getUint16(this.take(2, f), false);
  }
  be32(f: string): number {
    return this.v.getUint32(this.take(4, f), false);
  }
  i32(f: string): number {
    return this.v.getInt32(this.take(4, f), false);
  }
  be64(f: string): bigint {
    return this.v.getBigUint64(this.take(8, f), false);
  }
  /** A BE64 that must fit a JS number exactly (names, ids, counts). */
  num64(f: string): number {
    const at = this.pos;
    const x = this.be64(f);
    if (x > BigInt(Number.MAX_SAFE_INTEGER)) this.fail(`${f}: ${x} is too large`, at);
    return Number(x);
  }
  /** Host-order (little-endian) float, int and uint, for putFloat and raw struct writes. */
  f32(f: string): number {
    return this.v.getFloat32(this.take(4, f), true);
  }
  raw(n: number, f: string): Uint8Array {
    const at = this.take(n, f);
    return this.b.subarray(at, at + n);
  }
  str(f: string): string {
    const n = this.be32(`${f} length`);
    return utf8(this.raw(n, f));
  }
  /** A BE32 element count, refused when count * minEach bytes cannot follow. */
  count(f: string, minEach: number): number {
    const at = this.pos;
    const n = this.be32(`${f} count`);
    if (n * minEach > this.left) this.fail(`${f}: count ${n} does not fit in the ${this.left} bytes left`, at);
    return n;
  }
  done(): void {
    if (this.left !== 0) this.fail(`${this.left} bytes after the end`);
  }
}

/** UTF-8 to a string; invalid sequences become U+FFFD. */
function utf8(b: Uint8Array): string {
  const units: number[] = [];
  let s = "";
  for (let i = 0; i < b.length; ) {
    const c = b[i]!;
    let cp = 0xfffd;
    let len = 1;
    if (c < 0x80) cp = c;
    else if (c >= 0xc2 && c < 0xf5) {
      len = c < 0xe0 ? 2 : c < 0xf0 ? 3 : 4;
      let x = c & (0xff >> (len + 1));
      let ok = i + len <= b.length;
      for (let k = 1; ok && k < len; k++) {
        const d = b[i + k]!;
        if ((d & 0xc0) !== 0x80) ok = false;
        else x = (x << 6) | (d & 0x3f);
      }
      const min = len === 2 ? 0x80 : len === 3 ? 0x800 : 0x10000;
      if (ok && x >= min && x <= 0x10ffff && (x < 0xd800 || x > 0xdfff)) cp = x;
      else len = 1;
    }
    i += len;
    if (cp > 0xffff) units.push(0xd800 + ((cp - 0x10000) >> 10), 0xdc00 + ((cp - 0x10000) & 0x3ff));
    else units.push(cp);
    if (units.length >= 4096) {
      s += String.fromCharCode(...units);
      units.length = 0;
    }
  }
  return s + String.fromCharCode(...units);
}

function list<T>(r: Reader, f: string, minEach: number, item: (r: Reader) => T): T[] {
  const n = r.count(f, minEach);
  const out: T[] = [];
  for (let i = 0; i < n; i++) out.push(item(r));
  return out;
}

/** A COLL of (key, value) pairs into a map; a repeated key is corruption. */
function pairs<K, V>(r: Reader, f: string, minEach: number, key: (r: Reader) => K, value: (r: Reader, key: K) => V): Map<K, V> {
  const n = r.count(f, minEach);
  const out = new Map<K, V>();
  for (let i = 0; i < n; i++) {
    const at = r.pos;
    const k = key(r);
    if (out.has(k)) r.fail(`${f}: key ${String(k)} repeated`, at);
    out.set(k, value(r, k));
  }
  return out;
}

// ---- GL_STATE ----

/**
 * The GL_STATE part (FrameBuffer::onSave, host/frame_buffer.cpp). Throws on a malformed stream,
 * an unknown version or GLES API, and on any byte after the closing magic.
 */
export function parseGlState(bytes: Uint8Array): GlSnapshot {
  const r = new Reader(bytes, "GL state");
  const version = r.be32("version");
  if (version !== GLES_SNAPSHOT_VERSION) r.fail(`unknown snapshot version ${version}, expected ${GLES_SNAPSHOT_VERSION}`, 0);

  // eglSaveAllImages: GlobalNameSpace::onSave, then the EGL images (egl/egl_display.cpp).
  const textureIds = list(r, "texture index", 4, (r) => r.be32("texture id"));
  const eglImages = pairs(r, "EGL images", 8, (r) => r.be32("image id"), (r) => r.be32("image texture"));

  const framebufferWidth = r.be32("framebuffer width");
  const framebufferHeight = r.be32("framebuffer height");
  const dpr = r.f32("dpr");
  r.be32("active display config");
  list(r, "display configs", 20, (r) => r.raw(20, "display config"));
  list(r, "display ids", 4, (r) => r.be32("display id"));
  r.be32("use subwindow");
  r.be32("obsolete context flag");
  r.be32("fps stats");
  r.be32("stats frames");
  r.be64("stats start");

  const shareGroups = new Map<number, ShareGroup>();
  const contexts = pairs(r, "contexts", 9, (r) => r.be32("context handle"), (r, handle) => context(r, handle, shareGroups));

  r.u8("guest managed color buffer lifetime");
  const colorBuffers = pairs(r, "color buffers", 49, (r) => {
    const at = r.pos;
    if (r.be32("color buffer magic") !== COLOR_BUFFER_MAGIC) r.fail("color buffer magic is not 0xCAFEFACE", at);
    return r.be32("color buffer handle");
  }, colorBuffer);
  const lastPostedColorBuffer = r.be32("last posted color buffer");
  const windowSurfaces = pairs(r, "window surfaces", 32, (r) => r.be32("surface handle"), windowSurface);
  const procOwned = {
    windowSurfaces: list(r, "process window surfaces", 12, procEntry),
    colorBuffers: list(r, "process color buffers", 12, procEntry),
    eglImages: list(r, "process EGL images", 12, procEntry),
    contexts: list(r, "process contexts", 12, procEntry),
  };
  const at = r.pos;
  const resources = r.num64("process resources count");
  if (resources * 12 > r.left) r.fail(`process resources: count ${resources} does not fit in the ${r.left} bytes left`, at);
  r.raw(resources * 12, "process resources");
  list(r, "fence syncs", 8, (r) => r.be64("fence sync"));
  const magicAt = r.pos;
  if (r.be32("magic") !== SNAPSHOT_MAGIC) r.fail("closing magic is not 0xC0FFEEEE", magicAt);
  r.done();
  return {
    version,
    textureIds,
    eglImages,
    framebufferWidth,
    framebufferHeight,
    dpr,
    contexts,
    shareGroups,
    colorBuffers,
    lastPostedColorBuffer,
    windowSurfaces,
    procOwned,
  };
}

function procEntry(r: Reader): ProcOwned {
  return { puid: r.be64("process id"), handles: list(r, "process handles", 4, (r) => r.be32("handle")) };
}

// ColorBuffer::Impl::onSave (host/color_buffer.cpp) after the magic and handle, ColorBufferGl::onSave
// (always present with GL emulation), then FrameBuffer's refcount fields. ColorBufferVk writes
// nothing with Vulkan snapshots off.
function colorBuffer(r: Reader, handle: number): ColorBufferState {
  const width = r.be32("color buffer width");
  const height = r.be32("color buffer height");
  const format = r.be32("color buffer format");
  r.be32("color buffer GL handle");
  r.be32("color buffer GL width");
  r.be32("color buffer GL height");
  r.be32("color buffer GL format");
  const eglImage = r.be32("color buffer EGL image");
  const blitEglImage = r.be32("color buffer blit EGL image");
  r.be32("color buffer format check");
  const refcount = r.be32("color buffer refcount");
  const opened = r.bool("color buffer opened");
  r.be32("color buffer closed age");
  return { handle, width, height, format, eglImage, blitEglImage, refcount, opened };
}

// EmulatedEglWindowSurface::onSave with eglSaveConfig, then EmulationGl's color buffer.
function windowSurface(r: Reader, handle: number): WindowSurface {
  return {
    handle,
    attachedColorBuffer: r.be32("surface color buffer"),
    readContext: r.be32("surface read context"),
    drawContext: r.be32("surface draw context"),
    width: r.be32("surface width"),
    height: r.be32("surface height"),
    eglConfigId: r.be32("surface config"),
    colorBuffer: r.be32("surface window color buffer"),
  };
}

// EmulatedEglContext::onSave (host/gl/emulated_egl_context.cpp), then EglContext::onSave
// (egl/egl_context.cpp): the GLES context, config id, share group id, and the share group's
// objects if no earlier context in the stream carried them.
function context(r: Reader, handle: number, shareGroups: Map<number, ShareGroup>): GlesContext {
  const apiAt = r.pos;
  const api = r.be32("context GLES API");
  if (api < GLES_API.cm || api > GLES_API.gles3_2) r.fail(`unknown GLES API ${api}`, apiAt);
  const initialized = r.bool("context initialized");
  const glesMajor = r.be32("context GLES major");
  const glesMinor = r.be32("context GLES minor");
  const state = initialized ? contextState(r) : null;
  const framebuffers = nameSpace(r, "context framebuffers", 5, framebuffer);
  let v2: V2State | null = null;
  if (api === GLES_API.cm) skipGles1Tail(r, initialized);
  else v2 = v2Tail(r);
  const eglConfigId = r.be32("context config");
  const shareGroup = r.num64("share group id");
  if (!shareGroups.has(shareGroup)) shareGroups.set(shareGroup, shareGroupBody(r, shareGroup));
  return {
    handle,
    api,
    gles: api === GLES_API.cm ? 1 : api === GLES_API.gles2 ? 2 : 3,
    initialized,
    glesMajor,
    glesMinor,
    state,
    framebuffers,
    v2,
    eglConfigId,
    shareGroup,
  };
}

function bufferBinding(r: Reader): BufferBinding {
  return {
    buffer: r.be32("binding buffer"),
    offset: r.be32("binding offset"),
    size: r.be32("binding size"),
    stride: r.be32("binding stride"),
    divisor: r.be32("binding divisor"),
    isBindBase: r.bool("binding base"),
  };
}

// GLESpointer::onSave (common/gles_pointer.cpp).
function vertexAttrib(r: Reader): VertexAttrib {
  const size = r.i32("attrib size");
  const type = r.be32("attrib type");
  const stride = r.i32("attrib stride");
  const enabled = r.bool("attrib enabled");
  const normalize = r.bool("attrib normalize");
  const attribType = r.u8("attrib source");
  const buffer = r.be32("attrib buffer");
  const clientData = attribType === 0 ? r.raw(r.be32("attrib client data length"), "attrib client data") : null;
  const offset = r.be32("attrib offset");
  const isInt = r.bool("attrib integer");
  const divisor = r.be32("attrib divisor");
  const bindingIndex = r.be32("attrib binding");
  const relativeOffset = r.be32("attrib relative offset");
  const n = r.count("attrib value", 4);
  const value: number[] = [];
  for (let i = 0; i < n; i++) value.push(r.f32("attrib value"));
  return { size, type, stride, enabled, normalize, attribType, buffer, clientData, offset, isInt, divisor, bindingIndex, relativeOffset, value };
}

// VAOState::onSave (GLC).
function vaoState(r: Reader): VaoState {
  const elementArrayBuffer = r.be32("VAO element buffer");
  const attribs: VertexAttrib[] = [];
  for (let i = 0; i < 16; i++) attribs.push(vertexAttrib(r));
  const legacy = r.be64("VAO arrays map") !== 0n;
  const legacyArrays = legacy ? pairs(r, "VAO legacy arrays", 4, (r) => r.be32("legacy array type"), vertexAttrib) : null;
  const bindings = list(r, "VAO bindings", 21, bufferBinding);
  return { elementArrayBuffer, attribs, legacyArrays, bindings, bufferBacked: r.bool("VAO buffer backed"), everBound: r.bool("VAO ever bound") };
}

function rect(r: Reader, f: string): Rect | null {
  const set = r.bool(`${f} set`);
  const x = { x: r.i32(`${f} x`), y: r.i32(`${f} y`), width: r.i32(`${f} width`), height: r.i32(`${f} height`) };
  return set ? x : null;
}

// GLEScontext::onSave (GLC) from activeTexture to the default framebuffer's read buffer.
function contextState(r: Reader): ContextState {
  const activeTexture = r.be32("active texture");
  const vaos = pairs(r, "VAOs", 4, (r) => r.be32("VAO name"), vaoState);
  const boundVao = r.be32("bound VAO");
  const copyRead = r.be32("copy read buffer");
  const copyWrite = r.be32("copy write buffer");
  const pixelPack = r.be32("pixel pack buffer");
  const pixelUnpack = r.be32("pixel unpack buffer");
  const transformFeedback = r.be32("transform feedback buffer");
  const uniform = r.be32("uniform buffer");
  const atomicCounter = r.be32("atomic counter buffer");
  const dispatchIndirect = r.be32("dispatch indirect buffer");
  const drawIndirect = r.be32("draw indirect buffer");
  const shaderStorage = r.be32("shader storage buffer");
  const textureBuffer = r.be32("texture buffer");
  const indexed = {
    transformFeedback: list(r, "indexed transform feedback buffers", 21, bufferBinding),
    uniform: list(r, "indexed uniform buffers", 21, bufferBinding),
    atomicCounter: list(r, "indexed atomic counter buffers", 21, bufferBinding),
    shaderStorage: list(r, "indexed shader storage buffers", 21, bufferBinding),
  };
  const viewport = rect(r, "viewport");
  const polygonOffset = { factor: r.f32("polygon offset factor"), units: r.f32("polygon offset units") };
  const scissor = rect(r, "scissor");
  const enables = pairs(r, "enables", 5, (r) => r.be32("capability"), (r) => r.bool("enabled"));
  const blend: BlendState[] = [];
  const nBlend = r.count("blend states", 32);
  for (let i = 0; i < nBlend; i++) {
    const b = r.raw(32, "blend state");
    const v = new DataView(b.buffer, b.byteOffset, 32);
    blend.push({
      enabled: b[0] !== 0,
      equationRgb: v.getUint32(4, true),
      equationAlpha: v.getUint32(8, true),
      srcRgb: v.getUint32(12, true),
      dstRgb: v.getUint32(16, true),
      srcAlpha: v.getUint32(20, true),
      dstAlpha: v.getUint32(24, true),
      colorMask: [b[28] !== 0, b[29] !== 0, b[30] !== 0, b[31] !== 0],
    });
  }
  const pixelStore = pairs(r, "pixel store", 8, (r) => r.be32("pixel store name"), (r) => r.i32("pixel store value"));
  const cullFace = r.be32("cull face");
  const frontFace = r.be32("front face");
  const depthFunc = r.be32("depth func");
  const depthMask = r.bool("depth mask");
  const depthRange: [number, number] = [r.f32("depth near"), r.f32("depth far")];
  const lineWidth = r.f32("line width");
  const sampleCoverage = { value: r.f32("sample coverage"), invert: r.bool("sample coverage invert") };
  const st = r.raw(56, "stencil states");
  const sv = new DataView(st.buffer, st.byteOffset, 56);
  const face = (o: number): StencilFace => ({
    func: sv.getUint32(o, true),
    ref: sv.getInt32(o + 4, true),
    funcMask: sv.getUint32(o + 8, true),
    writeMask: sv.getUint32(o + 12, true),
    sfail: sv.getUint32(o + 16, true),
    dpfail: sv.getUint32(o + 20, true),
    dppass: sv.getUint32(o + 24, true),
  });
  const stencil = { front: face(0), back: face(28) };
  const clearColor: [number, number, number, number] = [r.f32("clear red"), r.f32("clear green"), r.f32("clear blue"), r.f32("clear alpha")];
  const clearDepth = r.f32("clear depth");
  const clearStencil = r.i32("clear stencil");
  const glError = r.be32("GL error");
  const unitsAt = r.pos;
  const maxTexUnits = r.be32("max texture units");
  const maxUsedTexUnit = r.be32("max used texture unit");
  if (maxTexUnits * 48 > r.left) r.fail(`texture units: ${maxTexUnits} units do not fit in the ${r.left} bytes left`, unitsAt);
  const tu = r.raw(maxTexUnits * 48, "texture units");
  const tv = new DataView(tu.buffer, tu.byteOffset, tu.byteLength);
  const textureUnits: TextureUnit[] = [];
  for (let u = 0; u < maxTexUnits; u++) {
    const textures: number[] = [];
    const enabled: boolean[] = [];
    for (let t = 0; t < 6; t++) {
      textures.push(tv.getUint32(u * 48 + t * 8, true));
      enabled.push(tu[u * 48 + t * 8 + 4] !== 0);
    }
    textureUnits.push({ textures, enabled });
  }
  const array = r.be32("array buffer");
  const elementArray = r.be32("element buffer");
  const renderbuffer = r.be32("renderbuffer");
  const drawFramebuffer = r.be32("draw framebuffer");
  const readFramebuffer = r.be32("read framebuffer");
  const defaultDrawBuffer = r.be32("default draw buffer");
  const defaultReadBuffer = r.be32("default read buffer");
  return {
    activeTexture,
    vaos,
    boundVao,
    buffers: { array, elementArray, copyRead, copyWrite, pixelPack, pixelUnpack, transformFeedback, uniform, atomicCounter, dispatchIndirect, drawIndirect, shaderStorage, textureBuffer },
    indexed,
    viewport,
    scissor,
    polygonOffset,
    enables,
    blend,
    pixelStore,
    cullFace,
    frontFace,
    depthFunc,
    depthMask,
    depthRange,
    lineWidth,
    sampleCoverage,
    stencil,
    clearColor,
    clearDepth,
    clearStencil,
    glError,
    maxTexUnits,
    maxUsedTexUnit,
    textureUnits,
    renderbuffer,
    drawFramebuffer,
    readFramebuffer,
    defaultDrawBuffer,
    defaultReadBuffer,
  };
}

// GLESv2Context::onSave's tail (gles_v2/gles_v2_context.cpp).
function v2Tail(r: Reader): V2State {
  const attribute0: [number, number, number, number] = [r.f32("attribute 0"), r.f32("attribute 0"), r.f32("attribute 0"), r.f32("attribute 0")];
  const attribute0Changed = r.bool("attribute 0 changed");
  const at = r.pos;
  const attribute0ArrayLength = r.be32("attribute 0 array length");
  if (attribute0ArrayLength * 16 > r.left) r.fail("attribute 0 array does not fit", at);
  r.raw(attribute0ArrayLength * 16, "attribute 0 array");
  const attribute0NeedsDisable = r.bool("attribute 0 needs disable");
  const program = r.be32("current program");
  const samplers = pairs(r, "bound samplers", 8, (r) => r.be32("sampler unit"), (r) => r.be32("sampler"));
  const transformFeedbacks = nameSpace(r, "transform feedbacks", 7, (r) => list(r, "transform feedback bindings", 21, bufferBinding));
  return { attribute0, attribute0Changed, attribute0ArrayLength, attribute0NeedsDisable, program, samplers, transformFeedbacks };
}

/** BE32 n, RAW(n * size): saveBuffer of a vector or of a fixed array. */
function skipVec(r: Reader, f: string, size: number): void {
  const n = r.count(f, size);
  r.raw(n * size, f);
}

// GLEScmContext::onSave's tail (gles_cm/gles_cm_context.cpp), parsed to be dropped. Struct
// sizes from gles_cm/gles_cm_context.h: mat4 64, GLValTyped 20, GLVal 16, Material 68,
// LightModel 20, Light 96, Fog 32.
function skipGles1Tail(r: Reader, initialized: boolean): void {
  skipVec(r, "GLES1 projection stack", 64);
  skipVec(r, "GLES1 modelview stack", 64);
  const stacks = r.count("GLES1 texture stacks", 4);
  for (let i = 0; i < stacks; i++) skipVec(r, "GLES1 texture stack", 64);
  for (const f of ["GLES1 texture envs", "GLES1 texgens"]) {
    const n = r.count(f, 4);
    for (let i = 0; i < n; i++) list(r, f, 24, (r) => r.raw(24, f));
  }
  r.be32("GLES1 client active texture");
  if (initialized) {
    r.be32("GLES1 shade model");
    r.raw(40, "GLES1 color and normal");
    const n = r.count("GLES1 texture coordinate arrays", 23);
    for (let i = 0; i < n; i++) vertexAttrib(r);
  }
  skipVec(r, "GLES1 multitexture coordinates", 16);
  skipVec(r, "GLES1 material", 68);
  skipVec(r, "GLES1 light model", 20);
  skipVec(r, "GLES1 lights", 96);
  skipVec(r, "GLES1 fog", 32);
}

/** NameSpace::onSave (common/object_name_space.cpp): BE32 n, then n x (BE64 local name, record). */
function nameSpace<T>(r: Reader, f: string, dataType: number, record: (r: Reader) => T): Map<number, T> {
  return pairs(r, f, 12, (r) => r.num64(`${f} name`), (r) => {
    objectType(r, f, dataType);
    return record(r);
  });
}

/** ObjectData::onSave's leading BE32 ObjectDataType (common/include/common/object_data.h). */
function objectType(r: Reader, f: string, want: number): void {
  const at = r.pos;
  const t = r.be32(`${f} object type`);
  if (t !== want) r.fail(`${f}: object type ${t}, expected ${want}`, at);
}

const DATA = { shader: 0, program: 1, texture: 2, buffer: 3, renderbuffer: 4, framebuffer: 5, sampler: 6, transformFeedback: 7 } as const;

// ShareGroup::onSave (common/share_group.cpp): one namespace per NamedObjectType, in order.
// NULLTYPE, QUERY, VERTEX_ARRAY_OBJECT and TRANSFORM_FEEDBACK hold no data in a share group,
// and the translator's own loader cannot load a record there, so one is corruption.
function shareGroupBody(r: Reader, id: number): ShareGroup {
  const none = (f: string) => {
    const at = r.pos;
    const n = r.be32(`${f} count`);
    if (n !== 0) r.fail(`${f}: ${n} records in a namespace that holds none`, at);
  };
  none("share group NULLTYPE namespace");
  const buffers = nameSpace(r, "buffers", DATA.buffer, bufferObject);
  const textures = nameSpace(r, "textures", DATA.texture, textureObject);
  const renderbuffers = nameSpace(r, "renderbuffers", DATA.renderbuffer, renderbufferObject);
  const framebuffers = nameSpace(r, "share group framebuffers", DATA.framebuffer, framebuffer);
  const shaders = new Map<number, ShaderObject>();
  const programs = new Map<number, ProgramObject>();
  const n = r.count("shaders and programs", 13);
  for (let i = 0; i < n; i++) {
    const at = r.pos;
    const name = r.num64("shader or program name");
    if (shaders.has(name) || programs.has(name)) r.fail(`shaders and programs: key ${name} repeated`, at);
    const kindAt = r.pos;
    const kind = r.u8("shader or program kind");
    if (kind === 0) {
      objectType(r, "shader", DATA.shader);
      shaders.set(name, shaderObject(r));
    } else if (kind === 1) {
      objectType(r, "program", DATA.program);
      programs.set(name, programObject(r));
    } else r.fail(`shader or program kind ${kind}`, kindAt);
  }
  const samplers = nameSpace(r, "samplers", DATA.sampler, samplerObject);
  none("share group query namespace");
  none("share group VAO namespace");
  none("share group transform feedback namespace");
  return { id, buffers, textures, renderbuffers, framebuffers, shaders, programs, samplers };
}

// GLESbuffer::onSave (common/gles_buffer.cpp).
function bufferObject(r: Reader): BufferObject {
  const size = r.be32("buffer size");
  const usage = r.be32("buffer usage");
  const data = r.raw(size, "buffer data");
  return { size, usage, data, wasBound: r.bool("buffer was bound") };
}

// TextureData::onSave (common/texture_data.cpp).
function textureObject(r: Reader): TextureObject {
  const target = r.be32("texture target");
  const width = r.be32("texture width");
  const height = r.be32("texture height");
  const depth = r.be32("texture depth");
  const border = r.be32("texture border");
  const internalFormat = r.be32("texture internal format");
  const format = r.be32("texture format");
  const type = r.be32("texture type");
  const sourceEGLImage = r.be32("texture EGL image");
  const hasStorage = r.bool("texture has storage");
  const wasBound = r.bool("texture was bound");
  const requiresAutoMipmap = r.bool("texture auto mipmap");
  const compressed = r.bool("texture compressed");
  const compressedFormat = r.be32("texture compressed format");
  const crop = r.raw(16, "texture crop rect");
  const cv = new DataView(crop.buffer, crop.byteOffset, 16);
  const cropRect: [number, number, number, number] = [cv.getInt32(0, true), cv.getInt32(4, true), cv.getInt32(8, true), cv.getInt32(12, true)];
  const texStorageLevels = r.be32("texture storage levels");
  r.be32("texture deprecated level");
  const globalName = r.be32("texture global name");
  const params = pairs(r, "texture params", 8, (r) => r.be32("texture param"), (r) => r.i32("texture param value"));
  return {
    target, width, height, depth, border, internalFormat, format, type, sourceEGLImage, hasStorage, wasBound,
    requiresAutoMipmap, compressed, compressedFormat, cropRect, texStorageLevels, globalName, params,
  };
}

// RenderbufferData::onSave (common/framebuffer_data.cpp).
function renderbufferObject(r: Reader): RenderbufferObject {
  return {
    attachedFramebuffer: r.be32("renderbuffer framebuffer"),
    attachedPoint: r.be32("renderbuffer attach point"),
    width: r.be32("renderbuffer width"),
    height: r.be32("renderbuffer height"),
    internalFormat: r.be32("renderbuffer internal format"),
    hostInternalFormat: r.be32("renderbuffer host format"),
    everBound: r.be32("renderbuffer ever bound") !== 0,
  };
}

// FramebufferData::onSave (common/framebuffer_data.cpp): MAX_ATTACH_POINTS (19) is written,
// then all 20 entries of m_attachPoints[MAX_ATTACH_POINTS + 1].
function framebuffer(r: Reader): FramebufferObject {
  const name = r.be32("framebuffer name");
  const at = r.pos;
  const points = r.be32("framebuffer attach point count");
  if (points !== 19) r.fail(`framebuffer attach point count ${points}, expected 19`, at);
  const attachments: FramebufferAttachment[] = [];
  for (let i = 0; i < 20; i++) {
    attachments.push({
      target: r.be32("attachment target"),
      name: r.be32("attachment name"),
      objectType: r.be32("attachment object type"),
      owned: r.bool("attachment owned"),
    });
  }
  const dirty = r.bool("framebuffer dirty");
  const hasBeenBound = r.bool("framebuffer bound");
  const hasDrawBuffers = r.bool("framebuffer has draw buffers");
  const n = r.count("draw buffers", 4);
  const db = r.raw(n * 4, "draw buffers");
  const dv = new DataView(db.buffer, db.byteOffset, db.byteLength);
  const drawBuffers: number[] = [];
  for (let i = 0; i < n; i++) drawBuffers.push(dv.getUint32(i * 4, true));
  return { name, attachments, dirty, hasBeenBound, hasDrawBuffers, drawBuffers, readBuffer: r.be32("framebuffer read buffer") };
}

// ShaderParser::onSave (gles_v2/shader_parser.cpp) after its kind byte and object type.
function shaderObject(r: Reader): ShaderObject {
  const source = r.str("shader source");
  r.raw(r.be32("shader translated source length"), "shader translated source");
  r.raw(r.be32("shader parsed source length"), "shader parsed source");
  r.raw(r.be32("shader compiled source length"), "shader compiled source");
  const infoLog = r.str("shader info log");
  const programs = list(r, "shader programs", 4, (r) => r.be32("shader program"));
  const type = r.be32("shader type");
  const compileStatus = r.bool("shader compile status");
  const deleteStatus = r.bool("shader delete status");
  r.u8("shader valid");
  r.u8("shader core profile");
  return { type, source, infoLog, programs, compileStatus, deleteStatus };
}

function attachedShader(r: Reader): AttachedShader {
  return { shader: r.be32("attached shader"), linkedSource: r.str("linked source") };
}

// ProgramData::onSave (gles_v2/program_data.cpp) after its kind byte and object type.
function programObject(r: Reader): ProgramObject {
  const attribLoc = (f: string) => pairs(r, f, 8, (r) => r.str(`${f} name`), (r) => r.be32(`${f} location`));
  const boundAttribLocations = attribLoc("bound attribute locations");
  const linkedAttribLocations = attribLoc("linked attribute locations");
  const uniforms = list(r, "uniforms", 22, (r): UniformValue => {
    const hostLocation = r.be32("uniform location");
    const count = r.be32("uniform count");
    const transpose = r.bool("uniform transpose");
    const type = r.be32("uniform type");
    const value = r.raw(r.be32("uniform value length"), "uniform value");
    return { hostLocation, count, transpose, type, value, guestName: r.str("uniform name") };
  });
  const uniformBlockBindings = pairs(r, "uniform blocks", 8, (r) => r.be32("uniform block"), (r) => r.be32("uniform block binding"));
  const transformFeedbackVaryings = list(r, "transform feedback varyings", 4, (r) => r.str("varying"));
  const transformFeedbackBufferMode = r.be32("transform feedback buffer mode");
  const vertex = attachedShader(r);
  const fragment = attachedShader(r);
  const compute = attachedShader(r);
  r.str("validation info log");
  const infoLog = r.str("program info log");
  r.be16("program padding");
  const validateStatus = r.bool("validate status");
  const linkStatus = r.bool("link status");
  const inUse = r.bool("program in use");
  const deleteStatus = r.bool("program delete status");
  const glesMajor = r.u8("program GLES major");
  const glesMinor = r.u8("program GLES minor");
  const uniformLocations = pairs(r, "guest uniform locations", 8, (r) => r.str("guest uniform name"), (r) => r.i32("guest uniform location"));
  return {
    boundAttribLocations, linkedAttribLocations, uniforms, uniformBlockBindings, transformFeedbackVaryings,
    transformFeedbackBufferMode, vertex, fragment, compute, infoLog, validateStatus, linkStatus, inUse,
    deleteStatus, glesMajor, glesMinor, uniformLocations,
  };
}

// SamplerData::onSave (gles_v2/sampler_data.cpp).
function samplerObject(r: Reader): SamplerObject {
  return {
    params: pairs(r, "sampler params", 8, (r) => r.be32("sampler param"), (r) => r.be32("sampler param value")),
    floatParams: pairs(r, "sampler float params", 8, (r) => r.be32("sampler param"), (r) => r.f32("sampler param value")),
  };
}

// ---- TEXTURE ----

const BLOB_TARGETS: ReadonlySet<number> = new Set([0x0de1, 0x8513, 0x806f, 0x8c1a]);

/**
 * A TEXTURE part's bytes after its id: SaveableTexture::onSave
 * (host/gl/glestranslator/common/saveable_texture.cpp). Pixel bytes are views into `bytes`.
 */
export function parseTexture(bytes: Uint8Array): TextureBlob {
  const r = new Reader(bytes, "texture");
  const blob: TextureBlob = {
    empty: bytes.length === 0,
    target: 0, width: 0, height: 0, depth: 0, format: 0, internalFormat: 0, type: 0, border: 0,
    texStorageLevels: 0, maxMipmapLevel: 0, faces: [], params: new Map(),
  };
  // GlobalNameSpace::onSave's saver writes nothing for a name with no saveable texture
  // (object_name_space.cpp). The format notes do not mention this empty blob.
  if (blob.empty) return blob;
  blob.target = r.be32("target");
  blob.width = r.be32("width");
  blob.height = r.be32("height");
  blob.depth = r.be32("depth");
  blob.format = r.be32("format");
  blob.internalFormat = r.be32("internal format");
  blob.type = r.be32("type");
  blob.border = r.be32("border");
  blob.texStorageLevels = r.be32("storage levels");
  blob.maxMipmapLevel = r.be32("max mipmap level");
  // Any other target writes the header alone.
  if (BLOB_TARGETS.has(blob.target)) {
    const levels = blob.texStorageLevels || blob.maxMipmapLevel + 1;
    const faceCount = blob.target === 0x8513 ? 6 : 1;
    const layered = blob.target === 0x806f || blob.target === 0x8c1a;
    const levelBytes = layered ? 16 : 12;
    if (faceCount * levels * levelBytes > r.left) r.fail(`${faceCount} x ${levels} levels do not fit in the ${r.left} bytes left`, 36);
    for (let f = 0; f < faceCount; f++) {
      const face: TextureLevel[] = [];
      for (let l = 0; l < levels; l++) {
        const width = r.be32("level width");
        const height = r.be32("level height");
        const depth = layered ? r.be32("level depth") : 1;
        face.push({ width, height, depth, pixels: r.raw(r.be32("level size"), "level pixels") });
      }
      blob.faces.push(face);
    }
    blob.params = pairs(r, "params", 8, (r) => r.be32("param"), (r) => r.i32("param value"));
  }
  r.done();
  return blob;
}

// ---- CHANNEL ----

/**
 * A CHANNEL part's bytes after its id: RenderThread::save (host/render_thread.cpp), U8 1 and the
 * thread's snapshot stream as BE32 length and bytes, or U8 0 when the thread's snapshot had not
 * finished. The stream holds the read buffer, the channel stream and the checksum state, which
 * are skipped, then RenderThreadInfo::onSave with its GL record.
 */
export function parseChannel(bytes: Uint8Array): ChannelState {
  const outer = new Reader(bytes, "channel");
  const state: ChannelState = {
    saved: outer.bool("saved"),
    context: 0, drawSurface: 0, readSurface: 0, contexts: [], windowSurfaces: [], puid: 0n,
    pendingReadBytes: 0, unreadStreamBytes: 0, pendingWriteBytes: 0, checksumVersion: 0, vkContextId: null,
  };
  // The format notes give only the U8 1 form; save() writes U8 0 alone when the thread's
  // snapshot did not finish.
  if (!state.saved) {
    outer.done();
    return state;
  }
  const inner = outer.raw(outer.be32("thread stream length"), "thread stream");
  outer.done();
  const r = new Reader(inner, "channel thread stream");
  // ReadBuffer::onSave (host/read_buffer.cpp).
  r.be32("read buffer size");
  state.pendingReadBytes = r.be32("read buffer data length");
  r.raw(state.pendingReadBytes, "read buffer data");
  // IOStream::save (host/iostream/include/gfxstream/host/iostream.h), then ChannelStream::onSave
  // (host/channel_stream.cpp). The join refuses address space graphics, so no RingStream.
  r.be32("stream buffer size");
  r.be32("stream free");
  r.u8("stream has buffer");
  state.unreadStreamBytes = r.be32("stream read length");
  r.raw(state.unreadStreamBytes, "stream read data");
  state.pendingWriteBytes = r.be32("stream write length");
  r.raw(state.pendingWriteBytes, "stream write data");
  // ChecksumCalculator::save (host/decoder_common/ChecksumCalculator.cpp).
  r.u8("checksum size");
  state.checksumVersion = r.be32("checksum version");
  r.be32("checksum reads");
  r.be32("checksum writes");
  // RenderThreadInfo::onSave (host/render_thread_info.cpp): no puid with Vulkan snapshots off.
  if (r.be32("has GL info") !== 0) {
    // RenderThreadInfoGl::onSave (host/gl/render_thread_info_gl.cpp).
    state.context = r.be32("current context");
    state.drawSurface = r.be32("draw surface");
    state.readSurface = r.be32("read surface");
    state.contexts = list(r, "thread contexts", 4, (r) => r.be32("thread context"));
    state.windowSurfaces = list(r, "thread window surfaces", 4, (r) => r.be32("thread window surface"));
    state.puid = r.be64("thread puid");
    r.be64("sync thread alias");
  }
  // RenderThreadInfoVk::onSave (host/vulkan/render_thread_info_vk.cpp): one BE32.
  if (r.be32("has Vulkan info") !== 0) state.vkContextId = r.be32("Vulkan context id");
  r.done();
  return state;
}
