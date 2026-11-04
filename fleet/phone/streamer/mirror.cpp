#include "mirror.h"

#include <string.h>

#include <algorithm>

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

MirrorZstd::MirrorZstd() {
  z_ = ZSTD_createCCtx();
  if (z_ && (ZSTD_isError(ZSTD_CCtx_setParameter(z_, ZSTD_c_compressionLevel, 3)) ||
             ZSTD_isError(ZSTD_CCtx_setParameter(z_, ZSTD_c_windowLog, 20)))) {
    ZSTD_freeCCtx(z_);
    z_ = nullptr;
  }
}

MirrorZstd::~MirrorZstd() { ZSTD_freeCCtx(z_); }

bool MirrorZstd::push(const uint8_t* p, size_t n, std::string* out) {
  if (!z_) return false;
  ZSTD_inBuffer in{p, n, 0};
  unsigned char buf[1 << 16];
  size_t before = out->size();
  size_t left;
  do {
    ZSTD_outBuffer o{buf, sizeof buf, 0};
    left = ZSTD_compressStream2(z_, &o, &in, ZSTD_e_flush);
    if (ZSTD_isError(left)) return false;
    out->append((const char*)buf, o.pos);
  } while (left != 0);
  in_bytes += n;
  out_bytes += out->size() - before;
  return true;
}

void TapCursor::split(const uint8_t* p, size_t n, std::vector<std::pair<bool, size_t>>* runs) {
  runs->clear();
  auto add = [&](bool join, size_t k) {
    if (k == 0) return;
    if (!runs->empty() && runs->back().first == join)
      runs->back().second += k;
    else
      runs->emplace_back(join, k);
  };
  while (n > 0) {
    if (body_left_ > 0) {
      size_t k = (size_t)std::min<uint64_t>(body_left_, n);
      add(join_, k);
      body_left_ -= k;
      p += k;
      n -= k;
      continue;
    }
    size_t k = std::min(n, sizeof head_ - head_len_);
    memcpy(head_ + head_len_, p, k);
    head_len_ += k;
    // once its type has arrived a header's bytes count with its record; before, as live
    if (head_len_ >= 4) join_ = (head_[0] | head_[1] << 8 | head_[2] << 16 | (uint32_t)head_[3] << 24) == 8;
    add(head_len_ >= 4 && join_, k);
    p += k;
    n -= k;
    if (head_len_ == sizeof head_) {
      body_left_ = head_[8] | head_[9] << 8 | head_[10] << 16 | (uint32_t)head_[11] << 24;
      head_len_ = 0;
    }
  }
}
