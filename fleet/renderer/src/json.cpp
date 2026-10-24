#include "json.h"

#include <cmath>
#include <cstdlib>
#include <cstring>
#include <cctype>
#include <cstdio>

namespace {
struct Parser {
  const std::string& s;
  size_t i = 0;
  int depth = 0;
  explicit Parser(const std::string& t) : s(t) {}

  [[noreturn]] void fail(const std::string& m) {
    throw std::runtime_error("json: " + m + " at offset " + std::to_string(i));
  }
  void ws() {
    while (i < s.size() && (s[i] == ' ' || s[i] == '\t' || s[i] == '\n' || s[i] == '\r')) i++;
  }
  bool lit(const char* w) {
    size_t n = strlen(w);
    if (s.compare(i, n, w) == 0) { i += n; return true; }
    return false;
  }
  static void utf8(std::string& o, unsigned cp) {
    if (cp < 0x80) o += (char)cp;
    else if (cp < 0x800) { o += (char)(0xC0 | (cp >> 6)); o += (char)(0x80 | (cp & 63)); }
    else if (cp < 0x10000) { o += (char)(0xE0 | (cp >> 12)); o += (char)(0x80 | ((cp >> 6) & 63)); o += (char)(0x80 | (cp & 63)); }
    else { o += (char)(0xF0 | (cp >> 18)); o += (char)(0x80 | ((cp >> 12) & 63)); o += (char)(0x80 | ((cp >> 6) & 63)); o += (char)(0x80 | (cp & 63)); }
  }
  unsigned hex4() {
    if (i + 4 > s.size()) fail("short \\u escape");
    unsigned v = 0;
    for (int k = 0; k < 4; k++) {
      char c = s[i++];
      v <<= 4;
      if (c >= '0' && c <= '9') v |= c - '0';
      else if (c >= 'a' && c <= 'f') v |= c - 'a' + 10;
      else if (c >= 'A' && c <= 'F') v |= c - 'A' + 10;
      else fail("bad \\u escape");
    }
    return v;
  }
  std::string str() {
    if (s[i] != '"') fail("expected string");
    i++;
    std::string o;
    while (true) {
      if (i >= s.size()) fail("unterminated string");
      char c = s[i++];
      if (c == '"') break;
      if ((unsigned char)c < 0x20) fail("control character in string");
      if (c != '\\') { o += c; continue; }
      if (i >= s.size()) fail("unterminated escape");
      char e = s[i++];
      switch (e) {
        case '"': o += '"'; break;
        case '\\': o += '\\'; break;
        case '/': o += '/'; break;
        case 'b': o += '\b'; break;
        case 'f': o += '\f'; break;
        case 'n': o += '\n'; break;
        case 'r': o += '\r'; break;
        case 't': o += '\t'; break;
        case 'u': {
          unsigned cp = hex4();
          if (cp >= 0xD800 && cp < 0xDC00 && i + 1 < s.size() && s[i] == '\\' && s[i + 1] == 'u') {
            i += 2;
            unsigned lo = hex4();
            if (lo >= 0xDC00 && lo < 0xE000) cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
          }
          utf8(o, cp);
          break;
        }
        default: fail("bad escape");
      }
    }
    return o;
  }
  Json value() {
    if (++depth > 64) fail("nesting too deep");
    ws();
    if (i >= s.size()) fail("unexpected end");
    Json v;
    char c = s[i];
    if (c == '{') {
      v.type = Json::Object;
      i++;
      ws();
      if (i < s.size() && s[i] == '}') { i++; depth--; return v; }
      while (true) {
        ws();
        std::string k = str();
        ws();
        if (i >= s.size() || s[i] != ':') fail("expected ':'");
        i++;
        v.obj[k] = value();
        ws();
        if (i < s.size() && s[i] == ',') { i++; continue; }
        if (i < s.size() && s[i] == '}') { i++; break; }
        fail("expected ',' or '}'");
      }
    } else if (c == '[') {
      v.type = Json::Array;
      i++;
      ws();
      if (i < s.size() && s[i] == ']') { i++; depth--; return v; }
      while (true) {
        v.arr.push_back(value());
        ws();
        if (i < s.size() && s[i] == ',') { i++; continue; }
        if (i < s.size() && s[i] == ']') { i++; break; }
        fail("expected ',' or ']'");
      }
    } else if (c == '"') {
      v.type = Json::String;
      v.str = str();
    } else if (lit("true")) {
      v.type = Json::Bool; v.b = true;
    } else if (lit("false")) {
      v.type = Json::Bool; v.b = false;
    } else if (lit("null")) {
      v.type = Json::Null;
    } else if (c == '-' || (c >= '0' && c <= '9')) {
      size_t st = i;
      if (s[i] == '-') i++;
      while (i < s.size() && (isdigit((unsigned char)s[i]) || s[i] == '.' || s[i] == 'e' || s[i] == 'E' || s[i] == '+' || s[i] == '-')) i++;
      std::string n = s.substr(st, i - st);
      char* end = nullptr;
      v.type = Json::Number;
      v.num = strtod(n.c_str(), &end);
      if (end != n.c_str() + n.size() || !std::isfinite(v.num)) { i = st; fail("bad number"); }
    } else {
      fail("unexpected character");
    }
    depth--;
    return v;
  }
};
}  // namespace

Json Json::parse(const std::string& text) {
  Parser p(text);
  Json v = p.value();
  p.ws();
  if (p.i != text.size()) p.fail("trailing characters");
  return v;
}

const Json& Json::at(const std::string& k) const {
  if (type != Object) throw std::runtime_error("expected an object holding '" + k + "'");
  auto it = obj.find(k);
  if (it == obj.end()) throw std::runtime_error("missing field '" + k + "'");
  return it->second;
}
double Json::number(const std::string& what) const {
  if (type != Number) throw std::runtime_error("'" + what + "' must be a number");
  return num;
}
const std::string& Json::string(const std::string& what) const {
  if (type != String) throw std::runtime_error("'" + what + "' must be a string");
  return str;
}
const std::vector<Json>& Json::array(const std::string& what) const {
  if (type != Array) throw std::runtime_error("'" + what + "' must be an array");
  return arr;
}

std::string json_escape(const std::string& s) {
  std::string o;
  for (unsigned char c : s) {
    if (c == '"') o += "\\\"";
    else if (c == '\\') o += "\\\\";
    else if (c < 0x20) { char b[8]; snprintf(b, sizeof b, "\\u%04x", c); o += b; }
    else o += (char)c;
  }
  return o;
}
