// HTTP/1.1 and WebSocket server: GET /health, POST /scene, GET /frames?h=,
// GET /still, GET /raw (the last two need the scene token on a fleet server). Plain on one port; with a TlsFront, also HTTPS/WSS
// on another.
#pragma once
#include "access.h"
#include <atomic>
#include <functional>
#include <map>
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
  // Whether the public fleet listener may serve a poster without a viewer token.
  virtual bool allows_fleet_still() const { return true; }
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
  // POST /action, the Windows bridge's fixed input sequences (fleet/PROTOCOL.md, "Windows
  // bridge"): a source that serves it answers each body with an HTTP status and a JSON reply.
  // The route needs the scene token as POST /scene does. The renderer serves none.
  virtual bool serves_action() { return false; }
  virtual int post_action(const std::string& body, std::string* reply) { return 404; }
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
  friend struct ServerTest;
  int open_listener(int port, std::string* err);
  // Accepts until the listener is shut down.
  void accept_loop(int lfd, bool tls);
  // An accepted connection from `peer`: takes its pending place and starts its thread, or
  // closes it (refused) when there is no place (fleet/PROTOCOL.md, "Connection limits") or no
  // thread. Returns whether it started.
  bool start_connection(int fd, const sockaddr* peer, socklen_t peer_len, bool tls);
  // One request. `pending` (null: none) is given back once the connection is admitted to a
  // stream or a mirror; `request_ms` bounds the wait for the whole request header.
  void handle(Conn& c, PendingPlace* pending = nullptr, int request_ms = REQUEST_WAIT_MS);
  void websocket(Conn& c, int h, int q, const std::string& key, PendingPlace* pending);
  FrameSource& p_;
  int lfd_ = -1, tfd_ = -1;
  TlsFront* tls_ = nullptr;
  SceneToken* token_ = nullptr;
  bool fleet() const { return token_ && token_->configured(); }
  // A fleet server's GET /raw and /still (debug and poster routes, which no viewer uses) need the
  // scene token as POST /scene does: 0 to go ahead, else the response already sent.
  int refuse_without_scene_token(Conn& c, std::map<std::string, std::string>& hdr);
  std::atomic<unsigned long long> refused_{0};  // connections refused for want of a thread (both accept loops)
  StreamTable streams_{MAX_FLEET_STREAMS};  // a fleet server's open frame streams
  // Connections that have reached no stream, on both listeners, and those refused past its caps.
  PendingTable pending_{MAX_PENDING_CONNECTIONS, MAX_PENDING_PER_SOURCE};
  std::atomic<unsigned long long> refused_pending_{0};
  // Tests: run between a refusal's close and its count, whichever comes first.
  std::function<void()> refuse_hook_;
  // Tests: shorter waits.
  int request_wait_ms_ = REQUEST_WAIT_MS;
  int admit_wait_ms_ = ADMIT_WAIT_MS;
};
