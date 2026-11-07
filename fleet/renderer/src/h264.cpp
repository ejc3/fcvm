#include "h264.h"

#include <cstdio>
#include <algorithm>
#include <iterator>
#include <vector>

long find_sps(const uint8_t* p, size_t n, size_t* len) {
  for (size_t i = 0; i + 3 < n; i++) {
    size_t sc = 0;
    if (p[i] == 0 && p[i + 1] == 0 && p[i + 2] == 1) sc = 3;
    else if (i + 4 < n && p[i] == 0 && p[i + 1] == 0 && p[i + 2] == 0 && p[i + 3] == 1) sc = 4;
    if (!sc) continue;
    size_t s = i + sc;
    if (s < n && (p[s] & 0x1f) == 7) {
      size_t e = s + 1;
      while (e + 2 < n && !(p[e] == 0 && p[e + 1] == 0 && (p[e + 2] == 1 || (e + 3 < n && p[e + 2] == 0 && p[e + 3] == 1)))) e++;
      if (e + 2 >= n) e = n;
      *len = e - s;
      return (long)s;
    }
    i = s - 1;
  }
  return -1;
}

namespace {
struct Bits {
  std::vector<uint8_t> b;  // RBSP: emulation prevention bytes removed
  size_t pos = 0;
  bool bad = false;
  uint32_t u1() {
    if (pos >= b.size() * 8) { bad = true; return 0; }
    uint32_t v = (b[pos >> 3] >> (7 - (pos & 7))) & 1;
    pos++;
    return v;
  }
  uint32_t u(int n) {
    uint32_t v = 0;
    for (int i = 0; i < n; i++) v = (v << 1) | u1();
    return v;
  }
  uint32_t ue() {
    int lz = 0;
    while (!bad && u1() == 0)
      if (++lz > 31) { bad = true; return 0; }
    return (uint32_t)(((uint64_t)1 << lz) - 1 + u(lz));
  }
  void se() { ue(); }
};

void skip_hrd(Bits& r) {
  uint32_t cpb = r.ue() + 1;
  r.u(8);
  for (uint32_t i = 0; i < cpb && !r.bad; i++) { r.ue(); r.ue(); r.u1(); }
  r.u(20);
}
}  // namespace

// H.264 7.3.2.1.1 and E.1.1, as far as bitstream_restriction.
SpsReorder sps_reorder(const uint8_t* p, size_t n) {
  SpsReorder out;
  Bits r;
  int zeros = 0;
  for (size_t i = 0; i < n; i++) {
    if (zeros >= 2 && p[i] == 3) { zeros = 0; continue; }
    r.b.push_back(p[i]);
    zeros = p[i] == 0 ? zeros + 1 : 0;
  }
  if (r.b.size() < 4 || (r.b[0] & 0x1f) != 7) return out;
  r.u(8);
  uint32_t profile = r.u(8);
  r.u(16);  // constraint flags, level_idc
  r.ue();   // seq_parameter_set_id
  static const uint32_t high[] = {100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135};
  if (std::find(std::begin(high), std::end(high), profile) != std::end(high)) {
    uint32_t chroma = r.ue();
    if (chroma == 3) r.u1();
    r.ue(); r.ue(); r.u1();
    if (r.u1()) {
      for (int i = 0; i < (chroma != 3 ? 8 : 12) && !r.bad; i++) {
        if (!r.u1()) continue;
        int last = 8, next = 8;
        for (int j = 0; j < (i < 6 ? 16 : 64) && !r.bad; j++) {
          if (next != 0) {
            uint32_t k = r.ue();
            int d = (k & 1) ? (int)((k + 1) / 2) : -(int)(k / 2);
            next = ((last + d) % 256 + 256) % 256;
          }
          if (next != 0) last = next;
        }
      }
    }
  }
  r.ue();  // log2_max_frame_num_minus4
  uint32_t poc = r.ue();
  if (poc == 0) r.ue();
  else if (poc == 1) {
    r.u1(); r.se(); r.se();
    uint32_t c = r.ue();
    for (uint32_t i = 0; i < c && !r.bad; i++) r.se();
  }
  out.max_num_ref_frames = r.ue();
  r.u1();
  r.ue(); r.ue();
  if (!r.u1()) r.u1();  // frame_mbs_only_flag, mb_adaptive_frame_field_flag
  r.u1();
  if (r.u1()) { r.ue(); r.ue(); r.ue(); r.ue(); }
  if (r.u1()) {  // vui_parameters_present_flag
    if (r.u1() && r.u(8) == 255) r.u(32);
    if (r.u1()) r.u1();
    if (r.u1()) { r.u(4); if (r.u1()) r.u(24); }
    if (r.u1()) { r.ue(); r.ue(); }
    if (r.u1()) { r.u(32); r.u(32); r.u1(); }
    uint32_t nal_hrd = r.u1();
    if (nal_hrd) skip_hrd(r);
    uint32_t vcl_hrd = r.u1();
    if (vcl_hrd) skip_hrd(r);
    if (nal_hrd || vcl_hrd) r.u1();
    r.u1();  // pic_struct_present_flag
    if (r.u1()) {
      out.restriction = true;
      r.u1(); r.ue(); r.ue(); r.ue(); r.ue();
      out.max_num_reorder_frames = r.ue();
      out.max_dec_frame_buffering = r.ue();
    }
  }
  // what follows must be rbsp_trailing_bits: a 1, then zeros
  if (r.bad || r.u1() != 1) return SpsReorder();
  while (r.pos < r.b.size() * 8)
    if (r.u1()) return SpsReorder();
  out.parsed = true;
  return out;
}

std::string avc_codec_string(const uint8_t* sps, size_t n) {
  if (n < 4) return "avc1.42E01E";
  char b[32];
  snprintf(b, sizeof b, "avc1.%02X%02X%02X", sps[1], sps[2], sps[3]);
  return b;
}

int quality_from_name(const std::string& n) {
  if (n == "lo") return 0;
  if (n == "std") return 1;
  if (n == "hi") return 2;
  return -1;
}
const char* quality_name(int q) { return q == 0 ? "lo" : q == 2 ? "hi" : "std"; }
