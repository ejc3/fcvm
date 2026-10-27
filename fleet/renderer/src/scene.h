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
  std::string text;  // printable ASCII, at most MAX_CHARS
  double t;
  int style = 0;     // TEXT_STYLE_*: how this word is drawn (lockup by default)
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
  double budget = 0;  // pixel-samples per frame for every tile; 0: the renderer's BUDGET
  // Layout generations: draw previous_rect for frames before rect_from_frame
  // (when has_previous_rect), rect from it; -1: absent.
  long long layout_generation = -1, rect_from_frame = -1;
  bool has_previous_rect = false;
  Rect previous_rect;
  // The coordinator's admission keys (raw, newest first): with any, GET /frames needs a
  // direct token made with one (admit.h). Empty: no check.
  std::vector<std::string> admit_keys;
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
// row; overlapping pixels are the same whole-image pixels and so match. A tile
// is at most 4096 wide (NVENC H.264): a rect too wide for that at the requested
// height gets a lower height, so the tile still covers the whole rect.
Tile tile_geometry(const Rect& r, double image_aspect, int requested_h);

struct CamPose {
  double pos[3], look[3], fov_deg;
  bool held;          // inside a hold (two or more equal consecutive keyframes, or outside the keyframes)
  double hold_start;  // t of the hold's first keyframe (stable for the whole hold)
};
// Catmull-Rom on pos and look, linear fov; holds as described in PROTOCOL.md.
CamPose camera_at(const std::vector<Keyframe>& kf, double t);

// The scene holding the camera where frame t of s has it, with the clouds and
// water as frame t draws them: frame t's reference, accumulated to many
// samples (fleet-renderer quality, the GPU test's quality check).
Scene held_reference(const Scene& s, double t);
// Scene time that drives the camera and animation (wraps for the demo loop).
double scene_time(const Scene& s, double t);
// The tile height that gives a whole image full_h pixels tall for rect r (the
// stream height to keep when a layout generation changes a tile's rect).
int tile_h_for(const Rect& r, double image_aspect, int full_h);

// The layout generation in force and a pending switch (PROTOCOL.md, "Layout
// generations"). on_scene takes each accepted scene: a new generation becomes
// pending; a scene without one takes effect at once, so it cancels a pending
// switch and leaves no generation in force (take_cleared reports that). take(k) puts a pending switch into
// force at its first frame; true when it did.
struct LayoutSwitch {
  long long cur = -1, next = -1, k = 0;
  bool pending = false;
  bool cleared = false;  // a numbered layout ended without a successor; see take_cleared
  // True once after a generationless scene ended a generation that was in
  // force: open streams hold a header naming it, and with the geometry
  // unchanged nothing else would make the renderer send them a new one.
  bool take_cleared();
  Rect from, to;
  void on_scene(const Scene& sc);
  bool take(long long frame);
};

// Host copy of the 2D noise table (the GPU gets the same values), used to keep
// the eye above ground and to place words.
struct HostNoise {
  std::vector<V4> tab;  // NOISE_N * NOISE_N
};
const HostNoise& host_noise();
void fill_noise_table(V4* tab);  // deterministic

// Build the kernel parameters for frame time t. `hold_key` receives a value that
// changes whenever the accumulated layer must restart.
// "HH:MM:SS PDT" or "HH:MM:SS PST" for a Unix time (the sky clock).
std::string pacific_clock(int64_t unix_s);
FrameParams build_frame(const Scene& s, const Tile& tile, double t, uint64_t* hold_key, bool* held);
GCam make_cam(const double pos[3], const double look[3], double fov_deg, double aspect);
// The flattest high ridge in view for a landmark of len_em em (scene.cpp).
// False when no ray meets land; `at` and `dist` are written either way.
bool find_ridge_cached(const GCam& wc, float asp, float len_em, float em_per_km, V3* at, float* dist);
