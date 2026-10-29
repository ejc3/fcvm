// HTTP/1.1 and WebSocket server: GET /health, POST /scene, GET /frames?h=,
// GET /still, GET /raw. Plain on one port; with a TlsFront, also HTTPS/WSS
// on another.
#pragma once
#include "access.h"
#include <atomic>
#include <memory>
#include <string>
#include <vector>

#include "subscriber.h"
#include "tls.h"

// What the server serves: the renderer's Pipeline, or the phone streamer
// (fleet/phone/streamer), which reads an emulator's screen instead of drawing.
class FrameSource {
 public:
  virtual ~FrameSource() = default;
  virtual std::shared_ptr<Subscriber> subscribe(int h, int q) = 0;
  virtual void unsubscribe(const std::shared_ptr<Subscriber>& s) = 0;
  virtual void wake() = 0;
  virtual bool post_scene(const std::string& body, std::string* err) = 0;
  virtual bool post_budget(const std::string& body, std::string* err) = 0;
  virtual bool raw_frame(double t, std::string* ppm, std::string* tile_json) = 0;
  virtual bool next_frame(int still_h, std::vector<uint8_t>* rgba, int* w, int* h, std::string* tile_json) = 0;
  virtual std::string health_json() = 0;
  // Whether a GET /frames with this direct token may stream.
  virtual bool admits(const std::string& token) = 0;
  // Whether a stream must show a direct token before it is served (a scene brought keys).
  virtual bool requires_admit() = 0;
  // Whether any scene has been posted (a fleet renderer streams nothing before one).
  virtual bool has_scene() = 0;
  // GET /mirror, the phone streamer's GPU mirror (fleet/PROTOCOL.md, "Phone streamer"): a
  // source that serves it says whether a first message's token opens it, and runs an opened
  // stream on the connection until it ends. The renderer serves none.
  virtual bool serves_mirror() { return false; }
  virtual bool mirror_admits(const std::string& token) { return false; }
  virtual void mirror(Conn& c, const std::string& token) {}
};

// One masked client frame of at most 64 KiB: its opcode and unmasked payload.
bool read_ws_message(Conn& c, int* op, std::string* pl);

class Server {
 public:
  explicit Server(FrameSource& p) : p_(p) {}
  // A fleet renderer (access.h): scene posts need the token, streams wait for a scene.
  void set_scene_token(SceneToken* t) { token_ = t; }
  // Binds and listens; returns false with a message. Then serve() accepts forever.
  bool listen(int port, std::string* err);
  bool listen_tls(int port, TlsFront* tls, std::string* err);
  void serve();

 private:
  int open_listener(int port, std::string* err);
  void accept_loop(int lfd, bool tls);
  void handle(Conn& c);
  void websocket(Conn& c, int h, int q, const std::string& key);
  FrameSource& p_;
  int lfd_ = -1, tfd_ = -1;
  TlsFront* tls_ = nullptr;
  SceneToken* token_ = nullptr;
  bool fleet() const { return token_ && token_->configured(); }
  std::atomic<unsigned long long> refused_{0};  // connections refused for want of a thread (both accept loops)
};
