#include "logic.h"

#include <math.h>
#include <png.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <algorithm>
#include <fstream>
#include <iterator>

#include "admit.h"
#include "json.h"

double unix_ms_now() {
  timespec ts;
  clock_gettime(CLOCK_REALTIME, &ts);
  return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}

double mono_ms_now() {
  timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}

// ---- thumbnails

namespace {
// The area average of cell (cx, cy): source pixels [x0, x1) x [y0, y1), at least one each way.
template <typename Px>
Thumb thumb_of(int w, int h, Px px) {
  Thumb t;
  if (w <= 0 || h <= 0) return t;
  t.rgb.assign((size_t)THUMB_W * THUMB_H * 3, 0.f);
  for (int cy = 0; cy < THUMB_H; cy++) {
    int y0 = (int)((long long)cy * h / THUMB_H), y1 = std::max(y0 + 1, (int)((long long)(cy + 1) * h / THUMB_H));
    for (int cx = 0; cx < THUMB_W; cx++) {
      int x0 = (int)((long long)cx * w / THUMB_W), x1 = std::max(x0 + 1, (int)((long long)(cx + 1) * w / THUMB_W));
      double r = 0, g = 0, b = 0;
      for (int y = y0; y < y1; y++)
        for (int x = x0; x < x1; x++) {
          uint8_t c[3];
          px(x, y, c);
          r += c[0];
          g += c[1];
          b += c[2];
        }
      double n = 255.0 * (x1 - x0) * (y1 - y0);
      float* o = &t.rgb[((size_t)cy * THUMB_W + cx) * 3];
      o[0] = (float)(r / n);
      o[1] = (float)(g / n);
      o[2] = (float)(b / n);
    }
  }
  return t;
}
}  // namespace

Thumb thumb_from_bgrx(const uint8_t* bgrx, int w, int h, size_t stride) {
  return thumb_of(w, h, [&](int x, int y, uint8_t* c) {
    const uint8_t* p = bgrx + (size_t)y * stride + (size_t)x * 4;
    c[0] = p[2];
    c[1] = p[1];
    c[2] = p[0];
  });
}

Thumb thumb_from_rgb(const uint8_t* rgb, int w, int h) {
  return thumb_of(w, h, [&](int x, int y, uint8_t* c) { memcpy(c, rgb + ((size_t)y * w + x) * 3, 3); });
}

double thumb_rmse(const Thumb& a, const Thumb& b) {
  if (a.empty() || b.empty() || a.rgb.size() != b.rgb.size()) return 1.0;
  double s = 0;
  for (size_t i = 0; i < a.rgb.size(); i++) {
    double d = (double)a.rgb[i] - b.rgb[i];
    s += d * d;
  }
  return sqrt(s / a.rgb.size());
}

double thumb_stddev(const Thumb& t) {
  if (t.empty()) return 0;
  size_t n = t.rgb.size() / 3;
  double s = 0, s2 = 0;
  for (size_t i = 0; i < n; i++) {
    double l = 0.2126 * t.rgb[i * 3] + 0.7152 * t.rgb[i * 3 + 1] + 0.0722 * t.rgb[i * 3 + 2];
    s += l;
    s2 += l * l;
  }
  double m = s / n;
  return sqrt(std::max(0.0, s2 / n - m * m));
}

namespace {
bool load_ppm(const std::string& data, std::vector<uint8_t>* rgb, int* w, int* h, std::string* err) {
  size_t p = 2;
  int vals[3];
  for (int i = 0; i < 3; i++) {
    while (p < data.size()) {
      if (isspace((unsigned char)data[p])) p++;
      else if (data[p] == '#') { while (p < data.size() && data[p] != '\n') p++; }
      else break;
    }
    size_t q = p;
    while (q < data.size() && isdigit((unsigned char)data[q])) q++;
    if (q == p || q - p > 6) { *err = "not a binary PPM header"; return false; }
    vals[i] = atoi(data.substr(p, q - p).c_str());
    p = q;
  }
  if (p >= data.size() || !isspace((unsigned char)data[p])) { *err = "not a binary PPM header"; return false; }
  p++;
  if (vals[0] <= 0 || vals[1] <= 0 || vals[0] > 16384 || vals[1] > 16384 || vals[2] != 255) {
    *err = "PPM: width and height 1..16384 and maxval 255 only";
    return false;
  }
  size_t need = (size_t)vals[0] * vals[1] * 3;
  if (data.size() - p < need) { *err = "PPM: short pixel data"; return false; }
  rgb->assign(data.begin() + p, data.begin() + p + need);
  *w = vals[0];
  *h = vals[1];
  return true;
}

bool load_png(const std::string& path, std::vector<uint8_t>* rgb, int* w, int* h, std::string* err) {
  png_image im;
  memset(&im, 0, sizeof im);
  im.version = PNG_IMAGE_VERSION;
  if (!png_image_begin_read_from_file(&im, path.c_str())) {
    *err = std::string("PNG: ") + im.message;
    return false;
  }
  if (im.width == 0 || im.height == 0 || im.width > 16384 || im.height > 16384) {
    png_image_free(&im);
    *err = "PNG: width and height 1..16384 only";
    return false;
  }
  im.format = PNG_FORMAT_RGB;
  rgb->assign(PNG_IMAGE_SIZE(im), 0);
  if (!png_image_finish_read(&im, nullptr, rgb->data(), 0, nullptr)) {
    *err = std::string("PNG: ") + im.message;
    png_image_free(&im);
    return false;
  }
  *w = (int)im.width;
  *h = (int)im.height;
  return true;
}
}  // namespace

bool load_image_rgb(const std::string& path, std::vector<uint8_t>* rgb, int* w, int* h, std::string* err) {
  std::ifstream f(path, std::ios::binary);
  if (!f) { *err = path + ": cannot be read"; return false; }
  std::string data((std::istreambuf_iterator<char>(f)), std::istreambuf_iterator<char>());
  if (data.size() >= 8 && memcmp(data.data(), "\x89PNG\r\n\x1a\n", 8) == 0) return load_png(path, rgb, w, h, err);
  if (data.size() >= 2 && data[0] == 'P' && data[1] == '6') return load_ppm(data, rgb, w, h, err);
  *err = path + ": neither a PNG nor a binary PPM";
  return false;
}

bool load_thumb(const std::string& path, Thumb* out, std::string* err) {
  std::vector<uint8_t> rgb;
  int w = 0, h = 0;
  if (!load_image_rgb(path, &rgb, &w, &h, err)) return false;
  *out = thumb_from_rgb(rgb.data(), w, h);
  return true;
}

bool write_thumb_ppm(const std::string& path, const Thumb& t, std::string* err) {
  if (t.empty()) { *err = "no thumbnail"; return false; }
  std::string tmp = path + ".tmp." + std::to_string(getpid());
  FILE* f = fopen(tmp.c_str(), "wb");
  if (!f) { *err = tmp + ": " + strerror(errno); return false; }
  fprintf(f, "P6\n%d %d\n255\n", THUMB_W, THUMB_H);
  std::vector<uint8_t> px(t.rgb.size());
  for (size_t i = 0; i < px.size(); i++) px[i] = (uint8_t)lround(std::min(1.f, std::max(0.f, t.rgb[i])) * 255);
  bool ok = fwrite(px.data(), 1, px.size(), f) == px.size();
  ok = fclose(f) == 0 && ok;
  if (!ok || rename(tmp.c_str(), path.c_str()) != 0) {
    *err = path + ": " + strerror(errno);
    unlink(tmp.c_str());
    return false;
  }
  return true;
}

void DesktopDetector::frame(double t_ms, const Thumb& th) {
  if (found_) return;
  if (!ref_.empty()) {
    if (thumb_rmse(th, ref_) <= DESKTOP_RMSE) {
      found_ = true;
      desktop_ms_ = t_ms;
      desktop_ = th;
    }
    return;
  }
  // a frame that settling would end on is checked before it can move the candidate
  tick(t_ms);
  if (found_) return;
  if (!have_candidate_ || thumb_rmse(th, candidate_) > SETTLE_RMSE) {
    have_candidate_ = true;
    candidate_ms_ = t_ms;
    candidate_ = th;
  }
}

void DesktopDetector::tick(double now_ms) {
  if (found_ || !ref_.empty() || !have_candidate_) return;
  if (now_ms - candidate_ms_ >= settle_ms_ && thumb_stddev(candidate_) >= SETTLE_MIN_STDDEV) {
    found_ = true;
    desktop_ms_ = candidate_ms_;
    desktop_ = candidate_;
  }
}

// ---- pictures

namespace {
inline uint8_t clamp8(int v) { return (uint8_t)(v < 0 ? 0 : v > 255 ? 255 : v); }
}  // namespace

void bgrx_to_i420(const uint8_t* src, size_t stride, int w, int h, uint8_t* y, uint8_t* u, uint8_t* v) {
  // the coefficients of fleet/phone/streamer/nv12.cpp, scaled by 2^16
  for (int r = 0; r < h; r++) {
    const uint8_t* s = src + (size_t)r * stride;
    uint8_t* d = y + (size_t)r * w;
    for (int x = 0; x < w; x++, s += 4)
      d[x] = clamp8((11966 * s[2] + 40254 * s[1] + 4064 * s[0] + (16 << 16) + 32768) >> 16);
  }
  for (int r = 0; r < h; r += 2) {
    const uint8_t* a = src + (size_t)r * stride;
    const uint8_t* b = src + (size_t)(r + 1) * stride;
    uint8_t* du = u + (size_t)(r / 2) * (w / 2);
    uint8_t* dv = v + (size_t)(r / 2) * (w / 2);
    for (int x = 0; x < w; x += 2) {
      int bl = a[x * 4] + a[x * 4 + 4] + b[x * 4] + b[x * 4 + 4];
      int g = a[x * 4 + 1] + a[x * 4 + 5] + b[x * 4 + 1] + b[x * 4 + 5];
      int rd = a[x * 4 + 2] + a[x * 4 + 6] + b[x * 4 + 2] + b[x * 4 + 6];
      du[x / 2] = clamp8(((-6591 * rd - 22189 * g + 28780 * bl) / 4 + (128 << 16) + 32768) >> 16);
      dv[x / 2] = clamp8(((28780 * rd - 26142 * g - 2638 * bl) / 4 + (128 << 16) + 32768) >> 16);
    }
  }
}

void bgrx_to_rgba(const uint8_t* src, size_t stride, int w, int h, uint8_t* rgba) {
  for (int r = 0; r < h; r++) {
    const uint8_t* s = src + (size_t)r * stride;
    uint8_t* d = rgba + (size_t)r * w * 4;
    for (int x = 0; x < w; x++, s += 4, d += 4) {
      d[0] = s[2];
      d[1] = s[1];
      d[2] = s[0];
      d[3] = 255;
    }
  }
}

// ---- reconnecting

int Backoff::next_ms() {
  int n = next_;
  next_ = std::min(2000, next_ * 2);
  return n;
}

// ---- the fixed actions

namespace {
constexpr Step chord1(uint16_t a) { return Step{STEP_CHORD, {a, 0, 0}, 1, nullptr, 0}; }
constexpr Step chord2(uint16_t a, uint16_t b) { return Step{STEP_CHORD, {a, b, 0}, 2, nullptr, 0}; }
constexpr Step chord3(uint16_t a, uint16_t b, uint16_t c) { return Step{STEP_CHORD, {a, b, c}, 3, nullptr, 0}; }
constexpr Step text(const char* t) { return Step{STEP_TEXT, {0, 0, 0}, 0, t, 0}; }
constexpr Step wait(int ms) { return Step{STEP_WAIT, {0, 0, 0}, 0, nullptr, ms}; }
constexpr Step wait_change(int ms) { return Step{STEP_WAIT_CHANGE, {0, 0, 0}, 0, nullptr, ms}; }

// The Run dialog takes a moment to open after Win+R; what is typed before it has focus goes
// to whatever had it.
constexpr int RUN_DIALOG_MS = 800;

constexpr Step kStartMenu[] = {chord1(SC_LWIN)};
constexpr Step kNotepad[] = {chord2(SC_LWIN, SC_KEY_R), wait(RUN_DIALOG_MS), text("notepad"), chord1(SC_RETURN),
                             wait_change(5000), wait(500), text("fcvm Windows sandbox")};
constexpr Step kExplorer[] = {chord2(SC_LWIN, SC_KEY_E)};
constexpr Step kTaskManager[] = {chord3(SC_LCONTROL, SC_LSHIFT, SC_ESCAPE)};
constexpr Step kAbout[] = {chord2(SC_LWIN, SC_KEY_R), wait(RUN_DIALOG_MS), text("winver"), chord1(SC_RETURN)};
constexpr Step kTerminal[] = {
    chord2(SC_LWIN, SC_KEY_R), wait(RUN_DIALOG_MS),
    text("powershell -NoExit -Command Get-ComputerInfo -Property OsName,OsVersion,CsProcessors,CsTotalPhysicalMemory"),
    chord1(SC_RETURN)};
constexpr Step kCloseAll[] = {Step{STEP_CLOSE_UNTIL_DESKTOP, {0, 0, 0}, 0, nullptr, CLOSE_ALL_TRIES}};

template <size_t N>
constexpr Action act(const char* name, const Step (&s)[N]) {
  return Action{name, s, (int)N};
}
}  // namespace

const Action kActions[] = {
    act("start_menu", kStartMenu), act("notepad", kNotepad), act("explorer", kExplorer),
    act("task_manager", kTaskManager), act("about", kAbout), act("terminal", kTerminal),
    act("close_all", kCloseAll),
};
const int kActionCount = (int)(sizeof kActions / sizeof kActions[0]);

const Action* action_find(const std::string& name) {
  for (int i = 0; i < kActionCount; i++)
    if (name == kActions[i].name) return &kActions[i];
  return nullptr;
}

std::string action_names() {
  std::string s;
  for (int i = 0; i < kActionCount; i++) s += (i ? ", " : "") + std::string(kActions[i].name);
  return s;
}

std::string action_refusal(const std::string& name) {
  return "unknown action \"" + name.substr(0, 64) + "\": the actions are " + action_names();
}

const Action* action_from_body(const std::string& body, std::string* err) {
  Json j;
  try {
    j = Json::parse(body);
  } catch (const std::exception& e) {
    *err = std::string("body: ") + e.what();
    return nullptr;
  }
  if (j.type != Json::Object || !j.has("name") || j.at("name").type != Json::String) {
    *err = "body: {\"name\": \"<action>\"} expected; the actions are " + action_names();
    return nullptr;
  }
  const Action* a = action_find(j.at("name").str);
  if (!a) *err = action_refusal(j.at("name").str);
  return a;
}

namespace {
// US layout: the scancode of each character, and whether it needs Shift.
bool us_key(char c, uint16_t* sc, bool* shift) {
  static const char* rows[] = {"qwertyuiop", "asdfghjkl", "zxcvbnm"};
  static const uint16_t starts[] = {0x10, 0x1E, 0x2C};
  *shift = false;
  char l = c;
  if (c >= 'A' && c <= 'Z') {
    *shift = true;
    l = (char)(c - 'A' + 'a');
  }
  for (int r = 0; r < 3; r++)
    if (const char* p = strchr(rows[r], l); p && l) {
      *sc = (uint16_t)(starts[r] + (p - rows[r]));
      return true;
    }
  if (c >= '1' && c <= '9') { *sc = (uint16_t)(0x02 + (c - '1')); return true; }
  switch (c) {
    case '0': *sc = 0x0B; return true;
    case '-': *sc = 0x0C; return true;
    case ',': *sc = 0x33; return true;
    case '.': *sc = 0x34; return true;
    case ' ': *sc = SC_SPACE; return true;
    default: return false;
  }
}
}  // namespace

bool step_keys(const Step& s, std::vector<KeyEvent>* out) {
  if (s.kind == STEP_CHORD) {
    for (int i = 0; i < s.nkeys; i++) out->push_back({s.keys[i], true});
    for (int i = s.nkeys - 1; i >= 0; i--) out->push_back({s.keys[i], false});
    return true;
  }
  if (s.kind != STEP_TEXT || !s.text) return false;
  for (const char* p = s.text; *p; p++) {
    uint16_t sc;
    bool shift;
    if (!us_key(*p, &sc, &shift)) return false;
    if (shift) out->push_back({SC_LSHIFT, true});
    out->push_back({sc, true});
    out->push_back({sc, false});
    if (shift) out->push_back({SC_LSHIFT, false});
  }
  return true;
}

const Step kCloseWindowStep = chord2(SC_LMENU, SC_F4);

// ---- POST /scene

bool bridge_scene_parse(const std::string& body, BridgeScene* out, std::string* err) {
  Json j;
  try {
    j = Json::parse(body);
  } catch (const std::exception& e) {
    *err = std::string("body: ") + e.what();
    return false;
  }
  if (j.type != Json::Object) {
    *err = "body: not an object";
    return false;
  }
  BridgeScene s;
  try {
    if (!j.has("epoch_unix_ms")) {
      *err = "epoch_unix_ms: missing";
      return false;
    }
    s.epoch_unix_ms = j.at("epoch_unix_ms").number("epoch_unix_ms");
    if (j.has("admit_keys") && j.at("admit_keys").type != Json::Null) {
      const auto& ks = j.at("admit_keys").array("admit_keys");
      if (ks.size() > 4) {
        *err = "admit_keys: more than 4 keys";
        return false;
      }
      for (const Json& k : ks) s.admit_keys.push_back(admit_key_from_hex(k.string("admit_keys[]")));
    }
  } catch (const std::exception& e) {
    *err = e.what();
    return false;
  }
  *out = s;
  return true;
}

// ---- the channel check

const char* const kForbiddenSettings[] = {
    "RedirectClipboard", "RedirectDrives", "RedirectHomeDrive", "RedirectPrinters", "RedirectSmartCards",
    "RedirectSerialPorts", "RedirectParallelPorts", "RedirectWebAuthN", "DeviceRedirection", "AudioPlayback",
    "AudioCapture", "RemoteApplicationMode", "RemoteAssistanceMode", "EncomspVirtualChannel",
    "RemdeskVirtualChannel", "SupportSSHAgentChannel", "LyncRdpMode", "RemoteCredentialGuard",
    "SupportEchoChannel", "SupportVideoOptimized", "SupportGeometryTracking",
};
const int kForbiddenSettingCount = (int)(sizeof kForbiddenSettings / sizeof kForbiddenSettings[0]);
const char* const kRequiredSettings[] = {"TlsSecurity", "SupportGraphicsPipeline", "GfxH264", "GfxAVC444", "GfxAVC444v2"};
const int kRequiredSettingCount = (int)(sizeof kRequiredSettings / sizeof kRequiredSettings[0]);
const char* const kOffSettings[] = {"NlaSecurity", "RdpSecurity", "ExtSecurity", "RdstlsSecurity", "AadSecurity",
                                    "SupportMultitransport", "GatewayEnabled", "AutoReconnectionEnabled"};
const int kOffSettingCount = (int)(sizeof kOffSettings / sizeof kOffSettings[0]);
const char* const kAllowedChannels[] = {"rdpgfx", "drdynvc", "ainput"};
const int kAllowedChannelCount = (int)(sizeof kAllowedChannels / sizeof kAllowedChannels[0]);

std::string settings_violation(const RdpSettingsView& v) {
  std::string out;
  auto add = [&](const std::string& s) { out += (out.empty() ? "" : "; ") + s; };
  auto get = [&](const char* name, bool* val) {
    for (const auto& kv : v.bools)
      if (kv.first == name) { *val = kv.second; return true; }
    return false;
  };
  auto want = [&](const char* const* names, int n, bool on) {
    for (int i = 0; i < n; i++) {
      bool val = false;
      if (!get(names[i], &val)) add(std::string(names[i]) + " not read");
      else if (val != on) add(std::string(names[i]) + (on ? " is off" : " is on"));
    }
  };
  want(kForbiddenSettings, kForbiddenSettingCount, false);
  want(kOffSettings, kOffSettingCount, false);
  want(kRequiredSettings, kRequiredSettingCount, true);
  auto allowed = [](const std::string& c) {
    for (int i = 0; i < kAllowedChannelCount; i++)
      if (c == kAllowedChannels[i]) return true;
    return false;
  };
  for (const auto& c : v.static_channels)
    if (!allowed(c)) add("static channel " + c);
  for (const auto& c : v.dynamic_channels)
    if (!allowed(c)) add("dynamic channel " + c);
  if (v.devices) add(std::to_string(v.devices) + " redirected devices");
  if (!v.proxy_ignored) add("a proxy may be used");
  return out;
}

// ---- the stream

uint32_t quality_rate_bps(int q, int w, int h, int fps) {
  const char* bpp_env = getenv("ENC_BPP");
  double bpp = q == 0 ? 0.25 : q == 2 ? 4.0 : (bpp_env && atof(bpp_env) > 0 ? atof(bpp_env) : 0.8);
  double floor_bps = q == 0 ? 150000.0 : q == 2 ? 2000000.0 : 600000.0;
  double cap_bps = q == 2 ? 160000000.0 : 20000000.0;
  return (uint32_t)std::max(floor_bps, std::min(cap_bps, (double)w * h * fps * bpp));
}

float quality_crf(int q) { return q == 0 ? 28.f : q == 2 ? 16.f : 21.f; }

std::string bridge_header_json(int w, int h, const std::string& codec, int fps, const char* q, uint32_t bitrate) {
  char b[512];
  snprintf(b, sizeof b,
           "{\"codec\":\"%s\",\"width\":%d,\"height\":%d,\"full_width\":%d,\"full_height\":%d,\"x0\":0,\"y0\":0,\"fps\":%d,"
           "\"crop\":{\"left\":0,\"top\":0,\"right\":0,\"bottom\":0},\"q\":\"%s\",\"bitrate\":%u,\"layout_generation\":null}",
           codec.c_str(), w, h, w, h, fps, q, bitrate);
  return b;
}

long long frame_slot(double unix_ms, double epoch_unix_ms, int fps) {
  return (long long)floor((unix_ms - epoch_unix_ms) * fps / 1000.0);
}

double frame_slot_t(double unix_ms, double epoch_unix_ms, int fps) {
  return (double)frame_slot(unix_ms, epoch_unix_ms, fps) / fps;
}

// ---- /health

std::string gfx_codec_name(uint32_t id) {
  switch (id) {
    case 0x0: return "uncompressed";
    case 0x3: return "remotefx";
    case 0x8: return "clearcodec";
    case 0x9: return "progressive";
    case 0xA: return "planar";
    case 0xB: return "avc420";
    case 0xC: return "alpha";
    case 0xD: return "progressive_v2";
    case 0xE: return "avc444";
    case 0xF: return "avc444v2";
    default: {
      char b[16];
      snprintf(b, sizeof b, "0x%04X", id & 0xFFFF);
      return b;
    }
  }
}

const char* bridge_state_name(BridgeState s) {
  switch (s) {
    case STATE_CONNECTED: return "connected";
    case STATE_DESKTOP: return "desktop";
    case STATE_FAILED: return "failed";
    default: return "connecting";
  }
}

void Durations::add(double ms) {
  v_.push_back(ms);
  if (v_.size() > 300) v_.pop_front();
}

double Durations::pct(double p) const {
  if (v_.empty()) return -1;
  std::vector<double> s(v_.begin(), v_.end());
  std::sort(s.begin(), s.end());
  return s[std::min(s.size() - 1, (size_t)(p * (s.size() - 1) + 0.5))];
}

namespace {
std::string num(double v, const char* fmt = "%.0f") {
  if (v < 0) return "null";
  char b[64];
  snprintf(b, sizeof b, fmt, v);
  return b;
}
std::string str_or_null(const std::string& s) { return s.empty() ? "null" : "\"" + json_escape(s.substr(0, 300)) + "\""; }
}  // namespace

std::string bridge_health_json(const BridgeHealth& h) {
  std::string streams = "[";
  for (size_t i = 0; i < h.streams.size(); i++) {
    int q = h.streams[i].second;
    streams += (i ? "," : "") + std::string("{\"h\":") + std::to_string(h.streams[i].first) + ",\"q\":\"" +
               (q == 0 ? "lo" : q == 2 ? "hi" : "std") + "\"}";
  }
  streams += "]";
  std::string heights = h.streams.empty() || !h.surface_h ? "[]" : "[" + std::to_string(h.surface_h) + "]";
  const WindowsTimes& t = h.times;
  bool ready = h.state == STATE_CONNECTED || h.state == STATE_DESKTOP;
  char b[8192];
  snprintf(b, sizeof b,
           "{\"fps\":%.1f,\"heights\":%s,\"streams\":%s,\"gpu\":null,\"t\":%.6f,\"frame\":%u,"
           "\"boot_id\":\"%s\",\"ready\":%s,%s\"startup_ms\":{\"first_frame_sent\":%s},"
           "\"windows\":{\"state\":\"%s\",\"connects\":%llu,\"attempts\":%llu,"
           "\"times_unix_ms\":{\"start\":%s,\"connect_started\":%s,\"tcp\":%s,\"rdp\":%s,\"first_frame\":%s,\"desktop\":%s},"
           "\"desktop_by\":\"%s\",\"surface\":[%d,%d],\"fps\":%.1f,\"encode_ms_p50\":%s,\"encode_ms_p90\":%s,"
           "\"frame_age_ms\":%s,\"last_error\":%s,\"h264\":%s,\"gfx_frames\":%llu,\"paint_frames\":%llu,"
           "\"gfx_codecs\":%s}",
           h.fps, heights.c_str(), streams.c_str(), h.t, h.frame, h.boot_id.c_str(), ready ? "true" : "false",
           h.error.empty() ? "" : ("\"error\":\"" + json_escape(h.error.substr(0, 300)) + "\",").c_str(), num(h.first_frame_sent_ms).c_str(),
           bridge_state_name(h.state), (unsigned long long)h.connects, (unsigned long long)h.attempts, num(t.start).c_str(),
           num(t.connect_started).c_str(), num(t.tcp).c_str(), num(t.rdp).c_str(), num(t.first_frame).c_str(),
           num(t.desktop).c_str(), h.desktop_by, h.surface_w, h.surface_h, h.fps, num(h.encode_p50, "%.2f").c_str(),
           num(h.encode_p90, "%.2f").c_str(), num(h.frame_age_ms).c_str(), str_or_null(h.last_error).c_str(),
           h.h264 ? "true" : "false", (unsigned long long)h.gfx_frames, (unsigned long long)h.paint_frames,
           h.gfx_codecs.c_str());
  return std::string(b) + ",\"viewers\":" + h.viewers + "}";
}
