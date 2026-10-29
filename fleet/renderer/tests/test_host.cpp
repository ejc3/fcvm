// Host-side tests: no GPU needed. Scene contract parsing, camera interpolation
// and holds, tile geometry, wire formats.
#include <unistd.h>

#include <cmath>
#include <cstring>
#include <fstream>
#include <regex>
#include <sstream>
#include <string>
#include <vector>

#include "../src/access.h"
#include "../src/admit.h"
#include "../src/budget.h"
#include "../src/params.h"
#include "../src/font_sdf.h"
#include "../src/jpeg.h"
#include "../src/json.h"
#include "../src/nvenc.h"
#include "../src/proto.h"
#include "../src/scene.h"
#include "../src/subscriber.h"
#include "../src/threads.h"
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


// Source-level checks for pipeline.cpp, which needs CUDA and so is not built
// here. Reads the file next to this one; an unreadable file fails the check.
static std::vector<std::string> source_lines(const std::string& rel) {
  std::string me = __FILE__;
  std::string dir = me.substr(0, me.find_last_of('/') + 1);
  std::ifstream f(dir + rel);
  std::vector<std::string> out;
  for (std::string l; std::getline(f, l);) out.push_back(l);
  return out;
}
// True when the line at i runs with hmu_ held: a lock_guard on hmu_ earlier in
// the same block or an enclosing one (not inside a block already closed, like a
// try before its catch), inside the same function. Walks back character by
// character: a '{' leaves for the enclosing block, a '}' enters a closed one.
static bool under_hmu(const std::vector<std::string>& src, size_t i) {
  int depth = 0, low = 0;
  for (size_t j = i; j-- > 0;) {
    const std::string& l = src[j];
    if (l.find("lock_guard<std::mutex>") != std::string::npos && l.find("(hmu_)") != std::string::npos && depth == low)
      return true;
    for (size_t c = l.size(); c-- > 0;) {
      if (l[c] == '}') depth++;
      if (l[c] == '{') low = std::min(low, --depth);
    }
    if (!l.empty() && l[0] != ' ' && l[0] != '}' && l[0] != '/' && l[0] != '#') return false;  // a function starts here
  }
  return false;
}

static Scene two_holds() {
  Scene s;
  s.rect = {0, 0, 1, 1};
  auto K = [&](double t, double x) { s.camera.push_back(Keyframe{t, {x, 1, 0}, {x, 1, 10}, 60}); };
  K(0, 0); K(5, 0);   // hold 0..5
  K(10, 10);          // move
  K(15, 10);          // hold 10..15 (and after)
  return s;
}

// The own-GPU autopilot (fleet/core/src/autopilot.rs) as the coordinator hands it to a
// renderer: fleet/core/tests/autopilot_lap.json holds one lap of keyframes, the same
// path paused part way, and the pose the coordinator computes at every quarter second.
// The coordinator starts a pause or a drag from the pose it computes, so it must be the
// pose this renderer draws; and the eye must clear the ground by more than the 60 m the
// renderer would otherwise lift it, which would move the camera off the path.
static void check_autopilot_lap() {
  std::string me = __FILE__;
  std::string dir = me.substr(0, me.find_last_of('/') + 1);
  std::ifstream f(dir + "../../core/tests/autopilot_lap.json");
  std::stringstream ss;
  ss << f.rdbuf();
  CHECK(f.good() && ss.str().size() > 0, "fleet/core/tests/autopilot_lap.json is readable");
  if (ss.str().empty()) return;
  Json j = Json::parse(ss.str());
  const V4* tab = host_noise().tab.data();
  double min_clear = 1e9, worst_t = 0, max_err = 0;
  int samples = 0;
  for (const char* part : {"lap", "paused"}) {
    std::vector<Keyframe> kf;
    for (const Json& k : j.at(part).at("camera").array("camera")) {
      Keyframe x{};
      x.t = k.at("t").number("t");
      for (int c = 0; c < 3; c++) {
        x.pos[c] = k.at("pos").arr[c].num;
        x.look[c] = k.at("look").arr[c].num;
      }
      x.fov_deg = k.at("fov_deg").number("fov_deg");
      kf.push_back(x);
    }
    for (const Json& s : j.at(part).at("samples").array("samples")) {
      double t = s.at("t").number("t");
      CamPose p = camera_at(kf, t);
      for (int c = 0; c < 3; c++) {
        max_err = std::max(max_err, std::fabs(p.pos[c] - s.at("pos").arr[c].num));
        max_err = std::max(max_err, std::fabs(p.look[c] - s.at("look").arr[c].num));
      }
      max_err = std::max(max_err, std::fabs(p.fov_deg - s.at("fov_deg").number("fov_deg")));
      double ground = std::max(0.0, (double)surface_h(v2((float)p.pos[0], (float)p.pos[2]), 9, tab));
      if (p.pos[1] - ground < min_clear) { min_clear = p.pos[1] - ground; worst_t = t; }
      samples++;
    }
  }
  CHECK(samples > 200, "%d samples", samples);
  CHECK(max_err < 1e-9, "the coordinator's camera differs from the renderer's by %g", max_err);
  CHECK(min_clear > 0.15, "the eye is %.3f km above the ground at t=%.2f", min_clear, worst_t);
}

int main() {
  // --- direct-stream admission (fleet/PROTOCOL.md "admission"; the same vector is in
  // src/fleet/coord/admission.rs: key 07 x 32, viewer v-3, expiry 1000)
  {
    const std::string key(32, '\x07');
    const std::string tok = "v1.1000.v-3.77RQqtaEAwJ9OBEoNeX1QubmZTlMfZcYJtIdWkCSW7Q";
    CHECK(admit_token_valid({key}, tok, 999) && admit_token_valid({key}, tok, 1000), "the coordinator's token opens a stream");
    CHECK(!admit_token_valid({key}, tok, 1001), "an expired token");
    CHECK(!admit_token_valid({std::string(32, '\x08')}, tok, 999), "another key's token");
    CHECK(admit_token_valid({std::string(32, '\x08'), key}, tok, 999), "the previous key still opens a stream");
    for (const char* bad : {"", "v1", "v1.1000.v-3", "v2.1000.v-3.77RQqtaEAwJ9OBEoNeX1QubmZTlMfZcYJtIdWkCSW7Q",
                            "v1.9999.v-3.77RQqtaEAwJ9OBEoNeX1QubmZTlMfZcYJtIdWkCSW7Q", "v1..v-3.x", "v1.1000..x"})
      CHECK(!admit_token_valid({key}, bad, 0), "refused: %s", bad);
    CHECK(admit_key_from_hex(std::string(64, '7')).size() == 32, "hex key");
    bool threw = false;
    try { admit_key_from_hex("zz"); } catch (const std::exception&) { threw = true; }
    CHECK(threw, "a bad hex key is refused");
    auto held = admit_keys_after({"a", "b"}, {"c"});
    CHECK(held.size() == 3 && held[0] == "c" && held[1] == "a", "a new key keeps the old ones after it");
    CHECK(admit_keys_after({"a", "b", "c", "d"}, {"e"}).size() == 4, "at most 4 keys");
    // A scene carries them as hex; a scene without them carries none.
    std::string with = std::string(kContractExample);
    with.insert(with.rfind('}'), ", \"admit_keys\": [\"" + std::string(64, '7') + "\"]");
    CHECK(parse_scene(with).admit_keys.size() == 1 && parse_scene(kContractExample).admit_keys.empty(), "admit_keys in a scene");
  }
  // --- access (access.h, fleet/PROTOCOL.md "Access")
  {
    CHECK(scene_access(false, "", "") == 0, "a renderer outside the fleet takes any scene");
    CHECK(scene_access(true, "", "Bearer x") == 503, "a fleet renderer without its token takes none");
    CHECK(scene_access(true, "tok-1", "") == 401, "no Authorization header");
    CHECK(scene_access(true, "tok-1", "Bearer tok-2") == 401, "another token");
    CHECK(scene_access(true, "tok-1", "Bearer tok-1x") == 401, "a longer token");
    CHECK(scene_access(true, "tok-1", "Basic tok-1") == 401, "another scheme");
    CHECK(scene_access(true, "tok-1", "Bearer ") == 401, "an empty bearer");
    CHECK(scene_access(true, "tok-1", "Bearer tok-1") == 0, "the token");
    CHECK(frames_access(true, false) == 503 && frames_access(true, true) == 0 && frames_access(false, false) == 0,
          "a fleet renderer streams nothing before its first scene");
    std::string tok;
    CHECK(admit_message("{\"admit\":\"v1.1.v-1.x\"}", &tok) && tok == "v1.1.v-1.x", "a first message's token");
    CHECK(!admit_message("{\"h\":540}", &tok) && !admit_message("not json", &tok) && !admit_message("{\"admit\":5}", &tok),
          "not an admit message");
    CHECK(scene_token_from_text("{\"cert\":\"c\",\"key\":\"k\",\"scene_token\":\"s-1\"}", true) == "s-1" &&
              scene_token_from_text("{\"cert\":\"c\",\"key\":\"k\"}", true).empty() &&
              scene_token_from_text("  s-2\n", false) == "s-2" && scene_token_from_text("{", true).empty(),
          "the token from the certificate JSON or its own file");
    // The file is read again when it changes, and a removed file authorizes nothing.
    char dir[] = "/tmp/fleet-access-XXXXXX";
    CHECK(mkdtemp(dir) != nullptr, "temp dir");
    std::string path = std::string(dir) + "/tls.json";
    SceneToken st;
    st.configure(path, true);
    CHECK(st.configured() && st.current().empty(), "configured, no file yet");
    { std::ofstream(path) << "{\"cert\":\"c\",\"key\":\"k\",\"scene_token\":\"s-1\"}"; }
    CHECK(st.current() == "s-1", "loaded");
    std::string tmp = path + ".new";
    { std::ofstream(tmp) << "{\"cert\":\"c\",\"key\":\"k\",\"scene_token\":\"s-22\"}"; }
    CHECK(rename(tmp.c_str(), path.c_str()) == 0, "rename");
    CHECK(st.current() == "s-22", "a replaced file is read again");
    unlink(path.c_str());
    CHECK(st.current().empty(), "a removed file authorizes nothing");
    rmdir(dir);
    SceneToken none;
    CHECK(!none.configured(), "no token file: not a fleet renderer");
  }
  // server.cpp needs the pipeline and so is not built here: where it asks these.
  {
    auto src = source_lines("../src/server.cpp");
    auto find_from = [&](long from, const std::string& needle) -> long {
      for (size_t i = from < 0 ? 0 : (size_t)from; i < src.size(); i++)
        if (src[i].find(needle) != std::string::npos) return (long)i;
      return -1;
    };
    long scene = find_from(0, "path == \"/scene\" || path == \"/budget\"");
    long access = find_from(scene, "scene_access(fleet()");
    long body = find_from(scene, "c.read(tmp, sizeof tmp)");
    long post = find_from(scene, "p_.post_scene(body");
    CHECK(scene >= 0 && access > scene && body > access && post > access,
          "POST /scene and /budget check the scene token before they read or apply a body (%ld %ld %ld %ld)", scene, access, body, post);
    long frames = find_from(0, "path == \"/frames\" && method == \"GET\"");
    long gate = find_from(frames, "frames_access(fleet(), p_.has_scene())");
    long upgrade = find_from(frames, "return websocket(");
    CHECK(frames >= 0 && gate > frames && upgrade > gate, "/frames waits for the first scene before it upgrades (%ld %ld %ld)", frames,
          gate, upgrade);
    CHECK(find_from(0, "\"admit\"") < 0, "no direct token is read from the URL");
    long ws = find_from(0, "void Server::websocket(");
    long needs = find_from(ws, "p_.requires_admit()");
    long first = find_from(needs, "admit_message(first, &token) || !p_.admits(token)");
    long sub = find_from(ws, "p_.subscribe(h, q)");
    CHECK(ws >= 0 && needs > ws && first > needs && sub > first,
          "a stream that needs a direct token reads it from the first message before it subscribes (%ld %ld %ld %ld)", ws, needs, first,
          sub);
  }

  check_autopilot_lap();
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

  // --- reordering an SPS declares (fleet/web/core/test/sps.test.ts has the same fixtures)
  {
    auto bytes = [](const char* hex) {
      std::vector<uint8_t> v;
      for (size_t i = 0; hex[i] && hex[i + 1]; i += 2) v.push_back((uint8_t)strtoul(std::string(hex + i, 2).c_str(), nullptr, 16));
      return v;
    };
    // The renderer's SPS before bitstreamRestrictionFlag (staging, h=360): no restriction.
    auto old = bytes("6764001eac2b2014863f2e02d404040500000303e80000ea6084");
    SpsReorder r = sps_reorder(old.data(), old.size());
    CHECK(r.parsed && !r.restriction && r.max_num_ref_frames == 3, "old renderer SPS: parsed %d restriction %d refs %u",
          (int)r.parsed, (int)r.restriction, r.max_num_ref_frames);
    // The same SPS with the restriction added (read by ffmpeg's trace_headers as 0 and 3).
    auto fixed = bytes("6764001eac2b2014863f2e02d404040500000303e80000ea608da0804248");
    r = sps_reorder(fixed.data(), fixed.size());
    CHECK(r.parsed && r.restriction && r.max_num_reorder_frames == 0 && r.max_dec_frame_buffering == 3,
          "restricted SPS: parsed %d restriction %d reorder %u dec %u", (int)r.parsed, (int)r.restriction,
          r.max_num_reorder_frames, r.max_dec_frame_buffering);
    // x264 with 3 B-frames (POC type 0), and with NAL HRD parameters before the restriction.
    auto bf = bytes("6764000dacd941419f9f0110000003001000000303c0f1429960");
    r = sps_reorder(bf.data(), bf.size());
    CHECK(r.parsed && r.restriction && r.max_num_reorder_frames == 2 && r.max_dec_frame_buffering == 4, "x264 -bf 3");
    auto hrd = bytes("67640014acb202833f3e022000000300200000079c04001e848003d09c58600f142a48");
    r = sps_reorder(hrd.data(), hrd.size());
    CHECK(r.parsed && r.restriction && r.max_num_reorder_frames == 0 && r.max_dec_frame_buffering == 3, "x264 nal-hrd");
    // No VUI at all.
    auto novui = bytes("6764001eac2b2014863f2a");
    r = sps_reorder(novui.data(), novui.size());
    CHECK(r.parsed && !r.restriction, "no VUI");
    // Truncated, a stray bit after the VUI, and a PPS: not a whole SPS.
    r = sps_reorder(fixed.data(), 12);
    CHECK(!r.parsed, "truncated SPS parsed");
    auto stray = fixed;
    stray.back() |= 0x01;
    r = sps_reorder(stray.data(), stray.size());
    CHECK(!r.parsed, "stray bit parsed");
    const uint8_t pps[] = {0x68, 0xee, 0x3c, 0xb0};
    CHECK(!sps_reorder(pps, sizeof pps).parsed, "PPS parsed as an SPS");
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
    CHECK(lc.words.size() == 1 && lc.words[0].text == "hi !", "text filtered to ASCII: '%s'", lc.words[0].text.c_str());
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

  // --- weather: keys parsed, eased between them, held outside them, in the hold key
  {
    const char* head = R"({"epoch_unix_ms":0,"rect":{"x":0,"y":0,"w":1,"h":1},"image_aspect":1.7778,
        "camera":[{"t":0,"pos":[0,1,0],"look":[0,1,1],"fov_deg":60}],"time_of_day":0.5,"weather":)";
    auto with = [&](const char* w) { return std::string(head) + w + "}"; };
    Scene s = parse_scene(with(R"([{"t":10,"cover":1,"towers":1,"rays":1,"snow_line_km":0.5},{"t":0}])"));
    CHECK(s.weather.size() == 2 && s.weather[0].t == 0 && s.weather[1].towers == 1, "weather keys sorted by t");
    CHECK(s.weather[0].cover == 0.5 && s.weather[0].towers == 0 && s.weather[0].rays == 0 && s.weather[0].snow_line_km == 1.6,
          "absent weather fields take the defaults");
    WeatherKey m = weather_at(s.weather, 5), a = weather_at(s.weather, -3), b = weather_at(s.weather, 99);
    CHECK(std::fabs(m.cover - 0.75) < 1e-12 && std::fabs(m.snow_line_km - 1.05) < 1e-12, "halfway eases halfway");
    WeatherKey q = weather_at(s.weather, 2.5);
    CHECK(q.towers > 0.0 && q.towers < 0.25, "eased, not linear, a quarter of the way (%g)", q.towers);
    CHECK(a.cover == 0.5 && b.cover == 1 && b.rays == 1, "held before the first and after the last key");
    CHECK(weather_at({}, 3).cover == 0.5 && weather_at({}, 3).snow_line_km == 1.6, "no keys: the defaults");
    auto bad = [&](const char* w) {
      try { parse_scene(with(w)); } catch (const std::exception&) { return true; }
      return false;
    };
    CHECK(bad(R"([{"t":0,"cover":1.5}])") && bad(R"([{"t":0,"snow_line_km":-1}])") && bad(R"([{"cover":0.2}])") &&
              bad(R"({"t":0})"),
          "out-of-range field, missing t and a non-array refused");
    // a held frame's key and parameters follow the weather at the hold's start
    Scene h = s;
    h.camera = {Keyframe{0, {0, 1, 0}, {0, 1, 1}, 60}, Keyframe{100, {0, 1, 0}, {0, 1, 1}, 60}};
    Tile tl = tile_geometry(Rect{}, 16.0 / 9.0, 90);
    uint64_t k1, k2;
    bool held;
    FrameParams p1 = build_frame(h, tl, 20, &k1, &held);
    CHECK(held && p1.cloud_cover == 0.5f && p1.cloud_top == 5.6f && p1.snow_line == 1.6f, "a hold draws its start's weather");
    Scene h2 = h;
    h2.weather[0].cover = 0.2;
    build_frame(h2, tl, 20, &k2, &held);
    CHECK(k1 != k2, "the weather is in the hold key");
    Scene mv = s;
    mv.camera = {Keyframe{0, {0, 1, 0}, {0, 1, 1}, 60}, Keyframe{100, {5, 1, 0}, {5, 1, 1}, 60}};
    FrameParams p2 = build_frame(mv, tl, 10, &k2, &held);
    CHECK(!held && p2.cloud_cover == 1.0f && std::fabs(p2.cloud_top - 9.6f) < 1e-5f && p2.rays == 1.0f && p2.snow_line == 0.5f,
          "a moving frame draws the weather at its own time");
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
    // The height a tile draws (HeightGuard). Staging, 2026-10-27: one viewer
    // opened a 16:9 terrain tile, which asked its renderer for 720 against the
    // wall's 432. Its frames took 36 ms (render 29 ms and two encodes) at the
    // lowest budget, so it drew every other slot and the wall, held to one
    // scene time, drew at 15 to 19 fps. A frame here costs 33 ns per drawn
    // pixel (14.5 ms at 432, the same tile's wall frames); a sky tile 16 ns.
    const double fslot = 1000.0 / 30;
    Rect r16{0.25, 0.5, 0.25, 0.25};
    auto px = [&](int h) {  // Renderer::drawn_pixels with the 16 px stream overlap
      Tile t = tile_geometry(r16, 16.0 / 9, h);
      int g = (int)std::ceil(GUARD_FRAC * t.full_h) + 2 + DENOISE_R + 16;
      return (double)(t.w + 2 * g) * (t.h + 2 * g);
    };
    // run the frame clock for `secs`: each frame takes ns_px * drawn pixels,
    // and a frame longer than a slot skips the slots it overran
    struct Run { long long frames = 0, skipped = 0, skipped_late = 0; int last_h = 0; };
    auto run = [&](HeightGuard& g, std::vector<int> asked, double ns_px, double t0, double secs) {
      Run o;
      double t = t0;
      while (t < t0 + secs) {
        int h = g.choose(asked, px, t);
        double ms = ns_px * 1e-6 * px(h);
        long long slots = std::max(1LL, (long long)std::ceil(ms / fslot - 1e-9));
        g.observe(t, h, ms);
        o.frames++;
        o.skipped += slots - 1;
        if (t >= t0 + 1.5) o.skipped_late += slots - 1;
        o.last_h = h;
        t += slots * fslot / 1000.0;
      }
      return o;
    };
    // the terrain tile: drawn at the wall's height for a minute, then a viewer opens it
    HeightGuard g;
    Run wall = run(g, {432}, 33.0, 0, 60);
    CHECK(wall.skipped == 0 && wall.last_h == 432, "the wall tile alone draws every slot (%lld skipped)", wall.skipped);
    Run open = run(g, {432, 720}, 33.0, 60, 60);
    CHECK(open.skipped_late == 0, "an opened terrain tile keeps every wall slot (%lld of %lld skipped after 1.5 s)",
          open.skipped_late, open.frames + open.skipped);
    CHECK(open.last_h == 432, "it draws at the wall's height and scales the opened stream up (drew %d)", open.last_h);
    // a fresh renderer asked for both at once tries the taller and steps down within 1.5 s
    HeightGuard f;
    Run fresh = run(f, {432, 720}, 33.0, 0, 30);
    CHECK(fresh.skipped <= 30 && fresh.skipped_late == 0 && fresh.last_h == 432,
          "a fresh terrain tile steps down within 1.5 s (%lld skipped, %lld after 1.5 s, drew %d)", fresh.skipped,
          fresh.skipped_late, fresh.last_h);
    // and does not try 720 again while the view stays as costly
    Run later = run(f, {432, 720}, 33.0, 30, 120);
    CHECK(later.skipped == 0 && later.last_h == 432, "no retry while 720 is predicted over its slot (%lld skipped)",
          later.skipped);
    // a sky tile affords 720: it draws the opened stream at its own height
    HeightGuard s;
    run(s, {432}, 16.0, 0, 30);
    Run sky = run(s, {432, 720}, 16.0, 30, 30);
    CHECK(sky.last_h == 720 && sky.skipped == 0, "a sky tile draws 720 (drew %d, %lld skipped)", sky.last_h, sky.skipped);
    // a view that turns costly while drawing 720 steps down; once cheap again, 720 comes back after the backoff
    Run turn = run(s, {432, 720}, 33.0, 60, 20);
    CHECK(turn.last_h == 432 && turn.skipped_late == 0, "costly view: back to 432 (drew %d, %lld skipped after 1.5 s)",
          turn.last_h, turn.skipped_late);
    Run cheap = run(s, {432, 720}, 16.0, 80, 30);
    CHECK(cheap.last_h == 432, "720 is not drawn again within the backoff (drew %d)", cheap.last_h);
    Run back = run(s, {432, 720}, 16.0, 110, 60);
    CHECK(back.last_h == 720 && back.skipped == 0, "after the backoff it is (drew %d)", back.last_h);
    // a tile too slow to draw min_frames frames in the 2 s window still steps down
    // (Codex on #1380: 432 and 2160 asked, 2160 at about 3 fps)
    HeightGuard crawl;
    Run c = run(crawl, {432, 2160}, 33.0, 0, 6);
    Run c2 = run(crawl, {432, 2160}, 33.0, 6, 20);
    CHECK(c.last_h == 432 && c2.skipped == 0 && c2.last_h == 432,
          "a 3 fps tile steps down (drew %d, then %lld skipped)", c2.last_h, c2.skipped);
    // a renderer at another frame rate judges frames against its own slot
    // (Codex on #1380: at FPS=60 a 20 ms 720 frame overruns every slot)
    HeightGuard sixty;
    sixty.slot_ms = 1000.0 / 60;
    double t60 = 0;
    for (int i = 0; i < 200; i++, t60 += 1.0 / 30) {  // 20 ms at 720 skips every other 60 fps slot
      int h = sixty.choose({432, 720}, px, t60);
      sixty.observe(t60, h, h == 720 ? 20.0 : 20.0 * px(432) / px(720));
    }
    CHECK(sixty.drawn() == 432, "a 60 fps renderer leaves a 20 ms height (drew %d)", sixty.drawn());
    {
      bool set = false;
      for (auto& l : source_lines("../src/pipeline.cpp"))
        set = set || l.find("height_guard_.slot_ms = 1000.0 / o.fps;") != std::string::npos;
      CHECK(set, "the pipeline gives the height guard its own frame slot");
    }
    // a single asked height is drawn however slow (nothing lower to go to)
    HeightGuard one;
    Run solo = run(one, {720}, 33.0, 0, 5);
    CHECK(solo.last_h == 720, "one height asked: drawn (%d)", solo.last_h);
    // and nothing asked is 0
    CHECK(one.choose({}, px, 10) == 0, "nothing asked");
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
  // --- text styles (words[].text_style)
  {
    auto scene_with = [](const std::string& word) {
      return parse_scene(R"({"epoch_unix_ms":0,"rect":{"x":0,"y":0,"w":1,"h":1},"image_aspect":1.7778,
        "camera":[{"t":0,"pos":[-15,2.2,-100],"look":[-70,1.4,-70],"fov_deg":62}],"time_of_day":0.245,"words":[)" + word +
                         "]}");
    };
    CHECK(scene_with(R"({"text":"AVA hi","t":1})").words[0].style == TEXT_STYLE_LOCKUP, "text_style absent: lockup");
    CHECK(scene_with(R"({"text":"a","t":1,"text_style":"skywriting"})").words[0].style == TEXT_STYLE_SKYWRITING &&
              scene_with(R"({"text":"a","t":1,"text_style":"landmark"})").words[0].style == TEXT_STYLE_LANDMARK &&
              scene_with(R"({"text":"a","t":1,"text_style":"lockup"})").words[0].style == TEXT_STYLE_LOCKUP &&
              scene_with(R"({"text":"a","t":1,"text_style":null})").words[0].style == TEXT_STYLE_LOCKUP,
          "text_style names");
    bool threw = false;
    try { scene_with(R"({"text":"a","t":1,"text_style":"classic"})"); } catch (const std::exception&) { threw = true; }
    CHECK(threw, "an unknown text_style (classic is gone) is refused");
    Tile tl = tile_geometry(Rect{0, 0, 1, 1}, 16.0 / 9, 540);
    uint64_t key;
    bool held;
    FrameParams pl = build_frame(scene_with(R"({"text":"AVA hi","t":1})"), tl, 2.0, &key, &held);
    CHECK(pl.n_text == 2 && pl.text[0].style == GTEXT_TYPE && pl.text[1].style == GTEXT_TYPE &&
              memcmp(pl.text[1].ch, "AVA hi", 6) == 0,
          "lockup: the clock and the word as type, case kept");
    CHECK(pl.text[0].occlude == 0 && pl.text[1].occlude == 1, "nothing hides the clock; terrain hides the word");
    // kerning: V tucks under A, so V starts before A's advance ends
    CHECK(pl.text[1].gx[1] < pl.text[1].gx[0] + 0.69f - 0.02f, "A V kerned (V at %.3f em)", pl.text[1].gx[1]);
    // the clock's digits sit on one advance: its width does not change as it counts
    FrameParams p2 = build_frame(scene_with(R"({"text":"AVA hi","t":1})"), tl, 3.0, &key, &held);
    CHECK(fabsf(pl.text[0].width - p2.text[0].width) < 1e-6f && pl.text[0].width > 3.0f,
          "the clock's width does not change as it counts (%.4f, %.4f)", pl.text[0].width, p2.text[0].width);
    // the message sits above the clock, both from one left edge
    auto screen = [](const FrameParams& f, const GText& g, float* u, float* v) {
      V3 d = g.center - f.cam.pos;
      *u = dot(d, f.cam.right) / dot(d, f.cam.fwd);
      *v = dot(d, f.cam.up) / dot(d, f.cam.fwd);
    };
    float um, vm, uc, vc;
    screen(pl, pl.text[1], &um, &vm);
    screen(pl, pl.text[0], &uc, &vc);
    CHECK(fabsf(um - uc) < 1e-4f && vm > vc && um < 0, "left-aligned lockup, message above the clock (u %.4f %.4f, v %.4f %.4f)",
          um, uc, vm, vc);
    // the clock keeps its place whatever the word's style, or with no word
    FrameParams ps = build_frame(scene_with(R"({"text":"Hello","t":1,"text_style":"skywriting"})"), tl, 2.0, &key, &held);
    FrameParams pk = build_frame(scene_with(R"({"text":"Hello","t":1,"text_style":"landmark"})"), tl, 2.0, &key, &held);
    FrameParams pn = build_frame(scene_with(R"({"text":"Hello","t":100})"), tl, 2.0, &key, &held);
    float u2, v2_, u3, v3_, u4, v4_;
    screen(ps, ps.text[0], &u2, &v2_);
    screen(pk, pk.text[0], &u3, &v3_);
    screen(pn, pn.text[0], &u4, &v4_);
    CHECK(pn.n_text == 1 && fabsf(u2 - uc) < 1e-5f && fabsf(u3 - uc) < 1e-5f && fabsf(u4 - uc) < 1e-5f &&
              fabsf(v2_ - vc) < 1e-5f && fabsf(v3_ - vc) < 1e-5f && fabsf(v4_ - vc) < 1e-5f,
          "the clock does not move with the word's style");
    CHECK(ps.text[1].style == GTEXT_SMOKE && pk.text[1].style == GTEXT_LANDMARK, "each word in its own style");
    // skywriting stays up about 25 s after the last letter, then fades
    FrameParams ps1 = build_frame(scene_with(R"({"text":"Hello","t":1,"text_style":"skywriting"})"), tl, 26.0, &key, &held);
    FrameParams ps2 = build_frame(scene_with(R"({"text":"Hello","t":1,"text_style":"skywriting"})"), tl, 50.0, &key, &held);
    CHECK(ps1.text[1].alpha == 1.0f && ps2.text[1].alpha == 0.0f, "skywriting: up at 25 s, gone at 49 s (%.2f, %.2f)",
          ps1.text[1].alpha, ps2.text[1].alpha);
    // landmark letters stand on the ground under their middles, facing where the camera was
    const GText& lg = pk.text[1];
    V3 nz = cross(lg.right, lg.up);
    float dcam = length(lg.center - pk.cam.pos);
    CHECK(dot(nz, pk.cam.pos - lg.center) > 0 && dcam > 2.0f && dcam < 15.0f, "landmark faces the camera, %.2f km out", dcam);
    int on = 0;
    for (int k = 0; k < lg.len; k++) {
      if (lg.ch[k] == ' ') continue;
      V3 base = lg.center + lg.right * ((lg.gx[k] + 0.25f) * lg.height) + lg.up * (lg.gy[k] * lg.height);
      double bp[3] = {base.x, base.y, base.z};
      double ground = std::max(0.0, (double)surface_h(v2(base.x, base.z), 9, host_noise().tab.data())) -
                      (bp[0] * bp[0] + bp[2] * bp[2]) / (2.0 * PLANET_R);
      on += fabs(base.y - ground) < 0.25 * lg.height;
    }
    CHECK(on >= lg.len - 1, "landmark letters stand on the ground (%d of %d)", on, lg.len);
    // a landmark word starts a new hold (its letters shade the terrain)
    uint64_t k1, k2;
    build_frame(scene_with(R"({"text":"Hello","t":100,"text_style":"landmark"})"), tl, 50.0, &k1, &held);
    build_frame(scene_with(R"({"text":"Hello","t":1,"text_style":"landmark"})"), tl, 50.0, &k2, &held);
    CHECK(held && k1 != k2, "a landmark word changes the hold key");
    // a landmark that appears on a frame slot inside a hold shades the terrain
    // the same way on every frame of that hold: tiles that start rendering at
    // the appearance frame and one frame later must accumulate the same samples
    {
      Scene sw = scene_with(R"({"text":"Hello","t":50,"text_style":"landmark"})");
      uint64_t ka, kb;
      bool ha, hb;
      FrameParams fa = build_frame(sw, tl, 50.0, &ka, &ha);
      FrameParams fb = build_frame(sw, tl, 50.0 + 1.0 / 30.0, &kb, &hb);
      int sa = 0, sb = 0;
      for (int i = 0; i < fa.n_text; i++) sa += casts_landmark_shadow(fa.text[i]);
      for (int i = 0; i < fb.n_text; i++) sb += casts_landmark_shadow(fb.text[i]);
      CHECK(ha && hb && ka == kb && sa == 1 && sb == 1,
            "a landmark appearing inside a hold casts its shadow from the hold's first frame (held %d %d, same key %d, shadows %d %d)",
            ha, hb, ka == kb, sa, sb);
    }
  }
  // --- review round: tile width, re-tile height, layout switch, words, wrapped lockup, pipeline locking
  {
    // a rect too wide to encode at the requested height keeps the whole rect
    Rect half_high{0, 0, 1, 0.5};
    Tile t = tile_geometry(half_high, 16.0 / 9, 2160);
    CHECK(t.w <= 4096 && std::abs(t.w - half_high.w * t.full_w) <= 1 && t.h == (int)std::lround(half_high.h * t.full_h),
          "a wide tile covers its rect (w %d of full %d, h %d of full %d)", t.w, t.full_w, t.h, t.full_h);
    Tile two = tile_geometry(Rect{0.5, 0, 0.5, 0.5}, 16.0 / 9, 2160);
    CHECK(two.x0 <= t.w && two.x0 + two.w >= two.full_w - 1, "two wide tiles meet and reach the right edge (%d+%d of %d)",
          two.x0, two.w, two.full_w);
    // a whole image taller than any tile gets the tallest tile, not the shortest
    int hh = tile_h_for(Rect{0, 0, 0.25, 1}, 16.0 / 9, 2880);
    CHECK(hh == 2160, "re-tile to a full-height column from a 2880-high image: tile height %d", hh);
    // a scene without layout_generation replaces a pending switch and ends the generation
    Scene g2;
    g2.layout_generation = 2;
    g2.rect_from_frame = 300;
    g2.has_previous_rect = true;
    g2.previous_rect = Rect{0, 0, 0.5, 0.5};
    g2.rect = Rect{0, 0, 0.5, 1};
    LayoutSwitch ls;
    ls.on_scene(g2);
    CHECK(ls.pending && ls.next == 2 && ls.k == 300, "a new generation is pending");
    ls.on_scene(g2);
    CHECK(ls.pending && ls.k == 300, "the same generation re-posted changes nothing");
    Scene plain;
    ls.on_scene(plain);
    CHECK(!ls.pending && !ls.take(300) && ls.cur == -1, "a generationless scene cancels the pending switch");
    ls.on_scene(g2);
    CHECK(ls.take(300) && ls.cur == 2 && !ls.pending, "the switch takes effect at its frame");
    ls.on_scene(plain);
    CHECK(ls.cur == -1 && !ls.pending, "a generationless scene leaves no generation in force (header null)");
    // leaving a numbered layout must reach open streams: the header names the
    // generation, and with the geometry unchanged nothing else would resend it
    CHECK(ls.take_cleared(), "leaving a numbered layout is reported once");
    CHECK(!ls.take_cleared(), "the report is consumed");
    ls.on_scene(plain);
    CHECK(!ls.take_cleared(), "a generationless scene after a generationless one reports nothing");
    ls.on_scene(g2);
    ls.on_scene(plain);
    CHECK(!ls.take_cleared(), "cancelling a pending switch while no generation was in force reports nothing");
    {
      auto hdr_a = std::make_shared<const std::string>("generation 2");
      auto hdr_b = std::make_shared<const std::string>("generation null");
      Subscriber open;
      open.offer_header(8, hdr_a);
      open.need_key = false;
      open.offer_header(8, hdr_b);
      CHECK(open.q.size() == 1, "an open stream does not get the same encoder's header twice");
      open.restart_header();
      open.offer_header(8, hdr_b);
      CHECK(open.q.size() == 2 && *open.q.back() == "generation null" && open.need_key,
            "a restarted stream gets the new header and waits for a keyframe");
    }
    // a word on screen is found behind words scheduled later
    std::string words = R"({"text":"early","t":1})";
    for (int i = 0; i < 16; i++) words += R"(,{"text":"later","t":)" + std::to_string(100 + i) + "}";
    Scene ws = parse_scene(R"({"epoch_unix_ms":0,"rect":{"x":0,"y":0,"w":1,"h":1},"image_aspect":1.7778,
        "camera":[{"t":0,"pos":[-15,2.2,-100],"look":[-70,1.4,-70],"fov_deg":60}],"time_of_day":0.245,"words":[)" + words + "]}");
    uint64_t key;
    bool held;
    Tile whole = tile_geometry(Rect{0, 0, 1, 1}, 16.0 / 9, 1080);
    FrameParams pw = build_frame(ws, whole, 10.0, &key, &held);
    CHECK(pw.n_text == 2 && memcmp(pw.text[1].ch, "early", 5) == 0, "the word at t=1 shows at t=10 behind 16 later ones");
    // a wrapped lockup message stays inside the image, its lines apart, above the clock
    Scene wl = parse_scene(R"({"epoch_unix_ms":0,"rect":{"x":0,"y":0,"w":1,"h":1},"image_aspect":1.7778,
        "camera":[{"t":0,"pos":[-15,2.2,-100],"look":[-70,1.4,-70],"fov_deg":60}],"time_of_day":0.245,
        "words":[{"text":"WELCOME TO THE AMAZING WORLD","t":1}]})");
    FrameParams pl = build_frame(wl, whole, 3.0, &key, &held);
    auto v_of = [&](const GText& g, float em_up) {
      V3 d = g.center + g.up * (em_up * g.height) - pl.cam.pos;
      return dot(d, pl.cam.up) / (dot(d, pl.cam.fwd) * pl.cam.tan_half);
    };
    CHECK(pl.n_text == 3, "clock and two lines (%d texts)", pl.n_text);
    if (pl.n_text == 3) {
      float top1 = v_of(pl.text[1], SDF_CAP_HEIGHT), base1 = v_of(pl.text[1], 0);
      float top2 = v_of(pl.text[2], SDF_CAP_HEIGHT), base2 = v_of(pl.text[2], 0);
      float clock_top = v_of(pl.text[0], SDF_CAP_HEIGHT);
      CHECK(top1 < 1.0f && top2 < 1.0f, "wrapped lockup inside the image (cap tops at v %.3f, %.3f)", top1, top2);
      CHECK(top2 < base1 && clock_top < base2, "lines apart and above the clock (%.3f < %.3f, %.3f < %.3f)", top2, base1,
            clock_top, base2);
    }
    // a lockup message wider than the image, with no space to wrap at or wrapped
    // into lines still too wide, ends inside the image
    auto right_u = [](const FrameParams& f, const GText& g) {
      V3 d = g.center + g.right * (g.width * g.height) - f.cam.pos;
      return dot(d, f.cam.right) / (dot(d, f.cam.fwd) * f.cam.tan_half * (16.0f / 9));
    };
    for (const char* text : {"WWWWWWWWWWWWWWWWWWWWWWWWWWWW",
                             "W WWWWWWWWWWWWWWWWWWWWWWWWWW"}) {
      Scene ww = parse_scene(std::string(R"({"epoch_unix_ms":0,"rect":{"x":0,"y":0,"w":1,"h":1},"image_aspect":1.7778,
          "camera":[{"t":0,"pos":[-15,2.2,-100],"look":[-70,1.4,-70],"fov_deg":60}],"time_of_day":0.245,
          "words":[{"text":")") + text + R"(","t":1}]})");
      FrameParams pw2 = build_frame(ww, whole, 3.0, &key, &held);
      float worst = -1e9f;
      for (int i = 1; i < pw2.n_text; i++) worst = std::max(worst, right_u(pw2, pw2.text[i]));
      CHECK(pw2.n_text >= 2 && worst < 1.0f, "a long lockup message ends inside the image (%zu chars, right edge u %.3f)",
            strlen(text), worst);
    }
    // the boot id: 32 hex digits, a new one per process, reported by /health
    {
      std::string a = new_boot_id(), b = new_boot_id();
      bool hex = a.size() == 32;
      for (char c : a) hex = hex && ((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f'));
      CHECK(hex, "a boot id is 32 lowercase hex digits: %s", a.c_str());
      CHECK(a != b, "two boot ids differ: %s %s", a.c_str(), b.c_str());
      bool reported = false, kept = false;
      for (auto& l : source_lines("../src/pipeline.cpp"))
        reported = reported || (l.find("\\\"boot_id\\\":") != std::string::npos && l.find("boot_id_") != std::string::npos);
      for (auto& l : source_lines("../src/pipeline.h"))
        kept = kept || l.find("const std::string boot_id_ = new_boot_id();") != std::string::npos;
      CHECK(kept && reported, "the pipeline makes one boot id at start (%d) and health_json reports it (%d)", kept, reported);
    }
    // pipeline.cpp: the fields health_json and fatal() read on server threads are
    // written under hmu_, and a failed encoder open gives its buffer back
    std::vector<std::string> src = source_lines("../src/pipeline.cpp");
    CHECK(src.size() > 100, "read src/pipeline.cpp (%zu lines)", src.size());
    int writes = 0, unguarded = 0;
    for (size_t i = 0; i < src.size(); i++) {
      for (const char* f : {"fatal_ =", "gpu_name_ =", "startup_ready_ms_ =", "first_frame_ms_ =", "first_sent_ms_ ="}) {
        if (src[i].find(f) == std::string::npos) continue;
        writes++;
        if (!under_hmu(src, i)) { unguarded++; fprintf(stderr, "  unguarded: pipeline.cpp:%zu: %s\n", i + 1, src[i].c_str()); }
      }
    }
    CHECK(writes >= 5 && unguarded == 0, "health fields written under hmu_ (%d writes, %d not)", writes, unguarded);
    bool fatal_read_locked = false, failed_retired = false;
    for (size_t i = 0; i < src.size(); i++) {
      if (src[i].find("return fatal_;") != std::string::npos) fatal_read_locked = under_hmu(src, i);
      if (src[i].find("if (ready.failed) {") != std::string::npos)
        for (size_t j = i + 1; j < src.size() && src[j].find("continue;") == std::string::npos; j++)
          failed_retired = failed_retired || src[j].find("retire(ready)") != std::string::npos;
    }
    CHECK(fatal_read_locked, "fatal() reads fatal_ under hmu_");
    CHECK(failed_retired, "a failed encoder open is retired (its NV12 buffer freed) before the stream is closed");
    // frame_once restarts every stream's header, under mu_, when a scene ended the numbered layout
    bool restarts = false, restart_locked = false, idr_forced = false;
    for (size_t i = 0; i < src.size(); i++) {
      if (src[i].find("lay_.take_cleared()") != std::string::npos) {
        restart_locked = src[i].find("if (") != std::string::npos;
        for (size_t j = i + 1; j < i + 4 && j < src.size(); j++) restarts = restarts || src[j].find("restart_header()") != std::string::npos;
      }
      if (src[i].find("generation_cleared) e.want_idr = true") != std::string::npos) idr_forced = true;
    }
    CHECK(restarts && restart_locked, "frame_once restarts the headers of open streams when the numbered layout ended");
    CHECK(idr_forced, "the encoders force a keyframe for the restarted headers");
  }
  // --- a connection the system has no thread for is refused, not fatal
  {
    // a maker that refuses, as std::thread's constructor does at a pids limit
    ThreadMaker refuse = [](std::function<void()>) -> std::thread {
      throw std::system_error(std::make_error_code(std::errc::resource_unavailable_try_again));
    };
    bool ran = false;
    std::thread t;
    CHECK(!start_thread([&] { ran = true; }, &t, refuse) && !ran && !t.joinable(),
          "a refused joinable thread reports false and its function never runs");
    CHECK(!start_thread([&] { ran = true; }, nullptr, refuse) && !ran,
          "a refused detached thread reports false and its function never runs");
    std::atomic<int> runs{0};
    CHECK(start_thread([&] { runs++; }, &t) && t.joinable(), "a thread the system gives is started");
    t.join();
    CHECK(runs == 1, "its function ran once");
    // every per-connection thread in server.cpp goes through start_thread: the
    // accept loop's and the stream writer's, and no other thread is started per
    // connection
    std::vector<std::string> src = source_lines("../src/server.cpp");
    // a thread constructed in place: std::thread(...) or std::thread name(...) / {...};
    // serve()'s TLS accept loop, started once at startup, is the one allowed
    const std::regex constructed(R"(std::thread(\s+\w+)?\s*[({])");
    int starts = 0, raw = 0;
    for (auto& l : src) {
      if (l.find("start_thread(") != std::string::npos) starts++;
      if (std::regex_search(l, constructed) && l.find("accept_loop(tfd_, true)") == std::string::npos) raw++;
    }
    CHECK(starts == 2 && raw == 0, "server.cpp starts its per-connection threads with start_thread (%d) and no other way (%d)",
          starts, raw);
  }
  // --- a slow subscriber whose queue is cleared still gets the stream header
  {
    auto msg = [](const char* m) { return std::make_shared<const std::string>(m); };
    Subscriber s;
    s.offer_header(1080, msg("hdr-1080"));
    CHECK(s.offer_frame(msg("key"), true) == Subscriber::SENT, "the first keyframe goes out");
    for (int i = 0; i <= SUB_MAX_QUEUED_MSGS; i++) s.push(msg("frame"));  // the reader stalls
    s.offer_header(720, msg("hdr-720"));  // its height changes: a new header is queued
    CHECK(s.offer_frame(msg("key"), true) == Subscriber::DROPPED, "an overflowing queue is dropped");
    // the next frame: the header for the current key, then the recovery keyframe
    s.offer_header(720, msg("hdr-720"));
    CHECK(s.offer_frame(msg("key"), false) == Subscriber::SKIPPED, "a delta frame waits for the keyframe");
    CHECK(s.offer_frame(msg("key"), true) == Subscriber::SENT, "the recovery keyframe goes out");
    std::string order;
    for (auto& m : s.q) order += *m + " ";
    CHECK(s.q.size() == 2 && *s.q[0] == "hdr-720" && *s.q[1] == "key",
          "after a dropped queue the header is sent again before the keyframe (queue: %s)", order.c_str());
  }
  {
    // A camera that sees no land: find_ridge_cached answers false and still
    // writes at and dist, since its cache stores them with the miss.
    const double pos[3] = {0.0, 2.0, 0.0}, look[3] = {0.0, 50.0, 0.001};
    GCam sky = make_cam(pos, look, 60.0, 16.0 / 9.0);
    V3 at = v3(NAN, NAN, NAN);
    float dist = NAN;
    bool ok = find_ridge_cached(sky, 16.0f / 9.0f, 4.0f, 0.1f, &at, &dist);
    CHECK(!ok && dist == 0.0f && at.x == sky.pos.x && at.y == sky.pos.y && at.z == sky.pos.z,
          "a ridge search that finds no land still writes its outputs (ok %d, dist %g, at.y %g)", ok, dist, at.y);
  }
  // The quality reference holds the camera where the frame has it and draws
  // the clouds and water at the frame's layer time: moving and after the
  // demo loop wraps (t 136 is scene time 16), and inside a hold (scene time
  // 5 of the hold 0..10, whose layer time is its start).
  {
    Scene s = default_scene();
    Tile tl = tile_geometry(Rect{}, s.image_aspect, 270);
    for (double t : {16.0, 136.0, 125.0}) {
      uint64_t k1, k2;
      bool h1, h2;
      FrameParams a = build_frame(s, tl, t, &k1, &h1);
      FrameParams b = build_frame(held_reference(s, t), tl, t, &k2, &h2);
      CHECK(h2 && b.layer_t == a.layer_t && b.wind.x == a.wind.x && b.wave_t == a.wave_t &&
                std::fabs(b.cam.pos.x - a.cam.pos.x) < 1e-6f && std::fabs(b.cam.fwd.z - a.cam.fwd.z) < 1e-6f,
            "the held reference of t %g: held %d, layer_t %g against %g", t, (int)h2, b.layer_t, a.layer_t);
    }
  }
  tlscheck::run();
  DONE();
}
