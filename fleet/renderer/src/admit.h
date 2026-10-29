// Direct-stream admission (fleet/PROTOCOL.md, "admission"): with the coordinator's keys,
// GET /frames needs `admit=v1.<expiry unix s>.<viewer>.<mac>`, the mac base64url (no
// padding) of HMAC-SHA256(key, "v1.<expiry>.<viewer>").
#pragma once
#include <cstdint>
#include <string>
#include <vector>

// The raw key of a 64-character hex string; throws std::runtime_error otherwise.
std::string admit_key_from_hex(const std::string& hex);
// Base64url without padding.
std::string b64url(const unsigned char* p, size_t n);
// Whether `token` was made with one of `keys` (raw) and has not expired at `now_s`.
bool admit_token_valid(const std::vector<std::string>& keys, const std::string& token, int64_t now_s);
// The keys to hold after a scene brings `fresh` (raw, newest first): the fresh ones, then
// the ones held before that are not among them, at most 4, so a token made just before a new
// key still opens a stream.
std::vector<std::string> admit_keys_after(const std::vector<std::string>& held, const std::vector<std::string>& fresh);
