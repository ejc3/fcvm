// H.264 encoding with NVENC (NVIDIA Video Codec SDK API, loaded at run time
// from libnvidia-encode.so.1, which the container gets through CDI).
#pragma once
#include <stdint.h>

#include <string>
#include <vector>

class H264Encoder {
 public:
  H264Encoder() = default;
  ~H264Encoder();
  H264Encoder(const H264Encoder&) = delete;
  H264Encoder& operator=(const H264Encoder&) = delete;

  // Encodes w x h NV12 frames read from `nv12` (device memory, `pitch` bytes per
  // row, luma rows then interleaved chroma rows) in the CUDA context `cu_ctx`.
  bool open(void* cu_ctx, void* nv12, size_t pitch, int w, int h, int fps, std::string* err);
  // Smallest frame NVENC accepts for H.264 on this GPU (queried once).
  static bool min_size(void* cu_ctx, int* w, int* h, std::string* err);
  // Encode the frame currently in the input buffer: one Annex-B access unit.
  bool encode(bool force_idr, std::vector<uint8_t>* out, bool* keyframe, std::string* err);
  // RFC 6381 codec string from the SPS, e.g. "avc1.640028".
  const std::string& codec() const { return codec_; }
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
};

// The codec string for an SPS NAL unit (first byte is the NAL header).
std::string avc_codec_string(const uint8_t* sps, size_t n);
// Find the first SPS (nal_unit_type 7) in an Annex-B stream; returns its offset or -1.
long find_sps(const uint8_t* p, size_t n, size_t* len);
