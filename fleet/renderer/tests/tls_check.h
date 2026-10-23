// TLS front checks for test_host: a self-signed pair made here, a loopback
// listener, OpenSSL clients.
#pragma once
#include <arpa/inet.h>
#include <netinet/in.h>
#include <openssl/err.h>
#include <openssl/evp.h>
#include <openssl/pem.h>
#include <openssl/ssl.h>
#include <openssl/x509.h>
#include <fcntl.h>
#include <signal.h>
#include <sys/socket.h>
#include <unistd.h>

#include <chrono>
#include <cstdio>
#include <string>
#include <thread>

#include "../src/json.h"
#include "../src/tls.h"
#include "check.h"

namespace tlscheck {

// A P-256 key and a self-signed certificate for CN, as PEM.
static void make_pair(const char* cn, std::string* cert, std::string* key) {
  EVP_PKEY* k = EVP_EC_gen("P-256");
  X509* x = X509_new();
  X509_set_version(x, 2);
  ASN1_INTEGER_set(X509_get_serialNumber(x), 1);
  X509_gmtime_adj(X509_getm_notBefore(x), 0);
  X509_gmtime_adj(X509_getm_notAfter(x), 3600);
  X509_set_pubkey(x, k);
  X509_NAME* n = X509_get_subject_name(x);
  X509_NAME_add_entry_by_txt(n, "CN", MBSTRING_ASC, (const unsigned char*)cn, -1, -1, 0);
  X509_set_issuer_name(x, n);
  X509_sign(x, k, EVP_sha256());
  BIO* b = BIO_new(BIO_s_mem());
  PEM_write_bio_X509(b, x);
  char* p;
  long len = BIO_get_mem_data(b, &p);
  cert->assign(p, (size_t)len);
  BIO_free(b);
  b = BIO_new(BIO_s_mem());
  PEM_write_bio_PrivateKey(b, k, nullptr, nullptr, 0, nullptr, nullptr);
  len = BIO_get_mem_data(b, &p);
  key->assign(p, (size_t)len);
  BIO_free(b);
  X509_free(x);
  EVP_PKEY_free(k);
}

static void write_secret(const std::string& path, const std::string& cert, const std::string& key) {
  std::string tmp = path + ".tmp";
  FILE* f = fopen(tmp.c_str(), "w");
  std::string j = "{\"cert\":\"" + json_escape(cert) + "\",\"key\":\"" + json_escape(key) + "\",\"domains\":[\"x\"]}";
  fwrite(j.data(), 1, j.size(), f);
  fclose(f);
  rename(tmp.c_str(), path.c_str());  // as the agent replaces it
}

static int listen_lo(int* port) {
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  sockaddr_in a{};
  a.sin_family = AF_INET;
  a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  bind(fd, (sockaddr*)&a, sizeof a);
  listen(fd, 16);
  socklen_t l = sizeof a;
  getsockname(fd, (sockaddr*)&a, &l);
  *port = ntohs(a.sin_port);
  return fd;
}

struct Client {
  SSL_CTX* ctx = nullptr;
  SSL* ssl = nullptr;
  int fd = -1;
  bool ok = false;
  std::string cn;
  Client(int port, int max_version) {
    ctx = SSL_CTX_new(TLS_client_method());
    SSL_CTX_set_min_proto_version(ctx, 0);
    if (max_version) SSL_CTX_set_max_proto_version(ctx, max_version);
    SSL_CTX_set_security_level(ctx, 0);  // let the TLS 1.0/1.1 probe be offered at all
    fd = socket(AF_INET, SOCK_STREAM, 0);
    sockaddr_in a{};
    a.sin_family = AF_INET;
    a.sin_port = htons((uint16_t)port);
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (connect(fd, (sockaddr*)&a, sizeof a) != 0) return;
    ssl = SSL_new(ctx);
    SSL_set_fd(ssl, fd);
    ok = SSL_connect(ssl) == 1;
    if (ok) {
      X509* peer = SSL_get1_peer_certificate(ssl);
      char b[128] = {0};
      if (peer) X509_NAME_get_text_by_NID(X509_get_subject_name(peer), NID_commonName, b, sizeof b);
      cn = b;
      X509_free(peer);
    }
    ERR_clear_error();
  }
  bool echo(const std::string& m) {
    if (!ok || SSL_write(ssl, m.data(), (int)m.size()) != (int)m.size()) return false;
    std::string got(m.size(), '\0');
    size_t n = 0;
    while (n < m.size()) {
      int r = SSL_read(ssl, &got[n], (int)(m.size() - n));
      if (r <= 0) return false;
      n += (size_t)r;
    }
    return got == m;
  }
  ~Client() {
    if (ssl) SSL_free(ssl);
    if (fd >= 0) close(fd);
    SSL_CTX_free(ctx);
  }
};

static void run() {
  signal(SIGPIPE, SIG_IGN);  // as the server does: OpenSSL writes with write(2)
  std::string c1, k1, c2, k2, cert, key, err;
  CHECK(parse_cert_json("{\"cert\":\"x\"}", &cert, &key, &err) == false, "a secret without 'key' is refused");
  make_pair("first", &c1, &k1);
  make_pair("second", &c2, &k2);
  char dir[] = "/tmp/fleet-tls-XXXXXX";
  CHECK(mkdtemp(dir) != nullptr, "temp dir");
  std::string path = std::string(dir) + "/tls.json";
  write_secret(path, c1, k2);  // a key that does not belong to the certificate
  TlsFront front;
  CHECK(!front.load(path, &err) && !front.loaded(), "a certificate with another key is refused (%s)", err.c_str());
  write_secret(path, c1, k1);
  CHECK(front.load(path, &err) && front.loaded() && front.generation() == 1 && front.not_after().size() == 20,
        "the pair loads (%s, expires %s)", err.c_str(), front.not_after().c_str());

  // An echo server: each connection reads and writes back from two threads,
  // the way the WebSocket reader and writer share one connection.
  int port;
  int lfd = listen_lo(&port);
  std::thread srv([&] {
    while (true) {
      int fd = accept(lfd, nullptr, nullptr);
      if (fd < 0) return;
      std::thread([&front, fd] {
        std::string e;
        std::unique_ptr<Conn> c = front.accept(fd, 5000, &e);
        if (!c) return;
        char b[4096];
        while (true) {
          long r = c->read(b, sizeof b);
          if (r <= 0) break;
          std::string m(b, (size_t)r);
          if (m == "BIG") {
            // a reader taking 256 KiB off the connection while another
            // thread writes 4 MiB through it, as the WebSocket reader and
            // writer do
            std::string in(256 << 10, '\0');
            bool rok = false;
            std::thread rd([&] { rok = c->read_exact(&in[0], in.size()); });
            std::string big(4 << 20, '\0');
            for (size_t i = 0; i < big.size(); i++) big[i] = (char)('a' + i % 26);
            c->write_all(big.data(), big.size());
            rd.join();
            for (size_t i = 0; rok && i < in.size(); i++) rok = in[i] == (char)('A' + i % 26);
            std::string ack = rok ? "OK" : "NO";
            c->write_all(ack.data(), ack.size());
            continue;
          }
          c->write_all(m.data(), m.size());
        }
      }).detach();
    }
  });

  {
    Client old(port, TLS1_1_VERSION);
    CHECK(!old.ok, "a TLS 1.1 client is refused");
  }
  {
    Client v12(port, TLS1_2_VERSION);
    CHECK(v12.ok && v12.echo("hello 1.2") && SSL_version(v12.ssl) == TLS1_2_VERSION, "TLS 1.2 works");
  }
  Client before(port, 0);
  CHECK(before.ok && before.cn == "first" && before.echo("before"), "TLS 1.3 with the first certificate (cn %s)",
        before.cn.c_str());
  // 4 MiB written by one server thread while another reads 256 KiB; the
  // client sends its part first, from its own non-blocking loop
  {
    bool ok = before.ok && SSL_write(before.ssl, "BIG", 3) == 3;
    std::string out(256 << 10, '\0');
    for (size_t i = 0; i < out.size(); i++) out[i] = (char)('A' + i % 26);
    std::string got(4 << 20, '\0');
    size_t sent = 0, n = 0;
    fcntl(before.fd, F_SETFL, fcntl(before.fd, F_GETFL) | O_NONBLOCK);
    auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(20);
    while (ok && (sent < out.size() || n < got.size()) && std::chrono::steady_clock::now() < deadline) {
      bool progress = false;
      if (sent < out.size()) {
        int w = SSL_write(before.ssl, &out[sent], (int)std::min<size_t>(out.size() - sent, 1000));
        if (w > 0) { sent += (size_t)w; progress = true; }
      }
      if (n < got.size()) {
        int r = SSL_read(before.ssl, &got[n], (int)(got.size() - n));
        if (r > 0) { n += (size_t)r; progress = true; }
        else if (SSL_get_error(before.ssl, r) != SSL_ERROR_WANT_READ && SSL_get_error(before.ssl, r) != SSL_ERROR_WANT_WRITE) ok = false;
      }
      if (!progress) { ERR_clear_error(); usleep(200); }
    }
    for (size_t i = 0; ok && i < got.size(); i++) ok = got[i] == (char)('a' + i % 26);
    char ack[2] = {0, 0};
    for (int i = 0; ok && i < 20000 && SSL_read(before.ssl, ack, 2) <= 0; i++) usleep(500);
    fcntl(before.fd, F_SETFL, fcntl(before.fd, F_GETFL) & ~O_NONBLOCK);
    CHECK(ok && n == got.size() && ack[0] == 'O' && ack[1] == 'K',
          "4 MiB out through the writer while the reader takes 256 KiB in (read %zu, ack %c%c)", n, ack[0] ? ack[0] : '-',
          ack[1] ? ack[1] : '-');
  }

  // Reload: replace the file; new connections get the new certificate and
  // the open one keeps working.
  // the watcher thread starts late (a busy host): a file replaced before it
  // first looks is still read
  front.test_watch_delay_ms = 300;
  front.watch(path, 100);
  write_secret(path, c2, k2);
  for (int i = 0; i < 100 && front.generation() < 2; i++) std::this_thread::sleep_for(std::chrono::milliseconds(50));
  CHECK(front.generation() == 2, "the changed file was loaded again (loads %llu)", (unsigned long long)front.generation());
  Client after(port, 0);
  CHECK(after.ok && after.cn == "second" && after.echo("after"), "a new connection gets the new certificate (cn %s)",
        after.cn.c_str());
  CHECK(before.echo("still here"), "a connection opened before the reload keeps working");
  write_secret(path, "{not pem", k2);
  std::this_thread::sleep_for(std::chrono::milliseconds(400));
  Client bad(port, 0);
  CHECK(front.generation() == 2 && bad.ok && bad.cn == "second", "a broken file leaves the loaded pair in use");
  shutdown(lfd, SHUT_RDWR);
  close(lfd);
  srv.join();
  unlink(path.c_str());
  rmdir(dir);
}
}  // namespace tlscheck
