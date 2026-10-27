// H.264 encoding with NVENC (NVIDIA Video Codec SDK API, loaded at run time
// from libnvidia-encode.so.1, which the container gets through CDI).
#pragma once
#include <stdint.h>

#include <string>
#include <vector>

// What an SPS NAL unit (first byte the NAL header, emulation prevention bytes
// in place) says about frame reordering. Without a VUI bitstream_restriction a
// decoder must assume the level's MaxDpbFrames: Chromium's H264Decoder then
// holds that many frames (7 or 8 at the wall's tile sizes) before outputting
// one. parsed is false when the bytes are not a whole SPS.
struct SpsReorder {
  bool parsed = false;
  bool restriction = false;  // bitstream_restriction_flag
  uint32_t max_num_ref_frames = 0;
  uint32_t max_num_reorder_frames = 0;
  uint32_t max_dec_frame_buffering = 0;
};
SpsReorder sps_reorder(const uint8_t* sps, size_t n);

class H264Encoder {
 public:
  H264Encoder() = default;
  ~H264Encoder();
  H264Encoder(const H264Encoder&) = delete;
  H264Encoder& operator=(const H264Encoder&) = delete;

  // Encodes w x h NV12 frames read from `nv12` (device memory, `pitch` bytes per
  // row, luma rows then interleaved chroma rows) in the CUDA context `cu_ctx`.
  // q: 0 lo, 1 std, 2 hi (see PROTOCOL.md, "Stream quality").
  bool open(void* cu_ctx, void* nv12, size_t pitch, int w, int h, int fps, int q, std::string* err);
  uint32_t bitrate() const { return rate_; }
  // Smallest frame NVENC accepts for H.264 on this GPU (queried once).
  static bool min_size(void* cu_ctx, int* w, int* h, std::string* err);
  // Encode the frame currently in the input buffer: one Annex-B access unit.
  bool encode(bool force_idr, std::vector<uint8_t>* out, bool* keyframe, std::string* err);
  // RFC 6381 codec string from the SPS, e.g. "avc1.640028".
  const std::string& codec() const { return codec_; }
  // The encoder's SPS as it reads back: open() sets a bitstream_restriction with
  // no reordering and checks that NVENC wrote it.
  const SpsReorder& reorder() const { return reorder_; }
  int width() const { return w_; }
  int height() const { return h_; }

 private:
  void* enc_ = nullptr;
  void* reg_ = nullptr;
  void* bs_ = nullptr;
  int w_ = 0, h_ = 0;
  size_t pitch_ = 0;
  uint64_t n_ = 0;
  std::string codec_;
  SpsReorder reorder_;
  uint32_t rate_ = 0;
};

// Quality names for the q parameter; -1 for an unknown name.
int quality_from_name(const std::string& n);
const char* quality_name(int q);

// The codec string for an SPS NAL unit (first byte is the NAL header).
std::string avc_codec_string(const uint8_t* sps, size_t n);
// Find the first SPS (nal_unit_type 7) in an Annex-B stream; returns its offset or -1.
long find_sps(const uint8_t* p, size_t n, size_t* len);
