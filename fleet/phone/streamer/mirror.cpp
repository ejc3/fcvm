#include "mirror.h"

#include "admit.h"

std::string admit_token_viewer(const std::string& token) {
  size_t last = token.rfind('.');
  if (last == std::string::npos || token.compare(0, 3, "v1.") != 0) return "";
  size_t dot = token.find('.', 3);
  if (dot == std::string::npos || dot >= last) return "";
  return token.substr(dot + 1, last - dot - 1);
}

bool mirror_admits(const std::vector<std::string>& keys, const std::string& mirror_viewer, const std::string& token,
                   int64_t now_s) {
  if (mirror_viewer.empty() || keys.empty()) return false;
  if (!admit_token_valid(keys, token, now_s)) return false;
  return admit_token_viewer(token) == mirror_viewer;
}

MirrorDeflate::MirrorDeflate(int level) {
  // -15: raw deflate, no zlib header, the format DecompressionStream("deflate-raw") reads
  ok_ = deflateInit2(&z_, level, Z_DEFLATED, -15, 8, Z_DEFAULT_STRATEGY) == Z_OK;
}

MirrorDeflate::~MirrorDeflate() {
  if (ok_) deflateEnd(&z_);
}

bool MirrorDeflate::push(const uint8_t* p, size_t n, std::string* out) {
  if (!ok_) return false;
  z_.next_in = const_cast<Bytef*>(p);
  z_.avail_in = (uInt)n;
  unsigned char buf[1 << 16];
  size_t before = out->size();
  do {
    z_.next_out = buf;
    z_.avail_out = sizeof buf;
    int rc = deflate(&z_, Z_SYNC_FLUSH);
    if (rc != Z_OK && rc != Z_BUF_ERROR) return false;
    out->append((const char*)buf, sizeof buf - z_.avail_out);
  } while (z_.avail_out == 0 || z_.avail_in > 0);
  in_bytes += n;
  out_bytes += out->size() - before;
  return true;
}
