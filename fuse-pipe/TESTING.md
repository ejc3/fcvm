# fuse-pipe Testing & Benchmarking

This document covers all testing and benchmarking for fuse-pipe. Each test/benchmark file references this document.

## Prerequisites

### Required
- Rust 1.70+ with `cargo`
- Linux with FUSE support (`/dev/fuse`)
- `fusermount3` or `fusermount`

### For Benchmarks (EC2)
- ARM64 bare-metal instance (c6g.metal) for optimal performance
- SSH key at `~/.ssh/fcvm-ec2`

### For pjdfstest
```bash
git clone https://github.com/pjd/pjdfstest /tmp/pjdfstest-check
cd /tmp/pjdfstest-check && autoreconf -ifs && ./configure && make
```

## Quick Reference

| Command | Description |
|---------|-------------|
| `make test` | Run all unit tests |
| `make test-integration` | Run FUSE integration tests |
| `make test-stress` | Run stress test (4 workers, 1000 ops) |
| `make bench` | Run all benchmarks |
| `make bench-trace` | Run benchmarks with tracing enabled |
| `make bench-remote` | Run benchmarks on EC2 |
| `make bench-logs` | View recent benchmark logs |
| `make bench-clean` | Clean up log/telemetry files |

## Test Types

| Type | Location | Purpose |
|------|----------|---------|
| Unit | `src/**/*.rs` | Module-level tests |
| Integration | `tests/integration.rs` | Basic FUSE operations |
| Stress | `tests/stress/` | Multi-worker load testing |
| Benchmarks | `benches/` | Performance measurement |
| POSIX | `tests/pjdfstest_*.rs` | Filesystem compliance |

## Step-by-Step Test Execution

Run tests in order from simplest to hardest. Each level should pass before proceeding.

### Level 1: Unit Tests (No FUSE Required)
```bash
make test
# or: cargo test --lib
```

### Level 2: Integration Tests (Basic FUSE Ops)
```bash
make test-integration
# or: cargo test --test integration
```

### Level 3: Stress Test (Multi-Worker Load)
```bash
make test-stress
# or: cargo build --release --test stress && sudo ./target/release/deps/stress-* --workers 4 --ops 1000
```

### Level 4: Quick Benchmarks
```bash
make bench-quick
# or: cargo bench --bench throughput -- --quick
```

### Level 5: Full Benchmark Suite
```bash
make bench
# or: cargo bench --bench throughput && cargo bench --bench operations
```

### Level 6: Benchmarks with Tracing
```bash
make bench-trace
# or: cargo bench --bench throughput --features trace-benchmarks
```

### Full Verification Command
```bash
make test && make test-integration && make test-stress && make bench-quick
```

## Tracing & Telemetry

### Enable Tracing

```bash
# With feature flag (traces every 100th request)
make bench-trace

# With RUST_LOG for application logs
RUST_LOG=debug make bench-trace
```

### Output Locations

| Type | Location |
|------|----------|
| Application logs | stderr |
| Telemetry summary | stderr |
| Telemetry JSON | `/tmp/fuse-bench-telemetry-{pid}-{id}.json` |
| Benchmark logs | `/tmp/fuse-bench-{pid}-{id}.log` |

### Telemetry JSON Format

```json
{
  "count": 1000,
  "total": {"min_ns": 50000, "p50_ns": 120000, "p90_ns": 250000, "p99_ns": 500000},
  "to_server": {...},
  "server_deser": {...},
  "server_spawn": {...},
  "server_fs": {...},
  "server_chan": {...},
  "to_client": {...},
  "client_done": {...},
  "by_operation": [
    {"op_name": "getattr", "count": 400, "total": {...}, "server_fs": {...}},
    {"op_name": "lookup", "count": 250, ...}
  ]
}
```

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                     Test Harness                             │
├──────────────────────────────────────────────────────────────┤
│  tests/stress/fixture.rs   ←── Shared FuseMount struct       │
│       ↓                                                      │
│  Spawns stress binary as subprocess:                         │
│    - `stress server --socket ... --root ...`                 │
│    - `stress client --socket ... --mount ... --trace-rate N` │
│                                                              │
│  Captures:                                                   │
│    - stderr → /tmp/fuse-bench-{pid}.log                      │
│    - telemetry → /tmp/fuse-bench-telemetry-{pid}.json        │
└──────────────────────────────────────────────────────────────┘
```

### Shared Fixture

All tests and benchmarks use the shared `FuseMount` struct from `tests/stress/fixture.rs`:

```rust
use fixture::{FuseMount, increase_ulimit, setup_test_data};

// Basic mount (no tracing)
let fuse = FuseMount::new(&data_dir, &mount_dir, 256);

// Mount with tracing
let fuse = FuseMount::with_tracing(&data_dir, &mount_dir, 256, 100, None);

// Access paths
let path = fuse.mount_path().join("test.txt");
```

## Performance Benchmarks

### Benchmark Results (c6g.metal, 64 ARM cores)

#### Parallel Reads (256 workers, 1024 files x 4KB)

| Readers | Time (ms) | vs Host | Speedup vs 1 Reader |
|---------|-----------|---------|---------------------|
| Host FS | 10.7 | 1.0x | - |
| 1 | 490.6 | 45.8x slower | 1.0x |
| 256 | 57.0 | 5.3x slower | 8.6x |

#### Parallel Writes (256 workers, with sync_all)

| Readers | Time (s) | vs Host |
|---------|----------|---------|
| Host FS | 0.862 | 1.0x |
| 256 | 2.765 | 3.2x slower |

### Key Findings

- **Optimal readers**: 256 readers provides best balance
- **Read overhead**: ~5-6x vs native filesystem
- **Write overhead**: ~3x vs native (bounded by disk I/O)
- **Scaling**: Performance plateaus at 16+ readers for reads

## Stress Test

The stress test runs multi-worker load testing against both host filesystem and FUSE:

```bash
# Default: 4 workers, 1000 ops each
cargo test --test stress --release

# Custom configuration
cargo test --test stress --release -- --workers 64 --readers 256 --ops 5000

# With tracing
cargo test --test stress --release -- -t 100
```

### Stress Test Subcommands

The stress binary also provides server/client subcommands for the test harness:

```bash
# Start server (passthrough filesystem)
./stress server --socket /tmp/fuse.sock --root /tmp/data

# Start client (FUSE mount)
./stress client --socket /tmp/fuse.sock --mount /tmp/mount --readers 256

# With tracing
./stress client ... --trace-rate 100 --telemetry-output /tmp/telemetry.json
```

## pjdfstest (POSIX Compliance)

Run the pjdfstest POSIX filesystem compliance suite:

```bash
# Fast subset
make test-pjdfs

# Full suite (requires pjdfstest-full feature)
cargo test --test pjdfstest_full --features pjdfstest-full -- --nocapture
```

### pjdfstest Prerequisites

```bash
# Install pjdfstest
git clone https://github.com/pjd/pjdfstest /tmp/pjdfstest-check
cd /tmp/pjdfstest-check
autoreconf -ifs
./configure
make
```

## Remote Benchmarking (EC2)

### Sync and Run

```bash
# Sync code to EC2
make sync

# Run benchmarks on EC2
make bench-remote

# Run with tracing
make bench-remote-trace

# View remote logs
make bench-logs-remote
```

### EC2 Configuration

| Setting | Value |
|---------|-------|
| Instance | c6g.metal (ARM64 bare metal) |
| Host | ubuntu@54.67.60.104 |
| SSH Key | ~/.ssh/fcvm-ec2 |

## Troubleshooting

### Mount Errors

```bash
# Force unmount stale mounts
fusermount3 -u /tmp/fuse-bench-mount

# Check for existing mounts
mount | grep fuse
```

### Socket Cleanup

```bash
# Remove stale sockets
rm -f /tmp/fuse-bench-*.sock
rm -f /tmp/fuse-stress*.sock
```

### Log Inspection

```bash
# View recent logs
ls -lt /tmp/fuse-bench-*.log | head -5

# View latest telemetry
cat $(ls -t /tmp/fuse-bench-telemetry-*.json | head -1) | jq .
```

## Key Files

| File | Purpose |
|------|---------|
| `tests/stress/fixture.rs` | Shared FuseMount struct |
| `tests/stress/main.rs` | Stress test binary |
| `tests/stress/harness.rs` | Stress test orchestration |
| `tests/integration.rs` | Basic FUSE operation tests |
| `benches/throughput.rs` | Parallel I/O benchmarks |
| `benches/operations.rs` | Single-operation latency benchmarks |
| `src/telemetry.rs` | Span collection and statistics |
| `src/protocol/wire.rs` | Span struct with timing fields |
