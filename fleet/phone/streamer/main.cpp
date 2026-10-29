// phone-streamer: serves a phone's screen as the renderer's frame stream (PROTOCOL.md,
// "Phone streamer"). Environment: PORT (8080), FPS (30), EMU_GRPC (the emulator's gRPC
// controller, 127.0.0.1:8554), PHONE_GO_FILE (stream nothing until it exists),
// PHONE_APP_PORT (7070, the app's control socket on this container's loopback), TLS_CERT_FILE
// and TLS_PORT (443) as for the renderer.
//
//   phone-streamer [serve]          serve
//   phone-streamer fail <message>   serve /health only, reporting the phone failed with
//                                   <message> (the phone's start found no GPU renderer)
#include <stdio.h>
#include <stdlib.h>

#include <string>
#include <thread>

#include "server.h"
#include "source.h"
#include "tls.h"

static double envd(const char* k, double def) {
  const char* v = getenv(k);
  return v && *v ? atof(v) : def;
}

int main(int argc, char** argv) {
  std::string cmd = argc > 1 ? argv[1] : "serve";
  StreamerOptions o;
  if (cmd == "fail") {
    o.fail = argc > 2 ? argv[2] : "the phone failed";
  } else if (cmd != "serve") {
    fprintf(stderr, "usage: phone-streamer [serve] | fail <message>\n");
    return 2;
  }
  int port = (int)envd("PORT", 8080);
  o.fps = (int)envd("FPS", 30);
  if (const char* s = getenv("EMU_GRPC")) o.screen_target = s;
  if (const char* s = getenv("PHONE_GO_FILE")) o.go_file = s;
  o.app_port = (int)envd("PHONE_APP_PORT", 7070);
  PhoneSource p(o);
  std::string err;
  if (o.fail.empty() && !p.init(&err)) {
    fprintf(stderr, "phone-streamer: GPU: %s\n", err.c_str());
    PhoneSource failed(StreamerOptions{o.screen_target, o.fps, "GPU: " + err, "", o.app_port});
    Server s(failed);
    if (!s.listen(port, &err)) return 1;
    s.serve();
    return 1;
  }
  Server s(p);
  if (!s.listen(port, &err)) {
    fprintf(stderr, "phone-streamer: %s\n", err.c_str());
    return 1;
  }
  TlsFront tls;
  const char* cert_file = getenv("TLS_CERT_FILE");
  if (cert_file && *cert_file) {
    int tport = (int)envd("TLS_PORT", 443);
    if (!tls.load(cert_file, &err)) fprintf(stderr, "phone-streamer: TLS certificate not loaded yet: %s\n", err.c_str());
    tls.watch(cert_file, 2000);
    if (!s.listen_tls(tport, &tls, &err)) {
      fprintf(stderr, "phone-streamer: %s\n", err.c_str());
      return 1;
    }
  }
  fprintf(stderr, "phone-streamer: listening on :%d, screen from %s\n", port, o.screen_target.c_str());
  std::thread rt([&] { p.run(); });
  std::thread al([&] { p.app_link(); });
  al.detach();
  s.serve();
  rt.join();
  return 1;
}
