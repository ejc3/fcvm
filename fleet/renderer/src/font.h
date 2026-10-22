// A stroke font: each glyph is a set of line segments on a 4 x 6 cell (y up).
// Text is drawn as the distance to the nearest segment, so it is crisp at any
// size and identical on every GPU.
#pragma once
#include <vector>

#include "vec.h"

#define GLYPH_W 4.0f
#define GLYPH_H 6.0f
#define GLYPH_ADVANCE 5.6f
#define FONT_MAX_SEGMENTS 640

struct FontTable {
  V4 seg[FONT_MAX_SEGMENTS];  // (x0, y0, x1, y1)
  short start[128];
  short count[128];
};

const FontTable& font_table();
bool font_has(unsigned char c);  // printable and drawn (space included)
