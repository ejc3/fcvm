// Path tracer for the fleet's planet. Every pixel is a pure function of the
// frame parameters and its whole-image coordinates: random numbers are seeded by
// (whole-image pixel, frame or hold seed, sample index), never by the tile, and
// nothing is reduced across pixels, so tiles drawn by different GPUs meet
// exactly.
#include <cuda.h>
#include <cuda_runtime.h>

#include <cstdio>
#include <cstring>
#include <stdexcept>

#include "font.h"
#include "render.h"
#include "world.h"

#define CK(x)                                                                                   \
  do {                                                                                          \
    cudaError_t e_ = (x);                                                                       \
    if (e_ != cudaSuccess) throw std::runtime_error(std::string(#x) + ": " + cudaGetErrorString(e_)); \
  } while (0)

#define TRANS_W 256
#define TRANS_H 64
#define SKY_W 128
#define SKY_H 64
#define CLOUD_N 128
#define CLOUD_BASE 2.3f
#define CLOUD_TOP 4.9f
#define PI_F 3.14159265f

struct Globals {
  const V4* noise;
  const V4* trans;
  const V4* sky;
  cudaTextureObject_t cloud;
  cudaTextureObject_t macro;
};

__constant__ FrameParams cP;
__constant__ Globals cG;
__constant__ V4 cSeg[FONT_MAX_SEGMENTS];
__constant__ short cGStart[128];
__constant__ short cGCount[128];

// ---------------------------------------------------------------- atmosphere

__device__ __forceinline__ V3 f3(float4 v) { return v3(v.x, v.y, v.z); }

__device__ __forceinline__ void densities(float h, float* r, float* m, float* o) {
  *r = __expf(-h * (1.0f / 8.0f));
  *m = __expf(-h * (1.0f / 1.2f));
  *o = fmaxf(0.0f, 1.0f - fabsf(h - 25.0f) * (1.0f / 15.0f));
}
#define RAY_B v3(5.802e-3f, 13.558e-3f, 33.1e-3f)
#define MIE_S 3.996e-3f
#define MIE_E 4.40e-3f
#define OZO_A v3(0.650e-3f, 1.881e-3f, 0.085e-3f)

__device__ __forceinline__ V3 extinction(float h) {
  float r, m, o;
  densities(h, &r, &m, &o);
  return RAY_B * r + v3(MIE_E * m) + OZO_A * o;
}

// Bilinear lookup in a float4 table, u and v in 0..1 (clamped).
__device__ V4 lut(const V4* t, int W, int H, float u, float v) {
  float x = sat(u) * (W - 1), y = sat(v) * (H - 1);
  int x0 = min((int)x, W - 2), y0 = min((int)y, H - 2);
  float fx = x - x0, fy = y - y0;
  V4 a = tab_fetch(t, y0 * W + x0), b = tab_fetch(t, y0 * W + x0 + 1);
  V4 c = tab_fetch(t, (y0 + 1) * W + x0), d = tab_fetch(t, (y0 + 1) * W + x0 + 1);
  auto l = [](V4 p, V4 q, float f) { return v4(p.x + (q.x - p.x) * f, p.y + (q.y - p.y) * f, p.z + (q.z - p.z) * f, p.w + (q.w - p.w) * f); };
  return l(l(a, b, fx), l(c, d, fx), fy);
}

// Transmittance from altitude h toward a direction whose cosine with the local
// vertical is mu, to the top of the atmosphere (0 if the planet is in the way).
__device__ V3 transmittance(float h, float mu) {
  V4 v = lut(cG.trans, TRANS_W, TRANS_H, mu * 0.5f + 0.5f, sqrtf(sat(h / 60.0f)));
  return v3(v.x, v.y, v.z);
}

__global__ void k_transmittance(V4* out) {
  int i = blockIdx.x * blockDim.x + threadIdx.x, j = blockIdx.y;
  if (i >= TRANS_W || j >= TRANS_H) return;
  float mu = (i / (float)(TRANS_W - 1)) * 2.0f - 1.0f;
  float vv = j / (float)(TRANS_H - 1);
  float h = vv * vv * 60.0f;
  float r = PLANET_R + h;
  // ground hit?
  float disc_g = r * r * (mu * mu - 1.0f) + PLANET_R * PLANET_R;
  if (mu < 0.0f && disc_g >= 0.0f) {
    out[j * TRANS_W + i] = v4(0, 0, 0, 0);
    return;
  }
  float d = -r * mu + sqrtf(fmaxf(r * r * (mu * mu - 1.0f) + ATMO_R * ATMO_R, 0.0f));
  const int N = 80;
  float ds = d / N;
  V3 od = v3(0);
  for (int k = 0; k < N; k++) {
    float s = (k + 0.5f) * ds;
    float rr = sqrtf(r * r + s * s + 2.0f * r * mu * s);
    od += extinction(rr - PLANET_R) * ds;
  }
  V3 T = v3(expf(-od.x), expf(-od.y), expf(-od.z));
  out[j * TRANS_W + i] = v4(T.x, T.y, T.z, 1);
}

__device__ __forceinline__ float phase_ray(float c) { return 3.0f / (16.0f * PI_F) * (1.0f + c * c); }
__device__ __forceinline__ float phase_hg(float c, float g) {
  float k = 1.0f + g * g - 2.0f * g * c;
  return (1.0f - g * g) / (4.0f * PI_F * k * sqrtf(k));
}
__device__ __forceinline__ float phase_mie(float c) {
  const float g = 0.8f;
  float k = 3.0f / (8.0f * PI_F) * (1.0f - g * g) / (2.0f + g * g);
  return k * (1.0f + c * c) / powf(1.0f + g * g - 2.0f * g * c, 1.5f);
}

// Unit "up" at p (radial direction from the planet centre).
__device__ __forceinline__ V3 up_at(V3 p) { return normalize(v3(p.x, p.y + PLANET_R, p.z)); }

// Distance along d from o to the sphere of altitude hs (largest root), or -1.
__device__ float sphere_far(V3 o, V3 d, float hs) {
  V3 oc = v3(o.x, o.y + PLANET_R, o.z);
  float b = dot(oc, d);
  float ho = alt_flat(o);
  float c = (ho - hs) * (2.0f * PLANET_R + ho + hs);
  float disc = b * b - c;
  if (disc < 0.0f) return -1.0f;
  return -b + sqrtf(disc);
}
// Nearest positive root, or -1.
__device__ float sphere_near(V3 o, V3 d, float hs) {
  V3 oc = v3(o.x, o.y + PLANET_R, o.z);
  float b = dot(oc, d);
  float ho = alt_flat(o);
  float c = (ho - hs) * (2.0f * PLANET_R + ho + hs);
  float disc = b * b - c;
  if (disc < 0.0f) return -1.0f;
  float s = sqrtf(disc);
  float t0 = -b - s, t1 = -b + s;
  if (t0 > 0.0f) return t0;
  if (t1 > 0.0f) return t1;
  return -1.0f;
}

// Single scattering along o + d*s, s in [0, tmax], N steps. Returns in-scattered
// radiance (for unit sun irradiance scaled by cP.sun_e) and the transmittance.
__device__ V3 scatter(V3 o, V3 d, float tmax, int N, V3* trans_out) {
  V3 sun = cP.sun_dir;
  float c = dot(d, sun);
  float pr = phase_ray(c), pm = phase_mie(c);
  float ds = tmax / N;
  V3 T = v3(1.0f), L = v3(0.0f);
  for (int k = 0; k < N; k++) {
    // quadratic spacing puts more steps near the eye
    float u0 = k / (float)N, u1 = (k + 1) / (float)N;
    float s0 = u0 * u0 * tmax, s1 = u1 * u1 * tmax;
    ds = s1 - s0;
    V3 p = o + d * (0.5f * (s0 + s1));
    float h = fmaxf(alt_flat(p), 0.0f);
    float r, m, oz;
    densities(h, &r, &m, &oz);
    V3 ext = RAY_B * r + v3(MIE_E * m) + OZO_A * oz;
    V3 stepT = v3(__expf(-ext.x * ds), __expf(-ext.y * ds), __expf(-ext.z * ds));
    float mu_s = dot(up_at(p), sun);
    V3 Ts = transmittance(h, mu_s);
    V3 scat = RAY_B * (r * pr) + v3(MIE_S * m * pm);
    // a cheap multiple-scattering term: isotropic, lit by a softened sun
    V3 ms = (RAY_B * r + v3(MIE_S * m)) * (0.09f * sat(mu_s * 3.0f + 0.35f));
    V3 S = (scat * Ts + ms * transmittance(h, fmaxf(mu_s, 0.0f) * 0.6f + 0.4f));
    // integrate S over the step analytically (energy conserving)
    V3 Sint = (S - S * stepT) / vmax(ext, 1e-7f);
    L += T * Sint;
    T *= stepT;
  }
  if (trans_out) *trans_out = T;
  return L * cP.sun_e;
}

// Per-frame sky radiance map around the camera, used for sky light.
__device__ V3 sky_dir_of(int i, int j) {
  float az = (i + 0.5f) / SKY_W * 2.0f * PI_F;
  float v = (j + 0.5f) / SKY_H * 2.0f - 1.0f;  // -1..1
  float el = v * fabsf(v) * 0.5f * PI_F;     // more rows near the horizon
  return v3(cosf(el) * cosf(az), sinf(el), cosf(el) * sinf(az));
}
__global__ void k_sky(V4* out) {
  int i = blockIdx.x * blockDim.x + threadIdx.x, j = blockIdx.y;
  if (i >= SKY_W || j >= SKY_H) return;
  V3 d = sky_dir_of(i, j);
  V3 o = cP.cam.pos;
  V3 L;
  if (d.y < -0.02f) {
    // below the horizon: haze over a dim ground
    float t = 60.0f;
    V3 T;
    L = scatter(o, d, t, 12, &T);
    float ms = sat(cP.sun_dir.y + 0.1f);
    L += T * v3(0.12f, 0.12f, 0.10f) * (cP.sun_e * (ms * 0.12f));
  } else {
    float t = sphere_far(o, d, 60.0f);
    L = scatter(o, d, fmaxf(t, 1.0f), 20, nullptr);
  }
  out[j * SKY_W + i] = v4(L.x, L.y, L.z, 1);
}
__device__ V3 sky_lookup(V3 d) {
  float az = atan2f(d.z, d.x);
  if (az < 0) az += 2.0f * PI_F;
  float el = asinf(clampf(d.y, -1.0f, 1.0f)) / (0.5f * PI_F);  // -1..1
  float v = el >= 0 ? sqrtf(el) : -sqrtf(-el);
  float u = az / (2.0f * PI_F);
  // manual wrap in u
  float x = u * SKY_W - 0.5f, y = (v * 0.5f + 0.5f) * SKY_H - 0.5f;
  int x0 = (int)floorf(x), y0 = (int)floorf(y);
  float fx = x - x0, fy = y - y0;
  int x1 = (x0 + 1) % SKY_W;
  x0 = (x0 + SKY_W) % SKY_W;
  int ya = max(0, min(SKY_H - 1, y0)), yb = max(0, min(SKY_H - 1, y0 + 1));
  V4 a = tab_fetch(cG.sky, ya * SKY_W + x0), b = tab_fetch(cG.sky, ya * SKY_W + x1);
  V4 c = tab_fetch(cG.sky, yb * SKY_W + x0), e = tab_fetch(cG.sky, yb * SKY_W + x1);
  V3 r0 = mix(v3(a.x, a.y, a.z), v3(b.x, b.y, b.z), fx);
  V3 r1 = mix(v3(c.x, c.y, c.z), v3(e.x, e.y, e.z), fx);
  return mix(r0, r1, fy);
}

// Sun irradiance reaching point p (before shadows).
__device__ __forceinline__ V3 sun_at(V3 p) {
  float h = fmaxf(alt_flat(p), 0.0f);
  return cP.sun_e * transmittance(h, dot(up_at(p), cP.sun_dir));
}

// ---------------------------------------------------------------- clouds

__device__ __forceinline__ float cloud_tex(V3 q) { return tex3D<float>(cG.cloud, q.x, q.y, q.z); }

__device__ float cloud_density(V3 p, bool detail) {
  float h = alt_flat(p);
  float h01 = (h - CLOUD_BASE) * (1.0f / (CLOUD_TOP - CLOUD_BASE));
  if (h01 <= 0.0f || h01 >= 1.0f) return 0.0f;
  // weather: where clouds form; drifts with the wind
  V2 wp = v2(p.x, p.z) * (1.0f / 42.0f);  // where clouds form; fixed to the ground
  float cov = fbm(wp + v2(31.7f, 4.1f), 3, cG.noise) * 0.5f + 0.5f;
  cov = smoothstepf(0.3f, 0.66f, cov) * 0.92f;
  if (cov <= 0.01f) return 0.0f;
  // rounded bottoms, cauliflower tops that narrow upward
  float prof = smoothstepf(0.0f, 0.12f, h01) * smoothstepf(1.0f, 0.55f, h01 + 0.25f * (1.0f - cov));
  V3 q = v3((p.x + cP.wind.x) * (1.0f / CLOUD_TILE_KM), h * (1.0f / 9.0f), (p.z + cP.wind.y) * (1.0f / CLOUD_TILE_KM));
  float base = cloud_tex(q);
  float thr = 1.0f - cov * prof;
  float d = (base - thr) / fmaxf(1.0f - thr, 1e-3f);
  if (d <= 0.0f) return 0.0f;
  if (detail) {
    V3 q2 = v3(q.x * 5.0f + 0.31f, q.y * 3.0f, q.z * 5.0f + 0.57f);
    float det = cloud_tex(q2);
    float e = 0.32f * mixf(det, 1.0f - det, sat(h01 * 3.0f));
    d = (d - e) / (1.0f - e);
    if (d <= 0.0f) return 0.0f;
  }
  return d * sat(d * 3.0f) * 28.0f;  // extinction per km
}

// Optical depth from p toward the sun through the cloud layer.
__device__ float cloud_light_od(V3 p, int n) {
  const float steps[6] = {0.06f, 0.12f, 0.22f, 0.4f, 0.7f, 1.2f};
  float od = 0.0f, s = 0.0f;
  for (int k = 0; k < n; k++) {
    float ds = steps[k];
    s += ds;
    od += cloud_density(p + cP.sun_dir * (s - 0.5f * ds), k < 2) * ds;
  }
  return od;
}

// Transmittance of sunlight through clouds reaching a ground point.
__device__ float cloud_shadow(V3 p, V3 l) {
  if (l.y <= 0.01f) return 1.0f;
  float t0 = sphere_near(p, l, CLOUD_BASE), t1 = sphere_near(p, l, CLOUD_TOP);
  if (t0 < 0.0f || t1 < 0.0f) return 1.0f;
  float od = 0.0f;
  const int N = 5;
  float ds = (t1 - t0) / N;
  for (int k = 0; k < N; k++) od += cloud_density(p + l * (t0 + (k + 0.5f) * ds), false) * ds;
  return __expf(-od * 0.7f);
}

struct CloudOut {
  V3 L;
  float T;
  float depth;  // transmittance-weighted distance
};

// March the cloud layer along o + d*s, s in [0, tmax].
__device__ CloudOut march_clouds(V3 o, V3 d, float tmax, int N, float jitter, bool cheap) {
  CloudOut out{v3(0.0f), 1.0f, 0.0f};
  float ho = alt_flat(o);
  float ta, tb;
  if (ho < CLOUD_BASE) {
    ta = sphere_near(o, d, CLOUD_BASE);
    if (ta < 0.0f) return out;
    tb = sphere_near(o, d, CLOUD_TOP);
  } else if (ho > CLOUD_TOP) {
    ta = sphere_near(o, d, CLOUD_TOP);
    if (ta < 0.0f) return out;
    float tbase = sphere_near(o, d, CLOUD_BASE);
    tb = tbase > 0.0f ? tbase : sphere_far(o, d, CLOUD_TOP);
  } else {
    ta = 0.0f;
    float t1 = sphere_near(o, d, CLOUD_TOP), t2 = sphere_near(o, d, CLOUD_BASE);
    tb = (t2 > 0.0f && (t1 < 0.0f || t2 < t1)) ? t2 : t1;
  }
  if (tb < 0.0f) tb = ta + 30.0f;
  tb = fminf(tb, tmax);
  tb = fminf(tb, ta + 45.0f);
  float fade_start = 70.0f;
  if (ta >= tb || ta > 160.0f) return out;
  float seg = tb - ta;
  float ds = seg / N;
  float c = dot(d, cP.sun_dir);
  V3 amb_top = sky_lookup(v3(0, 1, 0)) * 1.1f;
  V3 amb_bot = sky_lookup(normalize(v3(0.3f, 0.15f, 0.2f))) * 0.5f;
  float wsum = 0.0f, dsum = 0.0f;
  for (int k = 0; k < N; k++) {
    float s = ta + (k + jitter) * ds;
    V3 p = o + d * s;
    float sig = cloud_density(p, !cheap && out.T > 0.15f);
    if (s > fade_start) sig *= __expf(-(s - fade_start) * (1.0f / 40.0f));
    if (sig <= 0.0f) continue;
    float od = cloud_light_od(p, cheap ? 3 : 6);
    // several orders of scattering, each softer and less directional
    float S = 0.0f, a = 1.0f, b = 1.0f, g = 1.0f;
    for (int o2 = 0; o2 < 3; o2++) {
      float ph = mixf(phase_hg(c, 0.8f * g), phase_hg(c, -0.25f * g), 0.3f);
      S += a * __expf(-od * b) * ph;
      a *= 0.6f; b *= 0.22f; g *= 0.5f;
    }
    float h01 = sat((alt_flat(p) - CLOUD_BASE) / (CLOUD_TOP - CLOUD_BASE));
    // light reaching the inside is softer near the edges: "powder"
    float powder = 1.0f - 0.6f * __expf(-sig * 0.25f);
    V3 Lin = sun_at(p) * (S * 4.0f * PI_F * 0.5f * powder) + mix(amb_bot, amb_top, h01) * (0.35f + 0.65f * h01);
    float st = __expf(-sig * ds);
    V3 Sint = Lin * (1.0f - st);
    out.L += Sint * out.T;
    wsum += out.T * (1.0f - st);
    dsum += out.T * (1.0f - st) * s;
    out.T *= st;
    if (out.T < 0.01f) break;
  }
  out.depth = wsum > 0.0f ? dsum / wsum : ta;
  return out;
}

// ---------------------------------------------------------------- terrain

// Terrain on the GPU: large-scale fields from the baked texture inside the
// baked square, computed directly outside it.
__device__ __forceinline__ Macro macro_at(V2 p) {
  float u = p.x * (1.0f / MACRO_KM) + 0.5f, v = p.y * (1.0f / MACRO_KM) + 0.5f;
  if (u > 0.001f && u < 0.999f && v > 0.001f && v < 0.999f) {
    float4 m = tex2D<float4>(cG.macro, u, v);
    Macro r;
    r.land = m.x; r.massif = m.y; r.w = v2(m.z, m.w);
    return r;
  }
  return macro_direct(p, cG.noise);
}
__device__ __forceinline__ float terrain(V2 p, int oct) { return terrain_from(p, macro_at(p), oct, cG.noise); }
__device__ __forceinline__ float ground_h(V2 p, int oct) { return fmaxf(terrain(p, oct), 0.0f); }

__global__ void k_macro(float4* out) {
  int i = blockIdx.x * blockDim.x + threadIdx.x, j = blockIdx.y;
  if (i >= MACRO_N) return;
  // texel centres, matching normalized-coordinate filtering
  V2 p = v2(((i + 0.5f) / MACRO_N - 0.5f) * MACRO_KM, ((j + 0.5f) / MACRO_N - 0.5f) * MACRO_KM);
  Macro m = macro_direct(p, cG.noise);
  out[j * MACRO_N + i] = make_float4(m.land, m.massif, m.w.x, m.w.y);
}

__device__ __forceinline__ int march_oct(float t) { return t < 3.0f ? 9 : t < 12.0f ? 8 : t < 40.0f ? 7 : 6; }

// Intersect the ray with terrain and sea. Returns distance or -1. `tol` is the
// accepted miss as a fraction of distance (about a pixel's angle).
__device__ float march_terrain(V3 o, V3 d, float tmin, float tmax, int max_steps, int oct_cap, float tol = 0.0008f) {
  float t = tmin, tp = tmin;
  for (int i = 0; i < max_steps; i++) {
    V3 p = o + d * t;
    int oct = min(march_oct(t), oct_cap);
    float a = alt_flat(p);
    float dh = a - ground_h(v2(p.x, p.z), oct);
    if (dh < tol * t + 0.0002f) {
      if (i == 0) return t;
      // bisect between tp (above) and t (below)
      float lo = tp, hi = t;
      for (int k = 0; k < 5; k++) {
        float m = 0.5f * (lo + hi);
        V3 q = o + d * m;
        float e = alt_flat(q) - ground_h(v2(q.x, q.z), oct);
        if (e < 0.0f) hi = m; else lo = m;
      }
      return 0.5f * (lo + hi);
    }
    float rise = d.y + (p.x * d.x + p.z * d.z) * (1.0f / PLANET_R);
    if (a > TERRAIN_MAX && rise > 0.0f) return -1.0f;
    tp = t;
    t += fmaxf(dh * (t < 4.0f ? 0.45f : 0.6f), 0.0005f + 0.002f * t);
    if (t > tmax) return -1.0f;
  }
  return -1.0f;
}

// 1 if the sun (direction l) is visible from p over the terrain.
__device__ float terrain_shadow(V3 p, V3 l) {
  float t = 0.004f;
  for (int i = 0; i < 44; i++) {
    V3 q = p + l * t;
    float a = alt_flat(q);
    if (a > TERRAIN_MAX) break;
    float dh = a - ground_h(v2(q.x, q.z), t < 0.6f ? 7 : 5);
    if (dh < 0.0f) return 0.0f;
    t += fmaxf(dh * 0.55f, 0.004f + 0.006f * t);
    if (t > 40.0f) break;
  }
  return 1.0f;
}

__device__ V3 terrain_normal(V3 p, float eps, int oct) {
  V2 q = v2(p.x, p.z);
  float hx0 = terrain(q - v2(eps, 0), oct), hx1 = terrain(q + v2(eps, 0), oct);
  float hz0 = terrain(q - v2(0, eps), oct), hz1 = terrain(q + v2(0, eps), oct);
  return normalize(v3(hx0 - hx1, 2.0f * eps, hz0 - hz1));
}

struct Material {
  V3 albedo;
  float rough;
};

__device__ Material terrain_material(V3 p, float h, V3 n) {
  V2 q = v2(p.x, p.z);
  float v = fbm(q * 2.3f + v2(3.1f, 1.7f), 3, cG.noise);
  float v2_ = noise(q * 7.0f, cG.noise);
  float slope = n.y;
  V3 rock = mix(v3(0.33f, 0.28f, 0.23f), v3(0.19f, 0.17f, 0.16f), sat(v * 0.9f + 0.5f + 0.2f * v2_));
  V3 grass = mix(v3(0.13f, 0.16f, 0.055f), v3(0.045f, 0.075f, 0.03f), sat(v * 1.2f + 0.5f));
  V3 dry = v3(0.36f, 0.30f, 0.17f);
  V3 sand = v3(0.52f, 0.46f, 0.35f);
  V3 snow = v3(0.93f, 0.95f, 0.99f);
  float wg = smoothstepf(0.78f, 0.9f, slope + 0.05f * v2_) * (1.0f - smoothstepf(1.1f, 1.7f, h + v * 0.35f));
  V3 veg = mix(grass, dry, sat(smoothstepf(0.45f, 1.2f, h) * 0.6f + v * 0.4f));
  // forests: dark patches on gentle lower slopes
  float fo = fbm(q * 1.1f + v2(7.7f, 3.3f), 4, cG.noise) + 0.25f * v2_;
  float wf = smoothstepf(0.0f, 0.18f, fo) * smoothstepf(0.8f, 0.9f, slope) * (1.0f - smoothstepf(0.9f, 1.4f, h + 0.3f * v)) *
             smoothstepf(0.02f, 0.06f, h);
  veg = mix(veg, v3(0.028f, 0.045f, 0.022f), wf);
  V3 a = mix(rock, veg, wg);
  float ws = smoothstepf(0.014f, 0.003f, h + 0.004f * v2_) * smoothstepf(0.55f, 0.8f, slope);
  a = mix(a, sand, ws);
  float wsn = smoothstepf(1.55f, 1.95f, h + v * 0.45f + 0.35f * (slope - 0.7f)) * smoothstepf(0.55f, 0.78f, slope + 0.06f * v2_);
  a = mix(a, snow, wsn);
  Material m;
  m.albedo = a;
  m.rough = mixf(0.9f, 0.35f, wsn);
  return m;
}

__device__ __forceinline__ V3 cosine_dir(V3 n, float u1, float u2) {
  float r = sqrtf(u1), phi = 2.0f * PI_F * u2;
  V3 t = fabsf(n.y) < 0.99f ? normalize(cross(v3(0, 1, 0), n)) : v3(1, 0, 0);
  V3 b = cross(n, t);
  return normalize(t * (r * cosf(phi)) + b * (r * sinf(phi)) + n * sqrtf(fmaxf(0.0f, 1.0f - u1)));
}

// Direction toward a random point of the sun's disc (soft shadows).
__device__ V3 sun_sample(float u1, float u2) {
  V3 s = cP.sun_dir;
  V3 t = normalize(cross(fabsf(s.y) < 0.99f ? v3(0, 1, 0) : v3(1, 0, 0), s));
  V3 b = cross(s, t);
  float r = 0.012f * sqrtf(u1), a = 2.0f * PI_F * u2;
  return normalize(s + t * (r * cosf(a)) + b * (r * sinf(a)));
}

__device__ float ggx_spec(V3 n, V3 v, V3 l, float rough, float f0) {
  V3 h = normalize(v + l);
  float nh = fmaxf(dot(n, h), 0.0f), nl = fmaxf(dot(n, l), 0.0f), nv = fmaxf(dot(n, v), 1e-3f);
  float a = rough * rough, a2 = a * a;
  float dd = nh * nh * (a2 - 1.0f) + 1.0f;
  float D = a2 / (PI_F * dd * dd);
  float k = a * 0.5f;
  float G = (nl / (nl * (1 - k) + k)) * (nv / (nv * (1 - k) + k));
  float F = f0 + (1.0f - f0) * powf(1.0f - fmaxf(dot(h, v), 0.0f), 5.0f);
  return D * G * F / (4.0f * nv) ;  // already multiplied by nl via G/nl cancellation
}

// Light from the viewers' lights at p with normal n (no shadows).
__device__ V3 point_lights(V3 p, V3 n) {
  V3 L = v3(0.0f);
  for (int i = 0; i < cP.n_lights; i++) {
    V3 dl = cP.lights[i].pos - p;
    float d2 = dot(dl, dl);
    if (d2 > 9.0f) continue;
    float nd = fmaxf(dot(n, dl * rsqrtf(d2)), 0.0f);
    L += cP.lights[i].color * (nd * 0.004f / (d2 + 1e-4f));
  }
  return L;
}

// Radiance leaving a terrain point toward the eye, with one bounce of sky and ground light.
__device__ V3 shade_terrain(V3 p, V3 rd, float t, float pix, Rng& rng, bool full) {
  float h = terrain(v2(p.x, p.z), 8);
  float eps = fmaxf(0.0012f, t * pix);
  int oct = (int)clampf(floorf(log2f(2.0f / eps)) + 1.0f, 5.0f, 12.0f);
  V3 n = terrain_normal(p, eps, oct);
  Material m = terrain_material(p, h, n);
  V3 v = -rd;
  V3 L = v3(0.0f);
  // sun
  V3 l = sun_sample(rng.next(), rng.next());
  float nl = dot(n, l);
  V3 Es = sun_at(p);
  if (nl > 0.0f && Es.x + Es.y + Es.z > 1e-4f) {
    float vis = full ? terrain_shadow(p + n * 0.003f, l) : 1.0f;
    if (vis > 0.0f) vis *= cloud_shadow(p, l);
    float spec = ggx_spec(n, v, l, m.rough, 0.03f);
    L += Es * (vis * (nl / PI_F)) * m.albedo + Es * (vis * spec);
  }
  // one bounce: sky or lit ground
  V3 bd = cosine_dir(n, rng.next(), rng.next());
  V3 Lin;
  float th = full ? march_terrain(p + n * 0.004f, bd, 0.0f, 8.0f, 28, 5, 0.004f) : -1.0f;
  if (th < 0.0f) {
    Lin = sky_lookup(bd) * (1.0f - 0.35f * sat(bd.y * 2.0f));  // clouds shade part of the sky
  } else {
    V3 q = p + n * 0.004f + bd * th;
    float h2 = terrain(v2(q.x, q.z), 6);
    V3 n2 = terrain_normal(q, 0.01f, 6);
    Material m2 = terrain_material(q, h2, n2);
    Lin = m2.albedo * (sun_at(q) * (fmaxf(dot(n2, cP.sun_dir), 0.0f) * 0.75f / PI_F) + sky_lookup(v3(0, 1, 0)) * 0.6f);
  }
  L += m.albedo * Lin;
  L += m.albedo * point_lights(p, n);
  return L;
}

__device__ V3 sun_disc(V3 rd) {
  float c = dot(rd, cP.sun_dir);
  const float cr = 0.99997f;  // ~0.45 deg radius
  if (c < cr) return v3(0.0f);
  float x = sat((c - cr) / (1.0f - cr));
  float limb = 0.55f + 0.45f * sqrtf(x);
  return cP.sun_e * (limb * 9000.0f * smoothstepf(0.0f, 0.15f, x));
}

__device__ V3 stars(V3 rd) {
  float night = smoothstepf(0.02f, -0.12f, cP.sun_dir.y);
  if (night <= 0.0f || rd.y < 0.0f) return v3(0.0f);
  V3 q = rd * 700.0f;
  int ix = (int)floorf(q.x), iy = (int)floorf(q.y), iz = (int)floorf(q.z);
  uint32_t h = hash4((uint32_t)ix, (uint32_t)iy, (uint32_t)iz, 77u);
  if ((h & 1023u) > 2u) return v3(0.0f);
  V3 c = v3(ix + 0.5f, iy + 0.5f, iz + 0.5f) - q;
  float b = __expf(-dot(c, c) * 6.0f) * u01(hash_u32(h)) * night * 2.0f;
  return v3(b * 0.9f, b * 0.95f, b);
}

__device__ V3 sky_ray(V3 o, V3 d, bool with_sun) {
  float tt = sphere_far(o, d, 60.0f);
  V3 T;
  V3 L = scatter(o, d, fmaxf(tt, 1.0f), 24, &T);
  if (with_sun) L += sun_disc(d) * T;
  L += stars(d) * T;
  return L;
}

// Water at p (on the sea surface).
__device__ V3 shade_water(V3 p, V3 rd, float t, Rng& rng, bool full) {
  V2 q = v2(p.x, p.z) * 70.0f;
  float wt = cP.wave_t;
  V3 n1 = noised(q + v2(wt, 0.35f * wt), cG.noise);
  V3 n2 = noised(oct_step(q) * 1.3f + v2(-0.6f * wt, wt), cG.noise);
  V3 n3 = noised(q * 0.21f + v2(0.2f * wt, -0.1f * wt), cG.noise);
  V2 g = v2(n1.y + n2.y * 0.7f + n3.y * 1.5f, n1.z + n2.z * 0.7f + n3.z * 1.5f);
  float s = 0.05f / (1.0f + t * 0.35f);
  V3 n = normalize(v3(-g.x * s, 1.0f, -g.y * s));
  V3 v = -rd;
  float cv = fmaxf(dot(n, v), 0.0f);
  float F = 0.02f + 0.98f * powf(1.0f - cv, 5.0f);
  V3 r = reflect(rd, n);
  if (r.y < 0.004f) r = normalize(v3(r.x, 0.004f, r.z));
  // reflection
  V3 R;
  float tr = full ? march_terrain(p + v3(0, 0.0005f, 0), r, 0.0f, 80.0f, 56, 6, 0.002f) : -1.0f;
  if (tr >= 0.0f) {
    V3 qh = p + r * tr;
    float h2 = terrain(v2(qh.x, qh.z), 6);
    V3 n2 = terrain_normal(qh, fmaxf(0.004f, tr * 0.002f), 6);
    Material m2 = terrain_material(qh, h2, n2);
    V3 Ls = sun_at(qh) * (fmaxf(dot(n2, cP.sun_dir), 0.0f) / PI_F) + sky_lookup(v3(0, 1, 0)) * 0.7f;
    V3 Tair;
    V3 ins = scatter(p, r, tr, 6, &Tair);
    R = m2.albedo * Ls * Tair + ins;
  } else {
    R = sky_ray(p, r, false);
    if (full) {
      CloudOut c = march_clouds(p, r, 1e9f, 16, rng.next(), true);
      R = R * c.T + c.L;
    }
  }
  // glitter of the sun
  V3 l = sun_sample(rng.next(), rng.next());
  float vis = (full && l.y > 0.0f) ? terrain_shadow(p + v3(0, 0.002f, 0), l) * cloud_shadow(p, l) : 1.0f;
  V3 spec = sun_at(p) * (ggx_spec(n, v, l, fminf(0.07f + 0.004f * t, 0.3f), 0.02f) * vis);
  // body of the water: seabed seen through absorbing water
  float depth_m = fmaxf(-terrain(v2(p.x, p.z), 7), 0.0f) * 1000.0f;
  V3 sigma = v3(0.32f, 0.055f, 0.028f);  // per metre
  V3 path = vexp(sigma * (-2.2f * fminf(depth_m, 300.0f)));
  V3 Ein = sun_at(p) * (fmaxf(cP.sun_dir.y, 0.0f) / PI_F) + sky_lookup(v3(0, 1, 0)) * 0.8f;
  V3 bed = v3(0.55f, 0.5f, 0.38f) * Ein;
  V3 deep = v3(0.004f, 0.020f, 0.032f) * Ein * 3.0f;
  V3 body = bed * path + deep * (v3(1.0f) - path);
  // foam on the shore
  float foam = smoothstepf(1.6f, 0.0f, depth_m) * sat(noise(v2(p.x, p.z) * 520.0f + v2(wt * 3.0f, 0.0f), cG.noise) * 1.5f + 0.4f);
  body = mix(body, Ein * 0.75f, foam);
  return body * (1.0f - F) + R * F + spec;
}

struct Hit {
  V3 L;
  float dist;  // primary distance, 1e9 for sky
  float id;    // 0 sky, 1 land, 2 water
};

__device__ Hit trace(V3 o, V3 d, float pix, Rng& rng) {
  Hit hres;
  float th = march_terrain(o, d, 0.0f, 360.0f, 260, 9, fmaxf(0.6f * pix, 0.0002f));
  V3 L;
  if (th >= 0.0f) {
    V3 p = o + d * th;
    float h = terrain(v2(p.x, p.z), 8);
    if (h <= 0.0f) {
      p = o + d * th;
      L = shade_water(p, d, th, rng, true);
      hres.id = 2.0f;
    } else {
      L = shade_terrain(p, d, th, pix, rng, true);
      hres.id = 1.0f;
    }
    V3 T;
    V3 ins = scatter(o, d, th, 12, &T);
    L = L * T + ins;
    hres.dist = th;
  } else {
    L = sky_ray(o, d, true);
    hres.dist = 1e9f;
    hres.id = 0.0f;
  }
  // clouds in front of whatever was hit
  CloudOut c = march_clouds(o, d, th >= 0.0f ? th : 1e9f, 40, rng.next(), false);
  if (c.T < 0.999f) {
    V3 Ta;
    V3 ins = scatter(o, d, c.depth, 8, &Ta);
    L = L * c.T + c.L * Ta + ins * (1.0f - c.T);
  }
  hres.L = L;
  return hres;
}

// ---------------------------------------------------------------- overlays

__device__ float seg_dist(V2 p, V4 s) {
  V2 a = v2(s.x, s.y), b = v2(s.z, s.w);
  V2 pa = p - a, ba = b - a;
  float h = sat(dot(pa, ba) / fmaxf(dot(ba, ba), 1e-6f));
  return length(pa - ba * h);
}

__device__ float glyph_dist(unsigned char ch, V2 p) {
  if (ch >= 128) return 1e9f;
  int s0 = cGStart[ch], n = cGCount[ch];
  float d = 1e9f;
  for (int k = 0; k < n; k++) d = fminf(d, seg_dist(p, cSeg[s0 + k]));
  return d;
}

// Emissive text and light spheres, drawn per frame at the current t on top of
// the accumulated image. Exact (no sampling), so the same on every GPU. They
// are hidden where the pixel's first hit (`depth`, from the frame's or the
// hold's first sample) is nearer than they are.
__device__ V3 overlay(V3 o, V3 d, V3 base, float pix, float depth) {
  for (int i = 0; i < cP.n_text; i++) {
    const GText& g = cP.text[i];
    if (g.alpha <= 0.0f || g.len == 0) continue;
    V3 nrm = cross(g.right, g.up);
    float den = dot(d, nrm);
    if (fabsf(den) < 1e-5f) continue;
    float tp = dot(g.center - o, nrm) / den;
    if (tp <= 0.0f) continue;
    V3 q = o + d * tp - g.center;
    float sc = GLYPH_H / g.height;
    float width = g.len * GLYPH_ADVANCE - (GLYPH_ADVANCE - GLYPH_W);
    V2 uv = v2(dot(q, g.right) * sc + 0.5f * width, dot(q, g.up) * sc + 0.5f * GLYPH_H);
    const float margin = 3.5f;
    if (uv.x < -margin || uv.x > width + margin || uv.y < -margin || uv.y > GLYPH_H + margin) continue;
    int gi = (int)floorf(uv.x / GLYPH_ADVANCE);
    float dist = 1e9f;
    for (int k = gi - 1; k <= gi + 1; k++) {
      if (k < 0 || k >= g.len) continue;
      dist = fminf(dist, glyph_dist(g.ch[k], uv - v2(k * GLYPH_ADVANCE, 0.0f)));
    }
    const float sw = 0.42f;
    float fw = fmaxf(pix * tp * sc, 1e-4f);
    float cov = sat((sw - dist) / fw + 0.5f);
    float glow = __expf(-fmaxf(dist - sw, 0.0f) * 1.3f) * 0.22f;
    if (cov < 1e-3f && glow < 2e-3f) continue;
    if (depth < tp) continue;  // behind terrain: the pixel's first hit is nearer
    float Ta = __expf(-tp * 0.018f);
    V3 c = g.color * (g.alpha * Ta);
    base = mix(base, c, cov * g.alpha) + c * glow;
  }
  for (int i = 0; i < cP.n_lights; i++) {
    const GLight& l = cP.lights[i];
    V3 oc = l.pos - o;
    float tc = dot(oc, d);
    if (tc <= 0.0f) continue;
    float d2 = fmaxf(dot(oc, oc) - tc * tc, 0.0f);
    float r = l.radius;
    float dd = sqrtf(d2);
    float fw = fmaxf(pix * tc, 1e-5f);
    float cov = sat((r - dd) / fw + 0.5f);
    float glow = __expf(-dd / (r * 3.0f)) * 0.6f + __expf(-dd / (r * 20.0f)) * 0.12f;
    if (cov < 1e-3f && glow < 1e-3f) continue;
    if (depth < tc) continue;
    V3 c = l.color * (14.0f * l.alpha * __expf(-tc * 0.018f));
    base = mix(base, c, cov * l.alpha) + c * glow;
  }
  return base;
}

// ---------------------------------------------------------------- frame

__device__ __forceinline__ V3 ray_dir(const GCam& c, float px, float py) {
  float u = (px / cP.full_w) * 2.0f - 1.0f;
  float v = 1.0f - (py / cP.full_h) * 2.0f;
  return normalize(c.fwd + c.right * (u * c.tan_half * c.aspect) + c.up * (v * c.tan_half));
}

__device__ __forceinline__ float aces(float x) {
  return sat((x * (2.51f * x + 0.03f)) / (x * (2.43f * x + 0.59f) + 0.14f));
}
__device__ __forceinline__ float srgb(float x) {
  return x <= 0.0031308f ? 12.92f * x : 1.055f * powf(x, 1.0f / 2.4f) - 0.055f;
}

__global__ void k_frame(uint8_t* rgba, float4* hdr, float4* hold_sum, float2* hold_aux, const float4* taa_in,
                        const float2* taa_aux_in, float4* taa_out, float2* taa_aux_out) {
  int i = blockIdx.x * blockDim.x + threadIdx.x, j = blockIdx.y * blockDim.y + threadIdx.y;
  if (i >= cP.w || j >= cP.h) return;
  int idx = j * cP.w + i;
  uint32_t X = (uint32_t)(cP.x0 + i), Y = (uint32_t)(cP.y0 + j);
  const GCam& cam = cP.cam;
  float pix = 2.0f * cam.tan_half / cP.full_h;  // radians per pixel (approx)
  V3 color;
  float depth = 1e9f, id = 0.0f;
  if (cP.hold) {
    float4 acc = hold_sum[idx];
    float2 aux = hold_aux[idx];
    for (int s = 0; s < cP.spp; s++) {
      int k = cP.sample_base + s;
      if (k >= HOLD_MAX_SAMPLES) break;
      Rng rng{hash4(X, Y, cP.seed, (uint32_t)k)};
      float jx = rng.next(), jy = rng.next();
      Hit hh = trace(cam.pos, ray_dir(cam, X + jx, Y + jy), pix, rng);
      acc.x += hh.L.x; acc.y += hh.L.y; acc.z += hh.L.z; acc.w += 1.0f;
      if (k == 0) aux = make_float2(hh.dist, hh.id);
    }
    hold_sum[idx] = acc;
    hold_aux[idx] = aux;
    float4 th = taa_in[idx];
    float m = th.w * fmaxf(0.0f, 1.0f - acc.w / HOLD_BLEND_OUT);
    float inv = 1.0f / fmaxf(acc.w + m, 1e-6f);
    color = v3((acc.x + th.x * m) * inv, (acc.y + th.y * m) * inv, (acc.z + th.z * m) * inv);
    depth = aux.x; id = aux.y;
    taa_out[idx] = make_float4(color.x, color.y, color.z, fminf(acc.w + m, (float)TAA_MAX_HISTORY));
    taa_aux_out[idx] = make_float2(depth, id);
  } else {
    V3 sum = v3(0.0f);
    for (int s = 0; s < cP.spp; s++) {
      Rng rng{hash4(X, Y, cP.seed, (uint32_t)s)};
      float jx = rng.next(), jy = rng.next();
      Hit hh = trace(cam.pos, ray_dir(cam, X + jx, Y + jy), pix, rng);
      sum += hh.L;
      if (s == 0) { depth = hh.dist; id = hh.id; }
    }
    float n = (float)cP.spp;
    float hist_n = 0.0f;
    V3 hist = v3(0.0f);
    if (cP.have_prev) {
      // reproject this pixel's first hit into the previous frame
      V3 d0 = ray_dir(cam, X + 0.5f, Y + 0.5f);
      const GCam& pc = cP.prev;
      V3 rel = depth < 1e8f ? (cam.pos + d0 * depth - pc.pos) : d0;
      float z = dot(rel, pc.fwd);
      if (z > 1e-6f) {
        float sx = dot(rel, pc.right) / (z * pc.tan_half * pc.aspect);
        float sy = dot(rel, pc.up) / (z * pc.tan_half);
        float px = (sx * 0.5f + 0.5f) * cP.full_w - 0.5f - cP.x0;
        float py = (0.5f - 0.5f * sy) * cP.full_h - 0.5f - cP.y0;
        float expect = depth < 1e8f ? length(rel) : 1e9f;
        int ix = (int)floorf(px), iy = (int)floorf(py);
        float fx = px - ix, fy = py - iy;
        float wsum = 0.0f, nmin = 1e9f;
        for (int b = 0; b < 4; b++) {
          int xx = ix + (b & 1), yy = iy + (b >> 1);
          if (xx < 0 || yy < 0 || xx >= cP.w || yy >= cP.h) continue;
          float w = ((b & 1) ? fx : 1.0f - fx) * ((b >> 1) ? fy : 1.0f - fy);
          float2 a = taa_aux_in[yy * cP.w + xx];
          if (a.y != id) continue;
          if (depth < 1e8f && fabsf(a.x - expect) > 0.04f * expect + 0.002f) continue;
          float4 hv = taa_in[yy * cP.w + xx];
          hist += v3(hv.x, hv.y, hv.z) * w;
          wsum += w;
          nmin = fminf(nmin, hv.w);
        }
        if (wsum > 0.3f) {
          hist = hist / wsum;
          hist_n = fminf(nmin, (float)TAA_MAX_HISTORY);
        }
      }
    }
    float tot = hist_n + n;
    color = (hist * hist_n + sum) / tot;
    taa_out[idx] = make_float4(color.x, color.y, color.z, fminf(tot, (float)TAA_MAX_HISTORY));
    taa_aux_out[idx] = make_float2(depth, id);
  }
  V3 d0 = ray_dir(cam, X + 0.5f, Y + 0.5f);
  color = overlay(cam.pos, d0, color, pix, depth);
  hdr[idx] = make_float4(color.x, color.y, color.z, 1.0f);
  // tone map: ACES-like curve, slight saturation lift, sRGB, dither by whole-image pixel
  V3 c = color * cP.exposure;
  float l = lum(c);
  c = vmax(mix(v3(l), c, 1.08f), 0.0f);
  float dith = (u01(hash4(X, Y, 0xD17Eu, 0u)) - 0.5f) * (1.0f / 255.0f);
  uint8_t* o = rgba + 4 * idx;
  o[0] = (uint8_t)(sat(srgb(aces(c.x)) + dith) * 255.0f + 0.5f);
  o[1] = (uint8_t)(sat(srgb(aces(c.y)) + dith) * 255.0f + 0.5f);
  o[2] = (uint8_t)(sat(srgb(aces(c.z)) + dith) * 255.0f + 0.5f);
  o[3] = 255;
}

// ---------------------------------------------------------------- scaling + NV12

__device__ V3 area_sample(const uint8_t* src, int sw, int sh, float x0, float y0, float x1, float y1) {
  V3 acc = v3(0.0f);
  float wsum = 0.0f;
  int ia = (int)floorf(x0), ib = min((int)ceilf(x1), sw), ja = (int)floorf(y0), jb = min((int)ceilf(y1), sh);
  for (int y = max(ja, 0); y < jb; y++) {
    float wy = fminf(y + 1.0f, y1) - fmaxf((float)y, y0);
    if (wy <= 0) continue;
    for (int x = max(ia, 0); x < ib; x++) {
      float wx = fminf(x + 1.0f, x1) - fmaxf((float)x, x0);
      if (wx <= 0) continue;
      const uint8_t* p = src + 4 * (y * sw + x);
      float w = wx * wy;
      acc += v3(p[0], p[1], p[2]) * w;
      wsum += w;
    }
  }
  return acc / fmaxf(wsum, 1e-6f);
}

__global__ void k_nv12(const uint8_t* src, int sw, int sh, uint8_t* dst, size_t pitch, int dw, int dh) {
  int bx = blockIdx.x * blockDim.x + threadIdx.x, by = blockIdx.y * blockDim.y + threadIdx.y;
  if (2 * bx >= dw || 2 * by >= dh) return;
  float sx = (float)sw / dw, sy = (float)sh / dh;
  V3 rgb4[4];
  for (int k = 0; k < 4; k++) {
    int x = 2 * bx + (k & 1), y = 2 * by + (k >> 1);
    rgb4[k] = area_sample(src, sw, sh, x * sx, y * sy, (x + 1) * sx, (y + 1) * sy);
    // BT.709, limited range
    float Yv = 16.0f + (0.2126f * rgb4[k].x + 0.7152f * rgb4[k].y + 0.0722f * rgb4[k].z) * (219.0f / 255.0f);
    dst[y * pitch + x] = (uint8_t)clampf(Yv + 0.5f, 0.0f, 255.0f);
  }
  V3 m = (rgb4[0] + rgb4[1] + rgb4[2] + rgb4[3]) * 0.25f;
  float Cb = 128.0f + (-0.1146f * m.x - 0.3854f * m.y + 0.5f * m.z) * (224.0f / 255.0f);
  float Cr = 128.0f + (0.5f * m.x - 0.4542f * m.y - 0.0458f * m.z) * (224.0f / 255.0f);
  uint8_t* uv = dst + (size_t)dh * pitch + by * pitch + 2 * bx;
  uv[0] = (uint8_t)clampf(Cb + 0.5f, 0.0f, 255.0f);
  uv[1] = (uint8_t)clampf(Cr + 0.5f, 0.0f, 255.0f);
}

// ---------------------------------------------------------------- cloud volume

__device__ float vhash3(int x, int y, int z, int period, uint32_t seed) {
  x = ((x % period) + period) % period;
  y = ((y % period) + period) % period;
  z = ((z % period) + period) % period;
  return u01(hash4((uint32_t)x, (uint32_t)y, (uint32_t)z, seed));
}
__device__ float vnoise3(V3 p, int period, uint32_t seed) {
  int ix = (int)floorf(p.x), iy = (int)floorf(p.y), iz = (int)floorf(p.z);
  float fx = p.x - ix, fy = p.y - iy, fz = p.z - iz;
  fx = fx * fx * (3 - 2 * fx); fy = fy * fy * (3 - 2 * fy); fz = fz * fz * (3 - 2 * fz);
  float a = 0;
  float c[8];
  for (int k = 0; k < 8; k++) c[k] = vhash3(ix + (k & 1), iy + ((k >> 1) & 1), iz + (k >> 2), period, seed);
  float x00 = mixf(c[0], c[1], fx), x10 = mixf(c[2], c[3], fx), x01 = mixf(c[4], c[5], fx), x11 = mixf(c[6], c[7], fx);
  a = mixf(mixf(x00, x10, fy), mixf(x01, x11, fy), fz);
  return a;
}
__device__ float worley3(V3 p, int period, uint32_t seed) {
  int ix = (int)floorf(p.x), iy = (int)floorf(p.y), iz = (int)floorf(p.z);
  float best = 1e9f;
  for (int dz = -1; dz <= 1; dz++)
    for (int dy = -1; dy <= 1; dy++)
      for (int dx = -1; dx <= 1; dx++) {
        int cx = ix + dx, cy = iy + dy, cz = iz + dz;
        V3 f = v3(vhash3(cx, cy, cz, period, seed), vhash3(cx, cy, cz, period, seed + 1), vhash3(cx, cy, cz, period, seed + 2));
        V3 d = v3(cx + f.x, cy + f.y, cz + f.z) - p;
        best = fminf(best, dot(d, d));
      }
  return sqrtf(best);
}
__global__ void k_cloud_volume(float* out) {
  int x = blockIdx.x * blockDim.x + threadIdx.x, y = blockIdx.y, z = blockIdx.z;
  if (x >= CLOUD_N) return;
  V3 p = v3((x + 0.5f) / CLOUD_N, (y + 0.5f) / CLOUD_N, (z + 0.5f) / CLOUD_N);
  // tileable value-noise fbm and inverted Worley at several frequencies
  float perlin = 0, amp = 0.5f, tot = 0;
  for (int o = 0; o < 5; o++) {
    int f = 4 << o;
    perlin += amp * vnoise3(p * (float)f, f, 11u + o);
    tot += amp;
    amp *= 0.5f;
  }
  perlin /= tot;
  float w1 = 1.0f - worley3(p * 4.0f, 4, 101u), w2 = 1.0f - worley3(p * 8.0f, 8, 202u), w3 = 1.0f - worley3(p * 16.0f, 16, 303u);
  float wf = w1 * 0.625f + w2 * 0.25f + w3 * 0.125f;
  // Perlin-Worley: billowy cells with soft noise between
  float pw = sat((perlin - (1.0f - wf)) / fmaxf(wf, 1e-3f));
  float v = sat(0.55f * wf + 0.6f * pw - 0.08f);
  out[(z * CLOUD_N + y) * CLOUD_N + x] = v;
}

// ---------------------------------------------------------------- host

Renderer::Renderer() {}
Renderer::~Renderer() {}

bool Renderer::init(std::string* err) {
  try {
    CK(cudaSetDevice(0));
    CK(cudaFree(0));
    CUcontext c = nullptr;
    if (cuCtxGetCurrent(&c) != CUDA_SUCCESS || !c) throw std::runtime_error("no current CUDA context");
    ctx_ = c;
    cudaDeviceProp prop;
    CK(cudaGetDeviceProperties(&prop, 0));
    gpu_name_ = prop.name;
    char bus[32] = {0};
    CK(cudaDeviceGetPCIBusId(bus, sizeof bus, 0));
    pci_bus_id_ = bus;
    cudaStream_t st;
    CK(cudaStreamCreateWithFlags(&st, cudaStreamNonBlocking));
    stream_ = st;
    cudaEvent_t e0, e1;
    CK(cudaEventCreate(&e0));
    CK(cudaEventCreate(&e1));
    ev0_ = e0;
    ev1_ = e1;

    // noise table, identical to the host's
    const HostNoise& hn = host_noise();
    CK(cudaMalloc(&noise_, sizeof(V4) * NOISE_N * NOISE_N));
    CK(cudaMemcpyAsync(noise_, hn.tab.data(), sizeof(V4) * NOISE_N * NOISE_N, cudaMemcpyHostToDevice, st));
    // font
    const FontTable& f = font_table();
    CK(cudaMemcpyToSymbolAsync(cSeg, f.seg, sizeof f.seg, 0, cudaMemcpyHostToDevice, st));
    CK(cudaMemcpyToSymbolAsync(cGStart, f.start, sizeof f.start, 0, cudaMemcpyHostToDevice, st));
    CK(cudaMemcpyToSymbolAsync(cGCount, f.count, sizeof f.count, 0, cudaMemcpyHostToDevice, st));
    // transmittance
    CK(cudaMalloc(&trans_, sizeof(V4) * TRANS_W * TRANS_H));
    CK(cudaMalloc(&sky_, sizeof(V4) * SKY_W * SKY_H));
    k_transmittance<<<dim3((TRANS_W + 63) / 64, TRANS_H), 64, 0, st>>>((V4*)trans_);
    CK(cudaGetLastError());
    // cloud volume -> 3D texture with trilinear filtering and wrap
    float* vol = nullptr;
    CK(cudaMalloc(&vol, sizeof(float) * CLOUD_N * CLOUD_N * CLOUD_N));
    k_cloud_volume<<<dim3(CLOUD_N / 64, CLOUD_N, CLOUD_N), 64, 0, st>>>(vol);
    CK(cudaGetLastError());
    cudaChannelFormatDesc cd = cudaCreateChannelDesc<float>();
    cudaArray_t arr;
    CK(cudaMalloc3DArray(&arr, &cd, make_cudaExtent(CLOUD_N, CLOUD_N, CLOUD_N)));
    cudaMemcpy3DParms cp = {};
    cp.srcPtr = make_cudaPitchedPtr(vol, CLOUD_N * sizeof(float), CLOUD_N, CLOUD_N);
    cp.dstArray = arr;
    cp.extent = make_cudaExtent(CLOUD_N, CLOUD_N, CLOUD_N);
    cp.kind = cudaMemcpyDeviceToDevice;
    CK(cudaMemcpy3DAsync(&cp, st));
    cloud_arr_ = arr;
    cudaResourceDesc rd = {};
    rd.resType = cudaResourceTypeArray;
    rd.res.array.array = arr;
    cudaTextureDesc td = {};
    td.addressMode[0] = td.addressMode[1] = td.addressMode[2] = cudaAddressModeWrap;
    td.filterMode = cudaFilterModeLinear;
    td.readMode = cudaReadModeElementType;
    td.normalizedCoords = 1;
    cudaTextureObject_t tex;
    CK(cudaCreateTextureObject(&tex, &rd, &td, nullptr));
    cloud_tex_ = tex;
    Globals g{(const V4*)noise_, (const V4*)trans_, (const V4*)sky_, tex, 0};
    CK(cudaMemcpyToSymbolAsync(cG, &g, sizeof g, 0, cudaMemcpyHostToDevice, st));
    // bake the large-scale terrain fields into a filtered 2D texture
    float4* mac = nullptr;
    CK(cudaMalloc(&mac, sizeof(float4) * MACRO_N * MACRO_N));
    k_macro<<<dim3(MACRO_N / 128, MACRO_N), 128, 0, st>>>(mac);
    CK(cudaGetLastError());
    cudaChannelFormatDesc md = cudaCreateChannelDesc<float4>();
    cudaArray_t marr;
    CK(cudaMallocArray(&marr, &md, MACRO_N, MACRO_N));
    CK(cudaMemcpy2DToArrayAsync(marr, 0, 0, mac, MACRO_N * sizeof(float4), MACRO_N * sizeof(float4), MACRO_N,
                                cudaMemcpyDeviceToDevice, st));
    cudaResourceDesc mrd = {};
    mrd.resType = cudaResourceTypeArray;
    mrd.res.array.array = marr;
    cudaTextureDesc mtd = {};
    mtd.addressMode[0] = mtd.addressMode[1] = cudaAddressModeClamp;
    mtd.filterMode = cudaFilterModeLinear;
    mtd.readMode = cudaReadModeElementType;
    mtd.normalizedCoords = 1;
    cudaTextureObject_t mtex;
    CK(cudaCreateTextureObject(&mtex, &mrd, &mtd, nullptr));
    g.macro = mtex;
    CK(cudaMemcpyToSymbolAsync(cG, &g, sizeof g, 0, cudaMemcpyHostToDevice, st));
    CK(cudaStreamSynchronize(st));
    CK(cudaFree(vol));
    CK(cudaFree(mac));
  } catch (const std::exception& e) {
    *err = e.what();
    return false;
  }
  return true;
}

void Renderer::ensure_buffers(const Tile& t) {
  if (have_tile_ && t.w == tile_.w && t.h == tile_.h && t.full_w == tile_.full_w && t.full_h == tile_.full_h &&
      t.x0 == tile_.x0 && t.y0 == tile_.y0)
    return;
  bool realloc = !have_tile_ || t.w != tile_.w || t.h != tile_.h;
  size_t n = (size_t)t.w * t.h;
  if (realloc) {
    cudaFree(rgba_); cudaFree(hdr_); cudaFree(hold_sum_); cudaFree(hold_aux_);
    for (int i = 0; i < 2; i++) { cudaFree(taa_[i]); cudaFree(taa_aux_[i]); }
    CK(cudaMalloc(&rgba_, n * 4));
    CK(cudaMalloc(&hdr_, n * sizeof(float4)));
    CK(cudaMalloc(&hold_sum_, n * sizeof(float4)));
    CK(cudaMalloc(&hold_aux_, n * sizeof(float2)));
    for (int i = 0; i < 2; i++) {
      CK(cudaMalloc(&taa_[i], n * sizeof(float4)));
      CK(cudaMalloc(&taa_aux_[i], n * sizeof(float2)));
    }
  }
  tile_ = t;
  have_tile_ = true;
  reset_history();
}

void Renderer::reset_history() {
  if (!have_tile_) return;
  size_t n = (size_t)tile_.w * tile_.h;
  cudaStream_t st = (cudaStream_t)stream_;
  for (int i = 0; i < 2; i++) {
    CK(cudaMemsetAsync(taa_[i], 0, n * sizeof(float4), st));
    CK(cudaMemsetAsync(taa_aux_[i], 0, n * sizeof(float2), st));
  }
  CK(cudaMemsetAsync(hold_sum_, 0, n * sizeof(float4), st));
  CK(cudaMemsetAsync(hold_aux_, 0, n * sizeof(float2), st));
  hold_key_ = 0;
  hold_n_ = 0;
  have_prev_ = false;
}

void Renderer::render(const Scene& s, const Tile& tile, double t, bool accumulate, int spp) {
  ensure_buffers(tile);
  cudaStream_t st = (cudaStream_t)stream_;
  uint64_t key = 0;
  bool held = false;
  FrameParams p = build_frame(s, tile, t, &key, &held);
  if (spp <= 0) spp = (int)(budget_ / ((double)tile.w * tile.h));
  spp = spp < 1 ? 1 : spp > 64 ? 64 : spp;
  p.spp = spp;
  size_t n = (size_t)tile.w * tile.h;
  int in = cur_, out = 1 - cur_;
  if (!accumulate) {
    p.hold = 0;
    p.have_prev = 0;
    held = false;
  } else if (held) {
    if (key != hold_key_) {
      CK(cudaMemsetAsync(hold_sum_, 0, n * sizeof(float4), st));
      CK(cudaMemsetAsync(hold_aux_, 0, n * sizeof(float2), st));
      hold_key_ = key;
      hold_n_ = 0;
      hold_src_ = cur_;
    }
    in = hold_src_;
    out = 1 - hold_src_;
    p.sample_base = hold_n_;
    hold_n_ = hold_n_ + spp > HOLD_MAX_SAMPLES ? HOLD_MAX_SAMPLES : hold_n_ + spp;
  } else {
    hold_key_ = 0;
    hold_n_ = 0;
    p.have_prev = have_prev_ ? 1 : 0;
    p.prev = prev_cam_;
  }
  CK(cudaMemcpyToSymbolAsync(cP, &p, sizeof p, 0, cudaMemcpyHostToDevice, st));
  CK(cudaEventRecord((cudaEvent_t)ev0_, st));
  k_sky<<<dim3(SKY_W / 64, SKY_H), 64, 0, st>>>((V4*)sky_);
  dim3 bs(8, 8), gs((tile.w + 7) / 8, (tile.h + 7) / 8);
  k_frame<<<gs, bs, 0, st>>>((uint8_t*)rgba_, (float4*)hdr_, (float4*)hold_sum_, (float2*)hold_aux_,
                             (const float4*)taa_[in], (const float2*)taa_aux_[in], (float4*)taa_[out],
                             (float2*)taa_aux_[out]);
  CK(cudaGetLastError());
  CK(cudaEventRecord((cudaEvent_t)ev1_, st));
  CK(cudaEventSynchronize((cudaEvent_t)ev1_));
  float ms = 0;
  cudaEventElapsedTime(&ms, (cudaEvent_t)ev0_, (cudaEvent_t)ev1_);
  stats_.render_ms = ms;
  stats_.spp = (held && p.sample_base >= HOLD_MAX_SAMPLES) ? 0 : spp;
  stats_.hold = held;
  stats_.hold_samples = held ? hold_n_ : 0;
  if (accumulate) {
    cur_ = out;
    have_prev_ = true;
    prev_cam_ = p.cam;
  }
}

void Renderer::download_rgba(std::vector<uint8_t>* out) const {
  out->resize((size_t)tile_.w * tile_.h * 4);
  CK(cudaMemcpy(out->data(), rgba_, out->size(), cudaMemcpyDeviceToHost));
}
void Renderer::download_hdr(std::vector<float>* out) const {
  out->resize((size_t)tile_.w * tile_.h * 4);
  CK(cudaMemcpy(out->data(), hdr_, out->size() * sizeof(float), cudaMemcpyDeviceToHost));
}

void Renderer::to_nv12(void* dst, size_t pitch, int dw, int dh) {
  cudaStream_t st = (cudaStream_t)stream_;
  dim3 bs(16, 8), gs((dw / 2 + 15) / 16, (dh / 2 + 7) / 8);
  k_nv12<<<gs, bs, 0, st>>>((const uint8_t*)rgba_, tile_.w, tile_.h, (uint8_t*)dst, pitch, dw, dh);
  CK(cudaGetLastError());
  CK(cudaStreamSynchronize(st));
}

void Renderer::sync() { CK(cudaStreamSynchronize((cudaStream_t)stream_)); }

void* Renderer::alloc_nv12(int w, int h, size_t* pitch) {
  void* p = nullptr;
  CK(cudaMallocPitch(&p, pitch, (size_t)w, (size_t)h * 3 / 2));
  CK(cudaMemset2D(p, *pitch, 0, (size_t)w, (size_t)h * 3 / 2));
  return p;
}
void Renderer::free_dev(void* p) { cudaFree(p); }
