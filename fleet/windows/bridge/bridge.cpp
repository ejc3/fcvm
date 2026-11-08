#include "bridge.h"

#include <math.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

#include <algorithm>
#include <chrono>
#include <climits>
#include <thread>

#include "admit.h"
#include "json.h"
#include "proto.h"

namespace {
Thumb load_reference(const BridgeOptions& o) {
  Thumb t;
  if (o.logon || o.desktop_ref.empty()) return t;
  std::string err;
  if (!load_thumb(o.desktop_ref, &t, &err)) {
    // a reference that is not there yet: the gold flow makes it with the settled desktop
    fprintf(stderr, "windows-bridge: DESKTOP_REF not used (%s); the desktop is the first settled frame\n", err.c_str());
    t = Thumb();
  }
  return t;
}
std::string actions_json() {
  std::string s = "[";
  for (int i = 0; i < kActionCount; i++) s += (i ? ",\"" : "\"") + std::string(kActions[i].name) + "\"";
  return s + "]";
}
}  // namespace

Bridge::Bridge(BridgeOptions o)
    : opt_(std::move(o)), link_(opt_.rdp, this), boot_id_(new_boot_id()), detector_(load_reference(opt_), opt_.settle_ms) {
  times_.start = unix_ms_now();
}

// ---- the link's reports

void Bridge::on_attempt(double ms) {
  std::lock_guard<std::mutex> l(hmu_);
  attempts_++;
  if (times_.tcp < 0) times_.connect_started = ms;
}

void Bridge::on_tcp(double ms) {
  std::lock_guard<std::mutex> l(hmu_);
  if (times_.tcp < 0) times_.tcp = ms;
}

void Bridge::on_rdp(double ms) {
  std::lock_guard<std::mutex> l(hmu_);
  connects_++;
  connected_ = true;
  last_error_.clear();
  // a desktop settles on this connection's frames only
  detector_.reset_settle();
  if (times_.rdp < 0) times_.rdp = ms;
  fprintf(stderr, "windows-bridge: connected to %s (connection %llu)\n", opt_.rdp.socket.c_str(), (unsigned long long)connects_);
}

void Bridge::on_down(const std::string& why, bool fatal, bool was_connected) {
  std::lock_guard<std::mutex> l(hmu_);
  connected_ = false;
  last_error_ = why;
  if (fatal) fatal_ = why;
  // the surface stands still while disconnected because nothing arrives: that is no settle
  detector_.reset_settle();
  // every attempt before the sandbox listens fails: log the first of a run and the changes
  static std::string logged;
  if (fatal || was_connected || why != logged) fprintf(stderr, "windows-bridge: %s\n", why.c_str());
  logged = why;
  hcv_.notify_all();
}

void Bridge::desktop_found_locked() {
  times_.desktop = detector_.desktop_ms();
  fprintf(stderr, "windows-bridge: desktop (%s) at %.0f, %.0f ms after start\n", detector_.by(), times_.desktop,
          times_.desktop - times_.start);
  if (!detector_.has_reference() && !opt_.desktop_ref_out.empty()) {
    std::string err;
    if (!write_thumb_ppm(opt_.desktop_ref_out, detector_.desktop_thumb(), &err))
      fprintf(stderr, "windows-bridge: DESKTOP_REF_OUT: %s\n", err.c_str());
  }
  hcv_.notify_all();
}

void Bridge::on_gfx_command(uint32_t codec_id) {
  std::lock_guard<std::mutex> l(hmu_);
  gfx_codecs_[codec_id]++;
}

void Bridge::on_frame(const uint8_t* bgrx, int w, int h, size_t stride, double ms, bool gfx) {
  {
    std::lock_guard<std::mutex> l(smu_);
    surf_.resize((size_t)w * h * 4);
    for (int y = 0; y < h; y++) memcpy(&surf_[(size_t)y * w * 4], bgrx + (size_t)y * stride, (size_t)w * 4);
    surf_w_ = w;
    surf_h_ = h;
    surf_seq_++;
    surf_ms_ = ms;
  }
  {
    std::lock_guard<std::mutex> l(hmu_);
    if (times_.first_frame < 0) times_.first_frame = ms;
    (gfx ? gfx_frames_ : paint_frames_)++;
    if (!detector_.found()) {
      detector_.frame(ms, thumb_from_bgrx(bgrx, w, h, stride));
      if (detector_.found()) desktop_found_locked();
    }
  }
  std::lock_guard<std::mutex> l(mu_);
  cv_.notify_all();
}

void Bridge::on_tick(double ms) {
  // a settle only counts while connected: with no connection the surface stands still because
  // nothing arrives
  if (!link_.connected()) return;
  std::lock_guard<std::mutex> l(hmu_);
  if (detector_.found()) return;
  detector_.tick(ms);
  if (detector_.found()) desktop_found_locked();
}

bool Bridge::wait_desktop(double deadline_mono_ms) {
  std::unique_lock<std::mutex> l(hmu_);
  while (!detector_.found() && fatal_.empty()) {
    double left = deadline_mono_ms - mono_ms_now();
    if (left <= 0) return false;
    hcv_.wait_for(l, std::chrono::milliseconds((long long)std::min(left, 200.0)));
  }
  return detector_.found();
}

std::string Bridge::fatal() {
  std::lock_guard<std::mutex> l(hmu_);
  return fatal_;
}

std::string Bridge::times_json() {
  auto num = [](double v) { return v < 0 ? std::string("null") : std::to_string(llround(v)); };
  std::lock_guard<std::mutex> l(hmu_);
  const WindowsTimes& t = times_;
  return std::string("{\"desktop_by\":\"") + detector_.by() + "\",\"connects\":" + std::to_string(connects_) +
         ",\"attempts\":" + std::to_string(attempts_) + ",\"times_unix_ms\":{\"start\":" + num(t.start) +
         ",\"connect_started\":" + num(t.connect_started) + ",\"tcp\":" + num(t.tcp) + ",\"rdp\":" + num(t.rdp) +
         ",\"first_frame\":" + num(t.first_frame) + ",\"desktop\":" + num(t.desktop) + "}}";
}

// ---- streams

std::shared_ptr<Subscriber> Bridge::subscribe(int h, int q) {
  auto s = std::make_shared<Subscriber>();
  std::lock_guard<std::mutex> l(mu_);
  s->id = next_id_++;
  s->want_h = h;
  s->want_q = q;
  subs_.push_back(s);
  cv_.notify_all();
  return s;
}

void Bridge::unsubscribe(const std::shared_ptr<Subscriber>& s) {
  s->close();
  std::lock_guard<std::mutex> l(mu_);
  subs_.erase(std::remove(subs_.begin(), subs_.end(), s), subs_.end());
}

void Bridge::wake() {
  std::lock_guard<std::mutex> l(mu_);
  cv_.notify_all();
}

bool Bridge::post_scene(const std::string& body, std::string* err) {
  BridgeScene s;
  if (!bridge_scene_parse(body, &s, err)) return false;
  std::lock_guard<std::mutex> l(mu_);
  scene_ = s;
  // as the phone streamer (AdmitKeys::on_scene): a scene without keys turns the check off
  admit_keys_ = s.admit_keys.empty() ? std::vector<std::string>{} : admit_keys_after(admit_keys_, s.admit_keys);
  scene_version_++;
  cv_.notify_all();
  return true;
}

bool Bridge::post_budget(const std::string& body, std::string* err) {
  // the wall's budget controller posts every tile; a desktop's rate is Windows'
  try {
    Json j = Json::parse(body);
    j.at("budget").number("budget");
  } catch (const std::exception& e) {
    *err = e.what();
    return false;
  }
  return true;
}

bool Bridge::admits(const std::string& token) {
  std::lock_guard<std::mutex> l(mu_);
  return admit_keys_.empty() || admit_token_valid(admit_keys_, token, (int64_t)time(nullptr));
}

bool Bridge::requires_admit() {
  std::lock_guard<std::mutex> l(mu_);
  return !admit_keys_.empty();
}

bool Bridge::has_scene() {
  std::lock_guard<std::mutex> l(mu_);
  return scene_version_ > 0;
}

bool Bridge::next_frame(int, std::vector<uint8_t>* rgba, int* w, int* h, std::string* tile_json) {
  std::lock_guard<std::mutex> l(smu_);
  if (!surf_w_) return false;
  rgba->resize((size_t)surf_w_ * surf_h_ * 4);
  bgrx_to_rgba(surf_.data(), (size_t)surf_w_ * 4, surf_w_, surf_h_, rgba->data());
  *w = surf_w_;
  *h = surf_h_;
  *tile_json = "{\"windows\":true}";
  return true;
}

void Bridge::send(double t, double stamp_unix_ms) {
  std::vector<std::shared_ptr<Subscriber>> subs;
  {
    std::lock_guard<std::mutex> l(mu_);
    subs = subs_;
  }
  std::map<int, std::vector<std::shared_ptr<Subscriber>>> by_q;
  for (auto& s : subs) by_q[s->want_q.load()].push_back(s);
  for (auto it = encs_.begin(); it != encs_.end();) it = by_q.count(it->first) ? std::next(it) : encs_.erase(it);
  const int w = enc_w_, h = enc_h_;
  const uint8_t* y = i420_.data();
  const uint8_t* u = y + (size_t)w * h;
  const uint8_t* v = u + (size_t)(w / 2) * (h / 2);
  double t0 = mono_ms_now();
  bool sent = false;
  for (auto& [q, list] : by_q) {
    Enc& e = encs_[q];
    if (!e.enc || e.enc->width() != w || e.enc->height() != h) {
      e.enc.reset(new X264Encoder());
      std::string err;
      if (!e.enc->open(w, h, opt_.fps, q, &err)) {
        fprintf(stderr, "windows-bridge: encoder %dx%d q=%s: %s\n", w, h, quality_name(q), err.c_str());
        for (auto& s : list) s->close();
        encs_.erase(q);
        continue;
      }
      e.key = next_key_++;
      e.want_idr = true;
      for (auto& s : list) s->cur_h = 0;
    }
    std::string hdr = bridge_header_json(w, h, e.enc->codec(), opt_.fps, quality_name(q), e.enc->bitrate());
    auto hmsg = std::make_shared<const std::string>(ws_frame(1, hdr.data(), hdr.size()));
    for (auto& s : list) {
      s->offer_header(e.key, hmsg);
      if (s->need_key) e.want_idr = true;  // a new reader, or one dropped behind, starts at a keyframe
    }
    double now = mono_ms_now();
    bool force = e.want_idr || now - e.last_idr_ms >= 2000.0;
    std::vector<uint8_t> au;
    bool key = false;
    std::string err;
    if (!e.enc->encode(y, u, v, force, &au, &key, &err)) {
      fprintf(stderr, "windows-bridge: encode: %s\n", err.c_str());
      e.want_idr = true;
      continue;
    }
    if (key) {
      e.last_idr_ms = now;
      e.want_idr = false;
    }
    std::string payload(FCVF_HEADER_BYTES, '\0');
    fcvf_header((uint8_t*)&payload[0], key, w, h, t, frame_no_);
    payload.append((const char*)au.data(), au.size());
    auto msg = std::make_shared<const std::string>(ws_frame(2, payload.data(), payload.size()));
    for (auto& s : list)
      if (s->offer_frame(msg, key) == Subscriber::DROPPED) e.want_idr = true;
    sent = true;
  }
  if (!sent) return;
  std::lock_guard<std::mutex> l(hmu_);
  encode_ms_.add(mono_ms_now() - t0);
  last_t_ = t;
  sent_frame_ = frame_no_;
  last_stamp_ms_ = stamp_unix_ms;
  if (first_sent_ms_ < 0) first_sent_ms_ = unix_ms_now() - times_.start;
}

void Bridge::encode_loop() {
  const int fps = opt_.fps;
  uint64_t sent_seq = 0;
  long long last_slot = LLONG_MIN;
  double last_epoch = 0;
  double last_send = -1e18;  // monotonic ms
  double fps_t0 = mono_ms_now();
  int fps_n = 0;
  while (true) {
    bool any = false, need_key = false;
    double epoch = 0;
    {
      std::unique_lock<std::mutex> l(mu_);
      cv_.wait_for(l, std::chrono::milliseconds(50));
      any = !subs_.empty();
      for (auto& s : subs_) need_key = need_key || s->need_key;
      epoch = scene_.epoch_unix_ms;
    }
    double mono = mono_ms_now();
    if (mono - fps_t0 >= 1000.0) {
      std::lock_guard<std::mutex> l(hmu_);
      fps_ = fps_n * 1000.0 / (mono - fps_t0);
      fps_n = 0;
      fps_t0 = mono;
    }
    // Nobody watches: the surface stays current (on_frame), nothing is encoded.
    if (!any) {
      encs_.clear();
      continue;
    }
    if (epoch != last_epoch) {  // a new scene clock: slots restart
      last_epoch = epoch;
      last_slot = LLONG_MIN;
    }
    uint64_t seq;
    double frame_ms;
    {
      std::lock_guard<std::mutex> l(smu_);
      if (!surf_w_) continue;
      seq = surf_seq_;
      frame_ms = surf_ms_;
    }
    // A new frame is sent once per slot (at most FPS a second); a desktop that stands still is
    // sent again once a second and to a reader that needs a keyframe.
    bool fresh = seq != sent_seq;
    if (!fresh && !need_key && mono - last_send < 1000.0) continue;
    double now = unix_ms_now();
    long long now_slot = frame_slot(now, epoch, fps);
    if (now_slot <= last_slot) {
      double next_ms = epoch + (double)(last_slot + 1) * 1000.0 / fps;
      std::this_thread::sleep_for(std::chrono::microseconds((long long)std::max(0.0, (next_ms - now) * 1000.0) + 100));
      continue;
    }
    // A frame is stamped with the slot of its EndFrame; frames that came within one slot go out
    // as one, in the slot after the last one sent.
    long long slot = fresh ? std::max(frame_slot(frame_ms, epoch, fps), last_slot + 1) : now_slot;
    {
      std::lock_guard<std::mutex> l(smu_);
      enc_w_ = surf_w_ & ~1;
      enc_h_ = surf_h_ & ~1;
      i420_.resize((size_t)enc_w_ * enc_h_ * 3 / 2);
      uint8_t* y = i420_.data();
      bgrx_to_i420(surf_.data(), (size_t)surf_w_ * 4, enc_w_, enc_h_, y, y + (size_t)enc_w_ * enc_h_,
                   y + (size_t)enc_w_ * enc_h_ + (size_t)(enc_w_ / 2) * (enc_h_ / 2));
      sent_seq = surf_seq_;
    }
    last_slot = slot;
    last_send = mono_ms_now();
    frame_no_++;
    send((double)slot / fps, epoch + (double)slot * 1000.0 / fps);
    fps_n++;
  }
}

// ---- actions

Thumb Bridge::current_thumb() {
  std::lock_guard<std::mutex> l(smu_);
  if (!surf_w_) return Thumb();
  return thumb_from_bgrx(surf_.data(), surf_w_, surf_h_, (size_t)surf_w_ * 4);
}

int Bridge::post_action(const std::string& body, std::string* reply) {
  std::string err;
  const Action* a = action_from_body(body, &err);
  if (!a) {
    *reply = "{\"error\":\"" + json_escape(err) + "\",\"actions\":" + actions_json() + "}";
    return 400;
  }
  return play(*a, reply);
}

int Bridge::play(const Action& a, std::string* reply) {
  std::unique_lock<std::mutex> one(action_mu_, std::try_to_lock);
  auto fail = [&](int code, const std::string& why) {
    *reply = "{\"error\":\"" + json_escape(why) + "\",\"action\":\"" + a.name + "\"}";
    return code;
  };
  if (!one.owns_lock()) return fail(409, "another action is running");
  if (!link_.connected()) return fail(503, "not connected to the sandbox");
  double t0 = mono_ms_now();
  Thumb before = current_thumb();
  int presses = -1;
  for (int i = 0; i < a.nsteps; i++) {
    const Step& s = a.steps[i];
    std::vector<KeyEvent> keys;
    switch (s.kind) {
      case STEP_CHORD:
      case STEP_TEXT:
        before = current_thumb();
        if (!step_keys(s, &keys)) return fail(500, "a step of the action has a key the US map lacks");
        if (std::string e; !link_.send_keys(keys, &e)) return fail(503, e);
        break;
      case STEP_WAIT:
        std::this_thread::sleep_for(std::chrono::milliseconds(s.ms));
        break;
      case STEP_WAIT_CHANGE: {
        double until = mono_ms_now() + s.ms;
        while (thumb_rmse(current_thumb(), before) <= CHANGE_RMSE) {
          if (mono_ms_now() >= until)
            return fail(504, "the screen did not change within " + std::to_string(s.ms) + " ms; the rest of the action was not sent");
          std::this_thread::sleep_for(std::chrono::milliseconds(50));
        }
        break;
      }
      case STEP_CLOSE_UNTIL_DESKTOP: {
        Thumb ref;
        {
          std::lock_guard<std::mutex> l(hmu_);
          ref = detector_.has_reference() ? detector_.reference() : detector_.desktop_thumb();
        }
        presses = 0;
        auto at_desktop = [&] { return !ref.empty() && thumb_rmse(current_thumb(), ref) <= DESKTOP_RMSE; };
        for (int round = 0; round < s.ms && !at_desktop(); round++) {
          for (int k = 0; k < CLOSE_ROUND_KEYS && !at_desktop(); k++) {
            keys.clear();
            step_keys(kCloseRound[k], &keys);
            std::string e;
            if (!link_.send_keys(keys, &e)) return fail(503, e);
            presses++;
            std::this_thread::sleep_for(std::chrono::milliseconds(kCloseRoundWaitMs[k]));
          }
        }
        break;
      }
    }
  }
  char b[256];
  snprintf(b, sizeof b, "{\"ok\":true,\"action\":\"%s\",\"ms\":%.0f%s}", a.name, mono_ms_now() - t0,
           presses >= 0 ? (",\"presses\":" + std::to_string(presses)).c_str() : "");
  *reply = b;
  return 200;
}

// ---- /health

std::string Bridge::health_json() {
  BridgeHealth h;
  std::vector<std::shared_ptr<Subscriber>> subs;
  {
    std::lock_guard<std::mutex> l(mu_);
    for (auto& s : subs_) h.streams.push_back({s->want_h.load(), s->want_q.load()});
    subs = subs_;
  }
  h.viewers = viewers_json(subs);
  {
    std::lock_guard<std::mutex> l(smu_);
    h.surface_w = surf_w_;
    h.surface_h = surf_h_;
  }
  std::lock_guard<std::mutex> l(hmu_);
  h.fps = fps_;
  h.t = last_t_;
  h.frame = sent_frame_;
  h.boot_id = boot_id_;
  h.state = !fatal_.empty() ? STATE_FAILED : !connected_ ? STATE_CONNECTING : detector_.found() ? STATE_DESKTOP : STATE_CONNECTED;
  h.error = fatal_;
  h.last_error = last_error_;
  h.connects = connects_;
  h.attempts = attempts_;
  h.times = times_;
  h.desktop_by = detector_.by();
  h.encode_p50 = encode_ms_.pct(0.5);
  h.encode_p90 = encode_ms_.pct(0.9);
  h.frame_age_ms = last_stamp_ms_ < 0 ? -1 : std::max(0.0, unix_ms_now() - last_stamp_ms_);
  h.first_frame_sent_ms = first_sent_ms_;
  h.h264 = RdpLink::built_with_h264();
  h.gfx_frames = gfx_frames_;
  h.paint_frames = paint_frames_;
  h.gfx_codecs = "{";
  for (const auto& [id, n] : gfx_codecs_)
    h.gfx_codecs += (h.gfx_codecs.size() > 1 ? ",\"" : "\"") + gfx_codec_name(id) + "\":" + std::to_string(n);
  h.gfx_codecs += "}";
  return bridge_health_json(h);
}
