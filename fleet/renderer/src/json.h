// Minimal JSON reader (RFC 8259 subset sufficient for the scene and control
// messages): objects, arrays, numbers, strings with escapes, true/false/null.
#pragma once
#include <map>
#include <memory>
#include <stdexcept>
#include <string>
#include <vector>

struct Json {
  enum Type { Null, Bool, Number, String, Array, Object } type = Null;
  bool b = false;
  double num = 0;
  std::string str;
  std::vector<Json> arr;
  std::map<std::string, Json> obj;

  static Json parse(const std::string& text);  // throws std::runtime_error

  bool has(const std::string& k) const { return type == Object && obj.count(k); }
  const Json& at(const std::string& k) const;   // throws if missing
  double number(const std::string& what) const;  // throws if not a number
  const std::string& string(const std::string& what) const;
  const std::vector<Json>& array(const std::string& what) const;
};

std::string json_escape(const std::string& s);
