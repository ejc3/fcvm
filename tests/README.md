# fcvm Integration Tests

This directory contains integration tests for fcvm functionality.

## Test Structure

- `common/mod.rs` - Shared test utilities (VmFixture, health checking, etc.)
- `test_fuse_posix.rs` - POSIX FUSE compliance tests using pjdfstest
- `test_localhost_image.rs` - Localhost container image tests (skopeo workflow)
- `test_sanity.rs` - Basic VM startup and health check tests
- `test_snapshot_clone.rs` - Snapshot and clone tests
- `test_state_manager.rs` - State management tests
- `test_health_monitor.rs` - Health monitoring tests
- `test_fuse_in_vm.rs` - FUSE-in-VM integration tests

## POSIX FUSE Tests

The POSIX FUSE tests (`test_fuse_posix.rs`) validate that fcvm's FUSE volume implementation complies with POSIX filesystem semantics using the pjdfstest suite.

### Prerequisites

1. **pjdfstest installation**:
   ```bash
   git clone https://github.com/pjd/pjdfstest /tmp/pjdfstest-check
   cd /tmp/pjdfstest-check
   autoreconf -ifs
   ./configure
   make
   ```

2. **fcvm built**:
   ```bash
   cargo build --release
   ```

3. **Root access**: Tests require sudo to create VMs

### Running Tests

**List available test categories**:
```bash
cargo test --test test_fuse_posix list_categories -- --ignored --nocapture
```

**Run a specific category** (e.g., chmod tests):
```bash
cargo test --test test_fuse_posix test_posix_chmod -- --ignored --nocapture
```

**Run multiple categories in parallel** (4 threads):
```bash
cargo test --test test_fuse_posix -- --ignored --nocapture --test-threads=4
```

**Run all categories at once** (slower, better for CI):
```bash
cargo test --test test_fuse_posix test_posix_all_categories -- --ignored --nocapture
```

### Available Test Categories

Each category runs as a separate test for parallel execution:

- `test_posix_chmod` - chmod() permission tests
- `test_posix_chown` - chown() ownership tests
- `test_posix_link` - hard link tests
- `test_posix_mkdir` - directory creation tests
- `test_posix_mkfifo` - FIFO creation tests
- `test_posix_open` - file open/create tests
- `test_posix_rename` - rename/move tests
- `test_posix_rmdir` - directory removal tests
- `test_posix_symlink` - symbolic link tests
- `test_posix_truncate` - file truncation tests
- `test_posix_unlink` - file deletion tests

### Test Architecture

Each test:
1. Creates a unique VM using `VmFixture`
2. Mounts a FUSE volume from host to guest
3. Runs pjdfstest against the host directory
4. Files written on host are visible in guest via FUSE
5. Cleans up VM and test directories on completion

Tests run in **parallel** by default via cargo test, with each test getting:
- Unique VM name (e.g., `test_posix_chmod-12345-0`)
- Isolated host directory (`/tmp/fcvm-test-{pid}-{id}/host`)
- Isolated work directory for pjdfstest

### Makefile Integration

From your local machine (syncs to EC2 and runs):

```bash
# Run all POSIX tests in parallel (recommended)
make test-fuse-posix

# Run a specific category
make test-fuse-posix-chmod
```

From EC2 directly:

```bash
cd ~/fcvm
cargo test --test test_fuse_posix -- --ignored --nocapture --test-threads=4
```

## VM Fixture Pattern

The `VmFixture` struct in `common/mod.rs` provides a reusable pattern for VM-based integration tests:

```rust
use common::VmFixture;

#[tokio::test]
async fn test_my_feature() {
    // Create VM with FUSE volume
    let fixture = VmFixture::new("my-test")
        .await
        .expect("failed to create VM");

    // Access host directory
    let host_dir = fixture.host_dir();
    std::fs::write(host_dir.join("test.txt"), "hello").unwrap();

    // File is visible in guest at /mnt/test/test.txt

    // Fixture automatically cleans up VM on drop
}
```

### VmFixture Features

- **Automatic VM creation**: Spawns VM with FUSE volume
- **Health checking**: Waits for VM to become healthy before returning
- **Unique isolation**: Each test gets unique VM name and directories
- **Automatic cleanup**: Kills VM and removes directories on drop
- **PID tracking**: Tracks VM by fcvm process PID

## Debugging Tests

### View test output with timestamps:
```bash
cargo test --test test_fuse_posix test_posix_chmod -- --ignored --nocapture 2>&1 | ts '[%Y-%m-%d %H:%M:%S]'
```

### Run tests sequentially for easier debugging:
```bash
cargo test --test test_fuse_posix -- --ignored --nocapture --test-threads=1
```

### Check running VMs during tests:
```bash
# In another terminal
sudo fcvm ls
```

### Manual cleanup if tests fail:
```bash
# Kill all test VMs
ps aux | grep fcvm | grep test_posix | awk '{print $2}' | xargs sudo kill

# Remove test directories
rm -rf /tmp/fcvm-test-*
```

## Writing New Tests

To add a new POSIX test category:

1. Add the category to `test_fuse_posix.rs`:
   ```rust
   posix_test!(test_posix_mycategory, "mycategory");
   ```

2. The test will automatically:
   - Run in parallel with other tests
   - Get isolated VM and directories
   - Clean up on completion

## CI/CD Integration

Recommended CI workflow:

```yaml
- name: Install pjdfstest
  run: |
    git clone https://github.com/pjd/pjdfstest /tmp/pjdfstest-check
    cd /tmp/pjdfstest-check
    autoreconf -ifs && ./configure && make

- name: Build fcvm
  run: cargo build --release

- name: Run POSIX FUSE tests
  run: |
    cargo test --test test_fuse_posix -- --ignored --nocapture --test-threads=4
```

## Troubleshooting

### "fcvm binary not found"
- Build fcvm first: `cargo build --release`
- Or set PATH: `export PATH=$PATH:./target/release`

### "pjdfstest not found"
- Install pjdfstest (see Prerequisites above)
- Verify: `ls -l /tmp/pjdfstest-check/pjdfstest`

### "timeout waiting for VM to become healthy"
- Check VM logs: `sudo fcvm ls --json | jq`
- Increase timeout in `common/mod.rs` (default: 120s)
- Verify network: `sudo fcvm test sanity`

### Tests hang indefinitely
- VMs may not be cleaning up properly
- Manual cleanup: `ps aux | grep fcvm | grep test | awk '{print $2}' | xargs sudo kill`
- Check for orphaned processes: `sudo fcvm ls`

## Performance Notes

- **Parallel execution**: 4-8 tests can run simultaneously on typical hardware
- **VM startup time**: ~5-10 seconds per VM
- **Test duration**: 30-120 seconds per category (varies by complexity)
- **Total runtime**: ~5-15 minutes for all categories (parallel with 4 threads)

## Related Documentation

- Main README: `/Users/ejcampbell/src/fcvm/README.md`
- CLAUDE.md: `/Users/ejcampbell/src/fcvm/.claude/CLAUDE.md`
- fuse-pipe tests: `/Users/ejcampbell/src/fcvm/fuse-pipe/tests/README.md`
