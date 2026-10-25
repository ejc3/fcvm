// A small baseline JPEG encoder (JFIF, YCbCr 4:2:0, the example Huffman
// tables of ITU-T T.81 Annex K), for GET /still.
#pragma once
#include <stdint.h>

#include <string>

// rgba: w*h*4 bytes, row-major. quality 1..100.
std::string jpeg_encode(const uint8_t* rgba, int w, int h, int quality);
