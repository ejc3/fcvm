// The Windows bridge's frame source: the desktop FreeRDP composes (rdp.h), encoded with x264
// while anyone watches and served through the renderer's server as its /frames stream, with
// /health, /still, /scene and the fixed actions of /action (fleet/PROTOCOL.md, "Windows
// bridge").
#pragma once
#include <stdint.h>

#include <condition_variable>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

#include "logic.h"
#include "rdp.h"
#include "server.h"
#include "subscriber.h"
#include "x264enc.h"

struct BridgeOptions {
  RdpConfig rdp;
  int fps = 30;
  std::string desktop_ref;      // DESKTOP_REF: the desktop's reference picture
  std::string desktop_ref_out;  // DESKTOP_REF_OUT: where a settled desktop's thumbnail goes
  // logon: the desktop is the settled one whatever DESKTOP_REF says, after settle_ms
  bool logon = false;
  double settle_ms = SETTLE_MS;
};

class Bridge : public FrameSource, public RdpSink {
 public:
  explicit Bridge(BridgeOptions o);
  RdpLink& link() { return link_; }
  // The encoder thread: encodes the surface while a stream is open. Never returns.
  void encode_loop();
  // logon: waits until the desktop is found or the monotonic deadline passes.
  bool wait_desktop(double deadline_mono_ms);
  // {"desktop_by": .., "times_unix_ms": {..}, "connects": n, "attempts": n}
  std::string times_json();
  // The failure a fatal error left, "" while none.
  std::string fatal();

  // FrameSource
  std::shared_ptr<Subscriber> subscribe(int h, int q) override;
  void unsubscribe(const std::shared_ptr<Subscriber>& s) override;
  void wake() override;
  bool post_scene(const std::string& body, std::string* err) override;
  bool post_budget(const std::string& body, std::string* err) override;
  bool raw_frame(double, std::string*, std::string*) override { return false; }
  bool next_frame(int still_h, std::vector<uint8_t>* rgba, int* w, int* h, std::string* tile_json) override;
  // The desktop is private; fleet viewers must use the admitted /frames stream.
  bool allows_fleet_still() const override { return false; }
  std::string health_json() override;
  bool admits(const std::string& token) override;
  bool requires_admit() override;
  bool has_scene() override;
  bool serves_action() override { return true; }
  int post_action(const std::string& body, std::string* reply) override;

  // RdpSink
  void on_attempt(double unix_ms) override;
  void on_tcp(double unix_ms) override;
  void on_rdp(double unix_ms) override;
  void on_frame(const uint8_t* bgrx, int w, int h, size_t stride, double unix_ms, bool gfx) override;
  void on_gfx_command(uint32_t codec_id) override;
  void on_tick(double unix_ms) override;
  void on_down(const std::string& why, bool fatal, bool was_connected) override;

 private:
  struct Enc {
    std::unique_ptr<X264Encoder> enc;
    int key = 0;  // the header's key: changes with every reopen
    double last_idr_ms = -1e18;
    bool want_idr = true;
  };
  // Sends the action's steps; the HTTP status, with the reply.
  int play(const Action& a, std::string* reply);
  Thumb current_thumb();
  // under hmu_: a desktop the detector just found
  void desktop_found_locked();
  // Encodes the I420 picture for every quality a stream wants; false when nobody wants one.
  void send(double t, double stamp_unix_ms);

  BridgeOptions opt_;
  RdpLink link_;
  const std::string boot_id_;
  // the surface, under smu_
  std::mutex smu_;
  std::vector<uint8_t> surf_;
  int surf_w_ = 0, surf_h_ = 0;
  uint64_t surf_seq_ = 0;
  double surf_ms_ = 0;  // the host time of its last frame
  // streams and the scene, under mu_
  std::mutex mu_;
  std::condition_variable cv_;
  std::vector<std::shared_ptr<Subscriber>> subs_;
  int next_id_ = 1;
  BridgeScene scene_;
  uint64_t scene_version_ = 0;
  std::vector<std::string> admit_keys_;
  // the encoder thread's own
  std::map<int, Enc> encs_;
  int next_key_ = 1;
  std::vector<uint8_t> i420_;
  int enc_w_ = 0, enc_h_ = 0;
  uint32_t frame_no_ = 0;
  // one action at a time
  std::mutex action_mu_;
  // health and the desktop, under hmu_
  mutable std::mutex hmu_;
  std::condition_variable hcv_;
  DesktopDetector detector_;
  WindowsTimes times_;
  uint64_t connects_ = 0, attempts_ = 0;
  bool connected_ = false;
  std::string fatal_, last_error_;
  Durations encode_ms_;
  double fps_ = 0, last_t_ = 0, last_stamp_ms_ = -1, first_sent_ms_ = -1;
  uint32_t sent_frame_ = 0;  // the frame number of the newest frame sent
  uint64_t gfx_frames_ = 0, paint_frames_ = 0;
  std::map<uint32_t, uint64_t> gfx_codecs_;
};
