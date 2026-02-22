# Plan: Always use diff snapshots when a base exists

## Context

All snapshot creation goes through `create_snapshot_core(client, config, disk, parent_dir)`. When `parent_dir` is provided and has a `memory.bin`, it creates a diff snapshot (only dirty pages). When None, it creates a full snapshot (entire memory).

The problem: callers don't pass `parent_dir` when they should.

1. **Pre-start snapshot** (`podman/mod.rs:920`): `parent_snapshot_key: None` — correct, it's the first snapshot
2. **Startup snapshot** (`podman/mod.rs:979`): `parent_snapshot_key: None` — **wrong**, should use pre-start as base
3. **Manual snapshot** (`snapshot.rs:156-160`): reads `vm_state.config.snapshot_name` — **always None** for fresh VMs because nobody sets it after creating snapshots

## Fix

### 1. Pass pre-start key as parent for startup snapshot (`src/commands/podman/mod.rs`)

Line 979: `parent_snapshot_key: None` → `parent_snapshot_key: Some(key.as_str())`

Remove stale KVM dirty page tracking comment (lines 974-978).

### 2. Track last snapshot in VmState (`src/commands/podman/mod.rs`)

After each snapshot creation, update `vm_state.config.snapshot_name` and save state:

- After pre-start snapshot (line 927): set to pre-start key
- After startup snapshot (line 988): set to startup key

This way `snapshot create` (which reads `vm_state.config.snapshot_name`) automatically gets the diff base.

### 3. Track snapshot in manual create too (`src/commands/snapshot.rs`)

After `create_snapshot_core` succeeds (line 167), update `vm_state.config.snapshot_name` to the new tag and save state. This chains manual snapshots: snap-1 → snap-2 uses snap-1 as base.

### 4. Integration test (`tests/test_diff_snapshot.rs`)

New test: `test_diff_snapshot_uses_base`
- Start a rootless VM with a simple container
- Wait for healthy (pre-start snapshot created automatically)
- Create manual snapshot: `fcvm snapshot create --pid <pid> --tag snap-1`
- Verify `memory.diff` existed during creation (check logs for "creating diff snapshot")
- Check `vm_state.config.snapshot_name == "snap-1"`
- Create second snapshot: `fcvm snapshot create --pid <pid> --tag snap-2`
- Verify also diff
- Restore from snap-2, verify VM works

### Files to modify
- `src/commands/podman/mod.rs` — fix startup parent, store snapshot_name after pre-start/startup
- `src/commands/snapshot.rs` — store snapshot_name after manual create
- `tests/test_diff_snapshot.rs` — new integration test

### Verification
1. `make build`
2. `make test-root FILTER=diff_snapshot STREAM=1`
3. Manual: `fcvm snapshot create` on running www VM → check logs for "creating diff snapshot"
