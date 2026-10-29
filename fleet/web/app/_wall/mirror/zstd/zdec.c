/* The GPU mirror's zstd decoder (app/_wall/mirror/zstd.ts loads it): zstd's single-file decoder
 * built for wasm32 with no libc (scripts/build-zstd-wasm.sh). One streaming decoder in a fixed
 * workspace, for frames whose window is at most 1 MiB (the phone streamer's), so it never
 * allocates. The caller writes compressed bytes at zd_in(), calls zd_feed(n), then zd_step()
 * until it returns less than ZD_OUT_CAP with zd_in_left() == 0, taking each step's output from
 * zd_out(). */
#define ZSTD_STATIC_LINKING_ONLY
/* Only the zd_ functions are exported. */
#define ZSTDLIB_VISIBLE
#define ZSTDLIB_HIDDEN
#include "zstddeclib.c"

#define EXPORT(name) __attribute__((export_name(#name)))
#define WINDOW_LOG 20
#define ZD_IN_CAP (1u << 18)
#define ZD_OUT_CAP (1u << 18)
#define WORK_CAP (3u << 19)

static unsigned char work[WORK_CAP] __attribute__((aligned(16)));
static unsigned char inbuf[ZD_IN_CAP], outbuf[ZD_OUT_CAP];
static ZSTD_DStream *ds;
static ZSTD_inBuffer in;

/* The few libc functions zstd calls. With -mbulk-memory the builtins are memory.copy and
 * memory.fill. Nothing allocates: the decoder lives in `work`. */
void *memcpy(void *d, const void *s, size_t n) { return __builtin_memcpy(d, s, n); }
void *memmove(void *d, const void *s, size_t n) { return __builtin_memmove(d, s, n); }
void *memset(void *d, int c, size_t n) { return __builtin_memset(d, c, n); }
int memcmp(const void *a, const void *b, size_t n) {
  const unsigned char *x = a, *y = b;
  for (; n; n--, x++, y++)
    if (*x != *y) return *x - *y;
  return 0;
}
void *malloc(size_t n) { (void)n; return 0; }
void *calloc(size_t n, size_t m) { (void)n; (void)m; return 0; }
void free(void *p) { (void)p; }

/* A new stream: 0, or negative when the workspace cannot hold the decoder. */
EXPORT(zd_reset) int zd_reset(void) {
  if (!ds) {
    if (ZSTD_estimateDStreamSize((size_t)1 << WINDOW_LOG) > WORK_CAP) return -1;
    ds = ZSTD_initStaticDStream(work, WORK_CAP);
    if (!ds) return -2;
  }
  if (ZSTD_isError(ZSTD_DCtx_reset(ds, ZSTD_reset_session_and_parameters))) return -3;
  if (ZSTD_isError(ZSTD_DCtx_setParameter(ds, ZSTD_d_windowLogMax, WINDOW_LOG))) return -4;
  in.src = inbuf;
  in.size = 0;
  in.pos = 0;
  return 0;
}
EXPORT(zd_in) unsigned char *zd_in(void) { return inbuf; }
EXPORT(zd_out) unsigned char *zd_out(void) { return outbuf; }
EXPORT(zd_in_cap) unsigned zd_in_cap(void) { return ZD_IN_CAP; }
EXPORT(zd_out_cap) unsigned zd_out_cap(void) { return ZD_OUT_CAP; }
EXPORT(zd_feed) void zd_feed(unsigned n) {
  in.size = n < ZD_IN_CAP ? n : ZD_IN_CAP;
  in.pos = 0;
}
EXPORT(zd_in_left) unsigned zd_in_left(void) { return (unsigned)(in.size - in.pos); }
/* Bytes decoded into zd_out(), or -(100 + zstd's error code). */
EXPORT(zd_step) int zd_step(void) {
  ZSTD_outBuffer out = {outbuf, ZD_OUT_CAP, 0};
  size_t r = ZSTD_decompressStream(ds, &out, &in);
  if (ZSTD_isError(r)) return -100 - (int)ZSTD_getErrorCode(r);
  return (int)out.pos;
}
