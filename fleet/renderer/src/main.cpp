// fleet-renderer: renders one rectangle of the fleet's shared planet and
// streams it as H.264 over WebSocket. See fleet/PROTOCOL.md.
#include <unistd.h>

#include <algorithm>
#include <cmath>
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
#include "tls.h"

static void usage() {
  fprintf(stderr,
          "usage: fleet-renderer [serve]                      serve on $PORT (8080)\n"
          "       fleet-renderer still OUT.ppm [--t T] [--h H] [--scene FILE] [--spp N] [--frames N | --seq N] [--checker 0|2|4] [--hq 0|1]\n"
          "       fleet-renderer bench [--h H]... [--frames N]   time render + encode per height\n"
          "       fleet-renderer quality OUTDIR --t T [--scene FILE] [--h H] [--cols C --rows R] [--frames N] [--ref N] [--tile I]\n"
          "                                                     a moving frame of a C x R grid against a held reference: RMSE, render ms\n"
          "       fleet-renderer test [H]                       the seam test (whole image vs 2x2 tiles)\n"
          "       fleet-renderer startup [H]                    ms from process start to GPU ready, first frame, first encode\n"
          "       fleet-renderer sleep                          wait forever without using the GPU\n"
          "env: PORT, FPS (30), BUDGET pixel-samples per frame (250000), ADAPT (1), MARGIN (16), ACCUMULATE (1),\n"
          "     TLS_CERT_FILE (JSON {cert, key}; enables HTTPS/WSS), TLS_PORT (443)\n");
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
  int h = 720, spp = 0, frames = 1, seq = 0, checker = -1, hq = -1;
  Scene sc = default_scene();
  for (int i = 3; i + 1 < argc; i += 2) {
    std::string k = argv[i];
    if (k == "--t") t = atof(argv[i + 1]);
    else if (k == "--h") h = atoi(argv[i + 1]);
    else if (k == "--spp") spp = atoi(argv[i + 1]);
    else if (k == "--frames") frames = atoi(argv[i + 1]);
    else if (k == "--seq") seq = atoi(argv[i + 1]);
    else if (k == "--checker") checker = atoi(argv[i + 1]);
    else if (k == "--hq") hq = atoi(argv[i + 1]);
    else if (k == "--scene") sc = parse_scene(read_file(argv[i + 1]));
    else { usage(); return 2; }
  }
  Renderer r;
  r.set_budget(envd("BUDGET", 2.5e5));
  r.set_checkerboard(checker);
  r.set_quality(hq);
  std::string err;
  if (!r.init(&err)) { fprintf(stderr, "init: %s\n", err.c_str()); return 1; }
  Tile tile = tile_geometry(sc.rect, sc.image_aspect, h);
  // --seq N: the N consecutive frame slots ending at t, with history, as a
  // renderer drawing every slot would (t must be on the 1/30 s grid)
  if (seq > 0) frames = seq;
  for (int f = 0; f < frames; f++) {
    double tf = seq > 0 ? (std::llround(t * 30) - (seq - 1 - f)) / 30.0 : t;
    r.render(sc, tile, tf, frames > 1, spp);
    fprintf(stderr, "frame %d: %dx%d spp %d hq %d hold %d (%d samples) %.1f ms, %u bad samples\n", f, tile.w, tile.h,
            r.stats().spp, (int)r.stats().hq, (int)r.stats().hold, r.stats().hold_samples, r.stats().render_ms,
            r.stats().bad_samples);
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

// quality OUTDIR: a moving frame of a grid of tiles, each drawn in a fresh
// renderer with its history as the wall draws it, against a reference of the
// same frame (the same pose and scene time held and accumulated to --ref
// samples). Prints per-tile and whole-image RMSE (8-bit RGB) and render times;
// writes frame.ppm and ref.ppm, the whole image stitched from the tiles.
static void write_ppm(const std::string& path, int w, int h, const std::vector<uint8_t>& rgb) {
  FILE* fp = fopen(path.c_str(), "wb");
  if (!fp) throw std::runtime_error("cannot write " + path);
  fprintf(fp, "P6\n%d %d\n255\n", w, h);
  fwrite(rgb.data(), 1, rgb.size(), fp);
  fclose(fp);
}

static int cmd_quality(int argc, char** argv) {
  if (argc < 3) { usage(); return 2; }
  std::string out = argv[2];
  double t = 0;
  int h = 360, frames = 24, ref = 1024, cols = 4, rows = 4, only = -1;
  Scene sc = default_scene();
  for (int i = 3; i + 1 < argc; i += 2) {
    std::string k = argv[i];
    if (k == "--t") t = atof(argv[i + 1]);
    else if (k == "--h") h = atoi(argv[i + 1]);
    else if (k == "--frames") frames = atoi(argv[i + 1]);
    else if (k == "--ref") ref = atoi(argv[i + 1]);
    else if (k == "--cols") cols = atoi(argv[i + 1]);
    else if (k == "--rows") rows = atoi(argv[i + 1]);
    else if (k == "--tile") only = atoi(argv[i + 1]);
    else if (k == "--scene") sc = parse_scene(read_file(argv[i + 1]));
    else { usage(); return 2; }
  }
  Renderer r;
  double budget = envd("BUDGET", 2.5e5);
  r.set_budget(budget);
  r.set_margin((int)envd("MARGIN", 16));
  r.set_text_overlay(false);
  std::string err;
  if (!r.init(&err)) { fprintf(stderr, "init: %s\n", err.c_str()); return 1; }
  long long k = std::llround(t * 30);
  t = k / 30.0;
  Tile whole = tile_geometry(Rect{0, 0, 1.0 / cols, 1.0 / rows}, sc.image_aspect, h);
  int W = whole.full_w, H = whole.full_h;
  std::vector<uint8_t> img(3 * (size_t)W * H, 0), refimg(3 * (size_t)W * H, 0);
  // warm the GPU out of its idle clocks
  {
    Scene w = sc;
    w.rect = Rect{0, 0, 1.0 / cols, 1.0 / rows};
    for (int f = 0; f < 90; f++) r.render(w, whole, t - 3 + f / 30.0, true);
  }
  Scene hs = held_reference(sc, t);
  double se_all = 0;
  long n_all = 0;
  printf("{\"t\":%.4f,\"budget\":%.0f,\"full_w\":%d,\"full_h\":%d,\"tiles\":[", t, budget, W, H);
  bool first = true;
  for (int ti = 0; ti < cols * rows; ti++) {
    if (only >= 0 && ti != only) continue;
    Rect rc{(double)(ti % cols) / cols, (double)(ti / cols) / rows, 1.0 / cols, 1.0 / rows};
    Scene s = sc;
    s.rect = rc;
    Tile tl = tile_geometry(rc, sc.image_aspect, h);
    r.reset_history();
    r.set_quality(-1);
    std::vector<double> ms;
    for (int f = frames - 1; f >= 0; f--) {
      r.render(s, tl, (k - f) / 30.0, true);
      if (f < frames - 8) ms.push_back(r.stats().render_ms);
    }
    int spp = r.stats().spp;
    bool hq = r.stats().hq;
    std::vector<uint8_t> a, b;
    r.download_rgba(&a);
    if (ref <= 0) b = a;  // timing only
    // the reference: the same pose and scene time, held, on the same path
    Scene rs = hs;
    rs.rect = rc;
    r.reset_history();
    r.set_quality(hq ? 1 : 0);
    for (int n = 0; n < ref; n += 64) r.render(rs, tl, t, true, 64);
    if (ref > 0) r.download_rgba(&b);
    r.set_quality(-1);
    double se = 0;
    for (int j = 0; j < tl.h; j++)
      for (int i = 0; i < tl.w; i++) {
        int X = tl.x0 + i, Y = tl.y0 + j;
        if (X >= W || Y >= H) continue;
        for (int c = 0; c < 3; c++) {
          uint8_t va = a[4 * ((size_t)j * tl.w + i) + c], vb = b[4 * ((size_t)j * tl.w + i) + c];
          img[3 * ((size_t)Y * W + X) + c] = va;
          refimg[3 * ((size_t)Y * W + X) + c] = vb;
          se += (double)(va - vb) * (va - vb);
        }
      }
    long n = 3L * tl.w * tl.h;
    se_all += se;
    n_all += n;
    std::sort(ms.begin(), ms.end());
    double mean = 0;
    for (double m : ms) mean += m;
    mean /= std::max<size_t>(1, ms.size());
    printf("%s{\"tile\":%d,\"w\":%d,\"h\":%d,\"spp\":%d,\"hq\":%d,\"drawn_px\":%.0f,\"render_ms_mean\":%.2f,\"render_ms_max\":%.2f,"
           "\"rmse\":%.3f}",
           first ? "" : ",", ti, tl.w, tl.h, spp, (int)hq, r.drawn_pixels(tl), mean, ms.empty() ? 0.0 : ms.back(),
           std::sqrt(se / n));
    fflush(stdout);
    first = false;
  }
  printf("],\"rmse\":%.3f}\n", std::sqrt(se_all / std::max(1L, n_all)));
  write_ppm(out + "/frame.ppm", W, H, img);
  write_ppm(out + "/ref.ppm", W, H, refimg);
  return 0;
}


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
  if (!enc.open(r.cu_context(), nv, pitch, tl.w, tl.h, 30, 1, &err)) { fprintf(stderr, "encoder: %s\n", err.c_str()); return 1; }
  r.to_nv12(nv, pitch, tl.w, tl.h, tl.x0, tl.y0, tl.full_h);
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
    if (cmd == "quality") return cmd_quality(argc, argv);
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
  o.budget = envd("BUDGET", 2.5e5);
  o.adapt = envd("ADAPT", 1) != 0;
  o.fit_scale = envd("FIT_SCALE", 1.0);
  o.margin = (int)envd("MARGIN", 16);
  o.accumulate = envd("ACCUMULATE", 1) != 0;
  Pipeline p(o);
  p.set_process_start(start);
  Server s(p);
  // A fleet renderer's scene token (access.h): its own file on a tile instance, else beside
  // the certificate.
  SceneToken scene_token;
  scene_token.configure_from_env();
  if (scene_token.configured()) {
    s.set_scene_token(&scene_token);
    fprintf(stderr, "fleet-renderer: fleet mode, scene token %s\n", scene_token.current().empty() ? "not loaded yet" : "loaded");
  }
  std::string err;
  if (!s.listen(o.port, &err)) {
    fprintf(stderr, "fleet-renderer: %s\n", err.c_str());
    return 1;
  }
  fprintf(stderr, "fleet-renderer: listening on :%d (%.0f ms after process start)\n", o.port,
          (boottime_s() - start) * 1000.0);
  // HTTPS and WSS for browsers, when the host hands over a certificate file
  TlsFront tls;
  const char* cert_file = getenv("TLS_CERT_FILE");
  if (cert_file && *cert_file) {
    int tport = (int)envd("TLS_PORT", 443);
    if (!tls.load(cert_file, &err)) fprintf(stderr, "fleet-renderer: TLS certificate not loaded yet: %s\n", err.c_str());
    tls.watch(cert_file, 2000);
    if (!s.listen_tls(tport, &tls, &err)) {
      fprintf(stderr, "fleet-renderer: %s\n", err.c_str());
      return 1;
    }
    fprintf(stderr, "fleet-renderer: TLS on :%d, certificate %s (expires %s)\n", tport,
            tls.loaded() ? "loaded" : "not loaded", tls.not_after().c_str());
  }
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
  r.set_budget(envd("BUDGET", 2.5e5));
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
      if (!enc.open(r.cu_context(), nv, pitch, tl.w, tl.h, 30, 1, &err)) { fprintf(stderr, "encoder: %s\n", err.c_str()); return 1; }
      r.reset_history();
      double rsum = 0, esum = 0;
      int spp = 0;
      double t0 = mode == 0 ? 20.0 : 2.0;  // the demo path moves at 20 s and holds at 2 s
      for (int f = 0; f < frames; f++) {
        double t = t0 + f / 30.0;
        r.render(sc, tl, t, true);
        auto a = std::chrono::steady_clock::now();
        r.to_nv12(nv, pitch, tl.w, tl.h, tl.x0, tl.y0, tl.full_h);
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
