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

namespace {
bool write_all(int fd, const char* p, size_t n) {
  while (n) {
    ssize_t w = ::send(fd, p, n, MSG_NOSIGNAL);
    if (w < 0) {
      if (errno == EINTR) continue;
      return false;
    }
    p += w;
    n -= (size_t)w;
  }
  return true;
}
bool read_exact(int fd, void* buf, size_t n) {
  char* p = (char*)buf;
  while (n) {
    ssize_t r = ::recv(fd, p, n, 0);
    if (r == 0) return false;
    if (r < 0) {
      if (errno == EINTR) continue;
      return false;
    }
    p += r;
    n -= (size_t)r;
  }
  return true;
}
void respond(int fd, int code, const char* reason, const std::string& body, const char* ctype = "application/json",
             bool cors = false) {
  char h[512];
  snprintf(h, sizeof h,
           "HTTP/1.1 %d %s\r\nContent-Type: %s\r\nContent-Length: %zu\r\nCache-Control: no-store\r\n%sConnection: close\r\n\r\n",
           code, reason, ctype, body.size(), cors ? "Access-Control-Allow-Origin: *\r\n" : "");
  std::string out = std::string(h) + body;
  write_all(fd, out.data(), out.size());
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

bool Server::listen(int port, std::string* err) {
  signal(SIGPIPE, SIG_IGN);
  lfd_ = socket(AF_INET6, SOCK_STREAM, 0);
  bool v6 = lfd_ >= 0;
  if (!v6) lfd_ = socket(AF_INET, SOCK_STREAM, 0);
  if (lfd_ < 0) { *err = strerror(errno); return false; }
  int one = 1, zero = 0;
  setsockopt(lfd_, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
  int rc;
  if (v6) {
    setsockopt(lfd_, IPPROTO_IPV6, IPV6_V6ONLY, &zero, sizeof zero);  // also accept IPv4
    sockaddr_in6 a{};
    a.sin6_family = AF_INET6;
    a.sin6_port = htons((uint16_t)port);
    a.sin6_addr = in6addr_any;
    rc = bind(lfd_, (sockaddr*)&a, sizeof a);
  } else {
    sockaddr_in a{};
    a.sin_family = AF_INET;
    a.sin_port = htons((uint16_t)port);
    a.sin_addr.s_addr = htonl(INADDR_ANY);
    rc = bind(lfd_, (sockaddr*)&a, sizeof a);
  }
  if (rc != 0 || ::listen(lfd_, 128) != 0) { *err = std::string("bind/listen: ") + strerror(errno); return false; }
  return true;
}

void Server::serve() {
  while (true) {
    int fd = accept(lfd_, nullptr, nullptr);
    if (fd < 0) {
      if (errno == EINTR || errno == ECONNABORTED || errno == EMFILE) continue;
      perror("accept");
      continue;
    }
    std::thread([this, fd] {
      handle(fd);
      close(fd);
    }).detach();
  }
}

void Server::handle(int fd) {
  int one = 1;
  setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
  timeval tv{10, 0};
  setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
  std::string buf;
  char tmp[4096];
  size_t he;
  while ((he = buf.find("\r\n\r\n")) == std::string::npos) {
    if (buf.size() > 16384) return respond(fd, 431, "Request Header Fields Too Large", "{\"error\":\"header too large\"}");
    ssize_t r = recv(fd, tmp, sizeof tmp, 0);
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
  if (s1 == std::string::npos || s2 <= s1) return respond(fd, 400, "Bad Request", "{\"error\":\"bad request line\"}");
  std::string method = reqline.substr(0, s1), target = reqline.substr(s1 + 1, s2 - s1 - 1);
  std::string path = target, query;
  size_t q = target.find('?');
  if (q != std::string::npos) { path = target.substr(0, q); query = target.substr(q + 1); }

  if (path == "/health" && (method == "GET" || method == "HEAD")) {
    return respond(fd, 200, "OK", p_.health_json(), "application/json", true);
  }
  if (path == "/scene") {
    if (method != "POST") return respond(fd, 405, "Method Not Allowed", "{\"error\":\"POST only\"}");
    // A JSON content type forces a CORS preflight, which this server never
    // grants, so a page in a browser cannot change the scene.
    if (lower(hdr["content-type"]).find("application/json") != 0)
      return respond(fd, 415, "Unsupported Media Type", "{\"error\":\"Content-Type must be application/json\"}");
    long len = hdr.count("content-length") ? atol(hdr["content-length"].c_str()) : -1;
    if (len < 0 || len > (1 << 20)) return respond(fd, 411, "Length Required", "{\"error\":\"Content-Length 0..1048576 required\"}");
    while ((long)body.size() < len) {
      ssize_t r = recv(fd, tmp, sizeof tmp, 0);
      if (r <= 0) return;
      body.append(tmp, (size_t)r);
    }
    body.resize((size_t)len);
    std::string err;
    if (!p_.post_scene(body, &err)) return respond(fd, 400, "Bad Request", "{\"error\":\"" + json_escape(err) + "\"}");
    return respond(fd, 200, "OK", "{\"ok\":true}");
  }
  if (path == "/raw" && method == "GET") {  // debug: one unencoded frame, see PROTOCOL.md
    size_t tp = query.find("t=");
    if (tp == std::string::npos) return respond(fd, 400, "Bad Request", "{\"error\":\"t required\"}");
    double t = atof(query.c_str() + tp + 2);
    std::string ppm, tile;
    if (!p_.raw_frame(t, &ppm, &tile)) return respond(fd, 404, "Not Found", "{\"error\":\"that frame was not drawn\"}");
    char h[512];
    snprintf(h, sizeof h,
             "HTTP/1.1 200 OK\r\nContent-Type: image/x-portable-pixmap\r\nContent-Length: %zu\r\nX-Fleet-Tile: %s\r\nConnection: close\r\n\r\n",
             ppm.size(), tile.c_str());
    std::string out = std::string(h) + ppm;
    write_all(fd, out.data(), out.size());
    return;
  }
  if (path == "/still" && (method == "GET" || method == "HEAD")) {
    // the next frame drawn, as JPEG: at the height being streamed, or with no
    // stream at tile height h (the renderer keeps drawing that way for
    // STILL_HOLD_S, so in a hold repeated requests return a converging image)
    int quality = std::clamp(query_int(query, "quality", 90), 1, 100);
    int sh = query_int(query, "h", 540);
    if (sh < 16 || sh > 2160) return respond(fd, 400, "Bad Request", "{\"error\":\"h must be 16..2160\"}");
    std::vector<uint8_t> px;
    int w = 0, h = 0;
    std::string tile;
    if (!p_.next_frame(sh, &px, &w, &h, &tile)) return respond(fd, 503, "Service Unavailable", "{\"error\":\"no frame drawn\"}");
    std::string jpg = jpeg_encode(px.data(), w, h, quality);
    char hd[640];
    snprintf(hd, sizeof hd,
             "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: %zu\r\nX-Fleet-Tile: %s\r\n"
             "Access-Control-Allow-Origin: *\r\nAccess-Control-Expose-Headers: X-Fleet-Tile\r\nCache-Control: no-store\r\n"
             "Connection: close\r\n\r\n",
             jpg.size(), tile.c_str());
    std::string out = std::string(hd) + (method == "HEAD" ? std::string() : jpg);
    write_all(fd, out.data(), out.size());
    return;
  }
  if (path == "/frames" && method == "GET") {
    if (lower(hdr["upgrade"]) != "websocket" || hdr["sec-websocket-key"].empty())
      return respond(fd, 426, "Upgrade Required", "{\"error\":\"WebSocket upgrade required\"}");
    int h = query_int(query, "h", 0);
    if (h < 16 || h > 2160) return respond(fd, 400, "Bad Request", "{\"error\":\"h must be 16..2160\"}");
    std::string qs = query_str(query, "q", "std");
    int q = quality_from_name(qs);
    if (q < 0) return respond(fd, 400, "Bad Request", "{\"error\":\"q must be hi, std or lo\"}");
    return websocket(fd, h, q, hdr["sec-websocket-key"]);
  }
  respond(fd, 404, "Not Found", "{\"error\":\"not found\"}");
}

void Server::websocket(int fd, int h, int q, const std::string& key) {
  std::string resp = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " +
                     ws_accept(key) + "\r\n\r\n";
  if (!write_all(fd, resp.data(), resp.size())) return;
  timeval tv{0, 0};
  setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
  int sndbuf = 1 << 20;
  setsockopt(fd, SOL_SOCKET, SO_SNDBUF, &sndbuf, sizeof sndbuf);
  auto sub = p_.subscribe(h, q);
  // writer: drains the subscriber's queue
  std::thread writer([fd, sub] {
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
      if (!write_all(fd, m->data(), m->size())) {
        sub->close();
        break;
      }
    }
    shutdown(fd, SHUT_RDWR);
  });
  // reader: control messages from the client
  while (true) {
    uint8_t b2[2];
    if (!read_exact(fd, b2, 2)) break;
    int op = b2[0] & 0x0f;
    bool masked = b2[1] & 0x80;
    uint64_t n = b2[1] & 0x7f;
    if (n == 126) {
      uint8_t e[2];
      if (!read_exact(fd, e, 2)) break;
      n = (uint64_t)e[0] << 8 | e[1];
    } else if (n == 127) {
      uint8_t e[8];
      if (!read_exact(fd, e, 8)) break;
      n = 0;
      for (int i = 0; i < 8; i++) n = n << 8 | e[i];
    }
    if (n > 65536 || !masked) break;  // clients must mask; control messages are small
    uint8_t mk[4];
    if (!read_exact(fd, mk, 4)) break;
    std::string pl(n, '\0');
    if (n && !read_exact(fd, &pl[0], n)) break;
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
