/*
 * fcvm-gpu-scrub: zero every byte of GPU memory a process can allocate, on
 * every GPU the driver sees, before a container is given the GPUs.
 *
 * A GPU moves between VMs. NVIDIA documents that newly allocated memory is
 * not cleared, so a new VM's allocations can return the previous tenant's
 * data (scripts/gpu-residue measures it). fc-agent runs this once per restore,
 * after the driver initialised the GPUs and before the CDI spec is written
 * (fc-agent/src/gpu.rs), and refuses to publish the clone if it fails.
 *
 * Per GPU, on its own thread: create a context, allocate until the device
 * refuses even 2 MiB (halving the request on each refusal), then queue a
 * memset of zero over every allocation, synchronise, free, destroy the
 * context. `--parallel N` runs at most N GPUs at a time, because each context
 * costs guest memory; fc-agent picks N from MemAvailable.
 *
 * It uses the CUDA driver API from the driver image's libcuda.so.1, loaded
 * with dlopen, so it builds with a C compiler alone. Output, one line per GPU
 * and a summary:
 *   scrubbed device=<n> bytes=<b> free=<f> total=<t> ms=<ms>
 *   scrubbed devices=<n> bytes=<b> ms=<ms>
 * Exit status 0 only if every GPU was scrubbed; otherwise an error line names
 * the GPU and the call that failed.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <inttypes.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

typedef int CUresult;
typedef int CUdevice;
typedef void *CUcontext;
typedef unsigned long long CUdeviceptr;
typedef void *CUstream;

#define CUDA_SUCCESS 0
#define MIN_CHUNK ((size_t)2 << 20)
#define MAX_CHUNK ((size_t)1 << 30)
#define MAX_CHUNKS 8192
#define MAX_DEVICES 64

static CUresult (*cuInit)(unsigned int);
static CUresult (*cuDeviceGetCount)(int *);
static CUresult (*cuDeviceGet)(CUdevice *, int);
static CUresult (*cuCtxCreate)(CUcontext *, unsigned int, CUdevice);
static CUresult (*cuCtxDestroy)(CUcontext);
static CUresult (*cuCtxSynchronize)(void);
static CUresult (*cuMemGetInfo)(size_t *, size_t *);
static CUresult (*cuMemAlloc)(CUdeviceptr *, size_t);
static CUresult (*cuMemFree)(CUdeviceptr);
static CUresult (*cuMemsetD32Async)(CUdeviceptr, unsigned int, size_t, CUstream);
static CUresult (*cuGetErrorString)(CUresult, const char **);

static int load(void *lib, void **fn, const char *name) {
    *fn = dlsym(lib, name);
    if (!*fn) {
        fprintf(stderr, "fcvm-gpu-scrub: libcuda.so.1 has no %s\n", name);
        return -1;
    }
    return 0;
}

static const char *error_name(CUresult r) {
    const char *s = NULL;
    if (cuGetErrorString && cuGetErrorString(r, &s) == CUDA_SUCCESS && s)
        return s;
    return "unknown error";
}

static double now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
}

struct job {
    int ordinal;
    size_t bytes, free, total;
    double ms, ctx_ms, alloc_ms, zero_ms, free_ms;
    char error[256];
};

#define FAIL(j, call, r)                                                      \
    do {                                                                      \
        snprintf((j)->error, sizeof (j)->error, "%s: %s (%d)", call,          \
                 error_name(r), (int)(r));                                    \
        goto out;                                                             \
    } while (0)

static void *scrub(void *arg) {
    struct job *j = arg;
    double started = now_ms();
    CUdevice dev;
    CUcontext ctx = NULL;
    CUdeviceptr chunks[MAX_CHUNKS];
    size_t sizes[MAX_CHUNKS];
    size_t n = 0, chunk = MAX_CHUNK;
    CUresult r;

    double mark = started;
    if ((r = cuDeviceGet(&dev, j->ordinal)) != CUDA_SUCCESS) FAIL(j, "cuDeviceGet", r);
    if ((r = cuCtxCreate(&ctx, 0, dev)) != CUDA_SUCCESS) FAIL(j, "cuCtxCreate", r);
    j->ctx_ms = now_ms() - mark;
    mark = now_ms();
    if ((r = cuMemGetInfo(&j->free, &j->total)) != CUDA_SUCCESS) FAIL(j, "cuMemGetInfo", r);
    /* Take everything first, so no zeroed allocation is freed and handed out
     * again while there is still memory to take. */
    while (chunk >= MIN_CHUNK) {
        CUdeviceptr p;
        if (n == MAX_CHUNKS) {
            snprintf(j->error, sizeof j->error, "more than %d allocations", MAX_CHUNKS);
            goto out;
        }
        r = cuMemAlloc(&p, chunk);
        if (r == CUDA_SUCCESS) {
            chunks[n] = p;
            sizes[n++] = chunk;
            j->bytes += chunk;
        } else if (r == 2 /* CUDA_ERROR_OUT_OF_MEMORY */) {
            chunk /= 2;
        } else {
            FAIL(j, "cuMemAlloc", r);
        }
    }
    j->alloc_ms = now_ms() - mark;
    mark = now_ms();
    /* Every size is a power of two from 2 MiB up, so a whole number of words. */
    for (size_t i = 0; i < n; i++)
        if ((r = cuMemsetD32Async(chunks[i], 0, sizes[i] / 4, NULL)) != CUDA_SUCCESS)
            FAIL(j, "cuMemsetD32Async", r);
    if ((r = cuCtxSynchronize()) != CUDA_SUCCESS) FAIL(j, "cuCtxSynchronize", r);
    j->zero_ms = now_ms() - mark;
    if (j->bytes == 0) snprintf(j->error, sizeof j->error, "allocated no memory");
out:
    mark = now_ms();
    while (n > 0) cuMemFree(chunks[--n]);
    j->free_ms = now_ms() - mark;
    if (ctx && (r = cuCtxDestroy(ctx)) != CUDA_SUCCESS && !j->error[0])
        snprintf(j->error, sizeof j->error, "cuCtxDestroy: %s (%d)", error_name(r), (int)r);
    j->ms = now_ms() - started;
    return NULL;
}

int main(int argc, char **argv) {
    double started = now_ms();
    int parallel = MAX_DEVICES;
    if (argc == 3 && strcmp(argv[1], "--parallel") == 0) {
        /* Parsed by hand: atoi and strtol bind to __isoc23_strtol@GLIBC_2.38
         * when built against glibc 2.38 or later headers. */
        parallel = 0;
        for (const char *c = argv[2]; *c; c++) {
            if (*c < '0' || *c > '9' || parallel > MAX_DEVICES) {
                parallel = -1;
                break;
            }
            parallel = parallel * 10 + (*c - '0');
        }
    } else if (argc != 1) {
        fprintf(stderr, "usage: %s [--parallel N]\n", argv[0]);
        return 2;
    }
    if (parallel < 1) {
        fprintf(stderr, "fcvm-gpu-scrub: --parallel must be at least 1\n");
        return 2;
    }
    /* Under memory pressure the kernel kills this process first, so the scrub
     * fails (and the attach with it) rather than something else in the guest. */
    FILE *oom = fopen("/proc/self/oom_score_adj", "w");
    if (oom) {
        fputs("1000", oom);
        fclose(oom);
    }
    /* One hardware queue per context and modules loaded on first use: on a T4
     * in a 1 GiB guest this took eight GPUs, one at a time, from 4.19 s to
     * 2.47 s, and a context from about 230 to about 185 MB of guest memory. */
    setenv("CUDA_DEVICE_MAX_CONNECTIONS", "1", 1);
    setenv("CUDA_MODULE_LOADING", "LAZY", 1);
    void *lib = dlopen("libcuda.so.1", RTLD_NOW);
    if (!lib) {
        fprintf(stderr, "fcvm-gpu-scrub: %s\n", dlerror());
        return 1;
    }
    if (load(lib, (void **)&cuInit, "cuInit") ||
        load(lib, (void **)&cuDeviceGetCount, "cuDeviceGetCount") ||
        load(lib, (void **)&cuDeviceGet, "cuDeviceGet") ||
        load(lib, (void **)&cuCtxCreate, "cuCtxCreate_v2") ||
        load(lib, (void **)&cuCtxDestroy, "cuCtxDestroy_v2") ||
        load(lib, (void **)&cuCtxSynchronize, "cuCtxSynchronize") ||
        load(lib, (void **)&cuMemGetInfo, "cuMemGetInfo_v2") ||
        load(lib, (void **)&cuMemAlloc, "cuMemAlloc_v2") ||
        load(lib, (void **)&cuMemFree, "cuMemFree_v2") ||
        load(lib, (void **)&cuMemsetD32Async, "cuMemsetD32Async") ||
        load(lib, (void **)&cuGetErrorString, "cuGetErrorString"))
        return 1;

    CUresult r;
    int count = 0;
    double init_started = now_ms();
    if ((r = cuInit(0)) != CUDA_SUCCESS) {
        fprintf(stderr, "fcvm-gpu-scrub: cuInit: %s (%d)\n", error_name(r), (int)r);
        return 1;
    }
    if ((r = cuDeviceGetCount(&count)) != CUDA_SUCCESS) {
        fprintf(stderr, "fcvm-gpu-scrub: cuDeviceGetCount: %s (%d)\n", error_name(r), (int)r);
        return 1;
    }
    if (count <= 0 || count > MAX_DEVICES) {
        fprintf(stderr, "fcvm-gpu-scrub: the driver reports %d GPU(s)\n", count);
        return 1;
    }

    double init_ms = now_ms() - init_started;
    struct job jobs[MAX_DEVICES];
    pthread_t threads[MAX_DEVICES];
    memset(jobs, 0, sizeof jobs);
    /* At most `parallel` GPUs at a time: each context costs the guest memory. */
    for (int wave = 0; wave < count; wave += parallel) {
        int end = wave + parallel < count ? wave + parallel : count;
        for (int i = wave; i < end; i++) {
            jobs[i].ordinal = i;
            if (pthread_create(&threads[i], NULL, scrub, &jobs[i]) != 0) {
                fprintf(stderr, "fcvm-gpu-scrub: starting the thread for GPU %d failed\n", i);
                return 1;
            }
        }
        for (int i = wave; i < end; i++) pthread_join(threads[i], NULL);
    }
    int failed = 0;
    size_t bytes = 0;
    for (int i = 0; i < count; i++) {
        if (jobs[i].error[0]) {
            fprintf(stderr, "fcvm-gpu-scrub: GPU %d: %s\n", i, jobs[i].error);
            failed = 1;
            continue;
        }
        bytes += jobs[i].bytes;
        printf("scrubbed device=%d bytes=%zu free=%zu total=%zu ms=%.3f ctx_ms=%.3f "
               "alloc_ms=%.3f zero_ms=%.3f free_ms=%.3f\n",
               i, jobs[i].bytes, jobs[i].free, jobs[i].total, jobs[i].ms, jobs[i].ctx_ms,
               jobs[i].alloc_ms, jobs[i].zero_ms, jobs[i].free_ms);
    }
    if (failed) return 1;
    printf("scrubbed devices=%d bytes=%zu ms=%.3f init_ms=%.3f\n", count, bytes,
           now_ms() - started, init_ms);
    return 0;
}
