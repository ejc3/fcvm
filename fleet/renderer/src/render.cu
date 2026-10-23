// Path tracer for the fleet's planet. Every pixel is a pure function of the
// frame parameters and its whole-image coordinates: random numbers are seeded by
// (whole-image pixel, frame or hold seed, sample index), never by the tile, and
// nothing is reduced across pixels, so tiles drawn by different GPUs meet
// exactly.
#include <cuda.h>
#include <cuda_runtime.h>

#include <cstdio>
#include <algorithm>
#include <climits>
#include <cmath>
#include <cstring>
#include <stdexcept>

#include "font.h"
#define SDF_WITH_ATLAS
#include "font_sdf.h"
#include "budget.h"
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
#define CLOUD_BASE 2.7f
#define CLOUD_TOP 5.6f
#define PI_F 3.14159265f

// Checkerboard frames: does whole-image pixel (X, Y) draw a sample in the
// frame of slot parity `k` (cP.parity holds k & 3)? Stride 2: half the pixels,
// alternating. Stride 4: one pixel of each 2 x 2 block, in the order
// top-left, bottom-right, top-right, bottom-left over four slots.
__device__ __forceinline__ bool draws_here(uint32_t X, uint32_t Y);

struct Globals {
  const V4* noise;
  const V4* trans;
  const V4* sky;
  cudaTextureObject_t cloud;
  cudaTextureObject_t macro;
  const float4* macro_arr;  // the same fields, for C1-smooth sampling in shading
  cudaTextureObject_t sdf;  // Inter's signed distance atlas (font_sdf.h)
};

__constant__ FrameParams cP;
__device__ float g_sunvis;      // fraction of the sun the camera sees this frame (glare)
__device__ unsigned int g_bad;  // samples that came out non-finite (counted, then dropped)
__constant__ Globals cG;
__constant__ V4 cSeg[FONT_MAX_SEGMENTS];
__constant__ short cGStart[128];
__constant__ short cGCount[128];
__constant__ SdfGlyph cSdf[95];

// ---------------------------------------------------------------- atmosphere

__device__ __forceinline__ V3 f3(float4 v) { return v3(v.x, v.y, v.z); }

__device__ __forceinline__ void densities(float h, float* r, float* m, float* o) {
  *r = __expf(-h * (1.0f / 8.0f));
  *m = __expf(-h * (1.0f / 1.2f));
  *o = fmaxf(0.0f, 1.0f - fabsf(h - 25.0f) * (1.0f / 15.0f));
}
#define RAY_B v3(5.802e-3f, 13.558e-3f, 33.1e-3f)
#define MIE_S 9.0e-3f   // aerosols: about twice a clear day, for aerial haze
#define MIE_E 10.0e-3f
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
__device__ __forceinline__ float sph_alt(V3 p) { return length(v3(p.x, p.y + PLANET_R, p.z)) - PLANET_R; }

__device__ V3 scatter(V3 o, V3 d, float tmax, int N, V3* trans_out) {
  V3 sun = cP.sun_dir;
  float c = dot(d, sun);
  float pr = phase_ray(c), pm = phase_mie(c);
  V3 T = v3(1.0f), L = v3(0.0f);
  // a camera above the atmosphere starts at the ray's entry into it
  float t0 = 0.0f;
  if (sph_alt(o) > 59.9f) {
    t0 = sphere_near(o, d, 59.9f);
    if (t0 < 0.0f || t0 >= tmax) {
      if (trans_out) *trans_out = T;
      return L;
    }
  }
  float span = tmax - t0;
  float ds;
  for (int k = 0; k < N; k++) {
    // quadratic spacing puts more steps near the eye
    float u0 = k / (float)N, u1 = (k + 1) / (float)N;
    float s0 = t0 + u0 * u0 * span, s1 = t0 + u1 * u1 * span;
    ds = s1 - s0;
    V3 p = o + d * (0.5f * (s0 + s1));
    float h = fmaxf(sph_alt(p), 0.0f);
    float r, m, oz;
    densities(h, &r, &m, &oz);
    V3 ext = RAY_B * r + v3(MIE_E * m) + OZO_A * oz;
    V3 stepT = v3(__expf(-ext.x * ds), __expf(-ext.y * ds), __expf(-ext.z * ds));
    float mu_s = dot(up_at(p), sun);
    V3 Ts = transmittance(fminf(h, 60.0f), mu_s);
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
  // the sky as seen from near the ground below the camera: the map lights the
  // terrain, clouds and water, which are down there even when the eye is not
  V3 o = cP.cam.pos;
  float a = alt_flat(o);
  if (a > 4.0f) o.y -= a - 4.0f;
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

// Coverage (where clouds form, fixed to the ground) at wp, and a warp for the
// volume lookup from the same noise's second octave (its value and slope): the volume
// tiles every CLOUD_TILE_KM, which reads as a lattice of identical puffs from
// high up, so its lookup is shifted by a field that changes over ~20 km.
__device__ __forceinline__ float cloud_cov(V2 wp, V2* warp) {
  V2 p = wp + v2(31.7f, 4.1f);
  V3 n1 = noised(p, cG.noise), n2 = noised(oct_step(p), cG.noise);
  *warp = v2(0.45f * n2.x, 0.11f * (n2.y - n2.z));
  float cov = (0.5f * n1.x + 0.25f * n2.x) * 0.5f + 0.5f;  // fbm(p, 2) * 0.5 + 0.5
  return smoothstepf(0.27f, 0.64f, cov) * 0.97f;
}

// The cloud body: two octaves of the volume, the second rotated and scaled by
// a non-integer factor so the two do not repeat together.
__device__ __forceinline__ float cloud_base(V3 p, float h, V2 w, V3* qo) {
  V3 q = v3((p.x + cP.wind.x) * (1.0f / CLOUD_TILE_KM) + w.x, h * (1.0f / 6.0f),
            (p.z + cP.wind.y) * (1.0f / CLOUD_TILE_KM) + w.y);
  *qo = q;
  V3 r = v3(0.8f * q.x - 0.6f * q.z, q.y * 2.0f, 0.6f * q.x + 0.8f * q.z);
  return 0.7f * cloud_tex(q) + 0.3f * cloud_tex(v3(r.x * 2.71f + 0.17f, r.y, r.z * 2.71f + 0.41f));
}

// Test for the coarse march: false only where cloud_density is zero (it skips
// the height profile and the detail erosion, which only remove density).
__device__ bool cloud_maybe(V3 p) {
  float h = alt_flat(p);
  float h01 = (h - CLOUD_BASE) * (1.0f / (CLOUD_TOP - CLOUD_BASE));
  if (h01 <= 0.0f || h01 >= 1.0f) return false;
  V2 w;
  float cov = cloud_cov(v2(p.x, p.z) * (1.0f / 42.0f), &w);
  if (cov <= 0.01f) return false;
  V3 q;
  return cloud_base(p, h, w, &q) > 1.0f - cov;
}

__device__ float cloud_density(V3 p, bool detail) {
  float h = alt_flat(p);
  float h01 = (h - CLOUD_BASE) * (1.0f / (CLOUD_TOP - CLOUD_BASE));
  if (h01 <= 0.0f || h01 >= 1.0f) return 0.0f;
  V2 w;
  float cov = cloud_cov(v2(p.x, p.z) * (1.0f / 42.0f), &w);  // where clouds form; fixed to the ground
  if (cov <= 0.01f) return 0.0f;
  // two octaves of the volume for the body: cells of ~3.5 and ~1.3 km
  V3 q;
  float base = cloud_base(p, h, w, &q);
  // coverage picks where the volume is cloud; the height profile then gives
  // flat bases and lets denser cores rise higher, so tops round into towers
  // instead of every cloud being cut into a lens by one height threshold
  float d0 = (base - (1.0f - cov)) / fmaxf(cov, 1e-3f);
  if (d0 <= 0.0f) return 0.0f;
  float d = d0 * smoothstepf(0.0f, 0.05f, h01) * smoothstepf(1.0f, 0.55f, h01 + 0.6f * (1.0f - sat(d0 * 1.6f)));
  if (d <= 0.0f) return 0.0f;
  if (detail) {
    // erode the edges with finer noise: billows at the top, wisps below
    V3 q2 = v3(q.x * 9.0f + 0.31f, q.y * 7.0f, q.z * 9.0f + 0.57f);
    V3 q3 = v3(q.x * 31.0f + 0.71f, q.y * 23.0f, q.z * 31.0f + 0.13f);
    float det = 0.65f * cloud_tex(q2) + 0.35f * cloud_tex(q3);
    float e = 0.65f * mixf(1.0f - det, det, sat(h01 * 2.5f)) * (1.0f - 0.35f * sat(d));
    d = (d - e) / (1.0f - e);
    if (d <= 0.0f) return 0.0f;
  }
  // The remap above rises slowly where coverage is high: a deep region of thin
  // cloud, which seen from inside or near the layer read as a uniform fog
  // bank (it took kilometres of 30 m steps to become opaque). Density
  // saturates at a third of it, so cloud bodies get surfaces and shading.
  return sat(d * 3.0f) * 60.0f;  // extinction per km
}

// Optical depth from p toward the sun through the cloud layer.
__device__ float cloud_light_od(V3 p, int n) {
  const float steps[6] = {0.08f, 0.18f, 0.4f, 0.9f, 0.7f, 1.2f};
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
  // A low sun crosses 20 km or more of the layer, and five samples that far
  // apart found a cloud on almost every path, shading whole plains at once.
  // The shadow is taken from the first 8 km of the crossing, sampled every
  // 1.6 km at most: the clouds nearest the ground cast it, with the gaps
  // between them left lit.
  t1 = fminf(t1, t0 + 8.0f);
  float od = 0.0f;
  const int N = 5;
  float ds = (t1 - t0) / N;
  for (int k = 0; k < N; k++) od += cloud_density(p + l * (t0 + (k + 0.5f) * ds), false) * ds;
  // a cumulus passes on 10 to 20% of the sunlight as diffuse light even where
  // no direct ray gets through
  return 0.14f + 0.86f * __expf(-od * 0.7f);
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
  float fade_start = 70.0f + 1.5f * fmaxf(ho - CLOUD_TOP, 0.0f);  // seen from high up, clouds are far but not grazing
  if (ta >= tb || ta > 160.0f + 2.0f * fmaxf(ho, 0.0f)) return out;
  // Two step sizes: coarse through empty air (the shape only), fine inside
  // clouds, 2% of the distance (120 to 800 m), so the
  // eroded edges are sampled at their own scale at any distance. A uniform
  // step over the whole segment (up to 1 km) averaged the edge detail away,
  // which made distant clouds smooth gray lenses.
  float seg = tb - ta;
  float ds_c = fmaxf(seg / N, 0.15f);
  float c = dot(d, cP.sun_dir);
  V3 amb_top = sky_lookup(v3(0, 1, 0)) * 1.1f;
  V3 amb_bot = sky_lookup(normalize(v3(0.3f, 0.15f, 0.2f))) * 0.5f;
  float wsum = 0.0f, dsum = 0.0f;
  float s = ta + jitter * ds_c;
  bool fine = false;
  int empty = 0, lit_age = 0;
  float od = 0.0f;
  const int max_fine = cheap ? 40 : 64;  // fine steps; coarse steps are bounded by the segment
  int nfine = 0;
  for (int k = 0; k < N + max_fine + 2 && s < tb && nfine < max_fine; k++) {
    V3 p = o + d * s;
    if (!fine) {
      if (!cloud_maybe(p)) { s += ds_c; continue; }
      // entered a cloud: back up one coarse step and walk in finely
      s = fmaxf(ta, s - ds_c);
      fine = true;
      empty = 0;
      continue;
    }
    float ds = cheap ? fmaxf(0.06f, 0.012f * s) : cP.hq ? clampf(0.015f * s, 0.12f, 0.6f) : clampf(0.02f * s, 0.12f, 0.8f);
    nfine++;
    float sig = cloud_density(p, !cheap && out.T > 0.15f);
    if (s > fade_start) sig *= __expf(-(s - fade_start) * (1.0f / 40.0f));
    if (sig <= 0.0f) {
      if (++empty > 4) fine = false;  // left the cloud
      s += ds;
      continue;
    }
    empty = 0;
    // the light march is the expensive part; inside a cloud it changes
    // slowly, so take it on every fourth fine step
    if (lit_age++ % (cP.hq ? 2 : 4) == 0) od = cloud_light_od(p, cP.hq ? 4 : 3);
    // several orders of scattering, each softer and less directional
    float S = 0.0f, a = 1.0f, b = 1.0f, g = 1.0f;
    for (int o2 = 0; o2 < 3; o2++) {
      float ph = mixf(phase_hg(c, 0.85f * g), phase_hg(c, -0.2f * g), 0.4f);
      S += a * __expf(-od * b) * ph;
      a *= 0.55f; b *= 0.3f; g *= 0.5f;
    }
    float h01 = sat((alt_flat(p) - CLOUD_BASE) / (CLOUD_TOP - CLOUD_BASE));
    // light reaching the inside is softer near the edges: "powder"
    float powder = 1.0f - 0.6f * __expf(-sig * 0.25f);
    V3 Lin = sun_at(p) * (S * 4.0f * PI_F * 0.55f * powder) + mix(amb_bot, amb_top, h01) * (0.2f + 0.8f * h01);
    float st = __expf(-sig * ds);
    V3 Sint = Lin * (1.0f - st);
    out.L += Sint * out.T;
    wsum += out.T * (1.0f - st);
    dsum += out.T * (1.0f - st) * s;
    out.T *= st;
    // nearly opaque: the rest of the cloud adds what this step's light would,
    // and the march ends (cheaper than walking on to T < 0.03)
    if (out.T < (cP.hq ? 0.01f : 0.1f)) {
      out.L += Lin * out.T;
      wsum += out.T;
      dsum += out.T * s;
      out.T = 0.0f;
      break;
    }
    s += ds;
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

// The baked fields through a quadratic B-spline (9 texels). Hardware bilinear
// filtering is only C0 and steps its weights in 1/256 of a texel, which shows
// in shading normals as streaks; marching does not see that, shading does.
__device__ Macro macro_smooth(V2 p) {
  float x = (p.x * (1.0f / MACRO_KM) + 0.5f) * MACRO_N - 0.5f;
  float y = (p.y * (1.0f / MACRO_KM) + 0.5f) * MACRO_N - 0.5f;
  if (!(x > 2.0f && y > 2.0f && x < MACRO_N - 3.0f && y < MACRO_N - 3.0f)) return macro_direct(p, cG.noise);
  int ix = (int)floorf(x + 0.5f), iy = (int)floorf(y + 0.5f);
  float fx = x - ix, fy = y - iy;
  float wx[3] = {0.5f * (0.5f - fx) * (0.5f - fx), 0.75f - fx * fx, 0.5f * (0.5f + fx) * (0.5f + fx)};
  float wy[3] = {0.5f * (0.5f - fy) * (0.5f - fy), 0.75f - fy * fy, 0.5f * (0.5f + fy) * (0.5f + fy)};
  float4 a = make_float4(0, 0, 0, 0);
  for (int j = 0; j < 3; j++)
    for (int i = 0; i < 3; i++) {
      float4 v = __ldg(cG.macro_arr + (iy - 1 + j) * MACRO_N + (ix - 1 + i));
      float w = wx[i] * wy[j];
      a.x += v.x * w; a.y += v.y * w; a.z += v.z * w; a.w += v.w * w;
    }
  Macro r;
  r.land = a.x; r.massif = a.y; r.w = v2(a.z, a.w);
  return r;
}
__device__ __forceinline__ float terrain_s(V2 p, int oct) { return terrain_from(p, macro_smooth(p), oct, cG.noise); }
// Terrain, or the water over it (the sea at 0, lakes in basins).
__device__ __forceinline__ float ground_h(V2 p, int oct) {
  float w;
  float g = terrain_from(p, macro_at(p), oct, cG.noise, &w);
  return fmaxf(g, w);
}

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
__device__ float ring_cover(V3 o, V3 d);  // the orbital ring's opacity along a ray (below)

__device__ float terrain_shadow(V3 p, V3 l) {
  float t = 0.004f;
  const int n = cP.hq ? 56 : 32;  // the high-quality path follows long low-sun shadows further
  for (int i = 0; i < n; i++) {
    V3 q = p + l * t;
    float a = alt_flat(q);
    if (a > TERRAIN_MAX) break;
    float dh = a - ground_h(v2(q.x, q.z), t < 0.6f ? 7 : 5);
    if (dh < 0.0f) return 0.0f;
    t += fmaxf(dh * 0.55f, 0.004f + 0.006f * t);
    if (t > (cP.hq ? 60.0f : 40.0f)) break;
  }
  return 1.0f;
}

__device__ V3 terrain_normal(V3 p, float eps, int oct) {
  V2 q = v2(p.x, p.z);
  float hx0 = terrain_s(q - v2(eps, 0), oct), hx1 = terrain_s(q + v2(eps, 0), oct);
  float hz0 = terrain_s(q - v2(0, eps), oct), hz1 = terrain_s(q + v2(0, eps), oct);
  return normalize(v3(hx0 - hx1, 2.0f * eps, hz0 - hz1));
}

struct Material {
  V3 albedo;
  float rough;
  float rock;  // how much of the surface is bare rock (for detail normals)
};

__device__ Material terrain_material(V3 p, float h, V3 n) {
  V2 q = v2(p.x, p.z);
  float v = fbm(q * 2.3f + v2(3.1f, 1.7f), 3, cG.noise);
  float v2_ = noise(q * 7.0f, cG.noise);
  float slope = n.y;
  // rock: warm sandstone low down, cool granite high up, in strata
  float strata = 0.86f + 0.14f * sinf(h * 55.0f + v * 4.0f + 2.0f * v2_);
  V3 rock_lo = v3(0.42f, 0.33f, 0.24f), rock_hi = v3(0.40f, 0.40f, 0.41f);
  V3 rock = mix(rock_lo, rock_hi, smoothstepf(0.6f, 1.6f, h + 0.3f * v)) * strata;
  V3 scree = v3(0.47f, 0.45f, 0.42f);
  rock = mix(rock, scree, smoothstepf(0.45f, 0.62f, slope) * (1.0f - smoothstepf(0.62f, 0.78f, slope)) * 0.7f);
  // vegetation: lowland grass, dark forest, bright alpine meadow, autumn heath
  V3 grass = mix(v3(0.10f, 0.17f, 0.045f), v3(0.15f, 0.21f, 0.06f), sat(v * 1.2f + 0.5f));
  V3 meadow = v3(0.21f, 0.25f, 0.075f);
  V3 heath = v3(0.30f, 0.20f, 0.09f);
  V3 veg = mix(grass, meadow, smoothstepf(0.5f, 1.1f, h + 0.2f * v));
  veg = mix(veg, heath, smoothstepf(0.15f, 0.45f, v + 0.25f * v2_) * smoothstepf(0.7f, 1.3f, h));
  float fo = fbm(q * 1.1f + v2(7.7f, 3.3f), 4, cG.noise) + 0.25f * v2_;
  // forests: soft-edged stands on gentle lower slopes, mottled by the canopy
  // (a flat near-black forest read as holes in the ground)
  float wf = smoothstepf(-0.05f, 0.35f, fo) * smoothstepf(0.84f, 0.92f, slope) *
             (1.0f - smoothstepf(0.6f, 1.0f, h + 0.3f * v)) * smoothstepf(0.02f, 0.06f, h);
  float canopy = 0.75f + 0.5f * noise(q * 160.0f, cG.noise);
  veg = mix(veg, v3(0.085f, 0.125f, 0.05f) * canopy, wf * 0.85f);
  float wg = smoothstepf(0.76f, 0.86f, slope + 0.05f * v2_) * (1.0f - smoothstepf(1.45f, 1.85f, h + v * 0.3f));
  V3 a = mix(rock, veg, wg);
  V3 sand = v3(0.62f, 0.55f, 0.42f);
  float ws = smoothstepf(0.014f, 0.003f, h + 0.004f * v2_) * smoothstepf(0.55f, 0.8f, slope);
  a = mix(a, sand, ws);
  V3 snow = v3(0.95f, 0.96f, 1.0f);
  float wsn = smoothstepf(1.45f, 1.75f, h + v * 0.4f + 0.45f * (slope - 0.7f)) * smoothstepf(0.6f, 0.74f, slope + 0.05f * v2_);
  a = mix(a, snow, wsn);
  // fine albedo variation (tens of metres)
  float fd = fbm(q * 23.0f + v2(1.3f, 8.1f), 4, cG.noise);
  a = a * (1.0f + 0.35f * fd * (1.0f - wsn));
  Material m;
  m.albedo = a;
  m.rough = mixf(mixf(0.9f, 0.65f, 1.0f - wg), 0.3f, wsn);
  m.rock = (1.0f - wg) * (1.0f - wsn) * (1.0f - ws);
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
  V3 hv = v + l;
  float hl = dot(hv, hv);
  if (hl < 1e-12f) return 0.0f;  // looking straight at the sun past the point: no half vector
  V3 h = hv * rsqrtf(hl);
  float nh = fmaxf(dot(n, h), 0.0f), nl = fmaxf(dot(n, l), 0.0f), nv = fmaxf(dot(n, v), 1e-3f);
  float a = rough * rough, a2 = a * a;
  float dd = nh * nh * (a2 - 1.0f) + 1.0f;
  float D = a2 / (PI_F * dd * dd);
  float k = a * 0.5f;
  float G = (nl / (nl * (1 - k) + k)) * (nv / (nv * (1 - k) + k));
  // clamped: with the sun straight behind the eye dot(h, v) rounds to just over
  // 1, and the fast-math powf of a negative base is NaN
  float F = f0 + (1.0f - f0) * powf(sat(1.0f - dot(h, v)), 5.0f);
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
  float h = terrain_s(v2(p.x, p.z), 8);
  float eps = fmaxf(0.0012f, t * pix);
  int oct = (int)clampf(floorf(log2f(2.0f / eps)) + 1.0f, 5.0f, 12.0f);
  V3 n = terrain_normal(p, eps, oct);
  Material m = terrain_material(p, h, n);
  // Rock detail the heightfield cannot carry: gullies running down the slope
  // and fractured faces, as a normal perturbation, faded out where a pixel
  // covers more than the detail.
  if (m.rock > 0.05f && eps < 0.03f) {
    V2 q = v2(p.x, p.z);
    V3 g1 = noised(q * 45.0f + v2(2.1f, 7.3f), cG.noise);
    V3 g2 = noised(oct_step(q * 45.0f) * 2.3f, cG.noise);
    float k = m.rock * 0.07f * sat(1.0f - eps / 0.03f);
    // ridged: |noise| creases make gullies; the tilt is bounded so no facet
    // turns away from the light by more than the slope of a gully
    float s1 = g1.x < 0.0f ? -1.0f : 1.0f, s2 = g2.x < 0.0f ? -1.0f : 1.0f;
    V2 grad = v2(g1.y * s1 + 0.5f * g2.y * s2, g1.z * s1 + 0.5f * g2.z * s2);
    float gl = length(grad);
    if (gl > 3.0f) grad = grad * (3.0f / gl);
    n = normalize(n + v3(-grad.x, 0.0f, -grad.y) * k);
  }
  V3 v = -rd;
  V3 L = v3(0.0f);
  // sun
  V3 l = sun_sample(rng.next(), rng.next());
  float nl = dot(n, l);
  V3 Es = sun_at(p);
  if (nl > 0.0f && Es.x + Es.y + Es.z > 1e-4f) {
    float vis = full ? terrain_shadow(p + n * 0.003f, l) : 1.0f;
    if (vis > 0.0f) vis *= cloud_shadow(p, l) * (1.0f - ring_cover(p, l));
    float spec = ggx_spec(n, v, l, m.rough, 0.03f);
    L += Es * (vis * (nl / PI_F)) * m.albedo + Es * (vis * spec);
  }
  // one bounce: sky or lit ground
  V3 bd = cosine_dir(n, rng.next(), rng.next());
  V3 Lin;
  // the bounce ray starts with the octaves the surface was found with (a
  // coarser surface lies above it in places, and every bounce from there hit
  // at once: black patches); farther out march_oct lowers them
  float th = !full ? -1.0f
             : cP.hq ? march_terrain(p + n * 0.004f, bd, 0.0f, 8.0f, 24, 9, 0.004f)
                     : march_terrain(p + n * 0.004f, bd, 0.0f, 6.0f, 14, 9, 0.004f);
  if (th < 0.0f) {
    Lin = sky_lookup(bd);
  } else {
    V3 q = p + n * 0.004f + bd * th;
    float h2 = terrain(v2(q.x, q.z), 6);
    V3 n2 = terrain_normal(q, 0.01f, 6);
    Material m2 = terrain_material(q, h2, n2);
    if (cP.hq) {
      // the high-quality path: the bounce point's sunlight is shadowed, and
      // its own light comes from a second bounce (sky, or the ground's
      // albedo under the sky)
      float vis2 = terrain_shadow(q + n2 * 0.003f, cP.sun_dir);
      V3 bd2 = cosine_dir(n2, rng.next(), rng.next());
      float th2 = march_terrain(q + n2 * 0.004f, bd2, 0.0f, 6.0f, 14, 7, 0.004f);
      V3 L2 = th2 < 0.0f ? sky_lookup(bd2) : m2.albedo * sky_lookup(v3(0, 1, 0)) * 0.6f;
      Lin = m2.albedo * (sun_at(q) * (vis2 * fmaxf(dot(n2, cP.sun_dir), 0.0f) / PI_F) + L2);
    } else {
      Lin = m2.albedo * (sun_at(q) * (fmaxf(dot(n2, cP.sun_dir), 0.0f) * 0.75f / PI_F) + sky_lookup(v3(0, 1, 0)) * 0.6f);
    }
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

// The orbital ring: a band 120 km wide on a cylinder of radius R + 420 km
// around an axis through the planet's centre, its inner face toward the
// planet. ring_hit gives the distance to the band along o + d*s, or -1.
#define RING_R (PLANET_R + 420.0f)
#define RING_W 120.0f
__device__ __forceinline__ V3 ring_axis() { return normalize(v3(0.25f, 0.095f, 1.0f)); }  // arcs about 28 degrees up over the south
// In float: the stable form of the quadratic keeps the roots to about 1e-7
// relative (half a metre at the ring) with no cancellation, and the T4 runs
// double precision at 1/32 rate; sky rays from every bounce come through here.
__device__ float ring_hit(V3 o, V3 d) {
  V3 a = ring_axis();
  V3 c = v3(o.x, o.y + PLANET_R, o.z);  // relative to the planet's centre
  float oa = dot(c, a), da = dot(d, a);
  V3 pp = c - a * oa, qq = d - a * da;  // perpendicular to the axis
  float A = dot(qq, qq), B = 2.0f * dot(pp, qq);
  float Cc = (dot(pp, pp) - RING_R * RING_R);
  float disc = B * B - 4.0f * A * Cc;
  if (A < 1e-12f || disc < 0.0f) return -1.0f;
  float qv = -0.5f * (B + copysignf(sqrtf(disc), B));
  float t0 = qv / A, t1 = Cc / qv;
  float ts[2] = {fminf(t0, t1), fmaxf(t0, t1)};
  for (int i = 0; i < 2; i++) {
    float t = ts[i];
    if (t <= 0.0f) continue;
    if (fabsf(oa + t * da) <= RING_W * 0.5f) return t;
  }
  return -1.0f;
}

// Where a point of the band lies on it: u across the band (0 at one rail, 1
// at the other) and phi around it (radians).
__device__ __forceinline__ V2 ring_uv(V3 c, V3 a) {
  float h = dot(c, a);
  V3 radial = normalize(c - a * h);
  V3 b = normalize(cross(a, radial));
  V3 e = normalize(cross(b, a));
  return v2(h / RING_W + 0.5f, atan2f(dot(c, b), dot(c, e)));
}

// The band's structure across its width: two rails, lattice lanes at the
// edges, two open gaps crossed by trusses, solar lanes and plated lanes.
// Returns the opacity; *kind is 0 plate, 1 solar, 2 rail, 3 lattice.
__device__ float ring_alpha(V2 uv, int* kind) {
  float u = uv.x;
  float spoke = fractf(uv.y * (180.0f / PI_F) * 2.0f);  // trusses every half degree
  bool truss = spoke < 0.06f;
  *kind = 0;
  if (u < 0.02f || u > 0.98f) { *kind = 2; return 1.0f; }
  if (u < 0.12f || u > 0.88f) {  // lattice: a grid of beams with sky between them
    *kind = 3;
    float g = fractf(uv.y * (180.0f / PI_F) * 20.0f), k = fractf(u * 50.0f);
    return (g < 0.18f || k < 0.18f || truss) ? 0.9f : 0.12f;
  }
  if ((u > 0.19f && u < 0.23f) || (u > 0.71f && u < 0.75f)) return truss ? 0.9f : 0.0f;  // open gaps
  if ((u > 0.30f && u < 0.47f) || (u > 0.53f && u < 0.64f)) { *kind = 1; return 1.0f; }
  return 1.0f;
}

// Opacity of the band along o + d*s (0 where nothing is hit).
__device__ float ring_cover(V3 o, V3 d) {
  float t = ring_hit(o, d);
  if (t <= 0.0f) return 0.0f;
  V3 p = o + d * t;
  int k;
  return ring_alpha(ring_uv(v3(p.x, p.y + PLANET_R, p.z), ring_axis()), &k);
}

__device__ V3 ring_shade(V3 o, V3 d, float t, float* alpha) {
  V3 a = ring_axis();
  V3 p = o + d * t;
  V3 c = v3(p.x, p.y + PLANET_R, p.z);
  V2 uv = ring_uv(c, a);
  int kind;
  *alpha = ring_alpha(uv, &kind);
  V3 radial = normalize(c - a * dot(c, a));
  V3 n = dot(radial, d) > 0.0f ? -radial : radial;  // the face toward the viewer
  // plates: seams every 0.02 degrees around and five across each lane; each
  // plate a slightly different shade
  float pa = uv.y * 2865.0f;
  float panel = fractf(pa), lane = fractf(uv.x * 25.0f);
  float seam = smoothstepf(0.0f, 0.05f, panel) * smoothstepf(1.0f, 0.95f, panel) * smoothstepf(0.0f, 0.1f, lane) *
               smoothstepf(1.0f, 0.9f, lane);
  float shade = 0.85f + 0.3f * u01(hash_u32((uint32_t)(int)floorf(pa) * 31u + (uint32_t)(int)floorf(uv.x * 25.0f)));
  V3 alb;
  float rough, f0;
  if (kind == 1) { alb = v3(0.05f, 0.07f, 0.13f); rough = 0.12f; f0 = 0.06f; }        // solar lanes
  else if (kind == 2) { alb = v3(0.85f, 0.83f, 0.8f); rough = 0.25f; f0 = 0.5f; }   // rails
  else if (kind == 3) { alb = v3(0.45f, 0.44f, 0.43f); rough = 0.5f; f0 = 0.04f; }  // lattice
  else { alb = v3(0.72f, 0.71f, 0.7f) * shade; rough = 0.45f; f0 = 0.04f; }         // plates
  alb = alb * (0.6f + 0.4f * seam);
  // in the planet's shadow?
  V3 l = cP.sun_dir;
  float bb = dot(c, l), cc = dot(c, c) - PLANET_R * PLANET_R;
  bool lit = !(bb < 0.0f && bb * bb - cc > 0.0f);
  float nl = fmaxf(dot(n, l), 0.0f);
  V3 v = -d;
  V3 col = lit ? cP.sun_e * (alb * (nl / PI_F) + v3(ggx_spec(n, v, l, rough, f0))) : v3(0.0f);
  // lights along the seams, brightest on its dark side
  float lights = (1.0f - seam) * (kind == 0 ? 1.0f : 0.3f) * (lit ? 0.12f : 1.0f);
  col += v3(1.0f, 0.75f, 0.45f) * (0.8f * lights);
  // light from the planet's day side below it: the inner face sees a
  // hemisphere of sunlit ground and cloud (albedo about 0.3)
  col += alb * cP.sun_e * (0.12f * sat(cP.sun_dir.y * 4.0f + 0.3f));
  return col;
}

__device__ V3 sky_ray(V3 o, V3 d, bool with_sun) {
  float tt = sphere_far(o, d, 60.0f);
  V3 T;
  int n = sph_alt(o) > 15.0f ? 40 : 24;  // the limb seen from high up is a long path
  V3 L = scatter(o, d, fmaxf(tt, 1.0f), n, &T);
  V3 behind = stars(d);
  if (with_sun) behind += sun_disc(d);
  float tr = ring_hit(o, d);
  if (tr > 0.0f) {  // the ring, over the sun and stars where it is opaque
    float al;
    V3 rc = ring_shade(o, d, tr, &al);
    behind = rc * al + behind * (1.0f - al);
  }
  return L + behind * T;
}

// Water at p (on the sea, or on a lake at `level`).
__device__ V3 shade_water(V3 p, V3 rd, float t, float level, Rng& rng, bool full) {
  V2 q = v2(p.x, p.z) * 70.0f;
  float wt = cP.wave_t;
  V3 n1 = noised(q + v2(wt, 0.35f * wt), cG.noise);
  V3 n2 = noised(oct_step(q) * 1.3f + v2(-0.6f * wt, wt), cG.noise);
  V3 n3 = noised(q * 0.21f + v2(0.2f * wt, -0.1f * wt), cG.noise);
  V2 g = v2(n1.y + n2.y * 0.7f + n3.y * 1.5f, n1.z + n2.z * 0.7f + n3.z * 1.5f);
  // gusts: patches a few hundred metres across where the wind roughens the
  // water and calm ones that mirror, which still read from kilometres away
  float gust = smoothstepf(-0.2f, 0.5f, noise(v2(p.x, p.z) * 2.2f + v2(0.15f * wt, 0.05f * wt), cG.noise));
  float s = (0.025f + 0.06f * gust) / (1.0f + t * 0.35f);
  V3 n = normalize(v3(-g.x * s, 1.0f, -g.y * s));
  // ripples smaller than the pixel blur the reflection instead: tilt the
  // normal by a random amount that grows with distance and gusts
  {
    float rough = (0.004f + 0.01f * gust) * fminf(t, 20.0f);
    float u1 = rng.next(), u2 = rng.next();
    float r = rough * sqrtf(u1), ang = 6.2831853f * u2;
    n = normalize(n + v3(r * cosf(ang), 0.0f, r * sinf(ang)));
  }
  V3 v = -rd;
  float cv = fmaxf(dot(n, v), 0.0f);
  float F = 0.02f + 0.98f * powf(sat(1.0f - cv), 5.0f);
  V3 r = reflect(rd, n);
  if (r.y < 0.004f) r = normalize(v3(r.x, 0.004f, r.z));
  // reflection
  V3 R;
  float tr = full ? march_terrain(p + v3(0, 0.0005f, 0), r, 0.0f, 80.0f, 56, 6, 0.002f) : -1.0f;
  if (tr >= 0.0f) {
    // the reflected slope: its material in sun and sky light, without shadows
    V3 qh = p + r * tr;
    float h2 = terrain(v2(qh.x, qh.z), 6);
    V3 n2 = terrain_normal(qh, fmaxf(0.004f, tr * 0.002f), 6);
    Material m2 = terrain_material(qh, h2, n2);
    V3 Ls = sun_at(qh) * (fmaxf(dot(n2, cP.sun_dir), 0.0f) / PI_F) + sky_lookup(n2.y > 0.0f ? n2 : v3(0, 1, 0));
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
  float vis = (full && l.y > 0.0f) ? terrain_shadow(p + v3(0, 0.002f, 0), l) * cloud_shadow(p, l) * (1.0f - ring_cover(p, l)) : 1.0f;
  V3 spec = sun_at(p) * (ggx_spec(n, v, l, fminf(0.07f + 0.004f * t, 0.3f), 0.02f) * vis);
  // body of the water: seabed seen through absorbing water
  float depth_m = fmaxf(level - terrain(v2(p.x, p.z), 7), 0.0f) * 1000.0f;
  V3 sigma = v3(0.32f, 0.055f, 0.028f);  // per metre
  V3 path = vexp(sigma * (-2.2f * fminf(depth_m, 300.0f)));
  V3 Ein = sun_at(p) * (fmaxf(cP.sun_dir.y, 0.0f) / PI_F) + sky_lookup(v3(0, 1, 0)) * 0.8f;
  V3 bed = v3(0.55f, 0.5f, 0.38f) * Ein;
  V3 deep = v3(0.004f, 0.020f, 0.032f) * Ein * 3.0f;
  V3 body = bed * path + deep * (v3(1.0f) - path);
  // foam on the shore: surf on the sea, a thin bright line on lakes; the band
  // widens with distance so the shore still shows as a line a pixel or so wide
  float band = 1.6f + 2.2f * t;  // metres of depth
  float foam = smoothstepf(band, 0.0f, depth_m) * sat(noise(v2(p.x, p.z) * 520.0f + v2(wt * 3.0f, 0.0f), cG.noise) * 1.5f + 0.4f);
  if (level > 0.0f) foam *= 0.45f;
  body = mix(body, Ein * 0.75f, foam);
  return body * (1.0f - F) + R * F + spec;
}

struct Hit {
  V3 L;
  float dist;  // primary distance, 1e9 for sky
  float id;    // 0 sky, 1 land, 2 water (what lies behind any cloud)
};

// One thread: how much of the sun the camera sees (terrain, then clouds).
__global__ void k_sunvis() {
  V3 o = cP.cam.pos;
  float v = cP.sun_dir.y > -0.1f ? terrain_shadow(o, cP.sun_dir) : 0.0f;
  if (v > 0.0f) v *= cloud_shadow(o, cP.sun_dir) * (1.0f - ring_cover(o, cP.sun_dir));
  g_sunvis = v;
}

__device__ Hit trace(V3 o, V3 d, float pix, Rng& rng) {
  Hit hres;
  // far enough to reach the horizon from any height (from 100 km it is 1,130 km away)
  float hz = sqrtf(2.0f * PLANET_R * fmaxf(alt_flat(o), 0.0f)) + 300.0f;
  // below 10 km the horizon is inside 360 km and haze hides what lies past it;
  // from higher up the march reaches the horizon, with more steps
  bool high = hz > 360.0f + 300.0f;
  float th = march_terrain(o, d, 0.0f, high ? hz : 360.0f, high ? 360 : 260, 9, fmaxf(0.6f * pix, 0.0002f));
  V3 L;
  if (th >= 0.0f) {
    V3 p = o + d * th;
    float wl;
    float h = terrain_from(v2(p.x, p.z), macro_at(v2(p.x, p.z)), 8, cG.noise, &wl);
    if (h <= wl) {
      L = shade_water(p, d, th, wl, rng, true);
      hres.id = 2.0f;
    } else {
      L = shade_terrain(p, d, th, pix, rng, true);
      hres.id = 1.0f;
    }
    V3 T;
    V3 ins = scatter(o, d, th, cP.hq ? 12 : 8, &T);
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
  // Where the cloud is most of what the pixel shows, its distance is the one
  // that moves with the camera: reprojecting a near cloud through the terrain
  // behind it (or through the sky, at infinity) smeared it across the window.
  // The hit kind stays that of what lies behind it. Over land and water, where
  // the depth also decides which texels count, only a nearly opaque cloud
  // takes over: a thin one would split the ground into islands.
  if (c.T < (hres.id == 0.0f ? 0.5f : 0.2f)) hres.dist = c.depth;
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

// Distance from p (glyph units, z out of the text plane) to the strokes of
// glyphs gi-1..gi+1 of a text, as round tubes along each segment. Also returns
// the nearest point on the stroke centre lines (for the normal).
__device__ float text_sdf(const GText& g, V3 p, V3* nearest) {
  int gi = (int)floorf(p.x / GLYPH_ADVANCE);
  float best = 1e9f;
  V3 bc = v3(0.0f);
  for (int k = gi - 1; k <= gi + 1; k++) {
    if (k < 0 || k >= g.len) continue;
    unsigned char ch = g.ch[k];
    if (ch >= 128) continue;
    int s0 = cGStart[ch], n = cGCount[ch];
    V2 q = v2(p.x - k * GLYPH_ADVANCE, p.y);
    for (int m = 0; m < n; m++) {
      V4 sg = cSeg[s0 + m];
      V2 a = v2(sg.x, sg.y), b = v2(sg.z, sg.w);
      V2 pa = q - a, ba = b - a;
      float h = sat(dot(pa, ba) / fmaxf(dot(ba, ba), 1e-6f));
      V2 c = a + ba * h;
      V2 dd = q - c;
      float d2 = dot(dd, dd) + p.z * p.z;
      if (d2 < best) { best = d2; bc = v3(c.x + k * GLYPH_ADVANCE, c.y, 0.0f); }
    }
  }
  *nearest = bc;
  return sqrtf(best);
}

#define TEXT_TUBE 0.46f  // stroke radius, glyph units (words)
#define CLOCK_TUBE 0.72f  // the clock's heavier strokes


// ---------------------------------------------------------------- SDF type

// Per text, once per frame (k_textlight): the haze between the eye and the
// text's middle (transmittance and in-scattered light) and, for a sign, the
// sun reaching it. A text is small and at one depth, so one value serves all
// its pixels, and every tile computes the same.
__device__ V3 g_ttr[MAX_TEXTS], g_tins[MAX_TEXTS], g_tsun[MAX_TEXTS];

__device__ __forceinline__ V3 text_mid(const GText& g) {
  return g.center + g.right * (0.5f * g.width * g.height) + g.up * (0.36f * g.height);
}

// Signed distance (em, positive outside) from (x, y) on a text's baseline
// frame to the nearest glyph of the text, from the atlas; SDF_SPREAD where no
// glyph cell reaches.
__device__ float sdf_dist(const GText& g, float x, float y) {
  float best = SDF_SPREAD;
  for (int i = 0; i < g.len; i++) {
    unsigned char c = g.ch[i];
    if (c < 33 || c > 126) continue;
    const SdfGlyph& gl = cSdf[c - 32];
    float cx = (x - g.gx[i]) * SDF_PX_PER_EM - gl.ox;  // texels from the cell's left edge
    float cy = -y * SDF_PX_PER_EM - gl.oy;
    if (cx < 0.0f || cy < 0.0f || cx > gl.w || cy > gl.h) continue;
    float v = tex2D<float>(cG.sdf, gl.ax + cx, gl.ay + cy);
    best = fminf(best, (v * 255.0f - 128.0f) * (SDF_SPREAD / 127.0f));
  }
  return best;
}

// A ray against a text's plane: the distance along the ray and the point in
// em on the baseline frame. False when the ray runs along it or away.
__device__ bool text_plane(const GText& g, V3 o, V3 d, float* tp, float* x, float* y) {
  V3 n = cross(g.right, g.up);
  float den = dot(d, n);
  if (fabsf(den) < 1e-7f) return false;
  float t = dot(g.center - o, n) / den;
  if (t <= 0.0f) return false;
  V3 q = o + d * t - g.center;
  *tp = t;
  *x = dot(q, g.right) / g.height;
  *y = dot(q, g.up) / g.height;
  return true;
}

// Emissive type (text styles lockup, skywriting and inworld: the clock and the
// lockup's message): warm light in a plane at depth, attenuated and scattered
// by the haze in front of it.
__device__ V3 draw_type(const GText& g, int gi, V3 o, V3 d, V3 base, float pix, float depth) {
  float tp, x, y;
  if (!text_plane(g, o, d, &tp, &x, &y)) return base;
  if (x < -0.6f || x > g.width + 0.6f || y < -0.6f || y > 1.4f) return base;
  if (g.occlude && depth < tp) return base;
  float dist = sdf_dist(g, x, y);
  if (dist >= SDF_SPREAD - 1e-3f) return base;
  float fw = fmaxf(pix * tp / g.height, 1e-5f);  // a pixel in em
  float cov = sat(0.5f - dist / fw);
  V3 tr = g_ttr[gi], ins = g_tins[gi];
  float haze = 1.0f - (tr.x + tr.y + tr.z) * (1.0f / 3.0f);
  // a soft, wide shade behind the letters keeps them readable over bright
  // cloud, and the type's own light scattered by the haze around them
  float out = fmaxf(dist, 0.0f);
  base = base * (1.0f - 0.22f * g.alpha * __expf(-out * (1.0f / 0.16f)));
  float glow = __expf(-out * (1.0f / 0.05f)) * (0.03f + 0.6f * haze);
  V3 L = ins + g.color * tr;
  base = mix(base, L, cov * g.alpha);
  return base + g.color * tr * (glow * g.alpha * (1.0f - cov));
}

#define SMOKE_WRITE 2.2f   // em written per second
#define SMOKE_STEPS 24
// how thick smoke laid `a` seconds ago has spread (em); it stops at a third of
// the distance the atlas reaches past a letter, so the letters stay letters
__device__ __forceinline__ float smoke_thickness(float a) { return fminf(0.04f + 0.0015f * a, 0.09f); }

// Skywriting: the letters as smoke, written left to right at SMOKE_WRITE em/s.
// Smoke laid down `a` seconds ago has spread to a thickness of 0.07 + 0.01 a em
// (keeping its mass, so it thins), drifted with the wind, and frayed. Lit by
// the sun (forward-scattering) and the sky, behind the haze in front of it.
__device__ V3 draw_smoke(const GText& g, int gi, V3 o, V3 d, V3 base, float depth) {
  V3 n = cross(g.right, g.up);
  float em = g.height;
  V3 oc = o - g.center;
  V3 ol = v3(dot(oc, g.right), dot(oc, g.up), dot(oc, n)) / em;
  V3 dl = v3(dot(d, g.right), dot(d, g.up), dot(d, n)) / em;  // per km of ray
  if (fabsf(dl.z) < 1e-7f) return base;
  float smax = smoke_thickness(g.age + 1.0f);
  float Z = 2.5f * smax + 0.3f;
  float t0 = (-Z - ol.z) / dl.z, t1 = (Z - ol.z) / dl.z;
  if (t0 > t1) { float tt = t0; t0 = t1; t1 = tt; }
  t0 = fmaxf(t0, 0.0f);
  if (t1 <= t0) return base;
  V3 pm = ol + dl * (0.5f * (t0 + t1));
  float drift = 0.006f * g.age;
  if (pm.x < -1.5f || pm.x > g.width + 1.5f + drift || pm.y < -1.5f - Z || pm.y > 2.0f + Z) return base;
  if (g.occlude && depth < t0) return base;
  float dt = (t1 - t0) / SMOKE_STEPS;
  float T = 1.0f;
  V3 Ls = v3(0.0f);
  float cth = dot(d, cP.sun_dir);
  float ph = phase_hg(cth, 0.55f) * 0.7f + 0.3f / (4.0f * PI_F);  // forward-scattering, per steradian
  V3 Esun = g_tsun[gi] * 3.0f;  // single scattering, tripled for the light scattered more than once
  V3 amb = sky_lookup(v3(0, 1, 0)) * 1.0f + sky_lookup(-d) * 0.4f;
  for (int i = 0; i < SMOKE_STEPS; i++) {
    float t = t0 + (i + 0.5f) * dt;
    if (t > depth) break;
    V3 q = ol + dl * t;
    float al = g.age - q.x / SMOKE_WRITE;  // how long ago the smoke here was laid
    if (al < 0.0f) continue;
    float s = smoke_thickness(al);
    // drift with the wind, and fray: displaced by noise that grows with age
    V3 nq = v3(q.x * 0.11f + 0.37f, q.y * 0.11f + 0.71f, q.z * 0.11f + 0.13f + 0.002f * g.age);
    float n1 = cloud_tex(nq), n2 = cloud_tex(v3(nq.z + 0.5f, nq.x + 0.21f, nq.y + 0.63f));
    float amp = 0.02f + 0.002f * al;
    float x = q.x - 0.006f * al + (n1 - 0.5f) * 2.0f * amp;
    float y = q.y - 0.002f * al + (n2 - 0.5f) * 2.0f * amp;
    float sd = sdf_dist(g, x, y);
    if (sd >= 0.33f) continue;  // past the reach of the atlas: no smoke
    float r = sqrtf(fmaxf(sd, 0.0f) * fmaxf(sd, 0.0f) + q.z * q.z);
    float rho = 14.0f * (0.04f / s) * __expf(-(r * r) / (s * s)) * (0.55f + 0.9f * cloud_tex(nq * 2.3f));
    if (rho < 1e-4f) continue;
    float sig = rho * g.alpha * (dt / em);  // optical depth of the step: rho per em of path, dt km
    float a = 1.0f - __expf(-sig);
    float self = 0.55f + 0.45f * __expf(-rho * s * 3.0f);  // a dense core is darker inside
    Ls += (Esun * (ph * self) + amb) * (T * a);  // white smoke: albedo 1
    T *= 1.0f - a;
    if (T < 0.01f) break;
  }
  if (T > 0.999f) return base;
  V3 tr = g_ttr[gi], ins = g_tins[gi];
  return base * T + ins * (1.0f - T) + Ls * tr;
}

// A sign standing on the ground: a dark board on two posts with the word in
// warm neon, lit by the sun (shadowed by terrain and clouds) and the sky,
// hidden by nearer ground, behind the haze.
__device__ V3 draw_sign(const GText& g, int gi, V3 o, V3 d, V3 base, float pix, float depth) {
  float tp, x, y;
  if (!text_plane(g, o, d, &tp, &x, &y)) return base;
  if (g.occlude && depth < tp) return base;
  const float bx0 = -0.45f, bx1 = g.width + 0.45f, by0 = -0.45f, by1 = SDF_CAP_HEIGHT + 0.45f;
  float fw = fmaxf(pix * tp / g.height, 1e-5f);
  // signed distance to the board, and to the two posts below it
  float db = fmaxf(fmaxf(bx0 - x, x - bx1), fmaxf(by0 - y, y - by1));
  float pw = 0.06f;
  float p1 = fmaxf(fabsf(x - 0.15f * g.width) - pw, fmaxf(y - by0, by0 - 6.0f - y));
  float p2 = fmaxf(fabsf(x - 0.85f * g.width) - pw, fmaxf(y - by0, by0 - 6.0f - y));
  float dsh = fminf(db, fminf(p1, p2));
  float cov = sat(0.5f - dsh / fw);
  if (cov <= 0.0f) return base;
  V3 n = cross(g.right, g.up);
  bool front = dot(d, n) < 0.0f;
  V3 nf = front ? n : -n;
  float ndl = fmaxf(dot(nf, cP.sun_dir), 0.0f);
  V3 E = g_tsun[gi] * (ndl / PI_F) + sky_lookup(v3(0, 1, 0)) * 0.5f;
  V3 L = v3(0.05f, 0.05f, 0.055f) * E;  // graphite board and posts
  if (front && db <= 0.0f) {
    float dist = sdf_dist(g, x, y);
    float lc = sat(0.5f - dist / fw);
    L = L + g.color * (0.18f * __expf(-fmaxf(dist, 0.0f) * (1.0f / 0.12f)));  // the neon lights the board
    L = mix(L, g.color, lc);
  }
  return mix(base, g_tins[gi] + L * g_ttr[gi], cov * g.alpha);
}

__global__ void k_textlight() {
  int i = threadIdx.x;
  if (i >= cP.n_text) return;
  const GText& g = cP.text[i];
  if (g.style < GTEXT_TYPE) return;
  V3 o = cP.cam.pos, m = text_mid(g);
  float dist = length(m - o);
  V3 tr;
  g_tins[i] = scatter(o, (m - o) / fmaxf(dist, 1e-6f), dist, 8, &tr);
  g_ttr[i] = tr;
  float vis = cP.sun_dir.y > -0.1f ? terrain_shadow(m, cP.sun_dir) * fmaxf(cloud_shadow(m, cP.sun_dir), 0.4f) : 0.0f;
  g_tsun[i] = sun_at(m) * vis;
}

// Emissive text and light spheres, drawn per frame at the current t on top of
// the accumulated image. Exact (no sampling), so the same on every GPU. They
// are hidden where the pixel's first hit (`depth`, from the frame's or the
// hold's first sample) is nearer than they are. Letters are glowing glass
// tubes: lit by the sun and the sky, with an emissive core, a bright rim and a
// soft halo.
__device__ V3 overlay(V3 o, V3 d, V3 base, float pix, float depth) {
  for (int i = 0; i < cP.n_text; i++) {
    const GText& g = cP.text[i];
    if (g.alpha <= 0.0f || g.len == 0) continue;
    if (g.style == GTEXT_TYPE) { base = draw_type(g, i, o, d, base, pix, depth); continue; }
    if (g.style == GTEXT_SMOKE) { base = draw_smoke(g, i, o, d, base, depth); continue; }
    if (g.style == GTEXT_SIGN) { base = draw_sign(g, i, o, d, base, pix, depth); continue; }
    V3 nrm = cross(g.right, g.up);  // toward the camera that the text faces
    float sc = GLYPH_H / g.height;  // glyph units per km
    float width = g.len * GLYPH_ADVANCE - (GLYPH_ADVANCE - GLYPH_W);
    V3 oc = o - g.center;
    // ray in glyph space; t stays in km
    V3 ol = v3(dot(oc, g.right) * sc + 0.5f * width, dot(oc, g.up) * sc + 0.5f * GLYPH_H, dot(oc, nrm) * sc);
    V3 dl = v3(dot(d, g.right) * sc, dot(d, g.up) * sc, dot(d, nrm) * sc);
    if (fabsf(dl.z) < 1e-6f) continue;
    // slab |z| <= margin around the text plane
    const float margin = 3.0f;
    float t0 = (-margin - ol.z) / dl.z, t1 = (margin - ol.z) / dl.z;
    if (t0 > t1) { float tt = t0; t0 = t1; t1 = tt; }
    t0 = fmaxf(t0, 0.0f);
    if (t1 <= t0) continue;
    V3 pm = ol + dl * (0.5f * (t0 + t1));
    if (pm.x < -margin - 2.0f || pm.x > width + margin + 2.0f || pm.y < -margin - 2.0f || pm.y > GLYPH_H + margin + 2.0f)
      continue;
    float fw = fmaxf(pix * t0 * sc, 1e-4f);  // a pixel in glyph units at the slab
    float t = t0, mind = 1e9f, tmin = t0;
    bool hit = false;
    V3 nearest = v3(0.0f), ph = v3(0.0f), nmin = v3(0.0f);
    for (int it = 0; it < 40 && t < t1; it++) {
      V3 pl = ol + dl * t;
      V3 nr;
      float dist = text_sdf(g, pl, &nr) - (g.style == 1 ? CLOCK_TUBE : TEXT_TUBE);
      if (dist < mind) { mind = dist; ph = pl; nmin = nr; tmin = t; }
      if (dist < 0.25f * fw) { hit = true; break; }
      t += fmaxf(dist, 0.05f) / sc;
    }
    nearest = nmin;
    t = tmin;
    // full cover on a hit, partial for a ray passing within a pixel of the tube
    float cov = hit ? 1.0f : sat(1.0f - mind / fw);
    if (cov > 0.0f) hit = true;
    float glow = __expf(-fmaxf(mind, 0.0f) * 1.2f) * 0.18f;
    float tw = hit ? t : 0.5f * (t0 + t1);
    if (g.occlude && depth < tw) continue;  // behind terrain: the pixel's first hit is nearer
    float Ta = __expf(-tw * 0.018f);
    if (hit) {
      V3 nl = normalize(ph - nearest);
      V3 n = normalize(g.right * nl.x + g.up * nl.y + nrm * nl.z);
      V3 v = -d;
      V3 wp = o + d * t;
      V3 Es = sun_at(wp);
      float ndl = fmaxf(dot(n, cP.sun_dir), 0.0f);
      V3 amb = sky_lookup(n.y > -0.2f ? n : v3(0, 1, 0)) * 0.9f;
      float nv = sat(dot(n, v));
      float rim = powf(1.0f - nv, 2.5f);
      V3 L;
      if (g.style == 1) {
        // dark lacquered core, lit by the scene, with a bright rim: reads over
        // bright cloud (the core) and over dark ground (the rim)
        V3 lit = v3(0.03f) * (Es * (ndl / PI_F) + amb) + Es * ggx_spec(n, v, cP.sun_dir, 0.25f, 0.05f);
        L = lit + g.color * (0.03f + 2.4f * powf(1.0f - nv, 1.1f));
      } else {
        V3 col = g.color * (1.0f / 2.6f);  // albedo-scaled hue
        V3 lit = col * (Es * (ndl * 0.7f / PI_F) + amb) + Es * ggx_spec(n, v, cP.sun_dir, 0.22f, 0.04f);
        L = lit + g.color * (0.45f + 1.4f * rim);
      }
      base = mix(base, L * Ta, g.alpha * cov);
    }
    if (g.style == 1) {
      // a soft dark halo instead of a glow, so the outline holds over cloud
      if (!hit || cov < 1.0f) base = base * (1.0f - 0.6f * __expf(-fmaxf(mind, 0.0f) * 0.7f) * g.alpha);
    } else {
      base = base + g.color * (glow * g.alpha * Ta);
    }
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
// ACES filmic: sRGB -> ACES input, the fitted RRT + ODT curve, back to sRGB
// (the published fit by Stephen Hill).
__device__ V3 aces_fitted(V3 c) {
  V3 a = v3(0.59719f * c.x + 0.35458f * c.y + 0.04823f * c.z, 0.07600f * c.x + 0.90834f * c.y + 0.01566f * c.z,
            0.02840f * c.x + 0.13383f * c.y + 0.83777f * c.z);
  auto fit = [](float v) { return (v * (v + 0.0245786f) - 0.000090537f) / (v * (0.983729f * v + 0.4329510f) + 0.238081f); };
  a = v3(fit(a.x), fit(a.y), fit(a.z));
  return v3(1.60475f * a.x - 0.53108f * a.y - 0.07367f * a.z, -0.10208f * a.x + 1.10813f * a.y - 0.00605f * a.z,
            -0.00327f * a.x - 0.07276f * a.y + 1.07602f * a.z);
}

__device__ __forceinline__ float srgb(float x) {
  return x <= 0.0031308f ? 12.92f * x : 1.055f * powf(x, 1.0f / 2.4f) - 0.055f;
}

__device__ __forceinline__ bool draws_here(uint32_t X, uint32_t Y) {
  if (cP.half <= 1) return true;
  if (cP.half == 2) return ((X + Y + (uint32_t)cP.parity) & 1u) == 0u;
  const uint32_t order[4] = {0u, 3u, 1u, 2u};
  return ((X & 1u) + 2u * (Y & 1u)) == order[cP.parity & 3];
}

// Draw this frame's samples over the drawn region (tile plus guard band) into
// the ring entry for the frame: the mean of `spp` samples seeded by (pixel,
// frame seed, s), the first sample's hit distance and kind.
__global__ void k_moving(float4* ring, float2* ring_aux) {
  int i = blockIdx.x * blockDim.x + threadIdx.x, j = blockIdx.y * blockDim.y + threadIdx.y;
  if (i >= cP.ew || j >= cP.eh) return;
  uint32_t X = (uint32_t)(cP.ex0 + i), Y = (uint32_t)(cP.ey0 + j);
  int idx = j * cP.ew + i;
  if (!draws_here(X, Y)) {  // another part of the checkerboard
    ring[idx] = make_float4(0.0f, 0.0f, 0.0f, 0.0f);
    ring_aux[idx] = make_float2(0.0f, -1.0f);
    return;
  }
  const GCam& cam = cP.cam;
  float pix = 2.0f * cam.tan_half / cP.full_h;
  V3 sum = v3(0.0f);
  float depth = 1e9f, id = 0.0f;
  for (int s = 0; s < cP.spp; s++) {
    Rng rng{hash4(X, Y, cP.seed, (uint32_t)s)};
    float jx = rng.next(), jy = rng.next();
    Hit hh = trace(cam.pos, ray_dir(cam, X + jx, Y + jy), pix, rng);
    if (!isfinite(hh.L.x + hh.L.y + hh.L.z)) { atomicAdd(&g_bad, 1u); hh.L = v3(0.0f); }
    sum += hh.L;
    if (s == 0) { depth = hh.dist; id = hh.id; }
  }
  sum = sum / (float)cP.spp;
  ring[idx] = make_float4(sum.x, sum.y, sum.z, (float)cP.spp);
  ring_aux[idx] = make_float2(depth, id);
}

// Hold: add samples sample_base.. to each pixel's sum, in order, so the sum
// after n samples does not depend on how frames split them; the ring entry for
// this frame is the hold's current average.
__global__ void k_hold(float4* hold_sum, float2* hold_aux, float4* ring, float2* ring_aux) {
  int i = blockIdx.x * blockDim.x + threadIdx.x, j = blockIdx.y * blockDim.y + threadIdx.y;
  if (i >= cP.ew || j >= cP.eh) return;
  uint32_t X = (uint32_t)(cP.ex0 + i), Y = (uint32_t)(cP.ey0 + j);
  const GCam& cam = cP.cam;
  float pix = 2.0f * cam.tan_half / cP.full_h;
  int idx = j * cP.ew + i;
  float4 acc = hold_sum[idx];
  float2 aux = hold_aux[idx];
  // A pixel's next sample index is its own count, so the sum after n samples
  // is the same however frames (and the checkerboard) split them.
  int base = (int)acc.w;
  bool draw = draws_here(X, Y);
  for (int s = 0; draw && s < cP.spp; s++) {
    int k = base + s;
    if (k >= HOLD_MAX_SAMPLES) break;
    Rng rng{hash4(X, Y, cP.seed, (uint32_t)k)};
    float jx = rng.next(), jy = rng.next();
    Hit hh = trace(cam.pos, ray_dir(cam, X + jx, Y + jy), pix, rng);
    if (!isfinite(hh.L.x + hh.L.y + hh.L.z)) { atomicAdd(&g_bad, 1u); hh.L = v3(0.0f); }
    acc.x += hh.L.x; acc.y += hh.L.y; acc.z += hh.L.z; acc.w += 1.0f;
    if (k == 0) aux = make_float2(hh.dist, hh.id);
  }
  hold_sum[idx] = acc;
  hold_aux[idx] = aux;
  float inv = 1.0f / fmaxf(acc.w, 1.0f);
  ring[idx] = make_float4(acc.x * inv, acc.y * inv, acc.z * inv, acc.w);
  ring_aux[idx] = acc.w > 0.0f ? aux : make_float2(0.0f, -1.0f);
}

// Checkerboard frames: fill each pixel that has no samples (hit kind -1)
// from its 8 neighbours that do, at half their weight. Where a pair of
// opposite neighbours (left-right, up-down, or a diagonal) both drew and share
// a hit kind, the fill interpolates along the pair that differs least, so an
// edge (a silhouette against the sky) is continued along its own direction
// instead of being stepped. Otherwise it averages the neighbours of the first
// one's hit kind (left, right, up, down, then the diagonals). Only pixels of
// the drawn region whose neighbours all lie in it are ever read afterwards,
// and the image's edges are the same for every renderer, so the fill matches
// across tiles.
__global__ void k_fill(const float4* c, const float2* a, float4* co, float2* ao) {
  int i = blockIdx.x * blockDim.x + threadIdx.x, j = blockIdx.y * blockDim.y + threadIdx.y;
  if (i >= cP.ew || j >= cP.eh) return;
  int idx = j * cP.ew + i;
  float2 me = a[idx];
  if (me.y >= 0.0f) {
    co[idx] = c[idx];
    ao[idx] = me;
    return;
  }
  // the 4 edge neighbours first, then the 4 diagonal ones (a stride-4 frame
  // can leave a pixel with only a diagonal neighbour that drew); entries 2k
  // and 2k+1 are opposite each other
  const int dx[8] = {-1, 1, 0, 0, -1, 1, 1, -1}, dy[8] = {0, 0, -1, 1, -1, 1, -1, 1};
  float4 nc[8];
  float2 na[8];
  bool ok[8];
  for (int k = 0; k < 8; k++) {
    int ii = i + dx[k], jj = j + dy[k];
    ok[k] = false;
    if (ii < 0 || jj < 0 || ii >= cP.ew || jj >= cP.eh) continue;
    int nidx = jj * cP.ew + ii;
    na[k] = a[nidx];
    if (na[k].y < 0.0f) continue;
    nc[k] = c[nidx];
    ok[k] = true;
  }
  // the pair along the edge: both drew, same kind, least difference
  int best = -1;
  float bcost = 1e30f;
  for (int pr = 0; pr < 4; pr++) {
    int k0 = 2 * pr, k1 = k0 + 1;
    if (!ok[k0] || !ok[k1] || na[k0].y != na[k1].y) continue;
    float l0 = lum(v3(nc[k0].x, nc[k0].y, nc[k0].z)), l1 = lum(v3(nc[k1].x, nc[k1].y, nc[k1].z));
    float cost = fabsf(l0 - l1) / (l0 + l1 + 1e-4f);
    if (na[k0].x < 1e8f) cost += fabsf(na[k0].x - na[k1].x) / (na[k0].x + na[k1].x);
    if (cost < bcost) { bcost = cost; best = pr; }
  }
  float id = -1.0f, n = 0.0f, dsum = 0.0f, wsum = 0.0f;
  V3 col = v3(0.0f);
  for (int k = 0; k < 8; k++) {
    if (!ok[k]) continue;
    if (best >= 0 && k / 2 != best) continue;
    if (id < 0.0f) id = na[k].y;
    if (na[k].y != id) continue;
    col += v3(nc[k].x, nc[k].y, nc[k].z);
    wsum += nc[k].w;
    dsum += na[k].x;
    n += 1.0f;
  }
  if (n == 0.0f) {
    co[idx] = c[idx];
    ao[idx] = me;
    return;
  }
  col = col / n;
  co[idx] = make_float4(col.x, col.y, col.z, 0.5f * wsum / n);
  ao[idx] = make_float2(dsum / n, id);
}

struct Ring {
  const float4* c[TEMPORAL_WINDOW];
  const float2* a[TEMPORAL_WINDOW];
};

// Resolve the tile's pixels. Moving: this frame's samples plus the previous
// frames' (up to TEMPORAL_WINDOW - 1 of them) reprojected through this pixel's
// first hit (a cloud, where it covers more than half the pixel). A previous
// frame's texel counts only where its hit kind matches, its distance agrees
// within 4% (on land and water), and the displacement is within `guard`
// pixels; the rule names whole-image pixels only, and every texel it can read
// lies in the drawn region, so the result is the same on every renderer that
// drew the same frames. Hold: the hold's average, with the motion output it
// started from blended in until HOLD_BLEND_OUT samples.
__global__ void k_resolve(float4* resolved, const float4* hold_blend, const float4* hold_sum, const float2* /*hold_aux*/,
                          const float4* cur, const float2* cur_aux, Ring hist) {
  int i = blockIdx.x * blockDim.x + threadIdx.x, j = blockIdx.y * blockDim.y + threadIdx.y;
  if (i >= cP.rw || j >= cP.rh) return;
  int X = cP.rx0 + i, Y = cP.ry0 + j;
  int eidx = (Y - cP.ey0) * cP.ew + (X - cP.ex0);
  int idx = j * cP.rw + i;
  const GCam& cam = cP.cam;
  V3 d0 = ray_dir(cam, X + 0.5f, Y + 0.5f);
  V3 color;
  float depth, wtot;
  if (cP.hold && hold_sum[eidx].w == 0.0f) {
    float4 c0 = cur[eidx];  // no samples here yet (checkerboard): the filled value
    color = v3(c0.x, c0.y, c0.z);
    wtot = c0.w;
  } else if (cP.hold) {
    float4 acc = hold_sum[eidx];
    float4 hb = hold_blend[idx];
    float m = fminf(hb.w, (float)TAA_MAX_HISTORY) * fmaxf(0.0f, 1.0f - acc.w / HOLD_BLEND_OUT);
    float inv = 1.0f / fmaxf(acc.w + m, 1e-6f);
    color = v3((acc.x + hb.x * m) * inv, (acc.y + hb.y * m) * inv, (acc.z + hb.z * m) * inv);
    wtot = acc.w + m;
  } else {
    float4 c0 = cur[eidx];
    float2 a0 = cur_aux[eidx];
    depth = a0.x;
    float id = a0.y;
    V3 sum = v3(c0.x, c0.y, c0.z) * c0.w;
    wtot = c0.w;
    for (int h = 0; h < TEMPORAL_WINDOW - 1; h++) {
      if (!hist.c[h]) continue;
      const GCam& pc = cP.hist_cam[h];
      V3 rel = depth < 1e8f ? (cam.pos + d0 * depth - pc.pos) : d0;
      float z = dot(rel, pc.fwd);
      if (z <= 1e-6f) continue;
      float sx = dot(rel, pc.right) / (z * pc.tan_half * pc.aspect);
      float sy = dot(rel, pc.up) / (z * pc.tan_half);
      float qx = (sx * 0.5f + 0.5f) * cP.full_w - 0.5f, qy = (0.5f - 0.5f * sy) * cP.full_h - 0.5f;
      if (!(fabsf(qx - X) <= cP.guard && fabsf(qy - Y) <= cP.guard)) continue;
      if (!(qx >= 0.0f && qy >= 0.0f && qx <= cP.full_w - 1.0f && qy <= cP.full_h - 1.0f)) continue;
      float expect = depth < 1e8f ? length(rel) : 1e9f;
      int ix = (int)floorf(qx), iy = (int)floorf(qy);
      float fx = qx - ix, fy = qy - iy;
      V3 hc = v3(0.0f);
      float ws = 0.0f, wn = 1e9f;
      for (int b = 0; b < 4; b++) {
        int xx = min(ix + (b & 1), cP.full_w - 1), yy = min(iy + (b >> 1), cP.full_h - 1);
        float w = ((b & 1) ? fx : 1.0f - fx) * ((b >> 1) ? fy : 1.0f - fy);
        if (w <= 0.0f) continue;
        int e = (yy - cP.ey0) * cP.ew + (xx - cP.ex0);
        float2 a = hist.a[h][e];
        if (a.y != id) continue;
        // in the sky (clear, or a cloud over it) any texel of the same kind
        // counts: a cloud's edge blends with the sky beside it instead of
        // dropping its history
        if (id != 0.0f && depth < 1e8f && fabsf(a.x - expect) > 0.04f * expect + 0.002f) continue;
        float4 hv = hist.c[h][e];
        hc += v3(hv.x, hv.y, hv.z) * w;
        ws += w;
        wn = fminf(wn, hv.w);
      }
      if (ws < 0.3f) continue;
      float wf = fminf(wn, (float)TEMPORAL_FRAME_CAP);
      sum += hc * (wf / ws);
      wtot += wf;
    }
    color = sum / wtot;
  }
  resolved[idx] = make_float4(color.x, color.y, color.z, fminf(wtot, (float)HOLD_MAX_SAMPLES));
}

// Finish the out region (the tile plus the stream margin): where few samples are in (fast motion, a fresh
// renderer), an edge-aware 5x5 filter over the resolved region (which reaches
// DENOISE_R pixels past the tile, drawn under the same rules, so the filter
// matches across tile edges); then the overlays, tone mapping and dither.
__global__ void k_finish(uint8_t* rgba, float4* hdr, const float4* resolved, const float2* aux) {
  int i = blockIdx.x * blockDim.x + threadIdx.x, j = blockIdx.y * blockDim.y + threadIdx.y;
  if (i >= cP.ow || j >= cP.oh) return;
  int X = cP.ox0 + i, Y = cP.oy0 + j;
  int idx = j * cP.ow + i;
  const GCam& cam = cP.cam;
  float pix = 2.0f * cam.tan_half / cP.full_h;
  V3 d0 = ray_dir(cam, X + 0.5f, Y + 0.5f);
  float4 c = resolved[(Y - cP.ry0) * cP.rw + (X - cP.rx0)];
  float2 ca = aux[(Y - cP.ey0) * cP.ew + (X - cP.ex0)];
  float depth = ca.x;
  V3 color = v3(c.x, c.y, c.z);
  float strength = sat(1.5f - c.w / 32.0f);
  if (strength > 0.0f) {
    float lc = lum(color);
    V3 sum = v3(0.0f);
    float ws = 0.0f;
    for (int dy = -DENOISE_R; dy <= DENOISE_R; dy++)
      for (int dx = -DENOISE_R; dx <= DENOISE_R; dx++) {
        int xx = X + dx, yy = Y + dy;
        if (xx < cP.rx0 || yy < cP.ry0 || xx >= cP.rx0 + cP.rw || yy >= cP.ry0 + cP.rh) continue;
        float2 na = aux[(yy - cP.ey0) * cP.ew + (xx - cP.ex0)];
        if (na.y != ca.y) continue;
        if (depth < 1e8f && fabsf(na.x - depth) > 0.03f * depth + 0.002f) continue;
        float4 nv = resolved[(yy - cP.ry0) * cP.rw + (xx - cP.rx0)];
        V3 n = v3(nv.x, nv.y, nv.z);
        float w = __expf(-(float)(dx * dx + dy * dy) * (1.0f / 4.5f)) *
                  __expf(-fabsf(lum(n) - lc) / (0.35f * lc + 0.02f));
        sum += n * w;
        ws += w;
      }
    if (ws > 0.0f) color = mix(color, sum / ws, strength);
  }
  color = overlay(cam.pos, d0, color, pix, ca.y == 0.0f ? 1e9f : depth);  // clouds in the sky do not hide words
  // sun glare: the sun's disc spread by the eye and lens, scaled by how much
  // of the sun the camera sees (one value per frame), so it is the same on
  // every tile and tile edges match
  {
    float c = dot(d0, cP.sun_dir);
    float ang = acosf(clampf(c, -1.0f, 1.0f));
    V3 Es = sun_at(cam.pos) * g_sunvis;
    color += Es * (0.5f * __expf(-ang * (1.0f / 0.012f)) + 0.06f * __expf(-ang * (1.0f / 0.07f)) +
                   0.01f * __expf(-ang * (1.0f / 0.35f)));
  }
  hdr[idx] = make_float4(color.x, color.y, color.z, 1.0f);
  // tone map: ACES (fitted RRT + ODT), a warm/cool split, a light vignette in
  // whole-image coordinates, sRGB, dither by whole-image pixel
  V3 t = color * cP.exposure;
  // lift the deepest shadows before the curve's toe crushes them: up to 1.5x
  // below about 0.03 of the exposed range, nothing above 0.2
  t = t * (1.0f + 0.5f * __expf(-lum(t) * (1.0f / 0.04f)));
  t = aces_fitted(t);
  float l = lum(t);
  t = t * mix(v3(0.96f, 0.99f, 1.06f), v3(1.05f, 1.0f, 0.93f), smoothstepf(0.1f, 0.7f, l));
  float vx = (X + 0.5f) / cP.full_w - 0.5f, vy = (Y + 0.5f) / cP.full_h - 0.5f;
  t = t * (1.0f - 0.28f * (vx * vx + vy * vy));
  float dith = (u01(hash4((uint32_t)X, (uint32_t)Y, 0xD17Eu, 0u)) - 0.5f) * (1.0f / 255.0f);
  uint8_t* o = rgba + 4 * idx;
  o[0] = (uint8_t)(sat(srgb(sat(t.x)) + dith) * 255.0f + 0.5f);
  o[1] = (uint8_t)(sat(srgb(sat(t.y)) + dith) * 255.0f + 0.5f);
  o[2] = (uint8_t)(sat(srgb(sat(t.z)) + dith) * 255.0f + 0.5f);
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

__global__ void k_nv12(const uint8_t* src, int sw, int sh, uint8_t* dst, size_t pitch, int dw, int dh, float fx0, float fy0,
                       float sc) {
  int bx = blockIdx.x * blockDim.x + threadIdx.x, by = blockIdx.y * blockDim.y + threadIdx.y;
  if (2 * bx >= dw || 2 * by >= dh) return;
  V3 rgb4[4];
  for (int k = 0; k < 4; k++) {
    int x = 2 * bx + (k & 1), y = 2 * by + (k >> 1);
    // the source box of this pixel, clamped into the source (edge pixels repeat)
    float x0 = fx0 + x * sc, y0 = fy0 + y * sc;
    float x1 = x0 + sc, y1 = y0 + sc;
    x0 = clampf(x0, 0.0f, sw - 1.0f); y0 = clampf(y0, 0.0f, sh - 1.0f);
    x1 = clampf(x1, x0 + 0.5f, (float)sw); y1 = clampf(y1, y0 + 0.5f, (float)sh);
    rgb4[k] = area_sample(src, sw, sh, x0, y0, x1, y1);
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

static std::vector<uint8_t> b64_decode(const char* in) {
  auto val = [](char c) -> int {
    if (c >= 'A' && c <= 'Z') return c - 'A';
    if (c >= 'a' && c <= 'z') return c - 'a' + 26;
    if (c >= '0' && c <= '9') return c - '0' + 52;
    if (c == '+') return 62;
    if (c == '/') return 63;
    return -1;
  };
  std::vector<uint8_t> out;
  uint32_t acc = 0;
  int bits = 0;
  for (const char* p = in; *p; p++) {
    int v = val(*p);
    if (v < 0) continue;  // padding
    acc = (acc << 6) | (uint32_t)v;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out.push_back((uint8_t)((acc >> bits) & 0xFF));
    }
  }
  return out;
}

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
    cudaEvent_t e0, e1, e2, e3;
    CK(cudaEventCreate(&e0));
    CK(cudaEventCreate(&e1));
    CK(cudaEventCreate(&e2));
    CK(cudaEventCreate(&e3));
    ev0_ = e0;
    ev1_ = e1;
    evt0_ = e2;
    evt1_ = e3;

    // noise table, identical to the host's
    const HostNoise& hn = host_noise();
    CK(cudaMalloc(&noise_, sizeof(V4) * NOISE_N * NOISE_N));
    CK(cudaMemcpyAsync(noise_, hn.tab.data(), sizeof(V4) * NOISE_N * NOISE_N, cudaMemcpyHostToDevice, st));
    // font
    const FontTable& f = font_table();
    CK(cudaMemcpyToSymbolAsync(cSeg, f.seg, sizeof f.seg, 0, cudaMemcpyHostToDevice, st));
    CK(cudaMemcpyToSymbolAsync(cGStart, f.start, sizeof f.start, 0, cudaMemcpyHostToDevice, st));
    CK(cudaMemcpyToSymbolAsync(cGCount, f.count, sizeof f.count, 0, cudaMemcpyHostToDevice, st));
    // Inter's signed distance atlas -> a 2D texture, bilinear, read as 0..1
    cudaTextureObject_t sdf_tex_ = 0;
    {
      std::vector<uint8_t> atlas = b64_decode(kSdfAtlasB64);
      if (atlas.size() != (size_t)SDF_ATLAS_W * SDF_ATLAS_H) throw std::runtime_error("font atlas has the wrong size");
      cudaChannelFormatDesc ad = cudaCreateChannelDesc<unsigned char>();
      cudaArray_t aarr;
      CK(cudaMallocArray(&aarr, &ad, SDF_ATLAS_W, SDF_ATLAS_H));
      CK(cudaMemcpy2DToArray(aarr, 0, 0, atlas.data(), SDF_ATLAS_W, SDF_ATLAS_W, SDF_ATLAS_H, cudaMemcpyHostToDevice));
      cudaResourceDesc ard = {};
      ard.resType = cudaResourceTypeArray;
      ard.res.array.array = aarr;
      cudaTextureDesc atd = {};
      atd.addressMode[0] = atd.addressMode[1] = cudaAddressModeClamp;
      atd.filterMode = cudaFilterModeLinear;
      atd.readMode = cudaReadModeNormalizedFloat;
      atd.normalizedCoords = 0;
      CK(cudaCreateTextureObject(&sdf_tex_, &ard, &atd, nullptr));
      CK(cudaMemcpyToSymbolAsync(cSdf, kSdfGlyphs, sizeof kSdfGlyphs, 0, cudaMemcpyHostToDevice, st));
    }
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
    Globals g{(const V4*)noise_, (const V4*)trans_, (const V4*)sky_, tex, 0, nullptr};
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
    g.macro_arr = mac;
    g.sdf = sdf_tex_;
    CK(cudaMemcpyToSymbolAsync(cG, &g, sizeof g, 0, cudaMemcpyHostToDevice, st));
    CK(cudaStreamSynchronize(st));
    CK(cudaFree(vol));
  } catch (const std::exception& e) {
    *err = e.what();
    return false;
  }
  return true;
}

double Renderer::drawn_pixels(const Tile& t) const {
  int g = (int)ceilf(GUARD_FRAC * t.full_h) + 2 + DENOISE_R + margin_;
  return (double)(t.w + 2 * g) * (t.h + 2 * g);
}

void Renderer::set_margin(int m) {
  if (m == margin_) return;
  margin_ = m;
  have_tile_ = false;  // regions change: reallocate on the next frame
}

void Renderer::ensure_buffers(const Tile& t) {
  if (have_tile_ && t.w == tile_.w && t.h == tile_.h && t.full_w == tile_.full_w && t.full_h == tile_.full_h &&
      t.x0 == tile_.x0 && t.y0 == tile_.y0)
    return;
  // Regions, each inside the image (a tile may overhang it by its rounding
  // pixel; the regions then keep the tile):
  //   out      = tile + margin_      finished, tone-mapped, what streams encode
  //   resolved = out + DENOISE_R     temporal resolve, read by the finishing filter
  //   drawn    = resolved + g + 2    samples; reprojection taps reach g + 1 past resolved
  auto grow = [&](int x0, int y0, int x1, int y1, int m, int* a0, int* b0, int* a1, int* b1) {
    *a0 = std::max(0, x0 - m); *b0 = std::max(0, y0 - m);
    *a1 = std::max(std::min(t.full_w, x1 + m), x1); *b1 = std::max(std::min(t.full_h, y1 + m), y1);
  };
  int g = (int)ceilf(GUARD_FRAC * t.full_h);
  int ox0, oy0, ox1, oy1, rx0, ry0, rx1, ry1, ex0, ey0, ex1, ey1;
  grow(t.x0, t.y0, t.x0 + t.w, t.y0 + t.h, margin_, &ox0, &oy0, &ox1, &oy1);
  grow(ox0, oy0, ox1, oy1, DENOISE_R, &rx0, &ry0, &rx1, &ry1);
  grow(rx0, ry0, rx1, ry1, g + 2, &ex0, &ey0, &ex1, &ey1);
  size_t no = (size_t)(ox1 - ox0) * (oy1 - oy0);
  size_t nr = (size_t)(rx1 - rx0) * (ry1 - ry0);
  size_t ne = (size_t)(ex1 - ex0) * (ey1 - ey0);
  cudaFree(rgba_); cudaFree(hdr_); cudaFree(display_); cudaFree(hold_blend_);
  cudaFree(hold_sum_); cudaFree(hold_aux_);
  for (int i = 0; i < TEMPORAL_WINDOW; i++) { cudaFree(ring_[i]); cudaFree(ring_aux_[i]); }
  cudaFree(fill_c_); cudaFree(fill_a_);
  CK(cudaMalloc(&fill_c_, ne * sizeof(float4)));
  CK(cudaMalloc(&fill_a_, ne * sizeof(float2)));
  CK(cudaMalloc(&rgba_, no * 4));
  CK(cudaMalloc(&hdr_, no * sizeof(float4)));
  CK(cudaMalloc(&display_, nr * sizeof(float4)));
  CK(cudaMalloc(&hold_blend_, nr * sizeof(float4)));
  CK(cudaMalloc(&hold_sum_, ne * sizeof(float4)));
  CK(cudaMalloc(&hold_aux_, ne * sizeof(float2)));
  for (int i = 0; i < TEMPORAL_WINDOW; i++) {
    CK(cudaMalloc(&ring_[i], ne * sizeof(float4)));
    CK(cudaMalloc(&ring_aux_[i], ne * sizeof(float2)));
  }
  tile_ = t;
  ex0_ = ex0; ey0_ = ey0; ew_ = ex1 - ex0; eh_ = ey1 - ey0; guard_ = g;
  rx0_ = rx0; ry0_ = ry0; rw_ = rx1 - rx0; rh_ = ry1 - ry0;
  ox0_ = ox0; oy0_ = oy0; ow_ = ox1 - ox0; oh_ = oy1 - oy0;
  have_tile_ = true;
  reset_history();
}

void Renderer::reset_history() {
  for (int i = 0; i < TEMPORAL_WINDOW; i++) ring_slot_[i] = LLONG_MIN;
  hold_key_ = 0;
  hold_n_ = 0;
  hold_frames_ = 0;
  if (!have_tile_) return;
  size_t nr = (size_t)rw_ * rh_;
  CK(cudaMemsetAsync(display_, 0, nr * sizeof(float4), (cudaStream_t)stream_));
}

void Renderer::render(const Scene& s, const Tile& tile, double t, bool accumulate, int spp) {
  ensure_buffers(tile);
  cudaStream_t st = (cudaStream_t)stream_;
  uint64_t key = 0;
  bool held = false;
  FrameParams p = build_frame(s, tile, t, &key, &held);
  // Under 0.5 samples per drawn pixel, draw a checkerboard: one sample on
  // half the pixels per frame, or on a quarter under 0.3. (A checkerboard
  // frame is noisier, which costs bits and seams in the encoder, so wall tiles
  // stay at one or more samples on every pixel.)
  // A hold keeps the budget it started with, so its samples per frame and
  // its quality stay fixed while it accumulates (a budget change takes
  // effect in holds at the next hold).
  bool new_hold = accumulate && held && key != hold_key_;
  if (new_hold) hold_budget_ = budget_;
  double budget = (accumulate && held) ? hold_budget_ : budget_;
  int stride = checker_ > 0 ? checker_ : 1;
  int hq = quality_ >= 0 ? quality_ : 0;
  if (spp <= 0) {
    double per_px = budget / drawn_pixels(tile);
    if (checker_ < 0) stride = stride_for(per_px);
    spp = stride > 1 ? 1 : (int)std::lround(per_px);
    if (quality_ < 0) hq = (stride == 1 && per_px >= HQ_SPP) ? 1 : 0;
  }
  spp = spp < 1 ? 1 : spp > 64 ? 64 : spp;
  p.hq = hq;
  stats_.hq = hq != 0;
  bool half = stride > 1;
  p.spp = spp;
  p.half = stride;
  p.ex0 = ex0_; p.ey0 = ey0_; p.ew = ew_; p.eh = eh_; p.guard = guard_;
  p.rx0 = rx0_; p.ry0 = ry0_; p.rw = rw_; p.rh = rh_;
  p.ox0 = ox0_; p.oy0 = oy0_; p.ow = ow_; p.oh = oh_;
  size_t nr = (size_t)rw_ * rh_, ne = (size_t)ew_ * eh_;
  long long k = std::llround(t * fps_);
  int ri = (int)(((k % TEMPORAL_WINDOW) + TEMPORAL_WINDOW) % TEMPORAL_WINDOW);
  p.parity = (int)(k & 3);
  if (!accumulate) {
    held = false;
    p.hold = 0;
    reset_history();
  } else if (held) {
    if (new_hold) {
      CK(cudaMemsetAsync(hold_sum_, 0, ne * sizeof(float4), st));
      CK(cudaMemsetAsync(hold_aux_, 0, ne * sizeof(float2), st));
      CK(cudaMemcpyAsync(hold_blend_, display_, nr * sizeof(float4), cudaMemcpyDeviceToDevice, st));
      hold_key_ = key;
      hold_frames_ = 0;
    }
    // the fewest samples any pixel has before this frame (each pixel draws
    // every frame, or every other frame on a checkerboard)
    long long before = (hold_frames_ / stride) * (long long)spp;
    p.sample_base = (int)std::min<long long>(before, HOLD_MAX_SAMPLES);
    hold_frames_++;
    long long after = (hold_frames_ / stride) * (long long)spp;
    hold_n_ = (int)std::min<long long>(after, HOLD_MAX_SAMPLES);
  } else {
    hold_key_ = 0;
    hold_n_ = 0;
    hold_frames_ = 0;
  }
  // previous frames k-1 .. k-W+1 that this renderer drew
  Ring ring;
  for (int h = 0; h < TEMPORAL_WINDOW - 1; h++) {
    long long kk = k - 1 - h;
    int r = (int)(((kk % TEMPORAL_WINDOW) + TEMPORAL_WINDOW) % TEMPORAL_WINDOW);
    bool have = accumulate && !held && ring_slot_[r] == kk;
    ring.c[h] = have ? (const float4*)ring_[r] : nullptr;
    ring.a[h] = have ? (const float2*)ring_aux_[r] : nullptr;
    if (have) p.hist_cam[h] = ring_cam_[r];
  }
  CK(cudaMemcpyToSymbolAsync(cP, &p, sizeof p, 0, cudaMemcpyHostToDevice, st));
  CK(cudaEventRecord((cudaEvent_t)ev0_, st));
  k_sky<<<dim3(SKY_W / 64, SKY_H), 64, 0, st>>>((V4*)sky_);
  k_sunvis<<<1, 1, 0, st>>>();
  k_textlight<<<1, MAX_TEXTS, 0, st>>>();
  unsigned int zero = 0;
  CK(cudaMemcpyToSymbolAsync(g_bad, &zero, sizeof zero, 0, cudaMemcpyHostToDevice, st));
  dim3 bs(8, 8);
  CK(cudaEventRecord((cudaEvent_t)evt0_, st));
  if (held && p.sample_base < HOLD_MAX_SAMPLES) {
    k_hold<<<dim3((ew_ + 7) / 8, (eh_ + 7) / 8), bs, 0, st>>>((float4*)hold_sum_, (float2*)hold_aux_, (float4*)ring_[ri],
                                                               (float2*)ring_aux_[ri]);
  } else if (!held) {
    k_moving<<<dim3((ew_ + 7) / 8, (eh_ + 7) / 8), bs, 0, st>>>((float4*)ring_[ri], (float2*)ring_aux_[ri]);
  }
  CK(cudaGetLastError());
  CK(cudaEventRecord((cudaEvent_t)evt1_, st));
  if (half && !(held && p.sample_base >= HOLD_MAX_SAMPLES)) {
    k_fill<<<dim3((ew_ + 7) / 8, (eh_ + 7) / 8), bs, 0, st>>>((const float4*)ring_[ri], (const float2*)ring_aux_[ri],
                                                               (float4*)fill_c_, (float2*)fill_a_);
    CK(cudaGetLastError());
    std::swap(ring_[ri], fill_c_);
    std::swap(ring_aux_[ri], fill_a_);
  }
  k_resolve<<<dim3((rw_ + 7) / 8, (rh_ + 7) / 8), bs, 0, st>>>((float4*)display_, (const float4*)hold_blend_,
                                                                (const float4*)hold_sum_, (const float2*)hold_aux_,
                                                                (const float4*)ring_[ri], (const float2*)ring_aux_[ri], ring);
  CK(cudaGetLastError());
  k_finish<<<dim3((ow_ + 7) / 8, (oh_ + 7) / 8), bs, 0, st>>>(
      (uint8_t*)rgba_, (float4*)hdr_, (const float4*)display_, (const float2*)ring_aux_[ri]);
  CK(cudaGetLastError());
  CK(cudaEventRecord((cudaEvent_t)ev1_, st));
  CK(cudaEventSynchronize((cudaEvent_t)ev1_));
  float ms = 0;
  cudaEventElapsedTime(&ms, (cudaEvent_t)ev0_, (cudaEvent_t)ev1_);
  stats_.render_ms = ms;
  float tms = 0;
  cudaEventElapsedTime(&tms, (cudaEvent_t)evt0_, (cudaEvent_t)evt1_);
  stats_.trace_ms = tms;
  CK(cudaMemcpyFromSymbol(&stats_.bad_samples, g_bad, sizeof(unsigned int)));
  stats_.spp = (held && p.sample_base >= HOLD_MAX_SAMPLES) ? 0 : spp;
  stats_.half = stride;
  stats_.hold = held;
  stats_.hold_samples = held ? hold_n_ : 0;
  if (accumulate) {
    // A saturated hold draws nothing new; its ring entry still holds the
    // hold's average from an earlier frame, so mark it for this slot too by
    // copying (cheap, and only while saturated).
    if (held && p.sample_base >= HOLD_MAX_SAMPLES) {
      int prev = -1;
      for (int i = 0; i < TEMPORAL_WINDOW; i++)
        if (ring_slot_[i] != LLONG_MIN && (prev < 0 || ring_slot_[i] > ring_slot_[prev])) prev = i;
      if (prev >= 0 && prev != ri) {
        CK(cudaMemcpyAsync(ring_[ri], ring_[prev], ne * sizeof(float4), cudaMemcpyDeviceToDevice, st));
        CK(cudaMemcpyAsync(ring_aux_[ri], ring_aux_[prev], ne * sizeof(float2), cudaMemcpyDeviceToDevice, st));
        CK(cudaStreamSynchronize(st));
      }
    }
    ring_slot_[ri] = k;
    ring_cam_[ri] = p.cam;
  }
}

void Renderer::download_rgba(std::vector<uint8_t>* out) const {
  out->resize((size_t)tile_.w * tile_.h * 4);
  CK(cudaMemcpy2D(out->data(), (size_t)tile_.w * 4,
                  (const uint8_t*)rgba_ + ((size_t)(tile_.y0 - oy0_) * ow_ + (tile_.x0 - ox0_)) * 4, (size_t)ow_ * 4,
                  (size_t)tile_.w * 4, tile_.h, cudaMemcpyDeviceToHost));
}
void Renderer::download_hdr(std::vector<float>* out) const {
  out->resize((size_t)tile_.w * tile_.h * 4);
  CK(cudaMemcpy2D(out->data(), (size_t)tile_.w * 16,
                  (const uint8_t*)hdr_ + ((size_t)(tile_.y0 - oy0_) * ow_ + (tile_.x0 - ox0_)) * 16, (size_t)ow_ * 16,
                  (size_t)tile_.w * 16, tile_.h, cudaMemcpyDeviceToHost));
}

void Renderer::to_nv12(void* dst, size_t pitch, int dw, int dh, int sx0, int sy0, int sfull_h) {
  cudaStream_t st = (cudaStream_t)stream_;
  // stream pixel (u, v) covers whole-image stream pixels [sx0+u, sx0+u+1); in
  // this renderer's grid that is scale times as many, offset by the out region
  double scale = (double)tile_.full_h / sfull_h;
  float fx0 = (float)(sx0 * scale - ox0_), fy0 = (float)(sy0 * scale - oy0_);
  dim3 bs(16, 8), gs((dw / 2 + 15) / 16, (dh / 2 + 7) / 8);
  k_nv12<<<gs, bs, 0, st>>>((const uint8_t*)rgba_, ow_, oh_, (uint8_t*)dst, pitch, dw, dh, fx0, fy0, (float)scale);
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
void Renderer::bind_thread() { cudaSetDevice(0); }
