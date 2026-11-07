// Robot Park's frame stamp (PROTOCOL.md, "Phone app", `stamp`): the app writes its frame
// number k (t = k / 60) into the bottom 8 rows of its surface, 32 cells across the surface's
// width, most significant bit first, white for 1 and black for 0. Host-only, so it is
// tested without a GPU or an emulator.
#pragma once
#include <stddef.h>
#include <stdint.h>

#define STAMP_ROWS 8
#define STAMP_CELLS 32
#define STAMP_FPS 60
// The stamp's top bit: the app drew the frame in its held layout (a viewer holds the phone):
// the whole screen, its bars lifted above the band, so the band shows below them. The frame
// number is the other 31 bits.
#define STAMP_HELD_BIT 0x80000000u

// Reads the stamp of a top-down RGBA screen (`stride` bytes a row): the middle row of the
// bottom band, the middle pixel of each cell. True and *k set when every cell is clean
// black or white (each channel at most 96, or at least 160); false for anything else (the
// app's interface over the band, a screen with no stamp).
bool stamp_read(const uint8_t* rgba, int width, int height, size_t stride, uint32_t* k);

// Draws the stamp for k into the bottom band, as the app does. For tests.
void stamp_write(uint8_t* rgba, int width, int height, size_t stride, uint32_t k);
