#include "server.h"

#include <arpa/inet.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <signal.h>
#include <sys/socket.h>
#include <unistd.h>

#include <algorithm>
#include <cerrno>
#include <cstdio>
#include <cstring>
#include <map>
#include <thread>

#include "json.h"
#include "nvenc.h"
#include "jpeg.h"
#include "proto.h"
#include "threads.h"

namespace {
void respond(Conn& c, int code, const char* reason, const std::string& body, const char* ctype = "application/json",
             bool cors = false) {
  char h[512];
  snprintf(h, sizeof h,
           "HTTP/1.1 %d %s\r\nContent-Type: %s\r\nContent-Length: %zu\r\nCache-Control: no-store\r\n%sConnection: close\r\n\r\n",
           code, reason, ctype, body.size(), cors ? "Access-Control-Allow-Origin: *\r\n" : "");
  std::string out = std::string(h) + body;
  c.write_all(out.data(), out.size());
}
std::string lower(std::string s) {
  for (char& c : s) c = (char)tolower((unsigned char)c);
  return s;
}
std::string trim(const std::string& s) {
  size_t a = s.find_first_not_of(" \t"), b = s.find_last_not_of(" \t\r");
  return a == std::string::npos ? "" : s.substr(a, b - a + 1);
}
std::string query_str(const std::string& q, const std::string& k, const std::string& def) {
  size_t p = 0;
  while (p < q.size()) {
    size_t amp = q.find('&', p);
    std::string kv = q.substr(p, amp == std::string::npos ? std::string::npos : amp - p);
    size_t eq = kv.find('=');
    if (eq != std::string::npos && kv.substr(0, eq) == k) return kv.substr(eq + 1);
    if (amp == std::string::npos) break;
    p = amp + 1;
  }
  return def;
}
int query_int(const std::string& q, const std::string& k, int def) {
  size_t p = 0;
  while (p < q.size()) {
    size_t amp = q.find('&', p);
    std::string kv = q.substr(p, amp == std::string::npos ? std::string::npos : amp - p);
    size_t eq = kv.find('=');
    if (eq != std::string::npos && kv.substr(0, eq) == k) return atoi(kv.c_str() + eq + 1);
    if (amp == std::string::npos) break;
    p = amp + 1;
  }
  return def;
}
}  // namespace

int Server::open_listener(int port, std::string* err) {
  signal(SIGPIPE, SIG_IGN);
  int fd = socket(AF_INET6, SOCK_STREAM, 0);
  bool v6 = fd >= 0;
  if (!v6) fd = socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0) { *err = strerror(errno); return -1; }
  int one = 1, zero = 0;
  setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
  int rc;
  if (v6) {
    setsockopt(fd, IPPROTO_IPV6, IPV6_V6ONLY, &zero, sizeof zero);  // also accept IPv4
    sockaddr_in6 a{};
    a.sin6_family = AF_INET6;
    a.sin6_port = htons((uint16_t)port);
    a.sin6_addr = in6addr_any;
    rc = bind(fd, (sockaddr*)&a, sizeof a);
  } else {
    sockaddr_in a{};
    a.sin_family = AF_INET;
    a.sin_port = htons((uint16_t)port);
    a.sin_addr.s_addr = htonl(INADDR_ANY);
    rc = bind(fd, (sockaddr*)&a, sizeof a);
  }
  if (rc != 0 || ::listen(fd, 128) != 0) {
    *err = "bind/listen :" + std::to_string(port) + ": " + strerror(errno);
    close(fd);
    return -1;
  }
  return fd;
}

bool Server::listen(int port, std::string* err) {
  lfd_ = open_listener(port, err);
  return lfd_ >= 0;
}

bool Server::listen_tls(int port, TlsFront* tls, std::string* err) {
  tls_ = tls;
  tfd_ = open_listener(port, err);
  return tfd_ >= 0;
}

void Server::serve() {
  std::thread t;
  if (tfd_ >= 0) t = std::thread([this] { accept_loop(tfd_, true); });
  accept_loop(lfd_, false);
  if (t.joinable()) t.join();
}

void Server::accept_loop(int lfd, bool tls) {
  while (true) {
    int fd = accept(lfd, nullptr, nullptr);
    if (fd < 0) {
      if (errno == EINTR || errno == ECONNABORTED || errno == EMFILE) continue;
      perror("accept");
      continue;
    }
    bool started = start_thread(
        [this, fd, tls] {
          int one = 1;
          setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
          std::unique_ptr<Conn> c;
          if (tls) {
            std::string err;
            c = tls_->accept(fd, 10000, &err);  // closes fd on failure
            if (!c) return;
          } else {
            c.reset(new Conn(fd, nullptr));
          }
          handle(*c);
        },
        nullptr);
    if (!started) {
      // no thread for it (a pids or thread limit): refuse this connection
      close(fd);
      unsigned long long n = ++refused_;
      if ((n & (n - 1)) == 0) fprintf(stderr, "fleet-renderer: no thread for a new connection; refused %llu so far\n", n);
    }
  }
}

void Server::handle(Conn& c) {
  c.set_read_timeout_ms(10000);
  std::string buf;
  char tmp[4096];
  size_t he;
  while ((he = buf.find("\r\n\r\n")) == std::string::npos) {
    if (buf.size() > 16384) return respond(c, 431, "Request Header Fields Too Large", "{\"error\":\"header too large\"}");
    long r = c.read(tmp, sizeof tmp);
    if (r <= 0) return;
    buf.append(tmp, (size_t)r);
  }
  std::string head = buf.substr(0, he);
  std::string body = buf.substr(he + 4);
  size_t le = head.find("\r\n");
  std::string reqline = head.substr(0, le);
  std::map<std::string, std::string> hdr;
  size_t p = le == std::string::npos ? head.size() : le + 2;
  while (p < head.size()) {
    size_t e = head.find("\r\n", p);
    std::string line = head.substr(p, e == std::string::npos ? std::string::npos : e - p);
    size_t c = line.find(':');
    if (c != std::string::npos) hdr[lower(trim(line.substr(0, c)))] = trim(line.substr(c + 1));
    if (e == std::string::npos) break;
    p = e + 2;
  }
  size_t s1 = reqline.find(' '), s2 = reqline.rfind(' ');
  if (s1 == std::string::npos || s2 <= s1) return respond(c, 400, "Bad Request", "{\"error\":\"bad request line\"}");
  std::string method = reqline.substr(0, s1), target = reqline.substr(s1 + 1, s2 - s1 - 1);
  std::string path = target, query;
  size_t q = target.find('?');
  if (q != std::string::npos) { path = target.substr(0, q); query = target.substr(q + 1); }

  if (path == "/health" && (method == "GET" || method == "HEAD")) {
    std::string h = p_.health_json();
    if (tls_) {  // the TLS front's state, so a coordinator can see a missing or expiring certificate
      size_t e = h.rfind('}');
      if (e != std::string::npos)
        h = h.substr(0, e) + ",\"tls\":{\"loaded\":" + (tls_->loaded() ? "true" : "false") +
            ",\"loads\":" + std::to_string(tls_->generation()) + ",\"not_after\":\"" + tls_->not_after() + "\"}}";
    }
    return respond(c, 200, "OK", h, "application/json", true);
  }
  if (path == "/scene" || path == "/budget") {
    if (method != "POST") return respond(c, 405, "Method Not Allowed", "{\"error\":\"POST only\"}");
    // A JSON content type forces a CORS preflight, which this server never
    // grants, so a page in a browser cannot change the scene or the budget.
    if (lower(hdr["content-type"]).find("application/json") != 0)
      return respond(c, 415, "Unsupported Media Type", "{\"error\":\"Content-Type must be application/json\"}");
    long len = hdr.count("content-length") ? atol(hdr["content-length"].c_str()) : -1;
    if (len < 0 || len > (1 << 20)) return respond(c, 411, "Length Required", "{\"error\":\"Content-Length 0..1048576 required\"}");
    while ((long)body.size() < len) {
      long r = c.read(tmp, sizeof tmp);
      if (r <= 0) return;
      body.append(tmp, (size_t)r);
    }
    body.resize((size_t)len);
    std::string err;
    bool ok = path == "/scene" ? p_.post_scene(body, &err) : p_.post_budget(body, &err);
    if (!ok) return respond(c, 400, "Bad Request", "{\"error\":\"" + json_escape(err) + "\"}");
    return respond(c, 200, "OK", "{\"ok\":true}");
  }
  if (path == "/raw" && method == "GET") {  // debug: one unencoded frame, see PROTOCOL.md
    size_t tp = query.find("t=");
    if (tp == std::string::npos) return respond(c, 400, "Bad Request", "{\"error\":\"t required\"}");
    double t = atof(query.c_str() + tp + 2);
    std::string ppm, tile;
    if (!p_.raw_frame(t, &ppm, &tile)) return respond(c, 404, "Not Found", "{\"error\":\"that frame was not drawn\"}");
    char h[512];
    snprintf(h, sizeof h,
             "HTTP/1.1 200 OK\r\nContent-Type: image/x-portable-pixmap\r\nContent-Length: %zu\r\nX-Fleet-Tile: %s\r\nConnection: close\r\n\r\n",
             ppm.size(), tile.c_str());
    std::string out = std::string(h) + ppm;
    c.write_all(out.data(), out.size());
    return;
  }
  if (path == "/still" && (method == "GET" || method == "HEAD")) {
    // the next frame drawn, as JPEG: at the height being streamed, or with no
    // stream at tile height h (the renderer keeps drawing that way for
    // STILL_HOLD_S, so in a hold repeated requests return a converging image)
    int quality = std::clamp(query_int(query, "quality", 90), 1, 100);
    int sh = query_int(query, "h", 540);
    if (sh < 16 || sh > 2160) return respond(c, 400, "Bad Request", "{\"error\":\"h must be 16..2160\"}");
    std::vector<uint8_t> px;
    int w = 0, h = 0;
    std::string tile;
    if (!p_.next_frame(sh, &px, &w, &h, &tile)) return respond(c, 503, "Service Unavailable", "{\"error\":\"no frame drawn\"}");
    std::string jpg = jpeg_encode(px.data(), w, h, quality);
    char hd[640];
    snprintf(hd, sizeof hd,
             "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: %zu\r\nX-Fleet-Tile: %s\r\n"
             "Access-Control-Allow-Origin: *\r\nAccess-Control-Expose-Headers: X-Fleet-Tile\r\nCache-Control: no-store\r\n"
             "Connection: close\r\n\r\n",
             jpg.size(), tile.c_str());
    std::string out = std::string(hd) + (method == "HEAD" ? std::string() : jpg);
    c.write_all(out.data(), out.size());
    return;
  }
  if (path == "/frames" && method == "GET") {
    if (lower(hdr["upgrade"]) != "websocket" || hdr["sec-websocket-key"].empty())
      return respond(c, 426, "Upgrade Required", "{\"error\":\"WebSocket upgrade required\"}");
    // With the coordinator's admission keys, only a seated viewer's direct token opens a stream.
    if (!p_.admits(query_str(query, "admit", "")))
      return respond(c, 403, "Forbidden", "{\"error\":\"a direct token is required\"}");
    int h = query_int(query, "h", 0);
    if (h < 16 || h > 2160) return respond(c, 400, "Bad Request", "{\"error\":\"h must be 16..2160\"}");
    std::string qs = query_str(query, "q", "std");
    int q = quality_from_name(qs);
    if (q < 0) return respond(c, 400, "Bad Request", "{\"error\":\"q must be hi, std or lo\"}");
    return websocket(c, h, q, hdr["sec-websocket-key"]);
  }
  respond(c, 404, "Not Found", "{\"error\":\"not found\"}");
}

void Server::websocket(Conn& c, int h, int q, const std::string& key) {
  std::string resp = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " +
                     ws_accept(key) + "\r\n\r\n";
  if (!c.write_all(resp.data(), resp.size())) return;
  c.set_read_timeout_ms(0);
  c.set_option(SOL_SOCKET, SO_SNDBUF, 1 << 20);
  auto sub = p_.subscribe(h, q);
  // writer: drains the subscriber's queue
  std::thread writer;
  bool started = start_thread([&c, sub] {
    while (true) {
      std::shared_ptr<const std::string> m;
      {
        std::unique_lock<std::mutex> l(sub->mu);
        sub->cv.wait(l, [&] { return sub->closed || !sub->q.empty(); });
        if (sub->q.empty()) break;
        m = sub->q.front();
        sub->q.pop_front();
        sub->queued -= m->size();
      }
      if (!c.write_all(m->data(), m->size())) {
        sub->close();
        break;
      }
    }
    c.shutdown_both();
  }, &writer);
  if (!started) {
    // no thread for the writer (a pids or thread limit): end this stream only
    p_.unsubscribe(sub);
    unsigned long long n = ++refused_;
    if ((n & (n - 1)) == 0) fprintf(stderr, "fleet-renderer: no thread for a stream's writer; refused %llu so far\n", n);
    return;
  }
  // reader: control messages from the client
  while (true) {
    uint8_t b2[2];
    if (!c.read_exact(b2, 2)) break;
    int op = b2[0] & 0x0f;
    bool masked = b2[1] & 0x80;
    uint64_t n = b2[1] & 0x7f;
    if (n == 126) {
      uint8_t e[2];
      if (!c.read_exact(e, 2)) break;
      n = (uint64_t)e[0] << 8 | e[1];
    } else if (n == 127) {
      uint8_t e[8];
      if (!c.read_exact(e, 8)) break;
      n = 0;
      for (int i = 0; i < 8; i++) n = n << 8 | e[i];
    }
    if (n > 65536 || !masked) break;  // clients must mask; control messages are small
    uint8_t mk[4];
    if (!c.read_exact(mk, 4)) break;
    std::string pl(n, '\0');
    if (n && !c.read_exact(&pl[0], n)) break;
    for (size_t i = 0; i < n; i++) pl[i] ^= mk[i & 3];
    if (op == 8) {  // close
      sub->push(std::make_shared<const std::string>(ws_frame(8, pl.data(), std::min<size_t>(pl.size(), 2))));
      break;
    } else if (op == 9) {  // ping
      sub->push(std::make_shared<const std::string>(ws_frame(10, pl.data(), pl.size())));
    } else if (op == 1) {
      try {
        Json j = Json::parse(pl);
        if (j.has("h")) {
          int nh = (int)j.at("h").number("h");
          if (nh >= 16 && nh <= 2160) sub->want_h = nh;
        }
        if (j.has("q")) {
          int nq = quality_from_name(j.at("q").string("q"));
          if (nq >= 0) sub->want_q = nq;
        }
        p_.wake();
      } catch (const std::exception&) {
        // ignore malformed control messages
      }
    }
  }
  p_.unsubscribe(sub);
  {
    // let a queued close frame go out, then stop the writer
    std::unique_lock<std::mutex> l(sub->mu);
    sub->closed = true;
    sub->cv.notify_all();
  }
  writer.join();
}
