#include "jpeg.h"

#include <algorithm>
#include <cmath>
#include <cstring>
#include <vector>

namespace {
// Annex K tables
const uint8_t kLumQ[64] = {16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57,
                           69, 56, 14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64,
                           81, 104, 113, 92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99};
const uint8_t kChrQ[64] = {17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99,
                           99, 99, 47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
                           99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99};
const uint8_t kZigzag[64] = {0,  1,  8,  16, 9,  2,  3,  10, 17, 24, 32, 25, 18, 11, 4,  5,  12, 19, 26, 33, 40, 48,
                             41, 34, 27, 20, 13, 6,  7,  14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23,
                             30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63};
const uint8_t kDcLumBits[16] = {0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0};
const uint8_t kDcLumVal[12] = {0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11};
const uint8_t kDcChrBits[16] = {0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0};
const uint8_t kDcChrVal[12] = {0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11};
const uint8_t kAcLumBits[16] = {0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d};
const uint8_t kAcLumVal[162] = {
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14,
    0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09,
    0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a,
    0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65,
    0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88,
    0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9,
    0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca,
    0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea,
    0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa};
const uint8_t kAcChrBits[16] = {0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77};
const uint8_t kAcChrVal[162] = {
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71, 0x13, 0x22, 0x32,
    0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16,
    0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39,
    0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64,
    0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86,
    0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8,
    0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9,
    0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa};

struct Huff {
  uint16_t code[256];
  uint8_t len[256];
};
Huff build(const uint8_t bits[16], const uint8_t* vals) {
  Huff h;
  memset(&h, 0, sizeof h);
  int k = 0;
  uint16_t code = 0;
  for (int l = 1; l <= 16; l++) {
    for (int i = 0; i < bits[l - 1]; i++, k++) {
      h.code[vals[k]] = code++;
      h.len[vals[k]] = (uint8_t)l;
    }
    code <<= 1;
  }
  return h;
}

struct Bits {
  std::string out;
  uint32_t acc = 0;
  int n = 0;
  void put(uint32_t v, int len) {
    acc = (acc << len) | (v & ((1u << len) - 1));
    n += len;
    while (n >= 8) {
      uint8_t b = (uint8_t)(acc >> (n - 8));
      out += (char)b;
      if (b == 0xFF) out += (char)0;  // byte stuffing
      n -= 8;
    }
  }
  void flush() {
    if (n > 0) put(0x7F, 7);  // pad with ones
    n = 0;
  }
};

void fdct(float* d) {  // separable 8x8 DCT-II, scaled so DC = 8 * mean
  float tmp[64];
  for (int u = 0; u < 8; u++)
    for (int y = 0; y < 8; y++) {
      float s = 0;
      for (int x = 0; x < 8; x++) s += d[y * 8 + x] * cosf((2 * x + 1) * u * (float)M_PI / 16);
      tmp[y * 8 + u] = s * (u == 0 ? (float)M_SQRT1_2 : 1.0f) * 0.5f;
    }
  for (int v = 0; v < 8; v++)
    for (int u = 0; u < 8; u++) {
      float s = 0;
      for (int y = 0; y < 8; y++) s += tmp[y * 8 + u] * cosf((2 * y + 1) * v * (float)M_PI / 16);
      d[v * 8 + u] = s * (v == 0 ? (float)M_SQRT1_2 : 1.0f) * 0.5f;
    }
}

int category(int v) {
  int a = v < 0 ? -v : v, c = 0;
  while (a) { c++; a >>= 1; }
  return c;
}

void block(Bits& b, float* d, const uint8_t* q, int* prev_dc, const Huff& dc, const Huff& ac) {
  fdct(d);
  int z[64];
  for (int i = 0; i < 64; i++) z[i] = (int)lrintf(d[kZigzag[i]] / q[kZigzag[i]]);
  int diff = z[0] - *prev_dc;
  *prev_dc = z[0];
  int c = category(diff);
  b.put(dc.code[c], dc.len[c]);
  if (c) b.put(diff < 0 ? diff - 1 : diff, c);
  int run = 0;
  for (int i = 1; i < 64; i++) {
    if (z[i] == 0) { run++; continue; }
    while (run > 15) { b.put(ac.code[0xF0], ac.len[0xF0]); run -= 16; }
    int cc = category(z[i]);
    int sym = (run << 4) | cc;
    b.put(ac.code[sym], ac.len[sym]);
    b.put(z[i] < 0 ? z[i] - 1 : z[i], cc);
    run = 0;
  }
  if (run) b.put(ac.code[0], ac.len[0]);  // end of block
}

void seg(std::string& o, uint8_t marker, const std::string& body) {
  o += (char)0xFF;
  o += (char)marker;
  uint16_t n = (uint16_t)(body.size() + 2);
  o += (char)(n >> 8);
  o += (char)(n & 0xFF);
  o += body;
}
}  // namespace

std::string jpeg_encode(const uint8_t* rgba, int w, int h, int quality) {
  quality = std::clamp(quality, 1, 100);
  int scale = quality < 50 ? 5000 / quality : 200 - 2 * quality;
  uint8_t ql[64], qc[64];
  for (int i = 0; i < 64; i++) {
    ql[i] = (uint8_t)std::clamp((kLumQ[i] * scale + 50) / 100, 1, 255);
    qc[i] = (uint8_t)std::clamp((kChrQ[i] * scale + 50) / 100, 1, 255);
  }
  static const Huff dcl = build(kDcLumBits, kDcLumVal), dcc = build(kDcChrBits, kDcChrVal);
  static const Huff acl = build(kAcLumBits, kAcLumVal), acc = build(kAcChrBits, kAcChrVal);
  std::string o;
  o += (char)0xFF;
  o += (char)0xD8;  // SOI
  seg(o, 0xE0, std::string("JFIF\0\x01\x01\0\0\x01\0\x01\0\0", 14));
  std::string dqt;
  dqt += (char)0;
  for (int i = 0; i < 64; i++) dqt += (char)ql[kZigzag[i]];
  dqt += (char)1;
  for (int i = 0; i < 64; i++) dqt += (char)qc[kZigzag[i]];
  seg(o, 0xDB, dqt);
  std::string sof;
  sof += (char)8;
  sof += (char)(h >> 8); sof += (char)(h & 0xFF);
  sof += (char)(w >> 8); sof += (char)(w & 0xFF);
  sof += (char)3;
  sof += (char)1; sof += (char)0x22; sof += (char)0;  // Y, 2x2 sampling
  sof += (char)2; sof += (char)0x11; sof += (char)1;
  sof += (char)3; sof += (char)0x11; sof += (char)1;
  seg(o, 0xC0, sof);
  auto dht = [&](int cls, int id, const uint8_t bits[16], const uint8_t* vals, int nvals) {
    std::string b;
    b += (char)((cls << 4) | id);
    for (int i = 0; i < 16; i++) b += (char)bits[i];
    for (int i = 0; i < nvals; i++) b += (char)vals[i];
    seg(o, 0xC4, b);
  };
  dht(0, 0, kDcLumBits, kDcLumVal, 12);
  dht(1, 0, kAcLumBits, kAcLumVal, 162);
  dht(0, 1, kDcChrBits, kDcChrVal, 12);
  dht(1, 1, kAcChrBits, kAcChrVal, 162);
  seg(o, 0xDA, std::string("\x03\x01\x00\x02\x11\x03\x11\x00\x3f\x00", 10));
  Bits b;
  int pdy = 0, pdb = 0, pdr = 0;
  auto px = [&](int x, int y, int c) {
    x = std::min(x, w - 1);
    y = std::min(y, h - 1);
    return (float)rgba[4 * ((size_t)y * w + x) + c];
  };
  for (int my = 0; my < h; my += 16)
    for (int mx = 0; mx < w; mx += 16) {
      float Y[4][64], Cb[64], Cr[64];
      for (int i = 0; i < 64; i++) { Cb[i] = 0; Cr[i] = 0; }
      for (int y = 0; y < 16; y++)
        for (int x = 0; x < 16; x++) {
          float r = px(mx + x, my + y, 0), g = px(mx + x, my + y, 1), bl = px(mx + x, my + y, 2);
          int blk = (y / 8) * 2 + x / 8;
          Y[blk][(y % 8) * 8 + (x % 8)] = 0.299f * r + 0.587f * g + 0.114f * bl - 128.0f;
          int ci = (y / 2) * 8 + x / 2;
          Cb[ci] += 0.25f * (-0.168736f * r - 0.331264f * g + 0.5f * bl);
          Cr[ci] += 0.25f * (0.5f * r - 0.418688f * g - 0.081312f * bl);
        }
      for (int k = 0; k < 4; k++) block(b, Y[k], ql, &pdy, dcl, acl);
      block(b, Cb, qc, &pdb, dcc, acc);
      block(b, Cr, qc, &pdr, dcc, acc);
    }
  b.flush();
  o += b.out;
  o += (char)0xFF;
  o += (char)0xD9;  // EOI
  return o;
}
