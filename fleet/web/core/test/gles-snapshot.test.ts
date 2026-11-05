// The GLES snapshot parser (core/mirror/gles-snapshot.ts) against byte streams written by a
// small writer that follows gfxstream's C++ save code, one function per onSave (gles-writer.ts).

import { describe, expect, it } from "vitest";
import { DEFAULT_TEXTURE_NAME, type GlSnapshot, parseChannel, parseGlState, parseTexture } from "../mirror/gles-snapshot.ts";
import { channelBlob, FS, GL, glState, glStateWithQueries, textureBlob, VS } from "./gles-writer.ts";

// ---- tests ----

describe("parseGlState", () => {
  let parsed: GlSnapshot | undefined;
  const scene = () => (parsed ??= parseGlState(glState()));

  it("reads the frame, texture index and EGL images", () => {
    const snap = scene();
    expect(snap.version).toBe(6);
    expect(snap.textureIds).toEqual([100, 101, 102, 103]);
    expect([...snap.eglImages]).toEqual([[0x500, 102], [0x501, 103]]);
    expect([snap.framebufferWidth, snap.framebufferHeight, snap.dpr]).toEqual([720, 1280, 2]);
  });

  it("keeps contexts in stream order and attaches each to its share group", () => {
    const snap = scene();
    expect([...snap.contexts.keys()]).toEqual([0x11, 0x33, 0x22]);
    const a = snap.contexts.get(0x11)!;
    const b = snap.contexts.get(0x22)!;
    const c = snap.contexts.get(0x33)!;
    expect([a.gles, a.api, a.shareGroup, a.eglConfigId]).toEqual([3, 3, 7, 33]);
    expect([b.gles, b.initialized, b.state, b.shareGroup]).toEqual([3, false, null, 7]);
    expect(b.v2?.program).toBe(0);
    expect([c.gles, c.v2, c.shareGroup, c.state?.maxTexUnits]).toEqual([1, null, 9, 4]);
    expect(c.state?.vaos.get(0)?.legacyArrays?.get(0x8074)?.clientData).toEqual(new Uint8Array([1, 2, 3, 4]));
    expect([...snap.shareGroups.keys()]).toEqual([7, 9]);
  });

  it("reads a GLES3 context's state", () => {
    const snap = scene();
    const s = snap.contexts.get(0x11)!.state!;
    expect(s.activeTexture).toBe(1);
    expect(s.boundVao).toBe(9);
    expect(s.buffers).toEqual({
      array: 1, elementArray: 1, copyRead: 20, copyWrite: 21, pixelPack: 22, pixelUnpack: 23, transformFeedback: 24,
      uniform: 25, atomicCounter: 26, dispatchIndirect: 27, drawIndirect: 28, shaderStorage: 29, textureBuffer: 30,
    });
    expect(s.indexed.transformFeedback[1]).toEqual({ buffer: 1, offset: 16, size: 64, stride: 0, divisor: 0, isBindBase: true });
    expect(s.indexed.uniform.length).toBe(2);
    expect(s.viewport).toEqual({ x: 0, y: 0, width: 720, height: 1280 });
    expect(s.scissor).toBeNull();
    expect(s.polygonOffset).toEqual({ factor: 1.5, units: 2.5 });
    expect([...s.enables]).toEqual([[GL.BLEND, true], [GL.DEPTH_TEST, false]]);
    expect(s.blend).toEqual([{
      enabled: true, equationRgb: GL.FUNC_ADD, equationAlpha: GL.FUNC_ADD, srcRgb: GL.SRC_ALPHA, dstRgb: GL.ONE_MINUS_SRC_ALPHA,
      srcAlpha: GL.ONE, dstAlpha: GL.ONE_MINUS_SRC_ALPHA, colorMask: [true, false, true, true],
    }]);
    expect([...s.pixelStore]).toEqual([[GL.UNPACK_ALIGNMENT, 1]]);
    expect([s.cullFace, s.frontFace, s.depthFunc, s.depthMask, s.lineWidth]).toEqual([GL.BACK, GL.CCW, GL.LESS, true, 2]);
    expect(s.depthRange).toEqual([0, 1]);
    expect(s.sampleCoverage).toEqual({ value: 0.75, invert: true });
    expect(s.stencil.front).toEqual({ func: GL.EQUAL, ref: -3, funcMask: 0xff, writeMask: 0x0f, sfail: GL.KEEP, dpfail: GL.KEEP, dppass: GL.REPLACE });
    expect(s.stencil.back.writeMask).toBe(0xffffffff);
    expect(s.clearColor).toEqual([0.25, 0.5, 0.75, 1]);
    expect([s.clearDepth, s.clearStencil, s.maxTexUnits, s.maxUsedTexUnit]).toEqual([1, 7, 2, 1]);
    expect(s.textureUnits.map((u) => u.textures)).toEqual([[2, 0, 0, 0, 0, 0], [5, 0, 0, 0, 0, 0]]);
    expect([s.renderbuffer, s.drawFramebuffer, s.readFramebuffer, s.defaultDrawBuffer]).toEqual([3, 4, 4, GL.BACK]);
  });

  it("reads VAOs with their attribute pointers, bindings and element buffer", () => {
    const snap = scene();
    const vao = snap.contexts.get(0x11)!.state!.vaos.get(9)!;
    expect(vao.elementArrayBuffer).toBe(1);
    expect(vao.legacyArrays).toBeNull();
    expect(vao.attribs.length).toBe(16);
    expect(vao.attribs[0]).toMatchObject({ size: 3, type: GL.FLOAT, stride: 12, enabled: true, attribType: 1, buffer: 1, offset: 8, clientData: null });
    expect(vao.attribs[1]).toMatchObject({ attribType: 2, value: [1, 2, 3, 4], bindingIndex: 1 });
    expect(vao.attribs[2]?.clientData).toEqual(new Uint8Array([7, 8, 9]));
    expect(vao.bindings[0]).toMatchObject({ buffer: 1, offset: 8, stride: 12 });
    expect(vao.bindings[3]?.divisor).toBe(2);
  });

  it("reads per-context framebuffers, samplers and transform feedbacks", () => {
    const snap = scene();
    const a = snap.contexts.get(0x11)!;
    const fbo = a.framebuffers.get(4)!;
    expect(fbo.attachments.length).toBe(20);
    expect(fbo.attachments[0]).toEqual({ target: GL.TEXTURE_2D, name: 2, objectType: 0, owned: false });
    expect(fbo.attachments[1]).toEqual({ target: GL.RENDERBUFFER, name: 3, objectType: 3, owned: false });
    expect([fbo.drawBuffers, fbo.readBuffer, fbo.hasBeenBound]).toEqual([[GL.COLOR_ATTACHMENT0], GL.COLOR_ATTACHMENT0, true]);
    expect(a.v2).toMatchObject({ attribute0: [0, 0, 0, 1], attribute0Changed: true, attribute0ArrayLength: 2, program: 12 });
    expect([...a.v2!.samplers]).toEqual([[1, 6]]);
    expect(a.v2!.transformFeedbacks.get(0)?.[1]).toMatchObject({ buffer: 1, size: 64, isBindBase: true });
  });

  it("reads a share group's objects by guest name", () => {
    const snap = scene();
    const g = snap.shareGroups.get(7)!;
    const buf = g.buffers.get(1)!;
    expect([buf.size, buf.usage, buf.wasBound, [...buf.data]]).toEqual([8, GL.STATIC_DRAW, true, [1, 2, 3, 4, 5, 6, 7, 8]]);
    expect([...g.textures.keys()]).toEqual([2, DEFAULT_TEXTURE_NAME, 5]);
    expect(g.textures.get(2)).toMatchObject({ target: GL.TEXTURE_2D, width: 64, height: 32, globalName: 101, sourceEGLImage: 0, cropRect: [1, -2, 3, 4] });
    expect([...g.textures.get(2)!.params]).toEqual([[GL.MIN_FILTER, GL.LINEAR]]);
    expect(g.textures.get(5)).toMatchObject({ sourceEGLImage: 0x500, globalName: 102 });
    expect(g.renderbuffers.get(3)).toEqual({
      attachedFramebuffer: 4, attachedPoint: GL.DEPTH_ATTACHMENT, width: 64, height: 32,
      internalFormat: GL.DEPTH_COMPONENT16, hostInternalFormat: GL.DEPTH_COMPONENT16, everBound: true,
    });
    expect(g.shaders.get(10)).toEqual({ type: GL.VERTEX_SHADER, source: VS, infoLog: "ok", programs: [12], compileStatus: true, deleteStatus: false });
    expect(g.shaders.get(11)?.type).toBe(GL.FRAGMENT_SHADER);
    const p = g.programs.get(12)!;
    expect([...p.boundAttribLocations]).toEqual([["a_pos", 0]]);
    expect([...p.linkedAttribLocations]).toEqual([["a_pos", 0], ["a_uv", 1]]);
    expect(p.uniforms.length).toBe(1);
    expect(p.uniforms[0]).toMatchObject({ hostLocation: 3, count: 1, transpose: false, type: GL.FLOAT_VEC4, guestName: "u_color" });
    expect([...new Float32Array(p.uniforms[0]!.value.slice().buffer)]).toEqual([1, 0.5, 0.25, 1]);
    expect([...p.uniformBlockBindings]).toEqual([[0, 2]]);
    expect([p.transformFeedbackVaryings, p.transformFeedbackBufferMode]).toEqual([["v_out"], GL.SEPARATE_ATTRIBS]);
    expect([p.vertex, p.fragment, p.compute]).toEqual([{ shader: 10, linkedSource: VS }, { shader: 11, linkedSource: FS }, { shader: 0, linkedSource: "" }]);
    expect([p.infoLog, p.linkStatus, p.validateStatus, p.inUse, p.deleteStatus, p.glesMajor, p.glesMinor]).toEqual(["linked", true, true, true, false, 3, 0]);
    expect([...p.uniformLocations]).toEqual([["u_color", 1]]);
    const s = g.samplers.get(6)!;
    expect([[...s.params], [...s.floatParams]]).toEqual([[[GL.MIN_FILTER, GL.NEAREST]], [[GL.MIN_LOD, 0.5]]]);
    expect(snap.shareGroups.get(9)?.textures.size).toBe(0);
  });

  it("reads color buffers, window surfaces and process ownership", () => {
    const snap = scene();
    expect(snap.colorBuffers.get(0x40)).toEqual({ handle: 0x40, width: 640, height: 480, format: 1, eglImage: 0x500, blitEglImage: 0x501, refcount: 2, opened: true });
    expect(snap.lastPostedColorBuffer).toBe(0x40);
    expect(snap.windowSurfaces.get(0x60)).toEqual({
      handle: 0x60, attachedColorBuffer: 0x40, readContext: 0x11, drawContext: 0x11, width: 720, height: 1280, eglConfigId: 33, colorBuffer: 0x40,
    });
    expect(snap.procOwned.contexts).toEqual([{ puid: 5n, handles: [0x11, 0x22] }]);
    expect(snap.procOwned.eglImages).toEqual([]);
  });

  it("refuses every truncation with the offset, never reading past the end", () => {
    const full = glState();
    for (let n = 0; n < full.length; n++) {
      expect(() => parseGlState(full.subarray(0, n)), `prefix ${n}`).toThrow(/^GL state: .* at byte \d+$/);
    }
  });

  it("refuses an unknown version, an unknown GLES API, a trailing byte, a bad magic and a query record", () => {
    expect(() => parseGlState(glState({ version: 5 }))).toThrow("GL state: unknown snapshot version 5, expected 6 at byte 0");
    expect(() => parseGlState(glState({ api3: 9 }))).toThrow(/unknown GLES API 9 at byte \d+/);
    const full = glState();
    const extra = new Uint8Array(full.length + 1);
    extra.set(full);
    expect(() => parseGlState(extra)).toThrow(`GL state: 1 bytes after the end at byte ${full.length}`);
    expect(() => parseGlState(glState({ magic: 0xdeadbeef }))).toThrow(`closing magic is not 0xC0FFEEEE at byte ${full.length - 4}`);
    expect(() => parseGlState(glStateWithQueries())).toThrow(/share group query namespace: 1 records in a namespace that holds none/);
  });
});

describe("parseTexture", () => {
  it("reads a mipmapped 2D texture's levels as views into the part", () => {
    const bytes = textureBlob({ target: GL.TEXTURE_2D, w: 4, h: 2, storage: 0, maxLevel: 2 });
    const t = parseTexture(bytes);
    expect(t).toMatchObject({ empty: false, target: GL.TEXTURE_2D, width: 4, height: 2, format: GL.RGBA, internalFormat: GL.RGBA8, type: GL.UNSIGNED_BYTE, maxMipmapLevel: 2 });
    expect(t.faces.length).toBe(1);
    expect(t.faces[0]!.map((l) => [l.width, l.height, l.depth, l.pixels.length])).toEqual([[4, 2, 1, 32], [2, 1, 1, 8], [1, 1, 1, 4]]);
    const px = t.faces[0]![1]!.pixels;
    expect(px.buffer).toBe(bytes.buffer);
    expect([...px]).toEqual(new Array(8).fill(2));
    expect([...t.params]).toEqual([[GL.MIN_FILTER, GL.LINEAR], [GL.MAG_FILTER, GL.NEAREST], [GL.WRAP_S, GL.REPEAT]]);
  });

  it("reads a cube map's six faces, with storage levels over the max mipmap level", () => {
    const t = parseTexture(textureBlob({ target: GL.TEXTURE_CUBE_MAP, w: 2, h: 2, storage: 2, maxLevel: 5 }));
    expect(t.faces.length).toBe(6);
    expect(t.faces.map((f) => f.length)).toEqual([2, 2, 2, 2, 2, 2]);
    expect(t.faces.map((f) => f[0]!.pixels[0])).toEqual([1, 17, 33, 49, 65, 81]);
    expect(t.faces[5]![1]).toMatchObject({ width: 1, height: 1, depth: 1 });
  });

  it("reads a 2D array's layers per level", () => {
    const t = parseTexture(textureBlob({ target: GL.TEXTURE_2D_ARRAY, w: 2, h: 2, d: 3, storage: 1, maxLevel: 0, levels: [[2, 2, 3]] }));
    expect(t.faces[0]!.map((l) => [l.width, l.height, l.depth, l.pixels.length])).toEqual([[2, 2, 3, 48]]);
  });

  it("reads the header alone for an unsupported target, and an empty blob as empty", () => {
    const t = parseTexture(textureBlob({ target: GL.TEXTURE_2D_MULTISAMPLE, w: 4, h: 4, storage: 1, maxLevel: 0 }));
    expect([t.target, t.faces, t.params.size]).toEqual([GL.TEXTURE_2D_MULTISAMPLE, [], 0]);
    expect(parseTexture(new Uint8Array(0))).toMatchObject({ empty: true, target: 0, faces: [] });
  });

  it("refuses every truncation and a trailing byte", () => {
    const full = textureBlob({ target: GL.TEXTURE_CUBE_MAP, w: 2, h: 2, storage: 0, maxLevel: 1 });
    for (let n = 1; n < full.length; n++) {
      expect(() => parseTexture(full.subarray(0, n)), `prefix ${n}`).toThrow(/^texture: .* at byte \d+$/);
    }
    const extra = new Uint8Array(full.length + 1);
    extra.set(full);
    expect(() => parseTexture(extra)).toThrow(/bytes after the end/);
  });
});

describe("parseChannel", () => {
  it("returns the current context and surfaces", () => {
    expect(parseChannel(channelBlob())).toEqual({
      saved: true, context: 0x11, drawSurface: 0x60, readSurface: 0x60, contexts: [0x11], windowSurfaces: [], puid: 5n,
      pendingReadBytes: 3, unreadStreamBytes: 2, pendingWriteBytes: 4, checksumVersion: 1, vkContextId: null,
    });
  });

  it("reads an unfinished thread snapshot as unsaved", () => {
    expect(parseChannel(new Uint8Array([0]))).toMatchObject({ saved: false, context: 0, drawSurface: 0, readSurface: 0 });
  });

  it("refuses every truncation", () => {
    const full = channelBlob();
    for (let n = 0; n < full.length; n++) {
      expect(() => parseChannel(full.subarray(0, n)), `prefix ${n}`).toThrow(/^channel( thread stream)?: .* at byte \d+$/);
    }
  });
});
