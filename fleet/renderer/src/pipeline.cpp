#include "pipeline.h"

#include <sys/resource.h>
#include <time.h>
#include <unistd.h>

#include <algorithm>
#include <chrono>
#include <climits>
#include <cmath>
#include <cstdio>
#include <fstream>
#include <sstream>

#include "json.h"
#include "proto.h"

static bool rect_eq(const Rect& a, const Rect& b) { return a.x == b.x && a.y == b.y && a.w == b.w && a.h == b.h; }
static bool geo_eq(const Tile& a, const Tile& b) {
  return a.w == b.w && a.h == b.h && a.full_w == b.full_w && a.full_h == b.full_h && a.x0 == b.x0 && a.y0 == b.y0;
}
static int even_h(int h) { return std::clamp(h, 16, 2160) & ~1; }

double now_unix_ms() {
  timespec ts;
  clock_gettime(CLOCK_REALTIME, &ts);
  return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}
double boottime_s() {
  timespec ts;
  clock_gettime(CLOCK_BOOTTIME, &ts);
  return ts.tv_sec + ts.tv_nsec / 1e9;
}
static double mono_s() {
  timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec + ts.tv_nsec / 1e9;
}
double process_start_boottime_s() {
  std::ifstream f("/proc/self/stat");
  std::string s((std::istreambuf_iterator<char>(f)), std::istreambuf_iterator<char>());
  size_t p = s.rfind(')');
  if (p == std::string::npos) return boottime_s();
  std::istringstream is(s.substr(p + 2));
  std::string field;
  // fields after the command: state is field 3; starttime is field 22
  for (int i = 3; i <= 22; i++) {
    if (!(is >> field)) return boottime_s();
  }
  return std::stod(field) / (double)sysconf(_SC_CLK_TCK);
}

void Subscriber::push(std::shared_ptr<const std::string> m) {
  std::lock_guard<std::mutex> l(mu);
  q.push_back(m);
  queued += m->size();
  cv.notify_one();
}
void Subscriber::close() {
  std::lock_guard<std::mutex> l(mu);
  closed = true;
  cv.notify_all();
}

Pipeline::Pipeline(const Options& o) : opt_(o) {
  scene_ = std::make_shared<const Scene>(default_scene());
  budget_ = o.budget;
  fit_scale_ = o.fit_scale;
  r_.set_fps(o.fps);
  r_.set_margin(o.margin);
}

Pipeline::~Pipeline() {
  {
    std::lock_guard<std::mutex> l(aux_mu_);
    aux_stop_ = true;
  }
  aux_cv_.notify_all();
  if (aux_.joinable()) aux_.join();
}

void Pipeline::aux_post(std::function<void()> f) {
  {
    std::lock_guard<std::mutex> l(aux_mu_);
    aux_q_.push_back(std::move(f));
  }
  aux_cv_.notify_all();
}

void Pipeline::aux_loop() {
  Renderer::bind_thread();
  {
    // NVENC's smallest frame: opening a session to ask takes about 250 ms
    int w = 16, h = 16;
    std::string err;
    if (!H264Encoder::min_size(r_.cu_context(), &w, &h, &err)) w = h = 16;
    fprintf(stderr, "fleet-renderer: NVENC H.264 minimum frame %dx%d\n", w, h);
    std::lock_guard<std::mutex> l(aux_mu_);
    min_w_ = w;
    min_h_ = h;
  }
  min_cv_.notify_all();
  for (;;) {
    std::function<void()> f;
    {
      std::unique_lock<std::mutex> l(aux_mu_);
      aux_cv_.wait(l, [&] { return aux_stop_ || !aux_q_.empty(); });
      if (aux_q_.empty()) return;
      f = std::move(aux_q_.front());
      aux_q_.pop_front();
    }
    f();
  }
}

// hand an encoder to the aux thread to tear down (the encoder first, then its
// input buffer, which it has registered)
void Pipeline::retire(Enc& e) {
  std::shared_ptr<H264Encoder> enc(std::move(e.enc));
  void* nv12 = e.nv12;
  e.nv12 = nullptr;
  if (!enc && !nv12) return;
  Renderer* r = &r_;
  aux_post([enc, nv12, r]() mutable {
    enc.reset();
    if (nv12) r->free_dev(nv12);
  });
}

// Opens, on the aux thread, the encoders the streams will need from a pending
// layout's first frame: (height, quality) per stream at the new layout.
void Pipeline::prepare_layout(const std::shared_ptr<const Scene>& sc, const std::vector<std::pair<int, int>>& hq,
                              long long gen) {
  auto list = std::make_shared<std::vector<std::pair<int, Enc>>>();
  for (auto [h, q] : hq) {
    int key = even_h(h) * 4 + q;
    bool dup = false;
    for (auto& p : *list) dup = dup || p.first == key;
    if (dup) continue;
    Enc e;
    enc_layout(e, encode_geometry(*sc, even_h(h)));
    list->emplace_back(key, std::move(e));
  }
  {
    std::lock_guard<std::mutex> l(aux_mu_);
    prep_open_gen_ = gen;
  }
  open_async(list, gen);
}

// Opens encoders on the aux thread and hands them over in prepared_, by key.
// gen >= 0: for a pending layout, dropped once that layout is in effect.
// gen == kOpenNow: for a stream that is waiting now; a failure is handed over
// too (an entry with failed set), so the stream is closed rather than left
// waiting.
void Pipeline::open_async(std::shared_ptr<std::vector<std::pair<int, Enc>>> list, long long gen) {
  void* ctx = r_.cu_context();
  Renderer* r = &r_;
  int fps = opt_.fps;
  aux_post([this, list, ctx, r, fps, gen]() {
    for (auto& [key, e] : *list) {
      Enc old;
      try {
        e.nv12 = r->alloc_nv12(e.ew, e.eh, &e.pitch);
        e.enc = std::make_unique<H264Encoder>();
        std::string err;
        if (!e.enc->open(ctx, e.nv12, e.pitch, e.ew, e.eh, fps, key % 4, &err)) {
          fprintf(stderr, "fleet-renderer: encoder %dx%d: %s\n", e.ew, e.eh, err.c_str());
          e.enc.reset();
          e.failed = true;
        }
      } catch (const std::exception& ex) {
        fprintf(stderr, "fleet-renderer: encoder %dx%d: %s\n", e.ew, e.eh, ex.what());
        e.enc.reset();
        e.failed = true;
      }
      {
        std::lock_guard<std::mutex> l(aux_mu_);
        if ((e.enc || (e.failed && gen == kOpenNow)) && (gen == kOpenNow || prep_open_gen_ == gen)) {
          auto it = prepared_.find(key);
          if (it != prepared_.end()) old = std::move(it->second);
          prepared_[key] = std::move(e);
          e.nv12 = nullptr;  // handed over with the encoder
        }
      }
      // whatever was not handed over (a layout already in effect, an older
      // encoder for the same key) is torn down here
      e.enc.reset();
      if (e.nv12) r->free_dev(e.nv12);
      old.enc.reset();
      if (old.nv12) r->free_dev(old.nv12);
    }
  });
}

void Pipeline::enc_layout(Enc& e, const Tile& g) const {
  // overlap: up to `margin` pixels on each side that lie inside the image,
  // trimmed by one where needed to keep the encoded size even (4:2:0)
  int M = opt_.margin;
  int l = std::min(M, g.x0), r = std::max(0, std::min(M, g.full_w - (g.x0 + g.w)));
  int tp = std::min(M, g.y0), b = std::max(0, std::min(M, g.full_h - (g.y0 + g.h)));
  auto even = [M](int* a, int* c) {
    if ((*a + *c) & 1) {
      if (*c > 0 && *c < M) (*c)--;
      else if (*a > 0) (*a)--;
      else (*c)--;
    }
  };
  even(&l, &r);
  even(&tp, &b);
  e.geo = g;
  e.left = l; e.right = r; e.top = tp; e.bottom = b;
  e.ew = g.w + l + r; e.eh = g.h + tp + b;
}

void Pipeline::stop() {
  stop_ = true;
  cv_.notify_all();
}

std::string Pipeline::fatal() const { return fatal_; }

std::shared_ptr<Subscriber> Pipeline::subscribe(int h, int q) {
  auto s = std::make_shared<Subscriber>();
  s->want_h = h;
  s->want_q = q;
  {
    std::lock_guard<std::mutex> l(mu_);
    s->id = next_id_++;
    subs_.push_back(s);
  }
  cv_.notify_all();
  return s;
}

void Pipeline::unsubscribe(const std::shared_ptr<Subscriber>& s) {
  std::lock_guard<std::mutex> l(mu_);
  subs_.erase(std::remove(subs_.begin(), subs_.end(), s), subs_.end());
}

void Pipeline::wake() { cv_.notify_all(); }

bool Pipeline::post_scene(const std::string& body, std::string* err) {
  try {
    auto sc = std::make_shared<const Scene>(parse_scene(body));
    std::shared_ptr<const Scene> prev;
    if (sc->has_previous_rect) {
      auto p = std::make_shared<Scene>(*sc);
      p->rect = sc->previous_rect;
      prev = p;
    }
    long long k_last;
    {
      std::lock_guard<std::mutex> h(hmu_);
      k_last = std::llround(last_t_ * opt_.fps);
    }
    std::lock_guard<std::mutex> l(mu_);
    bool same_clock = scene_->epoch_unix_ms == sc->epoch_unix_ms;
    scene_ = sc;
    scene_prev_ = prev;
    // a new layout generation takes effect at its rect_from_frame (at once
    // without one): every renderer starts its history there, so the tiles of
    // the new layout draw from the same frames whether or not they moved
    if (sc->layout_generation >= 0 && sc->layout_generation != gen_cur_ &&
        !(switch_pending_ && gen_next_ == sc->layout_generation)) {
      switch_pending_ = true;
      gen_next_ = sc->layout_generation;
      switch_k_ = sc->rect_from_frame >= 0 ? sc->rect_from_frame : 0;
      switch_from_ = sc->has_previous_rect ? sc->previous_rect : sc->rect;
      switch_to_ = sc->rect;
    }
    scene_version_++;
    sched_ = schedule_after_scene(sched_, same_clock, sc->budget > 0, k_last);
  } catch (const std::exception& e) {
    *err = e.what();
    return false;
  }
  cv_.notify_all();
  return true;
}

bool Pipeline::post_budget(const std::string& body, std::string* err) {
  double b, t;
  try {
    Json j = Json::parse(body);
    b = j.at("budget").number("budget");
    t = j.at("t").number("t");
  } catch (const std::exception& e) {
    *err = e.what();
    return false;
  }
  if (!(b >= 1e4 && b <= 5e8)) { *err = "'budget' must be in 10000..500000000"; return false; }
  if (!std::isfinite(t)) { *err = "'t' must be a number"; return false; }
  {
    std::lock_guard<std::mutex> l(mu_);
    sched_.add((long long)std::ceil(t * opt_.fps - 1e-6), b);
  }
  return true;
}

// encoders are keyed by height and quality; ordered by height first
static int enc_key(const Subscriber& s) { return even_h(s.want_h) * 4 + std::clamp(s.want_q.load(), 0, 2); }

std::string Pipeline::header_json(const Enc& e, int q) {
  char b[448];
  snprintf(b, sizeof b,
           "{\"codec\":\"%s\",\"width\":%d,\"height\":%d,\"full_width\":%d,\"full_height\":%d,\"x0\":%d,\"y0\":%d,\"fps\":%d,"
           "\"crop\":{\"left\":%d,\"top\":%d,\"right\":%d,\"bottom\":%d},\"q\":\"%s\",\"bitrate\":%u,\"layout_generation\":%s}",
           e.enc->codec().c_str(), e.ew, e.eh, e.geo.full_w, e.geo.full_h, e.geo.x0, e.geo.y0, opt_.fps, e.left, e.top,
           e.right, e.bottom, quality_name(q), e.enc->bitrate(), gen_cur_ >= 0 ? std::to_string(gen_cur_).c_str() : "null");
  return b;
}

void Pipeline::run() {
  std::string err;
  if (!r_.init(&err)) {
    fatal_ = "GPU init failed: " + err;
    fprintf(stderr, "fleet-renderer: %s\n", fatal_.c_str());
    return;
  }
  gpu_name_ = r_.gpu_name();
  gpu_.start(r_.pci_bus_id());
  startup_ready_ms_ = (boottime_s() - proc_start_) * 1000.0;
  fprintf(stderr, "fleet-renderer: GPU %s (%s) ready %.0f ms after process start\n", gpu_name_.c_str(),
          r_.pci_bus_id().c_str(), startup_ready_ms_);
  ready_ = true;
  aux_ = std::thread([this] { aux_loop(); });
  long long last_k = LLONG_MIN;
  uint64_t seen_version = 0;
  uint32_t frame_no = 0;
  int window_frames = 0;
  double window_start = mono_s();
  while (!stop_) {
    std::shared_ptr<const Scene> sc;
    bool any, paced;
    {
      std::unique_lock<std::mutex> l(mu_);
      sc = scene_;
      any = !subs_.empty();
      // a recent GET /still keeps the renderer drawing every slot
      paced = any || std::chrono::steady_clock::now() < still_until_;
      if (scene_version_ != seen_version) { seen_version = scene_version_; last_k = LLONG_MIN; }
    }
    double now = now_unix_ms();
    double tcur = (now - (double)sc->epoch_unix_ms) / 1000.0;
    long long k = (long long)std::floor(tcur * opt_.fps);
    if (paced) {
      k += 1;  // the next slot
      if (k <= last_k) k = last_k + 1;
      double slot_ms = (double)sc->epoch_unix_ms + k * 1000.0 / opt_.fps;
      double wait = slot_ms - now_unix_ms();
      if (wait > 0) {
        std::unique_lock<std::mutex> l(mu_);
        cv_.wait_for(l, std::chrono::microseconds((long long)(wait * 1000)), [&] { return stop_.load(); });
      }
    } else if (last_k != LLONG_MIN) {
      // nobody is watching: draw twice a second so /health stays current
      std::unique_lock<std::mutex> l(mu_);
      cv_.wait_for(l, std::chrono::milliseconds(500), [&] {
        return stop_.load() || !subs_.empty() || scene_version_ != seen_version ||
               std::chrono::steady_clock::now() < still_until_;
      });
      if (!subs_.empty() || scene_version_ != seen_version || std::chrono::steady_clock::now() < still_until_) continue;
      k = std::max(k, last_k + 1);
    }
    double t = (double)k / opt_.fps;
    if (any && last_k != LLONG_MIN && k > last_k + 1) {
      std::lock_guard<std::mutex> l(hmu_);
      slots_missed_ += (uint64_t)(k - last_k - 1);
    }
    try {
      auto f0 = std::chrono::steady_clock::now();
      frame_once(t, frame_no);
      double fms = std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - f0).count();
      std::lock_guard<std::mutex> l(hmu_);
      frame_log_.push_back({std::chrono::steady_clock::now(), fms, rstats_.render_ms, encode_ms_});
      while (frame_log_.size() > 300) frame_log_.pop_front();
    } catch (const std::exception& e) {
      fatal_ = std::string("frame failed: ") + e.what();
      fprintf(stderr, "fleet-renderer: %s\n", fatal_.c_str());
      return;
    }
    frame_no++;
    last_k = std::max(k, primed_through_);  // slots drawn ahead are not drawn again
    window_frames++;
    double m = mono_s();
    if (m - window_start >= 1.0) {
      std::lock_guard<std::mutex> l(hmu_);
      fps_ = window_frames / (m - window_start);
      window_frames = 0;
      window_start = m;
    }
  }
}

// The stream's size for a requested height: the tile at that height, or, when
// NVENC's minimum frame is larger, the smallest larger height it accepts (the
// header says which; the client scales).
Tile Pipeline::encode_geometry(const Scene& sc, int h) {
  if (!min_known_) {  // asked on the aux thread at start; waits only in the first moments
    std::unique_lock<std::mutex> l(aux_mu_);
    min_cv_.wait(l, [&] { return min_w_ > 0; });
    min_known_ = true;
  }
  Tile g = tile_geometry(sc.rect, sc.image_aspect, h);
  while ((g.w < min_w_ || g.h < min_h_) && h < 2160) {
    h += 2;
    g = tile_geometry(sc.rect, sc.image_aspect, h);
  }
  return g;
}

void Pipeline::frame_once(double t, uint32_t frame_no) {
  std::vector<std::shared_ptr<Subscriber>> subs;
  std::shared_ptr<const Scene> sc;
  bool new_layout = false;
  std::vector<std::pair<int, int>> prep;  // a pending layout's streams: (height, quality)
  std::shared_ptr<const Scene> prep_scene;
  long long prep_gen = -1;
  long long pend_k = LLONG_MAX;  // the first frame of a pending layout
  primed_through_ = LLONG_MIN;
  {
    std::lock_guard<std::mutex> l(mu_);
    subs = subs_;
    sc = scene_;
    long long k = std::llround(t * opt_.fps);
    if (scene_prev_ && k < sc->rect_from_frame) sc = scene_prev_;  // the old layout until its last frame
    if (switch_pending_ && k >= switch_k_) {
      switch_pending_ = false;
      gen_cur_ = gen_next_;
      new_layout = true;
      // keep each stream's whole image as tall as it was: a stream's height
      // is its tile's, and the tile's share of the image may have changed
      bool moved = !rect_eq(switch_from_, switch_to_);
      for (auto& s : subs) {
        if (moved) {
          int full = tile_geometry(switch_from_, sc->image_aspect, s->want_h).full_h;
          s->want_h = tile_h_for(switch_to_, sc->image_aspect, full);
        }
        s->cur_h = 0;  // a new header (layout_generation) at this frame
        s->need_key = true;
      }
    }
    if (switch_pending_) pend_k = switch_k_;
    // open the pending layout's encoders ahead of its first frame
    if (switch_pending_ && prepared_gen_ != gen_next_ && !subs.empty()) {
      prepared_gen_ = gen_next_;
      if (!rect_eq(switch_from_, switch_to_)) {
        prep_scene = scene_;
        prep_gen = gen_next_;
        for (auto& s : subs)
          prep.push_back({tile_h_for(switch_to_, scene_->image_aspect,
                                     tile_geometry(switch_from_, scene_->image_aspect, s->want_h).full_h),
                          std::clamp(s->want_q.load(), 0, 2)});
      }
    }
  }
  if (!prep.empty()) prepare_layout(prep_scene, prep, prep_gen);
  double sw0 = mono_s();
  if (new_layout) r_.reset_history();
  // (re)assign subscribers to encoders by height; drop closed ones
  for (auto& [h, e] : encs_) {
    e.subs.erase(std::remove_if(e.subs.begin(), e.subs.end(),
                                [&](const std::shared_ptr<Subscriber>& s) {
                                  return std::find(subs.begin(), subs.end(), s) == subs.end() || enc_key(*s) != h;
                                }),
                 e.subs.end());
  }
  for (auto& s : subs) {
    Enc& e = encs_[enc_key(*s)];
    if (std::find(e.subs.begin(), e.subs.end(), s) == e.subs.end()) {
      e.subs.push_back(s);
      s->cur_h = 0;  // header pending
      s->need_key = true;
      e.want_idr = true;
    }
  }
  for (auto it = encs_.begin(); it != encs_.end();) {
    if (it->second.subs.empty()) {
      retire(it->second);
      it = encs_.erase(it);
    } else {
      ++it;
    }
  }
  int render_h = opt_.idle_h;
  {
    std::lock_guard<std::mutex> l(mu_);
    if (std::chrono::steady_clock::now() < still_until_) render_h = encode_geometry(*sc, still_h_).h;
  }
  if (!encs_.empty()) render_h = encode_geometry(*sc, encs_.rbegin()->first / 4).h;
  Tile rt = tile_geometry(sc->rect, sc->image_aspect, render_h);
  double px = r_.drawn_pixels(rt);
  // The budget for this slot: a posted schedule first (the same on every tile
  // of a grid), then the scene's, then a whole-image renderer's own, then the
  // default.
  bool whole = sc->rect.x == 0 && sc->rect.y == 0 && sc->rect.w == 1 && sc->rect.h == 1;
  auto budget_at = [&](long long kk, const char** src) {
    double b = opt_.budget;
    *src = "default";
    std::lock_guard<std::mutex> l(mu_);
    double v;
    if (sched_.at(kk, &v)) { b = v; *src = "schedule"; }
    else if (sc->budget > 0) { b = sc->budget; *src = "scene"; }
    else if (opt_.adapt && whole) { b = budget_; *src = "local"; }
    return b;
  };
  const char* source;
  long long k0 = std::llround(t * opt_.fps);
  // A stream's first frames. A moving frame averages the 7 frames before it,
  // so a tile that starts drawing at a new size (a stream starts, or changes
  // height) draws this slot and the next ones now, unsent, and its first frame
  // sent has all 7 behind it: it no longer depends on when this tile's stream
  // started, and matches a neighbour whose stream started at another slot. A
  // layout switch needs none of this (every tile starts its history at the
  // switch frame), nor does a whole-image renderer (it has no neighbours).
  bool prime = opt_.accumulate && !whole && !encs_.empty() && !new_layout && !r_.same_tile(rt);
  // Priming draws consecutive slots back to back from this one until at least
  // 7 are drawn and the next begins a slot or more from now, so no slot after the
  // priming is skipped either (a GPU clocking up from idle draws the first
  // frames slower than a slot). It stops after PRIME_MAX slots regardless.
  double budget;
  if (prime) {
    double p0 = mono_s();
    long long jn = k0;
    while (jn < pend_k) {  // never past a pending switch
      budget = budget_at(jn, &source);
      r_.set_budget(budget);
      r_.render(*sc, rt, (double)jn / opt_.fps, true);
      jn++;
      long long now_k = (long long)std::floor((now_unix_ms() - (double)sc->epoch_unix_ms) / 1000.0 * opt_.fps);
      // the next slot must not have begun by the time the loop picks it (one slot of margin)
      if (jn - k0 >= TEMPORAL_WINDOW - 1 && jn > now_k + 1) break;
      if (jn - k0 >= PRIME_MAX) break;
    }
    primed_through_ = jn - 1;
    fprintf(stderr, "fleet-renderer: %dx%d tile primed with slots %lld..%lld in %.1f ms\n", rt.w, rt.h, k0, jn - 1,
            (mono_s() - p0) * 1000.0);
  } else {
    budget = budget_at(k0, &source);
    r_.set_budget(budget);
    r_.render(*sc, rt, t, opt_.accumulate);
  }
  if (!prime) {
    // this renderer's fit, from its cost model (the fixed part of a frame and
    // the cost per sample of each path, from the frames it drew)
    const RenderStats& rs = r_.stats();
    std::lock_guard<std::mutex> l(hmu_);
    // a model belongs to one drawn size: a new size starts it again
    if (std::fabs(px - fit_px_) > 0.01 * px) { cost_ = CostModel(); cost_.hq_prior = HQ_COST_PRIOR; fit_px_ = px; budget_ = opt_.budget; }
    if (rs.spp > 0) cost_.observe(rs.hq, px / rs.half * rs.spp, rs.trace_ms, rs.render_ms);
    fit_ = cost_.fit(px, HQ_SPP, 1000.0 / opt_.fps) * fit_scale_;
    used_budget_ = budget;
    budget_source_ = source;
    // a whole-image renderer alone follows its own fit, on the ladder
    if (opt_.adapt && whole && fit_ > 0) budget_ = local_budget(fit_, px);
  }
  if (!prime) {
    // debug captures of this frame
    long long k = std::llround(t * opt_.fps);
    std::lock_guard<std::mutex> l(mu_);
    bool any = false;
    for (auto& rq : raw_reqs_) {
      bool next = rq->k == LLONG_MIN;
      if (rq->done || (!next && rq->k > k)) continue;
      if (next && rq->want_h > 0 && encs_.empty() && render_h != encode_geometry(*sc, rq->want_h).h)
        continue;  // drawn at another height
      rq->done = true;
      any = true;
      if (!next && rq->k < k) continue;  // that slot was skipped
      r_.download_rgba(&rq->rgba);
      rq->w = rt.w;
      rq->h = rt.h;
      char tj[384];
      snprintf(tj, sizeof tj,
               "{\"t\":%.17g,\"x0\":%d,\"y0\":%d,\"width\":%d,\"height\":%d,\"full_width\":%d,\"full_height\":%d,"
               "\"spp\":%d,\"checkerboard\":%d,\"hq\":%d,\"budget\":%.0f,\"budget_source\":\"%s\",\"hold\":%s,\"hold_samples\":%d,"
               "\"layout_generation\":%s}",
               t, rt.x0, rt.y0, rt.w, rt.h, rt.full_w, rt.full_h, r_.stats().spp, r_.stats().half, r_.stats().hq ? 1 : 0,
               budget, source, r_.stats().hold ? "true" : "false", r_.stats().hold_samples,
               gen_cur_ >= 0 ? std::to_string(gen_cur_).c_str() : "null");
      rq->tile = tj;
      rq->ok = true;
    }
    if (any) raw_cv_.notify_all();
  }
  double since_start = (boottime_s() - proc_start_) * 1000.0;
  if (first_frame_ms_ < 0) {
    first_frame_ms_ = since_start;
    fprintf(stderr, "fleet-renderer: first frame drawn %.0f ms after process start (%dx%d, %.1f ms)\n", first_frame_ms_,
            rt.w, rt.h, r_.stats().render_ms);
  }
  double enc_ms = 0;
  std::vector<int> heights;
  std::vector<std::pair<int, int>> streams;
  for (auto& [ekey, e] : encs_) {
    int h = ekey / 4, q = ekey % 4;
    if (heights.empty() || heights.back() != h) heights.push_back(h);
    streams.push_back({h, q});
    Tile g = encode_geometry(*sc, h);
    if (new_layout) e.want_idr = true;  // the first frame of a layout generation is a keyframe
    bool same = e.enc && geo_eq(g, e.geo);
    bool continue_encode = false;
    if (!same) {
      retire(e);
      Enc ready;
      {
        std::lock_guard<std::mutex> l(aux_mu_);
        auto pit = prepared_.find(ekey);
        if (pit != prepared_.end() && (pit->second.enc || pit->second.failed) && geo_eq(pit->second.geo, g)) {
          ready = std::move(pit->second);
          prepared_.erase(pit);
        }
      }
      if (ready.failed) {
        for (auto& s : e.subs) s->close();
        e.opening = false;
        continue;
      }
      if (ready.enc) {
        e.opening = false;
        e.enc = std::move(ready.enc);
        e.nv12 = ready.nv12;
        e.pitch = ready.pitch;
        e.geo = ready.geo;
        e.left = ready.left; e.right = ready.right; e.top = ready.top; e.bottom = ready.bottom;
        e.ew = ready.ew; e.eh = ready.eh;
        for (auto& s : e.subs) s->cur_h = 0;
        e.want_idr = true;
        continue_encode = true;
      }
    }
    if (!same && !continue_encode) {
      // open it on the aux thread (10 to 25 ms, more for the first one); the
      // stream waits for it, and frames keep being drawn meanwhile
      if (!e.opening || !geo_eq(e.open_geo, g)) {
        auto list = std::make_shared<std::vector<std::pair<int, Enc>>>();
        Enc ne;
        enc_layout(ne, g);
        list->emplace_back(ekey, std::move(ne));
        open_async(list, kOpenNow);
        e.opening = true;
        e.open_geo = g;
      }
      continue;
    }
    if (prime) continue;  // nothing is sent while a tile primes its history
    std::string hdr = header_json(e, q);
    auto hmsg = std::make_shared<const std::string>(ws_frame(1, hdr.data(), hdr.size()));
    for (auto& s : e.subs) {
      if (s->cur_h != ekey) {
        s->push(hmsg);
        s->cur_h = ekey;
        s->need_key = true;
      }
    }
    double t0 = mono_s();
    r_.to_nv12(e.nv12, e.pitch, e.ew, e.eh, g.x0 - e.left, g.y0 - e.top, g.full_h);
    bool force = e.want_idr || (t0 - e.last_idr) >= 2.0;
    std::vector<uint8_t> au;
    bool key = false;
    std::string err;
    if (!e.enc->encode(force, &au, &key, &err)) {
      fprintf(stderr, "fleet-renderer: encode %dx%d: %s\n", e.ew, e.eh, err.c_str());
      e.want_idr = true;
      continue;
    }
    enc_ms += (mono_s() - t0) * 1000.0;
    if (key) { e.last_idr = t0; e.want_idr = false; }
    std::string payload(FCVF_HEADER_BYTES, '\0');
    fcvf_header((uint8_t*)&payload[0], key, e.ew, e.eh, t, frame_no);
    payload.append((const char*)au.data(), au.size());
    auto msg = std::make_shared<const std::string>(ws_frame(2, payload.data(), payload.size()));
    for (auto& s : e.subs) {
      if (s->need_key && !key) continue;
      bool overflow;
      {
        std::lock_guard<std::mutex> l(s->mu);
        overflow = s->q.size() > 90 || s->queued > (16u << 20);
        if (overflow) { s->q.clear(); s->queued = 0; }
      }
      if (overflow) {  // a slow reader restarts at the next keyframe
        s->need_key = true;
        e.want_idr = true;
        continue;
      }
      s->need_key = false;
      s->push(msg);
      if (first_sent_ms_ < 0) {
        first_sent_ms_ = (boottime_s() - proc_start_) * 1000.0;
        fprintf(stderr, "fleet-renderer: first frame sent %.0f ms after process start (%dx%d)\n", first_sent_ms_, g.w, g.h);
      }
    }
  }
  if (new_layout)
    fprintf(stderr, "fleet-renderer: layout generation %lld from frame %lld (that frame took %.1f ms)\n", gen_cur_,
            std::llround(t * opt_.fps), (mono_s() - sw0) * 1000.0);
  {
    // encoders opened ahead and not taken: once no layout is pending, any that
    // no waiting stream asked for are torn down
    bool pending;
    {
      std::lock_guard<std::mutex> l(mu_);
      pending = switch_pending_;
    }
    std::vector<Enc> left;
    {
      std::lock_guard<std::mutex> l(aux_mu_);
      if (new_layout) prep_open_gen_ = -1;
      if (!pending) {
        for (auto it = prepared_.begin(); it != prepared_.end();) {
          auto w = encs_.find(it->first);
          if (w != encs_.end() && w->second.opening && geo_eq(w->second.open_geo, it->second.geo)) {
            ++it;
          } else {
            left.push_back(std::move(it->second));
            it = prepared_.erase(it);
          }
        }
      }
    }
    for (auto& e : left) retire(e);
  }
  std::lock_guard<std::mutex> l(hmu_);
  last_t_ = t;
  frame_ = frame_no;
  heights_ = heights;
  streams_ = streams;
  rstats_ = r_.stats();
  encode_ms_ = enc_ms;
}

bool Pipeline::capture(long long k, int want_h, std::vector<uint8_t>* rgba, int* w, int* h, std::string* tile_json) {
  auto r = std::make_shared<RawReq>();
  r->k = k;
  r->want_h = want_h;
  std::unique_lock<std::mutex> l(mu_);
  if (want_h > 0) {
    still_h_ = want_h;
    still_until_ = std::chrono::steady_clock::now() + std::chrono::seconds(STILL_HOLD_S);
  }
  raw_reqs_.push_back(r);
  cv_.notify_all();  // an idle render loop draws sooner
  raw_cv_.wait_for(l, std::chrono::seconds(15), [&] { return r->done; });
  raw_reqs_.erase(std::remove(raw_reqs_.begin(), raw_reqs_.end(), r), raw_reqs_.end());
  if (!r->ok) return false;
  *rgba = std::move(r->rgba);
  *w = r->w;
  *h = r->h;
  *tile_json = r->tile;
  return true;
}

bool Pipeline::raw_frame(double t, std::string* ppm, std::string* tile_json) {
  std::vector<uint8_t> px;
  int w, h;
  if (!capture(std::llround(t * opt_.fps), 0, &px, &w, &h, tile_json)) return false;
  char hd[64];
  snprintf(hd, sizeof hd, "P6\n%d %d\n255\n", w, h);
  *ppm = hd;
  ppm->reserve(ppm->size() + px.size() / 4 * 3);
  for (size_t i = 0; i < px.size(); i += 4) ppm->append((const char*)&px[i], 3);
  return true;
}

bool Pipeline::next_frame(int still_h, std::vector<uint8_t>* rgba, int* w, int* h, std::string* tile_json) {
  return capture(LLONG_MIN, still_h, rgba, w, h, tile_json);
}

std::string Pipeline::health_json() {
  GpuSample g = gpu_.latest();
  long long gcur, gnext, gk;
  bool gpend;
  {
    std::lock_guard<std::mutex> l(mu_);  // never held together with hmu_
    gcur = gen_cur_; gnext = gen_next_; gk = switch_k_; gpend = switch_pending_;
  }
  std::lock_guard<std::mutex> l(hmu_);
  std::ostringstream o;
  o.precision(6);
  o << "{\"fps\":" << std::round(fps_ * 10) / 10 << ",\"heights\":[";
  for (size_t i = 0; i < heights_.size(); i++) o << (i ? "," : "") << heights_[i];
  o << "],\"streams\":[";
  for (size_t i = 0; i < streams_.size(); i++)
    o << (i ? "," : "") << "{\"h\":" << streams_[i].first << ",\"q\":\"" << quality_name(streams_[i].second) << "\"}";
  o << "],\"gpu\":{\"name\":\"" << json_escape(g.ok ? g.name : gpu_name_) << "\"";
  auto num = [&](const char* k, double v) {
    o << ",\"" << k << "\":";
    if (v < 0) o << "null"; else o << std::round(v * 10) / 10;
  };
  num("temp_c", g.temp_c);
  num("power_w", g.power_w);
  num("util_pct", g.util_pct);
  num("mem_used_mb", g.mem_used_mb);
  if (!g.ok && !g.error.empty()) o << ",\"error\":\"" << json_escape(g.error) << "\"";
  o << "},";
  o.precision(15);
  o << "\"t\":" << last_t_ << ",\"frame\":" << frame_;
  o.precision(6);
  o << ",\"ready\":" << (ready_ ? "true" : "false");
  if (!fatal_.empty()) o << ",\"error\":\"" << json_escape(fatal_) << "\"";
  o << ",\"render_ms\":" << std::round(rstats_.render_ms * 10) / 10 << ",\"encode_ms\":" << std::round(encode_ms_ * 10) / 10
    << ",\"spp\":" << rstats_.spp << ",\"checkerboard\":" << rstats_.half << ",\"hold\":" << (rstats_.hold ? "true" : "false")
    << ",\"hold_samples\":" << rstats_.hold_samples << ",\"slots_missed\":" << slots_missed_;
  {
    // the last 2 s of frames: whole frame (render, encode, send) and its parts
    auto now = std::chrono::steady_clock::now();
    std::vector<double> f;
    double rmax = 0, emax = 0;
    for (auto& e : frame_log_)
      if (now - e.at < std::chrono::seconds(2)) { f.push_back(e.frame_ms); rmax = std::max(rmax, e.render_ms); emax = std::max(emax, e.encode_ms); }
    std::sort(f.begin(), f.end());
    auto pct = [&](double q) { return f.empty() ? 0.0 : f[std::min(f.size() - 1, (size_t)(q * f.size()))]; };
    o << ",\"frames_2s\":{\"n\":" << f.size() << ",\"frame_ms_p50\":" << std::round(pct(0.5) * 10) / 10
      << ",\"frame_ms_p99\":" << std::round(pct(0.99) * 10) / 10 << ",\"frame_ms_max\":" << std::round((f.empty() ? 0 : f.back()) * 10) / 10
      << ",\"render_ms_max\":" << std::round(rmax * 10) / 10 << ",\"encode_ms_max\":" << std::round(emax * 10) / 10 << "}";
  }
  o << ",\"trace_ms\":" << std::round(rstats_.trace_ms * 10) / 10;
  o << ",\"layout_generation\":";
  if (gcur >= 0) o << gcur; else o << "null";
  if (gpend) o << ",\"layout_pending\":{\"generation\":" << gnext << ",\"from_frame\":" << gk << "}";
  o << ",\"budget\":" << std::round(used_budget_) << ",\"budget_source\":\"" << budget_source_
    << "\",\"budget_fit\":" << std::round(fit_) << ",\"hq\":" << (rstats_.hq ? "true" : "false");
  {
    // CPU seconds this process has used (user + system), for cost per stream
    rusage ru;
    getrusage(RUSAGE_SELF, &ru);
    double cpu = ru.ru_utime.tv_sec + ru.ru_utime.tv_usec * 1e-6 + ru.ru_stime.tv_sec + ru.ru_stime.tv_usec * 1e-6;
    char b[32];
    snprintf(b, sizeof b, "%.3f", cpu);
    o << ",\"cpu_s\":" << b;
  }
  o << ",\"startup_ms\":{\"gpu_ready\":" << std::round(startup_ready_ms_) << ",\"first_frame_drawn\":"
    << std::round(first_frame_ms_) << ",\"first_frame_sent\":" << std::round(first_sent_ms_) << "}}";
  return o.str();
}
