// Host-only tests of the phone streamer: the stamp, the colour conversion, the geometry and
// header, the requests it sends the app, which copies become frames, and the scene it
// accepts. No GPU or emulator.
#include <math.h>
#include <string.h>

#include <zstd.h>

#include <fstream>
#include <sstream>
#include <string>
#include <vector>

#include "check.h"
#include "json.h"
#include "logic.h"
#include "mirror.h"
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

// A source file next to this one, whole ("" when unreadable, which fails the checks).
static std::string source_text(const std::string& name) {
  std::string me = __FILE__;
  std::ifstream f(me.substr(0, me.find_last_of('/') + 1) + name);
  std::stringstream ss;
  ss << f.rdbuf();
  return ss.str();
}

int main() {
  // A phone's streams are public like a renderer's: its server checks the scene token and
  // waits for the first scene when one is configured (renderer PROTOCOL.md, "Access").
  {
    std::string m = source_text("main.cpp");
    size_t conf = m.find("scene_token.configure_from_env();"), set = m.find("s.set_scene_token(&scene_token)");
    size_t listen = m.find("s.listen(port, &err)");
    CHECK(conf != std::string::npos && set != std::string::npos && listen != std::string::npos && conf < set && set < listen,
          "the phone streamer configures the scene token before it listens");
  }
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
  {  // direct-stream admission, as the renderer: the coordinator's test vector (key 07 x 32,
     // viewer v-3, expiry 1000), and every client while no scene brought keys
    const std::string tok = "v1.1000.v-3.77RQqtaEAwJ9OBEoNeX1QubmZTlMfZcYJtIdWkCSW7Q";
    AdmitKeys a;
    CHECK(a.admits("", 999), "no keys: every client");
    PhoneScene sc = parsed("{\"epoch_unix_ms\":1000,\"admit_keys\":[\"0707070707070707070707070707070707070707070707070707070707070707\"]}");
    a.on_scene(sc);
    CHECK(a.admits(tok, 999), "the coordinator's token opens a phone stream");
    CHECK(!a.admits("", 999) && !a.admits(tok, 1001), "no token, or an expired one, opens none");
    a.on_scene(parsed("{\"epoch_unix_ms\":1000}"));
    CHECK(a.admits("", 999), "a scene without keys opens every stream again");
    std::string err;
    PhoneScene bad;
    CHECK(!phone_scene_parse("{\"epoch_unix_ms\":1,\"admit_keys\":[\"xx\"]}", &bad, &err), "a key that is not 64 hex digits");
    // the keys are the streamer's: the app is not sent them
    Json j = Json::parse(app_scene_line(sc));
    CHECK(!j.has("admit_keys") && j.at("epoch_unix_ms").num == 1000 && j.at("type").str == "scene", "%s",
          app_scene_line(sc).c_str());
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
  {  // the GPU mirror's admission: only the token of the viewer the scene names as holding the
     // phone (the coordinator's test vector above: viewer v-3), never without keys or a holder
    const std::string tok = "v1.1000.v-3.77RQqtaEAwJ9OBEoNeX1QubmZTlMfZcYJtIdWkCSW7Q";
    const std::string key = "0707070707070707070707070707070707070707070707070707070707070707";
    CHECK(admit_token_viewer(tok) == "v-3", "the viewer a token names: %s", admit_token_viewer(tok).c_str());
    CHECK(admit_token_viewer("v1.1000.v.x.y.mac") == "v.x.y" && admit_token_viewer("v1.1000mac").empty() &&
              admit_token_viewer("v2.1.v.m").empty(),
          "the viewer is between the expiry and the mac");
    PhoneScene held = parsed("{\"epoch_unix_ms\":1000,\"admit_keys\":[\"" + key + "\"],\"mirror_viewer\":\"v-3\"}");
    AdmitKeys a;
    a.on_scene(held);
    CHECK(held.mirror_viewer == "v-3", "the scene names the holder");
    CHECK(mirror_admits(a.keys, held.mirror_viewer, tok, 999), "the holder's token opens the mirror");
    CHECK(!mirror_admits(a.keys, "v-4", tok, 999), "another viewer's token does not");
    CHECK(!mirror_admits(a.keys, "", tok, 999), "nobody holds the phone: no mirror");
    CHECK(!mirror_admits({}, "v-3", tok, 999), "no keys: no mirror, unlike /frames");
    // A fleet with no broadcast sends no admit_keys (every /frames stream is open) but still
    // sends the mirror its own keys.
    PhoneScene open_fleet = parsed("{\"epoch_unix_ms\":1000,\"mirror_keys\":[\"" + key + "\"],\"mirror_viewer\":\"v-3\"}");
    AdmitKeys frames, mk;
    frames.on_scene(open_fleet);
    mk.on_mirror_keys(open_fleet);
    CHECK(frames.keys.empty() && frames.admits("", 999), "mirror_keys leave /frames open");
    CHECK(mirror_admits(mk.keys, open_fleet.mirror_viewer, tok, 999), "and admit the holder to the mirror");
    CHECK(!Json::parse(app_scene_line(open_fleet)).has("mirror_keys"), "the app is not sent them");
    CHECK(!mirror_admits(a.keys, "v-3", tok, 1001), "an expired token");
    std::string forged = tok;
    forged[forged.size() - 2] = forged[forged.size() - 2] == 'A' ? 'B' : 'A';
    CHECK(!mirror_admits(a.keys, "v-3", forged, 999), "a token another key made");
    Json j = Json::parse(app_scene_line(held));
    CHECK(!j.has("mirror_viewer"), "the app is not sent the holder: %s", app_scene_line(held).c_str());
    std::string err;
    PhoneScene bad;
    CHECK(!phone_scene_parse("{\"epoch_unix_ms\":1,\"mirror_viewer\":" + std::string("\"") + std::string(129, 'v') + "\"}", &bad, &err),
          "a holder id over 128 characters");
  }
  {  // the mirror's zstd: one stream, each push a whole message the page decodes as it arrives
     // (a flushed block), the concatenation of everything pushed; its window fits the page's
     // decoder (1 MiB)
    MirrorZstd z;
    CHECK(z.ok(), "ZSTD_createCCtx");
    ZSTD_DStream* d = ZSTD_createDStream();
    CHECK(d && !ZSTD_isError(ZSTD_DCtx_setParameter(d, ZSTD_d_windowLogMax, 20)), "ZSTD_createDStream");
    std::string all, got;
    for (int i = 0; i < 50; i++) {
      std::string chunk;
      for (int k = 0; k < 1000 + 37 * i + (i == 40 ? (3 << 20) : 0); k++) chunk += (char)((k * 7 + i) % 61 + (k >> 12));
      all += chunk;
      std::string msg;
      CHECK(z.push((const uint8_t*)chunk.data(), chunk.size(), &msg), "push %d", i);
      ZSTD_inBuffer in{msg.data(), msg.size(), 0};
      unsigned char out[1 << 16];
      ZSTD_outBuffer o;
      do {
        o = {out, sizeof out, 0};
        size_t rc = ZSTD_decompressStream(d, &o, &in);
        CHECK(!ZSTD_isError(rc), "decompress: %s", ZSTD_getErrorName(rc));
        got.append((const char*)out, o.pos);
      } while (o.pos == o.size || in.pos < in.size);
      CHECK(got == all, "after message %d the page has everything pushed", i);
    }
    CHECK(z.in_bytes == all.size() && z.out_bytes < z.in_bytes / 4, "counted, and repetitive input shrinks: %llu of %llu",
          (unsigned long long)z.out_bytes, (unsigned long long)z.in_bytes);
    ZSTD_freeDStream(d);
  }
  {  // The mirror stream's teardown (source.cpp, PhoneSource::mirror), pinned at the source: no
     // host test can make a viewer that stops reading, or post a scene between the token check
     // and the stream's start.
    std::string src = source_text("source.cpp");
    size_t body = src.find("void PhoneSource::mirror(");
    CHECK(body != std::string::npos, "source.cpp has PhoneSource::mirror");
    std::string m = src.substr(body, src.find("\nbool PhoneSource::has_scene()", body) - body);
    // the close frame is the writer's last message, and a writer still blocked after a bounded
    // wait is freed by shutting the socket, all before the writer is joined
    size_t queued = m.find("close_msg = close_frame(code, reason);");
    size_t bounded = m.find("wait_for(l, std::chrono::seconds(2)");
    size_t freed = m.find("c.shutdown_both();", bounded);
    size_t joined = m.rfind("writer.join();");
    CHECK(queued != std::string::npos && bounded != std::string::npos && freed != std::string::npos &&
              queued < bounded && bounded < freed && freed < joined,
          "teardown: close frame queued, bounded wait, shutdown, then join");
    // the holder is checked again with the generation, under one lock, before the stream starts
    size_t gen = m.find("holder_gen = mirror_holder_gen_;");
    size_t again = m.find("scene_.mirror_viewer == viewer", gen);
    size_t refused = m.find("if (!still_holder)");
    size_t opened = m.find("std::unique_lock<std::mutex> one(mirror_mu_");
    CHECK(gen != std::string::npos && again != std::string::npos && refused != std::string::npos && again < refused &&
              refused < opened,
          "the holder is checked again with its generation before the stream starts");
    // after a holder change the writer skips what is queued but stays for the close frame, so
    // the old holder's page reads why ("the phone went back") instead of a dropped connection
    // ... and only the close frame passes the holder check: it is kept apart from the data, so
    // a data frame popped after the reader's close is still checked
    CHECK(m.find("if (!last && !holds()) continue;") != std::string::npos && m.find("last = true;") != std::string::npos &&
              m.find("last = done.load();") == std::string::npos,
          "a revoked stream's writer sends its close frame and no data");
    // no thread is made with a bare std::thread constructor, which throws at a thread limit
    CHECK(m.find("std::thread writer([") == std::string::npos && m.find("start_thread(") != std::string::npos,
          "the stream's threads start with start_thread");
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
