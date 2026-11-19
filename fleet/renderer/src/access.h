// Who may change a fleet renderer's scene and open its streams (fleet/PROTOCOL.md, "Access").
//
// The scene token is a secret the coordinator holds. It reaches a renderer the way its
// certificate does: as `scene_token` in the JSON file TLS_CERT_FILE names (a VM on a host
// agent), or as the whole of the file SCENE_TOKEN_FILE names (a tile instance, whose nginx
// holds the certificate). Either variable makes this a fleet renderer: POST /scene and
// /budget need `Authorization: Bearer <token>`, and GET /frames is refused until the first
// scene has said whether streams need a direct token.
#pragma once
#include <stdint.h>
#include <sys/socket.h>
#include <sys/stat.h>

#include <functional>
#include <map>
#include <mutex>
#include <string>
#include <utility>

class SceneToken {
 public:
  // `json`: the file is the certificate JSON (field scene_token), else it holds the token.
  void configure(const std::string& path, bool json);
  // From the environment: SCENE_TOKEN_FILE (the token), else TLS_CERT_FILE (the certificate
  // JSON's scene_token); neither leaves it unconfigured.
  void configure_from_env();
  // Whether this renderer runs for the fleet (a token file is named).
  bool configured() const { return !path_.empty(); }
  // The token now, read again whenever the file changed (a new inode, size or
  // modification time); empty while none can be read.
  std::string current();

 private:
  std::string path_;
  bool json_ = false;
  std::mutex mu_;
  struct stat st_ {};
  bool have_st_ = false;
  std::string token_;
};

// The token in the file's text: the JSON field, or the trimmed text. Empty when there is none.
std::string scene_token_from_text(const std::string& text, bool json);

// Equal strings, compared in time that depends only on their lengths.
bool constant_time_equal(const std::string& a, const std::string& b);

// POST /scene and /budget: 0 to go ahead, 503 when a fleet renderer has no token to check
// against, 401 when the Authorization header does not carry it. Not a fleet renderer: 0.
int scene_access(bool fleet, const std::string& token, const std::string& authorization);

// GET /frames: 0 to go ahead, 503 when a fleet renderer has had no scene yet.
int frames_access(bool fleet, bool has_scene);

// How long a stream that needs a direct token waits for the client's first message.
constexpr int ADMIT_WAIT_MS = 5000;

// The direct token in a client's first message, `{"admit":"<token>"}`; false when the text
// is not such a message.
bool admit_message(const std::string& text, std::string* token);

// What a fleet server streams (fleet/PROTOCOL.md, "Access", "Stream limits").
//
// At most this many frame streams at once: the coordinator's seats (--direct-capacity, at most
// MAX_SEATS in src/fleet/coord/admission.rs) and STREAM_RESERVE there for the aggregators that
// read a tile. The test `the_tile_stream_cap_covers_every_seat` holds the two files together.
constexpr size_t MAX_FLEET_STREAMS = 512;
// The tallest stream: the largest height the page asks for (STREAM_HEIGHTS in
// fleet/web/core/layout.ts, already scaled by the device pixel ratio).
constexpr int FLEET_MAX_STREAM_H = 1080;
// The quality a stream may ask for (quality_from_name: 0 lo, 1 std, 2 hi). The page asks std or
// lo; q=hi (up to 8 times std's bit rate) is for an aggregator, whose own tokens name a viewer
// `aggregator-<id>`. With admission off no token names anyone, and any quality is served.
int fleet_max_quality(bool admission, const std::string& viewer);

// The frame streams open on one server. A stream admitted for a viewer (the one its direct
// token names) replaces any stream already open for the same viewer, so a reloaded page gets its
// stream at once and one token never holds two; past `cap` streams a new one is refused.
class StreamTable {
 public:
  explicit StreamTable(size_t cap) : cap_(cap) {}
  // Takes a place for a stream of `viewer` (empty: admission is off and no viewer is named).
  // A stream already open for the same viewer is stopped: its `stop` is called once, under the
  // table's lock, so it runs while that stream's connection is still open (its close() waits for
  // the lock). The new stream takes the place whatever the count. Otherwise returns false when
  // `cap` streams are open. On true, *id is what close() takes.
  bool open(const std::string& viewer, std::function<void()> stop, uint64_t* id);
  // The stream has ended (its connection is about to close): frees its place.
  void close(uint64_t id);
  size_t size();
  // Tests: a smaller cap.
  void set_cap(size_t cap);

 private:
  struct Entry {
    std::string viewer;
    std::function<void()> stop;
  };
  std::mutex mu_;
  size_t cap_;
  uint64_t next_ = 1;
  std::map<uint64_t, Entry> open_;
  std::map<std::string, uint64_t> by_viewer_;  // the newest stream of each viewer
};

// Connections that have reached no stream yet (fleet/PROTOCOL.md, "Connection limits"): each
// holds a thread from accept until it is admitted to a stream or a mirror, or its request is
// answered, or a deadline below passes.
//
// At most this many at once, refused (closed at accept, before a thread is started) past it.
// It equals MAX_FLEET_STREAMS: every seat and every aggregator reader can reconnect at the same
// moment (a renderer restart, a viewer network blip) and still be served, and with every stream
// open (two threads each) the server holds at most 512 + 2 x 512 connection threads, inside the
// 2048 pids a container gets by default (threads.h).
constexpr size_t MAX_PENDING_CONNECTIONS = MAX_FLEET_STREAMS;
// At most this many from one source: an IPv4 address, or an IPv6 /64 (an IPv6 subscriber
// usually holds a whole /64; the host's Minecraft join limit counts by it too,
// src/fleet/agent/joinguard.rs). A page opens one stream to a renderer, and a reload one more
// while the older closes; 16 leaves room for several viewers behind one NAT address or /64
// joining at once, and an anonymous client needs 32 sources to hold every pending place.
constexpr size_t MAX_PENDING_PER_SOURCE = 16;
// The TLS handshake and the request header together, from accept: a client that has sent no
// whole request header by then is closed. Each had 10 s of its own before. A healthy link needs
// one round trip (fleet/renderer/README.md, "Connections that reach no stream": 0.6 s at p99
// for a 600 ms round trip), and a lossy one waits out retransmission timeouts (at a 300 ms round
// trip, 3.2 s at p99 with 5% loss each way, 6.8 s with 10%). The page gives a frame socket 5 s,
// then 10 s, to open (frameOpenDeadlineMs, fleet/web/core/net.ts), so a link that slow connects
// on its second try; the caps above, not this wait, are what bound an anonymous client.
constexpr int REQUEST_WAIT_MS = 10000;

// The source a connection is counted under for MAX_PENDING_PER_SOURCE: a key for an IPv4
// address (an IPv4-mapped IPv6 address is its IPv4 address) or an IPv6 /64. Empty for an
// address no viewer connects from directly: loopback, private (10/8, 172.16/12, 192.168/16,
// 100.64/10, fc00::/7) and link-local addresses, which are this host's relays (a tile's nginx,
// a GPU host's published-port proxy and IPv4 front) carrying many viewers each, and for an
// address family that is neither. Such a connection counts against MAX_PENDING_CONNECTIONS only.
std::string pending_source(const sockaddr* addr, size_t len);

// The places of the connections that have reached no stream. Memory is bounded by the cap: a
// source is kept only while it holds a place.
class PendingTable {
 public:
  PendingTable(size_t cap, size_t per_source) : cap_(cap), per_source_(per_source) {}
  // Takes a place for a connection from `source` (pending_source; empty: counted only in all).
  // False, and nothing taken, when `cap` connections are pending, or `per_source` from `source`.
  bool take(const std::string& source);
  // Gives back a place take() gave.
  void give_back(const std::string& source);
  size_t size();
  size_t from(const std::string& source);
  // Tests: other caps.
  void set_caps(size_t cap, size_t per_source);

 private:
  std::mutex mu_;
  size_t cap_, per_source_;
  size_t open_ = 0;
  std::map<std::string, size_t> by_source_;
};

// One place in a PendingTable, given back when this is destroyed or release() is called,
// whichever is first: every way a connection's thread ends gives its place back, and a
// connection admitted to a stream gives it back as it starts streaming.
class PendingPlace {
 public:
  PendingPlace(PendingTable* table, std::string source) : table_(table), source_(std::move(source)) {}
  ~PendingPlace() { release(); }
  PendingPlace(const PendingPlace&) = delete;
  PendingPlace& operator=(const PendingPlace&) = delete;
  void release() {
    if (table_) table_->give_back(source_);
    table_ = nullptr;
  }

 private:
  PendingTable* table_;
  std::string source_;
};
