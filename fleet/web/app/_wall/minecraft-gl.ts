// Minecraft mode's WebGL2: one context for the whole stage. Each world is drawn into its tile's
// rectangle with viewport and scissor, in six calls: sky, the columns' tops and their sides
// (instanced, eight bytes each), the world's boxes (trunks, plants, lone trees), the figures, and
// water. What is drawn is decided in
// core/minecraft3d.ts; this file holds the shaders and the buffers.

import { BOX_FLOATS, COLUMN_BYTES, MATERIAL_RGB01, project, viewProj, type Box, type McCamera, type McScene, type McSky, type McVec3 } from "../../core/index.ts";

const COLUMN_VS = `#version 300 es
layout(location=0) in vec3 aPos; layout(location=1) in float aFace; layout(location=2) in uvec2 aAt; layout(location=3) in uvec4 aCell;
uniform mat4 uVP; uniform vec3 uEye; uniform highp int uPass; uniform float uStep; uniform sampler2D uPal;
out vec3 vWorld; out vec2 vUV; out vec3 vAlb; flat out vec3 vN; flat out vec3 vFlag; out float vShade;
float hash(vec2 p) { return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453); }
void main() {
  float cx = float(aAt.x), cz = float(aAt.y);
  uint mat = aCell.x; float h = float(aCell.y), base = float(aCell.z); uint fl = aCell.w;
  float depth = float((fl >> 2u) & 7u);
  bool canopy = ((fl >> 6u) & 1u) == 1u;
  bool water = mat == 1u && !canopy;
  vec3 p = aPos;
  // Only the two sides that face the camera are drawn.
  float sx = uEye.x > cx + 0.5 * uStep ? 1.0 : 0.0, sz = uEye.z > cz + 0.5 * uStep ? 1.0 : 0.0;
  vN = vec3(0.0, 1.0, 0.0);
  if (aFace > 0.5 && aFace < 1.5) { p.x = sx; vN = vec3(sx * 2.0 - 1.0, 0.0, 0.0); }
  if (aFace > 1.5) { p.z = sz; vN = vec3(0.0, 0.0, sz * 2.0 - 1.0); }
  vec3 alb = texelFetch(uPal, ivec2(int(mat), 0), 0).rgb;
  if (canopy) alb *= 0.82 + 0.34 * hash(vec2(cx, cz));
  float top = h;
  if (water) {
    // Pass 0 draws the lake's bed, pass 1 its surface.
    if (uPass == 0) { top = h - depth; alb = mix(vec3(0.72, 0.66, 0.46), vec3(0.2, 0.26, 0.3), depth / 7.0); base = min(base, top); }
    else top = h - 0.14;
  }
  vWorld = vec3(cx + p.x * uStep, mix(min(base, top), top, p.y), cz + p.z * uStep);
  vUV = aFace < 0.5 ? vWorld.xz : (aFace < 1.5 ? vWorld.zy : vWorld.xy);
  vShade = (aFace < 0.5 ? 1.0 - float(fl & 3u) * 0.07 : mix(canopy ? 0.78 : 0.6, 1.0, p.y)) * (0.8 + (h - 126.0) * 0.006);
  vAlb = alb; vFlag = vec3(float((fl >> 7u) & 1u), float((fl >> 5u) & 1u), canopy ? 1.0 : 0.0);
  gl_Position = uVP * vec4(vWorld, 1.0);
}`;

const LIGHT = `
uniform vec3 uSun, uSunCol, uAmb, uFogCol, uEye; uniform vec2 uFog; uniform float uNight; uniform vec3 uLamp[3];
vec3 shade(vec3 alb, vec3 n, vec3 w, float emis) {
  vec3 c = alb * (uAmb + uSunCol * max(dot(n, uSun), 0.0));
  // At night each bot carries a light.
  for (int i = 0; i < 3; i++) { vec3 d = uLamp[i] - w; c += alb * vec3(1.0, 0.72, 0.4) * uNight * 3.2 * max(dot(n, normalize(d)), 0.25) / (1.0 + 0.09 * dot(d, d)); }
  return mix(c, alb * 1.25, emis);
}
vec3 fog(vec3 c, vec3 w) { return mix(c, uFogCol, smoothstep(uFog.x, uFog.y, distance(w, uEye))); }`;

const COLUMN_FS = `#version 300 es
precision highp float; precision highp int;
in vec3 vWorld; in vec2 vUV; in vec3 vAlb; flat in vec3 vN; flat in vec3 vFlag; in float vShade;
uniform int uPass; uniform vec3 uHor; uniform float uNearCut, uSubCut; uniform vec3 uSubject;
out vec4 o;
${LIGHT}
float hash(vec2 p) { return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453); }
void main() {
  // A canopy between the camera and what it looks at is left out.
  if (vFlag.z > 0.5 && (distance(vWorld.xz, uEye.xz) < uNearCut || distance(vWorld.xz, uSubject.xz) < uSubCut)) discard;
  vec2 g = fract(vUV); float e = min(min(g.x, 1.0 - g.x), min(g.y, 1.0 - g.y));
  // Block edges and a little grain, fading out with distance.
  float near = clamp(1.0 - (fwidth(vUV.x) + fwidth(vUV.y)) * 5.0, 0.0, 1.0);
  if (uPass == 1) {
    vec3 v = normalize(uEye - vWorld); float fr = pow(1.0 - max(v.y, 0.0), 3.0);
    vec3 c = mix(shade(vAlb, vN, vWorld, 0.0) * 0.9, uHor, 0.15 + 0.55 * fr);
    o = vec4(fog(c, vWorld), 0.62 + 0.3 * fr); return;
  }
  vec3 alb = vAlb * (0.95 + 0.1 * hash(floor(vUV) + vN.xz * 7.0) * near);
  alb *= 1.0 - (1.0 - smoothstep(0.0, 0.05, e)) * 0.24 * near;
  vec3 c = shade(alb, vN, vWorld, vFlag.y * 0.6) * vShade;
  // A column that differs from the fork's source is outlined.
  if (vFlag.x > 0.5) c = mix(c, vec3(1.0, 0.93, 0.55), (1.0 - smoothstep(0.06, 0.16, e)) * 0.95 + 0.12);
  o = vec4(fog(c, vWorld), 1.0);
}`;

const BOX_VS = `#version 300 es
layout(location=0) in vec3 aPos; layout(location=1) in vec3 aN;
layout(location=2) in vec3 iPos; layout(location=3) in vec3 iSize; layout(location=4) in float iYaw; layout(location=5) in vec4 iCol;
uniform mat4 uVP; out vec3 vWorld; out vec3 vN; out vec4 vCol; out float vY;
void main() {
  float c = cos(iYaw), s = sin(iYaw);
  vec3 l = (aPos - vec3(0.5, 0.0, 0.5)) * iSize;
  vWorld = iPos + vec3(l.x * c - l.z * s, l.y, l.x * s + l.z * c);
  vN = vec3(aN.x * c - aN.z * s, aN.y, aN.x * s + aN.z * c); vCol = iCol; vY = aPos.y;
  gl_Position = uVP * vec4(vWorld, 1.0);
}`;

const BOX_FS = `#version 300 es
precision highp float; precision highp int;
in vec3 vWorld; in vec3 vN; in vec4 vCol; in float vY; out vec4 o;
uniform float uNearCut, uSubCut; uniform vec3 uSubject;
${LIGHT}
void main() {
  // A canopy (kind -1) between the camera and what it looks at is left out; trunks stay.
  if (vCol.a < 0.0 && vCol.a > -1.5 && (distance(vWorld.xz, uEye.xz) < uNearCut || distance(vWorld.xz, uSubject.xz) < uSubCut)) discard;
  vec3 c = shade(vCol.rgb, normalize(vN), vWorld, max(vCol.a, 0.0)) * mix(0.8, 1.0, vY);
  o = vec4(fog(c, vWorld), 1.0);
}`;

const SKY_VS = `#version 300 es
layout(location=0) in vec2 aPos; out vec2 vP; void main() { vP = aPos; gl_Position = vec4(aPos, 0.999, 1.0); }`;

const SKY_FS = `#version 300 es
precision highp float;
in vec2 vP; uniform vec3 uZen, uHor, uFogCol, uSunCol; uniform vec3 uSunNdc; uniform float uHorizon, uNight, uAsp; out vec4 o;
float hash(vec2 p) { return fract(sin(dot(p, vec2(12.9898, 78.233))) * 43758.5453); }
void main() {
  float t = (vP.y - uHorizon) / (1.0 - uHorizon + 0.001);
  vec3 c = t > 0.0 ? mix(uHor, uZen, pow(clamp(t, 0.0, 1.0), 0.55)) : mix(uHor, uFogCol, clamp(-t * 6.0, 0.0, 1.0));
  if (uSunNdc.z > 0.5) { float r = length((vP - uSunNdc.xy) * vec2(uAsp, 1.0)); c += uSunCol * (smoothstep(0.075, 0.06, r) * 0.9 + 0.35 * exp(-r * 5.0)); }
  vec2 cell = floor(gl_FragCoord.xy / 2.0);
  if (t > 0.02 && hash(cell) > 0.9975) c += vec3(0.9) * uNight * hash(cell + 1.0);
  o = vec4(c, 1.0);
}`;

interface Program {
  p: WebGLProgram;
  u: Record<string, WebGLUniformLocation | null>;
}

function program(gl: WebGL2RenderingContext, vs: string, fs: string): Program {
  const p = gl.createProgram();
  for (const [type, src] of [
    [gl.VERTEX_SHADER, vs],
    [gl.FRAGMENT_SHADER, fs],
  ] as const) {
    const s = gl.createShader(type)!;
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(`shader: ${gl.getShaderInfoLog(s) ?? ""}`);
    gl.attachShader(p, s);
    gl.deleteShader(s);
  }
  gl.linkProgram(p);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(`program: ${gl.getProgramInfoLog(p) ?? ""}`);
  const u: Program["u"] = {};
  const count = gl.getProgramParameter(p, gl.ACTIVE_UNIFORMS) as number;
  for (let i = 0; i < count; i++) {
    const name = gl.getActiveUniform(p, i)!.name.replace("[0]", "");
    u[name] = gl.getUniformLocation(p, name);
  }
  return { p, u };
}

/** A world's buffers on the GPU: three lists of column instances, and its boxes. */
interface GpuWorld {
  scene: McScene;
  lists: { vao: WebGLVertexArrayObject; buf: WebGLBuffer }[];
  boxes: WebGLVertexArrayObject;
  boxBuf: WebGLBuffer;
}

export interface WorldDraw {
  id: string;
  /** CSS pixels from the canvas's top left corner. */
  rect: Box;
  scene: McScene;
  cam: McCamera;
  sky: McSky;
  /** Box instances drawn this frame only: the figures. */
  figures: Float32Array;
  /** Up to three lights the bots carry at night. */
  lamps: readonly McVec3[];
}

export class McGl {
  /** The context was lost: nothing is drawn any more. */
  lost = false;
  /** The browser draws WebGL on the CPU (SwiftShader, llvmpipe): expect long frames. */
  readonly software: boolean;
  /** Draw calls of the last frame. */
  calls = 0;
  private readonly canvas: HTMLCanvasElement;
  private readonly gl: WebGL2RenderingContext;
  private readonly column: Program;
  private readonly box: Program;
  private readonly sky: Program;
  private readonly columnGeo: WebGLBuffer;
  private readonly cubeGeo: WebGLBuffer;
  private readonly palette: WebGLTexture;
  private readonly skyVao: WebGLVertexArrayObject;
  private readonly figVao: WebGLVertexArrayObject;
  private readonly figBuf: WebGLBuffer;
  private readonly worlds = new Map<string, GpuWorld>();
  private scale = 1;
  /** Signals when the GPU has finished the last frame. */
  private fence: WebGLSync | null = null;
  private readonly onLost: (e: Event) => void;

  /** Null when the browser gives no WebGL2 context or cannot build the shaders. */
  static create(canvas: HTMLCanvasElement, preserve: boolean, lost: () => void): McGl | null {
    const gl = canvas.getContext("webgl2", { antialias: true, alpha: false, preserveDrawingBuffer: preserve, powerPreference: "low-power" });
    if (!gl) return null;
    try {
      return new McGl(canvas, gl, lost);
    } catch {
      return null;
    }
  }

  private constructor(canvas: HTMLCanvasElement, gl: WebGL2RenderingContext, lost: () => void) {
    this.canvas = canvas;
    this.gl = gl;
    const info = gl.getExtension("WEBGL_debug_renderer_info");
    this.software = /swiftshader|llvmpipe|software/i.test(String(info ? gl.getParameter(info.UNMASKED_RENDERER_WEBGL) : ""));
    this.column = program(gl, COLUMN_VS, COLUMN_FS);
    this.box = program(gl, BOX_VS, BOX_FS);
    this.sky = program(gl, SKY_VS, SKY_FS);
    // A column: its top, then its x side and its z side (the shader moves each to face the camera).
    const quad = (a: number[], b: number[], c: number[], d: number[], extra: number[]) => [a, b, c, a, c, d].flatMap((v) => [...v, ...extra]);
    const column = [...quad([0, 1, 0], [1, 1, 0], [1, 1, 1], [0, 1, 1], [0]), ...quad([0, 0, 0], [0, 0, 1], [0, 1, 1], [0, 1, 0], [1]), ...quad([0, 0, 0], [1, 0, 0], [1, 1, 0], [0, 1, 0], [2])];
    this.columnGeo = this.buffer(new Float32Array(column));
    const cube = [
      ...quad([0, 1, 0], [0, 1, 1], [1, 1, 1], [1, 1, 0], [0, 1, 0]),
      ...quad([0, 0, 0], [0, 0, 1], [0, 1, 1], [0, 1, 0], [-1, 0, 0]),
      ...quad([1, 0, 0], [1, 0, 1], [1, 1, 1], [1, 1, 0], [1, 0, 0]),
      ...quad([0, 0, 0], [1, 0, 0], [1, 1, 0], [0, 1, 0], [0, 0, -1]),
      ...quad([0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 1, 1], [0, 0, 1]),
    ];
    this.cubeGeo = this.buffer(new Float32Array(cube));
    const pal = new Uint8Array(64 * 4);
    MATERIAL_RGB01.forEach((c, i) => pal.set([c[0] * 255, c[1] * 255, c[2] * 255, 255], i * 4));
    this.palette = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, this.palette);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, 64, 1, 0, gl.RGBA, gl.UNSIGNED_BYTE, pal);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
    this.skyVao = gl.createVertexArray();
    gl.bindVertexArray(this.skyVao);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.buffer(new Float32Array([-1, -1, 1, -1, 1, 1, -1, -1, 1, 1, -1, 1])));
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
    this.figBuf = gl.createBuffer();
    this.figVao = this.boxVao(this.figBuf);
    gl.bindVertexArray(null);
    this.onLost = (e) => {
      e.preventDefault();
      this.lost = true;
      this.worlds.clear();
      this.fence = null;
      lost();
    };
    canvas.addEventListener("webglcontextlost", this.onLost);
  }

  private buffer(data: Float32Array): WebGLBuffer {
    const gl = this.gl;
    const b = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, b);
    gl.bufferData(gl.ARRAY_BUFFER, data, gl.STATIC_DRAW);
    return b;
  }

  /** A vertex array that draws the unit cube once per instance of `instances`. */
  private boxVao(instances: WebGLBuffer): WebGLVertexArrayObject {
    const gl = this.gl;
    const vao = gl.createVertexArray();
    gl.bindVertexArray(vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.cubeGeo);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 3, gl.FLOAT, false, 24, 0);
    gl.enableVertexAttribArray(1);
    gl.vertexAttribPointer(1, 3, gl.FLOAT, false, 24, 12);
    gl.bindBuffer(gl.ARRAY_BUFFER, instances);
    for (const [loc, size, off] of [
      [2, 3, 0],
      [3, 3, 12],
      [4, 1, 24],
      [5, 4, 28],
    ] as const) {
      gl.enableVertexAttribArray(loc);
      gl.vertexAttribPointer(loc, size, gl.FLOAT, false, BOX_FLOATS * 4, off);
      gl.vertexAttribDivisor(loc, 1);
    }
    return vao;
  }

  /** How many worlds hold buffers on the GPU. */
  get held(): number {
    return this.worlds.size;
  }

  /** Size the canvas for a stage of `w` x `h` CSS pixels at `scale` device pixels each. */
  resize(w: number, h: number, scale: number): void {
    const W = Math.max(1, Math.round(w * scale));
    const H = Math.max(1, Math.round(h * scale));
    if (this.canvas.width !== W || this.canvas.height !== H) {
      this.canvas.width = W;
      this.canvas.height = H;
    }
    this.scale = scale;
  }

  /** Start a frame: the whole canvas is the page's background. */
  begin(): void {
    const gl = this.gl;
    this.calls = 0;
    gl.disable(gl.SCISSOR_TEST);
    gl.clearColor(6 / 255, 8 / 255, 11 / 255, 1);
    gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
  }

  /** A vertex array that draws the column's faces once per instance of `instances`. */
  private columnVao(instances: WebGLBuffer): WebGLVertexArrayObject {
    const gl = this.gl;
    const vao = gl.createVertexArray();
    gl.bindVertexArray(vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.columnGeo);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 3, gl.FLOAT, false, 16, 0);
    gl.enableVertexAttribArray(1);
    gl.vertexAttribPointer(1, 1, gl.FLOAT, false, 16, 12);
    gl.bindBuffer(gl.ARRAY_BUFFER, instances);
    gl.enableVertexAttribArray(2);
    gl.vertexAttribIPointer(2, 2, gl.UNSIGNED_SHORT, COLUMN_BYTES, 0);
    gl.vertexAttribDivisor(2, 1);
    gl.enableVertexAttribArray(3);
    gl.vertexAttribIPointer(3, 4, gl.UNSIGNED_BYTE, COLUMN_BYTES, 4);
    gl.vertexAttribDivisor(3, 1);
    return vao;
  }

  /** End a frame: leave a marker the GPU reaches when it has drawn it. */
  end(): void {
    const gl = this.gl;
    if (this.fence) gl.deleteSync(this.fence);
    this.fence = gl.fenceSync(gl.SYNC_GPU_COMMANDS_COMPLETE, 0);
    gl.flush();
  }

  /**
   * Whether the GPU is still drawing the last frame. The page draws no new frame until it is done:
   * a renderer slower than the page would otherwise fall ever further behind, and the time a frame
   * really took is what the frame guard needs.
   */
  busy(): boolean {
    if (this.fence === null || this.lost) return false;
    const gl = this.gl;
    const r = gl.clientWaitSync(this.fence, 0, 0);
    if (r === gl.TIMEOUT_EXPIRED) return true;
    gl.deleteSync(this.fence);
    this.fence = null;
    return false;
  }

  private upload(id: string, scene: McScene): GpuWorld {
    const gl = this.gl;
    let w = this.worlds.get(id);
    if (!w) {
      const lists = [0, 1, 2].map(() => {
        const buf = gl.createBuffer();
        return { vao: this.columnVao(buf), buf };
      });
      const boxBuf = gl.createBuffer();
      w = { scene, lists, boxes: this.boxVao(boxBuf), boxBuf };
      this.worlds.set(id, w);
    } else if (w.scene === scene) return w;
    w.scene = scene;
    [scene.tops, scene.sides, scene.water].forEach((data, k) => {
      gl.bindBuffer(gl.ARRAY_BUFFER, w.lists[k]!.buf);
      gl.bufferData(gl.ARRAY_BUFFER, data, gl.DYNAMIC_DRAW);
    });
    gl.bindBuffer(gl.ARRAY_BUFFER, w.boxBuf);
    gl.bufferData(gl.ARRAY_BUFFER, scene.boxes, gl.DYNAMIC_DRAW);
    return w;
  }

  /** Draw one world into its rectangle. */
  world(d: WorldDraw): void {
    if (this.lost) return;
    const gl = this.gl;
    const g = this.upload(d.id, d.scene);
    const x = Math.round(d.rect.x * this.scale);
    const w = Math.round(d.rect.w * this.scale);
    const h = Math.round(d.rect.h * this.scale);
    const y = this.canvas.height - Math.round(d.rect.y * this.scale) - h;
    if (w <= 0 || h <= 0) return;
    gl.viewport(x, y, w, h);
    gl.enable(gl.SCISSOR_TEST);
    gl.scissor(x, y, w, h);
    const { cam, sky } = d;
    const aspect = w / h;
    const vp = viewProj(cam, aspect);
    const flat = Math.hypot(cam.target[0] - cam.eye[0], cam.target[2] - cam.eye[2]) || 1;
    const horizon = project(vp, [cam.eye[0] + ((cam.target[0] - cam.eye[0]) / flat) * 1e4, cam.eye[1], cam.eye[2] + ((cam.target[2] - cam.eye[2]) / flat) * 1e4]);
    const sun = project(vp, [cam.eye[0] + sky.light[0] * 1e4, cam.eye[1] + sky.light[1] * 1e4, cam.eye[2] + sky.light[2] * 1e4]);

    gl.disable(gl.DEPTH_TEST);
    gl.disable(gl.BLEND);
    gl.disable(gl.CULL_FACE);
    gl.useProgram(this.sky.p);
    let u = this.sky.u;
    gl.uniform3fv(u["uZen"]!, sky.zenith);
    gl.uniform3fv(u["uHor"]!, sky.horizon);
    gl.uniform3fv(u["uFogCol"]!, sky.fog);
    gl.uniform3fv(u["uSunCol"]!, sky.sunUp ? sky.lightColor : [0.5, 0.55, 0.7]);
    gl.uniform3f(u["uSunNdc"]!, sun?.[0] ?? 0, sun?.[1] ?? 0, sun ? 1 : 0);
    gl.uniform1f(u["uHorizon"]!, horizon?.[1] ?? -1);
    gl.uniform1f(u["uNight"]!, sky.night);
    gl.uniform1f(u["uAsp"]!, aspect);
    gl.bindVertexArray(this.skyVao);
    gl.drawArrays(gl.TRIANGLES, 0, 6);

    gl.enable(gl.DEPTH_TEST);
    gl.clear(gl.DEPTH_BUFFER_BIT);
    const lamps = new Float32Array(9);
    d.lamps.slice(0, 3).forEach((l, i) => lamps.set(l, i * 3));
    const light = (p: Program) => {
      gl.useProgram(p.p);
      gl.uniformMatrix4fv(p.u["uVP"]!, false, vp);
      gl.uniform3fv(p.u["uEye"]!, cam.eye);
      gl.uniform3fv(p.u["uSun"]!, sky.light);
      gl.uniform3fv(p.u["uSunCol"]!, sky.lightColor);
      gl.uniform3fv(p.u["uAmb"]!, sky.ambient);
      gl.uniform3fv(p.u["uFogCol"]!, sky.fog);
      gl.uniform2f(p.u["uFog"]!, cam.fog[0], cam.fog[1]);
      gl.uniform1f(p.u["uNight"]!, sky.night);
      gl.uniform3fv(p.u["uLamp"]!, lamps);
    };
    light(this.column);
    u = this.column.u;
    gl.uniform1f(u["uStep"]!, d.scene.step);
    gl.uniform1i(u["uPass"]!, 0);
    gl.uniform3fv(u["uHor"]!, sky.horizon);
    gl.uniform1f(u["uNearCut"]!, cam.nearCut);
    gl.uniform1f(u["uSubCut"]!, cam.subCut);
    gl.uniform3fv(u["uSubject"]!, cam.subject);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.palette);
    gl.uniform1i(u["uPal"]!, 0);
    // Tops are the first 6 vertices of a column, its two sides the next 12.
    gl.bindVertexArray(g.lists[0]!.vao);
    gl.drawArraysInstanced(gl.TRIANGLES, 0, 6, d.scene.tops.length / COLUMN_BYTES);
    gl.bindVertexArray(g.lists[1]!.vao);
    gl.drawArraysInstanced(gl.TRIANGLES, 6, 12, d.scene.sides.length / COLUMN_BYTES);

    light(this.box);
    gl.uniform1f(this.box.u["uNearCut"]!, cam.nearCut);
    gl.uniform1f(this.box.u["uSubCut"]!, cam.subCut);
    gl.uniform3fv(this.box.u["uSubject"]!, cam.subject);
    gl.bindVertexArray(g.boxes);
    gl.drawArraysInstanced(gl.TRIANGLES, 0, 30, d.scene.boxCount);
    gl.bindVertexArray(this.figVao);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.figBuf);
    gl.bufferData(gl.ARRAY_BUFFER, d.figures, gl.DYNAMIC_DRAW);
    gl.drawArraysInstanced(gl.TRIANGLES, 0, 30, d.figures.length / BOX_FLOATS);

    gl.useProgram(this.column.p);
    gl.uniform1i(u["uPass"]!, 1);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
    gl.depthMask(false);
    gl.bindVertexArray(g.lists[2]!.vao);
    gl.drawArraysInstanced(gl.TRIANGLES, 0, 6, d.scene.water.length / COLUMN_BYTES);
    gl.depthMask(true);
    gl.disable(gl.BLEND);
    gl.bindVertexArray(null);
    this.calls += 6;
  }

  /** Give back one world's buffers (its tile left the screen or lost its world). */
  release(id: string): void {
    const w = this.worlds.get(id);
    if (!w) return;
    const gl = this.gl;
    for (const l of w.lists) {
      gl.deleteVertexArray(l.vao);
      gl.deleteBuffer(l.buf);
    }
    gl.deleteVertexArray(w.boxes);
    gl.deleteBuffer(w.boxBuf);
    this.worlds.delete(id);
  }

  /** Give back every world's buffers (the tab is hidden); they are uploaded again when drawn. */
  releaseAll(): void {
    for (const id of [...this.worlds.keys()]) this.release(id);
  }

  dispose(): void {
    this.canvas.removeEventListener("webglcontextlost", this.onLost);
    this.releaseAll();
    this.lost = true;
  }
}
