// Host-only tests of the Windows bridge: the desktop check and its image files, the colour
// conversion, the encoder's stream, the fixed actions, the channel check, the stream header,
// /health and the reconnect schedule. No Windows and no FreeRDP.
#include <math.h>
#include <png.h>
#include <string.h>
#include <unistd.h>

#include <fstream>
#include <sstream>
#include <string>
#include <type_traits>
#include <vector>

#include "check.h"
#include "h264.h"
#include "json.h"
#include "logic.h"
#include "x264enc.h"

namespace {
// A source file next to this one, whole ("" when unreadable, which fails the checks).
std::string source_text(const std::string& name) {
  std::string me = __FILE__;
  std::ifstream f(me.substr(0, me.find_last_of('/') + 1) + name);
  std::stringstream ss;
  ss << f.rdbuf();
  return ss.str();
}
// The body of the function whose definition starts with `head` (to the first closing brace
// indented as `head` is), "" when absent.
std::string function_body(const std::string& src, const std::string& head) {
  size_t a = src.find(head);
  if (a == std::string::npos) return "";
  std::string end = "\n" + head.substr(0, head.find_first_not_of(' ')) + "}\n";
  size_t b = src.find(end, a);
  return b == std::string::npos ? "" : src.substr(a, b - a + end.size());
}
size_t count(const std::string& s, const std::string& what) {
  size_t n = 0;
  for (size_t p = s.find(what); p != std::string::npos; p = s.find(what, p + 1)) n++;
  return n;
}
std::vector<uint8_t> bgrx_fill(int w, int h, uint8_t r, uint8_t g, uint8_t b) {
  std::vector<uint8_t> v((size_t)w * h * 4);
  for (size_t i = 0; i < v.size(); i += 4) {
    v[i] = b;
    v[i + 1] = g;
    v[i + 2] = r;
    v[i + 3] = 0;
  }
  return v;
}
Thumb thumb_fill(float r, float g, float b) {
  Thumb t;
  t.rgb.resize((size_t)THUMB_W * THUMB_H * 3);
  for (size_t i = 0; i < t.rgb.size(); i += 3) {
    t.rgb[i] = r;
    t.rgb[i + 1] = g;
    t.rgb[i + 2] = b;
  }
  return t;
}
// A desktop-like thumbnail: a gradient (not flat) with a bar along the bottom.
Thumb desktop_thumb(float shift = 0) {
  Thumb t = thumb_fill(0, 0, 0);
  for (int y = 0; y < THUMB_H; y++)
    for (int x = 0; x < THUMB_W; x++) {
      float* p = &t.rgb[((size_t)y * THUMB_W + x) * 3];
      bool bar = y >= THUMB_H - 6;
      p[0] = std::min(1.f, (bar ? 0.1f : 0.1f + 0.5f * x / THUMB_W) + shift);
      p[1] = std::min(1.f, (bar ? 0.1f : 0.3f + 0.4f * y / THUMB_H) + shift);
      p[2] = std::min(1.f, (bar ? 0.15f : 0.7f) + shift);
    }
  return t;
}
std::vector<KeyEvent> keys_of(const Action& a) {
  std::vector<KeyEvent> out;
  for (int i = 0; i < a.nsteps; i++) step_keys(a.steps[i], &out);
  return out;
}
std::vector<std::string> texts_of(const Action& a) {
  std::vector<std::string> out;
  for (int i = 0; i < a.nsteps; i++)
    if (a.steps[i].kind == STEP_TEXT) out.push_back(a.steps[i].text);
  return out;
}
// NAL unit types of an Annex-B access unit.
std::vector<int> nal_types(const std::vector<uint8_t>& au) {
  std::vector<int> t;
  for (size_t i = 0; i + 3 < au.size(); i++)
    if (au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1) {
      t.push_back(au[i + 3] & 0x1f);
      i += 2;
    }
  return t;
}
}  // namespace

int main() {
  // ---- the desktop comparison
  {
    Thumb d = desktop_thumb();
    CHECK(thumb_rmse(d, d) == 0, "a thumbnail matches itself exactly");
    CHECK(fabs(thumb_rmse(thumb_fill(0, 0, 0), thumb_fill(1, 1, 1)) - 1.0) < 1e-9, "black against white is RMSE 1");
    CHECK(fabs(thumb_rmse(thumb_fill(0.5f, 0.5f, 0.5f), thumb_fill(0.54f, 0.54f, 0.54f)) - 0.04) < 1e-6,
          "an even 0.04 shift is RMSE 0.04");
    CHECK(thumb_rmse(d, Thumb()) == 1.0 && thumb_rmse(Thumb(), Thumb()) == 1.0, "an empty thumbnail matches nothing");
    // the rule: within 0.05
    CHECK(thumb_rmse(thumb_fill(0.5f, 0.5f, 0.5f), thumb_fill(0.549f, 0.549f, 0.549f)) <= DESKTOP_RMSE, "0.049 matches");
    CHECK(thumb_rmse(thumb_fill(0.5f, 0.5f, 0.5f), thumb_fill(0.551f, 0.551f, 0.551f)) > DESKTOP_RMSE, "0.051 does not");
    CHECK(thumb_stddev(thumb_fill(0.3f, 0.3f, 0.3f)) < 1e-6 && thumb_stddev(d) > SETTLE_MIN_STDDEV, "flat and not flat");
  }
  {  // the downscale: each cell is its area's mean, red from BGRX's third byte
    const int W = 320, H = 180;
    std::vector<uint8_t> px((size_t)W * H * 4, 0);
    for (int y = 0; y < H; y++)
      for (int x = 0; x < W; x++) {
        uint8_t* p = &px[((size_t)y * W + x) * 4];
        p[2] = ((x + y) & 1) ? 255 : 0;  // red checker
        p[0] = x < W / 2 ? 255 : 0;      // blue on the left half
      }
    Thumb t = thumb_from_bgrx(px.data(), W, H, (size_t)W * 4);
    CHECK(t.rgb.size() == (size_t)THUMB_W * THUMB_H * 3, "160 x 90 x 3");
    CHECK(fabs(t.rgb[0] - 0.5) < 1e-6 && fabs(t.rgb[1]) < 1e-6 && fabs(t.rgb[2] - 1.0) < 1e-6, "cell (0,0): %f %f %f",
          t.rgb[0], t.rgb[1], t.rgb[2]);
    const float* last = &t.rgb[t.rgb.size() - 3];
    CHECK(fabs(last[0] - 0.5) < 1e-6 && fabs(last[2]) < 1e-6, "the last cell: red half, no blue");
    // 1280 x 720 with padding in the stride
    std::vector<uint8_t> big = bgrx_fill(1280 + 8, 720, 10, 200, 30);
    Thumb tb = thumb_from_bgrx(big.data(), 1280, 720, (size_t)(1280 + 8) * 4);
    CHECK(fabs(tb.rgb[0] - 10 / 255.f) < 1e-5 && fabs(tb.rgb[1] - 200 / 255.f) < 1e-5 && fabs(tb.rgb[2] - 30 / 255.f) < 1e-5,
          "a stride wider than the picture");
  }
  {  // with a reference: the first frame within the RMSE
    DesktopDetector d(desktop_thumb());
    CHECK(d.has_reference() && std::string(d.by()) == "reference", "reference mode");
    d.frame(1000, thumb_fill(0, 0, 0));  // black
    d.frame(1500, thumb_fill(0.1f, 0.3f, 0.6f));  // "Please wait": blue
    CHECK(!d.found(), "black and a blue screen are not the desktop");
    d.frame(2000, desktop_thumb(0.03f));
    CHECK(d.found() && d.desktop_ms() == 2000, "the first frame within the RMSE is the desktop: %f", d.desktop_ms());
    d.frame(3000, desktop_thumb());
    CHECK(d.desktop_ms() == 2000, "only the first");
  }
  {  // without one: the first frame the surface then stays at for 2 s
    DesktopDetector d{Thumb()};
    CHECK(!d.has_reference() && std::string(d.by()) == "settled", "settle mode");
    d.frame(0, thumb_fill(0.1f, 0.3f, 0.6f));
    d.frame(500, desktop_thumb());
    d.frame(1000, desktop_thumb(0.005f));  // a clock digit: under SETTLE_RMSE, not a change
    d.tick(2400);
    CHECK(!d.found(), "1.9 s is not settled");
    d.tick(2500);
    CHECK(d.found() && d.desktop_ms() == 500, "settled at the frame it stayed at: %f", d.desktop_ms());
    CHECK(thumb_rmse(d.desktop_thumb(), desktop_thumb()) == 0, "and that frame's thumbnail");
  }
  {  // a change restarts the wait; a new frame after 2 s still dates the desktop from before it
    DesktopDetector d{Thumb()};
    d.frame(0, desktop_thumb());
    d.frame(1500, desktop_thumb(0.2f));
    d.tick(3000);
    CHECK(!d.found(), "a change at 1.5 s restarts the 2 s");
    d.frame(3600, desktop_thumb());
    CHECK(d.found() && d.desktop_ms() == 1500, "a frame arriving after the settle ends it at the settled frame");
  }
  {  // a flat picture (black, or one colour) never counts as settled
    DesktopDetector d{Thumb()};
    d.frame(0, thumb_fill(0, 0, 0));
    d.tick(10000);
    CHECK(!d.found(), "a black screen is not the desktop");
    DesktopDetector e(Thumb(), 5000);
    e.frame(0, desktop_thumb());
    e.tick(4999);
    CHECK(!e.found(), "logon's settle time is its own");
    e.tick(5000);
    CHECK(e.found(), "settled after 5 s");
  }
  {  // the reference files: the PPM written, a PNG, refusals
    std::string dir = "/tmp/windows-bridge-test-" + std::to_string(getpid());
    std::string ppm = dir + ".ppm", png = dir + ".png", bad = dir + ".txt";
    std::string err;
    Thumb d = desktop_thumb();
    CHECK(write_thumb_ppm(ppm, d, &err), "write: %s", err.c_str());
    Thumb back;
    CHECK(load_thumb(ppm, &back, &err), "load PPM: %s", err.c_str());
    CHECK(thumb_rmse(back, d) < 1.0 / 255, "the PPM round trip: %f", thumb_rmse(back, d));
    // a full-size PNG of the same picture loads as the same thumbnail
    std::vector<uint8_t> rgb((size_t)1280 * 720 * 3);
    for (int y = 0; y < 720; y++)
      for (int x = 0; x < 1280; x++)
        for (int c = 0; c < 3; c++)
          rgb[((size_t)y * 1280 + x) * 3 + c] = (uint8_t)lround(d.rgb[((size_t)(y / 8) * THUMB_W + x / 8) * 3 + c] * 255);
    png_image im;
    memset(&im, 0, sizeof im);
    im.version = PNG_IMAGE_VERSION;
    im.width = 1280;
    im.height = 720;
    im.format = PNG_FORMAT_RGB;
    CHECK(png_image_write_to_file(&im, png.c_str(), 0, rgb.data(), 0, nullptr), "png write");
    Thumb fromp;
    CHECK(load_thumb(png, &fromp, &err), "load PNG: %s", err.c_str());
    CHECK(thumb_rmse(fromp, d) < 1.0 / 255, "a 1280 x 720 PNG reduces to the same thumbnail: %f", thumb_rmse(fromp, d));
    std::ofstream(bad) << "not an image";
    CHECK(!load_thumb(bad, &fromp, &err) && err.find("neither") != std::string::npos, "a text file is refused: %s",
          err.c_str());
    CHECK(!load_thumb(dir + ".missing", &fromp, &err), "a missing file is refused");
    std::ofstream(bad) << "P6\n1 1\n65535\n";
    CHECK(!load_thumb(bad, &fromp, &err), "a 16-bit PPM is refused");
    unlink(ppm.c_str());
    unlink(png.c_str());
    unlink(bad.c_str());
  }

  // ---- BGRX to I420
  {
    // 4 x 4: red, green / blue, white quadrants, in a stride 8 bytes wider than the picture
    const int W = 4, H = 4;
    const size_t stride = W * 4 + 8;
    std::vector<uint8_t> px(stride * H, 0x77);
    auto set = [&](int x, int y, uint8_t r, uint8_t g, uint8_t b) {
      uint8_t* p = &px[y * stride + x * 4];
      p[0] = b;
      p[1] = g;
      p[2] = r;
      p[3] = 0x55;  // the unused byte is ignored
    };
    for (int y = 0; y < H; y++)
      for (int x = 0; x < W; x++) {
        bool right = x >= 2, bottom = y >= 2;
        if (!bottom && !right) set(x, y, 255, 0, 0);
        else if (!bottom) set(x, y, 0, 255, 0);
        else if (!right) set(x, y, 0, 0, 255);
        else set(x, y, 255, 255, 255);
      }
    std::vector<uint8_t> yv(16), u(4), v(4);
    bgrx_to_i420(px.data(), stride, W, H, yv.data(), u.data(), v.data());
    // BT.709 limited range: red (63, 102, 240), green (173, 42, 26), blue (32, 240, 118), white (235, 128, 128)
    CHECK(yv[0] == 63 && yv[2] == 173 && yv[8] == 32 && yv[10] == 235 && yv[15] == 235, "Y: %d %d %d %d", yv[0], yv[2], yv[8],
          yv[10]);
    CHECK(u[0] == 102 && u[1] == 42 && u[2] == 240 && u[3] == 128, "U: %d %d %d %d", u[0], u[1], u[2], u[3]);
    CHECK(v[0] == 240 && v[1] == 26 && v[2] == 118 && v[3] == 128, "V: %d %d %d %d", v[0], v[1], v[2], v[3]);
    std::vector<uint8_t> black = bgrx_fill(2, 2, 0, 0, 0), y1(4), u1(1), v1(1);
    bgrx_to_i420(black.data(), 8, 2, 2, y1.data(), u1.data(), v1.data());
    CHECK(y1[0] == 16 && u1[0] == 128 && v1[0] == 128, "black is (16, 128, 128)");
    std::vector<uint8_t> rgba(16 * 4);
    bgrx_to_rgba(px.data(), stride, W, H, rgba.data());
    CHECK(rgba[0] == 255 && rgba[1] == 0 && rgba[2] == 0 && rgba[3] == 255 && rgba[(2 * 4 + 0) * 4 + 2] == 255,
          "RGBA for /still: red first, alpha 255");
  }

  // ---- the encoder's stream
  {
    X264Encoder e;
    std::string err;
    const int W = 320, H = 180;
    CHECK(e.open(W, H, 30, 1, &err), "open: %s", err.c_str());
    CHECK(e.reorder().parsed && e.reorder().restriction && e.reorder().max_num_reorder_frames == 0 &&
              e.reorder().max_dec_frame_buffering == e.reorder().max_num_ref_frames,
          "the SPS says no reordering (the page shows each frame as it decodes it)");
    CHECK(e.codec().rfind("avc1.42", 0) == 0 || e.codec().rfind("avc1.4D", 0) == 0, "baseline or main: %s", e.codec().c_str());
    CHECK(e.bitrate() == quality_rate_bps(1, W, H, 30), "the header's rate is the renderer's for std");
    std::vector<uint8_t> pic((size_t)W * H * 3 / 2, 128);
    for (int i = 0; i < W * H; i++) pic[i] = (uint8_t)(i % 251);
    const uint8_t *y = pic.data(), *u = y + W * H, *v = u + W * H / 4;
    std::vector<uint8_t> au;
    bool key = false;
    CHECK(e.encode(y, u, v, false, &au, &key, &err) && key, "the first picture is a keyframe");
    CHECK(au.size() > 4 && au[0] == 0 && au[1] == 0 && au[2] == 0 && au[3] == 1, "Annex B");
    std::vector<int> t = nal_types(au);
    CHECK(count(std::string(t.begin(), t.end()), std::string(1, (char)7)) == 1 &&
              count(std::string(t.begin(), t.end()), std::string(1, (char)8)) == 1 &&
              count(std::string(t.begin(), t.end()), std::string(1, (char)5)) == 1,
          "SPS, PPS and one IDR slice (%zu NAL units)", t.size());
    size_t sl = 0;
    CHECK(find_sps(au.data(), au.size(), &sl) >= 0, "the keyframe carries its SPS");
    CHECK(e.encode(y, u, v, false, &au, &key, &err) && !key, "the next is not");
    t = nal_types(au);
    CHECK(t.size() == 1 && t[0] == 1, "one slice, no SPS");
    CHECK(e.encode(y, u, v, true, &au, &key, &err) && key, "a forced keyframe");
    CHECK(find_sps(au.data(), au.size(), &sl) >= 0, "with SPS and PPS again");
    X264Encoder lo, hi;
    CHECK(lo.open(1280, 720, 30, 0, &err) && hi.open(1280, 720, 30, 2, &err), "lo and hi open: %s", err.c_str());
    CHECK(lo.bitrate() == 6912000 && hi.bitrate() == 110592000 && quality_rate_bps(1, 1280, 720, 30) == 20000000,
          "the renderer's rates: lo %u, hi %u", lo.bitrate(), hi.bitrate());
    X264Encoder odd;
    CHECK(!odd.open(321, 180, 30, 1, &err), "an odd width is refused");
  }
  {  // the header and t
    Json j = Json::parse(bridge_header_json(1280, 720, "avc1.42C01F", 30, "std", 20000000));
    CHECK(j.at("width").num == 1280 && j.at("height").num == 720 && j.at("full_width").num == 1280 &&
              j.at("full_height").num == 720 && j.at("x0").num == 0 && j.at("y0").num == 0 && j.at("fps").num == 30 &&
              j.at("crop").at("bottom").num == 0 && j.at("q").str == "std" && j.at("codec").str == "avc1.42C01F" &&
              j.at("layout_generation").type == Json::Null,
          "a whole-image header");
    CHECK(frame_slot(2050, 1000, 30) == 31 && fabs(frame_slot_t(2050, 1000, 30) - 31.0 / 30) < 1e-12,
          "t is the 1/30 s slot on the scene's clock");
    CHECK(frame_slot(999, 1000, 30) == -1, "before the epoch, the slot before");
  }

  // ---- the fixed actions
  {
    static_assert(std::is_const<std::remove_extent<decltype(kActions)>::type>::value, "the action table is constant");
    static_assert(std::is_same<decltype(Action::steps), const Step*>::value, "an action's steps are constant");
    static_assert(std::is_same<decltype(Step::text), const char*>::value, "a step's text is a constant string");
    const char* names[] = {"start_menu", "notepad", "explorer", "task_manager", "about", "terminal", "close_all"};
    CHECK(kActionCount == 7, "seven actions");
    for (const char* n : names) {
      const Action* a = action_find(n);
      CHECK(a && std::string(a->name) == n && a->nsteps > 0, "%s", n);
      if (!a) continue;
      std::vector<KeyEvent> k;
      for (int i = 0; i < a->nsteps; i++)
        if (a->steps[i].kind == STEP_CHORD || a->steps[i].kind == STEP_TEXT)
          CHECK(step_keys(a->steps[i], &k), "%s step %d maps to keys", n, i);
      // every key that goes down comes up, in the action itself
      int held = 0;
      for (const KeyEvent& e : k) held += e.down ? 1 : -1;
      CHECK(held == 0, "%s releases every key it presses", n);
    }
    CHECK(action_names() == "start_menu, notepad, explorer, task_manager, about, terminal, close_all", "%s",
          action_names().c_str());
    for (const char* n : {"", "Notepad", "notepad ", "reset", "type", "cmd", "../notepad"})
      CHECK(!action_find(n), "\"%s\" is no action", n);
    std::string r = action_refusal("format c:");
    CHECK(r == "unknown action \"format c:\": the actions are start_menu, notepad, explorer, task_manager, about, terminal, "
               "close_all",
          "%s", r.c_str());
    std::string err;
    const Action* a = action_from_body("{\"name\":\"notepad\"}", &err);
    CHECK(a && std::string(a->name) == "notepad", "a body names an action");
    // what a caller adds changes nothing: the keys are the table's
    const Action* b = action_from_body("{\"name\":\"notepad\",\"text\":\"del /s C:\\\\\",\"keys\":[1,2]}", &err);
    CHECK(b == a && keys_of(*b) == keys_of(*a), "extra fields are ignored");
    CHECK(texts_of(*a) == std::vector<std::string>({"notepad", "fcvm Windows sandbox"}), "notepad types its fixed line only");
    CHECK(texts_of(*action_find("about")) == std::vector<std::string>({"winver"}), "about runs winver");
    CHECK(texts_of(*action_find("terminal")) ==
              std::vector<std::string>(
                  {"powershell -NoExit -Command Get-ComputerInfo -Property OsName,OsVersion,CsProcessors,CsTotalPhysicalMemory"}),
          "terminal runs Get-ComputerInfo for the OS fields");
    CHECK(!action_from_body("{\"name\":\"shutdown\"}", &err) && err == action_refusal("shutdown"), "refused: %s", err.c_str());
    CHECK(!action_from_body("{\"name\":3}", &err) && err.find("the actions are") != std::string::npos, "a name that is no string");
    CHECK(!action_from_body("not json", &err), "a body that is no JSON");
    CHECK(!action_from_body("[\"notepad\"]", &err), "a body that is no object");
    // chords press in order and release in reverse; capitals hold Shift
    std::vector<KeyEvent> k;
    step_keys(action_find("task_manager")->steps[0], &k);
    CHECK(k == std::vector<KeyEvent>({{SC_LCONTROL, true}, {SC_LSHIFT, true}, {SC_ESCAPE, true}, {SC_ESCAPE, false},
                                      {SC_LSHIFT, false}, {SC_LCONTROL, false}}),
          "Ctrl+Shift+Esc");
    k.clear();
    Step s{STEP_TEXT, {0, 0, 0}, 0, "Ab1-,", 0};
    CHECK(step_keys(s, &k) && k == std::vector<KeyEvent>({{SC_LSHIFT, true}, {0x1E, true}, {0x1E, false}, {SC_LSHIFT, false},
                                                          {0x30, true}, {0x30, false}, {0x02, true}, {0x02, false},
                                                          {0x0C, true}, {0x0C, false}, {0x33, true}, {0x33, false}}),
          "US scancodes");
    k.clear();
    Step bad{STEP_TEXT, {0, 0, 0}, 0, "a;b", 0};
    CHECK(!step_keys(bad, &k), "a character the map lacks is refused, not skipped");
    k.clear();
    CHECK(step_keys(kCloseWindowStep, &k) &&
              k == std::vector<KeyEvent>({{SC_LMENU, true}, {SC_F4, true}, {SC_F4, false}, {SC_LMENU, false}}),
          "close_all presses Alt+F4");
    CHECK(action_find("close_all")->steps[0].kind == STEP_CLOSE_UNTIL_DESKTOP && action_find("close_all")->steps[0].ms == 10,
          "close_all tries at most 10 times");
    CHECK(action_find("start_menu")->nsteps == 1 && action_find("start_menu")->steps[0].keys[0] == SC_LWIN,
          "start_menu is the Windows key");
    CHECK(action_find("explorer")->steps[0].keys[0] == SC_LWIN && action_find("explorer")->steps[0].keys[1] == SC_KEY_E,
          "explorer is Win+E");
  }
  {  // the table is the only source of key events (source-level: a key event reaches Windows
     // only through RdpLink::send_keys, which only Bridge::play calls, with step_keys' events)
    std::string rdp = source_text("rdp.cpp"), bridge = source_text("bridge.cpp"), main = source_text("main.cpp"),
                logic = source_text("logic.cpp");
    CHECK(!rdp.empty() && !bridge.empty() && !main.empty() && !logic.empty(), "sources read");
    std::string drain = function_body(rdp, "void RdpLink::drain_keys(");
    CHECK(count(rdp, "freerdp_input_send") == 1 && count(drain, "freerdp_input_send_keyboard_event_ex") == 1,
          "one FreeRDP input call, in drain_keys");
    for (const std::string* s : {&rdp, &bridge, &main, &logic})
      CHECK(count(*s, "freerdp_input_send_unicode") == 0 && count(*s, "freerdp_input_send_mouse") == 0 &&
                count(*s, "send_extended_mouse") == 0 && count(*s, "freerdp_input_send_rel") == 0,
            "no unicode typing and no mouse");
    CHECK(count(rdp, "keys_.push_back(") == 1 && count(function_body(rdp, "bool RdpLink::send_keys("), "keys_.push_back(") == 1,
          "the queue is filled by send_keys only");
    std::string play = function_body(bridge, "int Bridge::play(");
    CHECK(count(bridge, "send_keys(") == 2 && count(play, "send_keys(") == 2, "send_keys is called from Bridge::play only");
    CHECK(count(main, "send_keys(") == 0 && count(logic, "send_keys(") == 0, "nowhere else");
    CHECK(count(play, "step_keys(") == 2 && count(play, "std::vector<KeyEvent> keys;") == 1 && count(play, "keys.push_back") == 0 &&
              count(play, "KeyEvent{") == 0,
          "and play's keys come from step_keys");
    CHECK(count(play, "step_keys(s, &keys)") == 1 && count(play, "step_keys(kCloseWindowStep, &keys)") == 1 &&
              count(play, "const Step& s = a.steps[i];") == 1,
          "over the action's own steps and the close step");
    CHECK(count(function_body(bridge, "int Bridge::post_action("), "action_from_body(body") == 1 &&
              count(function_body(bridge, "int Bridge::post_action("), "play(*a") == 1,
          "POST /action only names an action in the table");
    std::string sk = function_body(logic, "bool step_keys(");
    CHECK(count(logic, "out->push_back(") == count(sk, "out->push_back(") && count(sk, "out->push_back(") > 0,
          "KeyEvents are made in step_keys only");
  }

  // ---- the channel check
  {
    RdpSettingsView good;
    for (int i = 0; i < kForbiddenSettingCount; i++) good.bools.push_back({kForbiddenSettings[i], false});
    for (int i = 0; i < kOffSettingCount; i++) good.bools.push_back({kOffSettings[i], false});
    for (int i = 0; i < kRequiredSettingCount; i++) good.bools.push_back({kRequiredSettings[i], true});
    good.dynamic_channels = {"ainput", "rdpgfx"};
    good.proxy_ignored = true;
    CHECK(settings_violation(good) == "", "the bridge's settings pass: %s", settings_violation(good).c_str());
    for (const char* n : {"RedirectClipboard", "RedirectDrives", "RedirectHomeDrive", "RedirectPrinters", "AudioPlayback",
                          "AudioCapture", "RedirectSmartCards", "RedirectSerialPorts", "RedirectParallelPorts",
                          "DeviceRedirection", "RemoteApplicationMode", "RedirectWebAuthN"}) {
      RdpSettingsView v = good;
      bool found = false;
      for (auto& kv : v.bools)
        if (kv.first == n) { kv.second = true; found = true; }
      CHECK(found, "%s is checked", n);
      std::string e = settings_violation(v);
      CHECK(e == std::string(n) + " is on", "%s on is refused: %s", n, e.c_str());
    }
    {
      RdpSettingsView v = good;
      v.bools.erase(v.bools.begin());
      CHECK(settings_violation(v) == std::string(kForbiddenSettings[0]) + " not read", "a setting not read is refused");
    }
    for (const char* n : {"NlaSecurity", "RdpSecurity"}) {
      RdpSettingsView v = good;
      for (auto& kv : v.bools)
        if (kv.first == n) kv.second = true;
      CHECK(settings_violation(v) == std::string(n) + " is on", "%s is refused", n);
    }
    for (const char* n : {"TlsSecurity", "SupportGraphicsPipeline", "GfxH264", "GfxAVC444"}) {
      RdpSettingsView v = good;
      for (auto& kv : v.bools)
        if (kv.first == n) kv.second = false;
      CHECK(settings_violation(v) == std::string(n) + " is off", "%s off is refused", n);
    }
    for (const char* c : {"cliprdr", "rdpsnd", "audin", "rdpdr", "urbdrc", "rail", "rdpecam", "rdp2tcp"}) {
      RdpSettingsView v = good;
      v.dynamic_channels.push_back(c);
      CHECK(settings_violation(v) == std::string("dynamic channel ") + c, "dynamic %s refused", c);
      v = good;
      v.static_channels.push_back(c);
      CHECK(settings_violation(v) == std::string("static channel ") + c, "static %s refused", c);
    }
    RdpSettingsView d = good;
    d.devices = 1;
    CHECK(settings_violation(d) == "1 redirected devices", "a redirected device is refused");
    RdpSettingsView px = good;
    px.proxy_ignored = false;
    CHECK(settings_violation(px) == "a proxy may be used", "a proxy is refused");
    for (const char* n : {"GatewayEnabled", "SupportMultitransport", "AutoReconnectionEnabled"}) {
      RdpSettingsView v = good;
      for (auto& kv : v.bools)
        if (kv.first == n) kv.second = true;
      CHECK(settings_violation(v) == std::string(n) + " is on", "%s is refused", n);
    }
    // the link checks the settings as configured and again once FreeRDP has loaded its channels,
    // and refuses the connection (fatal: no retry) on either
    std::string rdp = source_text("rdp.cpp");
    std::string load = function_body(rdp, "  static BOOL load_channels(");
    CHECK(count(load, "settings_violation(settings_view(instance->context->settings))") == 1 &&
              count(load, "return FALSE;") == 2 && count(load, "freerdp_client_load_channels") == 0 &&
              count(load, "freerdp_client_load_addins") == 0 &&
              load.find("settings_violation") < load.find("load_static(instance->context, DRDYNVC_SVC_CHANNEL_NAME"),
          "the link loads only its own channels, checks them before the transport loads, and refuses the connection");
    CHECK(count(rdp, "freerdp_client_load_channels") == 0 && count(rdp, "freerdp_client_load_addins") == 0,
          "FreeRDP's settings-driven channel loader is never used");
    CHECK(count(load, "{RDPGFX_CHANNEL_NAME, AINPUT_CHANNEL_NAME}") == 1, "the dynamic channels are rdpgfx and ainput");
    std::string att = function_body(rdp, "std::string RdpLink::attempt(");
    CHECK(count(att, "settings_violation(settings_view(ctx->settings))") == 1 && count(att, "*fatal = true;") == 2,
          "and before connecting");
    CHECK(count(rdp, "instance->LoadChannels = load_channels;") == 1, "the link's channel loader is the one FreeRDP uses");
  }

  // ---- POST /scene
  {
    BridgeScene s;
    std::string err;
    std::string key(64, 'a');
    CHECK(bridge_scene_parse("{\"epoch_unix_ms\":1700000000000,\"rect\":{\"x\":0,\"y\":0,\"w\":1,\"h\":1},\"admit_keys\":[\"" +
                                 key + "\"]}",
                             &s, &err) &&
              s.epoch_unix_ms == 1700000000000.0 && s.admit_keys.size() == 1 && s.admit_keys[0].size() == 32,
          "a scene with a key: %s", err.c_str());
    CHECK(!bridge_scene_parse("{}", &s, &err) && err == "epoch_unix_ms: missing", "epoch_unix_ms is required");
    CHECK(!bridge_scene_parse("{\"epoch_unix_ms\":1,\"admit_keys\":[\"zz\"]}", &s, &err), "a key that is no hex is refused");
    std::string five = "[\"" + key + "\",\"" + key + "\",\"" + key + "\",\"" + key + "\",\"" + key + "\"]";
    CHECK(!bridge_scene_parse("{\"epoch_unix_ms\":1,\"admit_keys\":" + five + "}", &s, &err) &&
              err == "admit_keys: more than 4 keys",
          "at most 4 keys");
    CHECK(bridge_scene_parse("{\"epoch_unix_ms\":5}", &s, &err) && s.admit_keys.empty(), "no keys");
  }

  // ---- /health
  {
    BridgeHealth h;
    h.boot_id = "0123456789abcdef0123456789abcdef";
    h.times.start = 1000;
    h.attempts = 3;
    std::string s = bridge_health_json(h);
    Json j = Json::parse(s);
    size_t at = 0;
    bool ordered = true;
    for (const char* k : {"\"fps\":", "\"heights\":", "\"streams\":", "\"gpu\":", "\"t\":", "\"frame\":"}) {
      size_t p = s.find(k);
      ordered = ordered && p != std::string::npos && p >= at;
      at = p;
    }
    CHECK(s.rfind("{\"fps\":", 0) == 0 && ordered, "the renderer's first six keys, in order: %s", s.c_str());
    CHECK(j.at("ready").type == Json::Bool && !j.at("ready").b && !j.has("error"), "connecting: not ready, no error");
    const Json& w = j.at("windows");
    CHECK(w.at("state").str == "connecting" && w.at("connects").num == 0 && w.at("attempts").num == 3, "the windows object");
    const Json& t = w.at("times_unix_ms");
    CHECK(t.at("start").num == 1000, "start");
    for (const char* k : {"connect_started", "tcp", "rdp", "first_frame", "desktop"})
      CHECK(t.at(k).type == Json::Null, "%s is null until it happens", k);
    for (const char* k : {"fps", "encode_ms_p50", "encode_ms_p90", "frame_age_ms"}) CHECK(w.has(k), "windows.%s", k);
    CHECK(w.at("encode_ms_p50").type == Json::Null && w.at("frame_age_ms").type == Json::Null, "null before any frame");
    CHECK(j.at("viewers").type == Json::Array && j.at("heights").arr.empty() && j.at("gpu").type == Json::Null, "no viewers");
    h.state = STATE_DESKTOP;
    h.connects = 1;
    h.times = {1000, 1100, 1105, 1900, 2000, 4100};
    h.surface_w = 1280;
    h.surface_h = 720;
    h.streams = {{540, 1}, {270, 0}};
    h.encode_p50 = 4.25;
    h.encode_p90 = 6.5;
    h.frame_age_ms = 12;
    h.viewers = "[{\"h\":540}]";
    j = Json::parse(bridge_health_json(h));
    const Json& w2 = j.at("windows");
    CHECK(j.at("ready").b && w2.at("state").str == "desktop" && w2.at("times_unix_ms").at("desktop").num == 4100 &&
              w2.at("times_unix_ms").at("tcp").num == 1105 && w2.at("encode_ms_p50").num == 4.25 &&
              w2.at("frame_age_ms").num == 12,
          "a desktop");
    CHECK(j.at("heights").arr.size() == 1 && j.at("heights").arr[0].num == 720 && j.at("streams").arr.size() == 2 &&
              j.at("streams").arr[1].at("q").str == "lo" && j.at("viewers").arr.size() == 1,
          "streams at the desktop's height");
    h.state = STATE_FAILED;
    h.error = "the connection's settings are refused: RedirectClipboard is on";
    j = Json::parse(bridge_health_json(h));
    CHECK(!j.at("ready").b && j.at("error").str == h.error && j.at("windows").at("state").str == "failed", "failed");
    for (BridgeState st : {STATE_CONNECTING, STATE_CONNECTED, STATE_DESKTOP, STATE_FAILED})
      CHECK(std::string(bridge_state_name(st)).find_first_not_of("abcdefghijklmnopqrstuvwxyz") == std::string::npos, "state");
    h.gfx_frames = 7;
    h.gfx_codecs = "{\"avc420\":5}";
    j = Json::parse(bridge_health_json(h));
    CHECK(j.at("windows").at("gfx_frames").num == 7 && j.at("windows").at("paint_frames").num == 0 &&
              j.at("windows").at("gfx_codecs").at("avc420").num == 5,
          "frames by path and codecs");
    CHECK(gfx_codec_name(0xB) == "avc420" && gfx_codec_name(0xE) == "avc444" && gfx_codec_name(0xF) == "avc444v2" &&
              gfx_codec_name(0x8) == "clearcodec" && gfx_codec_name(0x9) == "progressive" && gfx_codec_name(0x77) == "0x0077",
          "codec names");
    Durations d;
    CHECK(d.pct(0.5) == -1, "no samples");
    for (int i = 1; i <= 10; i++) d.add(i);
    CHECK(d.pct(0.5) == 6 && d.pct(0.9) == 9, "percentiles: %f %f", d.pct(0.5), d.pct(0.9));
  }

  // ---- reconnecting
  {
    Backoff b;
    std::vector<int> got;
    for (int i = 0; i < 8; i++) got.push_back(b.next_ms());
    CHECK(got == std::vector<int>({100, 200, 400, 800, 1600, 2000, 2000, 2000}), "100 ms doubling to 2 s");
    b.reset();
    CHECK(b.next_ms() == 100, "a connection that worked starts again at 100 ms");
  }
  {  // the server is set up as the renderer's: the scene token before it listens
    std::string m = source_text("main.cpp");
    size_t conf = m.find("scene_token.configure_from_env();"), set = m.find("s.set_scene_token(&scene_token)");
    size_t listen = m.find("s.listen(port, &err)");
    CHECK(conf != std::string::npos && set != std::string::npos && listen != std::string::npos && conf < set && set < listen,
          "the bridge configures the scene token before it listens");
  }
  DONE();
}
