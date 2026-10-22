// Per-frame inputs to the render kernels. Plain data: built on the host from
// the scene and t, copied to constant memory once per frame.
#pragma once
#include "vec.h"

#define MAX_TEXTS 17   // the clock plus the 16 newest words
#define MAX_CHARS 28
#define MAX_LIGHTS 32
#define HOLD_MAX_SAMPLES 1024  // a hold stops adding samples here
#define HOLD_BLEND_OUT 256     // motion history fades out of a hold by this sample count
#define TAA_MAX_HISTORY 16     // the last moving output weighs at most this many samples in a hold's blend-in
#define TEMPORAL_WINDOW 8      // a moving frame averages itself and its 7 predecessors
#define TEMPORAL_FRAME_CAP 64  // a previous frame weighs at most this many samples
#define GUARD_FRAC 0.015f      // reprojection reach: 1.5% of the whole image's height
#define DENOISE_R 2            // the finishing filter's radius, pixels
#define CLOUD_TILE_KM 14.0f    // the cloud volume repeats every this many km

struct GCam {
  V3 pos, fwd, right, up;
  float tan_half;  // tan(vertical fov / 2)
  float aspect;    // whole-image width / height
};

struct GText {
  V3 center, right, up;  // billboard frame; right and up are unit vectors
  float height;          // glyph cell height in km
  float alpha;           // fade in/out
  V3 color;              // emitted radiance
  int len;
  unsigned char ch[MAX_CHARS];
};

struct GLight {
  V3 pos;
  V3 color;  // emitted radiance of the sphere
  float radius;
  float alpha;  // fade-in, for the emissive sphere only
};

struct FrameParams {
  GCam cam;
  int full_w, full_h;  // whole image in pixels
  int x0, y0, w, h;    // this tile in whole-image pixels
  int ex0, ey0, ew, eh;  // the drawn region: the tile plus a guard band, clipped to the image
  int rx0, ry0, rw, rh;  // the resolved region: the out region plus DENOISE_R, clipped to the image
  int ox0, oy0, ow, oh;  // the out region: the tile plus the stream margin, clipped to the image
  int guard;             // largest reprojection displacement accepted, whole-image pixels
  GCam hist_cam[TEMPORAL_WINDOW];  // cameras of frames k-1, k-2, ... (moving reprojection)
  int spp;             // samples per pixel this frame
  int hold;            // 1: camera held, exact accumulation; 0: moving, reprojection
  int sample_base;     // hold: index of the first sample drawn this frame
  uint32_t seed;       // hold: the hold's seed; moving: the frame's seed
  float layer_t;       // scene time of the accumulated layer (clouds, water)
  V3 sun_dir;          // unit vector toward the sun
  V3 sun_e;            // solar irradiance above the atmosphere
  float exposure;
  V2 wind;             // cloud offset in km, already reduced modulo the noise period
  float wave_t;        // water animation phase, reduced modulo its period
  int n_text;
  GText text[MAX_TEXTS];
  int n_lights;        // lights lit in the accumulated layer
  GLight lights[MAX_LIGHTS];
};
