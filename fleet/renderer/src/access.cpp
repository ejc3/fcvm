#include "access.h"

#include <fcntl.h>
#include <stdlib.h>
#include <unistd.h>

#include "json.h"

void SceneToken::configure(const std::string& path, bool json) {
  std::lock_guard<std::mutex> l(mu_);
  path_ = path;
  json_ = json;
  have_st_ = false;
  token_.clear();
}

void SceneToken::configure_from_env() {
  const char* token_file = getenv("SCENE_TOKEN_FILE");
  const char* cert_json = getenv("TLS_CERT_FILE");
  if (token_file && *token_file) configure(token_file, false);
  else if (cert_json && *cert_json) configure(cert_json, true);
}

std::string SceneToken::current() {
  std::lock_guard<std::mutex> l(mu_);
  if (path_.empty()) return "";
  struct stat st {};
  if (stat(path_.c_str(), &st) != 0) {
    // Gone: keep nothing, so a removed token stops authorizing.
    have_st_ = false;
    token_.clear();
    return "";
  }
  if (have_st_ && st.st_ino == st_.st_ino && st.st_size == st_.st_size && st.st_mtim.tv_sec == st_.st_mtim.tv_sec &&
      st.st_mtim.tv_nsec == st_.st_mtim.tv_nsec)
    return token_;
  int fd = open(path_.c_str(), O_RDONLY | O_CLOEXEC);
  if (fd < 0) return token_;
  std::string text;
  char buf[4096];
  long n;
  while ((n = read(fd, buf, sizeof buf)) > 0 && text.size() < (1 << 20)) text.append(buf, (size_t)n);
  close(fd);
  token_ = scene_token_from_text(text, json_);
  st_ = st;
  have_st_ = true;
  return token_;
}

std::string scene_token_from_text(const std::string& text, bool json) {
  if (json) {
    try {
      Json j = Json::parse(text);
      if (!j.has("scene_token")) return "";
      return j.at("scene_token").string("scene_token");
    } catch (const std::exception&) {
      return "";
    }
  }
  size_t a = text.find_first_not_of(" \t\r\n"), b = text.find_last_not_of(" \t\r\n");
  return a == std::string::npos ? "" : text.substr(a, b - a + 1);
}

bool constant_time_equal(const std::string& a, const std::string& b) {
  if (a.size() != b.size()) return false;
  unsigned char d = 0;
  for (size_t i = 0; i < a.size(); i++) d |= (unsigned char)(a[i] ^ b[i]);
  return d == 0;
}

int scene_access(bool fleet, const std::string& token, const std::string& authorization) {
  if (!fleet) return 0;
  if (token.empty()) return 503;
  const std::string scheme = "Bearer ";
  if (authorization.size() <= scheme.size() || authorization.compare(0, scheme.size(), scheme) != 0) return 401;
  return constant_time_equal(authorization.substr(scheme.size()), token) ? 0 : 401;
}

int frames_access(bool fleet, bool has_scene) { return fleet && !has_scene ? 503 : 0; }

bool admit_message(const std::string& text, std::string* token) {
  try {
    Json j = Json::parse(text);
    if (!j.has("admit") || j.at("admit").type != Json::String) return false;
    *token = j.at("admit").str;
    return true;
  } catch (const std::exception&) {
    return false;
  }
}

int fleet_max_quality(bool admission, const std::string& viewer) {
  if (!admission) return 2;
  static const std::string kAggregator = "aggregator-";
  return viewer.size() > kAggregator.size() && viewer.compare(0, kAggregator.size(), kAggregator) == 0 ? 2 : 1;
}

bool StreamTable::open(const std::string& viewer, std::function<void()> stop, uint64_t* id) {
  std::lock_guard<std::mutex> l(mu_);
  auto held = viewer.empty() ? by_viewer_.end() : by_viewer_.find(viewer);
  if (held != by_viewer_.end()) {
    auto old = open_.find(held->second);
    if (old != open_.end() && old->second.stop) {
      old->second.stop();
      old->second.stop = nullptr;  // stopped once; it stays counted until it closes
    }
  } else if (open_.size() >= cap_) {
    return false;
  }
  *id = next_++;
  open_[*id] = Entry{viewer, std::move(stop)};
  if (!viewer.empty()) by_viewer_[viewer] = *id;
  return true;
}

void StreamTable::close(uint64_t id) {
  std::lock_guard<std::mutex> l(mu_);
  auto e = open_.find(id);
  if (e == open_.end()) return;
  if (!e->second.viewer.empty()) {
    auto v = by_viewer_.find(e->second.viewer);
    if (v != by_viewer_.end() && v->second == id) by_viewer_.erase(v);
  }
  open_.erase(e);
}

size_t StreamTable::size() {
  std::lock_guard<std::mutex> l(mu_);
  return open_.size();
}

void StreamTable::set_cap(size_t cap) {
  std::lock_guard<std::mutex> l(mu_);
  cap_ = cap;
}
