// Small vector math shared by host and device code. Host-only builds (the unit
// tests) compile it without CUDA.
#pragma once
#include <math.h>
#include <stdint.h>

#ifdef __CUDACC__
#define HD __host__ __device__ __forceinline__
#else
#define HD inline
#endif

struct V2 { float x, y; };
struct V3 { float x, y, z; };
struct alignas(16) V4 { float x, y, z, w; };

HD V2 v2(float x, float y) { return {x, y}; }
HD V3 v3(float x, float y, float z) { return {x, y, z}; }
HD V3 v3(float s) { return {s, s, s}; }
HD V4 v4(float x, float y, float z, float w) { return {x, y, z, w}; }

HD V2 operator+(V2 a, V2 b) { return {a.x + b.x, a.y + b.y}; }
HD V2 operator-(V2 a, V2 b) { return {a.x - b.x, a.y - b.y}; }
HD V2 operator*(V2 a, float s) { return {a.x * s, a.y * s}; }
HD V2 operator*(float s, V2 a) { return {a.x * s, a.y * s}; }
HD float dot(V2 a, V2 b) { return a.x * b.x + a.y * b.y; }
HD float length(V2 a) { return sqrtf(dot(a, a)); }

HD V3 operator+(V3 a, V3 b) { return {a.x + b.x, a.y + b.y, a.z + b.z}; }
HD V3 operator-(V3 a, V3 b) { return {a.x - b.x, a.y - b.y, a.z - b.z}; }
HD V3 operator-(V3 a) { return {-a.x, -a.y, -a.z}; }
HD V3 operator*(V3 a, V3 b) { return {a.x * b.x, a.y * b.y, a.z * b.z}; }
HD V3 operator/(V3 a, V3 b) { return {a.x / b.x, a.y / b.y, a.z / b.z}; }
HD V3 operator*(V3 a, float s) { return {a.x * s, a.y * s, a.z * s}; }
HD V3 operator*(float s, V3 a) { return {a.x * s, a.y * s, a.z * s}; }
HD V3 operator/(V3 a, float s) { float r = 1.0f / s; return {a.x * r, a.y * r, a.z * r}; }
HD V3& operator+=(V3& a, V3 b) { a.x += b.x; a.y += b.y; a.z += b.z; return a; }
HD V3& operator*=(V3& a, V3 b) { a.x *= b.x; a.y *= b.y; a.z *= b.z; return a; }
HD V3& operator*=(V3& a, float s) { a.x *= s; a.y *= s; a.z *= s; return a; }
HD float dot(V3 a, V3 b) { return a.x * b.x + a.y * b.y + a.z * b.z; }
HD V3 cross(V3 a, V3 b) { return {a.y * b.z - a.z * b.y, a.z * b.x - a.x * b.z, a.x * b.y - a.y * b.x}; }
HD float length(V3 a) { return sqrtf(dot(a, a)); }
HD V3 normalize(V3 a) { return a * (1.0f / sqrtf(fmaxf(dot(a, a), 1e-30f))); }
HD V3 vexp(V3 a) { return {expf(a.x), expf(a.y), expf(a.z)}; }
HD V3 vmax(V3 a, float b) { return {fmaxf(a.x, b), fmaxf(a.y, b), fmaxf(a.z, b)}; }
HD V3 vmin(V3 a, float b) { return {fminf(a.x, b), fminf(a.y, b), fminf(a.z, b)}; }
HD V3 reflect(V3 i, V3 n) { return i - n * (2.0f * dot(i, n)); }
HD float lum(V3 c) { return 0.2126f * c.x + 0.7152f * c.y + 0.0722f * c.z; }

HD float clampf(float x, float a, float b) { return fminf(fmaxf(x, a), b); }
HD float sat(float x) { return clampf(x, 0.0f, 1.0f); }
HD float mixf(float a, float b, float t) { return a + (b - a) * t; }
HD V3 mix(V3 a, V3 b, float t) { return a + (b - a) * t; }
HD float smoothstepf(float a, float b, float x) {
  float t = sat((x - a) / (b - a));
  return t * t * (3.0f - 2.0f * t);
}
HD float fractf(float x) { return x - floorf(x); }

// PCG-style integer hash: a pure function of its inputs, the same on every GPU.
HD uint32_t hash_u32(uint32_t v) {
  uint32_t state = v * 747796405u + 2891336453u;
  uint32_t word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
  return (word >> 22u) ^ word;
}
HD uint32_t hash4(uint32_t a, uint32_t b, uint32_t c, uint32_t d) {
  return hash_u32(a ^ hash_u32(b ^ hash_u32(c ^ hash_u32(d))));
}
HD float u01(uint32_t h) { return (float)(h >> 8) * (1.0f / 16777216.0f); }

struct Rng {
  uint32_t s;
  HD float next() {
    s = hash_u32(s);
    return u01(s);
  }
};
