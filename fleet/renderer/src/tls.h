// Connections (plain or TLS) and the TLS front: the certificate and key come
// from a JSON file the host hands to the VM ({"cert": full chain PEM, "key":
// PEM}, the fleet's tls-wildcard secret), and are loaded again when the file
// changes. New connections use the new pair; open ones keep theirs.
#pragma once
#include <stdint.h>
#include <sys/stat.h>

#include <atomic>
#include <memory>
#include <mutex>
#include <string>
#include <thread>

typedef struct ssl_st SSL;
typedef struct ssl_ctx_st SSL_CTX;

// One accepted connection. read() and write_all() may be called from one
// reader thread and one writer thread at the same time: TLS calls are made
// under a lock on a non-blocking socket, and waiting happens outside it.
class Conn {
 public:
  Conn(int fd, SSL* ssl);  // takes ownership of both
  ~Conn();
  Conn(const Conn&) = delete;
  Conn& operator=(const Conn&) = delete;
  bool tls() const { return ssl_ != nullptr; }
  // > 0 bytes read, 0 closed by the peer, < 0 error or timeout
  long read(void* buf, size_t n);
  bool read_exact(void* buf, size_t n);
  bool write_all(const void* p, size_t n);
  void set_read_timeout_ms(int ms);  // 0: wait forever
  void set_option(int level, int name, int value);
  void shutdown_both();  // wakes a blocked reader or writer

 private:
  int fd_;
  SSL* ssl_;
  std::mutex mu_;
  int rto_ms_ = 0;
};

class TlsFront {
 public:
  TlsFront();
  ~TlsFront();
  // Reads the JSON file and builds a context from it (TLS 1.2 and 1.3 only).
  // On failure the previous context, if any, stays in use.
  bool load(const std::string& path, std::string* err);
  // Checks the file every period_ms and loads it again when it changed (a new
  // inode, size or modification time). Runs on its own thread.
  void watch(const std::string& path, int period_ms);
  // Tests: the watcher thread waits this long before its first look (as a
  // thread scheduled late on a busy host would).
  int test_watch_delay_ms = 0;
  // TLS handshake on an accepted socket, within timeout_ms.
  std::unique_ptr<Conn> accept(int fd, int timeout_ms, std::string* err);
  // For /health: whether a pair is loaded, how many loads succeeded, and the
  // certificate's expiry (RFC 3339 UTC, empty if none).
  bool loaded() const;
  uint64_t generation() const { return gen_.load(); }
  std::string not_after() const;

 private:
  mutable std::mutex mu_;
  std::shared_ptr<SSL_CTX> ctx_;
  std::string not_after_;
  struct stat loaded_st_ {};  // the version (inode, size, mtime) of the file last loaded
  bool have_loaded_st_ = false;
  std::atomic<uint64_t> gen_{0};
  std::atomic<bool> stop_{false};
  std::thread watcher_;
};

// The full-chain PEM and key PEM from the secret's JSON text.
bool parse_cert_json(const std::string& text, std::string* cert_pem, std::string* key_pem, std::string* err);
