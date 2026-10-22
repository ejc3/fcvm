// The world's geometry, shared by the GPU kernels and by the host (which keeps
// the camera above ground). Units are kilometres, y is up, sea level is y' = 0
// where y' = y + (x^2 + z^2) / (2 R) is the height above the planet's surface.
#pragma once
#include "vec.h"

#define PLANET_R 6360.0f
#define ATMO_R 6420.0f
#define NOISE_N 256          // 2D noise table is NOISE_N x NOISE_N
#define TERRAIN_MAX 3.6f     // no terrain is higher than this (km)

#if defined(__CUDA_ARCH__)
HD V4 tab_fetch(const V4* t, int i) {
  float4 v = __ldg(reinterpret_cast<const float4*>(t) + i);
  return {v.x, v.y, v.z, v.w};
}
#else
HD V4 tab_fetch(const V4* t, int i) { return t[i]; }
#endif

// Height of p above the planet's surface (paraboloid approximation of the
// sphere, accurate to a metre within ~400 km of the origin).
HD float alt_flat(V3 p) { return p.y + (p.x * p.x + p.z * p.z) * (0.5f / PLANET_R); }

// Value noise with analytic derivatives from a table whose texel (i, j) holds
// the four lattice values (i,j) (i+1,j) (i,j+1) (i+1,j+1): one fetch per call.
// Returns (value in -1..1, d/dx, d/dy).
HD V3 noised(V2 x, const V4* tab) {
  float fx = floorf(x.x), fy = floorf(x.y);
  int ix = ((int)fx) & (NOISE_N - 1), iy = ((int)fy) & (NOISE_N - 1);
  V4 c = tab_fetch(tab, iy * NOISE_N + ix);
  float wx = x.x - fx, wy = x.y - fy;
  float ux = wx * wx * wx * (wx * (wx * 6.0f - 15.0f) + 10.0f);
  float uy = wy * wy * wy * (wy * (wy * 6.0f - 15.0f) + 10.0f);
  float dux = 30.0f * wx * wx * (wx * (wx - 2.0f) + 1.0f);
  float duy = 30.0f * wy * wy * (wy * (wy - 2.0f) + 1.0f);
  float k0 = c.x, k1 = c.y - c.x, k2 = c.z - c.x, k4 = c.x - c.y - c.z + c.w;
  return {k0 + k1 * ux + k2 * uy + k4 * ux * uy, dux * (k1 + k4 * uy), duy * (k2 + k4 * ux)};
}
HD float noise(V2 x, const V4* tab) { return noised(x, tab).x; }

// Rotate by ~37 degrees and scale by 2 between octaves so lattices never align.
HD V2 oct_step(V2 p) { return {1.6f * p.x - 1.2f * p.y + 0.37f, 1.2f * p.x + 1.6f * p.y - 1.71f}; }

HD float fbm(V2 p, int oct, const V4* tab) {
  float a = 0.0f, amp = 0.5f;
  for (int i = 0; i < oct; i++) {
    a += amp * noise(p, tab);
    amp *= 0.5f;
    p = oct_step(p);
  }
  return a;
}

// Large-scale terrain fields: where land is, where mountains rise, and the
// domain warp. They vary over tens of kilometres, so the GPU reads them from a
// baked texture (MACRO_*) and the host evaluates them directly.
struct Macro {
  float land, massif;
  V2 w;
};
#define MACRO_N 2048
#define MACRO_KM 1024.0f  // the baked square spans [-512, 512) km in x and z

HD Macro macro_direct(V2 pw, const V4* tab) {
  Macro m;
  float cont = fbm(pw * (1.0f / 260.0f) + v2(13.1f, 7.7f), 4, tab);
  float range = fbm(pw * (1.0f / 90.0f) + v2(-5.3f, 21.4f), 4, tab);
  V2 q = pw * (1.0f / 4.5f);
  m.w = v2(fbm(q * 0.21f + v2(1.7f, 9.2f), 3, tab), fbm(q * 0.21f + v2(8.3f, 2.8f), 3, tab));
  m.land = smoothstepf(-0.12f, 0.16f, cont + 0.08f);
  m.massif = smoothstepf(-0.12f, 0.32f, range) * m.land;
  return m;
}

// Terrain height (km above sea level, negative under the sea) at world xz.
// `oct` sets the detail octaves of the eroded layer: more for shading normals,
// fewer for distant marching.
HD float terrain_from(V2 pw, Macro m, int oct, const V4* tab) {
  V2 q = pw * (1.0f / 4.5f) + m.w * 1.2f;
  // Eroded fbm: octaves are damped where the accumulated slope is steep, so
  // detail gathers in valleys and ridges stay sharp.
  float a = 0.0f, amp = 1.0f;
  V2 d = {0.0f, 0.0f};
  V2 qe = q;
  for (int i = 0; i < oct; i++) {
    V3 n = noised(qe, tab);
    d = d + v2(n.y, n.z);
    a += amp * n.x / (1.0f + dot(d, d));
    amp *= 0.48f;
    qe = oct_step(qe);
  }
  // Ridged noise for long crests.
  V2 r = q * 0.62f + v2(4.4f, -2.2f);
  float rid = 0.0f, ra = 0.55f;
  for (int i = 0; i < 3; i++) {
    float nv = noise(r, tab);
    float n = 1.0f - sqrtf(nv * nv + 0.02f);  // soft crease
    rid += ra * n * n;
    ra *= 0.5f;
    r = oct_step(r);
  }
  float lowland = 0.13f + 0.08f * a;
  float mount = 0.2f + 1.5f * rid + 1.1f * a;
  return mixf(-0.55f + 0.25f * a, lowland, m.land) + m.massif * fmaxf(mount, 0.0f) * 1.15f;
}

HD float terrain_h(V2 pw, int oct, const V4* tab) { return terrain_from(pw, macro_direct(pw, tab), oct, tab); }
