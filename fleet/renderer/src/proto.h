// Wire formats: the FCVF frame header, WebSocket framing and handshake.
#pragma once
#include <stdint.h>

#include <string>
#include <vector>

#define FCVF_HEADER_BYTES 22
#define FCVF_FLAG_KEYFRAME 0x01

// bytes 0-3 "FCVF", 4 version 1, 5 flags, 6-7 width u16 LE, 8-9 height u16 LE,
// 10-17 t f64 LE, 18-21 frame number u32 LE.
void fcvf_header(uint8_t out[FCVF_HEADER_BYTES], bool keyframe, int w, int h, double t, uint32_t frame);
struct FcvfHeader {
  bool keyframe;
  int w, h;
  double t;
  uint32_t frame;
};
bool fcvf_parse(const uint8_t* p, size_t n, FcvfHeader* out);

std::string sha1_raw(const std::string& data);  // 20 bytes
std::string base64(const std::string& data);
std::string ws_accept(const std::string& client_key);
// One unmasked server frame (FIN set) with opcode 1 text, 2 binary, 8 close, 10 pong.
std::string ws_frame(int opcode, const void* data, size_t n);
