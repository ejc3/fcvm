// Who may change a fleet renderer's scene and open its streams (fleet/PROTOCOL.md, "Access").
//
// The scene token is a secret the coordinator holds. It reaches a renderer the way its
// certificate does: as `scene_token` in the JSON file TLS_CERT_FILE names (a VM on a host
// agent), or as the whole of the file SCENE_TOKEN_FILE names (a tile instance, whose nginx
// holds the certificate). Either variable makes this a fleet renderer: POST /scene and
// /budget need `Authorization: Bearer <token>`, and GET /frames is refused until the first
// scene has said whether streams need a direct token.
#pragma once
#include <sys/stat.h>

#include <mutex>
#include <string>

class SceneToken {
 public:
  // `json`: the file is the certificate JSON (field scene_token), else it holds the token.
  void configure(const std::string& path, bool json);
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
