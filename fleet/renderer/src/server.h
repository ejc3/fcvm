// HTTP/1.1 and WebSocket server: GET /health, POST /scene, GET /frames?h=.
#pragma once
#include <string>

#include "pipeline.h"

class Server {
 public:
  explicit Server(Pipeline& p) : p_(p) {}
  // Binds and listens; returns false with a message. Then serve() accepts forever.
  bool listen(int port, std::string* err);
  void serve();

 private:
  void handle(int fd);
  void websocket(int fd, int h, const std::string& key);
  Pipeline& p_;
  int lfd_ = -1;
};
