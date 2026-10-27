#include "admit.h"

#include <openssl/crypto.h>
#include <openssl/evp.h>
#include <openssl/hmac.h>

#include <algorithm>
#include <stdexcept>

std::string admit_key_from_hex(const std::string& hex) {
  if (hex.size() != 64) throw std::runtime_error("an admission key is 64 hex characters");
  std::string out;
  auto nib = [](char c) -> int {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
  };
  for (size_t i = 0; i < hex.size(); i += 2) {
    int a = nib(hex[i]), b = nib(hex[i + 1]);
    if (a < 0 || b < 0) throw std::runtime_error("an admission key is 64 hex characters");
    out.push_back((char)(a << 4 | b));
  }
  return out;
}

std::string b64url(const unsigned char* p, size_t n) {
  static const char A[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
  std::string out;
  for (size_t i = 0; i < n; i += 3) {
    uint32_t v = (uint32_t)p[i] << 16 | (i + 1 < n ? (uint32_t)p[i + 1] << 8 : 0) | (i + 2 < n ? p[i + 2] : 0);
    size_t chars = std::min<size_t>(n - i, 3) + 1;
    for (size_t k = 0; k < chars; k++) out.push_back(A[(v >> (18 - 6 * k)) & 63]);
  }
  return out;
}

bool admit_token_valid(const std::vector<std::string>& keys, const std::string& token, int64_t now_s) {
  size_t last = token.rfind('.');
  if (last == std::string::npos) return false;
  std::string signed_part = token.substr(0, last), mac = token.substr(last + 1);
  // v1 . expiry . viewer (the viewer may not be empty)
  if (signed_part.compare(0, 3, "v1.") != 0) return false;
  size_t dot = signed_part.find('.', 3);
  if (dot == std::string::npos || dot == 3 || dot + 1 >= signed_part.size()) return false;
  std::string exp = signed_part.substr(3, dot - 3);
  if (exp.size() > 18 || exp.find_first_not_of("0123456789") != std::string::npos) return false;
  if ((int64_t)std::stoll(exp) < now_s) return false;
  for (const std::string& k : keys) {
    unsigned char out[EVP_MAX_MD_SIZE];
    unsigned int len = 0;
    if (!HMAC(EVP_sha256(), k.data(), (int)k.size(), (const unsigned char*)signed_part.data(), signed_part.size(), out, &len))
      continue;
    std::string want = b64url(out, len);
    if (want.size() == mac.size() && CRYPTO_memcmp(want.data(), mac.data(), want.size()) == 0) return true;
  }
  return false;
}

std::vector<std::string> admit_keys_after(const std::vector<std::string>& held, const std::vector<std::string>& fresh) {
  std::vector<std::string> out = fresh;
  for (const std::string& k : held)
    if (std::find(out.begin(), out.end(), k) == out.end()) out.push_back(k);
  if (out.size() > 4) out.resize(4);
  return out;
}
