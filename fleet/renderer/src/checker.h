// The checkerboard (PROTOCOL.md, "Sampling, accumulation and determinism"):
// which pixels a frame draws at stride 1, 2 or 4, and the thread that traces
// each of them. Shared by the GPU and the host tests.
#pragma once
#include <cstdint>
#include "vec.h"

// Whether whole-image pixel (X, Y) draws on a frame of stride `half` whose
// slot is `parity` (k & 3).
HD bool checker_draws(uint32_t X, uint32_t Y, int half, int parity) {
  if (half <= 1) return true;
  if (half == 2) return ((X + Y + (uint32_t)parity) & 1u) == 0u;
  const uint32_t order[4] = {0u, 3u, 1u, 2u};
  return ((X & 1u) + 2u * (Y & 1u)) == order[parity & 3];
}

// The thread grid that traces a drawn region of ew x eh pixels: every column
// and row at stride 1, half the columns at stride 2, half the columns and the
// rows at stride 4.
HD int checker_threads_w(int ew, int half) { return half > 1 ? (ew + 1) / 2 : ew; }
HD int checker_threads_h(int eh, int half) { return half > 2 ? (eh + 1) / 2 : eh; }

// The drawn-region pixel (i, j) thread (ti, tj) traces, for a region whose
// first pixel is whole-image (ex0, ey0); false for a thread with none. Each
// drawn pixel has exactly one thread. One thread per drawn pixel: launched over
// every pixel, a checkerboard frame ran one warp lane in two or four, and a
// warp takes as long as its traced lanes, so the stride saved little trace time.
HD bool checker_pixel(int ti, int tj, int ex0, int ey0, int ew, int eh, int half, int parity, int* i, int* j) {
  if (half <= 1) {
    *i = ti; *j = tj;
    return ti < ew && tj < eh;
  }
  if (half == 2) {
    if (tj >= eh) return false;
    int s = (int)(((uint32_t)(ex0 + ey0 + tj) + (uint32_t)parity) & 1u);
    *i = 2 * ti + s; *j = tj;
    return *i < ew;
  }
  const uint32_t order[4] = {0u, 3u, 1u, 2u};
  uint32_t o = order[parity & 3];
  int sx = (int)(((o & 1u) + (uint32_t)ex0) & 1u), sy = (int)(((o >> 1) + (uint32_t)ey0) & 1u);
  *i = 2 * ti + sx; *j = 2 * tj + sy;
  return *i < ew && *j < eh;
}
