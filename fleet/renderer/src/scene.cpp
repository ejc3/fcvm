#include "scene.h"

#include <algorithm>
#include <cmath>
#include <cstring>
#include <ctime>
#include <stdexcept>

#include "font.h"
#include "json.h"
#include "world.h"

namespace {
void vec3(const Json& j, double out[3], const std::string& what) {
  const auto& a = j.array(what);
  if (a.size() != 3) throw std::runtime_error("'" + what + "' must have 3 numbers");
  for (int i = 0; i < 3; i++) out[i] = a[i].number(what);
}

std::string clean_text(const std::string& in) {
  std::string out;
  for (unsigned char c : in) {
    if (c >= 'a' && c <= 'z') c = (unsigned char)(c - 'a' + 'A');
    if (c >= 0x80) continue;  // the font is ASCII
    if (!font_has(c)) c = '?';
    if (out.size() < MAX_CHARS) out += (char)c;
  }
  // trim spaces
  size_t a = out.find_first_not_of(' '), b = out.find_last_not_of(' ');
  if (a == std::string::npos) return "";
  return out.substr(a, b - a + 1);
}

uint64_t mix64(uint64_t h, uint64_t v) {
  h ^= v + 0x9E3779B97F4A7C15ull + (h << 6) + (h >> 2);
  h *= 0xBF58476D1CE4E5B9ull;
  return h ^ (h >> 31);
}
uint64_t dbits(double d) {
  uint64_t u;
  memcpy(&u, &d, 8);
  return u;
}

bool same_pose(const Keyframe& a, const Keyframe& b) {
  for (int i = 0; i < 3; i++)
    if (a.pos[i] != b.pos[i] || a.look[i] != b.look[i]) return false;
  return a.fov_deg == b.fov_deg;
}

V3 v3d(const double p[3]) { return v3((float)p[0], (float)p[1], (float)p[2]); }
}  // namespace

Scene parse_scene(const std::string& body) {
  Json j = Json::parse(body);
  if (j.type != Json::Object) throw std::runtime_error("scene must be a JSON object");
  Scene s;
  double ep = j.at("epoch_unix_ms").number("epoch_unix_ms");
  if (ep != std::floor(ep) || std::fabs(ep) > 9.0e15) throw std::runtime_error("'epoch_unix_ms' must be an integer");
  s.epoch_unix_ms = (int64_t)ep;

  auto read_rect = [&](const Json& r, const std::string& name) {
    Rect x;
    x.x = r.at("x").number(name + ".x");
    x.y = r.at("y").number(name + ".y");
    x.w = r.at("w").number(name + ".w");
    x.h = r.at("h").number(name + ".h");
    if (!(x.w > 0 && x.h > 0 && x.x >= 0 && x.y >= 0 && x.x + x.w <= 1.0 + 1e-9 && x.y + x.h <= 1.0 + 1e-9))
      throw std::runtime_error("'" + name + "' must lie inside the unit square with positive w and h");
    if (x.w < 1.0 / 256 || x.h < 1.0 / 256) throw std::runtime_error("'" + name + "' is smaller than 1/256 of the image");
    return x;
  };
  s.rect = read_rect(j.at("rect"), "rect");
  // layout generations (the coordinator's re-tiling): `previous_rect` for
  // frames before `rect_from_frame`, `rect` from it
  auto frame_no = [&](const char* k) {
    double v = j.at(k).number(k);
    if (!(v >= 0 && v == std::floor(v) && v < 9.0e15)) throw std::runtime_error(std::string("'") + k + "' must be a non-negative integer");
    return (long long)v;
  };
  if (j.has("layout_generation") && j.at("layout_generation").type != Json::Null) s.layout_generation = frame_no("layout_generation");
  if (j.has("rect_from_frame") && j.at("rect_from_frame").type != Json::Null) s.rect_from_frame = frame_no("rect_from_frame");
  if (j.has("previous_rect") && j.at("previous_rect").type != Json::Null) {
    s.previous_rect = read_rect(j.at("previous_rect"), "previous_rect");
    s.has_previous_rect = true;
    if (s.rect_from_frame < 0) throw std::runtime_error("'previous_rect' needs 'rect_from_frame'");
  }

  s.image_aspect = j.at("image_aspect").number("image_aspect");
  if (!(s.image_aspect >= 0.1 && s.image_aspect <= 10)) throw std::runtime_error("'image_aspect' must be in 0.1..10");

  const auto& cams = j.at("camera").array("camera");
  if (cams.empty()) throw std::runtime_error("'camera' needs at least one keyframe");
  if (cams.size() > 4096) throw std::runtime_error("'camera' has more than 4096 keyframes");
  for (const Json& c : cams) {
    Keyframe k;
    k.t = c.at("t").number("camera[].t");
    vec3(c.at("pos"), k.pos, "camera[].pos");
    vec3(c.at("look"), k.look, "camera[].look");
    k.fov_deg = c.at("fov_deg").number("camera[].fov_deg");
    if (!(k.fov_deg > 1 && k.fov_deg < 170)) throw std::runtime_error("'fov_deg' must be in 1..170");
    s.camera.push_back(k);
  }
  std::stable_sort(s.camera.begin(), s.camera.end(), [](const Keyframe& a, const Keyframe& b) { return a.t < b.t; });

  s.time_of_day = j.at("time_of_day").number("time_of_day");
  if (!(s.time_of_day >= 0 && s.time_of_day <= 1)) throw std::runtime_error("'time_of_day' must be in 0..1");

  if (j.has("words")) {
    for (const Json& w : j.at("words").array("words")) {
      SceneWord sw;
      sw.text = clean_text(w.at("text").string("words[].text"));
      sw.t = w.at("t").number("words[].t");
      if (!sw.text.empty()) s.words.push_back(sw);
    }
  }
  if (j.has("lights")) {
    for (const Json& l : j.at("lights").array("lights")) {
      SceneLight sl;
      vec3(l.at("pos"), sl.pos, "lights[].pos");
      vec3(l.at("color"), sl.color, "lights[].color");
      sl.t = l.at("t").number("lights[].t");
      for (double c : sl.color)
        if (!(c >= 0 && c <= 64)) throw std::runtime_error("'lights[].color' components must be in 0..64");
      s.lights.push_back(sl);
    }
  }
  if (j.has("budget")) {
    s.budget = j.at("budget").number("budget");
    if (!(s.budget >= 1e4 && s.budget <= 5e7)) throw std::runtime_error("'budget' must be in 10000..50000000");
  }
  std::stable_sort(s.words.begin(), s.words.end(), [](const SceneWord& a, const SceneWord& b) { return a.t < b.t; });
  std::stable_sort(s.lights.begin(), s.lights.end(), [](const SceneLight& a, const SceneLight& b) { return a.t < b.t; });
  return s;
}

Scene default_scene() {
  Scene s;
  s.epoch_unix_ms = 0;
  s.loop_s = 120;
  s.time_of_day = 0.245;  // golden hour: the sun about 7 degrees up
  // From the sea toward a mountain massif and back, with two holds. Positions
  // are km, the second coordinate altitude above sea level.
  auto K = [&](double t, double px, double py, double pz, double lx, double ly, double lz, double fov) {
    Keyframe k{t, {px, py, pz}, {lx, ly, lz}, fov};
    s.camera.push_back(k);
  };
  K(0, -16, 0.3, -19, -42, 1.3, -72, 60);
  K(10, -16, 0.3, -19, -42, 1.3, -72, 60);  // hold
  K(28, -22, 0.7, -30, -45, 1.2, -85, 60);
  K(46, -32, 1.9, -55, -60, 1.9, -100, 62);
  K(56, -32, 1.9, -55, -60, 1.9, -100, 62);  // hold
  K(74, -45, 2.6, -75, -20, 0.8, -40, 64);
  K(92, -30, 1.2, -40, 10, 0.3, 10, 60);
  K(106, -14, 0.5, -14, -30, 0.6, -60, 60);
  K(120, -16, 0.3, -19, -42, 1.3, -72, 60);
  s.words.push_back({"FCVM", 5});
  return s;
}

Tile tile_geometry(const Rect& r, double image_aspect, int requested_h) {
  int h = std::clamp(requested_h, 16, 2160);
  h &= ~1;
  Tile t;
  t.h = h;
  t.full_h = std::max(16, (int)std::lround(h / r.h));
  t.full_w = std::max(16, (int)std::lround(t.full_h * image_aspect));
  t.y0 = (int)std::lround(r.y * t.full_h);
  t.x0 = (int)std::lround(r.x * t.full_w);
  int w = (int)std::lround(r.w * t.full_w);
  w = std::clamp(w, 16, 4096);
  t.w = (w + 1) & ~1;
  return t;
}

int tile_h_for(const Rect& r, double image_aspect, int full_h) {
  int best = 16, bd = 1 << 30;
  int c = (int)std::lround(full_h * r.h);
  for (int h = std::max(16, (c - 8) & ~1); h <= std::min(2160, c + 8); h += 2) {
    int d = std::abs(tile_geometry(r, image_aspect, h).full_h - full_h);
    if (d < bd) { bd = d; best = h; }
  }
  return best;
}

double scene_time(const Scene& s, double t) {
  if (s.loop_s > 0) {
    double m = std::fmod(t, s.loop_s);
    return m < 0 ? m + s.loop_s : m;
  }
  return t;
}

CamPose camera_at(const std::vector<Keyframe>& kf, double t) {
  CamPose out{};
  size_t n = kf.size();
  auto set = [&](const Keyframe& k) {
    for (int i = 0; i < 3; i++) { out.pos[i] = k.pos[i]; out.look[i] = k.look[i]; }
    out.fov_deg = k.fov_deg;
  };
  if (n == 0) {
    Keyframe k{0, {0, 1, 0}, {0, 1, 10}, 60};
    set(k);
    out.held = true;
    return out;
  }
  // start of the run of equal keyframes ending at j
  auto run_start = [&](size_t j) {
    while (j > 0 && same_pose(kf[j - 1], kf[j])) j--;
    return j;
  };
  if (t <= kf[0].t) {
    set(kf[0]);
    out.held = true;
    out.hold_start = kf[0].t;
    return out;
  }
  if (t >= kf[n - 1].t) {
    set(kf[n - 1]);
    out.held = true;
    out.hold_start = kf[run_start(n - 1)].t;
    return out;
  }
  size_t i = std::upper_bound(kf.begin(), kf.end(), t, [](double v, const Keyframe& k) { return v < k.t; }) - kf.begin() - 1;
  const Keyframe& k1 = kf[i];
  const Keyframe& k2 = kf[i + 1];
  if (same_pose(k1, k2)) {
    set(k1);
    out.held = true;
    out.hold_start = kf[run_start(i)].t;
    return out;
  }
  double dt = k2.t - k1.t;
  double u = (t - k1.t) / dt;
  // Tangents scaled to this segment; zero next to a hold so motion eases in and out.
  bool ease_in = (i == 0) || same_pose(kf[i - 1], k1);
  bool ease_out = (i + 2 >= n) || same_pose(k2, kf[i + 2]);
  double h00 = 2 * u * u * u - 3 * u * u + 1, h10 = u * u * u - 2 * u * u + u;
  double h01 = -2 * u * u * u + 3 * u * u, h11 = u * u * u - u * u;
  auto interp = [&](auto get, double* o) {
    for (int c = 0; c < 3; c++) {
      double p1 = get(k1)[c], p2 = get(k2)[c];
      double m1 = 0, m2 = 0;
      if (!ease_in) {
        const Keyframe& k0 = kf[i - 1];
        m1 = (p2 - get(k0)[c]) / (k2.t - k0.t) * dt;
      }
      if (!ease_out) {
        const Keyframe& k3 = kf[i + 2];
        m2 = (get(k3)[c] - p1) / (k3.t - k1.t) * dt;
      }
      o[c] = h00 * p1 + h10 * m1 + h01 * p2 + h11 * m2;
    }
  };
  interp([](const Keyframe& k) { return k.pos; }, out.pos);
  interp([](const Keyframe& k) { return k.look; }, out.look);
  out.fov_deg = k1.fov_deg + (k2.fov_deg - k1.fov_deg) * u;
  out.held = false;
  return out;
}

void fill_noise_table(V4* tab) {
  auto val = [](int i, int j) {
    uint32_t h = hash_u32((uint32_t)(i & (NOISE_N - 1)) * 0x8DA6B343u ^ hash_u32((uint32_t)(j & (NOISE_N - 1)) + 0x1234567u));
    return u01(h) * 2.0f - 1.0f;
  };
  for (int j = 0; j < NOISE_N; j++)
    for (int i = 0; i < NOISE_N; i++)
      tab[j * NOISE_N + i] = {val(i, j), val(i + 1, j), val(i, j + 1), val(i + 1, j + 1)};
}

const HostNoise& host_noise() {
  static HostNoise n = [] {
    HostNoise h;
    h.tab.resize(NOISE_N * NOISE_N);
    fill_noise_table(h.tab.data());
    return h;
  }();
  return n;
}

GCam make_cam(const double pos[3], const double look[3], double fov_deg, double aspect) {
  GCam c;
  double f[3] = {look[0] - pos[0], look[1] - pos[1], look[2] - pos[2]};
  double fl = std::sqrt(f[0] * f[0] + f[1] * f[1] + f[2] * f[2]);
  if (fl < 1e-9) { f[0] = 0; f[1] = 0; f[2] = 1; fl = 1; }
  for (double& v : f) v /= fl;
  // right = fwd x up, guarded against looking straight up or down
  double up[3] = {0, 1, 0};
  if (std::fabs(f[1]) > 0.999) { up[0] = 0; up[1] = 0; up[2] = 1; }
  double r[3] = {f[1] * up[2] - f[2] * up[1], f[2] * up[0] - f[0] * up[2], f[0] * up[1] - f[1] * up[0]};
  double rl = std::sqrt(r[0] * r[0] + r[1] * r[1] + r[2] * r[2]);
  for (double& v : r) v /= rl;
  double u[3] = {r[1] * f[2] - r[2] * f[1], r[2] * f[0] - r[0] * f[2], r[0] * f[1] - r[1] * f[0]};
  c.pos = v3d(pos);
  c.fwd = v3d(f);
  c.right = v3d(r);
  c.up = v3d(u);
  c.tan_half = (float)std::tan(fov_deg * M_PI / 360.0);
  c.aspect = (float)aspect;
  return c;
}

namespace {
double curvature(const double p[3]) { return (p[0] * p[0] + p[2] * p[2]) / (2.0 * PLANET_R); }
// Scene positions give altitude above sea level as their second coordinate.
// Keep the eye at least 60 m above ground and sea, then convert to world y.
void clamp_above_ground(double p[3]) {
  const V4* tab = host_noise().tab.data();
  double ground = std::max(0.0, (double)surface_h(v2((float)p[0], (float)p[2]), 9, tab));
  if (p[1] < ground + 0.06) p[1] = ground + 0.06;
  p[1] -= curvature(p);
}
void to_world(double p[3]) { p[1] -= curvature(p); }

V3 sun_direction(double tod) {
  const double lat = 38.0 * M_PI / 180, dec = 16.0 * M_PI / 180;
  double H = 2 * M_PI * (tod - 0.5);
  double east = -std::cos(dec) * std::sin(H);
  double north = std::sin(dec) * std::cos(lat) - std::cos(dec) * std::cos(H) * std::sin(lat);
  double up = std::sin(dec) * std::sin(lat) + std::cos(dec) * std::cos(H) * std::cos(lat);
  return normalize(v3((float)east, (float)up, (float)north));
}


void set_text(GText& g, const std::string& s) {
  g.len = (int)std::min<size_t>(s.size(), MAX_CHARS);
  memset(g.ch, 0, sizeof g.ch);
  for (int i = 0; i < g.len; i++) g.ch[i] = (unsigned char)s[i];
}
}  // namespace

namespace {
// Days from 1970-01-01 to a civil date (proleptic Gregorian), after Howard
// Hinnant's days_from_civil.
int64_t days_from_civil(int64_t y, int m, int d) {
  y -= m <= 2;
  int64_t era = (y >= 0 ? y : y - 399) / 400;
  int64_t yoe = y - era * 400;
  int64_t doy = (153 * (m + (m > 2 ? -3 : 9)) + 2) / 5 + d - 1;
  int64_t doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
  return era * 146097 + doe - 719468;
}
// The day of the month of the n-th Sunday of a month.
int nth_sunday(int64_t y, int m, int n) {
  int64_t first = days_from_civil(y, m, 1);
  int wd = (int)(((first % 7) + 11) % 7);  // 0 = Sunday (1970-01-01 was a Thursday)
  return 1 + (7 - wd) % 7 + 7 * (n - 1);
}
}  // namespace

// The clock's text: America/Los_Angeles, from the US rule (daylight time
// from 2:00 local on the second Sunday in March, 10:00 UTC, to 2:00 local on
// the first Sunday in November, 9:00 UTC), so no time zone data is needed and
// every tile draws the same digits.
std::string pacific_clock(int64_t unix_s) {
  time_t ts = (time_t)unix_s;
  struct tm u;
  gmtime_r(&ts, &u);
  int64_t y = u.tm_year + 1900;
  int64_t start = days_from_civil(y, 3, nth_sunday(y, 3, 2)) * 86400 + 10 * 3600;
  int64_t end = days_from_civil(y, 11, nth_sunday(y, 11, 1)) * 86400 + 9 * 3600;
  bool dst = unix_s >= start && unix_s < end;
  time_t local = (time_t)(unix_s + (dst ? -7 : -8) * 3600);
  struct tm l;
  gmtime_r(&local, &l);
  char buf[32];
  snprintf(buf, sizeof buf, "%02d:%02d:%02d %s", l.tm_hour, l.tm_min, l.tm_sec, dst ? "PDT" : "PST");
  return buf;
}

FrameParams build_frame(const Scene& s, const Tile& tile, double t, uint64_t* hold_key, bool* held) {
  FrameParams p;
  memset(&p, 0, sizeof p);
  double st = scene_time(s, t);
  CamPose pose = camera_at(s.camera, st);
  clamp_above_ground(pose.pos);
  to_world(pose.look);
  double aspect = (double)tile.full_w / tile.full_h;
  p.cam = make_cam(pose.pos, pose.look, pose.fov_deg, aspect);
  p.full_w = tile.full_w; p.full_h = tile.full_h;
  p.x0 = tile.x0; p.y0 = tile.y0; p.w = tile.w; p.h = tile.h;

  double layer_t = pose.held ? pose.hold_start : st;
  p.layer_t = (float)std::fmod(layer_t, 100000.0);
  p.sun_dir = sun_direction(s.time_of_day);
  p.sun_e = v3(1.0f, 0.97f, 0.92f) * 22.0f;
  float se = p.sun_dir.y;
  float day = smoothstepf(-0.12f, 0.3f, se);
  p.exposure = 0.8f / (0.04f + 2.6f * day);
  // Cloud offset: the cloud volume tiles every CLOUD_TILE_KM, so reducing
  // modulo the tile keeps it continuous at any t.
  double wx = std::fmod(layer_t * 0.010, (double)CLOUD_TILE_KM), wz = std::fmod(layer_t * 0.0035, (double)CLOUD_TILE_KM);
  p.wind = v2((float)wx, (float)wz);
  p.wave_t = (float)std::fmod(layer_t * 0.6, (double)NOISE_N);

  // Lights lit in the accumulated layer: every light whose t has passed.
  uint64_t key = 0x5EED;
  for (const SceneLight& l : s.lights) {
    if (l.t > st || p.n_lights >= MAX_LIGHTS) continue;
    GLight& g = p.lights[p.n_lights++];
    double lp[3] = {l.pos[0], l.pos[1], l.pos[2]};
    to_world(lp);
    g.pos = v3d(lp);
    g.color = v3((float)l.color[0], (float)l.color[1], (float)l.color[2]);
    g.radius = 0.035f;
    g.alpha = (float)std::min(1.0, (st - l.t) / 0.5);
    for (int i = 0; i < 3; i++) { key = mix64(key, dbits(l.pos[i])); key = mix64(key, dbits(l.color[i])); }
  }
  // Text: the clock first, then the newest words. Both are placed in screen
  // terms (direction from the eye) inside a safe area, so none is cut off at
  // the image's edge and the clock is always in view; they sit in the world at
  // a fixed distance and face the camera.
  const float tanh_ = p.cam.tan_half, asp = (float)aspect;
  auto place = [&](GText& g, float u, float v, float dist, float hfrac) {
    V3 dir = normalize(p.cam.fwd + p.cam.right * (u * tanh_ * asp) + p.cam.up * (v * tanh_));
    g.center = p.cam.pos + dir * dist;
    g.right = p.cam.right;
    g.up = p.cam.up;
    g.height = hfrac * 2.0f * tanh_ * dist;  // hfrac of the image's height
  };
  auto text_halfwidth_u = [&](int len, float hfrac) {  // half the text's width in u (-1..1 across the image)
    float wh = (len * GLYPH_ADVANCE - (GLYPH_ADVANCE - GLYPH_W)) / GLYPH_H;  // width / height
    return hfrac * wh / asp;
  };
  {
    GText& g = p.text[p.n_text++];
    set_text(g, pacific_clock((int64_t)std::floor(s.epoch_unix_ms / 1000.0 + t)));
    place(g, 0.0f, 0.66f, 2.0f, 0.075f);
    g.color = v3(1.0f, 0.86f, 0.62f) * 3.2f;  // the rim's light
    g.style = 1;
    g.occlude = 0;  // always readable
    g.alpha = 1.0f;
  }
  int first = std::max(0, (int)s.words.size() - (MAX_TEXTS - 1));
  int slot = 0;
  for (int i = (int)s.words.size() - 1; i >= first; i--) {  // newest first gets the top row
    const SceneWord& w = s.words[i];
    double age = st - w.t;
    if (age < 0 || age > 90) continue;
    GText& g = p.text[p.n_text++];
    set_text(g, w.text);
    uint32_t h = hash_u32((uint32_t)i * 2654435761u ^ (uint32_t)std::lround(w.t * 1000));
    const float hf = 0.05f;
    // six rows below the clock, three columns; the safe area is 85% of the
    // width and from 0.45 down to -0.75 of the height
    int row = slot % 6, col = (slot / 6) % 3;
    slot++;
    float v = 0.38f - row * 0.2f;
    float half = text_halfwidth_u(g.len, hf);
    float lim = std::max(0.0f, 0.85f - half);
    float u = std::clamp((col - 1) * 0.55f + (u01(h) - 0.5f) * 0.3f, -lim, lim);
    place(g, u, v, 2.5f, hf);
    float hue = u01(hash_u32(h ^ 0xABCDu));
    V3 col3 = v3(0.5f + 0.5f * cosf(6.2832f * hue), 0.5f + 0.5f * cosf(6.2832f * (hue + 0.33f)),
                 0.5f + 0.5f * cosf(6.2832f * (hue + 0.67f)));
    g.color = (col3 * 0.85f + v3(0.15f)) * 2.6f;
    g.style = 0;
    g.occlude = 1;
    g.alpha = (float)(std::min(1.0, age / 1.5) * std::min(1.0, (90 - age) / 3.0));
  }

  *held = pose.held;
  if (pose.held) {
    key = mix64(key, dbits(pose.hold_start));
    for (int i = 0; i < 3; i++) { key = mix64(key, dbits(pose.pos[i])); key = mix64(key, dbits(pose.look[i])); }
    key = mix64(key, dbits(pose.fov_deg));
    key = mix64(key, dbits(s.time_of_day));
    key = mix64(key, (uint64_t)tile.full_w << 32 | (uint32_t)tile.full_h);
    key = mix64(key, (uint64_t)p.n_lights);
    if (key == 0) key = 1;
    p.seed = (uint32_t)(key ^ (key >> 32));
  } else {
    key = 0;
    uint64_t tb = dbits(st);
    p.seed = hash_u32((uint32_t)tb ^ hash_u32((uint32_t)(tb >> 32)));
  }
  p.hold = pose.held ? 1 : 0;
  *hold_key = key;
  return p;
}
