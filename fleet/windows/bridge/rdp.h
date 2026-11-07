// The bridge's Remote Desktop client: FreeRDP 3, headless, its GDI composing every graphics
// command (the graphics pipeline's H.264 included) into one 32-bit BGRX surface. One link
// connects to the sandbox's rdp.sock and reconnects for ever (Backoff) until stopped: the bridge
// starts before the sandbox restores.
#pragma once
#include <stdint.h>

#include <atomic>
#include <condition_variable>
#include <mutex>
#include <string>
#include <vector>

#include "logic.h"

struct RdpConfig {
  std::string socket;         // the sandbox's rdp.sock (an absolute path: FreeRDP connects to it as a Unix socket)
  std::string user = "sandbox";
  std::string password_file;  // read at every attempt; never logged
  int width = 1280, height = 720;
};

// What the link reports, from FreeRDP's threads.
class RdpSink {
 public:
  virtual ~RdpSink() = default;
  virtual void on_attempt(double unix_ms) = 0;
  virtual void on_tcp(double unix_ms) = 0;
  virtual void on_rdp(double unix_ms) = 0;
  // The surface after a frame the server ended (an rdpgfx EndFrame, or a GDI EndPaint outside
  // the graphics pipeline): the whole desktop, BGRX.
  virtual void on_frame(const uint8_t* bgrx, int w, int h, size_t stride, double unix_ms, bool gfx) = 0;
  // An rdpgfx surface command arrived with this codec id (RDPGFX_CODECID_*).
  virtual void on_gfx_command(uint32_t codec_id) {}
  // About ten times a second while the link runs, connected or not.
  virtual void on_tick(double unix_ms) {}
  // An attempt failed or a connection ended. fatal: retrying cannot help, and the link stops.
  virtual void on_down(const std::string& why, bool fatal, bool was_connected) = 0;
};

class RdpLink {
 public:
  RdpLink(RdpConfig c, RdpSink* sink);
  ~RdpLink();
  RdpLink(const RdpLink&) = delete;
  RdpLink& operator=(const RdpLink&) = delete;
  // Connects, serves the connection, and reconnects after 100 ms doubling to 2 s, until stop()
  // or a fatal error. Returns then, disconnected.
  void run();
  // Ends run() from any thread; an open connection is closed with a disconnect, so Windows
  // keeps the session (Disconnected).
  void stop();
  bool connected() const { return connected_.load(); }
  // Sends the events in order from the connection's thread and returns once they are sent;
  // false when there is no connection, another batch is pending, a send fails, the connection
  // ends first, or 5 s pass (the batch is then dropped if the connection's thread has not
  // taken it). Only Bridge::play calls it, with events from step_keys().
  bool send_keys(const std::vector<KeyEvent>& ev, std::string* err);
  // Whether this FreeRDP was built with H.264 for the graphics pipeline.
  static bool built_with_h264();

  // FreeRDP's callbacks (rdp.cpp) reach the link through these.
  struct Hooks;
  friend struct Hooks;

 private:
  // One attempt; the error, "" when it connected and was stopped. *reached: RDP connected.
  std::string attempt(bool* reached, bool* fatal);
  void drain_keys(void* context);
  void fail_keys(const std::string& why);
  // A refusal that retrying cannot help (the channel check, the desktop size, a redirection),
  // from whichever FreeRDP thread finds it; attempt() ends the link with it.
  void set_fatal(const std::string& why);
  std::string fatal_reason();

  RdpConfig cfg_;
  RdpSink* sink_;
  std::atomic<bool> stop_{false};
  std::atomic<bool> connected_{false};
  std::mutex mu_;
  std::condition_variable cv_;
  // winpr event: input queued or stop. Made in the constructor and closed in the destructor,
  // so stop() and run() read it on any thread, in any order.
  void* const wake_;
  // under mu_
  KeyQueue keyq_;
  std::string fatal_;
  // per connection (its thread)
  void* gfx_end_frame_ = nullptr;   // the GDI's rdpgfx EndFrame, which the link wraps
  void* gfx_surface_command_ = nullptr;  // and its SurfaceCommand, to count codecs
  void* tcp_connect_ = nullptr;     // FreeRDP's own TCP connect, which the link wraps
};
