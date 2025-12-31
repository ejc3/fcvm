# fcvm Performance Guide

This document covers performance characteristics, benchmarks, and tuning for fcvm.

## Test Environment

All benchmarks run on AWS c7g.metal (bare-metal ARM64):

| Component | Specification |
|-----------|--------------|
| CPU | 64× Neoverse-V1 (Graviton3) |
| Architecture | aarch64 |
| Memory | 128GB |
| Storage | btrfs on NVMe |
| Kernel | 6.18+ with nested virtualization |
| Instance | c7g.metal |

---

## Quick Reference

### Run Benchmarks
```bash
make bench                    # Full benchmark suite (~5 minutes)
make bench-quick              # Quick iteration (fewer samples)
make bench-throughput         # Parallel I/O throughput only
make bench-operations         # Single-op latency only
make bench-protocol           # Serialization overhead only
```

---

## Parallel I/O Throughput

**Workload**: 256 parallel workers, 1024 files × 4KB each

### Parallel Reads

| FUSE Readers | Time | vs Host | Speedup vs 1 Reader |
|--------------|------|---------|---------------------|
| Host (direct) | 7.9ms | 1.0× | — |
| 1 reader | 393ms | 49.7× slower | 1.0× |
| 2 readers | 201ms | 25.5× slower | 2.0× |
| 4 readers | 109ms | 13.8× slower | 3.6× |
| 8 readers | 70ms | 8.9× slower | 5.6× |
| **16 readers** | **61ms** | **7.7× slower** | **6.4×** |
| 32 readers | 66ms | 8.4× slower | 6.0× |
| 64 readers | 65ms | 8.2× slower | 6.0× |
| 128 readers | 66ms | 8.4× slower | 6.0× |
| 256 readers | 66ms | 8.4× slower | 6.0× |

**Optimal for reads**: 16 readers (diminishing returns above)

### Parallel Writes (with fsync)

| FUSE Readers | Time | vs Host |
|--------------|------|---------|
| Host (direct) | 161ms | 1.0× |
| 1 reader | **3.01s** | **18.7× slower** |
| 4 readers | 816ms | 5.1× slower |
| 16 readers | 293ms | 1.8× slower |
| **64 readers** | **165ms** | **1.02× (matches host!)** |
| 256 readers | 162ms | 1.01× |

**Optimal for writes**: 64 readers (matches host performance)

### Key Finding: Serialization Bottleneck

With only 1 FUSE reader, all requests serialize through a single thread:
- Reads: 49.7× slower
- Writes: 18.7× slower

**Default is 64 readers** which balances read/write performance with memory usage.

---

## Single Operation Latency

Individual FUSE operation overhead (256 readers):

| Operation | Host | FUSE | Overhead |
|-----------|------|------|----------|
| getattr | 791ns | 832ns | 1.05× |
| lookup | 784ns | 832ns | 1.06× |
| read 4KB | 853ns | 796ns | **0.93× (faster!)** |
| write 4KB | 1.0µs | 119µs | 119× |
| open+close | 1.4µs | 98µs | 68× |
| readdir | 6.0µs | 274µs | 46× |
| create+unlink | 11.8µs | 300µs | 25× |

**Observations**:
- **Cached reads are faster** than host due to kernel page cache
- **Metadata ops** (getattr, lookup) have ~5% overhead
- **Mutating ops** (write, create) have significant overhead due to fsync

---

## Wire Protocol Performance

Serialization overhead for fuse-pipe protocol:

| Operation | Time | Throughput |
|-----------|------|------------|
| Serialize lookup request | 31ns | — |
| Deserialize lookup request | 100ns | — |
| Serialize attr response | 35ns | — |
| Serialize read response (4KB) | 3.2µs | 1.19 GiB/s |
| Serialize read response (64KB) | 50.6µs | 1.21 GiB/s |
| Serialize read response (128KB) | 101µs | 1.21 GiB/s |
| Roundtrip wire request | 105ns | — |
| Roundtrip wire response (4KB) | 8.1µs | 485 MiB/s |

**Observation**: Serialization overhead is negligible (~1% of total latency).

---

## Nested Virtualization (Inception)

fcvm supports running VMs inside VMs using ARM64 FEAT_NV2. This creates FUSE-over-FUSE:
- **L1**: Host → VM (one FUSE layer)
- **L2**: Host → L1 → L2 (two FUSE layers)

### L1 vs L2 Benchmark Results

**Test**: 10MB I/O operations, 100 metadata operations

| Metric | L1 | L2 | L2/L1 Ratio |
|--------|----|----|-------------|
| Local Write | 5ms | 7ms | 1.4× |
| Local Read | 2ms | 5ms | 2.5× |
| **FUSE Write (sync)** | **81ms** | **568ms** | **7.0×** |
| **FUSE Write (async)** | **56ms** | **165ms** | **2.9×** |
| FUSE Read | 46ms | 160ms | 3.5× |
| FUSE stat | 1.1ms | 2.4ms | 2.2× |
| FUSE small read | 1.5ms | 7.4ms | 4.9× |
| Memory Used | 423MB | 221MB | — |

### Why Sync Writes Are 7× Slower

Each L2 `fsync` must propagate synchronously through both FUSE layers:

```
L2 app calls fsync()
  ↓
L2 FUSE kernel → L2 fuse-pipe client → vsock
  ↓
L1 VolumeServer receives, calls fsync() on its FUSE mount
  ↓ (BLOCKS until complete)
L1 FUSE kernel → L1 fuse-pipe client → vsock
  ↓
Host VolumeServer receives, calls fsync() on btrfs
  ↓ (BLOCKS until disk sync)
Response propagates back through all layers
```

**Breakdown**:
| Component | L1 | L2 |
|-----------|----|----|
| Async data write (10MB) | 56ms | 165ms (2.9×) |
| Fsync overhead (10 ops) | 25ms total | 403ms total |
| **Per-fsync latency** | **2.5ms** | **40ms (16×)** |

The fsync alone is **16× slower** because it blocks through two FUSE layers.

### Optimizing L2 Workloads

1. **Avoid fsync when possible** - async writes are only 3× slower, not 7×
2. **Batch operations** - amortize fsync cost across many writes
3. **Use local storage** - L2's local disk (`/tmp`) is nearly as fast as L1
4. **Reduce FUSE readers** - saves memory at deeper nesting levels

```bash
# L2 with reduced readers (saves ~400MB virtual memory)
FCVM_FUSE_READERS=8 fcvm podman run ...
```

---

## FUSE Tracing

Enable per-operation tracing to diagnose latency issues:

```bash
# Trace every 100th request (recommended)
FCVM_FUSE_TRACE_RATE=100 fcvm podman run ...

# Trace all requests (high overhead, debugging only)
FCVM_FUSE_TRACE_RATE=1 fcvm podman run ...
```

### Trace Output

```
[TRACE         read] total=8940µs srv=159µs | fs=149 | to_srv=33 to_cli=1974
[TRACE        fsync] total=70000µs srv=3000µs | fs=2900 | to_srv=? to_cli=?
```

| Field | Meaning |
|-------|---------|
| `total` | End-to-end client round-trip (always accurate) |
| `srv` | Server-side processing time (always accurate) |
| `fs` | Filesystem operation time |
| `to_srv` | Network latency client→server (may show `?` if clocks differ) |
| `to_cli` | Network latency server→client (may show `?` if clocks differ) |

---

## Memory Efficiency

### UFFD Memory Sharing

Multiple VMs cloned from the same snapshot share memory via kernel page cache:

| Scenario | Expected RAM | Actual RAM |
|----------|--------------|------------|
| 1 VM (512MB) | 512MB | 512MB |
| 10 clones | 5.1GB | ~550MB |
| 50 clones | 25.6GB | ~600MB |

Memory is only copied on write (true CoW at page level).

### btrfs CoW Disk Snapshots

Disk cloning uses btrfs reflinks:

| Method | Time | Space |
|--------|------|-------|
| Regular copy (`cp`) | ~850ms | Full duplicate |
| Reflink copy | **~1.5ms** | Zero until modified |

---

## Configuration Reference

| Variable | Default | Purpose |
|----------|---------|---------|
| `FCVM_FUSE_READERS` | 64 | Number of FUSE reader threads |
| `FCVM_FUSE_TRACE_RATE` | 0 | Trace every Nth request (0=disabled) |

### Memory Usage

```
Memory per FUSE mount ≈ readers × 8MB (thread stack)

64 readers = 512MB virtual (RSS much lower due to lazy allocation)
8 readers = 64MB virtual
```

---

## Reproducing Benchmarks

### 1. fuse-pipe Benchmarks

```bash
# Full benchmark suite
make bench

# Results printed to stdout, graphs in target/criterion/
```

### 2. Inception Benchmarks

```bash
# Build inception kernel (first time, ~20 min)
./kernel/build.sh

# Run L2 test with benchmarks
make test-root FILTER=inception_l2 STREAM=1
```

### 3. FUSE Latency Tracing

```bash
# Trace L2 operations
FCVM_FUSE_TRACE_RATE=100 make test-root FILTER=inception_l2 STREAM=1 2>&1 | tee trace.log

# Extract traces
grep "TRACE" trace.log
```

---

## Summary

| Scenario | Recommendation |
|----------|----------------|
| General workloads | Use default 64 FUSE readers |
| Read-heavy | 16 readers is optimal |
| Write-heavy | 64+ readers needed |
| Memory constrained | Reduce to 8-16 readers |
| Nested VMs (L2+) | Avoid fsync, use local disk |
| Debug latency | Enable FUSE tracing |
| Many clones | Use UFFD memory sharing |
| Fast disk copies | Use btrfs reflinks |

---

## Related Documentation

- [README.md](README.md) - Getting started
- [.claude/CLAUDE.md](.claude/CLAUDE.md) - Development notes
- [fuse-pipe/benches/](fuse-pipe/benches/) - Benchmark source code
