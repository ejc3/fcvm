// Seam test on a GPU: the whole image and a 2x2 grid of tiles, drawn
// separately, must agree pixel for pixel when stitched by whole-image
// coordinates. Then the encoder's SPS must say frames are never reordered.
// Run in the container: fleet-renderer-test.
#include <algorithm>
#include <cmath>
#include <cuda_runtime.h>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

#include "../src/nvenc.h"
#include "../src/render.h"
#include "../src/scene.h"
#include "check.h"

// Section 10's bounds: the moving frame's error against its reference.
// Measured on a T4 (round 13): rmse 15.34 and grain 7.02 with round 12's 5 x 5 filter and plain
// reprojection of the sky; 6.10 and 4.45 with the a-trous denoiser and the sky's history clamp.
#define RMSE_MAX 9.0
#define GRAIN_MAX 5.5
// Section 10b's bounds: speckle, % of pixels. Measured on a T4 at the default
// h (270): cloud deck 1.46 before the cloud bit (2026-10-30), 0.64 after
// (the 1024-sample hold reads 0.91: the cloud's own fine edges); snow and rock
// 0.48 with the denoiser's normal stop at ^64, 0.31 at ^8 (the hold 0.21).
#define SPECKLE_CLOUDS 1.0
#define SPECKLE_SNOW 0.40
// Section 10c's bounds. Measured on a T4: main (93c33125) 80 pixels flickering and a rim of
// 8.19; with the four-point cut and the glow toward the letters' color, 43 and 3.34.
#define TEXT_FLICKER_MAX 60
#define TEXT_RIM_MAX 6.0

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

  // 7. The high-quality path (taken from HQ_SPP samples per drawn pixel; set
  // here directly): moving with history while the samples per frame change
  // mid-sequence, as when the controller raises every tile's budget at one
  // slot, and a hold in different batches. And it really changes the image.
  {
    r.set_quality(1);
    std::vector<std::pair<double, int>> seq;
    const long k0 = 37 * 30;
    for (int k = 0; k < 12; k++) seq.push_back({(k0 + k) / 30.0, k < 6 ? 2 : 8});
    Img wq = draw(r, s, Rect{0, 0, 1, 1}, 2 * h, seq, true);
    CHECK(r.stats().hq, "the high-quality path was taken");
    Img qq[4];
    quads(r, s, h, seq, true, qq);
    d = compare(wq, qq, &hd);
    CHECK(d == 0 && hd == 0, "high quality, moving, spp 2 then 8: %ld pixels differ (8-bit), %ld (float)", d, hd);
    Img whq = draw(r, s, Rect{0, 0, 1, 1}, 2 * h, {{th - 3, 4}, {th - 2, 4}, {th - 1, 4}, {th, 4}}, true);
    quads(r, s, h, {{th - 0.5, 8}, {th, 8}}, true, qq);
    d = compare(whq, qq, &hd);
    CHECK(d == 0 && hd == 0, "high quality, hold, 16 samples in different batches: %ld pixels differ (8-bit), %ld (float)",
          d, hd);
    r.set_quality(0);
    Img wlo = draw(r, s, Rect{0, 0, 1, 1}, 2 * h, {{th - 3, 4}, {th - 2, 4}, {th - 1, 4}, {th, 4}}, true);
    long dq = 0;
    for (size_t p = 0; p < wlo.rgba.size(); p += 4) dq += memcmp(&wlo.rgba[p], &whq.rgba[p], 3) != 0;
    CHECK(dq > (long)(wlo.rgba.size() / 4 / 10), "the high-quality path changes the image (%ld pixels)", dq);
    r.set_quality(-1);
    fprintf(stderr, "high quality moving whole: fnv64 %016llx\n", (unsigned long long)fnv(wq.rgba));
  }

  // 6. No sample is NaN. The golden-hour view looks toward pixels where the
  // sun is straight behind the eye: GGX's Fresnel term then took the
  // fast-math powf of a value just below zero, about one sample in a frame.
  {
    Scene g = s;
    g.camera.clear();
    g.camera.push_back(Keyframe{0, {-15, 2.2, -100}, {-70, 1.4, -70}, 62});
    g.time_of_day = 0.245;
    g.rect = Rect{0, 0, 1, 1};
    Tile gt = tile_geometry(g.rect, g.image_aspect, 1080);
    r.reset_history();
    unsigned bad = 0;
    for (int f = 0; f < 4; f++) {
      r.render(g, gt, 25.0, true, 16);
      bad += r.stats().bad_samples;
    }
    CHECK(bad == 0 && r.stats().hold_samples == 64, "golden-hour hold at 1080, 64 samples: %u non-finite samples", bad);
  }

  // 7. The text styles: a word in each style stitches, moving (accumulation
  // off) and as a hold in different batches, and is drawn. Skywriting and the
  // landmark stay where the camera saw them when the word came, so these use
  // views that keep them in frame: the shots' moving path (word at 0, frame at
  // 10) and its golden-hour hold (word at 5, frames at 22..25).
  const Keyframe kMove0{0, {-22, 0.7, -30}, {-45, 1.2, -85}, 60}, kMove1{60, {-40, 2.4, -70}, {-20, 0.8, -40}, 64};
  const Keyframe kHold{0, {-15, 2.2, -100}, {-70, 1.4, -70}, 62};
  for (int style = TEXT_STYLE_LOCKUP; style <= TEXT_STYLE_LANDMARK; style++) {
    const char* name[] = {"lockup", "skywriting", "landmark"};
    Scene ts = s;
    ts.camera = {kMove0, kMove1};
    ts.words = {{"Hello FLEET", 0.0, style}};
    Scene th2 = ts;
    th2.camera = {kHold};
    th2.words = {{"Seams hold", 5.0, style}};
    const double tmv = 10.0, thd = 25.0;
    Img w1 = draw(r, ts, Rect{0, 0, 1, 1}, 2 * h, {{tmv, 2}}, false);
    quads(r, ts, h, {{tmv, 2}}, false, q);
    d = compare(w1, q, &hd);
    CHECK(d == 0 && hd == 0, "%s, moving: %ld pixels differ (8-bit), %ld (float)", name[style], d, hd);
    Img w2 = draw(r, th2, Rect{0, 0, 1, 1}, 2 * h, {{thd - 3, 4}, {thd - 2, 4}, {thd - 1, 4}, {thd, 4}}, true);
    quads(r, th2, h, {{thd - 0.5, 8}, {thd, 8}}, true, q);
    d = compare(w2, q, &hd);
    CHECK(d == 0 && hd == 0, "%s, hold: %ld pixels differ (8-bit), %ld (float)", name[style], d, hd);
    Scene nw = ts;
    nw.words.clear();
    Img w3 = draw(r, nw, Rect{0, 0, 1, 1}, 2 * h, {{tmv, 2}}, false);
    long dw = 0;
    for (size_t p = 0; p < w1.rgba.size(); p += 4) dw += memcmp(&w1.rgba[p], &w3.rgba[p], 3) != 0;
    CHECK(dw > 300, "%s draws the word (%ld pixels)", name[style], dw);
    fprintf(stderr, "%s moving whole: fnv64 %016llx\n", name[style], (unsigned long long)fnv(w1.rgba));
  }

  // 7b. Weather (towers, cover, crepuscular rays, snow line), easing from the
  // defaults to its key: a moving camera toward a low sun with history, and a
  // hold in different batches, stitch exactly; the weather changes the image.
  {
    Scene ws = s;
    ws.time_of_day = 0.27;
    ws.camera = {Keyframe{0, {-100, 0.35, -90}, {-63.5, 2.6, -87}, 60}, Keyframe{60, {-96, 0.45, -92}, {-59.5, 2.7, -89}, 60}};
    WeatherKey a, b;
    a.t = 0;
    b.t = 20;
    b.cover = 0.62;
    b.towers = 1;
    b.rays = 1;
    b.snow_line_km = 0.8;
    ws.weather = {a, b};
    std::vector<std::pair<double, int>> seq;
    for (int k = 0; k < 12; k++) seq.push_back({(14 * 30 + k) / 30.0, 2});
    Img w1 = draw(r, ws, Rect{0, 0, 1, 1}, 2 * h, seq, true);
    quads(r, ws, h, seq, true, q);
    d = compare(w1, q, &hd);
    CHECK(d == 0 && hd == 0, "weather, moving with history: %ld pixels differ (8-bit), %ld (float)", d, hd);
    Scene wh2 = ws;
    wh2.camera = {ws.camera[0]};
    wh2.weather = {b};
    Img w2 = draw(r, wh2, Rect{0, 0, 1, 1}, 2 * h, {{22, 4}, {23, 4}, {24, 4}, {25, 4}}, true);
    quads(r, wh2, h, {{24.5, 8}, {25, 8}}, true, q);
    d = compare(w2, q, &hd);
    CHECK(d == 0 && hd == 0, "weather, hold in different batches: %ld pixels differ (8-bit), %ld (float)", d, hd);
    Scene nw = ws;
    nw.weather.clear();
    Img w3 = draw(r, nw, Rect{0, 0, 1, 1}, 2 * h, seq, true);
    long dw = 0;
    for (size_t p = 0; p < w1.rgba.size(); p += 4) dw += memcmp(&w1.rgba[p], &w3.rgba[p], 3) != 0;
    CHECK(dw > (long)(w1.rgba.size() / 4 / 4), "the weather changes the image (%ld of %zu pixels)", dw, w1.rgba.size() / 4);
    fprintf(stderr, "weather moving whole: fnv64 %016llx\n", (unsigned long long)fnv(w1.rgba));
  }

  // 8. Landmark letters cast shadows in the path-traced samples: with the
  // overlay off (the letters themselves not drawn), the hold still darkens
  // where they shade the terrain.
  {
    Scene a = s;
    a.camera = {kHold};
    a.words = {{"Seams hold", 5.0, TEXT_STYLE_LANDMARK}};
    Scene b = a;
    b.words.clear();
    r.set_text_overlay(false);
    std::vector<std::pair<double, int>> hold = {{22, 4}, {23, 4}, {24, 4}, {25, 4}};
    Img wa = draw(r, a, Rect{0, 0, 1, 1}, 2 * h, hold, true);
    Img wb = draw(r, b, Rect{0, 0, 1, 1}, 2 * h, hold, true);
    r.set_text_overlay(true);
    long darker = 0;
    for (size_t p = 0; p < wa.rgba.size(); p += 4) {
      float la = 0.2126f * wa.rgba[p] + 0.7152f * wa.rgba[p + 1] + 0.0722f * wa.rgba[p + 2];
      float lb = 0.2126f * wb.rgba[p] + 0.7152f * wb.rgba[p + 1] + 0.0722f * wb.rgba[p + 2];
      darker += la < lb - 6.0f;
    }
    CHECK(darker > 200, "landmark letters shade the terrain (%ld pixels darker)", darker);
  }

  // 9. The clock reads over cloud: the shots' moving path at 10 s, where a
  // bright cloud passes behind the clock. In the clock's box, its letters are
  // brighter than the pixels around them by at least 0.3 (8-bit luminance /
  // 255), and the box is mostly cloud.
  {
    Scene c = s;
    c.camera = {kMove0, kMove1};
    c.words.clear();
    c.rect = Rect{0, 0, 1, 1};
    const double tc = 10.0;
    std::vector<std::pair<double, int>> seq;
    for (int k = 0; k < 8; k++) seq.push_back({(tc * 30 - 7 + k) / 30.0, 2});
    Img on = draw(r, c, Rect{0, 0, 1, 1}, 2 * h, seq, true);
    r.set_text_overlay(false);
    Img off = draw(r, c, Rect{0, 0, 1, 1}, 2 * h, seq, true);
    r.set_text_overlay(true);
    uint64_t key;
    bool held;
    FrameParams fp = build_frame(c, on.tile, tc, &key, &held);
    const GText& g = fp.text[0];
    auto px = [&](V3 w, float* X, float* Y) {
      V3 dd = w - fp.cam.pos;
      float z = dot(dd, fp.cam.fwd);
      *X = (dot(dd, fp.cam.right) / (z * fp.cam.tan_half * fp.cam.aspect) + 1.0f) * 0.5f * on.tile.w;
      *Y = (1.0f - dot(dd, fp.cam.up) / (z * fp.cam.tan_half)) * 0.5f * on.tile.h;
    };
    float x0, y0, x1, y1;
    px(g.center + g.up * (-0.3f * g.height) + g.right * (-0.3f * g.height), &x0, &y1);
    px(g.center + g.up * (1.0f * g.height) + g.right * ((g.width + 0.3f) * g.height), &x1, &y0);
    double core = 0, rest = 0, back = 0;
    long nc = 0, nr = 0, nb = 0;
    for (int y = (int)y0; y <= (int)y1; y++)
      for (int x = (int)x0; x <= (int)x1; x++) {
        if (x < 0 || y < 0 || x >= on.tile.w || y >= on.tile.h) continue;
        size_t p = 4 * ((size_t)y * on.tile.w + x);
        float la = (0.2126f * on.rgba[p] + 0.7152f * on.rgba[p + 1] + 0.0722f * on.rgba[p + 2]) / 255.0f;
        float lb = (0.2126f * off.rgba[p] + 0.7152f * off.rgba[p + 1] + 0.0722f * off.rgba[p + 2]) / 255.0f;
        back += lb;
        nb++;
        if (la > lb + 0.1f) { core += la; nc++; } else { rest += la; nr++; }
      }
    double contrast = nc && nr ? core / nc - rest / nr : 0.0;
    double behind = nb ? back / nb : 0.0;
    CHECK(behind > 0.5, "the clock's box is over cloud (mean luminance behind it %.2f)", behind);
    CHECK(nc > 150 && contrast >= 0.3, "the clock reads over cloud: %ld letter pixels, contrast %.2f", nc, contrast);
  }

  // 10. Image quality while the camera moves. The directed camera's flight
  // from peak-seam to sunset-ridge, at the wall's sample rate (1 sample per
  // pixel per frame, 12 frames of history), drawn whole and compared with the
  // same pose and scene time held to 1024 samples. "grain" is the RMS of the
  // high-frequency part of the luminance error (the error minus its 5 x 5
  // mean); "rmse" is over RGB and also counts clouds that history drags
  // behind the camera. Both on 8-bit values.
  {
    Scene f = s;
    f.words.clear();
    f.lights.clear();
    f.time_of_day = 0.735;
    Keyframe a{0, {-47, 2.6, -101}, {-56, 2.75, -114.75}, 55}, b{20, {-36, 2.4, -70}, {-73.5, 2.4, -121.75}, 60};
    Keyframe a2 = a, b2 = b;
    a2.t = 7;
    b2.t = 27;
    f.camera = {a, a2, b, b2};
    const long kq = (long)std::llround(13.5 * 30);
    std::vector<std::pair<double, int>> seq;
    for (int k = 11; k >= 0; k--) seq.push_back({(kq - k) / 30.0, 1});
    r.set_text_overlay(false);
    r.set_quality(0);
    Img mv = draw(r, f, Rect{0, 0, 1, 1}, 2 * h, seq, true);
    Scene held = held_reference(f, kq / 30.0);
    std::vector<std::pair<double, int>> hold;
    for (int n = 0; n < 16; n++) hold.push_back({kq / 30.0, 64});
    Img ref = draw(r, held, Rect{0, 0, 1, 1}, 2 * h, hold, true);
    CHECK(r.stats().hold && r.stats().hold_samples == 1024, "the reference is a hold of 1024 samples (%d)", r.stats().hold_samples);
    r.set_text_overlay(true);
    r.set_quality(-1);
    int W = mv.tile.w, H = mv.tile.h;
    std::vector<double> le((size_t)W * H);
    double se = 0;
    for (int p = 0; p < W * H; p++) {
      double l = 0;
      for (int c = 0; c < 3; c++) {
        double e = (double)mv.rgba[4 * p + c] - ref.rgba[4 * p + c];
        se += e * e;
        l += e * (c == 0 ? 0.2126 : c == 1 ? 0.7152 : 0.0722);
      }
      le[p] = l;
    }
    double hf = 0;
    long nh = 0;
    for (int y = 2; y < H - 2; y++)
      for (int x = 2; x < W - 2; x++) {
        double m = 0;
        for (int dy = -2; dy <= 2; dy++)
          for (int dx = -2; dx <= 2; dx++) m += le[(size_t)(y + dy) * W + x + dx];
        double d = le[(size_t)y * W + x] - m / 25;
        hf += d * d;
        nh++;
      }
    double rmse = std::sqrt(se / (3.0 * W * H)), grain = std::sqrt(hf / nh);
    fprintf(stderr, "moving flight against a 1024-sample hold: rmse %.2f, grain %.2f\n", rmse, grain);
    CHECK(grain < GRAIN_MAX, "moving frame's grain against the reference: %.2f (< %.1f)", grain, GRAIN_MAX);
    CHECK(rmse < RMSE_MAX, "moving frame's error against the reference: rmse %.2f (< %.1f)", rmse, RMSE_MAX);
  }

  // 10b. Speckle while the camera moves: single pixels far from all their
  // neighbours, which a viewer sees as dithered dots and which H.264 turns
  // into blotches. Counted as the share of pixels whose luminance is more
  // than 12 (8-bit) from the median of their 3 x 3 neighbourhood, in a
  // moving frame at 1 sample per pixel with 12 frames of history, against the
  // same pose held to 1024 samples. A sunset cloud deck seen from above (the
  // first hit flips between a cloud and the ground behind it at every cloud
  // edge) and snow and rock under a low sun (detail normals turn sharply
  // between neighbours). Bounds: SPECKLE_* (top of this file).
  {
    const char* name[] = {"cloud deck", "snow and rock"};
    const Keyframe cam[2][2] = {{{0, {-58, 7, -79}, {-76, 5.2, -93}, 62}, {11, {-62, 7, -81}, {-82, 5.2, -93}, 62}},
                                {{0, {-47, 2.6, -101}, {-56, 2.75, -114.75}, 55}, {10, {-50, 2.6, -100}, {-56, 2.75, -114.75}, 55}}};
    const double tod[2] = {0.78, 0.79}, bound[2] = {SPECKLE_CLOUDS, SPECKLE_SNOW};
    for (int c = 0; c < 2; c++) {
      Scene f = s;
      f.words.clear();
      f.lights.clear();
      f.time_of_day = tod[c];
      f.camera = {cam[c][0], cam[c][1]};
      if (c == 1) f.weather = {WeatherKey{0, 0.5, 0, 0, 1.2}};
      const long kq = 150;
      std::vector<std::pair<double, int>> seq;
      for (int k = 11; k >= 0; k--) seq.push_back({(kq - k) / 30.0, 1});
      r.set_text_overlay(false);
      r.set_quality(0);
      Img mv = draw(r, f, Rect{0, 0, 1, 1}, 2 * h, seq, true);
      Scene held = held_reference(f, kq / 30.0);
      std::vector<std::pair<double, int>> hold;
      for (int n = 0; n < 16; n++) hold.push_back({kq / 30.0, 64});
      Img ref = draw(r, held, Rect{0, 0, 1, 1}, 2 * h, hold, true);
      r.set_text_overlay(true);
      r.set_quality(-1);
      auto speckle = [](const Img& im) {
        int W = im.tile.w, H = im.tile.h;
        std::vector<float> l((size_t)W * H);
        for (int p = 0; p < W * H; p++)
          l[p] = 0.2126f * im.rgba[4 * p] + 0.7152f * im.rgba[4 * p + 1] + 0.0722f * im.rgba[4 * p + 2];
        long n = 0, out = 0;
        for (int y = 1; y < H - 1; y++)
          for (int x = 1; x < W - 1; x++) {
            float v[9];
            for (int k = 0; k < 9; k++) v[k] = l[(size_t)(y + k / 3 - 1) * W + x + k % 3 - 1];
            std::nth_element(v, v + 4, v + 9);
            out += std::fabs(l[(size_t)y * W + x] - v[4]) > 12.0f;
            n++;
          }
        return 100.0 * out / n;
      };
      double sm = speckle(mv), sr = speckle(ref);
      fprintf(stderr, "%s, moving at 1 sample per pixel: speckle %.3f%% of pixels (the 1024-sample hold: %.3f%%)\n", name[c], sm, sr);
      CHECK(sm < bound[c], "%s: speckle %.3f%% (< %.2f%%)", name[c], sm, bound[c]);
    }
  }

  // 10c. A lockup word where a ridge cuts it, at the wall's scale (one tile of
  // a 4 x 4 wall at h=432), 1 sample per pixel with history, the camera all
  // but still. Drawn with and without the word; the word's effect is the
  // luminance difference. (a) The cut stays put: pixels whose effect changes
  // by more than 40 between two consecutive frames (one jittered sample per
  // pixel decided the cut, and it moved every frame). (b) No bright rim: the
  // mean effect 1 to 2 pixels outside the letters, over dark ground and sky,
  // less the mean 4 to 8 pixels out (a glow added with a 0.05 em falloff drew
  // a rim hugging each letter, kept by the encoder as a hard outline).
  {
    Scene f = s;
    f.lights.clear();
    f.time_of_day = 0.77;
    f.camera = {{0, {-55.0, 2.0, -113.0}, {-57.5, 2.3, -115.5}, 60}, {10, {-54.999, 2.0, -113.0}, {-57.5, 2.3, -115.5}, 60}};
    f.words = {{"Northern lights", 0.0, TEXT_STYLE_LOCKUP}};
    Scene nw = f;
    nw.words.clear();
    const Rect cell{0.25, 0, 0.25, 0.25};
    auto lum_of = [&](const Scene& sc, long k) {
      std::vector<std::pair<double, int>> seq;
      for (int j = 11; j >= 0; j--) seq.push_back({(k - j) / 30.0, 1});
      Img im = draw(r, sc, cell, 432, seq, true);
      std::vector<float> l(im.rgba.size() / 4);
      for (size_t p = 0; p < l.size(); p++) l[p] = 0.2126f * im.rgba[4 * p] + 0.7152f * im.rgba[4 * p + 1] + 0.0722f * im.rgba[4 * p + 2];
      return std::make_pair(l, im.tile);
    };
    auto [on_a, tl] = lum_of(f, 150);
    auto on_b = lum_of(f, 151).first;
    auto off_a = lum_of(nw, 150).first;
    auto off_b = lum_of(nw, 151).first;
    int W = tl.w, H = tl.h;
    long word = 0, flick = 0;
    std::vector<float> fx;
    for (int p = 0; p < W * H; p++) {
      float ea = on_a[p] - off_a[p], eb = on_b[p] - off_b[p];
      if (std::fabs(ea) <= 4.0f && std::fabs(eb) <= 4.0f) continue;
      word++;
      flick += std::fabs(ea - eb) > 40.0f;
      fx.push_back(on_a[p]);
    }
    std::sort(fx.begin(), fx.end());
    float fill_lum = fx.empty() ? 1e9f : 0.9f * fx[fx.size() * 95 / 100];
    std::vector<uint8_t> fill(W * H), d1, d2, d4, d8;
    for (int p = 0; p < W * H; p++) fill[p] = on_a[p] > fill_lum;
    auto dilate = [&](const std::vector<uint8_t>& m, int n) {
      std::vector<uint8_t> a = m, b(m.size());
      for (int it = 0; it < n; it++) {
        for (int y = 0; y < H; y++)
          for (int x = 0; x < W; x++) {
            int p = y * W + x;
            b[p] = a[p] || (x > 0 && a[p - 1]) || (x + 1 < W && a[p + 1]) || (y > 0 && a[p - W]) || (y + 1 < H && a[p + W]);
          }
        a.swap(b);
      }
      return a;
    };
    d1 = dilate(fill, 1); d2 = dilate(fill, 2); d4 = dilate(fill, 4); d8 = dilate(fill, 8);
    double near = 0, far = 0;
    long nn = 0, nf = 0;
    for (int p = 0; p < W * H; p++) {
      if (off_a[p] >= 120.0f) continue;
      float e = on_a[p] - off_a[p];
      if (d2[p] && !d1[p]) { near += e; nn++; }
      if (d8[p] && !d4[p]) { far += e; nf++; }
    }
    double rim = (nn ? near / nn : 0) - (nf ? far / nf : 0);
    fprintf(stderr, "a word behind a ridge: %ld word pixels, %ld flicker between frames; rim %.2f (%.2f near, %.2f far, %ld and %ld pixels)\n",
            word, flick, rim, nn ? near / nn : 0, nf ? far / nf : 0, nn, nf);
    CHECK(word > 20000 && nn > 500 && nf > 1000, "the word is drawn over dark ground (%ld pixels; rings %ld, %ld)", word, nn, nf);
    CHECK(flick <= TEXT_FLICKER_MAX, "the word's cut: %ld pixels flicker between frames (<= %d)", flick, TEXT_FLICKER_MAX);
    CHECK(rim < TEXT_RIM_MAX, "the word's glow: a rim %.2f brighter than its outer glow (< %.1f)", rim, TEXT_RIM_MAX);
  }

  // 11. A stream taller than the drawn frame (an opened tile asks 720 while
  // its frame is drawn at a wall height) is interpolated, not magnified into
  // squares. The whole image drawn at 2h and scaled to a stream 2.5 times as
  // tall: with nearest-pixel scaling every change of the luma along a row
  // falls between two stream pixels that read different drawn pixels; with
  // interpolation the change spreads over the pixels between (about 1 in
  // 2.5 pairs straddles a drawn pixel's edge).
  {
    Scene f = s;
    f.words.clear();
    Tile src = tile_geometry(Rect{0, 0, 1, 1}, f.image_aspect, 2 * h);
    r.reset_history();
    r.render(f, src, tm, false, 1);
    int sh = 5 * h;
    Tile dst = tile_geometry(Rect{0, 0, 1, 1}, f.image_aspect, sh);
    size_t pitch = 0;
    void* nv = r.alloc_nv12(dst.w, dst.h, &pitch);
    r.to_nv12(nv, pitch, dst.w, dst.h, 0, 0, dst.full_h);
    std::vector<uint8_t> y((size_t)dst.w * dst.h);
    cudaMemcpy2D(y.data(), dst.w, nv, pitch, dst.w, dst.h, cudaMemcpyDeviceToHost);
    r.free_dev(nv);
    double sc = (double)src.full_h / dst.full_h, at_edge = 0, all = 0;
    for (int j = 0; j < dst.h; j++)
      for (int i = 0; i + 1 < dst.w; i++) {
        double dv = std::abs((int)y[(size_t)j * dst.w + i + 1] - (int)y[(size_t)j * dst.w + i]);
        all += dv;
        if (std::floor(i * sc + 0.5 * sc) != std::floor((i + 1) * sc + 0.5 * sc)) at_edge += dv;
      }
    double share = all > 0 ? at_edge / all : 1.0;
    fprintf(stderr, "stream at %.2fx the drawn height: %.2f of the luma's change along rows is at drawn pixels' edges\n", 1 / sc, share);
    CHECK(share < 0.6, "a taller stream is interpolated: %.2f of the change at drawn pixels' edges (< 0.6)", share);
  }

  // 7. The encoder's SPS, in the sequence header and in a keyframe, carries a VUI
  // bitstream_restriction with max_num_reorder_frames 0. Without it a decoder
  // holds the level's MaxDpbFrames (Chrome on macOS: 7 or 8 frames at the
  // wall's tile sizes) before outputting each frame. Every quality, at tile
  // sizes that select levels 2.0 to 4.2.
  for (int q = 0; q < 3; q++)
    for (auto wh : {std::pair<int, int>{336, 196}, {656, 376}, {976, 556}, {1936, 1096}}) {
      size_t pitch = 0;
      void* nv12 = r.alloc_nv12(wh.first, wh.second, &pitch);
      H264Encoder enc;
      bool ok = enc.open(r.cu_context(), nv12, pitch, wh.first, wh.second, 30, q, &err);
      CHECK(ok, "encoder %dx%d q%d: %s", wh.first, wh.second, q, err.c_str());
      if (ok) {
        const SpsReorder& h0 = enc.reorder();
        CHECK(h0.parsed && h0.restriction && h0.max_num_reorder_frames == 0 &&
                  h0.max_dec_frame_buffering == h0.max_num_ref_frames,
              "%dx%d q%d sequence header: parsed %d restriction %d reorder %u dec_frame_buffering %u refs %u", wh.first,
              wh.second, q, (int)h0.parsed, (int)h0.restriction, h0.max_num_reorder_frames, h0.max_dec_frame_buffering,
              h0.max_num_ref_frames);
        std::vector<uint8_t> au;
        bool key = false;
        ok = enc.encode(true, &au, &key, &err);
        CHECK(ok && key, "keyframe %dx%d q%d: %s", wh.first, wh.second, q, err.c_str());
        size_t len = 0;
        long off = find_sps(au.data(), au.size(), &len);
        SpsReorder k = off >= 0 ? sps_reorder(au.data() + off, len) : SpsReorder();
        CHECK(k.parsed && k.restriction && k.max_num_reorder_frames == 0 && k.max_dec_frame_buffering == k.max_num_ref_frames,
              "%dx%d q%d keyframe SPS: parsed %d restriction %d reorder %u dec_frame_buffering %u refs %u", wh.first,
              wh.second, q, (int)k.parsed, (int)k.restriction, k.max_num_reorder_frames, k.max_dec_frame_buffering,
              k.max_num_ref_frames);
        if (q == 1) fprintf(stderr, "encoder %dx%d %s: max_num_ref_frames %u, max_dec_frame_buffering %u\n", wh.first,
                            wh.second, enc.codec().c_str(), k.max_num_ref_frames, k.max_dec_frame_buffering);
      }
      r.free_dev(nv12);
    }
  DONE();
}
