#include "source.h"

#include <cuda.h>
#include <sys/stat.h>
#include <cerrno>
#include <arpa/inet.h>
#include <netinet/in.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <algorithm>
#include <chrono>
#include <atomic>
#include <deque>
#include <thread>

#include "json.h"
#include "nv12.h"
#include "proto.h"
#include "threads.h"

namespace {
double unix_ms() {
  timespec ts;
  clock_gettime(CLOCK_REALTIME, &ts);
  return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}
double mono_s() {
  timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec + ts.tv_nsec / 1e9;
}
std::string cu_err(const char* what, CUresult r) {
  const char* s = nullptr;
  cuGetErrorString(r, &s);
  return std::string(what) + ": " + (s ? s : "CUDA error " + std::to_string((int)r));
}
}  // namespace

PhoneSource::PhoneSource(const StreamerOptions& o)
    : opt_(o), screen_(o.screen_target), gate_(o.fps), boot_id_(new_boot_id()) {
  proc_start_ms_ = unix_ms();
  if (!o.fail.empty()) fatal_ = o.fail;
}

bool PhoneSource::init(std::string* err) {
  CUresult r = cuInit(0);
  if (r != CUDA_SUCCESS) { *err = cu_err("cuInit", r); return false; }
  CUdevice dev;
  if ((r = cuDeviceGet(&dev, 0)) != CUDA_SUCCESS) { *err = cu_err("cuDeviceGet", r); return false; }
  char name[256] = {0}, bus[64] = {0};
  cuDeviceGetName(name, sizeof name, dev);
  cuDeviceGetPCIBusId(bus, sizeof bus, dev);
  CUcontext c;
  if ((r = cuDevicePrimaryCtxRetain(&c, dev)) != CUDA_SUCCESS) { *err = cu_err("cuDevicePrimaryCtxRetain", r); return false; }
  ctx_ = c;
  std::lock_guard<std::mutex> l(hmu_);
  gpu_name_ = name;
  bus_id_ = bus;
  return true;
}

void PhoneSource::set_fatal(const std::string& m) {
  fprintf(stderr, "phone-streamer: %s\n", m.c_str());
  std::lock_guard<std::mutex> l(hmu_);
  fatal_ = m;
}

bool PhoneSource::open_enc(Enc& e, int q, const PhoneGeo& g, std::string* err) {
  close_enc(e);
  cuCtxSetCurrent((CUcontext)ctx_);
  CUdeviceptr p = 0;
  size_t pitch = 0;
  CUresult r = cuMemAllocPitch(&p, &pitch, (size_t)g.w, (size_t)g.h * 3 / 2, 16);
  if (r != CUDA_SUCCESS) { *err = cu_err("cuMemAllocPitch", r); return false; }
  e.enc.reset(new H264Encoder());
  if (!e.enc->open(ctx_, (void*)p, pitch, g.w, g.h, opt_.fps, q, err)) {
    e.enc.reset();
    cuMemFree(p);
    return false;
  }
  e.nv12 = p;
  e.pitch = pitch;
  e.geo = g;
  e.key = next_key_++;
  e.want_idr = true;
  return true;
}

void PhoneSource::close_enc(Enc& e) {
  e.enc.reset();
  if (e.nv12) cuMemFree((CUdeviceptr)e.nv12);
  e.nv12 = 0;
}

std::shared_ptr<Subscriber> PhoneSource::subscribe(int h, int q) {
  auto s = std::make_shared<Subscriber>();
  std::lock_guard<std::mutex> l(mu_);
  s->id = next_id_++;
  s->want_h = h;
  s->want_q = q;
  subs_.push_back(s);
  woken_ = true;
  cv_.notify_all();
  return s;
}

void PhoneSource::unsubscribe(const std::shared_ptr<Subscriber>& s) {
  s->close();
  std::lock_guard<std::mutex> l(mu_);
  subs_.erase(std::remove(subs_.begin(), subs_.end(), s), subs_.end());
}

void PhoneSource::wake() {
  std::lock_guard<std::mutex> l(mu_);
  woken_ = true;
  cv_.notify_all();
}

bool PhoneSource::post_scene(const std::string& body, std::string* err) {
  PhoneScene s;
  if (!phone_scene_parse(body, &s, err)) return false;
  std::lock_guard<std::mutex> l(mu_);
  // a new holder (or none) retires every open mirror stream at once, writer included
  if (s.mirror_viewer != scene_.mirror_viewer) mirror_holder_gen_++;
  scene_ = s;
  admit_.on_scene(s);
  mirror_admit_.on_mirror_keys(s);
  scene_version_++;
  cv_.notify_all();
  return true;
}

bool PhoneSource::admits(const std::string& token) {
  std::lock_guard<std::mutex> l(mu_);
  return admit_.admits(token, (int64_t)time(nullptr));
}

bool PhoneSource::requires_admit() {
  std::lock_guard<std::mutex> l(mu_);
  return !admit_.keys.empty();
}

bool PhoneSource::mirror_admits(const std::string& token) {
  std::lock_guard<std::mutex> l(mu_);
  return ::mirror_admits(mirror_admit_.keys, scene_.mirror_viewer, token, (int64_t)time(nullptr));
}

void PhoneSource::mirror(Conn& c, const std::string& token) {
  const std::string viewer = admit_token_viewer(token);
  uint64_t holder_gen;
  bool still_holder;
  {
    // the holder may have changed since the token was checked: check again, with the
    // generation, under one lock
    std::lock_guard<std::mutex> l(mu_);
    holder_gen = mirror_holder_gen_;
    still_holder = !viewer.empty() && scene_.mirror_viewer == viewer;
  }
  // Still this stream's holder: checked before each message is queued and before each is sent.
  auto holds = [&] { return mirror_holder_gen_.load() == holder_gen; };
  auto close_frame = [](uint16_t code, const std::string& why) {
    std::string pl;
    pl += (char)(code >> 8);
    pl += (char)(code & 0xff);
    pl += why.substr(0, 120);
    return ws_frame(8, pl.data(), pl.size());
  };
  // before the writer starts: the only writer
  auto close_with = [&](uint16_t code, const std::string& why) {
    std::string f = close_frame(code, why);
    c.write_all(f.data(), f.size());
  };
  // The tap has one consumer: a second stream waits for the first to end, which it does once
  // its viewer no longer holds the phone or goes away.
  if (!still_holder) return close_with(1008, "the token of the viewer holding this phone is required");
  std::unique_lock<std::mutex> one(mirror_mu_, std::try_to_lock);
  if (!one.owns_lock()) return close_with(1013, "the mirror is open in another window");
  int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
  sockaddr_un a{};
  a.sun_family = AF_UNIX;
  snprintf(a.sun_path, sizeof a.sun_path, "%s", opt_.mirror_socket.c_str());
  if (fd < 0 || connect(fd, (sockaddr*)&a, sizeof a) != 0 || ::send(fd, "S", 1, MSG_NOSIGNAL) != 1) {
    std::string e = std::string("the emulator's command stream: ") + strerror(errno);
    if (fd >= 0) close(fd);
    {
      std::lock_guard<std::mutex> l(hmu_);
      mirror_error_ = e;
    }
    return close_with(1011, e);
  }
  {
    std::lock_guard<std::mutex> l(hmu_);
    mirror_opened_++;
    mirror_open_ = true;
    mirror_error_.clear();
  }
  c.set_option(SOL_SOCKET, SO_SNDBUF, 1 << 20);
  // The writer sends the queue; the client reader notices the viewer leave; this thread reads
  // the tap, deflates and queues, and ends the stream when the queue passes its bound.
  std::mutex qmu;
  std::condition_variable qcv;
  std::deque<std::string> q;
  size_t queued = 0;
  std::atomic<bool> done{false};
  bool writer_finished = false;  // under qmu
  std::string close_msg;          // under qmu: the close frame, the writer's last message
  std::thread writer, reader;
  bool started = start_thread(
      [&] {
        while (true) {
          std::string m;
          bool last = false;
          {
            std::unique_lock<std::mutex> l(qmu);
            qcv.wait(l, [&] { return done.load() || !q.empty() || !close_msg.empty(); });
            if (!q.empty()) {
              m = std::move(q.front());
              q.pop_front();
              queued -= m.size();
            } else if (!close_msg.empty()) {
              // the close frame is kept apart from the data: only it is sent whoever holds the phone
              m = std::move(close_msg);
              close_msg.clear();
              last = true;
            } else {
              break;
            }
          }
          // a stream whose holder changed sends nothing more but its close
          if (!last && !holds()) continue;
          if (!c.write_all(m.data(), m.size()) || last) break;
        }
        std::lock_guard<std::mutex> l(qmu);
        writer_finished = true;
        qcv.notify_all();
      },
      &writer);
  if (started) {
    started = start_thread(
        [&] {
          int op = 0;
          std::string pl;
          while (!done.load() && read_ws_message(c, &op, &pl) && op != 8) {
          }
          done.store(true);
          qcv.notify_all();
        },
        &reader);
  }
  if (!started) {
    // no thread for the stream (a pids or thread limit): refuse this one, keep the rest
    done.store(true);
    qcv.notify_all();
    c.shutdown_both();
    if (writer.joinable()) writer.join();
    close(fd);
    std::lock_guard<std::mutex> l(hmu_);
    mirror_open_ = false;
    mirror_error_ = "no thread for the mirror stream";
    return;
  }
  MirrorDeflate z;
  std::string reason;
  uint16_t code = 1000;
  std::vector<uint8_t> buf(1 << 18);
  while (!done.load()) {
    if (!holds()) {
      code = 1000;
      reason = "the phone went back";
      break;
    }
    pollfd p{fd, POLLIN, 0};
    int pr = poll(&p, 1, 200);
    if (pr == 0) continue;
    ssize_t n = pr > 0 ? ::recv(fd, buf.data(), buf.size(), 0) : -1;
    if (n <= 0) {
      if (n < 0 && errno == EINTR) continue;
      code = 1011;
      reason = "the emulator's command stream ended";
      break;
    }
    std::string out;
    if (!z.push(buf.data(), (size_t)n, &out)) {
      code = 1011;
      reason = "deflate failed";
      break;
    }
    std::string frame = ws_frame(2, out.data(), out.size());
    bool behind = false;
    {
      std::lock_guard<std::mutex> l(qmu);
      if (queued + frame.size() > MIRROR_MAX_QUEUED_BYTES) {
        behind = true;
      } else {
        queued += frame.size();
        q.push_back(std::move(frame));
      }
    }
    qcv.notify_all();
    {
      std::lock_guard<std::mutex> l(hmu_);
      mirror_tap_bytes_ += (uint64_t)n;
      mirror_wire_bytes_ += out.size();
      if (behind) mirror_behind_++;
    }
    if (behind) {
      code = 1013;
      reason = "the mirror fell behind: join again";
      break;
    }
  }
  close(fd);  // the tap stops copying when its consumer goes
  {
    // What is queued is dropped (a viewer that fell behind joins again from the start) and the
    // close frame is the writer's last message.
    std::unique_lock<std::mutex> l(qmu);
    q.clear();
    queued = 0;
    close_msg = close_frame(code, reason);
    done.store(true);
    qcv.notify_all();
    // A viewer that stopped reading leaves the writer blocked in a write: after 2 s the socket
    // is shut down, which ends the write.
    if (!qcv.wait_for(l, std::chrono::seconds(2), [&] { return writer_finished; })) c.shutdown_both();
  }
  writer.join();
  c.shutdown_both();
  reader.join();
  std::lock_guard<std::mutex> l(hmu_);
  mirror_open_ = false;
}

bool PhoneSource::has_scene() {
  std::lock_guard<std::mutex> l(mu_);
  return scene_version_ > 0;
}

bool PhoneSource::post_budget(const std::string& body, std::string* err) {
  // The wall's budget controller posts every tile; a phone draws at its display's rate
  // whatever the budget, so a well-formed budget is accepted and changes nothing.
  try {
    Json j = Json::parse(body);
    j.at("budget").number("budget");
  } catch (const std::exception& e) {
    *err = e.what();
    return false;
  }
  return true;
}

void PhoneSource::app_link() {
  // The app's control socket (PROTOCOL.md, "Phone app"): the posted scene, with the stamp
  // asked for, whenever it changes, and the host's clock every 2 s.
  int fd = -1;
  uint64_t sent_version = 0;
  double last_clock = 0;
  std::string inbox;
  while (true) {
    if (fd < 0) {
      fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
      sockaddr_in a{};
      a.sin_family = AF_INET;
      a.sin_port = htons((uint16_t)opt_.app_port);
      a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
      if (fd < 0 || connect(fd, (sockaddr*)&a, sizeof a) != 0) {
        if (fd >= 0) close(fd);
        fd = -1;
        std::this_thread::sleep_for(std::chrono::milliseconds(500));
        continue;
      }
      sent_version = 0;
      last_clock = 0;
      std::lock_guard<std::mutex> l(hmu_);
      app_connects_++;
    }
    std::string out;
    {
      std::unique_lock<std::mutex> l(mu_);
      cv_.wait_for(l, std::chrono::milliseconds(200), [&] { return scene_version_ != sent_version; });
      if (scene_.set && scene_version_ != sent_version) {
        out = app_scene_line(scene_);
        sent_version = scene_version_;
      }
    }
    double now = mono_s();
    if (now - last_clock >= 2.0) {
      out += app_clock_line(unix_ms());
      last_clock = now;
    }
    bool ok = true;
    for (size_t off = 0; ok && off < out.size();) {
      ssize_t n = ::send(fd, out.data() + off, out.size() - off, MSG_NOSIGNAL);
      if (n <= 0) ok = false;
      else off += (size_t)n;
    }
    // the replies: one line each; an error is kept for /health
    char buf[4096];
    ssize_t n = -1;
    while (ok && (n = recv(fd, buf, sizeof buf, MSG_DONTWAIT)) != 0) {
      if (n < 0) {
        if (errno != EAGAIN && errno != EWOULDBLOCK) ok = false;
        break;
      }
      inbox.append(buf, (size_t)n);
    }
    size_t nl;
    while ((nl = inbox.find('\n')) != std::string::npos) {
      std::string line = inbox.substr(0, nl);
      inbox.erase(0, nl + 1);
      std::string e = app_reply_error(line);
      if (!e.empty()) {
        fprintf(stderr, "phone-streamer: the app refused a request: %s\n", e.c_str());
        std::lock_guard<std::mutex> l(hmu_);
        app_error_ = e.substr(0, 300);
      }
    }
    // a reply line is short: an app that sends this much without a newline is not speaking
    // the protocol, and the link starts again
    if (inbox.size() > 65536) ok = false;
    if (!ok || n == 0) {
      close(fd);
      fd = -1;
      inbox.clear();
    }
  }
}

bool PhoneSource::next_frame(int, std::vector<uint8_t>* rgba, int* w, int* h, std::string* tile_json) {
  std::lock_guard<std::mutex> l(hmu_);
  if (last_rgba_.empty()) return false;
  *rgba = last_rgba_;
  *w = last_w_;
  *h = last_h_;
  *tile_json = "{\"picture\":\"screen\"}";
  return true;
}

bool PhoneSource::send(const PhoneGeo& g, double t, double ts_ms) {
  std::vector<std::shared_ptr<Subscriber>> subs;
  {
    std::lock_guard<std::mutex> l(mu_);
    subs = subs_;
  }
  // encoders for the qualities someone wants, at this geometry
  std::map<int, std::vector<std::shared_ptr<Subscriber>>> by_q;
  for (auto& s : subs) by_q[s->want_q.load()].push_back(s);
  for (auto it = encs_.begin(); it != encs_.end();) {
    if (!by_q.count(it->first)) {
      close_enc(it->second);
      it = encs_.erase(it);
    } else {
      ++it;
    }
  }
  double enc_ms = 0;
  for (auto& [q, list] : by_q) {
    Enc& e = encs_[q];
    if (!e.enc || e.geo != g) {
      std::string err;
      if (!open_enc(e, q, g, &err)) {
        fprintf(stderr, "phone-streamer: encoder %dx%d q=%s: %s\n", g.w, g.h, quality_name(q), err.c_str());
        for (auto& s : list) s->close();
        encs_.erase(q);
        continue;
      }
      for (auto& s : list) s->cur_h = 0;
    }
    std::string hdr = phone_header_json(g, e.enc->codec(), opt_.fps, quality_name(q), e.enc->bitrate());
    auto hmsg = std::make_shared<const std::string>(ws_frame(1, hdr.data(), hdr.size()));
    for (auto& s : list) {
      s->offer_header(e.key, hmsg);
      if (s->need_key) e.want_idr = true;  // a new reader, or one dropped behind, starts at a keyframe
    }
    double t0 = mono_s();
    CUDA_MEMCPY2D m;
    memset(&m, 0, sizeof m);
    m.srcMemoryType = CU_MEMORYTYPE_HOST;
    m.srcHost = nv12_host_.data();
    m.srcPitch = (size_t)g.w;
    m.dstMemoryType = CU_MEMORYTYPE_DEVICE;
    m.dstDevice = (CUdeviceptr)e.nv12;
    m.dstPitch = e.pitch;
    m.WidthInBytes = (size_t)g.w;
    m.Height = (size_t)g.h * 3 / 2;
    CUresult r = cuMemcpy2D(&m);
    if (r != CUDA_SUCCESS) {
      set_fatal(cu_err("cuMemcpy2D", r));
      for (auto& sub : subs) sub->close();
      return false;
    }
    bool force = e.want_idr || (t0 - e.last_idr) >= 2.0;
    std::vector<uint8_t> au;
    bool key = false;
    std::string err;
    if (!e.enc->encode(force, &au, &key, &err)) {
      fprintf(stderr, "phone-streamer: encode %dx%d: %s\n", g.w, g.h, err.c_str());
      e.want_idr = true;
      continue;
    }
    enc_ms += (mono_s() - t0) * 1000.0;
    if (key) { e.last_idr = t0; e.want_idr = false; }
    std::string payload(FCVF_HEADER_BYTES, '\0');
    fcvf_header((uint8_t*)&payload[0], key, g.w, g.h, t, frame_no_);
    payload.append((const char*)au.data(), au.size());
    auto msg = std::make_shared<const std::string>(ws_frame(2, payload.data(), payload.size()));
    for (auto& s : list) {
      Subscriber::Offer o = s->offer_frame(msg, key);
      if (o == Subscriber::DROPPED) e.want_idr = true;
    }
  }
  double sent = unix_ms();
  std::lock_guard<std::mutex> l(hmu_);
  if (!by_q.empty()) {
    capture_to_sent_.add(sent - ts_ms);
    encode_ms_.add(enc_ms);
    if (first_sent_ms_ < 0) first_sent_ms_ = sent - proc_start_ms_;
  }
  return true;
}

void PhoneSource::run() {
  if (!fatal_.empty()) return;  // a phone that failed before streaming serves only its /health
  cuCtxSetCurrent((CUcontext)ctx_);
  gpu_.start(bus_id_);
  // Nothing is captured until the phone is known to render on this GPU.
  struct stat gst;
  while (!opt_.go_file.empty() && stat(opt_.go_file.c_str(), &gst) != 0)
    std::this_thread::sleep_for(std::chrono::milliseconds(100));
  {
    std::lock_guard<std::mutex> l(hmu_);
    verified_ = true;
    ready_ = true;
  }
  screen_.start();
  uint64_t last = 0;
  ScreenFrame frame;
  double fps_t0 = mono_s();
  int fps_n = 0;
  double last_send = 0;
  double last_t = 0;
  PhoneGeo held;
  bool have_held = false;
  while (true) {
    // a frame, or 10 ms without one (time to see whether a reader needs a keyframe)
    bool fresh = screen_.next(last, 10, &frame);
    {
      std::lock_guard<std::mutex> l(hmu_);
      screen_error_ = screen_.error();
      app_screen_connects_ = screen_.connects();
    }
    if (!fresh) {
      // nothing new on the screen: a reader that joined meanwhile gets the held picture
      // as a keyframe, and every reader gets it again once a second
      bool need = false;
      {
        std::unique_lock<std::mutex> l(mu_);
        woken_ = false;
        for (auto& s : subs_) need = need || s->need_key;
      }
      if (have_held && (need || mono_s() - last_send >= 1.0)) {
        if (!send(held, last_t, unix_ms())) return;
        last_send = mono_s();
      }
      continue;
    }
    last = frame.seq;
    double ts_ms = frame.ts_us / 1000.0;
    int w = frame.width, h = frame.height;
    const std::vector<uint8_t>& rgba = frame.rgba;
    const uint8_t* pic = rgba.data();
    PhoneScene sc;
    {
      std::lock_guard<std::mutex> l(mu_);
      sc = scene_;
    }
    uint32_t k = 0;
    bool stamped = stamp_read(pic, w, h, (size_t)w * 4, &k);
    bool scene = stamped && stamp_plausible(k, ts_ms, sc);
    double t = 0;
    bool admit;
    {
      std::lock_guard<std::mutex> l(hmu_);
      if (stamped && !scene) implausible_++;
      admit = gate_.admit(scene, k, ts_ms, sc, &t);
      copies_++;
      screen_w_ = w;
      screen_h_ = h;
      if (first_capture_ms_ < 0) first_capture_ms_ = ts_ms - proc_start_ms_;
      if (admit && scene) draw_to_capture_.add(ts_ms - (sc.epoch_unix_ms + t * 1000.0));
      // GET /still: the screen as last copied, stamped or not
      last_rgba_ = rgba;
      last_w_ = w;
      last_h_ = h;
    }
    if (!admit) continue;
    PhoneGeo g = phone_geometry(scene, k, w, h, sc);
    nv12_host_.resize((size_t)g.w * g.h * 3 / 2);
    rgba_to_nv12(pic, (size_t)w * 4, g.w, g.h, nv12_host_.data(), (size_t)g.w);
    frame_no_++;
    if (!send(g, t, ts_ms)) return;
    held = g;
    have_held = true;
    last_send = mono_s();
    last_t = t;
    fps_n++;
    double now = mono_s();
    std::lock_guard<std::mutex> l(hmu_);
    t_ = t;
    frame_ = frame_no_;
    geo_ = g;
    if (now - fps_t0 >= 1.0) {
      fps_ = fps_n / (now - fps_t0);
      fps_n = 0;
      fps_t0 = now;
    }
  }
}

std::string PhoneSource::health_json() {
  GpuSample gs = gpu_.latest();
  std::vector<std::pair<int, int>> streams;
  {
    std::lock_guard<std::mutex> l(mu_);
    for (auto& s : subs_) streams.push_back({s->want_h.load(), s->want_q.load()});
  }
  std::lock_guard<std::mutex> l(hmu_);
  auto num = [](double v) { return v < 0 ? std::string("null") : std::to_string(v); };
  std::string sj = "[";
  for (size_t i = 0; i < streams.size(); i++)
    sj += (i ? "," : "") + std::string("{\"h\":") + std::to_string(streams[i].first) + ",\"q\":\"" +
          quality_name(streams[i].second) + "\"}";
  sj += "]";
  char b[4096];
  snprintf(b, sizeof b,
           "{\"fps\":%.1f,\"heights\":[%d],\"streams\":%s,"
           "\"gpu\":{\"name\":\"%s\",\"temp_c\":%s,\"power_w\":%s,\"util_pct\":%s,\"mem_used_mb\":%s},"
           "\"t\":%.6f,\"frame\":%u,\"boot_id\":\"%s\",\"ready\":%s,%s"
           "\"startup_ms\":{\"first_capture\":%.0f,\"first_frame_sent\":%.0f},"
           "\"phone\":{\"picture\":\"%s\",\"screen\":[%d,%d],\"copies\":%llu,\"scene_frames\":%llu,\"screen_frames\":%llu,"
           "\"implausible\":%llu,\"odd\":%llu,\"repeats\":%llu,\"verified\":%s,\"screen_error\":%s,"
           "\"screen_connects\":%llu,"
           "\"app_connects\":%llu,\"app_error\":%s,"
           "\"draw_to_capture_ms\":%s,\"capture_to_sent_ms\":%s,\"encode_ms\":%s}}",
           fps_, geo_.h, sj.c_str(), json_escape(gs.name.empty() ? gpu_name_ : gs.name).c_str(),
           num(gs.temp_c).c_str(), num(gs.power_w).c_str(), num(gs.util_pct).c_str(), num(gs.mem_used_mb).c_str(),
           t_, frame_, boot_id_.c_str(), ready_ && fatal_.empty() ? "true" : "false",
           fatal_.empty() ? "" : ("\"error\":\"" + json_escape(fatal_) + "\",").c_str(), first_capture_ms_, first_sent_ms_,
           frame_ ? (geo_.scene ? "scene" : "screen") : "none", screen_w_, screen_h_, (unsigned long long)copies_,
           (unsigned long long)gate_.scene, (unsigned long long)gate_.screen, (unsigned long long)implausible_, (unsigned long long)gate_.odd, (unsigned long long)gate_.repeats,
           verified_ ? "true" : "false",
           screen_error_.empty() ? "null" : ("\"" + json_escape(screen_error_) + "\"").c_str(),
           (unsigned long long)app_screen_connects_,
           (unsigned long long)app_connects_,
           app_error_.empty() ? "null" : ("\"" + json_escape(app_error_) + "\"").c_str(), draw_to_capture_.json().c_str(),
           capture_to_sent_.json().c_str(), encode_ms_.json().c_str());
  std::string h = b;
  char m[512];
  snprintf(m, sizeof m,
           ",\"mirror\":{\"open\":%s,\"opened\":%llu,\"behind\":%llu,\"tap_bytes\":%llu,\"wire_bytes\":%llu,\"error\":%s}}",
           mirror_open_ ? "true" : "false", (unsigned long long)mirror_opened_, (unsigned long long)mirror_behind_,
           (unsigned long long)mirror_tap_bytes_, (unsigned long long)mirror_wire_bytes_,
           mirror_error_.empty() ? "null" : ("\"" + json_escape(mirror_error_) + "\"").c_str());
  return h.substr(0, h.size() - 1) + m;
}
