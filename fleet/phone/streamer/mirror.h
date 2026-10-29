// The GPU mirror's route (fleet/PROTOCOL.md, "Phone streamer", GET /mirror): the emulator's
// command-stream tap (gfxstream's mirror_tap.h, a Unix socket in this container) relayed to
// the one viewer holding the phone, deflated. Host-only code, tested in test_host.cpp.
#pragma once
#include <stddef.h>
#include <stdint.h>
#include <zlib.h>

#include <string>
#include <vector>

// The viewer a direct token names (`v1.<expiry>.<viewer>.<mac>`), or "" for anything else.
// It does not check the mac: admits() does.
std::string admit_token_viewer(const std::string& token);

// Whether a mirror stream may open for this token: the scene names the viewer holding the
// phone (`mirror_viewer`), the coordinator's keys made the token, and it names that viewer.
bool mirror_admits(const std::vector<std::string>& keys, const std::string& mirror_viewer, const std::string& token,
                   int64_t now_s);

// One raw deflate stream for a connection: each push returns the bytes of one WebSocket
// message, ending at a byte boundary (Z_SYNC_FLUSH), so the page's one DecompressionStream
// ("deflate-raw") gives back everything pushed so far.
class MirrorDeflate {
 public:
  explicit MirrorDeflate(int level = 1);
  ~MirrorDeflate();
  MirrorDeflate(const MirrorDeflate&) = delete;
  MirrorDeflate& operator=(const MirrorDeflate&) = delete;
  bool ok() const { return ok_; }
  // Appends the compressed bytes of `n` input bytes to *out; false on a zlib error.
  bool push(const uint8_t* p, size_t n, std::string* out);
  uint64_t in_bytes = 0, out_bytes = 0;

 private:
  z_stream z_{};
  bool ok_ = false;
};

// A mirror stream's queue toward its viewer: bounded in bytes. A viewer this far behind is
// dropped and joins again (the page reopens the stream), so the tap's reader never waits on a
// slow viewer and the emulator never waits on the tap.
#define MIRROR_MAX_QUEUED_BYTES (8u << 20)
// The tap's socket in the phone container (phone.sh passes the same path to the emulator).
#define MIRROR_DEFAULT_SOCKET "/tmp/gfxstream-mirror.sock"
