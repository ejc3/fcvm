// RGBA to NV12 (BT.709, limited range, as the renderer's encoder signals in its VUI) on the
// CPU, for the phone streamer. Host-only, so it is tested without a GPU.
#pragma once
#include <stddef.h>
#include <stdint.h>

// Converts `h` rows of `w` RGBA pixels starting at `src` (rows `src_stride` bytes apart, the
// top row first) into NV12 at `dst`: `h` luma rows then h/2 rows of interleaved CbCr,
// `dst_pitch` bytes apart. w and h are even.
void rgba_to_nv12(const uint8_t* src, size_t src_stride, int w, int h, uint8_t* dst, size_t dst_pitch);
