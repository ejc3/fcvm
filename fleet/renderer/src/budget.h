// The grid-wide sample budget (PROTOCOL.md, "Budget"): the schedule a
// controller posts to every tile, each tile's fit, and the ladder a controller
// rounds to. Header-only so host tests use it without a GPU.
#pragma once
#include <algorithm>
#include <cmath>
#include <utility>
#include <deque>
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
