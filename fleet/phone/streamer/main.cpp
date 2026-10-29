// phone-streamer: serves a phone's screen as the renderer's frame stream (PROTOCOL.md,
// "Phone streamer"). Environment: PORT (8080), FPS (30), EMU_GRPC (the emulator's gRPC
// controller, 127.0.0.1:8554), PHONE_GO_FILE (stream nothing until it exists),
// PHONE_APP_PORT (7070, the app's control socket on this container's loopback), MIRROR_SOCKET
// (the emulator's command-stream tap, /tmp/gfxstream-mirror.sock), TLS_CERT_FILE,
// TLS_PORT (443) and SCENE_TOKEN_FILE as for the renderer (its PROTOCOL.md "Access": with a
// token file, scene posts need the token and streams wait for the first scene).
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
  if (const char* s = getenv("MIRROR_SOCKET")) o.mirror_socket = s;
  PhoneSource p(o);
  std::string err;
  TlsFront tls;
  // The plain listener, and the TLS one when a certificate is configured: a source that failed
  // to start serves its /health on both, as a running one does.
  SceneToken scene_token;
  scene_token.configure_from_env();
  auto listen = [&](Server& s) {
    if (scene_token.configured()) s.set_scene_token(&scene_token);
    if (!s.listen(port, &err)) {
      fprintf(stderr, "phone-streamer: %s\n", err.c_str());
      return false;
    }
    const char* cert_file = getenv("TLS_CERT_FILE");
    if (cert_file && *cert_file) {
      int tport = (int)envd("TLS_PORT", 443);
      if (!tls.load(cert_file, &err)) fprintf(stderr, "phone-streamer: TLS certificate not loaded yet: %s\n", err.c_str());
      tls.watch(cert_file, 2000);
      if (!s.listen_tls(tport, &tls, &err)) {
        fprintf(stderr, "phone-streamer: %s\n", err.c_str());
        return false;
      }
    }
    return true;
  };
  if (o.fail.empty() && !p.init(&err)) {
    fprintf(stderr, "phone-streamer: GPU: %s\n", err.c_str());
    PhoneSource failed(StreamerOptions{o.screen_target, o.fps, "GPU: " + err, "", o.app_port, o.mirror_socket});
    Server s(failed);
    if (!listen(s)) return 1;
    s.serve();
    return 1;
  }
  Server s(p);
  if (!listen(s)) return 1;
  fprintf(stderr, "phone-streamer: listening on :%d, screen from %s\n", port, o.screen_target.c_str());
  std::thread rt([&] { p.run(); });
  std::thread al([&] { p.app_link(); });
  al.detach();
  s.serve();
  rt.join();
  return 1;
}
