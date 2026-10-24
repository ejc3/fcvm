// Per-frame inputs to the render kernels. Plain data: built on the host from
// the scene and t, copied to constant memory once per frame.
#pragma once
#include "vec.h"

#define MAX_TEXTS 4    // the clock and the newest word (up to two lines)
#define MAX_CHARS 28
#define MAX_LIGHTS 32
#define HQ_COST_PRIOR 1.0  // cost per sample of the high-quality path over the fast path, until one is measured
#define HQ_SPP 8  // samples per drawn pixel from which a frame takes the high-quality path
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

// A message's text_style (words[].text_style); the clock is always type.
#define TEXT_STYLE_LOCKUP 0      // emissive type at depth, upper left, above the clock (the default)
#define TEXT_STYLE_SKYWRITING 1  // smoke puffs written across the sky
#define TEXT_STYLE_LANDMARK 2    // big solid letters standing on a ridge

// GText.style
#define GTEXT_TYPE 0      // emissive type in a plane facing the camera
#define GTEXT_SMOKE 1     // smoke puffs along the glyphs' centrelines
#define GTEXT_LANDMARK 2  // extruded letters standing in the world

struct GText {
  // The left end of the baseline; right and up are unit vectors spanning the
  // text's plane.
  V3 center, right, up;
  float height;          // the em, in km
  float alpha;           // fade in/out
  V3 color;              // type: emitted radiance after exposure; landmark: the dusk edge light
  int style;             // GTEXT_*
  int occlude;           // 1: hidden behind nearer terrain
  int len;
  unsigned char ch[MAX_CHARS];
  float gx[MAX_CHARS];   // each glyph's origin along the baseline, em (kerned)
  float gy[MAX_CHARS];   // each glyph's origin above the baseline, em (landmark letters follow the ground)
  float width;           // the advance of the whole line, em
  float ylo, yhi;        // the glyphs' extent above the baseline, em
  float age;             // smoke: seconds since the message appeared
  float shade;           // type: how strongly the shade behind it holds bright backgrounds down (0..1)
};

// Whether a text's letters shade the terrain (render.cu landmark_vis). The
// hold key covers exactly these texts (scene.cpp build_frame), so the answer
// must not depend on the fade: a landmark casts its whole shadow on every frame
// it is in the scene, including the one it appears on (age 0, alpha 0).
// Otherwise two tiles of one hold, one starting at the appearance frame and one
// a frame later, accumulate different samples under the same key.
HD bool casts_landmark_shadow(const GText& g) { return g.style == GTEXT_LANDMARK; }

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
  int hq;              // 1: the high-quality path (budget >= HQ_SPP samples per drawn pixel; see PROTOCOL.md)
  int half;            // checkerboard stride: 1 every pixel draws, 2 half, 4 a quarter (see draws_here)
  int parity;          // the frame slot k & 3
  int text_overlay;    // 1: draw the clock and words over the image (0 only in tests)
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
