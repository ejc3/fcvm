#pragma once
#include <cstdio>
#include <cstdlib>
static int g_fail = 0, g_pass = 0;
#define CHECK(c, ...)                                         \
  do {                                                        \
    if (c) { g_pass++; }                                      \
    else {                                                    \
      g_fail++;                                               \
      fprintf(stderr, "FAIL %s:%d: %s: ", __FILE__, __LINE__, #c); \
      fprintf(stderr, __VA_ARGS__);                           \
      fprintf(stderr, "\n");                                  \
    }                                                         \
  } while (0)
#define DONE()                                                             \
  do {                                                                     \
    fprintf(stderr, "%d checks passed, %d failed\n", g_pass, g_fail);      \
    return g_fail ? 1 : 0;                                                 \
  } while (0)
