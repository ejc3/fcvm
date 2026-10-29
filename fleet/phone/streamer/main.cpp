// phone-streamer: serves a phone's screen as the renderer's frame stream (PROTOCOL.md,
// "Phone streamer"). Environment: PORT (8080), FPS (30), EMU_GRPC (the emulator's gRPC
// controller, 127.0.0.1:8554), PHONE_GO_FILE (stream nothing until it exists),
// PHONE_APP_PORT (7070, the app's control socket on this container's loopback), TLS_CERT_FILE,
// TLS_PORT (443) and SCENE_TOKEN_FILE as for the renderer (its PROTOCOL.md "Access": with a
// token file, scene posts need the token and streams wait for the first scene).
//
//   phone-streamer [serve]          serve
//   phone-streamer fail <message>   serve /health only, reporting the phone failed with
//                                   <message> (the phone's start found no GPU renderer)
//   phone-streamer capture <secs> <file>
//                                   write the screen as raw NV12 frames at FPS for <secs>
//                                   (the newest frame at each tick, repeated while the screen
//                                   does not change); prints the frames written and the
//                                   screen updates received. For encoding offline.
#include <stdio.h>
#include <stdlib.h>

#include <chrono>
#include <vector>

#include <string>
#include <thread>

#include "nv12.h"
#include "screen.h"
#include "server.h"
#include "source.h"
#include "tls.h"

static double envd(const char* k, double def) {
  const char* v = getenv(k);
  return v && *v ? atof(v) : def;
}

static int capture(int argc, char** argv) {
  if (argc < 4) {
    fprintf(stderr, "usage: phone-streamer capture <secs> <file>\n");
    return 2;
  }
  double secs = atof(argv[2]);
  int fps = (int)envd("FPS", 30);
  const char* target = getenv("EMU_GRPC") ? getenv("EMU_GRPC") : "127.0.0.1:8554";
  FILE* out = fopen(argv[3], "wb");
  if (!out) {
    perror(argv[3]);
    return 1;
  }
  ScreenStream screen(target);
  screen.start();
  ScreenFrame f;
  uint64_t seq = 0;
  if (!screen.next(0, 10000, &f)) {
    fprintf(stderr, "phone-streamer: no screen frame in 10 s: %s\n", screen.error().c_str());
    return 1;
  }
  seq = f.seq;
  uint64_t first_seq = f.seq;
  std::vector<uint8_t> nv12((size_t)f.width * f.height * 3 / 2);
  int w = f.width, h = f.height;
  auto t0 = std::chrono::steady_clock::now();
  long frames = (long)(secs * fps);
  for (long i = 0; i < frames; i++) {
    auto tick = t0 + std::chrono::microseconds((long long)(i * 1e6 / fps));
    auto wait_ms = std::chrono::duration_cast<std::chrono::milliseconds>(tick - std::chrono::steady_clock::now()).count();
    ScreenFrame n;
    if (screen.next(seq, wait_ms > 0 ? (int)wait_ms : 0, &n) && n.width == w && n.height == h) {
      f = std::move(n);
      seq = f.seq;
    }
    std::this_thread::sleep_until(tick);
    rgba_to_nv12(f.rgba.data(), (size_t)w * 4, w, h, nv12.data(), (size_t)w);
    if (fwrite(nv12.data(), 1, nv12.size(), out) != nv12.size()) {
      perror("write");
      return 1;
    }
  }
  fclose(out);
  double el = std::chrono::duration<double>(std::chrono::steady_clock::now() - t0).count();
  printf("{\"frames\":%ld,\"fps\":%d,\"width\":%d,\"height\":%d,\"screen_updates\":%llu,\"seconds\":%.3f}\n", frames,
         fps, w, h, (unsigned long long)(seq - first_seq + 1), el);
  return 0;
}

int main(int argc, char** argv) {
  std::string cmd = argc > 1 ? argv[1] : "serve";
  if (cmd == "capture") return capture(argc, argv);
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
    PhoneSource failed(StreamerOptions{o.screen_target, o.fps, "GPU: " + err, "", o.app_port});
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
