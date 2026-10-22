// The GPU side: path tracer, accumulation, tone mapping, scaling to NV12.
#pragma once
#include <stdint.h>

#include <string>
#include <vector>

#include "params.h"
#include "scene.h"

struct RenderStats {
  float render_ms = 0;  // last frame: trace + accumulate + tone map
  int spp = 0;          // samples drawn per pixel in the last frame
  int hold_samples = 0; // samples accumulated in the current hold (0 while moving)
  bool hold = false;
};

class Renderer {
 public:
  Renderer();
  ~Renderer();
  // Creates the CUDA context and the tables. Returns false with a message.
  bool init(std::string* err);
  // Pixel-samples spent per frame (drawn pixels x samples); spp = budget / pixels, 1..64.
  void set_budget(double pixel_samples) { budget_ = pixel_samples; }
  // Frames are drawn at t = k / fps; the history ring is indexed by k.
  void set_fps(int fps) { fps_ = fps; }
  // Pixels drawn per frame for a tile (the tile plus its guard band, before
  // clipping to the image), so the sample count is the same for every tile of a grid.
  double drawn_pixels(const Tile& t) const;
  // Pixels finished past each side of the tile (inside the image), for streams
  // that encode an overlap the client crops. Same value on every renderer.
  void set_margin(int m);
  // Draw frame t into the tile's RGBA8 buffer on the device. accumulate=false draws
  // every frame independently (seeded by t only), which is what the seam test uses.
  // spp > 0 overrides the budget.
  void render(const Scene& s, const Tile& tile, double t, bool accumulate, int spp = 0);
  // Forget all history (a relaunched renderer starts like this).
  void reset_history();
  void download_rgba(std::vector<uint8_t>* out) const;  // w*h*4 bytes, row-major
  // Debug: the accumulated linear radiance (w*h*4 floats) of the last frame.
  void download_hdr(std::vector<float>* out) const;
  // Write BT.709 limited-range NV12 of dst_w x dst_h pixels whose top-left is
  // whole-image pixel (sx0, sy0) of a grid sfull_h pixels tall, area-sampled
  // from the last frame's out region (pixels beyond it repeat its edge).
  void to_nv12(void* dst, size_t pitch, int dst_w, int dst_h, int sx0, int sy0, int sfull_h);
  void sync();
  // Device NV12 buffer for an encoder input (luma rows then chroma rows).
  void* alloc_nv12(int w, int h, size_t* pitch);
  void free_dev(void* p);
  void* cu_context() const { return ctx_; }
  int width() const { return tile_.w; }
  int height() const { return tile_.h; }
  const RenderStats& stats() const { return stats_; }
  std::string gpu_name() const { return gpu_name_; }
  std::string pci_bus_id() const { return pci_bus_id_; }

 private:
  void ensure_buffers(const Tile& t);
  double budget_ = 1.0e6;
  int fps_ = 30;
  Tile tile_{};
  int ex0_ = 0, ey0_ = 0, ew_ = 0, eh_ = 0, guard_ = 0;  // drawn region and reprojection reach
  int rx0_ = 0, ry0_ = 0, rw_ = 0, rh_ = 0;               // resolved region
  int ox0_ = 0, oy0_ = 0, ow_ = 0, oh_ = 0;               // out region
  int margin_ = 0;
  bool have_tile_ = false;
  void* ctx_ = nullptr;
  std::string gpu_name_, pci_bus_id_;
  // device tables
  void* noise_ = nullptr;
  void* trans_ = nullptr;
  void* sky_ = nullptr;
  void* cloud_arr_ = nullptr;
  unsigned long long cloud_tex_ = 0;
  // per tile (core region)
  void* rgba_ = nullptr;
  void* hdr_ = nullptr;         // final linear color (tests)
  void* display_ = nullptr;     // float4 over the resolved region: last resolved color and its weight
  void* hold_blend_ = nullptr;  // display_ as it was when the current hold began
  // per drawn region (tile plus guard band)
  void* hold_sum_ = nullptr;    // float4 rgb sum + count
  void* hold_aux_ = nullptr;    // float2 depth + id of the hold's first sample
  void* ring_[TEMPORAL_WINDOW] = {};      // float4: frame's mean color + sample count
  void* ring_aux_[TEMPORAL_WINDOW] = {};  // float2: first hit distance + kind
  long long ring_slot_[TEMPORAL_WINDOW];  // frame slot held by each ring entry, or LLONG_MIN
  GCam ring_cam_[TEMPORAL_WINDOW];
  uint64_t hold_key_ = 0;
  int hold_n_ = 0;
  void* stream_ = nullptr;
  void* ev0_ = nullptr;
  void* ev1_ = nullptr;
  RenderStats stats_;
};
