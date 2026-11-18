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
#include <sys/stat.h>

#include <functional>
#include <map>
#include <mutex>
#include <string>

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
