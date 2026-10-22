// fleet-renderer: renders one rectangle of the fleet's shared planet and
// streams it as H.264 over WebSocket. See fleet/PROTOCOL.md.
#include <unistd.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <string>
#include <thread>
#include <vector>

#include "nvenc.h"
#include "pipeline.h"
#include "render.h"
#include "scene.h"
#include "server.h"

static void usage() {
  fprintf(stderr,
          "usage: fleet-renderer [serve]                      serve on $PORT (8080)\n"
          "       fleet-renderer still OUT.ppm [--t T] [--h H] [--scene FILE] [--spp N] [--frames N | --seq N]\n"
          "       fleet-renderer bench [--h H]... [--frames N]   time render + encode per height\n"
          "       fleet-renderer test [H]                       the seam test (whole image vs 2x2 tiles)\n"
          "       fleet-renderer startup [H]                    ms from process start to GPU ready, first frame, first encode\n"
          "       fleet-renderer sleep                          wait forever without using the GPU\n"
          "env: PORT, FPS (30), BUDGET pixel-samples per frame (500000), ADAPT (0), ACCUMULATE (1)\n");
}

static double envd(const char* k, double d) {
  const char* v = getenv(k);
  return v && *v ? atof(v) : d;
}

static std::string read_file(const std::string& p) {
  std::ifstream f(p);
  if (!f) throw std::runtime_error("cannot read " + p);
  return std::string((std::istreambuf_iterator<char>(f)), std::istreambuf_iterator<char>());
}

static int cmd_still(int argc, char** argv) {
  if (argc < 3) { usage(); return 2; }
  std::string out = argv[2];
  double t = 20;
  int h = 720, spp = 0, frames = 1, seq = 0;
  Scene sc = default_scene();
  for (int i = 3; i + 1 < argc; i += 2) {
    std::string k = argv[i];
    if (k == "--t") t = atof(argv[i + 1]);
    else if (k == "--h") h = atoi(argv[i + 1]);
    else if (k == "--spp") spp = atoi(argv[i + 1]);
    else if (k == "--frames") frames = atoi(argv[i + 1]);
    else if (k == "--seq") seq = atoi(argv[i + 1]);
    else if (k == "--scene") sc = parse_scene(read_file(argv[i + 1]));
    else { usage(); return 2; }
  }
  Renderer r;
  r.set_budget(envd("BUDGET", 5.0e5));
  std::string err;
  if (!r.init(&err)) { fprintf(stderr, "init: %s\n", err.c_str()); return 1; }
  Tile tile = tile_geometry(sc.rect, sc.image_aspect, h);
  // --seq N: the N consecutive frame slots ending at t, with history, as a
  // renderer drawing every slot would (t must be on the 1/30 s grid)
  if (seq > 0) frames = seq;
  for (int f = 0; f < frames; f++) {
    double tf = seq > 0 ? (std::llround(t * 30) - (seq - 1 - f)) / 30.0 : t;
    r.render(sc, tile, tf, frames > 1, spp);
    fprintf(stderr, "frame %d: %dx%d spp %d hold %d (%d samples) %.1f ms\n", f, tile.w, tile.h, r.stats().spp,
            (int)r.stats().hold, r.stats().hold_samples, r.stats().render_ms);
  }
  std::vector<uint8_t> px;
  r.download_rgba(&px);
  FILE* fp = fopen(out.c_str(), "wb");
  if (!fp) { perror(out.c_str()); return 1; }
  fprintf(fp, "P6\n%d %d\n255\n", tile.w, tile.h);
  for (size_t i = 0; i < px.size(); i += 4) fwrite(&px[i], 1, 3, fp);
  fclose(fp);
  return 0;
}

static int cmd_bench(int argc, char** argv);

// Process start to: GPU ready, first frame drawn, first frame encoded. The
// same steps `serve` takes before its first frame, without the network.
static int cmd_startup(int argc, char** argv) {
  double start = process_start_boottime_s();
  int h = argc > 2 ? atoi(argv[2]) : 240;
  Renderer r;
  std::string err;
  if (!r.init(&err)) { fprintf(stderr, "init: %s\n", err.c_str()); return 1; }
  double ready = (boottime_s() - start) * 1000.0;
  Scene sc = default_scene();
  Tile tl = tile_geometry(sc.rect, sc.image_aspect, h);
  r.render(sc, tl, 20.0, true);
  double drawn = (boottime_s() - start) * 1000.0;
  size_t pitch = 0;
  void* nv = r.alloc_nv12(tl.w, tl.h, &pitch);
  H264Encoder enc;
  if (!enc.open(r.cu_context(), nv, pitch, tl.w, tl.h, 30, &err)) { fprintf(stderr, "encoder: %s\n", err.c_str()); return 1; }
  r.to_nv12(nv, pitch, tl.w, tl.h);
  std::vector<uint8_t> au;
  bool key;
  if (!enc.encode(true, &au, &key, &err)) { fprintf(stderr, "encode: %s\n", err.c_str()); return 1; }
  double encoded = (boottime_s() - start) * 1000.0;
  printf("{\"gpu_ready_ms\":%.0f,\"first_frame_drawn_ms\":%.0f,\"first_frame_encoded_ms\":%.0f,\"width\":%d,\"height\":%d}\n",
         ready, drawn, encoded, tl.w, tl.h);
  return 0;
}

int main(int argc, char** argv) {
  std::string cmd = argc > 1 ? argv[1] : "serve";
  try {
    if (cmd == "still") return cmd_still(argc, argv);
    if (cmd == "bench") return cmd_bench(argc, argv);
    if (cmd == "startup") return cmd_startup(argc, argv);
    if (cmd == "sleep") {  // hold the container open without touching the GPU
      for (;;) pause();
    }
    if (cmd == "test") {  // the seam test, installed beside this binary
      std::string self = argv[0];
      std::string dir = self.find('/') == std::string::npos ? "/usr/local/bin" : self.substr(0, self.rfind('/'));
      std::string test = dir + "/fleet-renderer-test";
      argv[1] = (char*)test.c_str();
      execv(test.c_str(), argv + 1);
      perror(test.c_str());
      return 1;
    }
    if (cmd != "serve") { usage(); return 2; }
  } catch (const std::exception& e) {
    fprintf(stderr, "fleet-renderer: %s\n", e.what());
    return 1;
  }
  double start = process_start_boottime_s();
  Options o;
  o.port = (int)envd("PORT", 8080);
  o.fps = (int)envd("FPS", 30);
  o.budget = envd("BUDGET", 5.0e5);
  o.adapt = envd("ADAPT", 0) != 0;
  o.accumulate = envd("ACCUMULATE", 1) != 0;
  Pipeline p(o);
  p.set_process_start(start);
  Server s(p);
  std::string err;
  if (!s.listen(o.port, &err)) {
    fprintf(stderr, "fleet-renderer: %s\n", err.c_str());
    return 1;
  }
  fprintf(stderr, "fleet-renderer: listening on :%d (%.0f ms after process start)\n", o.port,
          (boottime_s() - start) * 1000.0);
  std::thread rt([&] { p.run(); });
  s.serve();
  rt.join();
  return 1;
}

#include "nvenc.h"
#include <chrono>
static int cmd_bench(int argc, char** argv) {
  std::vector<int> hs;
  int frames = 60;
  for (int i = 2; i + 1 < argc; i += 2) {
    std::string k = argv[i];
    if (k == "--h") hs.push_back(atoi(argv[i + 1]));
    else if (k == "--frames") frames = atoi(argv[i + 1]);
  }
  if (hs.empty()) hs = {136, 180, 240, 360, 720, 1080};
  Scene sc = default_scene();
  // a wall-like tile: 1/16 of the width, 1/9 of the height (an 16 x 9 grid)
  Rect wall{0.5, 0.333, 1.0 / 16, 1.0 / 9};
  Renderer r;
  r.set_budget(envd("BUDGET", 5.0e5));
  std::string err;
  if (!r.init(&err)) { fprintf(stderr, "init: %s\n", err.c_str()); return 1; }
  printf("height,width,spp,render_ms_mean,encode_ms_mean,fps_capacity,mode\n");
  for (int h : hs) {
    for (int mode = 0; mode < 2; mode++) {  // 0: moving camera, 1: held camera
      sc.rect = h >= 720 ? Rect{} : wall;
      Tile tl = tile_geometry(sc.rect, sc.image_aspect, h);
      size_t pitch = 0;
      void* nv = r.alloc_nv12(tl.w, tl.h, &pitch);
      H264Encoder enc;
      if (!enc.open(r.cu_context(), nv, pitch, tl.w, tl.h, 30, &err)) { fprintf(stderr, "encoder: %s\n", err.c_str()); return 1; }
      r.reset_history();
      double rsum = 0, esum = 0;
      int spp = 0;
      double t0 = mode == 0 ? 20.0 : 2.0;  // the demo path moves at 20 s and holds at 2 s
      for (int f = 0; f < frames; f++) {
        double t = t0 + f / 30.0;
        r.render(sc, tl, t, true);
        auto a = std::chrono::steady_clock::now();
        r.to_nv12(nv, pitch, tl.w, tl.h);
        std::vector<uint8_t> au;
        bool key;
        enc.encode(f == 0, &au, &key, &err);
        double ems = std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - a).count();
        if (f >= 5) { rsum += r.stats().render_ms; esum += ems; }
        if (f == 5) spp = r.stats().spp;
      }
      int n = frames - 5;
      double rm = rsum / n, em = esum / n;
      printf("%d,%d,%d,%.2f,%.2f,%.1f,%s\n", tl.h, tl.w, spp, rm, em, 1000.0 / (rm + em), mode ? "held" : "moving");
      fflush(stdout);
      r.free_dev(nv);
    }
  }
  return 0;
}
