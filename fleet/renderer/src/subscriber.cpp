#include "subscriber.h"

#include <cmath>
#include <cstdio>

void Subscriber::push_locked(std::shared_ptr<const std::string> m, Kind kind, double at) {
  queued += m->size();
  q.push_back({std::move(m), kind, at});
  cv.notify_one();
}

void Subscriber::push(std::shared_ptr<const std::string> m, Kind kind) {
  double at = now();
  std::lock_guard<std::mutex> l(mu);
  push_locked(std::move(m), kind, at);
}

void Subscriber::close() {
  std::lock_guard<std::mutex> l(mu);
  closed = true;
  cv.notify_all();
}

void Subscriber::restart_header() {
  std::lock_guard<std::mutex> l(mu);
  cur_h = 0;
  need_key = true;
}

void Subscriber::drop_locked() {
  for (auto it = q.begin(); it != q.end();) {
    if (it->kind == CONTROL) {
      ++it;
      continue;
    }
    queued -= it->m->size();
    it = q.erase(it);
  }
  drops++;
}

std::deque<Subscriber::Item>::iterator Subscriber::oldest_frame_locked() {
  for (auto it = q.begin(); it != q.end(); ++it)
    if (it->kind == FRAME) return it;
  return q.end();
}

void Subscriber::offer_header(int ekey, const std::shared_ptr<const std::string>& hdr) {
  double t = now();
  std::lock_guard<std::mutex> l(mu);
  if (writer_dropped) {
    // the writer dropped the queue (header included): send the header again, and
    // the next delta frame says DROPPED so the caller asks for a keyframe
    writer_dropped = false;
    cur_h = 0;
    need_key = true;
    report_drop = true;
  }
  if (cur_h == ekey) return;
  push_locked(hdr, HEADER, t);
  cur_h = ekey;
  need_key = true;
}

Subscriber::Offer Subscriber::offer_frame(const std::shared_ptr<const std::string>& msg, bool key) {
  // One critical section: a drop by the writer is seen here or happens after the
  // frame is queued (and then takes it too), never in between.
  double t = now();
  std::lock_guard<std::mutex> l(mu);
  if (writer_dropped) {
    // dropped since offer_header, so perhaps with the header: nothing goes out until
    // the next offer_header queues it again
    writer_dropped = false;
    cur_h = 0;
    need_key = true;
    report_drop = false;
    return DROPPED;
  }
  if (report_drop && !key) {
    report_drop = false;
    return DROPPED;
  }
  report_drop = false;
  if (need_key && !key) return SKIPPED;
  auto old = oldest_frame_locked();
  bool stale = old != q.end() && t - old->at_ms > SUB_MAX_AGE_MS;
  if (stale || q.size() > SUB_MAX_QUEUED_MSGS || queued > SUB_MAX_QUEUED_BYTES) {
    drop_locked();
    need_key = true;
    cur_h = 0;  // the cleared queue may have held the current header: send it again
    return DROPPED;
  }
  need_key = false;
  push_locked(msg, FRAME, t);
  return SENT;
}

std::shared_ptr<const std::string> Subscriber::pop_locked() {
  double t = now();
  while (!q.empty()) {
    auto old = oldest_frame_locked();
    if (old != q.end() && t - old->at_ms > SUB_MAX_AGE_MS) {
      // A frame this old is not live any more, and neither is what follows it
      // up to the next keyframe: drop to it, and the render thread asks for one.
      drop_locked();
      writer_dropped = true;
      continue;
    }
    auto m = std::move(q.front().m);
    queued -= m->size();
    q.pop_front();
    return m;
  }
  return nullptr;
}

std::shared_ptr<const std::string> Subscriber::try_pop() {
  std::lock_guard<std::mutex> l(mu);
  return pop_locked();
}

std::shared_ptr<const std::string> Subscriber::pop() {
  std::unique_lock<std::mutex> l(mu);
  while (true) {
    cv.wait(l, [&] { return closed || !q.empty(); });
    auto m = pop_locked();
    if (m) return m;
    if (closed) return nullptr;
  }
}

Subscriber::Depth Subscriber::depth() {
  double t = now();
  std::lock_guard<std::mutex> l(mu);
  auto old = oldest_frame_locked();
  return {q.size(), old == q.end() ? 0.0 : t - old->at_ms, drops};
}

std::string viewers_json(const std::vector<std::shared_ptr<Subscriber>>& subs) {
  std::string out = "[";
  for (size_t i = 0; i < subs.size(); i++) {
    Subscriber::Depth d = subs[i]->depth();
    char b[160];
    snprintf(b, sizeof b, "%s{\"h\":%d,\"queued\":%zu,\"oldest_ms\":%.0f,\"drops\":%llu}", i ? "," : "",
             subs[i]->want_h.load(), d.queued, std::round(d.oldest_ms), d.drops);
    out += b;
  }
  return out + "]";
}
