// One frame-stream connection's queue, filled by the render thread and drained
// by the connection's writer. Host-only code, so the header and keyframe rules
// are tested without a GPU (tests/test_host.cpp).
#pragma once
#include <atomic>
#include <condition_variable>
#include <deque>
#include <memory>
#include <mutex>
#include <string>

// A reader this far behind is dropped to the next keyframe.
#define SUB_MAX_QUEUED_MSGS 90
#define SUB_MAX_QUEUED_BYTES (16u << 20)

struct Subscriber {
  int id = 0;
  std::atomic<int> want_h{0};
  std::atomic<int> want_q{1};  // 0 lo, 1 std, 2 hi
  // filled by the render thread, drained by the connection's writer
  std::mutex mu;
  std::condition_variable cv;
  std::deque<std::shared_ptr<const std::string>> q;  // complete WebSocket frames
  size_t queued = 0;
  bool closed = false;
  // render-thread state
  int cur_h = 0;  // encoder key the header was sent for (0: header pending)
  bool need_key = true;
  void push(std::shared_ptr<const std::string> m);
  void close();
  // Render thread: forget the header sent, so the next offer_header queues
  // it again, ahead of a keyframe.
  void restart_header();

  // Render thread: queue the stream header unless this subscriber already has
  // the one for encoder key `ekey`; a new header waits for a keyframe.
  void offer_header(int ekey, const std::shared_ptr<const std::string>& hdr);
  enum Offer { SENT, SKIPPED, DROPPED };
  // Render thread: queue one frame. SKIPPED: waiting for a keyframe. DROPPED:
  // the reader fell too far behind, its queue was cleared, and it restarts at
  // the next keyframe (the caller asks the encoder for one).
  Offer offer_frame(const std::shared_ptr<const std::string>& msg, bool key);
};
