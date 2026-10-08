// Standalone unit tests: no FreeSWITCH symbols required.
#include <assert.h>
#include <stdint.h>
#include <string>
#include <vector>
#include <stdio.h>
#include "base64.hpp"
#include "vector_math.h"

using namespace drachtio;

static void test_base64() {
  const std::string in = "hello, audio fork!";
  std::string enc = base64_encode(in);
  assert(enc == "aGVsbG8sIGF1ZGlvIGZvcmsh");
  assert(base64_decode(enc) == in);
  assert(base64_decode(base64_encode(std::string())).empty());
  for (size_t n = 0; n < 8; n++) {
    std::string s(n, '\xff');
    assert(base64_decode(base64_encode(s)) == s);
  }
}

static void test_vector_add() {
  std::vector<int16_t> a(37), b(37);
  for (size_t i = 0; i < a.size(); i++) { a[i] = (int16_t)(i * 10); b[i] = (int16_t)(i * 3 - 20); }
  std::vector<int16_t> expect(a);
  for (size_t i = 0; i < a.size(); i++) expect[i] = (int16_t)(a[i] + b[i]);
  vector_add(a.data(), b.data(), a.size());
  assert(a == expect);
}

static void test_vector_normalize() {
  std::vector<int16_t> a = {0, 1, -1, 32767, -32768, 100, -100};
  std::vector<int16_t> expect(a);
  vector_normalize(a.data(), a.size());
  assert(a == expect);
}

// vector_math.cpp does not define vector_change_sln_volume_granular in its SSE2 branch
// (and nothing else in the module calls it), so only test it where it exists.
#ifndef USE_SSE2
static void test_volume() {
  std::vector<int16_t> a(33, 1000);
  vector_change_sln_volume_granular(a.data(), a.size(), 0);   // no-op
  for (int16_t v : a) assert(v == 1000);

  vector_change_sln_volume_granular(a.data(), a.size(), 1);   // +1 dB, ~x1.122
  for (int16_t v : a) assert(v >= 1121 && v <= 1123);

  std::vector<int16_t> loud(33, 20000);
  vector_change_sln_volume_granular(loud.data(), loud.size(), 20);  // saturates
  for (int16_t v : loud) assert(v == 32767);

  std::vector<int16_t> quiet(33, 1000);
  vector_change_sln_volume_granular(quiet.data(), quiet.size(), -50); // silence is mapped to 0 -> left unchanged
  for (int16_t v : quiet) assert(v == 1000);

  std::vector<int16_t> down(33, 1000);
  vector_change_sln_volume_granular(down.data(), down.size(), -10);  // ~x0.316
  for (int16_t v : down) assert(v >= 315 && v <= 317);
}
#endif

int main() {
  test_base64();
  test_vector_add();
  test_vector_normalize();
#ifndef USE_SSE2
  test_volume();
#endif
  puts("all unit tests passed");
  return 0;
}
