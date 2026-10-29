#include "logic.h"

#include <math.h>
#include <stdio.h>

#include <algorithm>
#include <climits>
#include <vector>

#include "admit.h"
#include "json.h"

namespace {
bool read_rect(const Json& r, const char* name, Rect4* out, std::string* err) {
  std::string n = name;
  out->x = r.at("x").number(n + ".x");
  out->y = r.at("y").number(n + ".y");
  out->w = r.at("w").number(n + ".w");
  out->h = r.at("h").number(n + ".h");
  if (!(out->w >= 1.0 / 256 && out->h >= 1.0 / 256 && out->x >= 0 && out->y >= 0 && out->x + out->w <= 1 + 1e-9 &&
        out->y + out->h <= 1 + 1e-9)) {
    *err = n + ": must lie inside the unit square, w and h at least 1/256";
    return false;
  }
  return true;
}
bool frame_no(const Json& j, const char* k, long long* out, std::string* err) {
  double g = j.at(k).number(k);
  if (!(g >= 0 && g == floor(g))) {
    *err = std::string(k) + ": must be a non-negative integer";
    return false;
  }
  *out = (long long)g;
  return true;
}
}  // namespace

bool phone_scene_parse(const std::string& body, PhoneScene* out, std::string* err) {
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
  PhoneScene s;
  try {
    if (!j.has("epoch_unix_ms")) {
      *err = "epoch_unix_ms: missing";
      return false;
    }
    s.epoch_unix_ms = j.at("epoch_unix_ms").number("epoch_unix_ms");
    if (j.has("rect") && j.at("rect").type != Json::Null && !read_rect(j.at("rect"), "rect", &s.rect, err)) return false;
    if (j.has("image_aspect") && j.at("image_aspect").type != Json::Null) {
      s.image_aspect = j.at("image_aspect").number("image_aspect");
      if (!(s.image_aspect >= 0.1 && s.image_aspect <= 10)) {
        *err = "image_aspect: must be 0.1 to 10";
        return false;
      }
    }
    auto present = [&](const char* k) { return j.has(k) && j.at(k).type != Json::Null; };
    if (present("layout_generation") && !frame_no(j, "layout_generation", &s.layout_generation, err)) return false;
    if (present("rect_from_frame") && !frame_no(j, "rect_from_frame", &s.rect_from_frame, err)) return false;
    if (present("admit_keys")) {
      const auto& ks = j.at("admit_keys").array("admit_keys");
      if (ks.size() > 4) {
        *err = "admit_keys: more than 4 keys";
        return false;
      }
      for (const Json& k : ks) s.admit_keys.push_back(admit_key_from_hex(k.string("admit_keys[]")));
    }
    if (present("previous_rect")) {
      if (s.rect_from_frame < 0) {
        *err = "previous_rect: needs rect_from_frame";
        return false;
      }
      if (!read_rect(j.at("previous_rect"), "previous_rect", &s.previous, err)) return false;
      s.has_previous = true;
    }
  } catch (const std::exception& e) {
    *err = e.what();
    return false;
  }
  s.set = true;
  s.body = body;
  *out = s;
  return true;
}

namespace {
void dump(const Json& j, std::string* out) {
  char b[40];
  switch (j.type) {
    case Json::Null: *out += "null"; break;
    case Json::Bool: *out += j.b ? "true" : "false"; break;
    case Json::Number: snprintf(b, sizeof b, "%.17g", j.num); *out += b; break;
    case Json::String: *out += "\"" + json_escape(j.str) + "\""; break;
    case Json::Array:
      *out += "[";
      for (size_t i = 0; i < j.arr.size(); i++) {
        if (i) *out += ",";
        dump(j.arr[i], out);
      }
      *out += "]";
      break;
    case Json::Object: {
      *out += "{";
      bool first = true;
      for (const auto& kv : j.obj) {
        if (!first) *out += ",";
        first = false;
        *out += "\"" + json_escape(kv.first) + "\":";
        dump(kv.second, out);
      }
      *out += "}";
      break;
    }
  }
}
}  // namespace

std::string app_scene_line(const PhoneScene& s) {
  // the scene as posted, with the request's type and the stamp asked for, and without the
  // admission keys, which are the streamer's and not the app's
  Json j = Json::parse(s.body);
  j.obj.erase("admit_keys");
  j.obj.erase("type");
  j.obj.erase("stamp");
  std::string rest;
  dump(j, &rest);
  return "{\"type\":\"scene\",\"stamp\":true" + std::string(rest.size() > 2 ? "," : "") + rest.substr(1) + "\n";
}

void AdmitKeys::on_scene(const PhoneScene& s) {
  keys = s.admit_keys.empty() ? std::vector<std::string>{} : admit_keys_after(keys, s.admit_keys);
}

bool AdmitKeys::admits(const std::string& token, int64_t now_s) const {
  return keys.empty() || admit_token_valid(keys, token, now_s);
}

std::string app_clock_line(double unix_ms) {
  char b[64];
  snprintf(b, sizeof b, "{\"type\":\"clock\",\"unix_ms\":%.0f}\n", unix_ms);
  return b;
}

std::string app_reply_error(const std::string& line) {
  Json j;
  try {
    j = Json::parse(line);
  } catch (const std::exception& e) {
    return std::string("not JSON: ") + line.substr(0, 200);
  }
  if (j.type != Json::Object) return "not a JSON object: " + line.substr(0, 200);
  if (!j.has("error")) return "";
  const Json& e = j.at("error");
  return e.type == Json::String ? e.str : line.substr(0, 300);
}

bool stamp_plausible(uint32_t k, double capture_unix_ms, const PhoneScene& sc) {
  if (!sc.set) return false;
  double t = (double)k / STAMP_FPS;
  double now = (capture_unix_ms - sc.epoch_unix_ms) / 1000.0;
  return fabs(t - now) <= STAMP_PLAUSIBLE_S;
}

PhoneGeo phone_geometry(bool scene, uint32_t k, int screen_w, int screen_h, const PhoneScene& sc) {
  PhoneGeo g;
  g.scene = scene;
  g.w = screen_w & ~1;
  g.h = screen_h & ~1;
  if (!scene || !sc.set) {
    g.full_w = g.w;
    g.full_h = g.h;
    return g;
  }
  bool before = sc.rect_from_frame >= 0 && (long long)(k / 2) < sc.rect_from_frame;
  Rect4 r = before && sc.has_previous ? sc.previous : sc.rect;
  g.crop_bottom = STAMP_ROWS;
  g.layout_generation = sc.layout_generation;
  // The app's own choice (SceneInput.viewAt in fleet/phone-app): with no image_aspect, or a
  // rect whose orientation is not the surface's (a turned phone), it draws the whole image
  // at the surface's aspect.
  double surface = (double)screen_w / screen_h;
  double aspect = sc.image_aspect;
  if (aspect <= 0 || ((r.w * aspect / r.h) > 1) != (surface > 1)) {
    r = Rect4{0, 0, 1, 1};
    aspect = surface;
  }
  g.full_h = (int)lround(screen_h / r.h);
  g.full_w = (int)lround(g.full_h * aspect);
  g.x0 = (int)lround(r.x * g.full_w);
  g.y0 = (int)lround(r.y * g.full_h);
  return g;
}

std::string phone_header_json(const PhoneGeo& g, const std::string& codec, int fps, const char* q, uint32_t bitrate) {
  char b[512];
  snprintf(b, sizeof b,
           "{\"codec\":\"%s\",\"width\":%d,\"height\":%d,\"full_width\":%d,\"full_height\":%d,\"x0\":%d,\"y0\":%d,\"fps\":%d,"
           "\"crop\":{\"left\":0,\"top\":0,\"right\":0,\"bottom\":%d},\"q\":\"%s\",\"bitrate\":%u,\"layout_generation\":%s,"
           "\"phone\":{\"picture\":\"%s\"}}",
           codec.c_str(), g.w, g.h, g.full_w, g.full_h, g.x0, g.y0, fps, g.crop_bottom, q, bitrate,
           g.layout_generation >= 0 ? std::to_string(g.layout_generation).c_str() : "null", g.scene ? "scene" : "screen");
  return b;
}

bool FrameGate::admit(bool is_scene, uint32_t k, double now_ms, const PhoneScene& sc, double* t) {
  if (is_scene) {
    if (have_k_ && k == last_k_) {
      repeats++;
      return false;
    }
    have_k_ = true;
    last_k_ = k;
    last_screen_slot_ = LLONG_MIN;
    if (k % 2) {  // the wall draws every 1/30 s; the app's odd frames are between
      odd++;
      return false;
    }
    scene++;
    *t = (double)k / STAMP_FPS;
    return true;
  }
  have_k_ = false;
  if (sc.epoch_unix_ms != screen_epoch_) {  // a new scene clock: slots restart
    screen_epoch_ = sc.epoch_unix_ms;
    last_screen_slot_ = LLONG_MIN;
  }
  long long slot = (long long)floor((now_ms - sc.epoch_unix_ms) * fps_ / 1000.0);
  if (slot <= last_screen_slot_) {
    repeats++;
    return false;
  }
  last_screen_slot_ = slot;
  screen++;
  *t = (double)slot / fps_;
  return true;
}

void Samples::add(double ms) {
  v_.push_back(ms);
  if (v_.size() > 300) v_.pop_front();
}

std::string Samples::json() const {
  if (v_.empty()) return "null";
  std::vector<double> s(v_.begin(), v_.end());
  std::sort(s.begin(), s.end());
  auto pct = [&](double p) { return s[std::min(s.size() - 1, (size_t)(p * (s.size() - 1) + 0.5))]; };
  char b[128];
  snprintf(b, sizeof b, "{\"p50\":%.1f,\"p90\":%.1f,\"n\":%zu}", pct(0.5), pct(0.9), s.size());
  return b;
}
