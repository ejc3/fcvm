// The scene contract (fleet/PROTOCOL.md): parsing, camera interpolation, tile
// geometry and the per-frame parameters. Host only, no CUDA.
#pragma once
#include <stdint.h>

#include <string>
#include <vector>

#include "params.h"
#include "vec.h"

struct Keyframe {
  double t;
  double pos[3];
  double look[3];
  double fov_deg;
};
struct SceneWord {
  std::string text;  // uppercased, filtered to the font, at most MAX_CHARS
  double t;
};
struct SceneLight {
  double pos[3];
  double color[3];
  double t;
};
struct Rect {
  double x = 0, y = 0, w = 1, h = 1;
};

struct Scene {
  int64_t epoch_unix_ms = 0;
  Rect rect;
  double image_aspect = 16.0 / 9.0;
  std::vector<Keyframe> camera;  // sorted by t
  double time_of_day = 0.35;
  std::vector<SceneWord> words;    // sorted by t
  std::vector<SceneLight> lights;  // sorted by t
  double loop_s = 0;  // > 0 only for the built-in demo scene: t wraps
};

// Throws std::runtime_error with a message naming the bad field.
Scene parse_scene(const std::string& body);
// The scene a renderer draws before anyone posts one: a looping flight with holds.
Scene default_scene();

struct Tile {
  int full_w, full_h, x0, y0, w, h;
};
// The tile's pixels for a requested height: the whole image is a pixel grid of
// full_w x full_h, and this tile is [x0, x0+w) x [y0, y0+h) of it. w and h are
// even (4:2:0 video), so a tile may overlap its neighbour by one pixel column or
// row; overlapping pixels are the same whole-image pixels and so match.
Tile tile_geometry(const Rect& r, double image_aspect, int requested_h);

struct CamPose {
  double pos[3], look[3], fov_deg;
  bool held;          // inside a hold (two or more equal consecutive keyframes, or outside the keyframes)
  double hold_start;  // t of the hold's first keyframe (stable for the whole hold)
};
// Catmull-Rom on pos and look, linear fov; holds as described in PROTOCOL.md.
CamPose camera_at(const std::vector<Keyframe>& kf, double t);

// Scene time that drives the camera and animation (wraps for the demo loop).
double scene_time(const Scene& s, double t);

// Host copy of the 2D noise table (the GPU gets the same values), used to keep
// the eye above ground and to place words.
struct HostNoise {
  std::vector<V4> tab;  // NOISE_N * NOISE_N
};
const HostNoise& host_noise();
void fill_noise_table(V4* tab);  // deterministic

// Build the kernel parameters for frame time t. `hold_key` receives a value that
// changes whenever the accumulated layer must restart.
FrameParams build_frame(const Scene& s, const Tile& tile, double t, uint64_t* hold_key, bool* held);
GCam make_cam(const double pos[3], const double look[3], double fov_deg, double aspect);
