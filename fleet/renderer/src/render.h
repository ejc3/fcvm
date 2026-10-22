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
  // Pixel-samples spent per frame (pixels x samples); spp = budget / pixels, 1..64.
  void set_budget(double pixel_samples) { budget_ = pixel_samples; }
  // Draw frame t into the tile's RGBA8 buffer on the device. accumulate=false draws
  // every frame independently (seeded by t only), which is what the seam test uses.
  // spp > 0 overrides the budget.
  void render(const Scene& s, const Tile& tile, double t, bool accumulate, int spp = 0);
  // Forget all history (a relaunched renderer starts like this).
  void reset_history();
  void download_rgba(std::vector<uint8_t>* out) const;  // w*h*4 bytes, row-major
  // Debug: the accumulated linear radiance (w*h*4 floats) of the last frame.
  void download_hdr(std::vector<float>* out) const;
  // Scale the last frame to dst_w x dst_h and write BT.709 limited-range NV12.
  void to_nv12(void* dst, size_t pitch, int dst_w, int dst_h);
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
  double budget_ = 2.0e6;
  Tile tile_{};
  bool have_tile_ = false;
  void* ctx_ = nullptr;
  std::string gpu_name_, pci_bus_id_;
  // device buffers
  void* noise_ = nullptr;
  void* trans_ = nullptr;
  void* sky_ = nullptr;
  void* cloud_arr_ = nullptr;
  unsigned long long cloud_tex_ = 0;
  void* rgba_ = nullptr;
  void* hdr_ = nullptr;        // final linear color (debug/test)
  void* hold_sum_ = nullptr;   // float4 rgb sum + count
  void* hold_aux_ = nullptr;   // float2 depth + id of the hold's first sample
  void* taa_[2] = {nullptr, nullptr};      // float4 rgb + count
  void* taa_aux_[2] = {nullptr, nullptr};  // float2 depth + id
  int cur_ = 0;         // index of the newest motion history
  int hold_src_ = 0;    // motion history frozen at the start of the hold
  uint64_t hold_key_ = 0;
  int hold_n_ = 0;
  bool have_prev_ = false;
  GCam prev_cam_{};
  void* stream_ = nullptr;
  void* ev0_ = nullptr;
  void* ev1_ = nullptr;
  RenderStats stats_;
};
