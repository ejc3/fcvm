// HTTP/1.1 and WebSocket server: GET /health, POST /scene, GET /frames?h=,
// GET /still, GET /raw. Plain on one port; with a TlsFront, also HTTPS/WSS
// on another.
#pragma once
#include "access.h"
#include <atomic>
#include <string>

#include "pipeline.h"
#include "tls.h"

class Server {
 public:
  explicit Server(Pipeline& p) : p_(p) {}
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
  Pipeline& p_;
  int lfd_ = -1, tfd_ = -1;
  TlsFront* tls_ = nullptr;
  SceneToken* token_ = nullptr;
  bool fleet() const { return token_ && token_->configured(); }
  std::atomic<unsigned long long> refused_{0};  // connections refused for want of a thread (both accept loops)
};
