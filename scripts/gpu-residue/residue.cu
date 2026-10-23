// gpu-residue: fill a GPU's memory with a marker, or look for it in fresh
// allocations. Built and run by scripts/gpu-residue/run.sh (make gpu-residue-test).
//
//   gpu-residue fill [--hold]
//       Allocate all free memory with cudaMalloc and write 16-byte records:
//       the 64-bit MARKER, then the record's byte offset in the fill. Prints
//       "FILLED bytes=<n>". Exits, or with --hold sleeps until killed.
//
//   gpu-residue scan runtime|driver|managed
//       Allocate as much as fill did (cudaMalloc, cuMemAlloc, or
//       cudaMallocManaged up to the free memory), write nothing, and count in
//       a GPU kernel the 8-byte words equal to MARKER and the words that are
//       not zero. Prints one "RESULT ..." line. Then, as a control, it writes
//       one marker into its own memory and fails unless a second scan counts
//       exactly one more.
//
// The fraction of memory holding the previous tenant's data is
// marker_words * 16 / bytes: each record is one marker word in 16 bytes.

#include <cuda.h>
#include <cuda_runtime.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <vector>

static const uint64_t MARKER = 0x5245534944554521ull;  // "RESIDUE!"
static const size_t MIN_CHUNK = 2ull << 20;
static const size_t MAX_CHUNK = 1ull << 30;

#define CHECK(call)                                                            \
  do {                                                                         \
    cudaError_t e_ = (call);                                                   \
    if (e_ != cudaSuccess) {                                                   \
      fprintf(stderr, "%s:%d %s: %s\n", __FILE__, __LINE__, #call,             \
              cudaGetErrorString(e_));                                         \
      exit(2);                                                                 \
    }                                                                          \
  } while (0)

struct Chunk {
  char *ptr;
  size_t bytes;
};

__global__ void fill_kernel(uint64_t *words, size_t n_records, uint64_t base) {
  size_t stride = (size_t)gridDim.x * blockDim.x;
  for (size_t i = (size_t)blockIdx.x * blockDim.x + threadIdx.x; i < n_records;
       i += stride) {
    words[2 * i] = MARKER;
    words[2 * i + 1] = base + 16 * i;
  }
}

__global__ void scan_kernel(const uint64_t *words, size_t n_words,
                            unsigned long long *marker,
                            unsigned long long *nonzero) {
  unsigned long long m = 0, nz = 0;
  size_t stride = (size_t)gridDim.x * blockDim.x;
  for (size_t i = (size_t)blockIdx.x * blockDim.x + threadIdx.x; i < n_words;
       i += stride) {
    uint64_t w = words[i];
    m += (w == MARKER);
    nz += (w != 0);
  }
  atomicAdd(marker, m);
  atomicAdd(nonzero, nz);
}

// Allocate until the device refuses even MIN_CHUNK, halving the request on
// each refusal. Returns what was allocated.
static std::vector<Chunk> allocate_all(const char *mode, size_t limit) {
  std::vector<Chunk> chunks;
  size_t chunk = MAX_CHUNK, total = 0;
  while (chunk >= MIN_CHUNK && total < limit) {
    size_t want = chunk < limit - total ? chunk : limit - total;
    want = want / MIN_CHUNK * MIN_CHUNK;
    if (want == 0) break;
    void *p = nullptr;
    bool ok;
    if (!strcmp(mode, "driver")) {
      CUdeviceptr d = 0;
      ok = cuMemAlloc(&d, want) == CUDA_SUCCESS;
      p = (void *)d;
    } else if (!strcmp(mode, "managed")) {
      ok = cudaMallocManaged(&p, want) == cudaSuccess;
    } else {
      ok = cudaMalloc(&p, want) == cudaSuccess;
    }
    if (ok) {
      chunks.push_back({(char *)p, want});
      total += want;
    } else {
      cudaGetLastError();
      chunk /= 2;
    }
  }
  return chunks;
}

static size_t sum(const std::vector<Chunk> &chunks) {
  size_t total = 0;
  for (auto &c : chunks) total += c.bytes;
  return total;
}

int main(int argc, char **argv) {
  if (argc < 2) {
    fprintf(stderr, "usage: %s fill [--hold] | scan runtime|driver|managed\n",
            argv[0]);
    return 2;
  }
  setvbuf(stdout, nullptr, _IOLBF, 0);
  // Create the context and load both kernels before taking the memory, so
  // neither needs memory after the allocations.
  CHECK(cudaFree(0));
  CHECK(cudaFuncSetCacheConfig(fill_kernel, cudaFuncCachePreferNone));
  CHECK(cudaFuncSetCacheConfig(scan_kernel, cudaFuncCachePreferNone));
  unsigned long long *counters;
  CHECK(cudaMalloc(&counters, 2 * sizeof(unsigned long long)));
  CHECK(cudaMemset(counters, 0, 2 * sizeof(unsigned long long)));
  scan_kernel<<<1, 1>>>((const uint64_t *)counters, 0, counters, counters + 1);
  CHECK(cudaDeviceSynchronize());

  size_t free_b = 0, total_b = 0;
  CHECK(cudaMemGetInfo(&free_b, &total_b));
  cudaDeviceProp prop;
  CHECK(cudaGetDeviceProperties(&prop, 0));

  if (!strcmp(argv[1], "fill")) {
    std::vector<Chunk> chunks = allocate_all("runtime", (size_t)-1);
    uint64_t base = 0;
    for (auto &c : chunks) {
      fill_kernel<<<4096, 256>>>((uint64_t *)c.ptr, c.bytes / 16, base);
      base += c.bytes;
    }
    CHECK(cudaGetLastError());
    CHECK(cudaDeviceSynchronize());
    // Check the fill landed: every record's marker must read back.
    for (auto &c : chunks)
      scan_kernel<<<4096, 256>>>((const uint64_t *)c.ptr, c.bytes / 8,
                                 counters, counters + 1);
    CHECK(cudaGetLastError());
    CHECK(cudaDeviceSynchronize());
    unsigned long long host[2];
    CHECK(cudaMemcpy(host, counters, sizeof host, cudaMemcpyDeviceToHost));
    size_t bytes = sum(chunks);
    if (host[0] * 16 != bytes) {
      fprintf(stderr, "fill check: %llu markers in %zu bytes\n", host[0], bytes);
      return 3;
    }
    printf("FILLED gpu=\"%s\" bytes=%zu total=%zu free_before=%zu chunks=%zu\n",
           prop.name, bytes, total_b, free_b, chunks.size());
    if (argc > 2 && !strcmp(argv[2], "--hold")) {
      for (;;) pause();
    }
    return 0;
  }

  if (!strcmp(argv[1], "scan") && argc > 2) {
    const char *mode = argv[2];
    if (strcmp(mode, "runtime") && strcmp(mode, "driver") &&
        strcmp(mode, "managed")) {
      fprintf(stderr, "unknown scan mode %s\n", mode);
      return 2;
    }
    // Managed memory can be oversubscribed, so it is capped at the free
    // device memory; the kernel's first touch places it on the GPU.
    size_t limit = !strcmp(mode, "managed") ? free_b - (64ull << 20) : (size_t)-1;
    std::vector<Chunk> chunks = allocate_all(mode, limit);
    auto scan_all = [&](unsigned long long out[2]) {
      CHECK(cudaMemset(counters, 0, 2 * sizeof(unsigned long long)));
      for (auto &c : chunks)
        scan_kernel<<<4096, 256>>>((const uint64_t *)c.ptr, c.bytes / 8,
                                   counters, counters + 1);
      CHECK(cudaGetLastError());
      CHECK(cudaDeviceSynchronize());
      CHECK(cudaMemcpy(out, counters, 2 * sizeof(unsigned long long),
                       cudaMemcpyDeviceToHost));
    };
    unsigned long long host[2];
    scan_all(host);
    size_t bytes = sum(chunks);
    // Positive control, after the count: write one marker into the last
    // allocation and scan again. The scan must see exactly one more, or a 0
    // above says nothing about this allocation path.
    if (chunks.empty()) {
      fprintf(stderr, "allocated nothing\n");
      return 3;
    }
    CHECK(cudaMemcpy(chunks.back().ptr, &MARKER, sizeof MARKER,
                     cudaMemcpyHostToDevice));
    unsigned long long again[2];
    scan_all(again);
    if (again[0] != host[0] + 1) {
      fprintf(stderr, "control: wrote 1 marker, scan went from %llu to %llu\n",
              host[0], again[0]);
      return 3;
    }
    printf("RESULT mode=%s gpu=\"%s\" bytes=%zu total=%zu free_before=%zu "
           "marker_words=%llu nonzero_words=%llu marker_fraction=%.6f "
           "nonzero_fraction=%.6f\n",
           mode, prop.name, bytes, total_b, free_b, host[0], host[1],
           bytes ? (double)host[0] * 16 / bytes : 0.0,
           bytes ? (double)host[1] * 8 / bytes : 0.0);
    return 0;
  }
  fprintf(stderr, "unknown command %s\n", argv[1]);
  return 2;
}
