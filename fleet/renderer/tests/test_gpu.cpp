// Seam test on a GPU: the whole image and a 2x2 grid of tiles, drawn
// separately, must agree pixel for pixel when stitched by whole-image
// coordinates. Run in the container: fleet-renderer-test.
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

#include "../src/render.h"
#include "../src/scene.h"
#include "check.h"

static Scene test_scene() {
  Scene s = default_scene();
  s.loop_s = 0;
  s.epoch_unix_ms = 1761100000000LL;
  s.image_aspect = 16.0 / 9;
  s.words = {{"SEAMLESS", 1.0}, {"HELLO FLEET", 2.0}};
  s.lights = {{{-36.0, 0.95, -22.0}, {1.0, 0.4, 0.1}, 0.5}};
  return s;
}

struct Img {
  Tile tile;
  std::vector<uint8_t> rgba;
  std::vector<float> hdr;
};

// frames: (t, spp) pairs, accumulated in one fresh renderer
static Img draw(Renderer& r, Scene s, Rect rect, int h, const std::vector<std::pair<double, int>>& frames, bool acc) {
  s.rect = rect;
  Img im;
  im.tile = tile_geometry(rect, s.image_aspect, h);
  r.reset_history();
  for (auto& f : frames) r.render(s, im.tile, f.first, acc, f.second);
  r.download_rgba(&im.rgba);
  r.download_hdr(&im.hdr);
  return im;
}

// Count pixels of the 2x2 tiles that differ from the whole image.
static long compare(const Img& whole, const Img* q, long* hdr_diff) {
  long diff = 0;
  *hdr_diff = 0;
  for (int k = 0; k < 4; k++) {
    const Img& t = q[k];
    for (int j = 0; j < t.tile.h; j++)
      for (int i = 0; i < t.tile.w; i++) {
        int X = t.tile.x0 + i, Y = t.tile.y0 + j;
        if (X >= whole.tile.w || Y >= whole.tile.h) continue;
        size_t a = 4 * ((size_t)Y * whole.tile.w + X), b = 4 * ((size_t)j * t.tile.w + i);
        if (memcmp(&whole.rgba[a], &t.rgba[b], 4) != 0) diff++;
        if (memcmp(&whole.hdr[a], &t.hdr[b], 16) != 0) (*hdr_diff)++;
      }
  }
  return diff;
}

static uint64_t fnv(const std::vector<uint8_t>& v) {
  uint64_t h = 1469598103934665603ull;
  for (uint8_t c : v) { h ^= c; h *= 1099511628211ull; }
  return h;
}

static void quads(Renderer& r, const Scene& s, int h, const std::vector<std::pair<double, int>>& frames, bool acc, Img* q) {
  for (int k = 0; k < 4; k++) q[k] = draw(r, s, Rect{0.5 * (k & 1), 0.5 * (k >> 1), 0.5, 0.5}, h, frames, acc);
}

int main(int argc, char** argv) {
  int h = argc > 1 ? atoi(argv[1]) : 270;  // tile height; the whole image is 2h tall
  Renderer r;
  std::string err;
  if (!r.init(&err)) { fprintf(stderr, "init: %s\n", err.c_str()); return 1; }
  fprintf(stderr, "GPU %s (%s)\n", r.gpu_name().c_str(), r.pci_bus_id().c_str());
  Scene s = test_scene();
  Img q[4];
  long hd;

  // 1. Every frame independent (accumulation off), moving camera, words, a light, the clock.
  const double tm = 37.25;  // the default path moves here
  Img whole = draw(r, s, Rect{0, 0, 1, 1}, 2 * h, {{tm, 2}}, false);
  quads(r, s, h, {{tm, 2}}, false, q);
  long d = compare(whole, q, &hd);
  CHECK(d == 0 && hd == 0, "accumulation off: %ld pixels differ (8-bit), %ld (float)", d, hd);
  fprintf(stderr, "whole %dx%d at t=%.2f: fnv64 %016llx\n", whole.tile.w, whole.tile.h, tm, (unsigned long long)fnv(whole.rgba));

  // 2. A hold, accumulated in different batches: whole image 4 frames of 4
  // samples, tiles 2 frames of 8. Same 16 samples, so the same pixels.
  const double th = 55.0;  // inside the demo path's hold 50..60
  Img wh = draw(r, s, Rect{0, 0, 1, 1}, 2 * h, {{th - 3, 4}, {th - 2, 4}, {th - 1, 4}, {th, 4}}, true);
  quads(r, s, h, {{th - 0.5, 8}, {th, 8}}, true, q);
  d = compare(wh, q, &hd);
  CHECK(d == 0 && hd == 0, "hold, 16 samples in different batches: %ld pixels differ (8-bit), %ld (float)", d, hd);
  CHECK(r.stats().hold && r.stats().hold_samples == 16, "the test really accumulated a hold (hold=%d samples=%d)",
        (int)r.stats().hold, r.stats().hold_samples);
  fprintf(stderr, "hold whole: fnv64 %016llx\n", (unsigned long long)fnv(wh.rgba));

  // 3. Moving camera with history: the same 12 consecutive frames on the 1/30 s
  // grid, drawn whole and as tiles, each in a fresh renderer. After the
  // history window every frame is a function of the frame sequence only, so the
  // last frames must stitch exactly.
  {
    std::vector<std::pair<double, int>> seq;
    const long k0 = 37 * 30;  // the default path moves here
    for (int k = 0; k < 12; k++) seq.push_back({(k0 + k) / 30.0, 2});
    Img wmv = draw(r, s, Rect{0, 0, 1, 1}, 2 * h, seq, true);
    Img qm[4];
    quads(r, s, h, seq, true, qm);
    d = compare(wmv, qm, &hd);
    CHECK(d == 0 && hd == 0, "moving with history, after the window: %ld pixels differ (8-bit), %ld (float)", d, hd);
    CHECK(!r.stats().hold, "the moving sequence is not a hold");
    // and the history is really used: the same last frame without it differs
    Img alone = draw(r, s, Rect{0, 0, 1, 1}, 2 * h, {seq.back()}, false);
    long used = 0;
    for (size_t p = 0; p < alone.rgba.size(); p += 4) used += memcmp(&alone.rgba[p], &wmv.rgba[p], 3) != 0;
    CHECK(used > (long)(alone.rgba.size() / 4 / 2), "history changes most pixels of a moving frame (%ld of %zu)", used,
          alone.rgba.size() / 4);
    fprintf(stderr, "moving whole: fnv64 %016llx\n", (unsigned long long)fnv(wmv.rgba));
  }

  // 4. Checkerboard frames (stride 2: half the pixels draw a sample per frame;
  // stride 4: a quarter; the rest are filled and come from history): moving
  // with history, and a hold whose samples per pixel come from 2x the slots at
  // spp 1 for the whole image and from the slots at spp 2 for the tiles.
  for (int stride : {2, 4}) {
    r.set_checkerboard(stride);
    std::vector<std::pair<double, int>> seq;
    const long k0 = 37 * 30;
    for (int k = 0; k < 12; k++) seq.push_back({(k0 + k) / 30.0, 1});
    Img wc = draw(r, s, Rect{0, 0, 1, 1}, 2 * h, seq, true);
    CHECK(r.stats().half == stride && !r.stats().hold, "stride %d: a moving checkerboard", stride);
    Img qc[4];
    quads(r, s, h, seq, true, qc);
    d = compare(wc, qc, &hd);
    CHECK(d == 0 && hd == 0, "stride %d, moving with history: %ld pixels differ (8-bit), %ld (float)", stride, d, hd);
    const long kh = 55 * 30;  // inside the hold 50..60
    std::vector<std::pair<double, int>> hw, ht;
    for (int k = 2 * stride - 1; k >= 0; k--) hw.push_back({(kh - k) / 30.0, 1});
    for (int k = stride - 1; k >= 0; k--) ht.push_back({(kh - k) / 30.0, 2});
    Img wh2 = draw(r, s, Rect{0, 0, 1, 1}, 2 * h, hw, true);
    CHECK(r.stats().hold && r.stats().hold_samples == 2, "stride %d hold, 2 samples (hold=%d samples=%d)", stride,
          (int)r.stats().hold, r.stats().hold_samples);
    Img qh[4];
    quads(r, s, h, ht, true, qh);
    d = compare(wh2, qh, &hd);
    CHECK(d == 0 && hd == 0, "stride %d hold, 2 samples in different batches: %ld pixels differ (8-bit), %ld (float)",
          stride, d, hd);
  }
  r.set_checkerboard(-1);

  // 5. Controls: the comparison can fail. A tile drawn one pixel to the right
  // but placed where the true tile goes, and another t.
  Img off = draw(r, s, Rect{0.5 + 1.0 / whole.tile.full_w, 0.5, 0.5, 0.5}, h, {{tm, 2}}, false);
  off.tile.x0 -= 1;
  Img q2[4] = {q[0], q[1], q[2], off};
  Img wm = draw(r, s, Rect{0, 0, 1, 1}, 2 * h, {{tm, 2}}, false);
  quads(r, s, h, {{tm, 2}}, false, q2);
  q2[3] = off;
  long dc = compare(wm, q2, &hd);
  CHECK(dc > 1000, "control: a tile shifted by one pixel must differ (%ld pixels differ)", dc);
  quads(r, s, h, {{tm + 1.0 / 30, 2}}, false, q2);
  dc = compare(wm, q2, &hd);
  CHECK(dc > 1000, "control: another frame t must differ (%ld pixels differ)", dc);
  DONE();
}
