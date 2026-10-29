#include "nv12.h"

// BT.709 limited range, 8-bit, coefficients scaled by 2^16:
//   Y  =  16 + ( 0.1826 R + 0.6142 G + 0.0620 B)
//   Cb = 128 + (-0.1006 R - 0.3386 G + 0.4392 B)
//   Cr = 128 + ( 0.4392 R - 0.3989 G - 0.0403 B)
namespace {
inline uint8_t clamp8(int v) { return (uint8_t)(v < 0 ? 0 : v > 255 ? 255 : v); }
}  // namespace

void rgba_to_nv12(const uint8_t* src, size_t src_stride, int w, int h, uint8_t* dst, size_t dst_pitch) {
  auto row = [&](int y) { return src + (size_t)y * src_stride; };
  for (int y = 0; y < h; y++) {
    const uint8_t* s = row(y);
    uint8_t* d = dst + (size_t)y * dst_pitch;
    for (int x = 0; x < w; x++, s += 4)
      d[x] = clamp8((11966 * s[0] + 40254 * s[1] + 4064 * s[2] + (16 << 16) + 32768) >> 16);
  }
  uint8_t* uv = dst + (size_t)h * dst_pitch;
  for (int y = 0; y < h; y += 2) {
    const uint8_t* a = row(y);
    const uint8_t* b = row(y + 1);
    uint8_t* d = uv + (size_t)(y / 2) * dst_pitch;
    for (int x = 0; x < w; x += 2) {
      // the mean of the 2 x 2 block
      int r = a[x * 4] + a[x * 4 + 4] + b[x * 4] + b[x * 4 + 4];
      int g = a[x * 4 + 1] + a[x * 4 + 5] + b[x * 4 + 1] + b[x * 4 + 5];
      int bl = a[x * 4 + 2] + a[x * 4 + 6] + b[x * 4 + 2] + b[x * 4 + 6];
      d[x] = clamp8(((-6591 * r - 22189 * g + 28780 * bl) / 4 + (128 << 16) + 32768) >> 16);
      d[x + 1] = clamp8(((28780 * r - 26142 * g - 2638 * bl) / 4 + (128 << 16) + 32768) >> 16);
    }
  }
}
