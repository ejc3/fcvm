#include "stamp.h"

bool stamp_read(const uint8_t* rgba, int width, int height, size_t stride, uint32_t* k) {
  if (width < STAMP_CELLS || height < STAMP_ROWS) return false;
  const uint8_t* row = rgba + (size_t)(height - STAMP_ROWS / 2) * stride;
  uint32_t v = 0;
  for (int i = 0; i < STAMP_CELLS; i++) {
    int x0 = i * width / STAMP_CELLS, x1 = (i + 1) * width / STAMP_CELLS;
    const uint8_t* p = row + (size_t)((x0 + x1) / 2) * 4;
    uint8_t lo = p[0], hi = p[0];
    for (int c = 1; c < 3; c++) {
      if (p[c] < lo) lo = p[c];
      if (p[c] > hi) hi = p[c];
    }
    int bit;
    if (lo >= 160) bit = 1;
    else if (hi <= 96) bit = 0;
    else return false;
    v = v << 1 | (uint32_t)bit;
  }
  *k = v;
  return true;
}

void stamp_write(uint8_t* rgba, int width, int height, size_t stride, uint32_t k) {
  for (int y = height - STAMP_ROWS; y < height; y++) {
    uint8_t* row = rgba + (size_t)y * stride;
    for (int i = 0; i < STAMP_CELLS; i++) {
      uint8_t c = ((k >> (31 - i)) & 1) ? 255 : 0;
      for (int x = i * width / STAMP_CELLS; x < (i + 1) * width / STAMP_CELLS; x++) {
        row[x * 4] = row[x * 4 + 1] = row[x * 4 + 2] = c;
        row[x * 4 + 3] = 255;
      }
    }
  }
}
