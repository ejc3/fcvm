// The emulator's screen, read through its gRPC controller (`emulator -grpc <port>`,
// streamScreenshot): every frame its display posts arrives as a whole RGBA image, so no copy
// is ever torn. A thread keeps the newest one; the capture loop takes it when it is new.
// The emulator reads the screen for this from the display's color buffer (its screenshot
// path), not through the frame recorder that `-share-vid` uses, which on this emulator
// shares black frames after a cold boot and loses a color buffer across a GPU release.
#pragma once
#include <stdint.h>

#include <condition_variable>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

struct ScreenFrame {
  std::vector<uint8_t> rgba;  // top row first
  int width = 0, height = 0;
  uint64_t seq = 0;      // ours: counts every frame received
  uint64_t ts_us = 0;    // the emulator's time of the copy (Unix us)
};

class ScreenStream {
 public:
  explicit ScreenStream(std::string target) : target_(std::move(target)) {}
  void start();  // the reader thread; reconnects when the stream ends
  // Waits up to `ms` for a frame newer than `after`; true and *out filled when one came.
  bool next(uint64_t after, int ms, ScreenFrame* out);
  // The stream's last error ("" while it reads frames).
  std::string error();
  uint64_t connects();

 private:
  void run();
  std::string target_;
  std::thread th_;
  std::mutex mu_;
  std::condition_variable cv_;
  ScreenFrame latest_;
  std::string error_ = "not connected yet";
  uint64_t connects_ = 0;
};
