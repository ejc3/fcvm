// The GPU mirror's route (fleet/PROTOCOL.md, "Phone streamer", GET /mirror): the emulator's
// command-stream tap (gfxstream's mirror_tap.h, a Unix socket in this container) relayed to
// the one viewer holding the phone, compressed with zstd. Host-only code, tested in test_host.cpp.
#pragma once
#include <stddef.h>
#include <stdint.h>
#include <zstd.h>

#include <string>
#include <vector>

// The viewer a direct token names (`v1.<expiry>.<viewer>.<mac>`), or "" for anything else.
// It does not check the mac: admits() does.
std::string admit_token_viewer(const std::string& token);

// Whether a mirror stream may open for this token: the scene names the viewer holding the
// phone (`mirror_viewer`), the coordinator's keys made the token, and it names that viewer.
bool mirror_admits(const std::vector<std::string>& keys, const std::string& mirror_viewer, const std::string& token,
                   int64_t now_s);

// One zstd stream for a connection (level 3, a 1 MiB window, the most the page's decoder
// takes): each push returns the bytes of one WebSocket message, ending at a block boundary
// (ZSTD_e_flush), so the page's streaming decoder gives back everything pushed so far.
class MirrorZstd {
 public:
  MirrorZstd();
  ~MirrorZstd();
  MirrorZstd(const MirrorZstd&) = delete;
  MirrorZstd& operator=(const MirrorZstd&) = delete;
  bool ok() const { return z_ != nullptr; }
  // Appends the compressed bytes of `n` input bytes to *out; false on a zstd error.
  bool push(const uint8_t* p, size_t n, std::string* out);
  uint64_t in_bytes = 0, out_bytes = 0;

 private:
  ZSTD_CCtx* z_ = nullptr;
};

// A mirror stream's queue toward its viewer: bounded in bytes. A viewer this far behind is
// dropped and joins again (the page reopens the stream), so the tap's reader never waits on a
// slow viewer and the emulator never waits on the tap.
#define MIRROR_MAX_QUEUED_BYTES (8u << 20)
// The tap's socket in the phone container (phone.sh passes the same path to the emulator).
#define MIRROR_DEFAULT_SOCKET "/tmp/gfxstream-mirror.sock"
