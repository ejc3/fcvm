// The phone streamer's decisions, kept apart from CUDA, NVENC and the emulator so they are
// tested on any host (test_host.cpp): the scene it keeps and passes to the app, the
// stream's geometry, the stream header, and which screen copies become frames.
#pragma once
#include <stdint.h>

#include <deque>
#include <string>

#include "stamp.h"

struct Rect4 {
  double x = 0, y = 0, w = 1, h = 1;
};

// The parts of POST /scene the streamer reads; the whole body goes to the app.
struct PhoneScene {
  bool set = false;
  double epoch_unix_ms = 0;
  Rect4 rect;
  double image_aspect = 0;            // 0: the surface's own (one phone, no stitching)
  long long layout_generation = -1;   // -1: none
  long long rect_from_frame = -1;     // the first frame (scene time x 30) drawn with `rect`
  bool has_previous = false;
  Rect4 previous;                     // drawn before rect_from_frame
  std::string body;
};
// Parses and checks a scene body; false with the field named in *err.
bool phone_scene_parse(const std::string& body, PhoneScene* out, std::string* err);

// The app's control request for a scene (PROTOCOL.md, "Phone app"): the body as posted,
// with `"type": "scene"` and `"stamp": true` added, as one line.
std::string app_scene_line(const PhoneScene& s);
// The app's `clock` request: the host's time now.
std::string app_clock_line(double unix_ms);
// What one reply line from the app's control socket says went wrong: "" for a JSON object
// with no "error" member, the error's text for one with it, and a description of the line
// for anything else (not JSON, not an object).
std::string app_reply_error(const std::string& line);

// A stamp read from a copy is the app's frame number only when it names a time near the
// copy's: the stamp has no checksum, so an interface that happens to show 32 black or white
// cells along the bottom must not pass for a frame of the scene.
bool stamp_plausible(uint32_t k, double capture_unix_ms, const PhoneScene& sc);
#define STAMP_PLAUSIBLE_S 5.0

// The picture a frame carries (always the whole screen) and where its scene sits in the
// whole image: the rows above the stamp's band.
struct PhoneGeo {
  bool scene = false;  // a stamped scene frame; false: a screen with no stamp
  int w = 0, h = 0;    // encoded picture: the screen (even)
  int crop_bottom = 0;
  int full_w = 0, full_h = 0, x0 = 0, y0 = 0;
  long long layout_generation = -1;
  bool operator==(const PhoneGeo& o) const {
    return scene == o.scene && w == o.w && h == o.h && crop_bottom == o.crop_bottom && full_w == o.full_w &&
           full_h == o.full_h && x0 == o.x0 && y0 == o.y0 && layout_generation == o.layout_generation;
  }
  bool operator!=(const PhoneGeo& o) const { return !(*this == o); }
};
// A scene frame k is placed by the rect the app drew it with: `previous` before
// `rect_from_frame` (on the 1/30 s grid), `rect` from it, the way the app picks (so a frame
// drawn before a re-tile is never labelled with the new place). The tile geometry is the
// app's: full_h = round(screen_h / rect.h), full_w = round(full_h * image_aspect),
// x0 = round(rect.x * full_w), y0 = round(rect.y * full_h). A screen frame is the whole
// screen, an image of its own.
PhoneGeo phone_geometry(bool scene, uint32_t k, int screen_w, int screen_h, const PhoneScene& sc);

// The text header that opens a stream and precedes a change of geometry.
std::string phone_header_json(const PhoneGeo& g, const std::string& codec, int fps, const char* q, uint32_t bitrate);

// Which screen copies become frames. The app draws on a 1/60 s grid; a scene frame is sent
// for every even k (the wall's 1/30 s grid), once. A screen frame at most `fps` times a
// second, stamped with the slot of its copy time.
class FrameGate {
 public:
  explicit FrameGate(int fps) : fps_(fps) {}
  bool admit(bool scene, uint32_t k, double now_ms, const PhoneScene& sc, double* t);
  uint64_t repeats = 0, odd = 0, scene = 0, screen = 0;

 private:
  int fps_;
  bool have_k_ = false;
  uint32_t last_k_ = 0;
  long long last_screen_slot_ = -1;
  double screen_epoch_ = 0;
};

// The last samples of a duration, for /health's percentiles.
class Samples {
 public:
  void add(double ms);
  // "null" without samples, else {"p50":..,"p90":..,"n":..}
  std::string json() const;

 private:
  std::deque<double> v_;
};
