// One frame-stream connection's queue, filled by the render thread and drained
// by the connection's writer. Host-only code, so the header and keyframe rules
// are tested without a GPU (tests/test_host.cpp).
#pragma once
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <deque>
#include <functional>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

// A reader this far behind is dropped to the next keyframe.
#define SUB_MAX_QUEUED_MSGS 90
#define SUB_MAX_QUEUED_BYTES (16u << 20)
// ... and so is one whose oldest queued frame waited this long (#1478). At 30
// fps that is 6 frames: a frame of 80 KB takes 30 ms to send at 20 Mbit/s, so
// a reader that keeps up holds one or two, and a link that hesitates for a
// few frames does not cost a keyframe; a stalled one resumes at most this far
// behind instead of replaying everything queued during the stall.
#define SUB_MAX_AGE_MS 200.0

struct Subscriber {
  // What a queued message is: a frame (dropped when the reader falls behind),
  // the stream header (dropped with the frames and sent again), or a control
  // frame (a pong or a close, never dropped).
  enum Kind { FRAME, HEADER, CONTROL };
  struct Item {
    std::shared_ptr<const std::string> m;
    Kind kind;
    double at_ms;  // when it was queued (now())
  };

  int id = 0;
  std::atomic<int> want_h{0};
  std::atomic<int> want_q{1};  // 0 lo, 1 std, 2 hi
  // Milliseconds on a monotonic clock; tests set their own.
  std::function<double()> now = [] {
    return std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now().time_since_epoch()).count();
  };
  // filled by the render thread, drained by the connection's writer
  std::mutex mu;
  std::condition_variable cv;
  std::deque<Item> q;  // complete WebSocket frames
  size_t queued = 0;
  bool closed = false;
  // under mu: times the queue was dropped to the next keyframe, and whether the
  // writer dropped it since the render thread last looked
  unsigned long long drops = 0;
  bool writer_dropped = false;
  // render-thread state
  int cur_h = 0;  // encoder key the header was sent for (0: header pending)
  bool need_key = true;
  bool report_drop = false;  // the writer dropped the queue: the next delta frame says DROPPED
  void push(std::shared_ptr<const std::string> m, Kind kind = CONTROL);
  void close();
  // Render thread: forget the header sent, so the next offer_header queues
  // it again, ahead of a keyframe.
  void restart_header();

  // Render thread: queue the stream header unless this subscriber already has
  // the one for encoder key `ekey`; a new header waits for a keyframe.
  void offer_header(int ekey, const std::shared_ptr<const std::string>& hdr);
  enum Offer { SENT, SKIPPED, DROPPED };
  // Render thread: queue one frame. SKIPPED: waiting for a keyframe. DROPPED:
  // the reader fell too far behind (too many messages or bytes queued, or its
  // oldest frame older than SUB_MAX_AGE_MS), its queued frames were dropped,
  // and it restarts at the next keyframe (the caller asks the encoder for one).
  Offer offer_frame(const std::shared_ptr<const std::string>& msg, bool key);

  // Writer: the next message to send, or null once closed with nothing left.
  // Blocks while the queue is empty. A frame that waited longer than
  // SUB_MAX_AGE_MS is not sent: the queued frames are dropped and the stream
  // restarts at the next keyframe.
  std::shared_ptr<const std::string> pop();
  // pop() without the wait: null when nothing is queued now.
  std::shared_ptr<const std::string> try_pop();

  // For /health: messages queued, the age of the oldest queued frame (ms, 0
  // with none) and the drops so far.
  struct Depth {
    size_t queued;
    double oldest_ms;
    unsigned long long drops;
  };
  Depth depth();

 private:
  // under mu: drop every queued frame and header, keep control frames
  void drop_locked();
  // under mu: the first queued frame, or q.end()
  std::deque<Item>::iterator oldest_frame_locked();
  // render thread: take a drop the writer made
  void take_writer_drop();
  std::shared_ptr<const std::string> pop_locked();
};

// /health's `viewers`: one object per stream, in subscription order, with its
// height, messages queued, the age of its oldest queued frame and its drops.
std::string viewers_json(const std::vector<std::shared_ptr<Subscriber>>& subs);
