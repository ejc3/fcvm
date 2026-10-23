// The grid-wide sample budget (PROTOCOL.md, "Budget"): the schedule a
// controller posts to every tile, each tile's fit, and the ladder a controller
// rounds to. Header-only so host tests use it without a GPU.
#pragma once
#include <algorithm>
#include <cmath>
#include <utility>
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
