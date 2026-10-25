#include "proto.h"

#include <cstring>

void fcvf_header(uint8_t o[FCVF_HEADER_BYTES], bool keyframe, int w, int h, double t, uint32_t frame) {
  o[0] = 'F'; o[1] = 'C'; o[2] = 'V'; o[3] = 'F';
  o[4] = 1;
  o[5] = keyframe ? FCVF_FLAG_KEYFRAME : 0;
  o[6] = (uint8_t)(w & 0xff); o[7] = (uint8_t)((w >> 8) & 0xff);
  o[8] = (uint8_t)(h & 0xff); o[9] = (uint8_t)((h >> 8) & 0xff);
  uint64_t tb;
  memcpy(&tb, &t, 8);
  for (int i = 0; i < 8; i++) o[10 + i] = (uint8_t)(tb >> (8 * i));
  for (int i = 0; i < 4; i++) o[18 + i] = (uint8_t)(frame >> (8 * i));
}

bool fcvf_parse(const uint8_t* p, size_t n, FcvfHeader* out) {
  if (n < FCVF_HEADER_BYTES || memcmp(p, "FCVF", 4) != 0 || p[4] != 1) return false;
  out->keyframe = (p[5] & FCVF_FLAG_KEYFRAME) != 0;
  out->w = p[6] | (p[7] << 8);
  out->h = p[8] | (p[9] << 8);
  uint64_t tb = 0;
  for (int i = 0; i < 8; i++) tb |= (uint64_t)p[10 + i] << (8 * i);
  memcpy(&out->t, &tb, 8);
  out->frame = 0;
  for (int i = 0; i < 4; i++) out->frame |= (uint32_t)p[18 + i] << (8 * i);
  return true;
}

std::string sha1_raw(const std::string& data) {
  uint32_t h[5] = {0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0};
  std::string m = data;
  uint64_t bits = (uint64_t)data.size() * 8;
  m += (char)0x80;
  while (m.size() % 64 != 56) m += (char)0;
  for (int i = 7; i >= 0; i--) m += (char)(bits >> (8 * i));
  auto rol = [](uint32_t x, int c) { return (x << c) | (x >> (32 - c)); };
  for (size_t off = 0; off < m.size(); off += 64) {
    uint32_t w[80];
    for (int i = 0; i < 16; i++)
      w[i] = (uint32_t)(uint8_t)m[off + 4 * i] << 24 | (uint32_t)(uint8_t)m[off + 4 * i + 1] << 16 |
             (uint32_t)(uint8_t)m[off + 4 * i + 2] << 8 | (uint32_t)(uint8_t)m[off + 4 * i + 3];
    for (int i = 16; i < 80; i++) w[i] = rol(w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16], 1);
    uint32_t a = h[0], b = h[1], c = h[2], d = h[3], e = h[4];
    for (int i = 0; i < 80; i++) {
      uint32_t f, k;
      if (i < 20) { f = (b & c) | (~b & d); k = 0x5A827999; }
      else if (i < 40) { f = b ^ c ^ d; k = 0x6ED9EBA1; }
      else if (i < 60) { f = (b & c) | (b & d) | (c & d); k = 0x8F1BBCDC; }
      else { f = b ^ c ^ d; k = 0xCA62C1D6; }
      uint32_t tmp = rol(a, 5) + f + e + k + w[i];
      e = d; d = c; c = rol(b, 30); b = a; a = tmp;
    }
    h[0] += a; h[1] += b; h[2] += c; h[3] += d; h[4] += e;
  }
  std::string out;
  for (uint32_t v : h)
    for (int i = 3; i >= 0; i--) out += (char)(v >> (8 * i));
  return out;
}

std::string base64(const std::string& d) {
  static const char* T = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  std::string o;
  size_t i = 0;
  for (; i + 2 < d.size(); i += 3) {
    uint32_t v = (uint8_t)d[i] << 16 | (uint8_t)d[i + 1] << 8 | (uint8_t)d[i + 2];
    o += T[v >> 18]; o += T[(v >> 12) & 63]; o += T[(v >> 6) & 63]; o += T[v & 63];
  }
  if (i + 1 == d.size()) {
    uint32_t v = (uint8_t)d[i] << 16;
    o += T[v >> 18]; o += T[(v >> 12) & 63]; o += "==";
  } else if (i + 2 == d.size()) {
    uint32_t v = (uint8_t)d[i] << 16 | (uint8_t)d[i + 1] << 8;
    o += T[v >> 18]; o += T[(v >> 12) & 63]; o += T[(v >> 6) & 63]; o += '=';
  }
  return o;
}

std::string ws_accept(const std::string& key) {
  return base64(sha1_raw(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"));
}

std::string ws_frame(int opcode, const void* data, size_t n) {
  std::string f;
  f += (char)(0x80 | (opcode & 0x0f));
  if (n < 126) {
    f += (char)n;
  } else if (n < 65536) {
    f += (char)126;
    f += (char)(n >> 8);
    f += (char)(n & 0xff);
  } else {
    f += (char)127;
    for (int i = 7; i >= 0; i--) f += (char)((uint64_t)n >> (8 * i));
  }
  f.append((const char*)data, n);
  return f;
}
