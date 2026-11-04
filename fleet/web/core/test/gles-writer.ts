// A writer for gfxstream's GLES snapshot streams that follows its C++ save code, one function per
// onSave, and the scene the parser and the mirror's join loader are tested against
// (gles-snapshot.test.ts, mirror-gl.test.ts).

import { DEFAULT_TEXTURE_NAME } from "../mirror/gles-snapshot.ts";

// Stream: putByte, putBe16/32/64 big-endian; putFloat and write() in host order (little-endian).
class W {
  private readonly out: number[] = [];
  byte(v: number | boolean): this {
    this.out.push(Number(v) & 0xff);
    return this;
  }
  be16(v: number): this {
    this.out.push((v >>> 8) & 0xff, v & 0xff);
    return this;
  }
  be32(v: number): this {
    const x = v >>> 0;
    this.out.push(x >>> 24, (x >>> 16) & 0xff, (x >>> 8) & 0xff, x & 0xff);
    return this;
  }
  be64(v: number | bigint): this {
    const x = BigInt(v);
    return this.be32(Number(x >> 32n)).be32(Number(x & 0xffffffffn));
  }
  float(v: number): this {
    return this.write(new Uint8Array(new Float32Array([v]).buffer));
  }
  write(b: Uint8Array): this {
    for (const x of b) this.out.push(x);
    return this;
  }
  le32(...xs: number[]): this {
    return this.write(new Uint8Array(new Uint32Array(xs.map((x) => x >>> 0)).buffer));
  }
  string(s: string): this {
    const b = utf8(s);
    return this.be32(b.length).write(b);
  }
  bytes(): Uint8Array {
    return new Uint8Array(this.out);
  }
}

function utf8(s: string): Uint8Array {
  const out: number[] = [];
  for (const ch of s) {
    const c = ch.codePointAt(0)!;
    if (c < 0x80) out.push(c);
    else if (c < 0x800) out.push(0xc0 | (c >> 6), 0x80 | (c & 63));
    else if (c < 0x10000) out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
    else out.push(0xf0 | (c >> 18), 0x80 | ((c >> 12) & 63), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
  }
  return new Uint8Array(out);
}

export const GL = {
  TEXTURE_2D: 0x0de1, TEXTURE_CUBE_MAP: 0x8513, TEXTURE_2D_ARRAY: 0x8c1a, TEXTURE_2D_MULTISAMPLE: 0x9100,
  RGBA: 0x1908, RGBA8: 0x8058, UNSIGNED_BYTE: 0x1401, FLOAT: 0x1406, FLOAT_VEC4: 0x8b52,
  MIN_FILTER: 0x2801, MAG_FILTER: 0x2800, WRAP_S: 0x2802, LINEAR: 0x2601, NEAREST: 0x2600, REPEAT: 0x2901, MIN_LOD: 0x813a,
  STATIC_DRAW: 0x88e4, RENDERBUFFER: 0x8d41, DEPTH_COMPONENT16: 0x81a5, DEPTH_ATTACHMENT: 0x8d00, COLOR_ATTACHMENT0: 0x8ce0,
  VERTEX_SHADER: 0x8b31, FRAGMENT_SHADER: 0x8b30, SEPARATE_ATTRIBS: 0x8c8d,
  BLEND: 0x0be2, DEPTH_TEST: 0x0b71, UNPACK_ALIGNMENT: 0x0cf5, BACK: 0x0405, CCW: 0x0901, LESS: 0x0201, EQUAL: 0x0202,
  FUNC_ADD: 0x8006, SRC_ALPHA: 0x0302, ONE_MINUS_SRC_ALPHA: 0x0303, ONE: 1, KEEP: 0x1e00, REPLACE: 0x1e01,
};

// ---- C++ save code, transcribed ----

interface Binding { buffer?: number; offset?: number; size?: number; stride?: number; divisor?: number; base?: boolean }
// BufferBinding::onSave (gles_context.cpp)
const bufferBinding = (w: W, b: Binding = {}) =>
  w.be32(b.buffer ?? 0).be32(b.offset ?? 0).be32(b.size ?? 0).be32(b.stride ?? 0).be32(b.divisor ?? 0).byte(b.base ?? false);
// saveContainer (gl_snapshot_serializers.h)
const bindings = (w: W, bs: Binding[]) => {
  w.be32(bs.length);
  for (const b of bs) bufferBinding(w, b);
};

interface Pointer {
  size?: number; type?: number; stride?: number; enabled?: boolean; normalize?: boolean; attribType?: number; buffer?: number;
  data?: Uint8Array; offset?: number; isInt?: boolean; divisor?: number; bindingIndex?: number; relOffset?: number; values?: number[];
}
// GLESpointer::onSave (gles_pointer.cpp)
function glesPointer(w: W, p: Pointer = {}): void {
  w.be32(p.size ?? 4).be32(p.type ?? GL.FLOAT).be32(p.stride ?? 0).byte(p.enabled ?? false).byte(p.normalize ?? false);
  const attribType = p.attribType ?? 0;
  w.byte(attribType).be32(p.buffer ?? 0);
  if (attribType === 0) {
    if (p.data && p.data.length > 0) w.be32(p.data.length).write(p.data);
    else w.be32(0);
  }
  w.be32(p.offset ?? 0).byte(p.isInt ?? false).be32(p.divisor ?? 0).be32(p.bindingIndex ?? 0).be32(p.relOffset ?? 0);
  const values = p.values ?? [];
  w.be32(values.length);
  for (const v of values) w.float(v);
}

interface Vao { element?: number; attribs?: Map<number, Pointer>; legacy?: Map<number, Pointer>; bindings?: Binding[] }
// VAOState::onSave (gles_context.cpp)
function vaoState(w: W, v: Vao): void {
  w.be32(v.element ?? 0);
  for (let i = 0; i < 16; i++) glesPointer(w, v.attribs?.get(i));
  if (v.legacy) {
    w.be64(0x7f00_1234_5678n);
    w.be32(v.legacy.size);
    for (const [k, p] of v.legacy) glesPointer(w.be32(k), p);
  } else w.be64(0);
  bindings(w, v.bindings ?? Array.from({ length: 16 }, () => ({})));
  w.byte(true).byte(true);
}

// NameSpace::onSave (object_name_space.cpp): records start with their ObjectDataType
const nameSpace = (w: W, entries: [number | bigint, (w: W) => void][]) => {
  w.be32(entries.length);
  for (const [name, rec] of entries) rec(w.be64(name));
};

interface Fbo { name: number; attach: Map<number, [number, number, number, boolean]>; drawBuffers: number[]; readBuffer: number }
// FramebufferData::onSave (framebuffer_data.cpp)
function framebufferData(w: W, f: Fbo): void {
  w.be32(5).be32(f.name).be32(19);
  for (let i = 0; i < 20; i++) {
    const [target, name, type, owned] = f.attach.get(i) ?? [0, 0, 0, false];
    w.be32(target).be32(name).be32(type).byte(owned);
  }
  w.byte(false).byte(true).byte(true).be32(f.drawBuffers.length).le32(...f.drawBuffers).be32(f.readBuffer);
}

// GLEScontext::onSave (gles_context.cpp), the part written only once initialized
function glesContextInitialized(w: W, o: { vaos: [number, Vao][]; boundVao: number; texUnits: number[][]; gles1?: boolean; generic?: number[] }): void {
  w.be32(1); // m_activeTexture
  w.be32(o.vaos.length);
  for (const [name, v] of o.vaos) vaoState(w.be32(name), v);
  w.be32(o.boundVao);
  for (let i = 0; i < 11; i++) w.be32(o.generic?.[i] ?? 20 + i); // copyRead .. textureBuffer
  bindings(w, [{}, { buffer: 1, offset: 16, size: 64, base: true }, {}, {}]);
  bindings(w, [{ buffer: 1, size: 256 }, {}]);
  bindings(w, [{}]);
  bindings(w, [{}]);
  w.byte(true).be32(0).be32(0).be32(720).be32(1280);
  w.float(1.5).float(2.5);
  w.byte(false).be32(-1).be32(0).be32(0).be32(0);
  w.be32(2).be32(GL.BLEND).byte(true).be32(GL.DEPTH_TEST).byte(false);
  w.be32(1);
  w.write(new Uint8Array([1, 0, 0, 0])).le32(GL.FUNC_ADD, GL.FUNC_ADD, GL.SRC_ALPHA, GL.ONE_MINUS_SRC_ALPHA, GL.ONE, GL.ONE_MINUS_SRC_ALPHA);
  w.write(new Uint8Array([1, 0, 1, 1]));
  w.be32(1).be32(GL.UNPACK_ALIGNMENT).be32(1);
  w.be32(GL.BACK).be32(GL.CCW).be32(GL.LESS).byte(true);
  w.float(0).float(1).float(2).float(0.75).byte(true);
  w.le32(GL.EQUAL, -3, 0xff, 0x0f, GL.KEEP, GL.KEEP, GL.REPLACE); // m_stencilStates[0]
  w.le32(0x0207, 0, 0xffffffff, 0xffffffff, GL.KEEP, GL.KEEP, GL.KEEP); // [1]: ALWAYS
  w.float(0.25).float(0.5).float(0.75).float(1).float(1);
  w.be32(7).be32(0).be32(o.texUnits.length).be32(1);
  for (const unit of o.texUnits) for (let t = 0; t < 6; t++) w.le32(unit[t] ?? 0).write(new Uint8Array([o.gles1 && t === 0 ? 1 : 0, 0, 0, 0]));
  w.be32(1).be32(1).be32(3).be32(4).be32(4).be32(GL.BACK).be32(GL.BACK);
}

// GLESv2Context::onSave's tail (gles_v2_context.cpp)
function v2Tail(w: W, o: { program: number; samplers: [number, number][]; tf: [number, Binding[]][] }): void {
  w.float(0).float(0).float(0).float(1).byte(true);
  w.be32(2).write(new Uint8Array(32)).byte(false);
  w.be32(o.program).be32(o.samplers.length);
  for (const [unit, s] of o.samplers) w.be32(unit).be32(s);
  nameSpace(w, o.tf.map(([name, bs]) => [name, (w: W) => bindings(w.be32(7), bs)]));
}

// GLEScmContext::onSave's tail (gles_cm_context.cpp)
function gles1Tail(w: W): void {
  w.be32(2).write(new Uint8Array(128)); // projection stack
  w.be32(1).write(new Uint8Array(64)); // modelview stack
  w.be32(2).be32(1).write(new Uint8Array(64)).be32(0); // texture matrix stacks
  w.be32(4).be32(1).be32(0x2200).write(new Uint8Array(20)).be32(0).be32(0).be32(0); // tex envs
  w.be32(0); // texgens
  w.be32(0x84c0); // client active texture
  w.be32(0x1d01).write(new Uint8Array(40)).be32(4); // initialized: shade model, color, normal
  for (let i = 0; i < 4; i++) glesPointer(w, { data: i === 1 ? new Uint8Array([9, 9]) : undefined });
  w.be32(4).write(new Uint8Array(64)).be32(1).write(new Uint8Array(68)).be32(1).write(new Uint8Array(20));
  w.be32(8).write(new Uint8Array(768)).be32(1).write(new Uint8Array(32));
}

// TextureData::onSave (texture_data.cpp)
function textureData(w: W, t: { target: number; w: number; h: number; image?: number; global: number; params: [number, number][] }): void {
  w.be32(2).be32(t.target).be32(t.w).be32(t.h).be32(1).be32(0).be32(GL.RGBA8).be32(GL.RGBA).be32(GL.UNSIGNED_BYTE);
  w.be32(t.image ?? 0).byte(true).byte(true).byte(false).byte(false).be32(0);
  w.le32(1, -2, 3, 4).be32(0).be32(0).be32(t.global).be32(t.params.length);
  for (const [p, v] of t.params) w.be32(p).be32(v);
}

// ShaderParser::onSave (shader_parser.cpp)
function shaderParser(w: W, type: number, src: string, programs: number[]): void {
  w.byte(0).be32(0).string(src).string("translated").string("parsed").string("compiled").string("ok");
  w.be32(programs.length);
  for (const p of programs) w.be32(p);
  w.be32(type).byte(true).byte(false).byte(true).byte(true);
}

// ProgramData::onSave (program_data.cpp)
function programData(w: W, vs: string, fs: string, blocks: [number, number][] = [[0, 2]]): void {
  w.byte(1).be32(1);
  w.be32(1).string("a_pos").be32(0);
  w.be32(2).string("a_pos").be32(0).string("a_uv").be32(1);
  w.be32(1).be32(3).be32(1).byte(false).be32(GL.FLOAT_VEC4).be32(16).write(new Uint8Array(new Float32Array([1, 0.5, 0.25, 1]).buffer)).string("u_color");
  w.be32(blocks.length);
  for (const [index, binding] of blocks) w.be32(index).be32(binding);
  w.be32(1).string("v_out").be32(GL.SEPARATE_ATTRIBS);
  w.be32(10).string(vs).be32(11).string(fs).be32(0).string("");
  w.string("").string("linked").be16(0);
  w.byte(true).byte(true).byte(true).byte(false).byte(3).byte(0);
  w.be32(1).string("u_color").be32(1);
}

// ShareGroup::onSave (share_group.cpp): ten namespaces in NamedObjectType order
function shareGroup7(w: W, vs: string, fs: string, blocks?: [number, number][]): void {
  nameSpace(w, []);
  nameSpace(w, [[1, (w) => w.be32(3).be32(8).be32(GL.STATIC_DRAW).write(new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8])).byte(true)]]);
  nameSpace(w, [
    [2, (w) => textureData(w, { target: GL.TEXTURE_2D, w: 64, h: 32, global: 101, params: [[GL.MIN_FILTER, GL.LINEAR]] })],
    [BigInt(DEFAULT_TEXTURE_NAME), (w) => textureData(w, { target: GL.TEXTURE_2D, w: 0, h: 0, global: 100, params: [] })],
    [5, (w) => textureData(w, { target: GL.TEXTURE_2D, w: 640, h: 480, image: 0x500, global: 102, params: [] })],
  ]);
  nameSpace(w, [[3, (w) => w.be32(4).be32(4).be32(GL.DEPTH_ATTACHMENT).be32(64).be32(32).be32(GL.DEPTH_COMPONENT16).be32(GL.DEPTH_COMPONENT16).be32(1)]]);
  nameSpace(w, []);
  nameSpace(w, [
    [10, (w) => shaderParser(w, GL.VERTEX_SHADER, vs, [12])],
    [12, (w) => programData(w, vs, fs, blocks)],
    [11, (w) => shaderParser(w, GL.FRAGMENT_SHADER, fs, [12])],
  ]);
  nameSpace(w, [[6, (w) => w.be32(6).be32(1).be32(GL.MIN_FILTER).be32(GL.NEAREST).be32(1).be32(GL.MIN_LOD).float(0.5)]]);
  nameSpace(w, []);
  nameSpace(w, []);
  nameSpace(w, []);
}

function emptyShareGroup(w: W): void {
  for (let i = 0; i < 10; i++) nameSpace(w, []);
}

const saveProcOwned = (w: W, entries: [number, number[]][]) => {
  w.be32(entries.length);
  for (const [puid, hs] of entries) {
    w.be64(puid).be32(hs.length);
    for (const h of hs) w.be32(h);
  }
};

export const VS = "#version 300 es\n// café ✓ \u{1f600}\nin vec3 a_pos; void main() { gl_Position = vec4(a_pos, 1.0); }";
export const FS = "#version 300 es\nprecision mediump float; uniform vec4 u_color; out vec4 o; void main() { o = u_color; }";

/** FrameBuffer::Impl::onSave (frame_buffer.cpp) for one scene, with overrides for corruption tests. */
export function glState(
  o: {
    version?: number;
    api3?: number;
    magic?: number;
    element?: number;
    /** Context 0x11's generic buffer bindings, copyRead .. textureBuffer (default 20 .. 30). */
    generic?: number[];
    /** Context 0x11's constant value of attribute 1 in vertex array 9 (default [1, 2, 3, 4]). */
    attrib1?: number[];
    /** Program 12's uniform block bindings, host index to binding (default [[0, 2]]). */
    blocks?: [number, number][];
  } = {},
): Uint8Array {
  const w = new W();
  w.be32(o.version ?? 6);
  // eglSaveAllImages: GlobalNameSpace::onSave, then the EGL images
  w.be32(4).be32(100).be32(101).be32(102).be32(103);
  w.be32(2).be32(0x500).be32(102).be32(0x501).be32(103);
  w.be32(720).be32(1280).float(2).be32(0);
  w.be32(1).be32(0).be32(720).be32(1280).be32(320).be32(320);
  w.be32(0);
  w.be32(0).be32(1).be32(60).be32(1000).be64(123456789);

  w.be32(3); // contexts
  // A: GLES 3.0, initialized, first of share group 7
  w.be32(0x11).be32(o.api3 ?? 3);
  w.byte(true).be32(3).be32(0);
  glesContextInitialized(w, {
    vaos: [
      [0, {}],
      [9, {
        element: o.element ?? 1,
        attribs: new Map<number, Pointer>([
          [0, { size: 3, stride: 12, enabled: true, attribType: 1, buffer: 1, offset: 8 }],
          [1, { attribType: 2, values: o.attrib1 ?? [1, 2, 3, 4], bindingIndex: 1 }],
          [2, { size: 2, type: GL.UNSIGNED_BYTE, enabled: true, data: new Uint8Array([7, 8, 9]), bindingIndex: 2 }],
        ]),
        bindings: Array.from({ length: 16 }, (_, i) => (i === 0 ? { buffer: 1, offset: 8, stride: 12 } : i === 3 ? { divisor: 2 } : {})),
      }],
    ],
    boundVao: 9,
    texUnits: [[2], [5]],
    generic: o.generic,
  });
  nameSpace(w, [[4, (w) => framebufferData(w, {
    name: 4,
    attach: new Map([[0, [GL.TEXTURE_2D, 2, 0, false]], [1, [GL.RENDERBUFFER, 3, 3, false]]]),
    drawBuffers: [GL.COLOR_ATTACHMENT0],
    readBuffer: GL.COLOR_ATTACHMENT0,
  })]]);
  v2Tail(w, { program: 12, samplers: [[1, 6]], tf: [[0, [{}, { buffer: 1, size: 64, base: true }, {}, {}]]] });
  w.be32(33).be64(7);
  shareGroup7(w, VS, FS, o.blocks);

  // C: GLES1, initialized, alone in share group 9
  w.be32(0x33).be32(1);
  w.byte(true).be32(1).be32(1);
  glesContextInitialized(w, {
    vaos: [[0, { legacy: new Map<number, Pointer>([[0x8074, { size: 3, enabled: true, data: new Uint8Array([1, 2, 3, 4]) }], [0x8076, { attribType: 1, buffer: 0 }]]) }]],
    boundVao: 0,
    texUnits: [[0], [0], [0], [0]],
    gles1: true,
  });
  nameSpace(w, []);
  gles1Tail(w);
  w.be32(34).be64(9);
  emptyShareGroup(w);

  // B: GLES 3.0, never made current, shares group 7, so no share group body
  w.be32(0x22).be32(3);
  w.byte(false).be32(3).be32(0);
  nameSpace(w, []);
  v2Tail(w, { program: 0, samplers: [], tf: [] });
  w.be32(33).be64(7);

  w.byte(true); // guestManagedColorBufferLifetime
  w.be32(1).be32(0xcafeface).be32(0x40).be32(640).be32(480).be32(1);
  w.be32(0x40).be32(640).be32(480).be32(1).be32(0x500).be32(0x501).be32(0);
  w.be32(2).byte(true).be32(5000);
  w.be32(0x40); // last posted
  w.be32(1).be32(0x60).be32(0x40).be32(0x11).be32(0x11).be32(720).be32(1280).be32(33).be32(0x40);
  saveProcOwned(w, [[5, [0x60]]]);
  saveProcOwned(w, [[5, [0x40]]]);
  saveProcOwned(w, []);
  saveProcOwned(w, [[5, [0x11, 0x22]]]);
  w.be64(1).be64(5).be32(9); // process resources
  w.be32(2).be64(0x7f0000001000n).be64(0x7f0000002000n); // fence syncs
  w.be32(o.magic ?? 0xc0ffeeee);
  return w.bytes();
}

/** The same scene, but share group 7's QUERY namespace claims one record. */
export function glStateWithQueries(): Uint8Array {
  const full = glState();
  const w = new W();
  shareGroup7(w, VS, FS);
  const body = w.bytes();
  const at = indexOf(full, body);
  // QUERY, VERTEX_ARRAY_OBJECT and TRANSFORM_FEEDBACK are the body's last three counts
  const out = full.slice();
  new DataView(out.buffer).setUint32(at + body.length - 12, 1, false);
  return out;
}

function indexOf(hay: Uint8Array, needle: Uint8Array): number {
  outer: for (let i = 0; i + needle.length <= hay.length; i++) {
    for (let j = 0; j < needle.length; j++) if (hay[i + j] !== needle[j]) continue outer;
    return i;
  }
  throw new Error("not found");
}


// SaveableTexture::onSave (saveable_texture.cpp)
export function textureBlob(o: { target: number; w: number; h: number; d?: number; storage: number; maxLevel: number; levels?: [number, number, number][] }): Uint8Array {
  const w = new W();
  w.be32(o.target).be32(o.w).be32(o.h).be32(o.d ?? 1).be32(GL.RGBA).be32(GL.RGBA8).be32(GL.UNSIGNED_BYTE).be32(0).be32(o.storage).be32(o.maxLevel);
  if (![GL.TEXTURE_2D, GL.TEXTURE_CUBE_MAP, 0x806f, GL.TEXTURE_2D_ARRAY].includes(o.target)) return w.bytes();
  const numLevels = o.storage || o.maxLevel + 1;
  const layered = o.target === GL.TEXTURE_2D_ARRAY || o.target === 0x806f;
  const faces = o.target === GL.TEXTURE_CUBE_MAP ? 6 : 1;
  for (let f = 0; f < faces; f++) {
    for (let l = 0; l < numLevels; l++) {
      const [lw, lh, ld] = o.levels?.[l] ?? [Math.max(o.w >> l, 1), Math.max(o.h >> l, 1), 1];
      w.be32(lw).be32(lh);
      if (layered) w.be32(ld);
      const px = new Uint8Array(lw * lh * ld * 4).fill(16 * f + l + 1);
      w.be32(px.length).write(px);
    }
  }
  w.be32(3).be32(GL.MIN_FILTER).be32(GL.LINEAR).be32(GL.MAG_FILTER).be32(GL.NEAREST).be32(GL.WRAP_S).be32(GL.REPEAT);
  return w.bytes();
}

// RenderThread::save (render_thread.cpp) over the thread's saveSnapshot stream
export function channelBlob(o: { context?: number; draw?: number; read?: number } = {}): Uint8Array {
  const inner = new W();
  inner.be32(1024).be32(3).write(new Uint8Array([1, 2, 3])); // ReadBuffer::onSave
  inner.be32(16384).be32(16000).byte(true); // IOStream::save
  inner.be32(2).write(new Uint8Array([4, 5])).be32(4).write(new Uint8Array([6, 7, 8, 9])); // ChannelStream::onSave
  inner.byte(4).be32(1).be32(77).be32(78); // ChecksumCalculator::save
  inner.be32(1).be32(o.context ?? 0x11).be32(o.draw ?? 0x60).be32(o.read ?? 0x60).be32(1).be32(0x11).be32(0).be64(5).be64(0); // RenderThreadInfoGl
  inner.be32(0); // no Vulkan info
  const b = inner.bytes();
  return new W().byte(1).be32(b.length).write(b).bytes();
}
