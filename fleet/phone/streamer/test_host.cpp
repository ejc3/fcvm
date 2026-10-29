// Host-only tests of the phone streamer: the stamp, the colour conversion, the geometry and
// header, the requests it sends the app, which copies become frames, and the scene it
// accepts. No GPU or emulator.
#include <math.h>
#include <string.h>

#include <string>
#include <vector>

#include "check.h"
#include "json.h"
#include "logic.h"
#include "nv12.h"
#include "stamp.h"

namespace {
std::vector<uint8_t> screen(int w, int h, uint8_t fill) {
  std::vector<uint8_t> v((size_t)w * h * 4, fill);
  for (size_t i = 3; i < v.size(); i += 4) v[i] = 255;
  return v;
}
PhoneScene parsed(const std::string& body) {
  PhoneScene s;
  std::string err;
  if (!phone_scene_parse(body, &s, &err)) fprintf(stderr, "parse: %s\n", err.c_str());
  return s;
}
}  // namespace

int main() {
  const int W = 720, H = 1280;
  {  // the app's stamp reads back, on a portrait and a landscape surface
    for (int rot = 0; rot < 2; rot++) {
      int w = rot ? H : W, h = rot ? W : H;
      for (uint32_t k : {0u, 1u, 30u, 123456u, 0xFFFFFFFFu, 0x80000001u}) {
        auto s = screen(w, h, 90);
        stamp_write(s.data(), w, h, (size_t)w * 4, k);
        uint32_t got = 7;
        CHECK(stamp_read(s.data(), w, h, (size_t)w * 4, &got) && got == k, "%dx%d k=%u got %u", w, h, k, got);
      }
    }
    // the app's own cells: 32 across the width, each w*i/32 to w*(i+1)/32 (720 is not a
    // multiple of 32), most significant bit first
    auto s = screen(W, H, 0);
    for (int x = 0; x < W * 1 / 32; x++)
      for (int y = H - 8; y < H; y++) memset(&s[((size_t)y * W + x) * 4], 255, 3);
    uint32_t k = 0;
    CHECK(stamp_read(s.data(), W, H, (size_t)W * 4, &k) && k == 0x80000000u, "the first cell is the top bit: %08x", k);
  }
  {  // grey along the bottom (the app's tab bar, a screen with no stamp) is no stamp
    auto s = screen(W, H, 128);
    uint32_t k;
    CHECK(!stamp_read(s.data(), W, H, (size_t)W * 4, &k), "a grey band is no stamp");
    stamp_write(s.data(), W, H, (size_t)W * 4, 99);
    uint8_t* mid = &s[((size_t)(H - 4) * W + (size_t)(20 * W / 32 + W / 64)) * 4];
    mid[0] = mid[1] = mid[2] = 128;
    CHECK(!stamp_read(s.data(), W, H, (size_t)W * 4, &k), "one grey cell is no stamp");
  }
  {  // a stamp counts only near the copy's time: the bars of an interface are not frame numbers
    PhoneScene sc = parsed("{\"epoch_unix_ms\":1000000}");
    double now = 1000000 + 60000.0;  // t = 60 s
    CHECK(stamp_plausible(3600, now, sc), "k = 3600 is t 60 s");
    CHECK(stamp_plausible(3600 - 4 * 60, now, sc), "4 s behind");
    CHECK(!stamp_plausible(0, now, sc), "all black is t 0");
    CHECK(!stamp_plausible(0xFFFFFFFFu, now, sc), "all white is far ahead");
    PhoneScene none;
    CHECK(!stamp_plausible(3600, now, none), "no scene, no stamp");
  }

  {  // BT.709 limited range
    struct C { uint8_t r, g, b, y, cb, cr; } cs[] = {
        {255, 255, 255, 235, 128, 128}, {0, 0, 0, 16, 128, 128}, {255, 0, 0, 63, 102, 240},
        {0, 255, 0, 173, 42, 26},       {0, 0, 255, 32, 240, 118}};
    for (auto c : cs) {
      std::vector<uint8_t> px(4 * 4 * 4);
      for (int i = 0; i < 16; i++) { px[i * 4] = c.r; px[i * 4 + 1] = c.g; px[i * 4 + 2] = c.b; px[i * 4 + 3] = 255; }
      std::vector<uint8_t> out(4 * 6);
      rgba_to_nv12(px.data(), 16, 4, 4, out.data(), 4);
      CHECK(abs(out[0] - c.y) <= 1 && abs(out[16] - c.cb) <= 1 && abs(out[17] - c.cr) <= 1,
            "rgb %d,%d,%d -> %d %d %d, want %d %d %d", c.r, c.g, c.b, out[0], out[16], out[17], c.y, c.cb, c.cr);
    }
  }

  {  // geometry: the app's tile geometry at the surface's height; the stamp's rows cropped
    PhoneScene sc = parsed("{\"epoch_unix_ms\":1000,\"rect\":{\"x\":0.25,\"y\":0,\"w\":0.25,\"h\":1},\"image_aspect\":2.25,"
                           "\"layout_generation\":3,\"moves\":[]}");
    PhoneGeo g = phone_geometry(true, 10, W, H, sc);
    CHECK(g.scene && g.w == 720 && g.h == 1280 && g.crop_bottom == 8 && g.full_h == 1280 && g.full_w == 2880 &&
              g.x0 == 720 && g.y0 == 0 && g.layout_generation == 3,
          "scene geo %dx%d crop %d full %dx%d at %d,%d", g.w, g.h, g.crop_bottom, g.full_w, g.full_h, g.x0, g.y0);
    PhoneGeo s = phone_geometry(false, 10, W, H, sc);
    CHECK(!s.scene && s.crop_bottom == 0 && s.full_w == 720 && s.full_h == 1280 && s.x0 == 0 && s.layout_generation == -1,
          "screen geo full %dx%d", s.full_w, s.full_h);
    Json j = Json::parse(phone_header_json(g, "avc1.64001f", 30, "std", 600000));
    CHECK(j.at("width").num == 720 && j.at("height").num == 1280 && j.at("full_width").num == 2880 && j.at("x0").num == 720 &&
              j.at("layout_generation").num == 3 && j.at("phone").at("picture").str == "scene" &&
              j.at("crop").at("bottom").num == 8 && j.at("crop").at("top").num == 0,
          "scene header");
    j = Json::parse(phone_header_json(s, "avc1.64001f", 30, "std", 600000));
    CHECK(j.at("layout_generation").type == Json::Null && j.at("phone").at("picture").str == "screen", "screen header");
  }
  {  // a re-tile: a frame the app drew before rect_from_frame is placed by previous_rect
    PhoneScene sc = parsed("{\"epoch_unix_ms\":1000,\"rect\":{\"x\":0.5,\"y\":0,\"w\":0.5,\"h\":1},\"image_aspect\":1.125,"
                           "\"layout_generation\":4,\"rect_from_frame\":300,\"previous_rect\":{\"x\":0,\"y\":0,\"w\":0.5,\"h\":1}}");
    CHECK(phone_geometry(true, 599, W, H, sc).x0 == 0, "frame 599 (30-grid 299) is the old place");
    CHECK(phone_geometry(true, 600, W, H, sc).x0 == 720, "frame 600 (30-grid 300) is the new place");
    PhoneScene nop = parsed("{\"epoch_unix_ms\":1000,\"rect\":{\"x\":0.5,\"y\":0,\"w\":0.5,\"h\":1},\"image_aspect\":1.125,"
                            "\"rect_from_frame\":300}");
    CHECK(phone_geometry(true, 10, W, H, nop).x0 == 720, "without previous_rect the rect holds throughout");
  }
  {  // the app's whole-image fallback (SceneInput.viewAt): a turned phone, or no image_aspect
    PhoneScene sc = parsed("{\"epoch_unix_ms\":1000,\"rect\":{\"x\":0.5,\"y\":0,\"w\":0.5,\"h\":1},\"image_aspect\":1.125}");
    PhoneGeo g = phone_geometry(true, 10, H, W, sc);  // 1280x720: the phone turned
    CHECK(g.full_w == 1280 && g.full_h == 720 && g.x0 == 0 && g.y0 == 0, "a turned phone is the whole image: %dx%d at %d,%d",
          g.full_w, g.full_h, g.x0, g.y0);
    PhoneScene noa = parsed("{\"epoch_unix_ms\":1000,\"rect\":{\"x\":0.5,\"y\":0,\"w\":0.5,\"h\":1}}");
    g = phone_geometry(true, 10, W, H, noa);
    CHECK(g.full_w == 720 && g.full_h == 1280 && g.x0 == 0, "no image_aspect: the whole image, %dx%d at %d", g.full_w, g.full_h,
          g.x0);
    g = phone_geometry(true, 10, W, H, sc);  // upright: the assigned rect
    CHECK(g.full_w == 1440 && g.x0 == 720, "an upright phone keeps its rect: %d at %d", g.full_w, g.x0);
  }
  {  // the app's replies, read as JSON
    CHECK(app_reply_error("{\"ok\":true}") == "", "an ok reply");
    CHECK(app_reply_error("{\"ok\":true,\"last\":{\"error\":null}}") == "", "an ok reply with an \"error\" key below the top");
    CHECK(app_reply_error("{\"error\":\"name: one of scene, info\"}") == "name: one of scene, info", "an error reply");
    CHECK(app_reply_error("not json").find("not JSON") == 0, "a line that is not JSON");
    CHECK(app_reply_error("[1]").find("not a JSON object") == 0, "a reply that is not an object");
  }
  {  // scenes refused, each naming its field
    PhoneScene sc;
    std::string err;
    CHECK(!phone_scene_parse("{}", &sc, &err) && err.find("epoch_unix_ms") == 0, "%s", err.c_str());
    CHECK(!phone_scene_parse("{\"epoch_unix_ms\":1,\"rect\":{\"x\":0.9,\"y\":0,\"w\":0.2,\"h\":1}}", &sc, &err) &&
              err.find("rect") == 0,
          "%s", err.c_str());
    CHECK(!phone_scene_parse("{\"epoch_unix_ms\":1,\"image_aspect\":40}", &sc, &err) && err.find("image_aspect") == 0, "%s",
          err.c_str());
    CHECK(!phone_scene_parse("{\"epoch_unix_ms\":1,\"layout_generation\":1.5}", &sc, &err) &&
              err.find("layout_generation") == 0,
          "%s", err.c_str());
    CHECK(!phone_scene_parse("{\"epoch_unix_ms\":1,\"previous_rect\":{\"x\":0,\"y\":0,\"w\":1,\"h\":1}}", &sc, &err) &&
              err.find("previous_rect") == 0,
          "%s", err.c_str());
    CHECK(!phone_scene_parse("not json", &sc, &err), "not json");
  }
  {  // what the app is sent: the scene as posted with the stamp asked for, one line; the clock
    PhoneScene sc = parsed("{\"epoch_unix_ms\":1000,\n \"rect\":{\"x\":0,\"y\":0,\"w\":1,\"h\":1},\"words\":[]}");
    std::string line = app_scene_line(sc);
    CHECK(line.back() == '\n' && line.find('\n') == line.size() - 1, "one line: %s", line.c_str());
    Json j = Json::parse(line);
    CHECK(j.at("type").str == "scene" && j.at("stamp").b && j.at("epoch_unix_ms").num == 1000 && j.has("words"), "%s",
          line.c_str());
    j = Json::parse(app_scene_line(parsed("{ \"epoch_unix_ms\":5 }")));
    CHECK(j.at("type").str == "scene", "a minimal body");
    j = Json::parse(app_clock_line(1761100012345.0));
    CHECK(j.at("type").str == "clock" && j.at("unix_ms").num == 1761100012345.0, "%s", app_clock_line(1761100012345.0).c_str());
  }
  {  // the gate: the wall's grid of the app's frames, each once; the screen at most fps a second
    PhoneScene sc = parsed("{\"epoch_unix_ms\":1000}");
    FrameGate g(30);
    double t = -1;
    CHECK(g.admit(true, 120, 3000, sc, &t) && t == 2.0, "frame 120 is t 2, got %f", t);
    CHECK(!g.admit(true, 120, 3016, sc, &t), "the same frame again is a repeat");
    CHECK(!g.admit(true, 121, 3020, sc, &t), "an odd frame is between the wall's");
    CHECK(g.admit(true, 122, 3033, sc, &t) && fabs(t - 122.0 / 60) < 1e-12, "frame 122");
    CHECK(g.admit(false, 0, 4000, sc, &t) && t == 3.0, "screen at capture 4000 is slot 90, t %f", t);
    CHECK(!g.admit(false, 0, 4010, sc, &t), "a second screen copy in the same slot waits");
    CHECK(g.admit(false, 0, 4040, sc, &t) && fabs(t - 91.0 / 30) < 1e-12, "the next slot");
    CHECK(g.admit(true, 122, 4050, sc, &t), "a scene after the screen starts again, even at an old frame");
    CHECK(g.odd == 1 && g.repeats == 2 && g.scene == 3 && g.screen == 2, "counts odd %llu repeats %llu scene %llu screen %llu",
          (unsigned long long)g.odd, (unsigned long long)g.repeats, (unsigned long long)g.scene, (unsigned long long)g.screen);
  }
  {  // a scene posted while the screen streams: its clock's slots are far below the old ones
    PhoneScene sc = parsed("{\"epoch_unix_ms\":0}");
    FrameGate g(30);
    double t = -1;
    CHECK(g.admit(false, 0, 1000000, sc, &t) && t == 1000.0, "screen with the old clock, t %f", t);
    sc = parsed("{\"epoch_unix_ms\":990000}");
    CHECK(g.admit(false, 0, 1000040, sc, &t) && fabs(t - 10.0333333) < 1e-6, "after the new scene, t %f", t);
    CHECK(!g.admit(false, 0, 1000050, sc, &t), "and its slots are still once each");
  }
  {  // percentiles
    Samples s;
    CHECK(s.json() == "null", "empty");
    for (int i = 1; i <= 100; i++) s.add(i);
    Json j = Json::parse(s.json());
    CHECK(j.at("p50").num == 51 && j.at("p90").num == 90 && j.at("n").num == 100, "%s", s.json().c_str());
  }
  DONE();
}
