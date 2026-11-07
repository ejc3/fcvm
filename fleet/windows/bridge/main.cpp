// windows-bridge: serves a Windows sandbox's desktop as the renderer's frame stream
// (fleet/PROTOCOL.md, "Windows bridge"). Environment: PORT (8080), FPS (30), RDP_SOCKET (the
// sandbox's rdp.sock), RDP_USER (sandbox), RDP_PASSWORD_FILE, RDP_WIDTH (1280), RDP_HEIGHT
// (720), DESKTOP_REF, DESKTOP_REF_OUT, TLS_CERT_FILE, TLS_PORT (443) and SCENE_TOKEN_FILE as
// for the renderer (its PROTOCOL.md "Access"), LOGON_DEADLINE_SECS (300, logon only).
//
//   windows-bridge [serve]                  serve
//   windows-bridge logon [--settle-secs N]  connect, wait until the desktop has stood still for
//                                           N s (5 by default), write its thumbnail to DESKTOP_REF_OUT, print
//                                           the times as one JSON line, disconnect (the session
//                                           stays logged on, Disconnected) and exit 0; exit 1
//                                           at the deadline
//   windows-bridge check-build              exit 0 when FreeRDP has H.264 for the graphics pipeline
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#include <string>
#include <thread>

#include "bridge.h"
#include "server.h"
#include "tls.h"

static double envd(const char* k, double def) {
  const char* v = getenv(k);
  return v && *v ? atof(v) : def;
}
static std::string envs(const char* k, const char* def) {
  const char* v = getenv(k);
  return v && *v ? v : def;
}

static int usage() {
  fprintf(stderr, "usage: windows-bridge [serve] | logon [--settle-secs N] | check-build\n");
  return 2;
}

// As a container's PID 1 the process gets no default action for SIGTERM, so `podman stop`
// would wait out its timeout and kill it. The sandbox behind it is discarded with it, so
// nothing needs an orderly close: exit at once.
static void on_term(int) { _exit(0); }

int main(int argc, char** argv) {
  struct sigaction sa = {};
  sa.sa_handler = on_term;
  sigaction(SIGTERM, &sa, nullptr);
  sigaction(SIGINT, &sa, nullptr);
  std::string cmd = argc > 1 ? argv[1] : "serve";
  if (cmd == "check-build") {
    bool ok = RdpLink::built_with_h264();
    printf("FreeRDP H.264 for the graphics pipeline: %s\n", ok ? "yes" : "no");
    return ok ? 0 : 1;
  }
  BridgeOptions o;
  if (cmd == "logon") {
    if (argc != 2 && (argc != 4 || std::string(argv[2]) != "--settle-secs" || atof(argv[3]) <= 0)) return usage();
    o.logon = true;
    o.settle_ms = (argc == 4 ? atof(argv[3]) : 5.0) * 1000.0;
  } else if (cmd != "serve" || argc > 2) {
    return usage();
  }
  o.rdp.socket = envs("RDP_SOCKET", "");
  o.rdp.user = envs("RDP_USER", "sandbox");
  o.rdp.password_file = envs("RDP_PASSWORD_FILE", "");
  o.rdp.width = (int)envd("RDP_WIDTH", 1280);
  o.rdp.height = (int)envd("RDP_HEIGHT", 720);
  o.fps = (int)envd("FPS", 30);
  o.desktop_ref = envs("DESKTOP_REF", "");
  o.desktop_ref_out = envs("DESKTOP_REF_OUT", "");
  if (o.rdp.socket.empty() || o.rdp.socket[0] != '/') {
    fprintf(stderr, "windows-bridge: RDP_SOCKET must be the absolute path of the sandbox's rdp.sock\n");
    return 2;
  }
  if (o.rdp.width < 200 || o.rdp.width > 4096 || o.rdp.height < 200 || o.rdp.height > 2160 || (o.rdp.width & 1) ||
      (o.rdp.height & 1)) {
    fprintf(stderr, "windows-bridge: RDP_WIDTH 200..4096 and RDP_HEIGHT 200..2160, both even\n");
    return 2;
  }
  if (o.fps < 1 || o.fps > 60) {
    fprintf(stderr, "windows-bridge: FPS 1..60\n");
    return 2;
  }
  if (!RdpLink::built_with_h264())
    fprintf(stderr, "windows-bridge: this FreeRDP has no H.264: Windows will send its other codecs\n");

  Bridge b(o);
  if (o.logon) {
    double deadline = mono_ms_now() + envd("LOGON_DEADLINE_SECS", 300) * 1000.0;
    std::thread lt([&] { b.link().run(); });
    bool ok = b.wait_desktop(deadline);
    b.link().stop();
    lt.join();
    std::string fatal = b.fatal();
    printf("%s\n", b.times_json().c_str());
    fflush(stdout);
    if (!ok) {
      fprintf(stderr, "windows-bridge: logon: %s\n", fatal.empty() ? "no settled desktop before the deadline" : fatal.c_str());
      return 1;
    }
    return 0;
  }

  int port = (int)envd("PORT", 8080);
  std::string err;
  SceneToken scene_token;
  scene_token.configure_from_env();
  Server s(b);
  if (scene_token.configured()) s.set_scene_token(&scene_token);
  if (!s.listen(port, &err)) {
    fprintf(stderr, "windows-bridge: %s\n", err.c_str());
    return 1;
  }
  TlsFront tls;
  const char* cert_file = getenv("TLS_CERT_FILE");
  if (cert_file && *cert_file) {
    int tport = (int)envd("TLS_PORT", 443);
    if (!tls.load(cert_file, &err)) fprintf(stderr, "windows-bridge: TLS certificate not loaded yet: %s\n", err.c_str());
    tls.watch(cert_file, 2000);
    if (!s.listen_tls(tport, &tls, &err)) {
      fprintf(stderr, "windows-bridge: %s\n", err.c_str());
      return 1;
    }
  }
  fprintf(stderr, "windows-bridge: listening on :%d, desktop from %s\n", port, o.rdp.socket.c_str());
  std::thread lt([&] { b.link().run(); });
  std::thread et([&] { b.encode_loop(); });
  et.detach();
  lt.detach();
  s.serve();
  return 1;
}
