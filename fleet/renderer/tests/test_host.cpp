// Host-side tests: no GPU needed. Scene contract parsing, camera interpolation
// and holds, tile geometry, wire formats.
#include <cmath>
#include <cstring>
#include <string>
#include <vector>

#include "../src/budget.h"
#include "../src/jpeg.h"
#include "../src/json.h"
#include "../src/nvenc.h"
#include "../src/proto.h"
#include "../src/scene.h"
#include "../src/world.h"
#include "check.h"
#include "tls_check.h"

static const char* kContractExample =
    R"({"epoch_unix_ms": 1761100000000, "rect": {"x":0.5,"y":0.0,"w":0.0625,"h":0.111},
        "image_aspect": 1.7778,
        "camera": [{"t": 0.0, "pos": [0,1,0], "look": [0,1,10], "fov_deg": 60.0},
                   {"t": 10.0, "pos": [5,1,0], "look": [5,1,10], "fov_deg": 60.0}],
        "time_of_day": 0.35, "words": [{"text":"HELLO","t":12.0}],
        "lights": [{"pos":[1,1,1],"color":[1,0.5,0.2],"t":30.0}]})";

static Scene two_holds() {
  Scene s;
  s.rect = {0, 0, 1, 1};
  auto K = [&](double t, double x) { s.camera.push_back(Keyframe{t, {x, 1, 0}, {x, 1, 10}, 60}); };
  K(0, 0); K(5, 0);   // hold 0..5
  K(10, 10);          // move
  K(15, 10);          // hold 10..15 (and after)
  return s;
}

int main() {
  // --- WebSocket handshake (RFC 6455 section 1.3 example)
  CHECK(ws_accept("dGhlIHNhbXBsZSBub25jZQ==") == "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=", "got %s",
        ws_accept("dGhlIHNhbXBsZSBub25jZQ==").c_str());
  CHECK(base64("f") == "Zg==" && base64("fo") == "Zm8=" && base64("foo") == "Zm9v", "base64");
  {
    std::string f = ws_frame(2, std::string(300, 'x').data(), 300);
    CHECK((uint8_t)f[0] == 0x82 && (uint8_t)f[1] == 126 && (uint8_t)f[2] == 1 && (uint8_t)f[3] == 44 && f.size() == 304,
          "ws frame with 16-bit length");
  }

  // --- FCVF header: exact bytes and round trip
  {
    uint8_t h[FCVF_HEADER_BYTES];
    fcvf_header(h, true, 0x0102, 0x0304, 12.5, 0xA1B2C3D4u);
    const uint8_t expect_t[8] = {0, 0, 0, 0, 0, 0, 0x29, 0x40};  // 12.5 as f64 LE
    CHECK(memcmp(h, "FCVF", 4) == 0 && h[4] == 1 && h[5] == 1, "magic/version/flags");
    CHECK(h[6] == 0x02 && h[7] == 0x01 && h[8] == 0x04 && h[9] == 0x03, "width/height LE");
    CHECK(memcmp(h + 10, expect_t, 8) == 0, "t f64 LE");
    CHECK(h[18] == 0xD4 && h[19] == 0xC3 && h[20] == 0xB2 && h[21] == 0xA1, "frame u32 LE");
    FcvfHeader p;
    CHECK(fcvf_parse(h, sizeof h, &p) && p.keyframe && p.w == 0x0102 && p.h == 0x0304 && p.t == 12.5 &&
              p.frame == 0xA1B2C3D4u,
          "round trip");
  }

  // --- codec string from an SPS
  {
    const uint8_t au[] = {0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0, 0, 0, 1, 0x68, 0xEB, 0xE3};
    size_t len = 0;
    long off = find_sps(au, sizeof au, &len);
    CHECK(off == 4 && len == 6, "find_sps off=%ld len=%zu", off, len);
    CHECK(avc_codec_string(au + off, len) == "avc1.640028", "%s", avc_codec_string(au + off, len).c_str());
  }

  // --- JPEG: markers, and a flat image codes small (the decode is checked with ffmpeg in the PR)
  {
    std::vector<uint8_t> px(37 * 21 * 4, 128);
    std::string j = jpeg_encode(px.data(), 37, 21, 90);
    CHECK(j.size() > 600 && (uint8_t)j[0] == 0xFF && (uint8_t)j[1] == 0xD8 && (uint8_t)j[j.size() - 2] == 0xFF &&
              (uint8_t)j[j.size() - 1] == 0xD9 && j.size() < 1200,
          "SOI/EOI and size %zu", j.size());
  }

  // --- scene parsing
  {
    Scene s = parse_scene(kContractExample);
    CHECK(s.epoch_unix_ms == 1761100000000LL && s.rect.x == 0.5 && s.rect.w == 0.0625, "fields");
    CHECK(s.camera.size() == 2 && s.words.size() == 1 && s.words[0].text == "HELLO" && s.lights.size() == 1, "lists");
    Scene e = parse_scene(R"({"epoch_unix_ms":0,"rect":{"x":0,"y":0,"w":1,"h":1},"image_aspect":1.7778,
        "camera":[{"t":0,"pos":[0,1,0],"look":[0,1,1],"fov_deg":60}],"time_of_day":0.5,"words":[],"lights":[]})");
    CHECK(e.words.empty() && e.lights.empty(), "empty words and lights parse");
    Scene lc = parse_scene(R"({"epoch_unix_ms":0,"rect":{"x":0,"y":0,"w":1,"h":1},"image_aspect":1.7778,
        "camera":[{"t":0,"pos":[0,1,0],"look":[0,1,1],"fov_deg":60}],"time_of_day":0.5,"words":[{"text":"hi é!","t":1}]})");
    CHECK(lc.words.size() == 1 && lc.words[0].text == "HI !", "text uppercased and filtered: '%s'", lc.words[0].text.c_str());
    auto bad = [](const char* j) {
      try { parse_scene(j); } catch (const std::exception&) { return true; }
      return false;
    };
    CHECK(bad("{}"), "empty object rejected");
    Scene b = parse_scene(R"({"epoch_unix_ms":0,"rect":{"x":0,"y":0,"w":1,"h":1},"image_aspect":1.7,"camera":[{"t":0,"pos":[0,1,0],"look":[0,1,1],"fov_deg":60}],"time_of_day":0.5,"budget":400000})");
    CHECK(b.budget == 400000 && e.budget == 0, "budget parsed, absent means 0");
    CHECK(bad(R"({"epoch_unix_ms":0,"rect":{"x":0,"y":0,"w":1,"h":1},"image_aspect":1.7,"camera":[{"t":0,"pos":[0,1,0],"look":[0,1,1],"fov_deg":60}],"time_of_day":0.5,"budget":5})"),
          "budget below 10000 rejected");
    CHECK(bad(R"({"epoch_unix_ms":0,"rect":{"x":0.9,"y":0,"w":0.2,"h":1},"image_aspect":1.7,"camera":[{"t":0,"pos":[0,1,0],"look":[0,1,1],"fov_deg":60}],"time_of_day":0.5})"),
          "rect outside the image rejected");
    CHECK(bad(R"({"epoch_unix_ms":0,"rect":{"x":0,"y":0,"w":1,"h":1},"image_aspect":1.7,"camera":[],"time_of_day":0.5})"),
          "no keyframes rejected");
    CHECK(bad(R"({"epoch_unix_ms":0.5,"rect":{"x":0,"y":0,"w":1,"h":1},"image_aspect":1.7,"camera":[{"t":0,"pos":[0,1,0],"look":[0,1,1],"fov_deg":60}],"time_of_day":0.5})"),
          "fractional epoch rejected");
    CHECK(bad(R"({"epoch_unix_ms":0,"rect":{"x":0,"y":0,"w":1,"h":1},"image_aspect":1.7,"camera":[{"t":0,"pos":[0,1],"look":[0,1,1],"fov_deg":60}],"time_of_day":0.5})"),
          "pos with 2 numbers rejected");
  }

  // --- camera: keyframes are hit exactly, outside the range the nearest holds, holds detected
  {
    Scene s = two_holds();
    CamPose a = camera_at(s.camera, -3);
    CHECK(a.held && a.pos[0] == 0 && a.hold_start == 0, "before the first keyframe holds it");
    CamPose b = camera_at(s.camera, 2.5);
    CHECK(b.held && b.hold_start == 0 && b.pos[0] == 0, "equal keyframes 0..5 are a hold from t=0");
    CamPose c = camera_at(s.camera, 7.5);
    CHECK(!c.held && c.pos[0] > 0 && c.pos[0] < 10, "moving between 5 and 10: x=%f", c.pos[0]);
    CamPose d = camera_at(s.camera, 10);
    CHECK(d.held && d.pos[0] == 10 && d.hold_start == 10, "keyframe at 10 starts the hold");
    CamPose e = camera_at(s.camera, 99);
    CHECK(e.held && e.pos[0] == 10 && e.hold_start == 10, "after the last keyframe the trailing hold continues");
    // eases into and out of holds: velocity near the ends of the move is small
    double v0 = (camera_at(s.camera, 5.01).pos[0] - camera_at(s.camera, 5.0).pos[0]) / 0.01;
    double vm = (camera_at(s.camera, 7.51).pos[0] - camera_at(s.camera, 7.5).pos[0]) / 0.01;
    CHECK(v0 < 0.1 && vm > 2.0, "ease out of a hold: v0=%f vmid=%f", v0, vm);
    // Catmull-Rom passes through an interior keyframe of a moving run
    Scene m;
    for (int i = 0; i < 4; i++) m.camera.push_back(Keyframe{(double)i, {(double)i * i, 1, 0}, {0, 1, 10}, 60});
    CHECK(std::fabs(camera_at(m.camera, 2.0).pos[0] - 4.0) < 1e-12, "passes through keyframes");
    CHECK(std::fabs(camera_at(m.camera, 1.5).fov_deg - 60) < 1e-12, "fov linear");
  }

  // --- tile geometry: a grid of rects covers the whole-image pixel grid with no gaps
  {
    for (int h : {135, 136, 180, 240, 360}) {
      int cols = 16, rows = 9;
      Tile first = tile_geometry(Rect{0, 0, 1.0 / cols, 1.0 / rows}, 16.0 / 9, h);
      bool ok = true;
      for (int r = 0; r < rows; r++)
        for (int c = 0; c < cols; c++) {
          Tile t = tile_geometry(Rect{(double)c / cols, (double)r / rows, 1.0 / cols, 1.0 / rows}, 16.0 / 9, h);
          if (t.full_w != first.full_w || t.full_h != first.full_h) ok = false;
          if (t.w % 2 || t.h % 2) ok = false;
          if (c + 1 < cols) {
            Tile n = tile_geometry(Rect{(double)(c + 1) / cols, (double)r / rows, 1.0 / cols, 1.0 / rows}, 16.0 / 9, h);
            if (n.x0 > t.x0 + t.w || n.x0 < t.x0 + t.w - 1) ok = false;  // no gap, at most one pixel of overlap
          } else if (t.x0 + t.w < t.full_w) ok = false;
          if (r + 1 < rows) {
            Tile n = tile_geometry(Rect{(double)c / cols, (double)(r + 1) / rows, 1.0 / cols, 1.0 / rows}, 16.0 / 9, h);
            if (n.y0 > t.y0 + t.h || n.y0 < t.y0 + t.h - 1) ok = false;
          } else if (t.y0 + t.h < t.full_h) ok = false;
        }
      CHECK(ok, "16x9 grid at h=%d tiles the whole image", h);
    }
    Tile whole = tile_geometry(Rect{0, 0, 1, 1}, 16.0 / 9, 540);
    Tile q = tile_geometry(Rect{0.5, 0.5, 0.5, 0.5}, 16.0 / 9, 270);
    CHECK(whole.full_w == q.full_w && whole.full_h == q.full_h && q.x0 == 480 && q.y0 == 270 && q.w == 480 && q.h == 270,
          "2x2 quarter: full %dx%d at %d,%d size %dx%d", q.full_w, q.full_h, q.x0, q.y0, q.w, q.h);
  }

  // --- the hold seed is a function of the scene and the hold, never of the rect or of when we start
  {
    Scene s = two_holds();
    s.time_of_day = 0.4;
    s.image_aspect = 16.0 / 9;
    uint64_t k1, k2, k3, k4;
    bool h1, h2, h3, h4;
    Tile a = tile_geometry(Rect{0, 0, 0.5, 0.5}, 16.0 / 9, 270);
    Tile b = tile_geometry(Rect{0.5, 0.5, 0.5, 0.5}, 16.0 / 9, 270);
    FrameParams pa = build_frame(s, a, 1.0, &k1, &h1);
    FrameParams pb = build_frame(s, b, 4.0, &k2, &h2);
    CHECK(h1 && h2 && k1 == k2 && pa.seed == pb.seed && pa.layer_t == pb.layer_t, "same hold, other rect and t: same seed");
    FrameParams pc = build_frame(s, a, 12.0, &k3, &h3);
    CHECK(h3 && k3 != k1, "another hold has another seed");
    s.time_of_day = 0.41;
    build_frame(s, a, 1.0, &k4, &h4);
    CHECK(k4 != k1, "time of day is part of the hold");
    uint64_t km;
    bool hm;
    FrameParams pm = build_frame(s, a, 7.0, &km, &hm);
    CHECK(!hm && km == 0 && pm.hold == 0, "moving camera is not a hold");
    (void)pc;
  }
  {
    // Lakes: somewhere in the basins around the golden-hour view the water
    // stands above the carved floor, and an eye placed at the lake's level is
    // lifted 60 m above the water, not above the lake bed.
    const V4* tab = host_noise().tab.data();
    float bx = 0, bz = 0, bw = 0, bg = 0;
    bool found = false;
    for (float z = -120.0f; z <= -60.0f && !found; z += 0.25f)
      for (float x = -60.0f; x <= 0.0f && !found; x += 0.25f) {
        float w;
        float g = terrain_from(v2(x, z), macro_direct(v2(x, z), tab), 9, tab, &w);
        if (w > 0.0f && g < w - 0.005f) { found = true; bx = x; bz = z; bw = w; bg = g; }
      }
    CHECK(found, "a lake (water above its bed, above sea level) near the golden-hour view");
    CHECK(surface_h(v2(bx, bz), 9, tab) == bw, "the surface over a lake is its water");
    Scene s;
    s.rect = {0, 0, 1, 1};
    s.camera.push_back(Keyframe{0, {bx, bg, bz}, {bx + 5, bg, bz + 5}, 60});
    uint64_t k;
    bool held;
    Tile t = tile_geometry(s.rect, 16.0 / 9, 180);
    FrameParams p = build_frame(s, t, 0.0, &k, &held);
    double alt = p.cam.pos.y + (bx * bx + bz * bz) / (2.0 * PLANET_R);
    CHECK(alt >= bw + 0.0599, "an eye at a lake is lifted 60 m above its water");
  }
  {
    // The sky clock in Pacific time across both changes of 2026 and 2027, at
    // the exact seconds (values from tzdata's America/Los_Angeles).
    struct { int64_t t; const char* want; } c[] = {
        {1772963999, "01:59:59 PST"}, {1772964000, "03:00:00 PDT"},   // 2026-03-08 10:00 UTC
        {1793523599, "01:59:59 PDT"}, {1793523600, "01:00:00 PST"},   // 2026-11-01 09:00 UTC
        {1805018399, "01:59:59 PST"}, {1805018400, "03:00:00 PDT"},   // 2027-03-14
        {1825577999, "01:59:59 PDT"}, {1825578000, "01:00:00 PST"},   // 2027-11-07
        {1767225600, "16:00:00 PST"},                                 // 2026-01-01 00:00 UTC
        {1784160000, "17:00:00 PDT"},                                 // 2026-07-16 00:00 UTC
    };
    for (auto& k : c) {
      std::string got = pacific_clock(k.t);
      CHECK(got == k.want, "pacific_clock(%lld) = %s, want %s", (long long)k.t, got.c_str(), k.want);
    }
  }
  {
    // The grid-wide budget: a schedule gives every slot one budget, the same
    // for every tile that was given the same entries; the fit and the ladder.
    BudgetSchedule a, b;
    double x, y;
    CHECK(!a.at(10, &x), "an empty schedule names no budget");
    a.add(100, 4e5);
    a.add(160, 8e5);
    b.add(160, 8e5);  // posted in another order
    b.add(100, 4e5);
    bool same = true;
    for (long long k = 90; k < 200; k++) {
      bool ha = a.at(k, &x), hb = b.at(k, &y);
      same = same && ha == hb && (!ha || x == y);
    }
    CHECK(same, "two tiles with the same entries use the same budget at every slot");
    BudgetSchedule c;
    c.add(100, 4e5);
    c.add(160, 8e5);
    CHECK(c.at(159, &x) && x == 4e5 && c.at(160, &x) && x == 8e5, "a budget takes effect exactly at its slot");
    c.add(200, 9e5);
    c.add(200, 5e5);
    CHECK(c.at(210, &x) && x == 5e5, "a later entry for the same slot replaces it");
    CHECK(std::fabs(budget_fit(1e6, 20.0, 1000.0 / 30) - 1e6 * 0.7 * (1000.0 / 30) / 20.0) < 1, "fit at 70%% of a slot");
    CHECK(ladder_floor(250000) == 250000 && ladder_floor(297302) == 297302 && ladder_floor(297300) == 250000 &&
              ladder_floor(1e6) == 1000000 && ladder_floor(10) == 62500,
          "ladder: %.0f %.0f %.0f %.0f", ladder_floor(250000), ladder_floor(297300), ladder_floor(1e6), ladder_floor(10));
    // A renderer alone at 1280x720 (about 1.08 M drawn pixels) whose fit is
    // 406 k samples: a stride-2 budget would draw 540 k, so it stays on
    // stride 4 (the 720p row of round 8's first run sat on stride 2 at 31 ms).
    double px720 = 1.08e6;
    CHECK(samples_for(local_budget(406000, px720), px720) <= 406000 && stride_for(local_budget(406000, px720) / px720) == 4,
          "a stride's share is not chosen over the fit (budget %.0f)", local_budget(406000, px720));
    CHECK(local_budget(5e6, 50000) == ladder_floor(5e6), "at many samples per pixel the ladder rung stands");
    // The cost model: 50,000 drawn pixels, a fixed 4 ms, the fast path 40 ns
    // per sample. Slot 33.3 ms, target 23.3 ms: 19.3 ms of samples.
    CostModel m;
    m.hq_prior = 2.0;
    m.observe(false, 250000, 10.0, 14.0);  // 250 k samples in 10 ms
    double fslot = 1000.0 / 30;
    // the high-quality path at 80 ns per sample (the prior) needs 400 k = 8 spp
    // for 32 ms: unaffordable, so the fit stays just under 8 spp on the fast path
    double f1 = m.fit_now(50000, 8, fslot);
    CHECK(f1 < 8 * 50000 && f1 > 7.9 * 50000, "fast path capped under the high-quality threshold (%.0f)", f1);
    // measured: the high-quality path costs 30 ns per sample here, so 19.3 ms buys 644 k
    m.observe(true, 500000, 15.0, 19.0);
    double f2 = m.fit_now(50000, 8, fslot);
    CHECK(std::fabs(f2 - (0.7 * fslot - 4.0) / 3e-5) < 1000, "high-quality fit from its own cost (%.0f)", f2);
    // and the frame it predicts lands at 70% of the slot
    CHECK(std::fabs(4.0 + f2 * 3e-5 - 0.7 * fslot) < 0.05, "predicted frame at the target");
  }
  {
    // The windowed fit: a camera path whose cost per sample swings 2x every
    // 20 s (40 ns on easy views, 80 ns on hard ones) at 30 fps. The latest
    // frames' fit swings 2x with it; the window's holds at the hard views.
    CostModel w;
    const double fslot = 1000.0 / 30, px = 50000;
    double lo_now = 1e18, hi_now = 0, lo_w = 1e18, hi_w = 0, first_w = -1, early = 0;
    for (int f = 0; f < 30 * 300; f++) {
      double t = f / 30.0;
      double c = (int(t / 20) % 2) ? 8e-5 : 4e-5;
      w.observe(false, 200000, 200000 * c, 200000 * c + 4.0, t);
      double fn = w.fit_now(px, 8, fslot), fw = w.fit(px, 8, fslot, t);
      if (t < 59.9) early = std::max(early, fw);
      if (fw > 0 && first_w < 0) first_w = t;
      if (t >= 120) {
        lo_now = std::min(lo_now, fn); hi_now = std::max(hi_now, fn);
        lo_w = std::min(lo_w, fw); hi_w = std::max(hi_w, fw);
      }
    }
    CHECK(early == 0 && first_w >= 59.9 && first_w < 62, "the windowed fit is known from 60 s (%.1f)", first_w);
    CHECK(hi_now / lo_now > 1.8, "the latest frames' fit swings with the path (%.0f to %.0f)", lo_now, hi_now);
    CHECK(hi_w / lo_w < 1.01, "the window's fit holds (%.0f to %.0f)", lo_w, hi_w);
    CHECK(std::fabs(lo_w - (0.7 * fslot - 4.0) / 8e-5) < 2000, "at the hard views' cost (%.0f)", lo_w);
  }
  {
    // Layout generations: the coordinator's scene body (fleet/PROTOCOL.md on
    // fleet-control: "layout_generation", "rect_from_frame", "previous_rect").
    std::string b = R"({"epoch_unix_ms": 1, "rect": {"x": 0.25, "y": 0, "w": 0.25, "h": 1}, "image_aspect": 1.7778,
        "camera": [{"t": 0, "pos": [0,1,0], "look": [0,1,10], "fov_deg": 60}], "time_of_day": 0.3,
        "layout_generation": 2, "rect_from_frame": 900, "previous_rect": {"x": 0.5, "y": 0, "w": 0.5, "h": 0.5}})";
    Scene sc = parse_scene(b);
    CHECK(sc.layout_generation == 2 && sc.rect_from_frame == 900 && sc.has_previous_rect && sc.previous_rect.x == 0.5 &&
              sc.previous_rect.h == 0.5 && sc.rect.x == 0.25,
          "layout fields parsed");
    std::string n = R"({"epoch_unix_ms": 1, "rect": {"x": 0, "y": 0, "w": 1, "h": 1}, "image_aspect": 1.7778,
        "camera": [{"t": 0, "pos": [0,1,0], "look": [0,1,10], "fov_deg": 60}], "time_of_day": 0.3,
        "layout_generation": 3, "rect_from_frame": null, "previous_rect": null})";
    Scene sn = parse_scene(n);
    CHECK(sn.layout_generation == 3 && sn.rect_from_frame == -1 && !sn.has_previous_rect, "null layout fields");
    bool threw = false;
    try {
      parse_scene(R"({"epoch_unix_ms": 1, "rect": {"x": 0, "y": 0, "w": 1, "h": 1}, "image_aspect": 1.7778,
        "camera": [{"t": 0, "pos": [0,1,0], "look": [0,1,10], "fov_deg": 60}], "time_of_day": 0.3,
        "previous_rect": {"x": 0, "y": 0, "w": 1, "h": 1}})");
    } catch (const std::exception&) { threw = true; }
    CHECK(threw, "previous_rect without rect_from_frame is refused");
    // a stream keeps its whole image's height across a re-tile: 2x2 -> 4x1
    Rect a{0, 0, 0.5, 0.5}, c{0, 0, 0.25, 1};
    int full = tile_geometry(a, 16.0 / 9, 270).full_h;
    int h2 = tile_h_for(c, 16.0 / 9, full);
    CHECK(full == 540 && h2 == 540 && tile_geometry(c, 16.0 / 9, h2).full_h == 540, "540 -> tile height %d", h2);
    // a budget posted with a t in the past applies from the next slot
    BudgetSchedule bs;
    double bb;
    bs.add((long long)std::ceil(0.0 * 30), 707107);
    CHECK(bs.at(26000, &bb) && bb == 707107, "a past t applies at once");
    // a scene on the same clock keeps the schedule; on a new clock the budget
    // in force carries over from slot 0; a scene with its own budget clears it
    BudgetSchedule s2;
    s2.add(100, 500000);
    s2.add(400, 353553);
    BudgetSchedule same = schedule_after_scene(s2, true, false, 200);
    CHECK(same.at(200, &bb) && bb == 500000 && same.at(400, &bb) && bb == 353553, "same clock keeps the entries");
    BudgetSchedule fresh = schedule_after_scene(s2, false, false, 200);
    CHECK(fresh.at(0, &bb) && bb == 500000 && fresh.at(400, &bb) && bb == 500000, "new clock carries the budget in force");
    BudgetSchedule none = schedule_after_scene(s2, false, false, 50);
    CHECK(none.size() == 0, "nothing in force, nothing carried");
    CHECK(schedule_after_scene(s2, true, true, 200).size() == 0, "a scene budget clears the schedule");
  }
  tlscheck::run();
  DONE();
}
