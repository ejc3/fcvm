#include "screen.h"

#include <grpcpp/grpcpp.h>

#include <chrono>

#include "screen.grpc.pb.h"

using android::emulation::control::EmulatorController;
using android::emulation::control::Image;
using android::emulation::control::ImageFormat;

void ScreenStream::start() {
  th_ = std::thread([this] { run(); });
  th_.detach();
}

void ScreenStream::run() {
  uint64_t n = 0;
  while (true) {
    auto channel = grpc::CreateChannel(target_, grpc::InsecureChannelCredentials());
    auto stub = EmulatorController::NewStub(channel);
    grpc::ClientContext ctx;
    ImageFormat want;
    want.set_format(ImageFormat::RGBA8888);
    auto reader = stub->streamScreenshot(&ctx, want);
    {
      std::lock_guard<std::mutex> l(mu_);
      connects_++;
    }
    Image img;
    while (reader->Read(&img)) {
      int w = (int)img.format().width(), h = (int)img.format().height();
      const std::string& px = img.image();
      if (w <= 0 || h <= 0 || px.size() < (size_t)w * h * 4) continue;  // the display is off
      std::lock_guard<std::mutex> l(mu_);
      error_.clear();
      // top row first (measured: the proto's comment says bottom up, the image is not)
      latest_.rgba.assign(px.begin(), px.begin() + (size_t)w * h * 4);
      latest_.width = w;
      latest_.height = h;
      latest_.seq = ++n;
      latest_.ts_us = img.timestampus();
      cv_.notify_all();
    }
    grpc::Status st = reader->Finish();
    {
      std::lock_guard<std::mutex> l(mu_);
      error_ = "the emulator's screen stream ended: " + (st.ok() ? std::string("ok") : st.error_message());
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(300));
  }
}

bool ScreenStream::next(uint64_t after, int ms, ScreenFrame* out) {
  std::unique_lock<std::mutex> l(mu_);
  if (!cv_.wait_for(l, std::chrono::milliseconds(ms), [&] { return latest_.seq > after; })) return false;
  *out = latest_;
  return true;
}

std::string ScreenStream::error() {
  std::lock_guard<std::mutex> l(mu_);
  return error_;
}

uint64_t ScreenStream::connects() {
  std::lock_guard<std::mutex> l(mu_);
  return connects_;
}
