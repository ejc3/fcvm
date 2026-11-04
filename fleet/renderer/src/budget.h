// The grid-wide sample budget (PROTOCOL.md, "Budget"): the schedule a
// controller posts to every tile, each tile's fit, and the ladder a controller
// rounds to. Header-only so host tests use it without a GPU.
#pragma once
#include <algorithm>
#include <climits>
#include <cmath>
#include <utility>
#include <deque>
#include <map>
#include <vector>

// Budgets that take effect from a frame slot: every tile is given the same
// entries, so every tile uses the same budget at the same slot.
class BudgetSchedule {
 public:
  // From slot k on, use budget b. A later entry for the same slot replaces it.
  void add(long long k, double b) {
    for (auto& e : v_)
      if (e.first == k) { e.second = b; return; }
    v_.emplace_back(k, b);
    std::sort(v_.begin(), v_.end());
  }
  // The budget for slot k: the latest entry at or before k. Entries older than
  // the one in effect at k are dropped.
  bool at(long long k, double* b) {
    int in = -1;
    for (int i = 0; i < (int)v_.size(); i++)
      if (v_[i].first <= k) in = i;
    if (in < 0) return false;
    if (in > 0) v_.erase(v_.begin(), v_.begin() + in);
    *b = v_[0].second;
    return true;
  }
  size_t size() const { return v_.size(); }

 private:
  std::vector<std::pair<long long, double>> v_;
};

// The schedule a new scene leaves. On the same clock (the same epoch_unix_ms,
// as for a re-tile) the entries still name the same slots and stay. On a new
// clock they name nothing, so only the budget in force at the last frame drawn
// carries over, from slot 0, until the controller posts again: every tile of a
// grid had that budget, so the frames drawn between the scene post and the
// controller's next budget post still agree. A scene with its own budget
// clears the schedule.
inline BudgetSchedule schedule_after_scene(BudgetSchedule s, bool same_clock, bool scene_budget, long long k_last) {
  if (scene_budget) return BudgetSchedule();
  if (same_clock) return s;
  BudgetSchedule n;
  double b;
  if (s.at(k_last, &b)) n.add(0, b);
  return n;
}

// The budget at which this tile's frame would take `frac` of its slot, from the
// samples it drew and the GPU time they took (fixed costs count as per-sample
// cost, so the fit errs low).
inline double budget_fit(double samples_drawn, double render_ms, double slot_ms, double frac = 0.70) {
  if (samples_drawn <= 0 || render_ms <= 0) return 0;
  return samples_drawn * frac * slot_ms / render_ms;
}

// The checkerboard stride for a budget per drawn pixel: a quarter of the
// pixels under 0.3, half under 0.5, else every pixel (PROTOCOL.md).
inline int stride_for(double per_px) { return per_px < 0.3 ? 4 : per_px < 0.5 ? 2 : 1; }

// Samples a frame of `px` drawn pixels really draws for budget b (a stride
// draws one sample on its share of the pixels, whatever the budget).
inline double samples_for(double b, double px) {
  int st = stride_for(b / px);
  double spp = st > 1 ? 1 : std::min(64.0, std::max(1.0, std::round(b / px)));
  return px / st * spp;
}

// The largest budget whose frame draws no more than `fit` samples on `px` drawn
// pixels (samples_for). A frame's samples are a step function of its budget:
// under 0.3 px a quarter of the pixels draw, under 0.5 px half, and from there
// every pixel draws round(b / px) samples, so a budget just past 0.5 px draws
// twice what one just under it does. A renderer reports its fit as this budget,
// so a controller that takes the reported fit as a budget never posts one whose
// samples the tile cannot draw in time: a 432-line staging tile (550,896 drawn
// pixels) fitting 359,222 samples reported 359,222, and the rung under it / 1.15
// (297,302) drew 550,896 samples (2026-11-04).
inline double budget_for_samples(double fit, double px) {
  if (fit <= 0 || px <= 0) return fit;
  for (int k = 64; k >= 1; k--)
    if (k * px <= fit) return k == 64 ? fit : (k + 0.5) * px * 0.999;
  if (0.5 * px <= fit) return 0.5 * px * 0.999;
  if (0.25 * px <= fit) return 0.3 * px * 0.999;
  return fit;  // not even the floor fits: the fit itself, under every rung that draws
}

// What a renderer reports as `budget_fit`: its cost model's fit, in samples,
// as the largest budget whose frame draws that many samples or fewer.
inline double reported_fit(double fit_samples, double px) { return budget_for_samples(fit_samples, px); }

// The ladder a controller rounds a fit down to: 250,000 x 2^(n/4) for
// n = -8 .. 40 (62,500 to 256,000,000), so budgets change in steps of 19%.
inline double ladder_floor(double fit) {
  double best = 250000.0 * std::pow(2.0, -8 / 4.0);
  for (int n = -8; n <= 40; n++) {
    double r = 250000.0 * std::pow(2.0, n / 4.0);
    if (r <= fit * (1 + 1e-9)) best = r;
  }
  return std::round(best);
}

// The rung a renderer drawing alone takes: the highest at or under its fit
// whose frame really draws no more samples than the fit (a stride rounds a
// budget up to its share of the pixels).
inline double local_budget(double fit, double px) {
  double b = ladder_floor(fit);
  while (b > 62500 && samples_for(b, px) > fit) b = ladder_floor(b * 0.999);
  return b;
}

// A renderer's cost model for its fit: the fixed part of a frame (everything
// but the samples) and the cost per sample of each path, from the frames it
// drew. The fit is the largest budget whose predicted frame takes `frac` of a
// slot: on the fast path below HQ_SPP samples per drawn pixel, on the
// high-quality path from it, whichever allows more.
//
// The costs come from a window: the 95th percentile of each over the frames
// of the last `window_s` seconds, so the fit holds over what the camera has
// shown in that time rather than following the last few frames (on a moving
// camera the latest frames' cost swung 2x, and the fit with it). Until the
// window spans `ready_s`, fit() is 0 (not known yet) and fit_now() follows
// the latest frames (smoothed over about five). The first `settle_s` seconds
// stay out of the window: a GPU coming up from idle clocks draws them slower,
// and they would hold the fit down until they left it.
struct CostModel {
  double fixed_ms = -1, c_fast = -1, c_hq = -1;  // smoothed latest; -1: not seen yet
  double hq_prior = 1.0;                          // c_hq / c_fast before a high-quality frame is seen
  double window_s = 180.0, ready_s = 60.0, pct = 0.95;
  double settle_s = 5.0;  // the first seconds after a reset stay out of the window (a GPU leaving idle clocks)
  struct Obs {
    double t;
    bool hq;
    double c, fixed;
  };
  std::deque<Obs> obs;
  double first_t = -1;
  // percentiles over the window, refreshed at most once a second
  double w_fixed = -1, w_fast = -1, w_hq = -1, w_at = -1e18;

  void observe(bool hq, double samples, double trace_ms, double render_ms, double now_s = -1) {
    if (samples <= 0 || trace_ms <= 0) return;
    auto ew = [](double& v, double x) { v = v < 0 ? x : v + (x - v) * 0.2; };
    double fx = std::max(0.0, render_ms - trace_ms), c = trace_ms / samples;
    ew(fixed_ms, fx);
    ew(hq ? c_hq : c_fast, c);
    if (now_s < 0) return;
    if (first_t < 0) first_t = now_s;
    if (now_s - first_t < settle_s) return;
    obs.push_back({now_s, hq, c, fx});
    while (!obs.empty() && obs.front().t < now_s - window_s) obs.pop_front();
    if (now_s - w_at >= 1.0) {
      w_at = now_s;
      std::vector<double> f, a, b;
      for (const Obs& o : obs) {
        f.push_back(o.fixed);
        (o.hq ? b : a).push_back(o.c);
      }
      auto q = [this](std::vector<double>& v) {
        if (v.size() < 30) return -1.0;
        size_t n = std::min(v.size() - 1, (size_t)(pct * v.size()));
        std::nth_element(v.begin(), v.begin() + n, v.end());
        return v[n];
      };
      w_fixed = q(f);
      w_fast = q(a);
      w_hq = q(b);
    }
  }
  // The drawn size changed by `px_ratio` (new / old): the fixed part of a frame
  // (the per-pixel passes) scales with it, the cost per sample does not. Keeping
  // the window keeps the fit known: started again at every size change, a tile
  // whose height guard moved it between two heights reported no fit for 60 s at
  // a time, and a controller never raises while a tile has none.
  void rescale(double px_ratio) {
    if (!(px_ratio > 0)) return;
    if (fixed_ms >= 0) fixed_ms *= px_ratio;
    for (Obs& o : obs) o.fixed *= px_ratio;
    w_at = -1e18;  // percentiles again at the next frame
    if (w_fixed >= 0) w_fixed *= px_ratio;
  }
  bool ready(double now_s) const { return first_t >= 0 && now_s - first_t >= ready_s && w_fixed >= 0; }
  static double fit_from(double fixed, double cfast, double chq, double prior, double px, double hq_spp, double slot_ms,
                         double frac) {
    if (fixed < 0 || (cfast < 0 && chq < 0)) return 0;
    double avail = frac * slot_ms - fixed;
    if (avail <= 0) return 1e4;
    double cf = cfast > 0 ? cfast : chq / prior;
    double ch = chq > 0 ? chq : cfast * prior;
    double b_fast = std::min(avail / cf, hq_spp * px * 0.999);
    double b_hq = avail / ch;
    return b_hq >= hq_spp * px ? std::max(b_hq, b_fast) : b_fast;
  }
  // from the latest frames (px: drawn pixels; hq_spp: samples per drawn pixel
  // where the high-quality path starts)
  double fit_now(double px, double hq_spp, double slot_ms, double frac = 0.70) const {
    return fit_from(fixed_ms, c_fast, c_hq, hq_prior, px, hq_spp, slot_ms, frac);
  }
  // from the window; 0 until it is ready. A path the window has not seen
  // takes the other's cost (times the prior).
  double fit(double px, double hq_spp, double slot_ms, double now_s, double frac = 0.70) const {
    if (!ready(now_s)) return 0;
    return fit_from(w_fixed, w_fast, w_hq, hq_prior, px, hq_spp, slot_ms, frac);
  }
};

// The height a tile draws at. A renderer draws one frame per slot, at the
// tallest height its streams ask, and scales it to the others. An opened tile
// asks a 16:9 wall tile for 720 against the wall's 432, 2.65x the drawn pixels:
// on terrain that frame takes longer than a slot even at the lowest budget (a
// stride-4 checkerboard with one sample is the least a frame draws), so the
// tile skips slots, and the page, which shows every tile at one scene time,
// draws the whole wall at that tile's rate (staging, 2026-10-27: 36 ms frames,
// 15 fps, the wall at 19 fps). So a tile draws at the tallest asked height whose
// frame fits its slot, and a taller stream is scaled up from it.
//
// A taller height is predicted from the frames at the height drawn now, part by
// part: the trace (the samples) grows with the samples the budget draws at that
// height (samples_for: the same budget at more pixels is the same samples, until a
// checkerboard stride rounds it up), the rest of the render (the per-pixel passes)
// with the drawn pixels, and the encodes stay as they are (every asked stream is
// encoded whatever height is drawn). Predicting the whole frame from the pixels,
// as before, held a tile asked for 180 (the broadcast's aggregator) and 432 (a
// viewer) at 180 at the same budget: on staging, 2026-11-03, 8 of 14 tiles drew
// 180 for a 432 or 540 view, which the page scaled up 2.4 to 3 times.
class HeightGuard {
 public:
  // One frame: its render, the part of it that traced `samples` samples, and its encodes.
  struct Cost {
    double render_ms = 0, trace_ms = 0, enc_ms = 0, samples = 0;
    double frame_ms() const { return render_ms + enc_ms; }
  };
  double slot_ms = 1000.0 / 30;
  double over_frac = 0.85;   // frames whose 90th percentile passes this share of a slot: the height is too tall
  double fit_frac = 0.70;    // a step down goes to the tallest height predicted within this share
  double up_frac = 0.65;     // a step up goes only to a height predicted within this share
  double window_s = 2.0;     // frames judged: the last window_s, or the last min_frames if older
  double stale_s = 10.0;     // frames older than this are never judged (the tile stopped streaming)
  double settle_s = 0.3;     // frames this soon after a switch are not judged (priming, new encoder)
  double hold_s = 10.0;      // a height is drawn this long before a taller one is tried
  double backoff_s = 60.0;   // a height that overran is not drawn again for this long
  size_t min_frames = 15;

  // One frame drawn at height h, and what it cost.
  void observe(double now_s, int h, const Cost& c) {
    if (h != drawn_ || now_s - since_ < settle_s) return;
    frames_.push_back({now_s, c});
    prune(now_s);
  }

  // The height to draw for the asked heights (ascending, distinct); px(h) is
  // the pixels a frame at h draws and samples(h) the samples it traces at the
  // budget in force. 0 when nothing is asked.
  template <class Px, class Samples>
  int choose(const std::vector<int>& asked, Px px, Samples samples, double now_s) {
    if (asked.empty()) return 0;
    prune(now_s);
    auto in = [&](int h) { return std::find(asked.begin(), asked.end(), h) != asked.end(); };
    auto banned = [&](int h) {
      auto it = banned_.find(h);
      return it != banned_.end() && now_s < it->second;
    };
    // judged once min_frames are in, or, for a tile too slow to draw that many,
    // once three frames span the window
    bool enough = frames_.size() >= min_frames ||
                  (frames_.size() >= 3 && now_s - frames_.front().first >= window_s);
    double p90 = enough ? pct90([](const Cost& c) { return c.frame_ms(); }) : -1;
    // a frame at h predicted from each frame drawn now: its 90th percentile
    auto predicted = [&](int h) {
      double pr = px(h) / px(drawn_), sh = samples(h);
      return pct90([&](const Cost& c) {
        double tr = c.samples > 0 ? c.trace_ms * sh / c.samples : c.trace_ms * pr;
        return c.enc_ms + (c.render_ms - c.trace_ms) * pr + tr;
      });
    };
    // the tallest asked height under `below` predicted within frac of a slot
    auto fitting = [&](int below, double frac) {
      int best = 0;
      for (int h : asked)
        if (h < below && !banned(h) && p90 >= 0 && predicted(h) <= frac * slot_ms) best = h;
      return best;
    };
    if (drawn_ == 0 || !in(drawn_)) {
      // a new set of streams: predicted from the frames drawn so far, else the tallest
      int h = drawn_ > 0 ? fitting(INT_MAX, fit_frac) : 0;
      if (h == 0)
        for (int a : asked)
          if (!banned(a)) h = a;
      switch_to(h ? h : asked.front(), now_s);
      return drawn_;
    }
    if (p90 > over_frac * slot_ms && drawn_ > asked.front()) {
      banned_[drawn_] = now_s + backoff_s;
      int h = fitting(drawn_, fit_frac);
      switch_to(h ? h : asked.front(), now_s);
      return drawn_;
    }
    if (p90 >= 0 && now_s - since_ >= hold_s) {
      int h = fitting(INT_MAX, up_frac);
      if (h > drawn_) switch_to(h, now_s);
    }
    return drawn_;
  }
  int drawn() const { return drawn_; }

 private:
  void switch_to(int h, double now_s) {
    if (h == drawn_) return;
    drawn_ = h;
    since_ = now_s;
    frames_.clear();
  }
  void prune(double now_s) {
    while (!frames_.empty() && (frames_.front().first < now_s - stale_s ||
                                (frames_.size() > min_frames && frames_.front().first < now_s - window_s)))
      frames_.pop_front();
  }
  template <class F>
  double pct90(F of) const {
    std::vector<double> v;
    for (auto& f : frames_) v.push_back(of(f.second));
    size_t n = std::min(v.size() - 1, (size_t)(0.9 * v.size()));
    std::nth_element(v.begin(), v.begin() + n, v.end());
    return v[n];
  }
  int drawn_ = 0;
  double since_ = -1e18;
  std::deque<std::pair<double, Cost>> frames_;
  std::map<int, double> banned_;
};
