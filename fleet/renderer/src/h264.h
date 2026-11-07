// H.264 helpers that need no encoder: what an SPS says about reordering, its codec string,
// and the stream quality names. Shared by the renderer's NVENC encoder and by sources that
// encode another way (the Windows bridge, with x264).
#pragma once
#include <stdint.h>

#include <string>

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

// Quality names for the q parameter; -1 for an unknown name.
int quality_from_name(const std::string& n);
const char* quality_name(int q);

// The codec string for an SPS NAL unit (first byte is the NAL header).
std::string avc_codec_string(const uint8_t* sps, size_t n);
// Find the first SPS (nal_unit_type 7) in an Annex-B stream; returns its offset or -1.
long find_sps(const uint8_t* p, size_t n, size_t* len);
