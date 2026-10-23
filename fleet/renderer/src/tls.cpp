#include "tls.h"

#include <fcntl.h>
#include <openssl/err.h>
#include <openssl/pem.h>
#include <openssl/ssl.h>
#include <openssl/x509.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
#include <cerrno>
#include <chrono>
#include <cstdio>
#include <ctime>

#include "json.h"

namespace {
std::string ssl_errors() {
  std::string s;
  unsigned long e;
  while ((e = ERR_get_error()) != 0) {
    char b[256];
    ERR_error_string_n(e, b, sizeof b);
    if (!s.empty()) s += "; ";
    s += b;
  }
  return s.empty() ? "unknown TLS error" : s;
}

// Wait until fd is ready for `ev`; false on timeout or error. ms < 0: forever.
bool wait_fd(int fd, short ev, int ms) {
  pollfd p{fd, ev, 0};
  while (true) {
    int r = poll(&p, 1, ms);
    if (r > 0) return true;
    if (r == 0) return false;
    if (errno != EINTR) return false;
  }
}

long ms_left(std::chrono::steady_clock::time_point deadline) {
  return std::chrono::duration_cast<std::chrono::milliseconds>(deadline - std::chrono::steady_clock::now()).count();
}

// A server context from PEM text: the first certificate is the leaf, the rest
// its chain. TLS 1.2 and 1.3 only; forward-secret AEAD suites for 1.2.
SSL_CTX* build_ctx(const std::string& cert_pem, const std::string& key_pem, std::string* not_after, std::string* err) {
  SSL_CTX* ctx = SSL_CTX_new(TLS_server_method());
  if (!ctx) { *err = ssl_errors(); return nullptr; }
  SSL_CTX_set_min_proto_version(ctx, TLS1_2_VERSION);
  SSL_CTX_set_options(ctx, SSL_OP_NO_RENEGOTIATION | SSL_OP_CIPHER_SERVER_PREFERENCE | SSL_OP_NO_COMPRESSION);
  SSL_CTX_set_cipher_list(ctx, "ECDHE+AESGCM:ECDHE+CHACHA20");
  SSL_CTX_set_mode(ctx, SSL_MODE_ENABLE_PARTIAL_WRITE | SSL_MODE_ACCEPT_MOVING_WRITE_BUFFER | SSL_MODE_RELEASE_BUFFERS);
  BIO* cb = BIO_new_mem_buf(cert_pem.data(), (int)cert_pem.size());
  X509* leaf = PEM_read_bio_X509(cb, nullptr, nullptr, nullptr);
  bool ok = leaf && SSL_CTX_use_certificate(ctx, leaf) == 1;
  if (ok) {
    const ASN1_TIME* na = X509_get0_notAfter(leaf);
    struct tm tmv;
    if (na && ASN1_TIME_to_tm(na, &tmv) == 1) {
      char b[32];
      strftime(b, sizeof b, "%Y-%m-%dT%H:%M:%SZ", &tmv);
      *not_after = b;
    }
  }
  int chain = 0;
  while (ok) {
    X509* c = PEM_read_bio_X509(cb, nullptr, nullptr, nullptr);
    if (!c) { ERR_clear_error(); break; }  // end of the PEM text
    ok = SSL_CTX_add0_chain_cert(ctx, c) == 1;
    if (!ok) X509_free(c);
    chain++;
  }
  if (leaf) X509_free(leaf);
  BIO_free(cb);
  if (!ok) {
    *err = "certificate: " + ssl_errors();
    SSL_CTX_free(ctx);
    return nullptr;
  }
  BIO* kb = BIO_new_mem_buf(key_pem.data(), (int)key_pem.size());
  EVP_PKEY* key = PEM_read_bio_PrivateKey(kb, nullptr, nullptr, nullptr);
  BIO_free(kb);
  ok = key && SSL_CTX_use_PrivateKey(ctx, key) == 1 && SSL_CTX_check_private_key(ctx) == 1;
  if (key) EVP_PKEY_free(key);
  if (!ok) {
    *err = "key: " + ssl_errors();
    SSL_CTX_free(ctx);
    return nullptr;
  }
  static const unsigned char alpn_h1[] = "\x08http/1.1";
  SSL_CTX_set_alpn_select_cb(
      ctx,
      [](SSL*, const unsigned char** out, unsigned char* outlen, const unsigned char* in, unsigned int inlen,
         void*) -> int {
        // WebSocket runs over HTTP/1.1: pick it if offered, else no ALPN
        if (SSL_select_next_proto((unsigned char**)out, outlen, alpn_h1, sizeof alpn_h1 - 1, in, inlen) ==
            OPENSSL_NPN_NEGOTIATED)
          return SSL_TLSEXT_ERR_OK;
        return SSL_TLSEXT_ERR_NOACK;
      },
      nullptr);
  return ctx;
}
}  // namespace

bool parse_cert_json(const std::string& text, std::string* cert_pem, std::string* key_pem, std::string* err) {
  try {
    Json j = Json::parse(text);
    *cert_pem = j.at("cert").string("cert");
    *key_pem = j.at("key").string("key");
  } catch (const std::exception& e) {
    *err = e.what();
    return false;
  }
  if (cert_pem->find("-----BEGIN CERTIFICATE-----") == std::string::npos) { *err = "'cert' holds no PEM certificate"; return false; }
  if (key_pem->find("PRIVATE KEY-----") == std::string::npos) { *err = "'key' holds no PEM private key"; return false; }
  return true;
}

// ---------------------------------------------------------------- Conn

Conn::Conn(int fd, SSL* ssl) : fd_(fd), ssl_(ssl) {}

Conn::~Conn() {
  if (ssl_) {
    {
      std::lock_guard<std::mutex> l(mu_);
      SSL_shutdown(ssl_);  // best effort close_notify; the socket is non-blocking
    }
    SSL_free(ssl_);
  }
  close(fd_);
}

void Conn::set_option(int level, int name, int value) { setsockopt(fd_, level, name, &value, sizeof value); }

void Conn::set_read_timeout_ms(int ms) {
  rto_ms_ = ms;
  if (!ssl_) {
    timeval tv{ms / 1000, (ms % 1000) * 1000};
    setsockopt(fd_, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
  }
}

void Conn::shutdown_both() { ::shutdown(fd_, SHUT_RDWR); }

long Conn::read(void* buf, size_t n) {
  if (!ssl_) {
    while (true) {
      ssize_t r = ::recv(fd_, buf, n, 0);
      if (r < 0 && errno == EINTR) continue;
      return (long)r;
    }
  }
  while (true) {
    int r, e;
    {
      std::lock_guard<std::mutex> l(mu_);
      ERR_clear_error();
      r = SSL_read(ssl_, buf, (int)std::min<size_t>(n, 1 << 30));
      e = r > 0 ? SSL_ERROR_NONE : SSL_get_error(ssl_, r);
    }
    if (r > 0) return r;
    if (e == SSL_ERROR_ZERO_RETURN) return 0;
    if (e == SSL_ERROR_WANT_READ) {
      if (!wait_fd(fd_, POLLIN, rto_ms_ > 0 ? rto_ms_ : -1)) return -1;
    } else if (e == SSL_ERROR_WANT_WRITE) {
      if (!wait_fd(fd_, POLLOUT, rto_ms_ > 0 ? rto_ms_ : -1)) return -1;
    } else {
      return e == SSL_ERROR_SYSCALL ? 0 : -1;
    }
  }
}

bool Conn::read_exact(void* buf, size_t n) {
  char* p = (char*)buf;
  while (n) {
    long r = read(p, n);
    if (r <= 0) return false;
    p += r;
    n -= (size_t)r;
  }
  return true;
}

bool Conn::write_all(const void* data, size_t n) {
  const char* p = (const char*)data;
  if (!ssl_) {
    while (n) {
      ssize_t w = ::send(fd_, p, n, MSG_NOSIGNAL);
      if (w < 0) {
        if (errno == EINTR) continue;
        return false;
      }
      p += w;
      n -= (size_t)w;
    }
    return true;
  }
  while (n) {
    int w, e;
    {
      std::lock_guard<std::mutex> l(mu_);
      ERR_clear_error();
      w = SSL_write(ssl_, p, (int)std::min<size_t>(n, 1 << 30));
      e = w > 0 ? SSL_ERROR_NONE : SSL_get_error(ssl_, w);
    }
    if (w > 0) {
      p += w;
      n -= (size_t)w;
      continue;
    }
    if (e == SSL_ERROR_WANT_WRITE) {
      if (!wait_fd(fd_, POLLOUT, -1)) return false;
    } else if (e == SSL_ERROR_WANT_READ) {
      if (!wait_fd(fd_, POLLIN, -1)) return false;
    } else {
      return false;
    }
  }
  return true;
}

// ---------------------------------------------------------------- TlsFront

TlsFront::TlsFront() {}

TlsFront::~TlsFront() {
  stop_ = true;
  if (watcher_.joinable()) watcher_.join();
}

bool TlsFront::load(const std::string& path, std::string* err) {
  // The version is taken from the descriptor that is read, so it names the
  // file actually loaded even if another replaces it meanwhile.
  int fd = open(path.c_str(), O_RDONLY | O_CLOEXEC);
  if (fd < 0) { *err = path + ": cannot read"; return false; }
  struct stat st {};
  std::string text;
  bool ok = fstat(fd, &st) == 0;
  char buf[16384];
  for (ssize_t n; ok && (n = read(fd, buf, sizeof buf)) != 0;) {
    if (n < 0) {
      if (errno == EINTR) continue;
      ok = false;
    } else {
      text.append(buf, (size_t)n);
    }
  }
  close(fd);
  if (!ok) { *err = path + ": cannot read"; return false; }
  std::string cert, key, na;
  if (!parse_cert_json(text, &cert, &key, err)) { *err = path + ": " + *err; return false; }
  SSL_CTX* c = build_ctx(cert, key, &na, err);
  if (!c) { *err = path + ": " + *err; return false; }
  std::shared_ptr<SSL_CTX> sp(c, SSL_CTX_free);
  {
    std::lock_guard<std::mutex> l(mu_);
    ctx_ = sp;  // connections already open hold their own reference to the old one
    not_after_ = na;
    loaded_st_ = st;
    have_loaded_st_ = true;
  }
  gen_++;
  return true;
}

void TlsFront::watch(const std::string& path, int period_ms) {
  // A file is loaded once per version (inode, size, modification time): a
  // rename into place or a rewrite is a new version. One read halfway
  // through a rewrite fails to parse, and the writer's last write makes a
  // newer version that is read again. The watcher starts from the version
  // load() read, so a file replaced after the load, before or after watch()
  // is called, is read again.
  struct stat last0 {};
  bool have0;
  {
    std::lock_guard<std::mutex> l(mu_);
    last0 = loaded_st_;
    have0 = have_loaded_st_;
  }
  watcher_ = std::thread([this, path, period_ms, last0, have0] {
    struct stat last = last0;
    bool have = have0;
    if (test_watch_delay_ms > 0) usleep(test_watch_delay_ms * 1000);
    while (!stop_) {
      struct stat st {};
      if (stat(path.c_str(), &st) == 0 &&
          !(have && st.st_ino == last.st_ino && st.st_size == last.st_size && st.st_mtim.tv_sec == last.st_mtim.tv_sec &&
            st.st_mtim.tv_nsec == last.st_mtim.tv_nsec)) {
        have = true;
        last = st;
        std::string err;
        if (load(path, &err))
          fprintf(stderr, "fleet-renderer: TLS certificate loaded from %s (load %llu, expires %s)\n", path.c_str(),
                  (unsigned long long)generation(), not_after().c_str());
        else
          fprintf(stderr, "fleet-renderer: TLS certificate not loaded: %s\n", err.c_str());
      }
      for (int i = 0; i < period_ms / 50 && !stop_; i++) usleep(50 * 1000);
    }
  });
}

bool TlsFront::loaded() const {
  std::lock_guard<std::mutex> l(mu_);
  return ctx_ != nullptr;
}

std::string TlsFront::not_after() const {
  std::lock_guard<std::mutex> l(mu_);
  return not_after_;
}

std::unique_ptr<Conn> TlsFront::accept(int fd, int timeout_ms, std::string* err) {
  std::shared_ptr<SSL_CTX> ctx;
  {
    std::lock_guard<std::mutex> l(mu_);
    ctx = ctx_;
  }
  if (!ctx) {
    *err = "no certificate loaded";
    close(fd);
    return nullptr;
  }
  SSL* ssl = SSL_new(ctx.get());  // holds its own reference to ctx
  if (!ssl) {
    *err = ssl_errors();
    close(fd);
    return nullptr;
  }
  fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK);
  SSL_set_fd(ssl, fd);
  auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(timeout_ms);
  while (true) {
    ERR_clear_error();
    int r = SSL_accept(ssl);
    if (r == 1) return std::unique_ptr<Conn>(new Conn(fd, ssl));
    int e = SSL_get_error(ssl, r);
    long left = ms_left(deadline);
    bool ok = left > 0 && ((e == SSL_ERROR_WANT_READ && wait_fd(fd, POLLIN, (int)left)) ||
                           (e == SSL_ERROR_WANT_WRITE && wait_fd(fd, POLLOUT, (int)left)));
    if (!ok) {
      *err = left <= 0 ? std::string("handshake timed out") : "handshake: " + ssl_errors();
      SSL_free(ssl);
      close(fd);
      return nullptr;
    }
  }
}
