// Starting a thread when the system may refuse one. Host-only, so it is tested
// without a GPU (tests/test_host.cpp).
#pragma once
#include <functional>
#include <system_error>
#include <thread>
#include <utility>

// The thread maker: std::thread's constructor, which throws std::system_error
// when the system has no thread to give. Tests pass one that refuses.
using ThreadMaker = std::function<std::thread(std::function<void()>)>;

inline std::thread make_std_thread(std::function<void()> f) { return std::thread(std::move(f)); }

// Runs f on a new thread, detached when `joinable` is null and moved into
// *joinable otherwise. Returns false, and f never runs, when the thread cannot
// be started: a pids or thread limit (EAGAIN). Each frame stream takes two
// threads, and the EC2 tile's container had a pids limit of 2048, so its
// renderer terminated on an uncaught std::system_error at about the 1,020th
// viewer and every viewer of the tile lost its stream (README.md, "Viewers per
// renderer"). A caller refuses that one connection instead.
inline bool start_thread(std::function<void()> f, std::thread* joinable, const ThreadMaker& make = make_std_thread) {
  std::thread t;
  try {
    t = make(std::move(f));
  } catch (const std::system_error&) {
    return false;
  }
  if (joinable) *joinable = std::move(t);
  else t.detach();
  return true;
}
