#include "subscriber.h"

void Subscriber::push(std::shared_ptr<const std::string> m) {
  std::lock_guard<std::mutex> l(mu);
  q.push_back(m);
  queued += m->size();
  cv.notify_one();
}

void Subscriber::close() {
  std::lock_guard<std::mutex> l(mu);
  closed = true;
  cv.notify_all();
}

void Subscriber::offer_header(int ekey, const std::shared_ptr<const std::string>& hdr) {
  if (cur_h == ekey) return;
  push(hdr);
  cur_h = ekey;
  need_key = true;
}

Subscriber::Offer Subscriber::offer_frame(const std::shared_ptr<const std::string>& msg, bool key) {
  if (need_key && !key) return SKIPPED;
  {
    std::lock_guard<std::mutex> l(mu);
    if (q.size() > SUB_MAX_QUEUED_MSGS || queued > SUB_MAX_QUEUED_BYTES) {
      q.clear();
      queued = 0;
      need_key = true;
      cur_h = 0;  // the cleared queue may have held the current header: send it again
      return DROPPED;
    }
  }
  need_key = false;
  push(msg);
  return SENT;
}
