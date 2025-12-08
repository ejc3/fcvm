# fuse-pipe Testing & Benchmarking

This document covers all testing and benchmarking for fuse-pipe.

## Prerequisites

### Required
- Rust 1.70+ with `cargo`
- Linux with FUSE support (`/dev/fuse`)
- `fusermount3` or `fusermount`
- Root access (most tests require sudo for FUSE mounts)

### For pjdfstest
```bash
git clone https://github.com/pjd/pjdfstest /tmp/pjdfstest
cd /tmp/pjdfstest && autoreconf -ifs && ./configure && make
sudo cp pjdfstest /usr/local/bin/
```

### For EC2 Benchmarks
- ARM64 bare-metal instance (c6g.metal) for optimal performance
- SSH key at `~/.ssh/fcvm-ec2`

## Quick Reference

| Command | Description | Requires Root |
|---------|-------------|---------------|
| `cargo test --lib` | Unit tests | No |
| `sudo cargo test --test integration` | Basic FUSE operations | Yes |
| `sudo cargo test --test test_permission_edge_cases` | Permission edge cases | Yes |
| `sudo cargo test --test pjdfstest_fast` | Quick POSIX compliance (32 readers) | Yes |
| `sudo cargo test --test pjdfstest_full` | Full POSIX compliance (8789 tests) | Yes |
| `sudo cargo test --test pjdfstest_stress` | Parallel stress test (85 jobs) | Yes |
| `sudo cargo bench --bench throughput` | I/O throughput at varying concurrency | Yes |
| `sudo cargo bench --bench operations` | Single-operation latency | Yes |
| `make test-all` | Run everything (unit + integration + pjdfs + bench) | Yes |

## Test Files

| File | Purpose | Tests |
|------|---------|-------|
| `src/**/*.rs` | Unit tests | ~20 |
| `tests/integration.rs` | Basic FUSE operations (read, write, mkdir, etc.) | 15 |
| `tests/test_permission_edge_cases.rs` | Permission edge cases, setuid/setgid | 18 |
| `tests/pjdfstest_fast.rs` | Quick POSIX compliance test (32 readers) | 8789 |
| `tests/pjdfstest_full.rs` | Full POSIX compliance test (256 readers) | 8789 |
| `tests/pjdfstest_stress.rs` | Parallel stress test (5 instances × 17 categories) | ~44000 |
| `tests/pjdfstest_common.rs` | Shared pjdfstest harness code | - |
| `tests/common/mod.rs` | Shared `FuseMount` fixture | - |
| `benches/throughput.rs` | Parallel read/write at varying reader counts | - |
| `benches/operations.rs` | Single-operation latency benchmarks | - |
| `benches/protocol.rs` | Protocol serialization benchmarks | - |

## Step-by-Step Test Execution

Run tests in order from simplest to hardest.

### Level 1: Unit Tests (No FUSE Required)
```bash
cargo test --lib
```

### Level 2: Integration Tests (Basic FUSE Ops)
```bash
sudo cargo test --release --test integration -- --nocapture
```

### Level 3: Permission Edge Cases
```bash
sudo cargo test --release --test test_permission_edge_cases -- --nocapture
```

### Level 4: Quick POSIX Compliance (pjdfstest)
```bash
sudo cargo test --release --test pjdfstest_fast -- --nocapture
```

### Level 5: Full POSIX Compliance (8789 tests)
```bash
sudo cargo test --release --test pjdfstest_full -- --nocapture
```

### Level 6: Parallel Stress Test (race condition detection)
```bash
sudo cargo test --release --test pjdfstest_stress -- --nocapture
```
Runs 5 parallel instances of each of 17 pjdfstest categories simultaneously (85 total parallel jobs, ~44000 tests).

### Level 7: Throughput Benchmarks
```bash
sudo cargo bench --bench throughput
```
Tests parallel read/write performance at different reader counts (1, 2, 4, 8, 16, 32, 64, 128, 256).

### Full Verification
```bash
make test-all
# or manually:
cargo test --lib && \
sudo cargo test --release --test integration && \
sudo cargo test --release --test pjdfstest_full && \
sudo cargo bench --bench throughput
```

## pjdfstest Categories

The POSIX compliance tests cover 17 categories:

| Category | Tests | Description |
|----------|-------|-------------|
| chmod | 327 | Permission mode changes |
| chown | 1497 | Ownership changes |
| chflags | 14 | File flags (limited on Linux) |
| ftruncate | 89 | Truncate via file descriptor |
| granular | 7 | Granular permission checks |
| link | 359 | Hard links |
| mkdir | 118 | Directory creation |
| mkfifo | 120 | FIFO creation |
| mknod | 186 | Device node creation |
| open | 328 | File open modes |
| posix_fallocate | 1 | Space preallocation |
| rename | 4886 | File/directory renames |
| rmdir | 145 | Directory removal |
| symlink | 95 | Symbolic links |
| truncate | 84 | Truncate by path |
| unlink | 440 | File removal |
| utimensat | 122 | Timestamp modification |

## Benchmark Results (c6g.metal, 64 ARM cores)

### Parallel Reads (256 workers, 1024 files × 4KB)

| Readers | Time | vs Host | Speedup vs 1 Reader |
|---------|------|---------|---------------------|
| Host FS | ~29ms | 1.0x | - |
| 1 | ~1.5s | 52x slower | 1.0x |
| 16 | ~100ms | 3.4x slower | 15x |
| 256 | ~60ms | 2.1x slower | 25x |

### Key Findings

- **Optimal readers**: 256 readers provides best throughput
- **Read overhead**: ~2-3x vs native filesystem at optimal concurrency
- **Scaling**: Near-linear improvement from 1→16 readers, diminishing returns after
- **No race conditions**: Stress test validates thread-safety of credential switching

## Debugging

### Enable Tracing
```bash
# All components
RUST_LOG=debug sudo -E cargo test --release --test integration -- --nocapture

# Specific targets
RUST_LOG="passthrough=debug,fuse_pipe=info" sudo -E cargo test ...
```

### Tracing Targets

| Target | Component |
|--------|-----------|
| `fuse_pipe::fixture` | Test fixture setup/teardown |
| `fuse-pipe::server` | Async server |
| `fuse-pipe::client` | FUSE client, multiplexer |
| `passthrough` | PassthroughFs operations |

### Mount Cleanup
```bash
# Force unmount stale mounts
sudo fusermount3 -u /tmp/fuse-*-mount*
sudo fusermount -u /tmp/fuse-*-mount*

# Remove stale sockets
rm -f /tmp/fuse-*.sock
```

## Shared Fixture

All tests use the `FuseMount` struct from `tests/common/mod.rs`:

```rust
#[path = "common/mod.rs"]
mod common;

use common::{FuseMount, increase_ulimit, setup_test_data};

#[test]
fn test_something() {
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    // fuse.mount_path() is the FUSE mount point
    // data_dir is the backing storage
}
```

## Remote Testing (EC2)

From the project root:
```bash
# Sync code
make sync

# Run tests on EC2
ssh -i ~/.ssh/fcvm-ec2 ubuntu@54.67.60.104 \
  "cd ~/fcvm && source ~/.cargo/env && \
   sudo cargo test --release -p fuse-pipe --test pjdfstest_full -- --nocapture"
```

## Key Files

| File | Purpose |
|------|---------|
| `tests/common/mod.rs` | Shared FuseMount fixture |
| `tests/pjdfstest_common.rs` | pjdfstest harness |
| `benches/throughput.rs` | Parallel I/O benchmarks |
| `src/server/credentials.rs` | CredentialsGuard for uid/gid switching |
| `src/server/passthrough.rs` | PassthroughFs implementation |
