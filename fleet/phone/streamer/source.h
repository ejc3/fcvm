// The phone streamer's frame source: reads the emulator's screen from its gRPC controller, keeps the app's stamped scene frames (or the whole screen when the app's interface
// shows), converts to NV12, encodes with NVENC on the VM's GPU and serves the renderer's
// /frames protocol through the renderer's server.
#pragma once
#include <stdint.h>

#include <condition_variable>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

#include "gpustats.h"
#include "logic.h"
#include "nvenc.h"
#include "server.h"
#include "screen.h"
#include "subscriber.h"

struct StreamerOptions {
  std::string screen_target = "127.0.0.1:8554";  // the emulator's gRPC controller
  int fps = 30;
  std::string fail;        // set: the phone failed before streaming (reported in /health, nothing streams)
  // Nothing is captured or streamed until this file exists (phone.sh writes it once the
  // phone renders on the VM's GPU); empty: stream at once.
  std::string go_file;
  int app_port = 7070;  // the app's control socket, forwarded to this container's loopback
};

class PhoneSource : public FrameSource {
 public:
  explicit PhoneSource(const StreamerOptions& o);
  // CUDA's context on device 0 and NVENC's library. False with a message.
  bool init(std::string* err);
  void run();  // the capture thread; returns only on a fatal error

  std::shared_ptr<Subscriber> subscribe(int h, int q) override;
  void unsubscribe(const std::shared_ptr<Subscriber>& s) override;
  void wake() override;
  bool post_scene(const std::string& body, std::string* err) override;
  bool post_budget(const std::string& body, std::string* err) override;
  bool raw_frame(double t, std::string* ppm, std::string* tile_json) override { return false; }
  bool next_frame(int still_h, std::vector<uint8_t>* rgba, int* w, int* h, std::string* tile_json) override;
  std::string health_json() override;
  bool admits(const std::string& token) override;
  bool requires_admit() override;
  bool has_scene() override;
  // The app link (a thread of its own): passes the scene and the host's clock to the app.
  void app_link();

 private:
  struct Enc {
    std::unique_ptr<H264Encoder> enc;
    uint64_t nv12 = 0;  // CUdeviceptr
    size_t pitch = 0;
    PhoneGeo geo;
    int key = 0;  // the header's key: changes with every reopen
    double last_idr = -1e9;
    bool want_idr = true;
  };
  bool open_enc(Enc& e, int q, const PhoneGeo& g, std::string* err);
  void close_enc(Enc& e);
  // Encodes and queues one frame; false after a fatal error (the capture thread then stops).
  bool send(const PhoneGeo& g, double t, double ts_ms);
  void set_fatal(const std::string& m);

  StreamerOptions opt_;
  void* ctx_ = nullptr;  // CUcontext
  std::string gpu_name_, bus_id_;
  GpuStats gpu_;
  ScreenStream screen_;
  std::mutex mu_;
  std::condition_variable cv_;
  PhoneScene scene_;  // under mu_
  AdmitKeys admit_;   // under mu_
  uint64_t scene_version_ = 0;  // under mu_: bumped by every post
  std::vector<std::shared_ptr<Subscriber>> subs_;  // under mu_
  int next_id_ = 1;
  bool woken_ = false;  // under mu_
  std::map<int, Enc> encs_;  // by quality; capture thread
  int next_key_ = 1;
  std::vector<uint8_t> nv12_host_;  // the frame being sent, NV12, pitch = width
  std::vector<uint8_t> last_rgba_;  // the last screen copied, for GET /still (under hmu_)
  int last_w_ = 0, last_h_ = 0;
  FrameGate gate_;
  uint32_t frame_no_ = 0;
  // health, under hmu_
  mutable std::mutex hmu_;
  std::string fatal_;
  bool ready_ = false;
  double t_ = 0;
  uint32_t frame_ = 0;
  double fps_ = 0;
  PhoneGeo geo_;
  int screen_w_ = 0, screen_h_ = 0;
  uint64_t copies_ = 0;
  Samples draw_to_capture_, capture_to_sent_, encode_ms_;
  double proc_start_ms_ = 0, first_capture_ms_ = -1, first_sent_ms_ = -1;
  std::string screen_error_;
  uint64_t app_screen_connects_ = 0;
  uint64_t implausible_ = 0;   // stamps naming a time far from the copy's
  uint64_t app_connects_ = 0;  // connections to the app's control socket
  std::string app_error_;      // the app's last refusal
  bool verified_ = false;  // PHONE_GO_FILE seen: the phone renders on the GPU
  const std::string boot_id_;
};
