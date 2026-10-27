// viewerload: many synthetic viewers of renderer frame streams, subscribing as the
// page does (GET /frames?h=H[&q=Q] over ws:// or wss://), recording per frame its
// arrival delay (arrival on this clock minus the frame's slot time, epoch + t) and
// per connection missing frame numbers and repeated stream headers (the renderer
// re-sends the header after it drops a slow reader to the next keyframe).
//
//   viewerload --epoch-ms E --conns N [--h 270] [--q std] [--warmup 5] [--secs 20]
//              [--threads 16] [--connect-threads 64] [--out FILE] URL...
//
// URL is ws://HOST:PORT/frames or wss://HOST:PORT/frames (HOST may be [v6]); each
// gets N connections. The certificate is not verified. Prints one JSON object per
// URL and one for all of them (also written to FILE).
//
// Build: g++ -O2 -std=c++17 -pthread -o viewerload viewerload.cpp -lssl -lcrypto
#include <arpa/inet.h>
#include <fcntl.h>
#include <netdb.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <openssl/err.h>
#include <openssl/rand.h>
#include <openssl/ssl.h>
#include <poll.h>
#include <signal.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <cerrno>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

namespace {

double now_ms() {
  timespec ts;
  clock_gettime(CLOCK_REALTIME, &ts);
  return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
}

struct Url {
  bool tls = false;
  std::string host, port, path, text;
};

bool parse_url(const std::string& s, Url* u) {
  u->text = s;
  std::string rest;
  if (s.rfind("wss://", 0) == 0) { u->tls = true; rest = s.substr(6); }
  else if (s.rfind("ws://", 0) == 0) { u->tls = false; rest = s.substr(5); }
  else return false;
  size_t slash = rest.find('/');
  std::string hp = rest.substr(0, slash);
  u->path = slash == std::string::npos ? "/" : rest.substr(slash);
  if (!hp.empty() && hp[0] == '[') {
    size_t e = hp.find(']');
    if (e == std::string::npos) return false;
    u->host = hp.substr(1, e - 1);
    u->port = e + 2 <= hp.size() ? hp.substr(e + 2) : "";
  } else {
    size_t c = hp.rfind(':');
    u->host = hp.substr(0, c);
    u->port = c == std::string::npos ? "" : hp.substr(c + 1);
  }
  if (u->port.empty()) u->port = u->tls ? "443" : "80";
  return true;
}

// delay histogram: 0.25 ms buckets up to 30 s, the last bucket holds the rest
constexpr int kBuckets = 120000;
constexpr double kBucketMs = 0.25;

struct Stats {
  uint64_t frames = 0, bytes = 0, keyframes = 0, gap_events = 0, gap_frames = 0, headers = 0, extra_headers = 0;
  std::vector<uint32_t> hist = std::vector<uint32_t>(kBuckets, 0);
  double delay_max = 0, delay_min = 1e18;
  void add_delay(double d) {
    int b = d < 0 ? 0 : (int)std::min<double>(kBuckets - 1, d / kBucketMs);
    hist[b]++;
    delay_max = std::max(delay_max, d);
    delay_min = std::min(delay_min, d);
  }
  void merge(const Stats& o) {
    frames += o.frames; bytes += o.bytes; keyframes += o.keyframes; gap_events += o.gap_events;
    gap_frames += o.gap_frames; headers += o.headers; extra_headers += o.extra_headers;
    for (int i = 0; i < kBuckets; i++) hist[i] += o.hist[i];
    delay_max = std::max(delay_max, o.delay_max);
    delay_min = std::min(delay_min, o.delay_min);
  }
  double pct(double q) const {
    uint64_t n = 0;
    for (auto v : hist) n += v;
    if (!n) return -1;
    uint64_t want = (uint64_t)std::ceil(q * n), acc = 0;
    if (want == 0) want = 1;
    for (int i = 0; i < kBuckets; i++) {
      acc += hist[i];
      if (acc >= want) return (i + 0.5) * kBucketMs;
    }
    return kBuckets * kBucketMs;
  }
};

struct Conn {
  int url = 0;
  int fd = -1;
  SSL* ssl = nullptr;
  std::string buf;   // unparsed bytes
  bool have_prev = false;
  uint32_t prev = 0;
  uint64_t frames_in_window = 0, bytes_in_window = 0;
  int headers = 0;
  bool closed = false;
  bool closed_in_window = false;
};

struct Shared {
  std::vector<Url> urls;
  double epoch_ms = 0;
  // measurement window, unix ms: set by main once every connection is open, read by the workers
  std::atomic<double> win0{1e18}, win1{1e18};
  std::atomic<bool> stop{false};
};

SSL_CTX* g_ctx = nullptr;

bool wait_fd(int fd, short ev, int ms) {
  pollfd p{fd, ev, 0};
  int r;
  do r = poll(&p, 1, ms); while (r < 0 && errno == EINTR);
  return r > 0;
}

// Blocking connect, TLS handshake and WebSocket upgrade; then the socket is non-blocking.
bool open_conn(const Url& u, const std::string& query, Conn* c, std::string* err) {
  addrinfo hints{}, *res = nullptr;
  hints.ai_socktype = SOCK_STREAM;
  int gr = getaddrinfo(u.host.c_str(), u.port.c_str(), &hints, &res);
  if (gr != 0) { *err = gai_strerror(gr); return false; }
  int fd = socket(res->ai_family, SOCK_STREAM | SOCK_NONBLOCK, 0);
  if (fd < 0) { *err = strerror(errno); freeaddrinfo(res); return false; }
  int rc = connect(fd, res->ai_addr, res->ai_addrlen);
  freeaddrinfo(res);
  if (rc != 0 && errno != EINPROGRESS) { *err = std::string("connect: ") + strerror(errno); close(fd); return false; }
  if (rc != 0) {
    if (!wait_fd(fd, POLLOUT, 15000)) { *err = "connect timeout"; close(fd); return false; }
    int so = 0; socklen_t sl = sizeof so;
    getsockopt(fd, SOL_SOCKET, SO_ERROR, &so, &sl);
    if (so) { *err = std::string("connect: ") + strerror(so); close(fd); return false; }
  }
  int one = 1;
  setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
  c->fd = fd;
  double deadline = now_ms() + 15000;
  auto io_wait = [&](int e) {
    int left = (int)(deadline - now_ms());
    if (left <= 0) return false;
    return wait_fd(fd, e == SSL_ERROR_WANT_WRITE ? POLLOUT : POLLIN, left);
  };
  if (u.tls) {
    c->ssl = SSL_new(g_ctx);
    SSL_set_fd(c->ssl, fd);
    SSL_set_tlsext_host_name(c->ssl, "viewerload.invalid");
    while (true) {
      int r = SSL_connect(c->ssl);
      if (r == 1) break;
      int e = SSL_get_error(c->ssl, r);
      if ((e != SSL_ERROR_WANT_READ && e != SSL_ERROR_WANT_WRITE) || !io_wait(e)) {
        *err = "TLS handshake failed";
        return false;
      }
    }
  }
  unsigned char kb[16];
  RAND_bytes(kb, sizeof kb);
  static const char* b64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  std::string key;
  for (int i = 0; i < 16; i += 3) {
    uint32_t v = kb[i] << 16 | (i + 1 < 16 ? kb[i + 1] << 8 : 0) | (i + 2 < 16 ? kb[i + 2] : 0);
    key += b64[(v >> 18) & 63];
    key += b64[(v >> 12) & 63];
    key += i + 1 < 16 ? b64[(v >> 6) & 63] : '=';
    key += i + 2 < 16 ? b64[v & 63] : '=';
  }
  std::string hostport = u.host.find(':') != std::string::npos ? "[" + u.host + "]:" + u.port : u.host + ":" + u.port;
  std::string req = "GET " + u.path + query + " HTTP/1.1\r\nHost: " + hostport +
                    "\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: " + key +
                    "\r\n\r\n";
  size_t off = 0;
  while (off < req.size()) {
    long w;
    if (c->ssl) {
      w = SSL_write(c->ssl, req.data() + off, (int)(req.size() - off));
      if (w <= 0) {
        int e = SSL_get_error(c->ssl, (int)w);
        if ((e != SSL_ERROR_WANT_READ && e != SSL_ERROR_WANT_WRITE) || !io_wait(e)) { *err = "write request"; return false; }
        continue;
      }
    } else {
      w = send(fd, req.data() + off, req.size() - off, MSG_NOSIGNAL);
      if (w < 0) {
        if ((errno != EAGAIN && errno != EINTR) || !io_wait(SSL_ERROR_WANT_WRITE)) { *err = "write request"; return false; }
        continue;
      }
    }
    off += (size_t)w;
  }
  // response head
  char tmp[4096];
  while (c->buf.find("\r\n\r\n") == std::string::npos) {
    long r;
    if (c->ssl) {
      r = SSL_read(c->ssl, tmp, sizeof tmp);
      if (r <= 0) {
        int e = SSL_get_error(c->ssl, (int)r);
        if ((e != SSL_ERROR_WANT_READ && e != SSL_ERROR_WANT_WRITE) || !io_wait(e)) { *err = "read response"; return false; }
        continue;
      }
    } else {
      r = recv(fd, tmp, sizeof tmp, 0);
      if (r == 0) { *err = "closed before response"; return false; }
      if (r < 0) {
        if ((errno != EAGAIN && errno != EINTR) || !io_wait(SSL_ERROR_WANT_READ)) { *err = "read response"; return false; }
        continue;
      }
    }
    c->buf.append(tmp, (size_t)r);
    if (c->buf.size() > 65536) { *err = "response head too large"; return false; }
  }
  if (c->buf.rfind("HTTP/1.1 101", 0) != 0) {
    *err = "no upgrade: " + c->buf.substr(0, c->buf.find("\r\n"));
    return false;
  }
  c->buf.erase(0, c->buf.find("\r\n\r\n") + 4);
  return true;
}

struct Worker {
  int ep = -1;
  std::vector<Conn*> conns;
  std::vector<Stats> per_url;
  std::mutex mu;  // guards conns while connecting
};

// Parse complete WebSocket frames from c->buf.
void consume(Conn* c, Shared& sh, Worker& w, double at) {
  Stats& st = w.per_url[c->url];
  bool in_win = at >= sh.win0.load() && at < sh.win1.load();
  size_t p = 0;
  const std::string& b = c->buf;
  while (b.size() - p >= 2) {
    uint8_t b0 = (uint8_t)b[p], b1 = (uint8_t)b[p + 1];
    int op = b0 & 0x0f;
    uint64_t n = b1 & 0x7f;
    size_t hl = 2;
    if (n == 126) {
      if (b.size() - p < 4) break;
      n = (uint64_t)(uint8_t)b[p + 2] << 8 | (uint8_t)b[p + 3];
      hl = 4;
    } else if (n == 127) {
      if (b.size() - p < 10) break;
      n = 0;
      for (int i = 0; i < 8; i++) n = n << 8 | (uint8_t)b[p + 2 + i];
      hl = 10;
    }
    if (b1 & 0x80) hl += 4;  // a server never masks; tolerate it anyway
    if (b.size() - p < hl + n) break;
    const uint8_t* pl = (const uint8_t*)b.data() + p + hl;
    if (op == 2 && n >= 22 && memcmp(pl, "FCVF", 4) == 0) {
      bool key = pl[5] & 1;
      uint64_t tb = 0;
      for (int i = 0; i < 8; i++) tb |= (uint64_t)pl[10 + i] << (8 * i);
      double t;
      memcpy(&t, &tb, 8);
      uint32_t fr = 0;
      for (int i = 0; i < 4; i++) fr |= (uint32_t)pl[18 + i] << (8 * i);
      if (in_win) {
        st.frames++;
        st.bytes += n + hl;
        c->frames_in_window++;
        c->bytes_in_window += n + hl;
        if (key) st.keyframes++;
        st.add_delay(at - (sh.epoch_ms + t * 1000.0));
        if (c->have_prev && fr != c->prev + 1) {
          st.gap_events++;
          if (fr > c->prev) st.gap_frames += fr - c->prev - 1;
        }
      }
      c->have_prev = true;
      c->prev = fr;
    } else if (op == 1) {
      c->headers++;
      if (in_win) {
        st.headers++;
        if (c->headers > 1) st.extra_headers++;
      }
    } else if (op == 8) {
      c->closed = true;
    }
    p += hl + n;
  }
  c->buf.erase(0, p);
}

void close_conn(Conn* c, Shared& sh, double at) {
  if (c->closed && c->fd < 0) return;
  c->closed = true;
  if (at >= sh.win0.load() && at < sh.win1.load()) c->closed_in_window = true;
  if (c->fd >= 0) {
    close(c->fd);
    c->fd = -1;
  }
}

void work(Worker* w, Shared* sh) {
  std::vector<epoll_event> evs(1024);
  std::vector<char> tmp(1 << 18);
  while (!sh->stop) {
    int n = epoll_wait(w->ep, evs.data(), (int)evs.size(), 100);
    for (int i = 0; i < n; i++) {
      Conn* c = (Conn*)evs[i].data.ptr;
      if (c->fd < 0) continue;
      while (true) {
        long r;
        if (c->ssl) {
          r = SSL_read(c->ssl, tmp.data(), (int)tmp.size());
          if (r <= 0) {
            int e = SSL_get_error(c->ssl, (int)r);
            if (e == SSL_ERROR_WANT_READ || e == SSL_ERROR_WANT_WRITE) break;
            close_conn(c, *sh, now_ms());
            break;
          }
        } else {
          r = recv(c->fd, tmp.data(), tmp.size(), 0);
          if (r == 0) { close_conn(c, *sh, now_ms()); break; }
          if (r < 0) {
            if (errno == EAGAIN || errno == EINTR) break;
            close_conn(c, *sh, now_ms());
            break;
          }
        }
        c->buf.append(tmp.data(), (size_t)r);
        consume(c, *sh, *w, now_ms());
        if (c->closed) { close_conn(c, *sh, now_ms()); break; }
      }
    }
  }
}

std::string jnum(double v) {
  char b[64];
  if (v < 0 || std::isnan(v)) return "null";
  snprintf(b, sizeof b, "%.2f", v);
  return b;
}

}  // namespace

int main(int argc, char** argv) {
  int conns = 1, threads = 16, cthreads = 64, h = 270;
  double warmup = 5, secs = 20, epoch = -1;
  std::string q, out;
  std::vector<std::string> urls;
  for (int i = 1; i < argc; i++) {
    std::string a = argv[i];
    auto next = [&]() -> std::string {
      if (i + 1 >= argc) { fprintf(stderr, "viewerload: %s needs a value\n", a.c_str()); exit(2); }
      return argv[++i];
    };
    if (a == "--conns") conns = atoi(next().c_str());
    else if (a == "--threads") threads = atoi(next().c_str());
    else if (a == "--connect-threads") cthreads = atoi(next().c_str());
    else if (a == "--h") h = atoi(next().c_str());
    else if (a == "--q") q = next();
    else if (a == "--warmup") warmup = atof(next().c_str());
    else if (a == "--secs") secs = atof(next().c_str());
    else if (a == "--epoch-ms") epoch = atof(next().c_str());
    else if (a == "--out") out = next();
    else if (a.rfind("ws", 0) == 0) urls.push_back(a);
    else { fprintf(stderr, "viewerload: unknown argument %s\n", a.c_str()); return 2; }
  }
  if (urls.empty() || epoch < 0 || conns < 1 || threads < 1) {
    fprintf(stderr, "usage: viewerload --epoch-ms E --conns N [--h H] [--q Q] [--warmup S] [--secs S] [--threads T] URL...\n");
    return 2;
  }
  signal(SIGPIPE, SIG_IGN);
  g_ctx = SSL_CTX_new(TLS_client_method());
  SSL_CTX_set_verify(g_ctx, SSL_VERIFY_NONE, nullptr);
  SSL_CTX_set_mode(g_ctx, SSL_MODE_ENABLE_PARTIAL_WRITE | SSL_MODE_ACCEPT_MOVING_WRITE_BUFFER);
  Shared sh;
  sh.epoch_ms = epoch;
  for (auto& s : urls) {
    Url u;
    if (!parse_url(s, &u)) { fprintf(stderr, "viewerload: bad url %s\n", s.c_str()); return 2; }
    sh.urls.push_back(u);
  }
  std::string query = "?h=" + std::to_string(h) + (q.empty() || q == "std" ? "" : "&q=" + q);

  std::vector<std::unique_ptr<Worker>> ws;
  for (int i = 0; i < threads; i++) {
    auto w = std::make_unique<Worker>();
    w->ep = epoll_create1(0);
    w->per_url.resize(sh.urls.size());
    ws.push_back(std::move(w));
  }
  std::vector<std::thread> wt;
  for (auto& w : ws) wt.emplace_back(work, w.get(), &sh);

  // connect: every URL's connections interleaved, so tiles fill at the same pace
  std::vector<std::pair<int, int>> todo;
  for (int k = 0; k < conns; k++)
    for (int u = 0; u < (int)sh.urls.size(); u++) todo.push_back({u, k});
  std::atomic<size_t> next{0};
  std::vector<std::atomic<int>> ok(sh.urls.size()), failed(sh.urls.size());
  std::mutex emu;
  std::vector<std::string> first_err(sh.urls.size());
  double c0 = now_ms();
  std::vector<double> connect_ms;
  std::mutex cmu;
  std::vector<std::thread> ct;
  for (int i = 0; i < cthreads; i++)
    ct.emplace_back([&] {
      while (true) {
        size_t j = next++;
        if (j >= todo.size()) return;
        int u = todo[j].first;
        auto* c = new Conn();
        c->url = u;
        std::string err;
        double t0 = now_ms();
        if (!open_conn(sh.urls[u], query, c, &err)) {
          failed[u]++;
          {
            std::lock_guard<std::mutex> l(emu);
            if (first_err[u].empty()) first_err[u] = err;
          }
          if (c->ssl) SSL_free(c->ssl);
          if (c->fd >= 0) close(c->fd);
          delete c;
          continue;
        }
        {
          std::lock_guard<std::mutex> l(cmu);
          connect_ms.push_back(now_ms() - t0);
        }
        ok[u]++;
        Worker& w = *ws[j % ws.size()];
        {
          std::lock_guard<std::mutex> l(w.mu);
          w.conns.push_back(c);
        }
        epoll_event ev{};
        ev.events = EPOLLIN | EPOLLRDHUP;
        ev.data.ptr = c;
        // what came with the response head (before the window, so only the connection's
        // own fields change)
        consume(c, sh, w, now_ms());
        epoll_ctl(w.ep, EPOLL_CTL_ADD, c->fd, &ev);
      }
    });
  for (auto& t : ct) t.join();
  double c1 = now_ms();
  sh.win1 = now_ms() + (warmup + secs) * 1000.0;  // win1 first: a worker never sees win0 set and win1 not
  sh.win0 = now_ms() + warmup * 1000.0;
  fprintf(stderr, "viewerload: %zu connections opened in %.0f ms; measuring %.0f s after %.0f s\n", connect_ms.size(),
          c1 - c0, secs, warmup);
  while (now_ms() < sh.win1.load() + 200) usleep(50000);
  sh.stop = true;
  for (auto& t : wt) t.join();

  std::sort(connect_ms.begin(), connect_ms.end());
  auto cpct = [&](double p) { return connect_ms.empty() ? -1.0 : connect_ms[std::min(connect_ms.size() - 1, (size_t)(p * connect_ms.size()))]; };
  std::string res = "{\"conns_per_url\":" + std::to_string(conns) + ",\"h\":" + std::to_string(h) + ",\"q\":\"" +
                    (q.empty() ? "std" : q) + "\",\"secs\":" + jnum(secs) + ",\"connect_ms_p50\":" + jnum(cpct(0.5)) +
                    ",\"connect_ms_p95\":" + jnum(cpct(0.95)) + ",\"connect_ms_max\":" + jnum(connect_ms.empty() ? -1 : connect_ms.back()) +
                    ",\"connect_wall_ms\":" + jnum(c1 - c0) + ",\"win0_ms\":" + jnum(sh.win0.load()) + ",\"win1_ms\":" + jnum(sh.win1.load()) + ",\"urls\":[";
  Stats all;
  uint64_t all_closed = 0;
  int all_ok = 0, all_failed = 0;
  double all_fps_min = 1e18, worst_p50 = -1, worst_p95 = -1;
  for (size_t u = 0; u < sh.urls.size(); u++) {
    Stats st;
    uint64_t closed = 0;
    double fps_min = 1e18;
    for (auto& w : ws) {
      st.merge(w->per_url[u]);
      for (Conn* c : w->conns)
        if (c->url == (int)u) {
          if (c->closed_in_window) closed++;
          fps_min = std::min(fps_min, c->frames_in_window / secs);
        }
    }
    all.merge(st);
    all_closed += closed;
    all_ok += ok[u];
    all_failed += failed[u];
    all_fps_min = std::min(all_fps_min, fps_min);
    if (st.frames) {
      worst_p50 = std::max(worst_p50, st.pct(0.5) - st.delay_min);
      worst_p95 = std::max(worst_p95, st.pct(0.95) - st.delay_min);
    }
    double nc = std::max(1, ok[u].load());
    res += std::string(u ? "," : "") + "{\"url\":\"" + sh.urls[u].text + "\",\"ok\":" + std::to_string(ok[u]) +
           ",\"failed\":" + std::to_string(failed[u]) + ",\"first_error\":\"" + first_err[u] + "\",\"closed_in_window\":" +
           std::to_string(closed) + ",\"fps_per_conn\":" + jnum(st.frames / nc / secs) + ",\"fps_min_conn\":" +
           jnum(fps_min > 1e17 ? -1 : fps_min) + ",\"mbit_per_conn\":" + jnum(st.bytes * 8.0 / nc / secs / 1e6) +
           ",\"delay_p50_ms\":" + jnum(st.pct(0.5)) + ",\"delay_p95_ms\":" + jnum(st.pct(0.95)) +
           ",\"delay_p99_ms\":" + jnum(st.pct(0.99)) + ",\"delay_max_ms\":" + jnum(st.delay_max) +
           ",\"delay_min_ms\":" + jnum(st.frames ? st.delay_min : -1) +
           ",\"p50_over_min_ms\":" + jnum(st.frames ? st.pct(0.5) - st.delay_min : -1) +
           ",\"p95_over_min_ms\":" + jnum(st.frames ? st.pct(0.95) - st.delay_min : -1) +
           ",\"gap_events\":" + std::to_string(st.gap_events) + ",\"gap_frames\":" + std::to_string(st.gap_frames) +
           ",\"extra_headers\":" + std::to_string(st.extra_headers) + ",\"keyframes_per_conn\":" + jnum(st.keyframes / nc) + "}";
  }
  double nc = std::max(1, all_ok);
  res += "],\"all\":{\"ok\":" + std::to_string(all_ok) + ",\"failed\":" + std::to_string(all_failed) +
         ",\"closed_in_window\":" + std::to_string(all_closed) + ",\"fps_per_conn\":" + jnum(all.frames / nc / secs) +
         ",\"fps_min_conn\":" + jnum(all_fps_min > 1e17 ? -1 : all_fps_min) + ",\"mbit_per_conn\":" +
         jnum(all.bytes * 8.0 / nc / secs / 1e6) + ",\"gbit_total\":" + jnum(all.bytes * 8.0 / secs / 1e9) +
         ",\"delay_p50_ms\":" + jnum(all.pct(0.5)) + ",\"delay_p95_ms\":" + jnum(all.pct(0.95)) +
         ",\"delay_p99_ms\":" + jnum(all.pct(0.99)) + ",\"delay_max_ms\":" + jnum(all.delay_max) +
         ",\"worst_url_p50_over_min_ms\":" + jnum(worst_p50) + ",\"worst_url_p95_over_min_ms\":" + jnum(worst_p95) +
         ",\"gap_events\":" + std::to_string(all.gap_events) + ",\"gap_frames\":" + std::to_string(all.gap_frames) +
         ",\"extra_headers\":" + std::to_string(all.extra_headers) + ",\"keyframes_per_conn\":" + jnum(all.keyframes / nc) + "}}";
  printf("%s\n", res.c_str());
  if (!out.empty()) {
    FILE* f = fopen(out.c_str(), "w");
    if (f) { fprintf(f, "%s\n", res.c_str()); fclose(f); }
  }
  return 0;
}
