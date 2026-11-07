// H.264 encoding of the desktop with x264 on the host's CPU (the host's GPUs belong to the
// wall). The stream is what the page's WebCodecs decoder and the renderer's protocol need:
// Annex B, one slice, no B-frames, SPS and PPS before every keyframe, a VUI with BT.709
// limited range and a bitstream_restriction saying nothing is reordered.
#pragma once
#include <stdint.h>

#include <string>
#include <vector>

#include "h264.h"

typedef struct x264_t x264_t;

class X264Encoder {
 public:
  X264Encoder() = default;
  ~X264Encoder();
  X264Encoder(const X264Encoder&) = delete;
  X264Encoder& operator=(const X264Encoder&) = delete;

  // w x h I420 pictures (w and h even) at `fps`; q 0 lo, 1 std, 2 hi (PROTOCOL.md "Stream
  // quality"). Fails when x264's SPS lacks the bitstream_restriction the page needs.
  bool open(int w, int h, int fps, int q, std::string* err);
  // One Annex-B access unit for the picture in y/u/v (strides w, w/2, w/2). force_idr asks
  // for a keyframe; *keyframe says whether this is one (an IDR).
  bool encode(const uint8_t* y, const uint8_t* u, const uint8_t* v, bool force_idr, std::vector<uint8_t>* out,
              bool* keyframe, std::string* err);
  const std::string& codec() const { return codec_; }
  uint32_t bitrate() const { return rate_; }
  const SpsReorder& reorder() const { return reorder_; }
  int width() const { return w_; }
  int height() const { return h_; }

 private:
  x264_t* enc_ = nullptr;
  int w_ = 0, h_ = 0;
  int64_t pts_ = 0;
  uint32_t rate_ = 0;
  std::string codec_;
  SpsReorder reorder_;
};
