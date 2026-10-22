// The render loop and its subscribers: one frame per 1/fps slot of the shared
// clock, drawn once at the largest height any subscriber wants, scaled and
// encoded once per height that has a subscriber.
#pragma once
#include <stdint.h>

#include <atomic>
#include <condition_variable>
#include <deque>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

#include "gpustats.h"
#include "nvenc.h"
#include "render.h"
#include "scene.h"

struct Subscriber {
  int id = 0;
  std::atomic<int> want_h{0};
  std::atomic<int> want_q{1};  // 0 lo, 1 std, 2 hi
  // filled by the render thread, drained by the connection's writer
  std::mutex mu;
  std::condition_variable cv;
  std::deque<std::shared_ptr<const std::string>> q;  // complete WebSocket frames
  size_t queued = 0;
  bool closed = false;
  // render-thread state
  int cur_h = 0;  // encoder key the header was sent for (0: header pending)
  bool need_key = true;
  void push(std::shared_ptr<const std::string> m);
  void close();
};

struct Options {
  int port = 8080;
  int fps = 30;
  double budget = 2.5e5;  // pixel-samples per frame (drawn pixels x samples)
  // Adjust the budget so a frame's GPU time fits its slot. Off by default:
  // with it, tiles of one image draw different sample counts, and moving
  // frames stop matching across tile edges.
  bool adapt = false;
  int idle_h = 144;
  bool accumulate = true;
  // Pixels encoded past each side of the tile (inside the image); the client
  // crops them, so no tile edge is an edge of an encoded picture.
  int margin = 16;
};

class Pipeline {
 public:
  explicit Pipeline(const Options& o);
  // Process start, as CLOCK_BOOTTIME seconds (for start-to-first-frame timing).
  void set_process_start(double boottime_s) { proc_start_ = boottime_s; }
  void run();  // the render thread; returns only on a fatal error
  void stop();

  std::shared_ptr<Subscriber> subscribe(int h, int q);
  void unsubscribe(const std::shared_ptr<Subscriber>& s);
  void wake();
  bool post_scene(const std::string& body, std::string* err);
  // Debug: the unencoded tile of the frame at time t (a future frame slot), as
  // a binary PPM. Waits for that frame; false if this renderer skipped it.
  bool raw_frame(double t, std::string* ppm, std::string* tile_json);
  std::string health_json();
  bool ready() const { return ready_; }
  std::string fatal() const;

 private:
  struct Enc {
    std::unique_ptr<H264Encoder> enc;
    void* nv12 = nullptr;
    size_t pitch = 0;
    Tile geo{};           // the inner rectangle (the tile)
    int left = 0, top = 0, right = 0, bottom = 0;  // overlap encoded around it
    int ew = 0, eh = 0;   // encoded size
    double last_idr = -1e9;
    bool want_idr = true;
    std::vector<std::shared_ptr<Subscriber>> subs;
  };
  void frame_once(double t, uint32_t frame_no);
  Tile encode_geometry(const Scene& sc, int h);
  int min_w_ = 0, min_h_ = 0;
  std::string header_json(const Enc& e, int q);
  struct RawReq {
    long long k;
    bool done = false;
    bool ok = false;
    std::string ppm, tile;
  };
  std::vector<std::shared_ptr<RawReq>> raw_reqs_;  // under mu_
  std::condition_variable raw_cv_;

  Options opt_;
  Renderer r_;
  GpuStats gpu_;
  std::mutex mu_;
  std::condition_variable cv_;
  std::shared_ptr<const Scene> scene_;
  uint64_t scene_version_ = 0;
  std::vector<std::shared_ptr<Subscriber>> subs_;
  int next_id_ = 1;
  std::map<int, Enc> encs_;
  std::atomic<bool> stop_{false};
  std::atomic<bool> ready_{false};
  std::string fatal_;
  double proc_start_ = 0;
  double budget_ = 1.0e6;  // current pixel-samples per frame
  // health
  std::mutex hmu_;
  double last_t_ = 0;
  uint32_t frame_ = 0;
  double fps_ = 0;
  std::vector<int> heights_;
  std::vector<std::pair<int, int>> streams_;  // (height, quality) being encoded
  RenderStats rstats_;
  double encode_ms_ = 0;
  uint64_t slots_missed_ = 0;  // frame slots skipped while someone was subscribed
  double startup_ready_ms_ = -1, first_frame_ms_ = -1, first_sent_ms_ = -1;
  std::string gpu_name_;
};

double now_unix_ms();
double boottime_s();
double process_start_boottime_s();
