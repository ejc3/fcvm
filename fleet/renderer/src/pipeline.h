// The render loop and its subscribers: one frame per 1/fps slot of the shared
// clock, drawn once at the largest height any subscriber wants, scaled and
// encoded once per height that has a subscriber.
#pragma once
#include <stdint.h>

#include <atomic>
#include <climits>
#include <chrono>
#include <condition_variable>
#include <deque>
#include <functional>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#include "budget.h"
#include "gpustats.h"

// how long one GET /still keeps the renderer drawing every slot at its height
#define STILL_HOLD_S 30
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
  double budget = 2.5e5;  // pixel-samples per frame (drawn pixels x samples) until something sets one
  // A renderer drawing the whole image (no neighbours to match) adapts its own
  // budget to its fit. Tiles of a grid never do: they take the budget the
  // controller posts to all of them (POST /budget), so they agree at every slot.
  bool adapt = true;
  int idle_h = 144;
  bool accumulate = true;
  // Pixels encoded past each side of the tile (inside the image); the client
  // crops them, so no tile edge is an edge of an encoded picture.
  int margin = 16;
  // Test hook: scales the reported budget_fit, as a slower (< 1) or faster GPU
  // would. Never set in production.
  double fit_scale = 1.0;
};

class Pipeline {
 public:
  explicit Pipeline(const Options& o);
  ~Pipeline();
  // Process start, as CLOCK_BOOTTIME seconds (for start-to-first-frame timing).
  void set_process_start(double boottime_s) { proc_start_ = boottime_s; }
  void run();  // the render thread; returns only on a fatal error
  void stop();

  std::shared_ptr<Subscriber> subscribe(int h, int q);
  void unsubscribe(const std::shared_ptr<Subscriber>& s);
  void wake();
  bool post_scene(const std::string& body, std::string* err);
  // POST /budget: {"budget": B, "t": T}, B pixel-samples per frame from the
  // first slot at or after scene time T.
  bool post_budget(const std::string& body, std::string* err);
  // Debug: the unencoded tile of the frame at time t (a future frame slot), as
  // a binary PPM. Waits for that frame; false if this renderer skipped it.
  bool raw_frame(double t, std::string* ppm, std::string* tile_json);
  // The next frame drawn, unencoded (RGBA, w x h), for GET /still. While
  // nothing streams, the renderer then draws every slot as for a stream of
  // tile height `still_h` (the `h` of GET /frames) for STILL_HOLD_S, so a hold
  // converges between requests; the frame returned is drawn that way. While a stream runs, the frame is
  // the stream's.
  bool next_frame(int still_h, std::vector<uint8_t>* rgba, int* w, int* h, std::string* tile_json);
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
    bool opening = false;  // an encoder for open_geo is being opened on the aux thread
    Tile open_geo{};
    bool failed = false;   // (an aux open's result) the encoder did not open
    std::vector<std::shared_ptr<Subscriber>> subs;
  };
  static constexpr long long kOpenNow = -2;
  void open_async(std::shared_ptr<std::vector<std::pair<int, Enc>>> list, long long gen);
  std::condition_variable min_cv_;
  bool min_known_ = false;     // render thread
  long long primed_through_ = LLONG_MIN;  // render thread: the last slot drawn ahead by this frame
  static constexpr long long PRIME_MAX = 90;  // slots a tile draws ahead at most when it primes
  static constexpr int WARM_S = 10;  // seconds a posted scene keeps the renderer drawing every slot
  std::chrono::steady_clock::time_point warm_until_{};  // under mu_
  bool had_streams_ = false;  // render thread: the last frame drawn had a stream
  int warm_h_ = 540;  // render thread: the stream height to warm at (the last one streamed)
  void frame_once(double t, uint32_t frame_no);
  void set_fatal(const std::string& m);
  // overlap and encoded size of an encoder for tile g
  void enc_layout(Enc& e, const Tile& g) const;
  // Work kept off the render thread: tearing an encoder down (40 to 250 ms
  // measured on a T4) and opening the encoders of a pending layout ahead of its
  // first frame, so a re-tile skips no slot (a skipped slot leaves a tile
  // without that frame in its history, and its moving frames stop matching
  // its neighbours' for 8 frames).
  void aux_loop();
  void aux_post(std::function<void()> f);
  void retire(Enc& e);
  std::thread aux_;
  std::mutex aux_mu_;
  std::condition_variable aux_cv_;
  std::deque<std::function<void()>> aux_q_;
  bool aux_stop_ = false;
  // encoders opened ahead for the pending layout, by encoder key (under aux_mu_)
  std::map<int, Enc> prepared_;
  long long prepared_gen_ = -1;  // render thread: the generation prepared for
  long long prep_open_gen_ = -1;  // under aux_mu_: the generation whose encoders may still be added
  void prepare_layout(const std::shared_ptr<const Scene>& sc, const std::vector<std::pair<int, int>>& hq, long long gen);
  Tile encode_geometry(const Scene& sc, int h);
  int min_w_ = 0, min_h_ = 0;  // under aux_mu_ until min_known_
  std::string header_json(const Enc& e, int q, long long gen);
  struct RawReq {
    long long k;  // the frame slot wanted; LLONG_MIN: the next frame drawn
    int want_h = 0;  // with no stream, only a frame drawn for this tile height (0: any)
    bool done = false;
    bool ok = false;
    std::vector<uint8_t> rgba;
    int w = 0, h = 0;
    std::string tile;
  };
  int still_h_ = 0;                                 // under mu_
  std::chrono::steady_clock::time_point still_until_{};  // under mu_
  bool capture(long long k, int want_h, std::vector<uint8_t>* rgba, int* w, int* h, std::string* tile_json);
  std::vector<std::shared_ptr<RawReq>> raw_reqs_;  // under mu_
  std::condition_variable raw_cv_;

  Options opt_;
  Renderer r_;
  GpuStats gpu_;
  std::mutex mu_;
  std::condition_variable cv_;
  std::shared_ptr<const Scene> scene_;
  // layout generations: the scene with previous_rect, drawn before
  // rect_from_frame; the generation in effect and a pending switch (under mu_)
  std::shared_ptr<const Scene> scene_prev_;
  LayoutSwitch lay_;  // under mu_ (post_scene writes it)
  uint64_t scene_version_ = 0;
  std::vector<std::shared_ptr<Subscriber>> subs_;
  int next_id_ = 1;
  std::map<int, Enc> encs_;
  std::atomic<bool> stop_{false};
  std::atomic<bool> ready_{false};
  std::string fatal_;
  double proc_start_ = 0;
  double budget_ = 1.0e6;  // a whole-image renderer's own adapted budget
  BudgetSchedule sched_;   // under mu_
  // health: the budget the last frame used, where it came from, and this
  // renderer's fit (budget at 70% of a slot, smoothed)
  double used_budget_ = 0, fit_ = 0, fit_now_ = 0, fit_px_ = 0, fit_scale_ = 1.0;
  CostModel cost_;
  struct FrameLog {
    std::chrono::steady_clock::time_point at;
    double frame_ms, render_ms, encode_ms;
  };
  std::deque<FrameLog> frame_log_;  // under hmu_
  const char* budget_source_ = "default";
  // health
  mutable std::mutex hmu_;
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
  // fatal_, gpu_name_ and the startup_ms fields are written by the render
  // thread and read by health_json and fatal() on server threads: all under hmu_
};

double now_unix_ms();
double boottime_s();
double process_start_boottime_s();
